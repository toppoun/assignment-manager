import assert from "node:assert/strict";
import { test } from "node:test";
import { createHttpApi } from "../frontend/http_api.js";
import { createWebSyncModel, createWebSyncView } from "../frontend/web_sync.js";
import { createAssignmentListModel } from "../frontend/list_tasks.js";
import { DEFAULT_CONTROLS, buildQueryRequest } from "../frontend/presentation.js";
import { createWebApp } from "../frontend/web_auth.js";
import { documentFixture } from "./dom_fixture.mjs";
const id = "a".repeat(64), other = "b".repeat(64);
const success = { status: "success", counts: { created: 1, updated: 2, unchanged: 3 } };
const failed = { status: "failed", error: "network_failed" }, missing = { status: "not_configured" };
const finished = (m = success, w = success, shouldReload = true) => ({ runId: id, state: "finished", report: { moodle: m, webclass: w }, shouldReload });
const flags = { moodleConfigured: true, webclassConfigured: true };
const idle = { running: false, activeRunId: null, lastRun: null };
const running = { running: true, activeRunId: id, lastRun: null };
const response = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
const options = { newKey: () => "synthetic-key", wait: async () => {}, maxPolls: 3 };
const base = extra => ({ settingsStatus: async () => flags, syncStatus: async () => idle, startSync: async () => ({ runId: id, state: "running" }), syncRun: async () => finished(), ...extra });

test("HTTP sync uses POST JSON, session CSRF, distinct Idempotency-Key and 202 then fixed run URL", async () => {
  const calls = [];
  const api = createHttpApi(async (url, request) => {
    calls.push({ url, request });
    if (url === "/api/v1/session") return response({ authenticated: true, userId: "owner", csrfToken: "c".repeat(64), expiresAt: 2, absoluteExpiresAt: 3 });
    if (request.method === "POST") return response({ runId: id, state: "running", statusUrl: `/api/v1/sync/runs/${id}` }, 202);
    if (url === "/api/v1/sync/status") return response(running);
    return response(finished());
  });
  await api.session(); await api.startSync("unique-intent");
  const { url, request } = calls.at(-1);
  assert.equal(url, "/api/v1/sync"); assert.equal(request.method, "POST"); assert.equal(request.body, "{}");
  assert.equal(request.headers["Idempotency-Key"], "unique-intent"); assert.equal(request.headers["X-CSRF-Token"], "c".repeat(64));
  assert.equal(request.headers["X-Assignment-Manager"], "web-3a"); assert.equal(request.credentials, "same-origin");
  assert.deepEqual(await api.syncStatus(), running); assert.deepEqual(await api.syncRun(id), finished());
  await assert.rejects(api.startSync("bad key"), { code: "invalid_request" });
  for (const value of [null, { ...finished(), runId: other }, { ...finished(), report: { moodle: { status: "failed", error: "PRIVATE HTML" }, webclass: success } }, { ...finished(), shouldReload: "yes" }]) {
    await assert.rejects(createHttpApi(async () => response(value)).syncRun(id), { code: "response_invalid" });
  }
  await assert.rejects(createHttpApi(async () => response({ runId: id, state: "running", statusUrl: `/api/v1/sync/runs/${id}` })).startSync("key"), { code: "response_invalid" });
});

test("sync button and bounded polling: one accepted job, duplicate clicks blocked and source counts shown", async () => {
  const gate = deferred(), entered = deferred(); let posts = 0, polls = 0, reloads = 0;
  const model = createWebSyncModel(base({ startSync: async key => { posts++; assert.equal(key, "synthetic-key"); return { runId: id }; }, syncRun: async () => {
    if (++polls === 1) { entered.resolve(); await gate.promise; return { runId: id, state: "running" }; } return finished();
  } }), async () => { reloads++; }, () => {}, options);
  await model.refresh(); const execution = model.run(); await entered.promise;
  assert.equal(model.snapshot().busy, true); assert.equal(await model.run(), false);
  gate.resolve(); assert.equal(await execution, true);
  assert.equal(posts, 1); assert.equal(polls, 2); assert.equal(reloads, 1); assert.equal(model.snapshot().busy, false);
  assert.match(model.snapshot().results[0].message, /新規 1・更新 2・変更なし 3/);
});

