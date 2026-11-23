import { spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:http";
import { connect } from "node:net";
import { setTimeout as delay } from "node:timers/promises";

// DevTools uses inherited pipes. No local or remote debugging port is opened.
class BrowserPipe {
  constructor(edge, maxBytes) {
    this.input = edge.stdio[3];
    this.pending = new Map();
    this.nextId = 0;
    this.buffer = "";
    this.onEvent = () => {};
    edge.stdio[4].setEncoding("utf8").on("data", (chunk) => {
      try {
        this.buffer += chunk;
        if (this.buffer.length > maxBytes) throw new Error();
        let end;
        while ((end = this.buffer.indexOf("\0")) !== -1) {
          const message = JSON.parse(this.buffer.slice(0, end));
          this.buffer = this.buffer.slice(end + 1);
          if (message.id !== undefined) {
            const reply = this.pending.get(message.id);
            if (reply) {
              this.pending.delete(message.id);
              reply(message.error ? null : message.result);
            }
          } else {
            this.onEvent(message.method, message.params, message.sessionId);
          }
        }
      } catch {
        this.onEvent("pipeError");
        this.close();
      }
    });
    for (const stream of [edge.stdio[4], this.input]) stream.on("error", () => this.close());
    edge.once("exit", () => this.close());
  }

  send(method, params = {}, sessionId, timeout = 5000) {
    if (this.closed) return Promise.reject(new Error());
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(method));
      }, timeout);
      this.pending.set(id, (result) => {
        clearTimeout(timer);
        if (result == null) reject(new Error(method));
        else resolve(result);
      });
      this.input.write(JSON.stringify({ id, method, params, sessionId }) + "\0");
    });
  }

  close() {
    this.closed = true;
    for (const reply of this.pending.values()) reply(null);
    this.pending.clear();
  }
}

