import assert from "node:assert/strict";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { test } from "node:test";
import { runBrowser } from "../src/webclass_browser.mjs";

const run = process.platform === "win32" && process.env.ASSIGNMENT_MANAGER_TEST_EDGE === "1";

test("isolated Edge SSO reuses an opted-in profile and caps courses to one GET per run", { skip: !run, timeout: 60000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), "webclass-edge-fixture-"));
  const courses = await readFile(new URL("../fixtures/webclass/courses_basic.json", import.meta.url), "utf8");
  const coursesPath = "/webclass/ip_mods.php/plugin/score_summary_table/courses";
  const dashboardPath = "/webclass/ip_mods.php/plugin/score_summary_table/dashboard";
  const contentsPath = coursesPath.replace(/courses$/, "contents");
  const servers = [];
  const counters = { portal: 0, consent: 0, remembered: 0, acs: 0, courses: 0, contents: 0, unknown: 0 };
  const secret = "synthetic-unexported-session";
  let resultMode = "ok";
  let initialCookieSeen = false;

  async function serve(handler) {
    const server = createServer(handler);
    servers.push(server);
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    return "http://127.0.0.1:" + server.address().port;
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
      '<form id="assertion" action="' + lms + '/acs" method="post">' +
      '<input type="hidden" name="SAMLResponse" value="fixture-assertion"></form>' +
      '<script>localStorage.setItem("synthetic-consent", "fixture");document.getElementById("assertion").submit();</script>');
    const idp = await serve((request, response) => {
      if (request.url === "/consent" && request.method === "GET") {
        const target = resultMode === "popup" ? ' target="_blank"' : "";
        const checkRemembered = resultMode === "reauth" ? "" :
          '<script>if (localStorage.getItem("synthetic-consent") === "fixture") location.replace("/remembered");</script>';
        html(response, '<form action="/continue" method="post"' + target + '><button id="accept">Accept</button></form>' + checkRemembered);
      } else if (request.url === "/remembered" && request.method === "GET") {
        counters.remembered++;
        forwardAssertion(response);
      } else if (request.url === "/continue" && request.method === "POST") {
        counters.consent++;
        if (resultMode === "unknown") {
          response.writeHead(302, { Location: unknown + "/blocked" }).end();
        } else {
          forwardAssertion(response);
        }
      } else {
        response.writeHead(404).end();
      }
    });
    const lms = await serve((request, response) => {
      if (request.url === "/acs" && request.method === "POST") {
        counters.acs++;
        request.resume().on("end", () => response.writeHead(303, {
          Location: "/webclass/",
          "Set-Cookie": "fixture=" + secret + "; Path=/webclass; HttpOnly; SameSite=Lax",
        }).end());
      } else if (request.url === "/webclass/") {
        if (request.headers.cookie !== "fixture=" + secret) {
          response.writeHead(401).end();
          return;
        }
        html(response, '<a href="/webclass/logout.php">ログアウト</a>' +
          '<script>for (const path of ' + JSON.stringify([contentsPath, coursesPath, coursesPath]) +
          ') fetch(path).catch(()=>{});</script>');
      } else if (request.url === coursesPath && request.method === "GET") {
        counters.courses++;
        if (request.headers.cookie !== "fixture=" + secret || resultMode === "expired") {
          response.writeHead(401).end();
        } else if (resultMode === "redirect") {
          response.writeHead(302, { Location: "/webclass/blocked" }).end();
        } else {
          response.writeHead(200, { "Content-Type": "application/json" }).end(courses);
        }
      } else {
        if (request.url === contentsPath) counters.contents++;
        if (request.url === "/webclass/blocked") counters.unknown++;
        response.writeHead(404).end();
      }
    });
    const portal = await serve((request, response) => {
      counters.portal++;
      if (request.headers.cookie) initialCookieSeen = true;
      response.writeHead(302, { Location: idp + "/consent" }).end();
    });
    const flags = ["--proxy-bypass-list=" + [portal, idp, lms, unknown].map((url) => new URL(url).host).join(";")];
    const edge = join(process.env["ProgramFiles(x86)"], "Microsoft", "Edge", "Application", "msedge.exe");

    async function testRun(profile, interact, options = {}) {
      return runBrowser({
        edge, profile: join(root, profile), entry: portal + "/portal",
        origins: [portal, idp, lms], lms, coursesPath, dashboardPath,
        maxBytes: 1024 * 1024, loginMillis: 6000, ...options,
      }, { flags, interact });
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
    const interrupted = await testRun("confirmed", (_pipe, _session, terminate) => terminate());
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
    assert.equal(redirected.reason, "course_request_failed");
    assert.equal(redirected.courses_get, 1);
    assert.equal(counters.acs, 6);
    assert.equal(counters.courses, 6);
    assert.equal(counters.contents, 0);
    assert.equal(counters.unknown, 0);
    assert.equal(initialCookieSeen, false);
  } finally {
    await Promise.all(servers.map((server) => new Promise((resolve) => server.close(resolve))));
    await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