for (const [name, m, w, reload] of [["partial failure", failed, success, true], ["all failure", failed, failed, true], ["not configured", missing, missing, false]]) {
  test(`${name} preserves source semantics and shouldReload`, async () => {
    let reloads = 0;
    const model = createWebSyncModel(base({ syncRun: async () => finished(m, w, reload) }), async () => { reloads++; }, () => {}, options);
    await model.refresh(); await model.run();
    assert.equal(reloads, Number(reload)); assert.equal(model.snapshot().results.length, 2);
    assert.equal(model.snapshot().results[0].outcome, m.status === "failed" ? "failed" : "missing");
  });
}

test("another tab's active run is observed without starting another sync", async () => {
  let posts = 0, polls = 0;
  const model = createWebSyncModel(base({ syncStatus: async () => running, startSync: async () => { posts++; }, syncRun: async () => { polls++; return finished(); } }), async () => {}, () => {}, options);
  await model.refresh(); assert.equal(posts, 0); assert.equal(polls, 1); assert.equal(model.snapshot().results.length, 2);
});

test("polling errors and timeout do not assert source failure; resume existing run", async () => {
  for (const mode of ["error", "timeout"]) {
    let restored = false, polls = 0, posts = 0;
    const model = createWebSyncModel(base({ startSync: async () => { posts++; return { runId: id }; }, syncStatus: async () => restored ? running : idle,
      syncRun: async () => { polls++; if (restored) return finished(); if (mode === "error") throw { code: "network_failed", message: "PRIVATE" }; return { runId: id, state: "running" }; },
    }), async () => {}, () => {}, options);
    await model.refresh(); assert.equal(await model.run(), false);
    assert.equal(polls, mode === "error" ? 1 : 3); assert.equal(model.snapshot().uncertain, true);
    assert.equal(model.snapshot().results.length, 0); assert.doesNotMatch(JSON.stringify(model.snapshot()), /PRIVATE/);
    assert.match(model.snapshot().message, /可能性/);
    restored = true; await model.run(); assert.equal(posts, 1); assert.equal(model.snapshot().uncertain, false);
  }
});

test("lost POST reply retries original idempotency key even when global status is idle", async () => {
  const keys = []; let posts = 0;
  const model = createWebSyncModel(base({ startSync: async key => { keys.push(key); if (++posts === 1) throw { code: "network_failed" }; return { runId: id }; } }), async () => {}, () => {}, options);
  await model.refresh(); await model.run(); await model.refresh();
  assert.equal(keys.length, 1); // navigation/status refresh cannot resend a write
  await model.run();
  assert.deepEqual(keys, ["synthetic-key", "synthetic-key"]); assert.equal(model.snapshot().results.length, 2);
});

test("dispose stops timers and suppresses stale GET, POST and reload after logout/user replacement", async () => {
  for (const stage of ["post", "poll", "timer"]) {
    const gate = deferred(), entered = deferred(); let reloads = 0, polls = 0, renders = 0;
    const model = createWebSyncModel(base({ startSync: async () => { if (stage === "post") { entered.resolve(); await gate.promise; } return { runId: id }; },
      syncRun: async () => { polls++; if (stage === "poll") { entered.resolve(); await gate.promise; } return stage === "timer" ? { runId: id, state: "running" } : finished(); },
    }), async () => { reloads++; }, () => { renders++; }, { ...options, wait: async (_, signal) => { entered.resolve(); await new Promise(resolve => signal.addEventListener("abort", resolve, { once: true })); } });
    await model.refresh(); const execution = model.run(); await entered.promise;
    model.dispose(); const prior = renders; gate.resolve(); await execution;
    assert.equal(renders, prior); assert.equal(reloads, 0); assert.equal(polls, stage === "post" ? 0 : 1);
    assert.equal(model.snapshot().runId, null);
  }
});