// HTTPS tunnels stay opaque and use Edge's normal certificate validation.
async function startProxy(origins) {
  const hosts = new Map(origins.map((origin) => {
    const url = new URL(origin);
    return [url.host + ":443", url.hostname];
  }));
  const sockets = new Set();
  const server = createServer((_request, response) => response.writeHead(403).end());
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("error", () => socket.destroy());
    socket.once("close", () => sockets.delete(socket));
  });
  server.on("connect", (request, client, head) => {
    const hostname = hosts.get(request.url);
    if (!hostname) {
      client.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
      return;
    }
    const upstream = connect({ host: hostname, port: 443 });
    upstream.setTimeout(15000, () => upstream.destroy());
    upstream.once("connect", () => {
      upstream.setTimeout(0);
      client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head.length) upstream.write(head);
      upstream.pipe(client).pipe(upstream);
    });
    upstream.once("error", () => client.destroy());
    upstream.once("close", () => client.destroy());
    client.once("close", () => upstream.destroy());
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return {
    port: server.address().port,
    close() {
      for (const socket of sockets) socket.destroy();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}

function parseUrl(raw) {
  try {
    const url = new URL(raw);
    return url.username || url.password ? null : url;
  } catch {
    return null;
  }
}

function isWebClassPage(url, config) {
  return url?.origin === config.lms && url.pathname.startsWith("/webclass/");
}

// Runs in an isolated JS world on the verified WebClass document.
function loggedIn(dashboard) {
  const signedIn = (doc) => {
    if (!doc || doc.readyState === "loading" || doc.querySelector('input[type="password"]')) return false;
    return [...doc.querySelectorAll("a[href], button, input[type=submit]")].some((control) => {
      const label = (control.textContent || control.value || "").trim();
      return /^(ログアウト|log\s*out|sign\s*out)$/i.test(label);
    });
  };
  if (location.pathname === dashboard && document.readyState !== "loading" &&
      !document.querySelector('input[type="password"]')) return true;
  if (signedIn(document)) return true;
  return [...document.querySelectorAll("iframe, frame")].some((frame) => {
    const doc = frame.contentDocument;
    return doc?.location?.origin === location.origin && signedIn(doc);
  });
}

async function readCourses(origin, path, maxBytes) {
  if (location.origin !== origin || !location.pathname.startsWith("/webclass/")) {
    return { reason: "unknown_destination" };
  }
  try {
    const response = await fetch(path, {
      method: "GET", mode: "same-origin", credentials: "same-origin",
      redirect: "error", cache: "no-store", signal: AbortSignal.timeout(15000),
      headers: { Accept: "application/json" },
    });
    if (response.status === 401 || response.status === 403) return { reason: "auth_expired" };
    if (response.status !== 200 || response.url !== origin + path || response.redirected ||
        response.headers.has("content-range") || response.headers.has("link") ||
        response.headers.get("content-type")?.split(";")[0].trim().toLowerCase() !== "application/json") {
      return { reason: "invalid_course_response" };
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder("utf-8", { fatal: true });
    let bytes = 0;
    let body = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) return { reason: "ok", body: body + decoder.decode() };
      bytes += value.length;
      if (bytes > maxBytes) {
        void reader.cancel();
        return { reason: "course_response_limit" };
      }
      body += decoder.decode(value, { stream: true });
    }
  } catch {
    return { reason: "course_request_failed" };
  }
}

const expression = (fn, ...args) => "(" + fn.toString() + ")(" + args.map((arg) => JSON.stringify(arg)).join(",") + ")";

async function evaluate(pipe, session, frameId, script, timeout) {
  const { executionContextId } = await pipe.send("Page.createIsolatedWorld", {
    frameId, worldName: "assignment-manager",
  }, session);
  const { result, exceptionDetails } = await pipe.send("Runtime.evaluate", {
    expression: script, contextId: executionContextId, returnByValue: true, awaitPromise: true,
  }, session, timeout);
  return exceptionDetails ? undefined : result.value;
}

function apiPath(url, config) {
  if (url.origin !== config.lms) return null;
  let path = url.pathname;
  for (let pass = 0; pass < 3 && /%[a-f\d]{2}/i.test(path); pass++) path = decodeURIComponent(path);
  if (/%[a-f\d]{2}/i.test(path)) return "blocked";
  path = path.toLowerCase().replaceAll("\\", "/").replace(/\/{2,}/g, "/");
  const api = config.coursesPath.slice(0, config.coursesPath.lastIndexOf("/") + 1);
  if (path.includes(api)) {
    const operation = path.slice(path.indexOf(api) + api.length);
    if (/^(?:[\/;]*)(?:courses|contents)(?:$|[\/;.?#])/.test(operation)) return operation === "courses" ? "courses" : "blocked";
  }
  return null;
}

async function configurePage(pipe, session, page, config) {
  await pipe.send("Fetch.enable", { patterns: [
    { urlPattern: "*", resourceType: "Document" },
    { urlPattern: config.lms + "/*" },
  ] }, session);
  await pipe.send("Runtime.runIfWaitingForDebugger", {}, session);
  await pipe.send("Network.setBypassServiceWorker", { bypass: true }, session);
  await pipe.send("Page.enable", {}, session);
  const { frameTree } = await pipe.send("Page.getFrameTree", {}, session);
  page.frame = { id: frameTree.frame.id, webclass: isWebClassPage(parseUrl(frameTree.frame.url), config) };
  page.configured = true;
}

export async function runBrowser(config, fixture) {
  let edge;
  let edgeExited;
  let proxy;
  let stopped = false;
  let lmsReached = false;
  let coursesGet = 0;
  let finish;
  const stoppedResult = new Promise((resolve) => { finish = resolve; });
  const stop = (reason) => { stopped = true; finish({ reason }); };
  const timeout = setTimeout(() => stop("login_timeout"), config.loginMillis);
  let outcome;
  try {
    proxy = await startProxy(config.origins.filter((origin) => new URL(origin).protocol === "https:"));
    const flags = [
      "--no-startup-window", "--remote-debugging-pipe", "--user-data-dir=" + config.profile,
      "--no-first-run", "--no-default-browser-check", "--disable-sync", "--disable-extensions",
      "--disable-background-networking", "--disable-component-update", "--disable-quic",
      "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
      "--proxy-server=http://127.0.0.1:" + proxy.port,
      "--proxy-bypass-list=<-loopback>",
      "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
    ];
    if (fixture) flags.push("--headless=new", ...fixture.flags);
    edge = spawn(config.edge, flags, { stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
    edgeExited = new Promise((resolve) => {
      edge.once("exit", () => { stop("browser_closed"); resolve(); });
      edge.once("error", () => { stop("edge_start_failed"); resolve(); });
    });
    const pipe = new BrowserPipe(edge, config.maxBytes * 6 + 65536);
    const pages = new Map();
    let armed;
    let documents = 0;

    // Interception only reads routing metadata, never request headers or form fields.
    async function requestPaused({ request: { url: raw, method }, requestId, resourceType, frameId }, session) {
      const url = parseUrl(raw);
      const document = resourceType === "Document";
      const mainFrame = pages.get(session)?.frame;
      const topLevel = document && (!mainFrame || mainFrame.id === frameId);
      const navigationAllowed = !document || (url && config.origins.includes(url.origin) && /^(GET|POST)$/.test(method));
      let allowed = !stopped && navigationAllowed;
      if (!navigationAllowed && topLevel) stop("unknown_destination");
      if (topLevel && ++documents > 128) {
        stop("navigation_limit");
        allowed = false;
      }
      if (allowed && url) {
        const operation = apiPath(url, config);
        if (operation) {
          allowed = operation === "courses" && !document && method === "GET" &&
            url.pathname === config.coursesPath && !url.search &&
            session === armed?.session && frameId === armed.frameId && coursesGet === 0;
          if (allowed) coursesGet++;
        }
      }
      await pipe.send(allowed ? "Fetch.continueRequest" : "Fetch.failRequest",
        allowed ? { requestId } : { requestId, errorReason: "BlockedByClient" }, session);
    }

    pipe.onEvent = (method, params, session) => {
      if (method === "Target.attachedToTarget" && params.targetInfo.type === "page") {
        if (pages.size >= 4) { stop("navigation_limit"); return; }
        const page = { target: params.targetInfo.targetId };
        pages.set(params.sessionId, page);
        page.ready = configurePage(pipe, params.sessionId, page, config);
        void page.ready.catch(() => { if (pages.has(params.sessionId)) stop("browser_protocol"); });
      } else if (method === "Target.detachedFromTarget") {
        if (pages.delete(params.sessionId) && pages.size === 0) stop("browser_closed");
      } else if (method === "Fetch.requestPaused") {
        void requestPaused(params, session).catch(() => { if (pages.has(session)) stop("browser_protocol"); });
      } else if (method === "Page.frameNavigated" && !params.frame.parentId) {
        const page = pages.get(session);
        if (page) {
          const url = parseUrl(params.frame.url);
          page.frame = { id: params.frame.id, webclass: isWebClassPage(url, config) };
          if (page.frame.webclass) lmsReached = true;
          if (!config.origins.includes(url?.origin) && params.frame.url !== "about:blank") {
            stop("unknown_destination");
          }
        }
      } else if (method === "Page.navigatedWithinDocument") {
        const page = pages.get(session);
        if (page?.frame?.id === params.frameId) {
          page.frame = { id: params.frameId, webclass: isWebClassPage(parseUrl(params.url), config) };
        }
      } else if (method === "pipeError") {
        stop("browser_protocol");
      }
    };

    async function interact() {
      await pipe.send("Browser.setDownloadBehavior", { behavior: "deny" });
      // Even a retained profile must start a new, guarded tab after an interrupted run.
      const { targetInfos } = await pipe.send("Target.getTargets");
      if (targetInfos.some(({ type }) => type === "page")) {
        return { reason: "profile_startup_tabs" };
      }
      await pipe.send("Target.setAutoAttach", {
        autoAttach: true, waitForDebuggerOnStart: true, flatten: true,
        filter: [{ type: "page" }, { exclude: true }],
      });
      const { targetId } = await pipe.send("Target.createTarget", { url: "about:blank" });
      const [session, page] = [...pages].find(([, candidate]) => candidate.target === targetId) ?? [];
      if (!page) return { reason: "browser_protocol" };
      await page.ready;
      await pipe.send("Page.navigate", { url: config.entry }, session);
      if (fixture?.interact) await fixture.interact(pipe, session, () => edge.kill());
      while (!stopped) {
        for (const [tab, candidate] of pages) {
          const frame = candidate.frame;
          if (!candidate.configured || !frame?.webclass) continue;
          lmsReached = true;
          const verify = expression(loggedIn, config.dashboardPath);
          const ready = await evaluate(pipe, tab, frame.id, verify)
            .catch(() => false);
          if (!ready || stopped || candidate.frame !== frame) continue;
          armed = { session: tab, frameId: frame.id };
          const read = expression(readCourses, config.lms, config.coursesPath, config.maxBytes);
          return await evaluate(pipe, tab, frame.id, verify + " ? " + read +
            ' : ({ reason: "auth_expired" })', 20000)
            ?? { reason: "course_request_failed" };
        }
        await delay(250);
      }
      return stoppedResult;
    }

    outcome = await Promise.race([
      stoppedResult, interact().catch(() => ({ reason: "browser_protocol" })),
    ]);
    stopped = true;
    await pipe.send("Browser.close", {}, undefined, 2500).catch(() => {});
  } catch {
    outcome = { reason: "edge_start_failed" };
  } finally {
    clearTimeout(timeout);
    if (edgeExited) {
      const exited = await Promise.race([edgeExited.then(() => true), delay(3000, false, { ref: false })]);
      if (!exited) {
        edge.kill();
        await Promise.race([edgeExited, delay(1500)]);
      }
    }
    await proxy?.close();
  }
  return { ...outcome, courses_get: coursesGet, lms_reached: lmsReached };
}
