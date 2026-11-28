import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, createPrivateKey, X509Certificate } from "node:crypto";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:https";
import { Socket, connect, createServer as createTcpServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import { test } from "node:test";
import { runBrowser, startProxy } from "../src/webclass_browser.mjs";

const isMac = process.platform === "darwin";
const browserName = isMac ? "Chrome" : "Edge";
const run = (isMac && process.env.ASSIGNMENT_MANAGER_TEST_CHROME === "1") ||
  (process.platform === "win32" && process.env.ASSIGNMENT_MANAGER_TEST_EDGE === "1");

test("the loopback proxy rejects an unlisted CONNECT before dialing", async () => {
  let dialed = false;
  const proxy = await startProxy(["https://127.0.0.1:443"], () => { dialed = true; });
  try {
    const client = connect({ host: "127.0.0.1", port: proxy.port });
    client.setEncoding("ascii");
    client.write("CONNECT 127.0.0.1:444 HTTP/1.1\r\nHost: 127.0.0.1:444\r\n\r\n");
    let reply = "";
    for await (const chunk of client) reply += chunk;
    assert.match(reply, /^HTTP\/1\.1 403 Forbidden\r\n/);
    assert.equal(dialed, false);
  } finally {
    await proxy.close();
  }
});

test("the loopback proxy records a stalled CONNECT after the observation cursor", { timeout: 2000 }, async () => {
  const origin = "https://127.0.0.1:443";
  const proxy = await startProxy([origin], () => new Socket(), 25);
  try {
    const cursor = proxy.cursor();
    const client = connect({ host: "127.0.0.1", port: proxy.port });
    client.setEncoding("ascii");
    client.write("CONNECT 127.0.0.1:443 HTTP/1.1\r\nHost: 127.0.0.1:443\r\n\r\n");
    let reply = "";
    for await (const chunk of client) reply += chunk;
    assert.match(reply, /^HTTP\/1\.1 502 Bad Gateway\r\n/);
    assert.equal(proxy.failureSince(origin, cursor), "timeout");
    assert.equal(proxy.failureSince(origin, proxy.cursor()), undefined);
  } finally {
    await proxy.close();
  }
});

test(`isolated ${browserName} reads only the requested WebClass API endpoints over synthetic HTTPS`, { skip: !run, timeout: 180000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), "webclass-browser-fixture-"));
  const readFixture = (path, encoding) => readFile(new URL(path, import.meta.url), encoding);
  const [courses, contents, certDer, keyDer, driver, cssoForm] = await Promise.all([
    readFixture("../fixtures/webclass/courses_basic.json", "utf8"),
    readFixture("../fixtures/webclass/contents_basic.json", "utf8"),
    readFixture("../fixtures/network/server.der"),
    readFixture("../fixtures/network/server-key.der"),
    readFixture("../src/webclass_browser.mjs", "utf8"),
    readFixture("../fixtures/webclass_auth/hoppii_csso_form.html", "utf8"),
  ]);
  const certificate = new X509Certificate(certDer);
  const key = createPrivateKey({ key: keyDer, format: "der", type: "pkcs8" }).export({ type: "pkcs8", format: "pem" });
  const tls = { cert: certificate.toString(), key };
  const pin = createHash("sha256").update(certificate.publicKey.export({ type: "spki", format: "der" })).digest("base64");
  const coursesPath = "/webclass/ip_mods.php/plugin/score_summary_table/courses";
  const dashboardPath = "/webclass/ip_mods.php/plugin/score_summary_table/dashboard";
  const contentsPath = coursesPath.replace(/courses$/, "contents");
  const ssoPostPath = "/webclass/singlesignon.php";
  const loginPath = "/webclass/login.php";
  const cssoQuery = "?ticket=fixture-private-csso-query";
  const scienceGroup = "fixture-scientific-english-2";
  const scienceCourses = JSON.stringify(JSON.parse(courses).map((course) => course.group_id === "fixture-course-002"
    ? { ...course, group_id: scienceGroup, group_name: "科学英語2（合成）" } : course));
  const scienceContents = contents.replaceAll("fixture-course-002", scienceGroup);
  const special = " fixture-course+日本 &group_id=other/%?# ";
  const selectPath = (group) => contentsPath + "?" + new URLSearchParams({ group_id: group });
  const servers = [];
  const counters = { portal: 0, consent: 0, remembered: 0, acs: 0, queriedCsso: 0,
    courses: 0, contents: 0, unknown: 0, mutation: 0, login: 0 };
  const contentsRequests = [];
  const secret = "synthetic-unexported-session";
  const failedNavigation = "/disconnected?state=fixture-private-navigation";
  let resultMode = "ok";
  let initialCookieSeen = false;
  let handoffOrigin;
  const usesPortalCsso = () => resultMode === "csso-query" || resultMode === "csso-unapproved-post";

  async function listen(server) {
    servers.push(server);
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    return "https://127.0.0.1:" + server.address().port;
  }

  function serve(handler) {
    return listen(createServer(tls, handler));
  }

  const html = (response, body) => response.writeHead(200, {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
  }).end("<!doctype html><meta charset=utf-8>" + body);

  try {
    const unknown = await serve((_request, response) => {
      counters.unknown++;
      response.writeHead(404).end();
    });
    const forwardAssertion = (response) => html(response,
      '<form id="assertion" action="' + (usesPortalCsso() ? portal + "/saml/acs" : (handoffOrigin ?? lms) + ssoPostPath) + '" method="post">' +
      '<input type="hidden" name="SAMLResponse" value="fixture-assertion"></form>' +
      '<script>localStorage.setItem("synthetic-consent", "fixture");document.getElementById("assertion").submit();</script>');
    const idp = await serve((request, response) => {
      if (request.url === "/consent" && request.method === "GET") {
        const target = resultMode.startsWith("popup") ? ' target="_blank"' : "";
        const checkRemembered = resultMode === "reauth" ? "" :
          '<script>if (localStorage.getItem("synthetic-consent") === "fixture") location.replace("/remembered");</script>';
        html(response, '<form action="/continue" method="post"' + target + '><button id="accept">Accept</button></form>' + checkRemembered);
      } else if (request.url === "/remembered" && request.method === "GET") {
        counters.remembered++;
        if (resultMode === "navigation-error") {
          response.writeHead(302, { Location: failedNavigation }).end();
        } else if (resultMode === "embedded") {
          // Wait for the embedded frame and image attempts before completing SSO.
          html(response, '<body><script>' +
            'let remaining = 3; const next = () => { if (--remaining === 0) location.replace("/assertion"); };' +
            'for (const [tag, source] of ' + JSON.stringify([
              ["iframe", unknown + "/frame"], ["iframe", failedNavigation], ["img", unknown + "/image"],
            ]) + ') {' +
            'const child = document.createElement(tag); child.onload = child.onerror = next;' +
            'child.src = source; document.body.appendChild(child); }</script>');
        } else {
          forwardAssertion(response);
        }
      } else if (request.url === failedNavigation) {
        request.socket.destroy();
      } else if (request.url === "/assertion" && resultMode === "embedded") {
        forwardAssertion(response);
      } else if (request.url === "/continue" && request.method === "POST") {
        counters.consent++;
        if (resultMode === "unknown" || resultMode === "popup-unknown") {
          response.writeHead(302, { Location: unknown + "/blocked" }).end();
        } else if (resultMode === "navigation-error") {
          response.writeHead(302, { Location: failedNavigation }).end();
        } else {
          forwardAssertion(response);
        }
      } else {
        response.writeHead(404).end();
      }
    });
    const lms = await serve((request, response) => {
      if ((request.url === ssoPostPath || request.url === ssoPostPath + cssoQuery) && request.method === "POST") {
        if (resultMode === "sso-error") {
          request.socket.destroy();
          return;
        }
        counters.acs++;
        if (request.url === ssoPostPath + cssoQuery) counters.queriedCsso++;
        request.resume().on("end", () => response.writeHead(303, {
          Location: "/webclass/",
          "Set-Cookie": "fixture=" + secret + "; Path=/webclass; HttpOnly; Secure; SameSite=Lax" +
            (resultMode.startsWith("operation-") && resultMode !== "operation-session-only" ? "; Max-Age=300" : ""),
        }).end());
      } else if (new URL(request.url, lms).pathname === loginPath) {
        counters.login++;
        html(response, '<input type="password">');
      } else if (request.url === dashboardPath) {
        if (resultMode === "operation-idp") {
          response.writeHead(302, { Location: idp + "/consent" }).end();
        } else if (resultMode === "operation-document-forbidden") {
          response.writeHead(403).end();
        } else if (resultMode === "operation-unsigned-dashboard") {
          html(response, '<p>Anonymous</p><div hidden><input type="password"><a href="/webclass/logout.php">ログアウト</a></div>');
        } else if (request.headers.cookie !== "fixture=" + secret) {
          response.writeHead(302, { Location: loginPath }).end();
        } else if (resultMode === "operation-visible-login") {
          html(response, '<input hidden type="password"><input type="password"><a href="/webclass/logout.php">ログアウト</a>');
        } else {
          html(response, '<a href="/webclass/logout.php">ログアウト</a><div hidden><input type="password"></div>');
        }
      } else if (request.url === "/webclass/") {
        if (request.headers.cookie !== "fixture=" + secret) {
          response.writeHead(401).end();
          return;
        }
        const loggedIn = '<a href="/webclass/logout.php">ログアウト</a>';
        const blockedPosts = resultMode === "csso-query" ? ["/webclass/submit", ssoPostPath + cssoQuery] : ["/webclass/submit"];
        const loginIndicator =
          '<script>const worker = new Worker("/webclass/guard-worker.js");' +
          'Promise.all([new Promise(resolve => worker.onmessage = resolve),' +
          '...' + JSON.stringify(blockedPosts) + '.map(path => fetch(path, { method: "POST" }).catch(()=>{}))]).then(() =>' +
          'document.body.insertAdjacentHTML("afterbegin", ' + JSON.stringify(loggedIn) + '));</script>';
        html(response, loginIndicator +
          '<script>for (const path of ' + JSON.stringify([contentsPath, selectPath("fixture-course-003"),
            contentsPath.replace("contents", "%63ontents") + "?group_id=fixture-course-003", coursesPath, coursesPath]) +
          ') fetch(path).catch(()=>{});</script>');
      } else if (request.url === "/webclass/guard-worker.js") {
        response.writeHead(200, { "Content-Type": "text/javascript" }).end(
          'Promise.all(' + JSON.stringify([coursesPath, selectPath("fixture-course-003")]) +
          '.map(path => fetch(path).catch(()=>{}))).then(() => postMessage("guard-checked"));');
      } else if (request.url === coursesPath && request.method === "GET") {
        counters.courses++;
        if (request.headers.cookie !== "fixture=" + secret || resultMode === "expired") {
          response.writeHead(401).end();
        } else if (resultMode === "redirect") {
          response.writeHead(302, { Location: "/webclass/blocked" }).end();
        } else if (resultMode === "operation-login-redirect") {
          response.writeHead(302, { Location: loginPath + "?fixture-query=private" }).end();
        } else if (resultMode === "operation-login-html") {
          html(response, '<input type="password">');
        } else if (resultMode === "operation-server") {
          response.writeHead(503).end();
        } else if (resultMode === "operation-network") {
          request.socket.destroy();
        } else {
          const body = resultMode === "csso-query" ? scienceCourses :
            resultMode === "special" ? courses.replace("fixture-course-002", special) : courses;
          response.writeHead(200, { "Content-Type": "application/json" }).end(body);
        }
      } else if (new URL(request.url, lms).pathname.replace("%63", "c") === contentsPath) {
        counters.contents++;
        contentsRequests.push({ url: request.url, method: request.method, referrer: request.headers.referer, accept: request.headers.accept });
        if (request.headers.cookie !== "fixture=" + secret || resultMode === "contents-expired") {
          response.writeHead(403).end();
        } else if (resultMode === "operation-contents-auth-redirect") {
          response.writeHead(302, { Location: loginPath + "?fixture-query=private" }).end();
        } else if (resultMode === "operation-contents-login-html") {
          html(response, '<input type="password">');
        } else if (resultMode === "contents-redirect") {
          response.writeHead(302, { Location: selectPath("fixture-course-003") }).end();
        } else if (resultMode === "partial") {
          response.writeHead(200, { "Content-Type": "application/json", Link: '<next>; rel="next"' }).end(contents);
        } else if (resultMode === "html") {
          html(response, "<form>Login</form>");
        } else if (resultMode === "large") {
          response.writeHead(200, { "Content-Type": "application/json" }).end(" ".repeat(1024 * 1024 + 1));
        } else {
          const body = resultMode === "csso-query" ? scienceContents :
            resultMode === "special" ? contents.replaceAll("fixture-course-002", special) : contents;
          response.writeHead(200, { "Content-Type": "application/json" }).end(body);
        }
      } else {
        if (request.method === "POST") counters.mutation++;
        if (request.url === "/webclass/blocked") counters.unknown++;
        response.writeHead(404).end();
      }
    });
    const portal = await serve((request, response) => {
      counters.portal++;
      if (request.headers.cookie) initialCookieSeen = true;
      if (usesPortalCsso() && request.url === "/saml/acs" && request.method === "POST") {
        request.resume().on("end", () => response.writeHead(303, { Location: "/portal/sso/Parm.php" }).end());
        return;
      }
      if (usesPortalCsso() && request.url === "/portal/sso/Parm.php") {
        const target = resultMode === "csso-query" ? ssoPostPath : "/webclass/submit";
        html(response, cssoForm.replace('action="' + ssoPostPath + '"', 'action="' + lms + target + cssoQuery + '"') +
          '<script>document.forms[0].requestSubmit(document.querySelector("input[type=submit]"));</script>');
        return;
      }
      response.writeHead(302, { Location: idp + "/consent" }).end();
    });
    // The synthetic pin and loopback routes are supplied only by this test.
    const certificateFlag = "--ignore-certificate-errors-spki-list=" + pin;
    const flags = [certificateFlag,
      "--proxy-bypass-list=<-loopback>;" + [portal, idp, lms].map((url) => new URL(url).host).join(";")];
    const executable = isMac ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" :
      join(process.env["ProgramFiles(x86)"], "Microsoft", "Edge", "Application", "msedge.exe");

    function config(profile, options = {}) {
      return {
        executable, profile: join(root, profile), entry: portal + "/portal",
        origins: [portal, idp, lms], lms, coursesPath, contentsPath, dashboardPath, ssoPostPath,
        loginPath, maxBytes: 1024 * 1024, loginMillis: 6000, ...options,
      };
    }

    async function testRun(profile, interact, options = {}) {
      return runBrowser(config(profile, options), { flags, interact });
    }

    async function checkSelected(profile, group, interact, browserFlags = flags) {
      const options = `{ flags: ${JSON.stringify(browserFlags)}, interact: ${interact?.toString() ?? "undefined"} }`;
      const child = spawn(process.execPath, ["--input-type=module", "--eval", driver + "\nawait runDriver(" + options + ");"],
        { stdio: ["pipe", "pipe", "inherit"] });
      const exit = once(child, "close");
      const output = createInterface({ input: child.stdout });
      const lines = output[Symbol.asyncIterator]();
      const send = (value) => child.stdin.write(JSON.stringify(value) + "\n");
      try {
        send(config(profile, { selectContents: true }));
        let done = JSON.parse((await lines.next()).value);
        if (done.stage === "courses") {
          assert.ok(!JSON.stringify(done).includes(secret));
          // Stand in for Rust's pipe response. Rust tests exercise real validation and query encoding.
          const listed = new Set(JSON.parse(done.body).map(({ group_id }) => group_id));
          send(Array.isArray(group)
            ? group.filter((id) => listed.has(id)).map(selectPath)
            : listed.has(group) ? selectPath(group) : null);
          done = JSON.parse((await lines.next()).value);
        }
        assert.equal(done.stage, "finished");
        assert.ok(!JSON.stringify(done).includes(secret));
        assert.ok(!JSON.stringify(done).includes("fixture-private-navigation"));
        assert.ok(!JSON.stringify(done).includes("fixture-private-csso-query"));
        assert.equal((await lines.next()).done, true);
        assert.deepEqual(await exit, [0, null]);
        return done.reply;
      } finally {
        child.stdin.end();
        output.close();
        await exit;
      }
    }

    async function clickFixtureConsent(pipe, session) {
      for (let attempt = 0; attempt < 50; attempt++) {
        const { result } = await pipe.send("Runtime.evaluate", {
          expression: '(() => { const button = document.querySelector("#accept"); if (button) button.click(); return !!button })()',
          returnByValue: true, userGesture: true,
        }, session).catch(() => ({ result: {} }));
        if (result.value) return;
        await delay(50);
      }
      throw new Error("synthetic fixture did not get to the consent button");
    }

    const ok = await testRun("confirmed", clickFixtureConsent);
    assert.equal(ok.reason, "ok");
    assert.equal(ok.courses_get, 1);
    assert.equal(ok.lms_reached, true);
    assert.equal(JSON.parse(ok.body).length, 3);
    assert.ok(!JSON.stringify(ok).includes(secret));

    const retained = await testRun("confirmed");
    assert.equal(retained.reason, "ok");
    assert.equal(retained.courses_get, 1);
    assert.equal(counters.remembered, 1);
    assert.equal(counters.consent, 1);

    resultMode = "reauth";
    const interrupted = await testRun("confirmed", (_pipe, _session, terminate) =>
      terminate(isMac ? "SIGKILL" : "SIGTERM"));
    assert.equal(interrupted.reason, "browser_closed");
    assert.equal(interrupted.courses_get, 0);
    const renewed = await testRun("confirmed", clickFixtureConsent);
    assert.equal(renewed.reason, "ok");
    assert.equal(renewed.courses_get, 1);
    assert.equal(counters.remembered, 1);
    assert.equal(counters.consent, 2);

    resultMode = "popup";
    const popup = await testRun("popup", clickFixtureConsent);
    assert.equal(popup.reason, "ok");
    assert.equal(popup.courses_get, 1);

    const separate = await testRun("separate", undefined, { loginMillis: 2500 });
    assert.equal(separate.reason, "login_timeout");
    assert.equal(separate.courses_get, 0);
    assert.equal(separate.lms_reached, false);

    resultMode = "unknown";
    const unknownNav = await testRun("unknown", clickFixtureConsent);
    assert.equal(unknownNav.reason, "unknown_destination");
    assert.equal(unknownNav.courses_get, 0);
    assert.deepEqual(unknownNav.navigation, { stage: "idp", kind: "top_level_request", cause: "navigation_blocked" });

    resultMode = "popup-unknown";
    const unknownPopup = await testRun("popup-unknown", clickFixtureConsent);
    assert.equal(unknownPopup.reason, "unknown_destination");
    assert.deepEqual(unknownPopup.navigation, { stage: "idp", kind: "top_level_request", cause: "navigation_blocked" });
    assert.equal(unknownPopup.courses_get, 0);

    const closed = await testRun("closed", async (pipe) => {
      await pipe.send("Browser.close").catch(() => {});
    });
    assert.equal(closed.reason, "browser_closed");
    assert.equal(closed.courses_get, 0);

    resultMode = "expired";
    const expired = await testRun("expired", clickFixtureConsent);
    assert.equal(expired.reason, "auth_expired");
    assert.equal(expired.courses_get, 1);

    resultMode = "redirect";
    const redirected = await testRun("redirect", clickFixtureConsent);
    assert.equal(redirected.reason, "unexpected_api_redirect");
    assert.equal(redirected.courses_get, 1);
    assert.equal(counters.acs, 6);
    assert.equal(counters.courses, 6);
    assert.equal(counters.contents, 0);
    assert.equal(counters.unknown, 0);
    assert.equal(initialCookieSeen, false);

    resultMode = "ok";
    const oneCourse = await checkSelected("assignment-fresh", "fixture-course-002", clickFixtureConsent);
    assert.equal(oneCourse.reason, "ok");
    assert.equal(oneCourse.courses_get, 1);
    assert.equal(oneCourse.contents_get, 1);
    assert.deepEqual(JSON.parse(oneCourse.body), JSON.parse(contents));

    resultMode = "special";
    const reused = await checkSelected("confirmed", special);
    assert.equal(reused.reason, "ok");
    assert.equal(reused.courses_get, 1);
    assert.equal(reused.contents_get, 1);
    assert.ok(JSON.parse(reused.body).every(({ group_id }) => group_id === special));
    assert.equal(counters.remembered, 2);

    resultMode = "ok";
    const missing = await checkSelected("confirmed", "not-in-courses");
    assert.equal(missing.reason, "course_selection_stopped");
    assert.equal(missing.courses_get, 1);
    assert.equal(missing.contents_get, 0);
    for (const [mode, diagnostic] of [
      ["contents-expired", "access_denied"], ["contents-redirect", "unexpected_api_redirect"],
      ["partial", "invalid_contents_response"], ["html", "invalid_contents_response"],
      ["large", "contents_response_limit"],
    ]) {
      resultMode = mode;
      const result = await checkSelected("confirmed", "fixture-course-002");
      assert.equal(result.reason, diagnostic, mode);
      assert.equal(result.courses_get, 1, mode);
      assert.equal(result.contents_get, 1, mode);
      assert.equal(result.body, undefined, mode);
    }
    assert.equal(counters.contents, 7);
    assert.equal(counters.courses, 14);
    assert.deepEqual(contentsRequests.map(({ url }) => url), [
      selectPath("fixture-course-002"), selectPath(special), ...Array(5).fill(selectPath("fixture-course-002")),
    ]);
    assert.ok(contentsRequests.every(({ method, referrer, accept }) =>
      method === "GET" && referrer === lms + dashboardPath && accept === "application/json"));
    assert.equal(counters.unknown, 0);
    assert.equal(counters.mutation, 0);

    resultMode = "navigation-error";
    for (const [profile, interact] of [["confirmed", undefined], ["failed-fresh", clickFixtureConsent]]) {
      const failedIdp = await checkSelected(profile, "fixture-course-002", interact);
      assert.deepEqual(failedIdp, {
        reason: "browser_navigation_failed",
        navigation: { stage: "idp", kind: "browser_error_page", cause: "connection_closed", request_method: "get" },
        diagnostic: { failure: { stage: "navigation", reason: "navigation_failed" }, shutdown: "clean" },
        lms_reached: false, courses_get: 0, contents_get: 0,
      });
    }
    resultMode = "sso-error";
    const failedSso = await checkSelected("confirmed", "fixture-course-002");
    assert.deepEqual(failedSso, {
      reason: "browser_navigation_failed",
      navigation: { stage: "webclass_sso", kind: "browser_error_page", cause: "connection_closed", request_method: "post" },
      diagnostic: { failure: { stage: "navigation", reason: "navigation_failed" }, shutdown: "clean" },
      lms_reached: false, courses_get: 0, contents_get: 0,
    });
    assert.equal(counters.contents, 7);
    assert.equal(counters.courses, 14);

    resultMode = "embedded";
    const embedded = await checkSelected("confirmed", "fixture-course-002");
    assert.equal(embedded.reason, "ok");
    assert.equal(embedded.courses_get, 1);
    assert.equal(embedded.contents_get, 1);
    assert.equal(counters.courses, 15);
    assert.equal(counters.contents, 8);
    assert.equal(counters.unknown, 0);

    resultMode = "csso-query";
    const throughProxy = [certificateFlag];
    const listed = await runBrowser(config("confirmed"), { flags: throughProxy });
    assert.equal(listed.reason, "ok");
    assert.equal(listed.courses_get, 1);
    assert.equal(listed.contents_get, 0);
    assert.equal(JSON.parse(listed.body).find(({ group_id }) => group_id === scienceGroup).group_name, "科学英語2（合成）");
    assert.equal(counters.queriedCsso, 1);

    const remembered = counters.remembered;
    const consent = counters.consent;
    const selected = await checkSelected("confirmed", scienceGroup, undefined, throughProxy);
    assert.equal(selected.reason, "ok", JSON.stringify(selected.navigation));
    assert.equal(selected.courses_get, 1);
    assert.equal(selected.contents_get, 1);
    assert.deepEqual(JSON.parse(selected.body), JSON.parse(scienceContents));
    assert.equal(contentsRequests.at(-1).url, selectPath(scienceGroup));
    assert.equal(counters.remembered, remembered + 1);
    assert.equal(counters.consent, consent);
    assert.equal(counters.queriedCsso, 2);

    const fresh = await checkSelected("csso-query-fresh", scienceGroup, clickFixtureConsent, throughProxy);
    assert.equal(fresh.reason, "ok");
    assert.equal(fresh.courses_get, 1);
    assert.equal(fresh.contents_get, 1);
    assert.deepEqual(JSON.parse(fresh.body), JSON.parse(scienceContents));
    assert.equal(counters.consent, consent + 1);
    assert.equal(counters.queriedCsso, 3);

    resultMode = "csso-unapproved-post";
    const blocked = await checkSelected("confirmed", scienceGroup, undefined, throughProxy);
    assert.equal(blocked.reason, "browser_navigation_failed");
    assert.equal(blocked.navigation.cause, "navigation_blocked");
    assert.equal(blocked.lms_reached, false);
    assert.equal(blocked.courses_get, 0);
    assert.equal(blocked.contents_get, 0);
    assert.equal(counters.unknown, 0);
    assert.equal(counters.mutation, 0);

    resultMode = "ok";
    const beforeBatch = contentsRequests.length;
    const selectedGroups = ["fixture-course-001", "fixture-course-002"];
    const batch = await checkSelected("confirmed", selectedGroups);
    assert.equal(batch.reason, "ok");
    assert.equal(batch.courses_get, 1);
    assert.equal(batch.contents_get, 2);
    assert.equal(batch.bodies.length, 2);
    assert.deepEqual(contentsRequests.slice(beforeBatch).map(({ url }) => url), selectedGroups.map(selectPath));
    const repeated = await checkSelected("confirmed", [selectedGroups[0], selectedGroups[0]]);
    assert.equal(repeated.reason, "browser_protocol");
    assert.equal(repeated.contents_get, 0);
    assert.equal(contentsRequests.length, beforeBatch + 2);

    async function failedHandoff(origin, cause, proxyCause, options = {}) {
      handoffOrigin = origin;
      const before = { courses: counters.courses, contents: counters.contents };
      const reply = await runBrowser(config("confirmed", { origins: [portal, idp, origin], lms: origin }),
        { flags: [certificateFlag], ...options }, () => assert.fail("SSO failure reached course selection"));
      assert.equal(reply.reason, "browser_navigation_failed");
      assert.equal(reply.lms_reached, false);
      assert.equal(reply.courses_get, 0);
      assert.equal(reply.contents_get, 0);
      assert.equal(reply.navigation.stage, "webclass_sso");
      assert.equal(reply.navigation.request_method, "post");
      assert.equal(reply.navigation.cause, cause);
      if (proxyCause === "timeout" && reply.navigation.proxy_connect_observed === undefined) {
        // Chrome may start CONNECT before the document event supplies an observation cursor.
        // The proxy's timeout recording is asserted directly in the stalled CONNECT test.
        assert.deepEqual(reply.diagnostic.failure, { stage: "navigation", reason: "navigation_failed" });
      } else {
        assert.equal(reply.navigation.proxy_connect_observed, proxyCause);
      }
      assert.ok(!JSON.stringify(reply).includes("127.0.0.1"));
      assert.equal(counters.courses, before.courses);
      assert.equal(counters.contents, before.contents);
    }

    resultMode = "ok";
    const badTls = await listen(createTcpServer((socket) => {
      socket.once("data", () => socket.end("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n"));
    }));
    await failedHandoff(badTls, "tls_failure", undefined);

    const reset = await listen(createTcpServer((socket) => {
      socket.once("data", () => socket.resetAndDestroy());
    }));
    const bypassReset = "--proxy-bypass-list=<-loopback>;" + new URL(reset).host;
    await failedHandoff(reset, "connection_reset", undefined, { flags: [certificateFlag, bypassReset] });

    const unavailable = createTcpServer();
    const refused = await listen(unavailable);
    await new Promise((resolve) => unavailable.close(resolve));
    await failedHandoff(refused, "proxy_failure", "connection_refused");

    // Keep local HTTP/TLS servers real. Inject only the unresolvable/stalled dial in the proxy.
    const unresolvedPort = Number(new URL(lms).port);
    const dialWithLookup = (lookup) => (target) => target.port === unresolvedPort
      ? connect({ ...target, host: "fixture.invalid", lookup }) : connect(target);
    const rejectLookup = (_host, _options, callback) => {
      queueMicrotask(() => callback(Object.assign(new Error(), { code: "ENOTFOUND" })));
    };
    await failedHandoff(lms, "proxy_failure", "dns_failure", { dial: dialWithLookup(rejectLookup) });
    await failedHandoff(lms, "proxy_failure", "timeout", { dial: dialWithLookup(() => {}), connectMillis: 350 });

    if (isMac) {
      handoffOrigin = undefined;
      let interrupted;
      const unaffected = await testRun("other-running-chrome", async (pipe, session) => {
        interrupted = await testRun("interrupted-chrome", (_pipe, _session, terminate) => terminate("SIGKILL"));
        await clickFixtureConsent(pipe, session);
      });
      assert.equal(unaffected.reason, "ok");
      assert.equal(unaffected.courses_get, 1);
      assert.equal(interrupted?.reason, "browser_closed");
      assert.equal(interrupted.diagnostic.shutdown, "killed");
    }

    handoffOrigin = undefined;
    resultMode = "operation-authenticate";
    const base = { ...counters };
    const authenticated = await runBrowser(config("operation-retained", { operation: "authenticate" }),
      { flags, headed: isMac, interact: clickFixtureConsent });
    assert.equal(authenticated.reason, "authenticated");
    assert.equal(authenticated.courses_get, 0);
    assert.equal(authenticated.contents_get, 0);
    assert.equal(authenticated.body, undefined);
    assert.deepEqual(authenticated.diagnostic, { failure: undefined, authenticated_by: "logout_control", shutdown: "clean" });
    assert.equal(counters.consent, base.consent + 1);
    assert.equal(counters.acs, base.acs + 1);
    assert.equal(counters.courses, base.courses);

    const headless = (profile = "operation-retained") => config(profile, {
      operation: "headless", entry: lms + dashboardPath,
    });
    const acquired = await runBrowser(headless(), { flags: [certificateFlag] },
      () => selectPath("fixture-course-002"));
    assert.equal(acquired.reason, "ok");
    assert.equal(acquired.courses_get, 1);
    assert.equal(acquired.contents_get, 1);
    assert.equal(acquired.diagnostic.authenticated_by, "logout_control");

    for (const [mode, reason, requests, stage, detail, profile] of [
      ["operation-authenticate", "reauth_required", 0, "navigation", "authentication_redirect", "operation-empty"],
      ["operation-idp", "reauth_required", 0, "navigation", "authentication_redirect"],
      ["operation-document-forbidden", "access_denied", 0, "navigation", "access_denied"],
      ["operation-visible-login", "reauth_required", 0, "page_detection", "login_form_detected"],
      ["operation-login-redirect", "reauth_required", 1, "courses_api", "authentication_redirect"],
      ["operation-login-html", "reauth_required", 1, "courses_api", "login_html_response"],
      ["expired", "auth_expired", 1, "courses_api", "session_expired"],
      ["operation-server", "server_unavailable", 1, "courses_api", "server_unavailable"],
      ["operation-network", "network_failed", 1, "courses_api", "request_failed"],
    ]) {
      resultMode = mode;
      const failed = await runBrowser(headless(profile), { flags: [certificateFlag] },
        () => assert.fail("failed acquisition reached course selection"));
      assert.equal(failed.reason, reason, mode);
      assert.equal(failed.courses_get, requests, mode);
      assert.equal(failed.contents_get, 0, mode);
      assert.equal(failed.body, undefined, mode);
      assert.deepEqual(failed.diagnostic.failure, { stage, reason: detail }, mode);
      assert.doesNotMatch(JSON.stringify(failed.diagnostic), /fixture|https?:|cookie|password/i);
    }
    for (const [mode, reason, detail] of [
      ["operation-contents-auth-redirect", "reauth_required", "authentication_redirect"],
      ["operation-contents-login-html", "reauth_required", "login_html_response"],
      ["contents-expired", "access_denied", "access_denied"],
    ]) {
      resultMode = mode;
      const failed = await runBrowser(headless(), { flags: [certificateFlag] }, () => selectPath("fixture-course-002"));
      assert.equal(failed.reason, reason, mode);
      assert.equal(failed.courses_get, 1, mode);
      assert.equal(failed.contents_get, 1, mode);
      assert.deepEqual(failed.diagnostic.failure, { stage: "assignments_api", reason: detail }, mode);
    }
    assert.equal(counters.portal, base.portal + 1);
    assert.equal(counters.consent, base.consent + 1);
    assert.equal(counters.acs, base.acs + 1);
    assert.equal(counters.login, 0);

    resultMode = "operation-session-only";
    const session = await runBrowser(config("session-only", { operation: "authenticate" }),
      { flags, headed: isMac, interact: clickFixtureConsent });
    assert.equal(session.reason, "authenticated");
    const restarted = await runBrowser(headless("session-only"), { flags: [certificateFlag] });
    assert.equal(restarted.reason, "reauth_required");
    assert.equal(restarted.courses_get, 0);
    assert.deepEqual(restarted.diagnostic.failure, { stage: "navigation", reason: "authentication_redirect" });

    resultMode = "operation-unsigned-dashboard";
    for (const [operation, reason] of [["authenticate", "login_timeout"], ["headless", "webclass_ready_timeout"]]) {
      const unsigned = await runBrowser({ ...headless("unsigned-" + operation), operation, loginMillis: 2500 },
        { flags: [certificateFlag] });
      assert.equal(unsigned.reason, reason);
      assert.equal(unsigned.courses_get, 0);
      assert.equal(unsigned.contents_get, 0);
      assert.deepEqual(unsigned.diagnostic.failure, { stage: "page_detection", reason: "authenticated_marker_missing" });
    }
  } finally {
    await Promise.all(servers.map((server) => new Promise((resolve) => server.close(resolve))));
    await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
