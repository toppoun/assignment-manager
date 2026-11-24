import { spawn } from "node:child_process";
import { once } from "node:events";
import { rm } from "node:fs/promises";
import { createServer } from "node:http";
import { connect } from "node:net";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";

// DevTools uses inherited pipes. No local or remote debugging port is opened.
class BrowserPipe {
  constructor(browser, maxBytes) {
    this.input = browser.stdio[3];
    this.pending = new Map();
    this.nextId = 0;
    this.buffer = "";
    this.onEvent = () => {};
    browser.stdio[4].setEncoding("utf8").on("data", (chunk) => {
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
    for (const stream of [browser.stdio[4], this.input]) stream.on("error", () => this.close());
    browser.once("exit", () => this.close());
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

// HTTPS tunnels stay opaque and use the browser's normal certificate validation.
export async function startProxy(origins, dial = connect, connectMillis = 15000) {
  const targets = new Map(origins.map((origin) => {
    const url = new URL(origin);
    const port = Number(url.port || 443);
    return [`${url.hostname}:${port}`, { origin, host: url.hostname, port }];
  }));
  let sequence = 0;
  const failures = new Map(origins.map((origin) => [origin, new Map()]));
  const sockets = new Set();
  const server = createServer((_request, response) => response.writeHead(403).end());
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.on("error", () => socket.destroy());
    socket.once("close", () => sockets.delete(socket));
  });
  server.on("connect", (request, client, head) => {
    const target = targets.get(request.url);
    if (!target) {
      client.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
      return;
    }
    const attempt = ++sequence;
    let state = "connecting";
    const upstream = dial({ host: target.host, port: target.port });
    const timer = setTimeout(() => fail("timeout"), connectMillis);
    function fail(cause) {
      if (state !== "connecting") return;
      state = "closed";
      const observed = failures.get(target.origin);
      observed.set(cause, Math.max(attempt, observed.get(cause) ?? 0));
      clearTimeout(timer);
      client.end("HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n");
      upstream.destroy();
    }
    upstream.once("connect", () => {
      if (state !== "connecting") return;
      state = "tunneled";
      clearTimeout(timer);
      client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
      if (head.length) upstream.write(head);
      upstream.pipe(client).pipe(upstream);
    });
    upstream.once("error", ({ code }) => {
      if (state === "connecting") fail(proxySocketCause(code));
      else if (state === "tunneled") client.destroy();
    });
    upstream.once("close", () => {
      if (state === "connecting") fail("connection_closed");
      else if (state === "tunneled") client.destroy();
    });
    client.once("close", () => {
      state = "closed";
      clearTimeout(timer);
      upstream.destroy();
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return {
    port: server.address().port,
    cursor: () => sequence,
    failureSince(origin, cursor) {
      const observed = [...(failures.get(origin) ?? [])].filter(([, attempt]) => attempt > cursor);
      return observed.length === 1 ? observed[0][0] : undefined;
    },
    close() {
      for (const socket of sockets) socket.destroy();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}

function proxySocketCause(code) {
  switch (code) {
    case "ENOTFOUND": case "EAI_AGAIN": case "ENODATA": return "dns_failure";
    case "ECONNREFUSED": return "connection_refused";
    case "ECONNRESET": return "connection_reset";
    case "ETIMEDOUT": return "timeout";
    default: return "unknown_network_failure";
  }
}

function browserNetworkCause({ errorText: code, blockedReason }) {
  if (blockedReason || /^net::ERR_BLOCKED_BY_[A-Z_]+$/.test(code)) return "navigation_blocked";
  if (/^net::ERR_(NAME_NOT_RESOLVED|NAME_RESOLUTION_FAILED|DNS_[A-Z_]+)$/.test(code)) return "dns_failure";
  if (/^net::ERR_(CERT_[A-Z_]+|SSL_[A-Z_]+|BAD_SSL_CLIENT_AUTH_CERT)$/.test(code)) return "tls_failure";
  if (/^net::ERR_(PROXY_[A-Z_]+|TUNNEL_CONNECTION_FAILED|MANDATORY_PROXY_CONFIGURATION_FAILED)$/.test(code)) return "proxy_failure";
  switch (code) {
    case "net::ERR_CONNECTION_REFUSED": return "connection_refused";
    case "net::ERR_CONNECTION_RESET": return "connection_reset";
    case "net::ERR_CONNECTION_CLOSED": case "net::ERR_EMPTY_RESPONSE": return "connection_closed";
    case "net::ERR_TIMED_OUT": case "net::ERR_CONNECTION_TIMED_OUT": return "timeout";
    default: return "unknown_network_failure";
  }
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

function navigationStage(url, config) {
  if (url?.protocol !== "https:") return null;
  const [hoppii, idp] = config.origins;
  if (url.origin === hoppii) return "hoppii";
  if (url.origin === idp) return "idp";
  if (url.origin === config.lms) {
    return url.pathname === config.ssoPostPath ? "webclass_sso" : "webclass";
  }
  return null;
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

async function readJson(origin, target, maxBytes, kind, referrer) {
  if (location.origin !== origin || !location.pathname.startsWith("/webclass/")) {
    return { reason: "acquisition_context_changed" };
  }
  try {
    const response = await fetch(target, {
      method: "GET", mode: "same-origin", credentials: "same-origin",
      redirect: "error", cache: "no-store", signal: AbortSignal.timeout(15000),
      headers: { Accept: "application/json" },
      referrer: referrer ?? undefined, referrerPolicy: referrer ? "same-origin" : undefined,
    });
    if (response.status === 401 || response.status === 403) return { reason: "auth_expired" };
    if (response.status !== 200 || response.url !== origin + target || response.redirected ||
        response.headers.has("content-range") || response.headers.has("link") ||
        response.headers.get("content-type")?.split(";")[0].trim().toLowerCase() !== "application/json") {
      return { reason: `invalid_${kind}_response` };
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
        return { reason: `${kind}_response_limit` };
      }
      body += decoder.decode(value, { stream: true });
    }
  } catch {
    return { reason: `${kind}_request_failed` };
  }
}

const expression = (fn, ...args) => "(" + fn.toString() + ")(" + args.map((arg) => JSON.stringify(arg)).join(",") + ")";

async function isolatedWorld(pipe, session, frameId) {
  const { executionContextId } = await pipe.send("Page.createIsolatedWorld", {
    frameId, worldName: "assignment-manager",
  }, session);
  return executionContextId;
}

async function evaluate(pipe, session, contextId, script, timeout) {
  const { result, exceptionDetails } = await pipe.send("Runtime.evaluate", {
    expression: script, contextId, returnByValue: true, awaitPromise: true,
  }, session, timeout);
  return exceptionDetails ? undefined : result.value;
}

function isGuardedApi(url, config) {
  let path = url.pathname;
  for (let pass = 0; pass < 3 && /%[a-f\d]{2}/i.test(path); pass++) path = decodeURIComponent(path);
  if (/%[a-f\d]{2}/i.test(path)) return true;
  path = path.toLowerCase().replaceAll("\\", "/").replace(/\/{2,}/g, "/");
  const api = config.coursesPath.slice(0, config.coursesPath.lastIndexOf("/") + 1);
  if (path.includes(api)) {
    const operation = path.slice(path.indexOf(api) + api.length);
    return /^(?:[\/;]*)(?:courses|contents)(?:$|[\/;.?#])/.test(operation);
  }
  return false;
}

function attachTargets(pipe, types, session) {
  return pipe.send("Target.setAutoAttach", {
    autoAttach: true, waitForDebuggerOnStart: true, flatten: true,
    filter: [...types.map((type) => ({ type })), { exclude: true }],
  }, session);
}

function guardTarget(pipe, session, config) {
  return pipe.send("Fetch.enable", { patterns: [
    { urlPattern: "*", resourceType: "Document" },
    { urlPattern: config.lms + "/*" },
  ] }, session);
}

const embeddedTargetTypes = ["iframe", "worker", "shared_worker", "service_worker"];

async function configureEmbedded(pipe, session, config, type) {
  if (type === "iframe") {
    await guardTarget(pipe, session, config);
  } else {
    // Worker targets lack Fetch interception. Only documents need WebClass traffic.
    await pipe.send("Network.setBlockedURLs", { urls: [config.lms + "/*"] }, session);
  }
  await attachTargets(pipe, embeddedTargetTypes, session);
  await pipe.send("Runtime.runIfWaitingForDebugger", {}, session);
}

async function configurePage(pipe, session, page, config, onFrame) {
  await guardTarget(pipe, session, config);
  await attachTargets(pipe, embeddedTargetTypes, session);
  await pipe.send("Runtime.runIfWaitingForDebugger", {}, session);
  await pipe.send("Network.setBypassServiceWorker", { bypass: true }, session);
  // Diagnostics are optional on browser versions that cannot enable this domain here.
  await pipe.send("Network.enable", { maxPostDataSize: 0, maxTotalBufferSize: 0, maxResourceBufferSize: 0 }, session)
    .catch(() => {});
  await pipe.send("Page.enable", {}, session);
  const previousFrame = page.frame;
  const { frameTree } = await pipe.send("Page.getFrameTree", {}, session);
  if (page.frame === previousFrame) onFrame(page, frameTree.frame);
  page.configured = true;
}

export async function runBrowser(config, fixture, selectContents, signal) {
  let browser;
  let browserExited;
  let browserReady = false;
  let proxy;
  const isMac = process.platform === "darwin";
  const startupFailure = isMac ? "chrome_start_failed" : "edge_start_failed";
  let stopped = false;
  let lmsReached = false;
  const requests = { course: 0, contents: 0 };
  let finish;
  const stoppedResult = new Promise((resolve) => { finish = resolve; });
  const stop = (reason, navigation) => {
    if (stopped) return;
    stopped = true;
    finish(Promise.resolve(navigation).then((navigation) => ({ reason, navigation })));
  };
  const interrupted = () => stop("browser_closed");
  signal?.addEventListener("abort", interrupted, { once: true });
  if (signal?.aborted) interrupted();
  const timeout = setTimeout(() => stop("login_timeout"), config.loginMillis);
  let outcome;
  try {
    proxy = await startProxy(config.origins.filter((origin) => new URL(origin).protocol === "https:"),
      fixture?.dial, fixture?.connectMillis);
    if (stopped) throw new Error();
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
    browser = spawn(config.executable, flags, { stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"] });
    browserExited = new Promise((resolve) => {
      browser.once("exit", () => {
        stop(isMac && !browserReady ? startupFailure : "browser_closed");
        resolve(true);
      });
      browser.once("error", () => {
        if (browser.pid === undefined) {
          stop(startupFailure);
          resolve(true);
        }
      });
    });
    const pipe = new BrowserPipe(browser, config.maxBytes * 6 + 65536);
    const pages = new Map();
    const embedded = new Set();
    let armed;
    let reading = false;
    let documents = 0;

    async function describeNetworkFailure(page, frameId, loaderId, stage) {
      const matching = () => [...page.documents.values()].find((document) =>
        document.frameId === frameId && document.loaderId === loaderId && document.stage === stage);
      // Failure and error-page notifications can arrive on either side of one another.
      if (!matching()?.cause) await delay(200);
      const document = matching();
      return {
        stage, kind: "browser_error_page",
        cause: document?.cause ?? "unknown_network_failure",
        request_method: document?.method,
        proxy_connect_observed: document?.proxyConnectObserved,
      };
    }

    function recordFrame(page, frame) {
      const initial = page.initialNavigations.get(frame.id);
      page.initialNavigations.clear();
      if (initial) page.stage = initial.stage;
      const url = parseUrl(frame.url);
      page.frame = { id: frame.id, webclass: isWebClassPage(url, config) };
      if (page.frame.webclass) lmsReached = true;
      if (initial?.failure) {
        stop(initial.failure.reason, initial.failure.navigation);
        return;
      }

      const stage = navigationStage(url, config);
      if (stage) {
        page.stage = stage;
      } else if (frame.url === "chrome-error://chromewebdata/") {
        const failedStage = navigationStage(parseUrl(frame.unreachableUrl), config);
        stop("browser_navigation_failed", describeNetworkFailure(page, frame.id, frame.loaderId, failedStage ?? page.stage));
      } else if (frame.url !== "about:blank") {
        const internal = ["about:", "chrome:", "edge:"].includes(url?.protocol);
        stop("unknown_destination", { stage: page.stage,
          kind: internal ? "browser_internal_page" : "top_level_commit", cause: "navigation_blocked" });
      }
    }

    // Interception only reads routing metadata, never request headers or form fields.
    async function requestPaused({ request: { url: raw, method }, requestId, resourceType, frameId }, session) {
      const url = parseUrl(raw);
      const document = resourceType === "Document";
      const page = pages.get(session);
      const mainFrame = page?.frame;
      const topLevel = document && mainFrame && mainFrame.id === frameId;
      const initializing = document && page && !mainFrame;
      const stage = document ? navigationStage(url, config) : null;
      const navigationAllowed = !document || (stage !== null && /^(GET|POST)$/.test(method));
      let allowed = !stopped && navigationAllowed;
      if (!navigationAllowed && topLevel) {
        stop(stage ? "navigation_method_blocked" : "unknown_destination",
          { stage: stage ?? page.stage, kind: "top_level_request", cause: "navigation_blocked" });
      }
      if ((topLevel || initializing) && ++documents > 128) {
        stop("navigation_limit");
        allowed = false;
      }
      if (allowed && url?.origin === config.lms) {
        if (reading || isGuardedApi(url, config)) {
          allowed = armed && (resourceType === "XHR" || resourceType === "Fetch") && method === "GET" &&
            session === armed.session && frameId === armed.frame.id && mainFrame === armed.frame &&
            url.href === config.lms + armed.target && requests[armed.kind] === 0;
          if (allowed) requests[armed.kind]++;
        } else if (method !== "GET" && method !== "HEAD") {
          // A CSSO form can include a query on this fixed endpoint.
          allowed = document && method === "POST" && url.pathname === config.ssoPostPath;
        }
      }
      if (initializing) {
        // A popup can pause its initial request before Page.getFrameTree responds.
        // Answer the interception immediately and defer only the terminal diagnostic.
        const initial = page.initialNavigations.get(frameId) ?? { stage: page.stage };
        if (allowed) initial.stage = stage;
        if (!navigationAllowed) initial.failure ??= {
          reason: stage ? "navigation_method_blocked" : "unknown_destination",
          navigation: { stage: stage ?? initial.stage, kind: "top_level_request", cause: "navigation_blocked" },
        };
        page.initialNavigations.set(frameId, initial);
      } else if (allowed && topLevel) {
        page.stage = stage;
      }
      await pipe.send(allowed ? "Fetch.continueRequest" : "Fetch.failRequest",
        allowed ? { requestId } : { requestId, errorReason: "BlockedByClient" }, session);
    }

    pipe.onEvent = (method, params, session) => {
      if (method === "Target.attachedToTarget" && params.targetInfo.type === "page") {
        if (pages.size >= 4) { stop("navigation_limit"); return; }
        const { targetId, openerId } = params.targetInfo;
        const opener = [...pages.values()].find(({ target }) => target === openerId);
        const page = { target: targetId, stage: opener?.stage ?? "startup", initialNavigations: new Map(), documents: new Map() };
        pages.set(params.sessionId, page);
        page.ready = configurePage(pipe, params.sessionId, page, config, recordFrame);
        void page.ready.catch(() => { if (pages.has(params.sessionId)) stop("browser_protocol"); });
      } else if (method === "Target.attachedToTarget" && embeddedTargetTypes.includes(params.targetInfo.type)) {
        const child = params.sessionId;
        embedded.add(child);
        void configureEmbedded(pipe, child, config, params.targetInfo.type)
          .catch(() => { if (embedded.has(child)) stop("browser_protocol"); });
      } else if (method === "Target.detachedFromTarget") {
        embedded.delete(params.sessionId);
        if (pages.delete(params.sessionId) && pages.size === 0) stop("browser_closed");
      } else if (method === "Fetch.requestPaused") {
        void requestPaused(params, session).catch(() => {
          if (pages.has(session) || embedded.has(session)) stop("browser_protocol");
        });
      } else if (method === "Network.requestWillBeSent" && params.type === "Document") {
        const page = pages.get(session);
        if (!page || !params.frameId || !params.loaderId || (page.frame && page.frame.id !== params.frameId)) return;
        const url = parseUrl(params.request.url);
        const stage = navigationStage(url, config);
        const requestMethod = params.request.method;
        if (!stage || (requestMethod !== "GET" && requestMethod !== "POST")) {
          page.documents.delete(params.requestId);
          return;
        }
        if (page.documents.size >= 128 && !page.documents.has(params.requestId)) {
          page.documents.delete(page.documents.keys().next().value);
        }
        page.documents.set(params.requestId, {
          frameId: params.frameId, loaderId: params.loaderId, stage, method: requestMethod.toLowerCase(),
          origin: url.origin, proxyCursor: proxy.cursor(),
        });
      } else if (method === "Network.loadingFailed" && params.type === "Document") {
        const document = pages.get(session)?.documents.get(params.requestId);
        if (document) {
          document.cause = browserNetworkCause(params);
          if (params.errorText === "net::ERR_TUNNEL_CONNECTION_FAILED" ||
              params.errorText === "net::ERR_PROXY_UNABLE_TO_CONNECT_TO_DESTINATION") {
            document.proxyConnectObserved = proxy.failureSince(document.origin, document.proxyCursor);
          }
        }
      } else if (method === "Network.loadingFinished") {
        pages.get(session)?.documents.delete(params.requestId);
      } else if (method === "Page.frameNavigated" && !params.frame.parentId) {
        const page = pages.get(session);
        if (page) recordFrame(page, params.frame);
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
      browserReady = true;
      // Even a retained profile must start a new, guarded tab after an interrupted run.
      const { targetInfos } = await pipe.send("Target.getTargets");
      if (targetInfos.some(({ type }) => type === "page")) {
        return { reason: "profile_startup_tabs" };
      }
      await attachTargets(pipe, ["page", "shared_worker", "service_worker"]);
      const { targetId } = await pipe.send("Target.createTarget", { url: "about:blank" });
      const [session, page] = [...pages].find(([, candidate]) => candidate.target === targetId) ?? [];
      if (!page) return { reason: "browser_protocol" };
      await page.ready;
      if (stopped) return stoppedResult;
      await pipe.send("Page.navigate", { url: config.entry }, session);
      if (stopped) return stoppedResult;
      if (fixture?.interact) await fixture.interact(pipe, session, (signal) => browser.kill(signal));
      while (!stopped) {
        for (const [tab, candidate] of pages) {
          const frame = candidate.frame;
          if (!candidate.configured || !frame?.webclass) continue;
          lmsReached = true;
          const verify = expression(loggedIn, config.dashboardPath);
          const context = await isolatedWorld(pipe, tab, frame.id).catch(() => undefined);
          const ready = context !== undefined && await evaluate(pipe, tab, context, verify).catch(() => false);
          if (!ready || stopped || candidate.frame !== frame) continue;
          reading = true;
          async function read(kind, target, referrer = null) {
            if (stopped || candidate.frame !== frame) return { reason: "auth_expired" };
            armed = { session: tab, frame, kind, target };
            const fetchJson = expression(readJson, config.lms, target, config.maxBytes, kind, referrer);
            try {
              return await evaluate(pipe, tab, context, verify + " ? " + fetchJson +
                ' : ({ reason: "auth_expired" })', 20000) ?? { reason: `${kind}_request_failed` };
            } catch {
              return { reason: `${kind}_request_failed` };
            } finally {
              armed = undefined;
            }
          }
          const courses = await read("course", config.coursesPath);
          if (courses.reason !== "ok" || !selectContents) return courses;
          if (stopped || requests.course !== 1 || candidate.frame !== frame) return { reason: "auth_expired" };
          const target = await selectContents(courses.body);
          if (target === null) return { reason: "course_selection_stopped" };
          const url = parseUrl(config.lms + target);
          if (url?.origin !== config.lms || url.pathname !== config.contentsPath || url.hash ||
              url.searchParams.size !== 1 || !url.searchParams.get("group_id")) {
            return { reason: "browser_protocol" };
          }
          return read("contents", target, config.lms + config.dashboardPath);
        }
        await delay(250);
      }
      return stoppedResult;
    }

    outcome = await Promise.race([
      stoppedResult, interact().catch(() => ({ reason: isMac && !browserReady ? startupFailure : "browser_protocol" })),
    ]);
    if (stopped) outcome = await stoppedResult;
    stopped = true;
    await pipe.send("Browser.close", {}, undefined, 2500).catch(() => {});
  } catch {
    outcome = stopped ? await stoppedResult : { reason: proxy ? startupFailure : "proxy_failure" };
  } finally {
    clearTimeout(timeout);
    signal?.removeEventListener("abort", interrupted);
    if (browserExited) {
      const exited = await Promise.race([browserExited, delay(3000, false, { ref: false })]);
      if (!exited) {
        browser.kill();
        const terminated = await Promise.race([browserExited, delay(1500, false)]);
        if (isMac && !terminated) {
          browser.kill("SIGKILL");
          if (!await Promise.race([browserExited, delay(1500, false)])) {
            outcome = { reason: "profile_cleanup_failed" };
            for (const stream of browser.stdio) stream?.destroy();
            browser.unref();
          }
        }
      }
    }
    await proxy?.close();
  }
  return { ...outcome, courses_get: requests.course, contents_get: requests.contents, lms_reached: lmsReached };
}

async function runDriver(fixture) {
  const input = createInterface({ input: process.stdin });
  const lines = input[Symbol.asyncIterator]();
  const write = (message) => process.stdout.write(JSON.stringify(message) + "\n");
  const isMac = process.platform === "darwin";
  const parent = new AbortController();
  const interrupt = () => parent.abort();
  if (isMac) {
    // The CLI's pipe closes even when it is killed before it can run destructors.
    input.once("close", interrupt);
    process.stdout.on("error", interrupt);
    process.on("SIGTERM", interrupt);
    process.on("SIGINT", interrupt);
  }
  try {
    const { value, done } = await lines.next();
    if (done) return;
    const config = JSON.parse(value);
    const selectContents = config.selectContents ? async (body) => {
      write({ stage: "courses", body });
      const { value, done } = await lines.next();
      return done ? null : JSON.parse(value);
    } : undefined;
    const reply = await runBrowser(config, fixture, selectContents, isMac ? parent.signal : undefined);
    if (parent.signal.aborted && config.temporaryProfile && reply.reason !== "profile_cleanup_failed") {
      await rm(config.profile, { recursive: true, force: true });
    }
    if (!process.stdout.destroyed) write({ stage: "finished", reply });
  } finally {
    if (isMac) {
      process.off("SIGTERM", interrupt);
      process.off("SIGINT", interrupt);
    }
    input.close();
    process.stdin.destroy();
  }
}