test("completion pending plus sync reload serializes writes and retains latest filter/sort", async () => {
  const saving = deferred(), imported = deferred(), syncing = deferred(); const queries = []; let completion = "incomplete";
  const list = createAssignmentListModel({ listAssignments: async query => { queries.push(query); return { items: [{ id: "assignment", completion }], courses: [], totalCount: 1 }; },
    setCompletion: async () => { await saving.promise; completion = "completed"; return completion; } });
  await list.reload();
  const sync = createWebSyncModel(base({ syncRun: async () => { syncing.resolve(); await imported.promise; return finished(failed, success); } }), () => list.reload(), () => {}, options);
  await sync.refresh(); const completing = list.setCompletion("assignment", "completed"); const execution = sync.run(); await syncing.promise;
  const controls = { ...DEFAULT_CONTROLS, completion: "completed", sort: "title", deadline: "today", course: "unassigned" };
  const filtering = list.reload(controls); imported.resolve(); saving.resolve();
  await Promise.all([completing, filtering, execution]);
  assert.deepEqual(queries.at(-1), buildQueryRequest(controls)); assert.equal(list.snapshot().response.items[0].completion, "completed"); assert.equal(list.snapshot().pendingId, null);
});

test("job error, AlreadyRunning, restart 404, status fetch error, reload failure are safe", async () => {
  for (const error of ["already_running", "job_failed"]) {
    let reloads = 0; const model = createWebSyncModel(base({ syncRun: async () => ({ runId: id, state: "failed", error, shouldReload: error !== "already_running" }) }), async () => { reloads++; }, () => {}, options);
    await model.refresh(); await model.run(); assert.equal(reloads, error === "already_running" ? 0 : 1); assert.equal(model.snapshot().results.length, 0); assert.ok(model.snapshot().error);
  }
  const model = createWebSyncModel(base({ syncStatus: async () => { throw { code: "network_failed" }; } }), async () => {}, () => {}, options);
  await model.refresh(); assert.equal(model.snapshot().uncertain, true);
  const lost = createWebSyncModel(base({ syncRun: async () => { throw { code: "not_found" }; } }), async () => {}, () => {}, options);
  await lost.refresh(); await lost.run(); assert.match(lost.snapshot().error, /再起動/); assert.equal(lost.snapshot().runId, null);
  const reload = createWebSyncModel(base(), async () => false, () => {}, options);
  await reload.refresh(); await reload.run(); assert.equal(reload.snapshot().results.length, 2); assert.match(reload.snapshot().error, /一覧を更新できません/);
});

test("Web sync view renders safe shared source messages and clears on disposal", async () => {
  const { doc, ids } = documentFixture();
  const view = createWebSyncView(base({ syncStatus: async () => ({ ...idle, lastRun: finished() }) }), async () => {}, doc);
  await view.load(); assert.equal(ids["web-sync-now"].textContent, "同期"); assert.equal(ids["web-sync-now"].disabled, false);
  assert.equal(ids["web-sync-results"].children.length, 2); view.dispose(); assert.equal(ids["web-sync-results"].children.length, 0);
});

test("auth lifecycle clears sync panel and late run results after logout", async () => {
  const { doc, ids } = documentFixture(); const pending = deferred(), entered = deferred(); let polls = 0;
  const apiSession = { authenticated: true, userId: "owner", csrfToken: "c".repeat(64), expiresAt: 2, absoluteExpiresAt: 3 };
  const app = createWebApp(doc, async url => {
    if (url === "/api/v1/session") return response(apiSession);
    if (url === "/api/v1/settings/status") return response(flags);
    if (url === "/api/v1/sync/status") return response(running);
    if (url.startsWith("/api/v1/sync/runs/")) { polls++; entered.resolve(); return pending.promise; }
    if (url === "/api/v1/auth/logout") return response({ authenticated: false });
    return response({ items: [], courses: [], totalCount: 0, evaluatedAt: "2026-10-22T00:00:00Z", timeZone: "Asia/Tokyo" });
  });
  await app.start(); await entered.promise; await ids["show-settings"].click(); assert.equal(ids["web-sync-panel"].hidden, false);
  await ids.logout.click(); pending.resolve(response(finished())); await new Promise(resolve => setImmediate(resolve));
  assert.equal(ids["web-sync-panel"].hidden, true); assert.equal(ids["web-sync-results"].children.length, 0); assert.equal(polls, 1);
});
