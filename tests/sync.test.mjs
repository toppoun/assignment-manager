import assert from "node:assert/strict";
import { test } from "node:test";
import { createSyncModel, initializeSync, sourceResultPresentation, syncButtonState } from "../frontend/sync.js";
import { createListTaskRunner } from "../frontend/list_tasks.js";
import { createSettingsModel } from "../frontend/settings.js";

const counts = { created: 2, updated: 1, unchanged: 3 };
const success = { status: "success", counts };
const missing = { status: "not_configured" };
const failed = { status: "failed", error: "authentication_failed", message: "PRIVATE_PASSWORD" };
const flags = { moodleConfigured: true, webclassConfigured: true, syncOnStartup: false };
const report = (moodle, webclass) => ({ status: "completed", moodle, webclass });

test("Sync button requires configured service and disables while a command is pending", async () => {
  let finish;
  const calls = [];
  let reloads = 0;
  const model = createSyncModel(command => { calls.push(command); return new Promise(resolve => { finish = resolve; }); }, async () => { reloads += 1; });
  assert.equal(syncButtonState(model.snapshot()).disabled, true);
  model.configure(flags);
  assert.equal(syncButtonState(model.snapshot()).disabled, false);
  const running = model.run();
  assert.equal(model.snapshot().busy, true);
  assert.equal(syncButtonState(model.snapshot()).label, "同期中…");
  assert.equal(await model.run(), false);
  assert.deepEqual(calls, ["sync_now"]);
  finish(report(success, success));
  assert.equal(await running, true);
  assert.equal(reloads, 1);
  assert.equal(model.snapshot().busy, false);
  assert.equal(model.snapshot().results[0].outcome, "success");
});

test("Every configured source result reloads the list, including partial and all failures", async () => {
  for (const response of [report(success, success), report(success, failed), report(failed, success), report(failed, failed), report(success, missing)]) {
    let reloads = 0;
    const model = createSyncModel(async () => response, async () => { reloads += 1; });
    model.configure(flags);
    await model.run();
    assert.equal(reloads, 1);
    assert.equal(model.snapshot().results.length, 2);
    assert.ok(!JSON.stringify(model.snapshot()).includes("PRIVATE_PASSWORD"));
  }
});

test("Unconfigured sources do not call sync; removal raced with command gives normal missing result", async () => {
  let calls = 0;
  let reloads = 0;
  const model = createSyncModel(async command => { calls += 1; return command === "run_startup_sync" ? null : report(missing, missing); }, async () => { reloads += 1; });
  model.configure({ moodleConfigured: false, webclassConfigured: false, syncOnStartup: true });
  await model.startup();
  await model.run();
  assert.equal(calls, 1); // Rust owns startup eligibility, even with no configured source
  model.configure(flags);
  await model.run();
  assert.equal(calls, 2);
  assert.equal(reloads, 0);
  assert.match(model.snapshot().message, /設定されていません/);
});

test("Already running is ignored safely without queuing a new run or reloading unfinished data", async () => {
  let reloads = 0;
  const model = createSyncModel(async () => ({ status: "already_running" }), async () => { reloads += 1; });
  model.configure(flags);
  assert.equal(await model.run(), false);
  assert.equal(reloads, 0);
  assert.match(model.snapshot().message, /すでに実行中/);
});

test("Failure messages are fixed and never retain core/backend payloads or raw DTO fields", async () => {
  const secret = "PRIVATE_URL_PASSWORD_COOKIE_SCOPE";
  for (const error of ["network_failed", "authentication_failed", "invalid_configuration", "source_data_invalid", "persistence_failed", "credential_unavailable", "unexpected", secret]) {
    const result = sourceResultPresentation("WebClass", { status: "failed", error, message: secret, body: secret });
    assert.ok(!JSON.stringify(result).includes(secret));
  }
  let reloads = 0;
  const model = createSyncModel(async () => { throw { message: secret }; }, async () => { reloads += 1; });
  model.configure(flags);
  assert.equal(await model.run(), false);
  assert.equal(model.snapshot().busy, false);
  assert.equal(reloads, 1);
  assert.ok(!JSON.stringify(model.snapshot()).includes(secret));
  assert.match(model.snapshot().message, /引き続き利用/);
});

test("Startup delegates policy to Rust after local list/settings, including off and recreated models", async () => {
  for (const enabled of [false, true]) {
    const events = [];
    let checked = false;
    const invoke = async command => {
      assert.equal(command, "run_startup_sync");
      events.push("startup-policy");
      if (checked) return null;
      checked = true;
      return enabled ? report(success, missing) : null;
    };
    const model = createSyncModel(invoke, async () => { events.push("reload"); });
    await initializeSync(async () => { events.push("local"); }, async () => { events.push("settings"); return { ...flags, syncOnStartup: enabled }; }, model);
    assert.deepEqual(events, enabled ? ["local", "settings", "startup-policy", "reload"] : ["local", "settings", "startup-policy"]);
    assert.equal(await model.startup(), false);
    model.configure({ ...flags, syncOnStartup: true });
    assert.equal(await model.startup(), false); // settings save never triggers a second startup sync
    const recreated = createSyncModel(invoke, async () => { events.push("reload"); });
    recreated.configure({ ...flags, syncOnStartup: true });
    assert.equal(await recreated.startup(), false);
    assert.equal(events.filter(event => event === "reload").length, enabled ? 1 : 0);
  }
});

test("Startup authentication failure keeps initial local list and still refreshes committed data", async () => {
  const events = [];
  const model = createSyncModel(async () => { events.push("sync"); return report(missing, failed); }, async () => { events.push("reload"); });
  await initializeSync(async () => { events.push("local"); }, async () => ({ ...flags, syncOnStartup: true }), model);
  assert.deepEqual(events, ["local", "sync", "reload"]);
  assert.equal(model.snapshot().results[1].outcome, "failed");
  assert.equal(model.snapshot().busy, false);
});

test("Sync refresh waits for an active toggle and queries current filter/sort after it", async () => {
  const runListTask = createListTaskRunner();
  let finishToggle;
  const events = [];
  let selected = "title";
  const toggle = runListTask(async () => { events.push("toggle"); await new Promise(resolve => { finishToggle = resolve; }); events.push("committed"); });
  const model = createSyncModel(async () => report(success, failed), () => runListTask(async () => { events.push(`query:${selected}`); }));
  model.configure(flags);
  const pending = model.run();
  await Promise.resolve();
  assert.deepEqual(events, ["toggle"]);
  assert.equal(model.snapshot().busy, true);
  selected = "course";
  finishToggle();
  await toggle;
  await pending;
  assert.deepEqual(events, ["toggle", "committed", "query:course"]);
});

test("Startup toggle sends a non-secret request, updates only after success and retains value on failure", async () => {
  const calls = [];
  let fail = false;
  const model = createSettingsModel(async (command, args) => {
    calls.push({ command, args });
    if (command === "get_settings_status") return flags;
    if (fail) throw { code: "settings_write_failed", message: "PRIVATE_PASSWORD" };
  });
  await model.load();
  assert.equal(model.snapshot().syncOnStartup, false);
  await model.setStartupSync(true);
  assert.deepEqual(calls[1], { command: "set_startup_sync", args: { enabled: true } });
  assert.equal(model.snapshot().syncOnStartup, true);
  fail = true;
  assert.equal(await model.setStartupSync(false), false);
  assert.equal(model.snapshot().syncOnStartup, true);
  assert.ok(!model.snapshot().error.includes("PRIVATE_PASSWORD"));
});

test("List refresh failure keeps source results, releases busy state and uses a fixed message", async () => {
  const model = createSyncModel(async () => report(success, failed), async () => { throw { message: "PRIVATE_DB_PATH" }; });
  model.configure(flags);
  assert.equal(await model.run(), true);
  assert.equal(model.snapshot().busy, false);
  assert.equal(model.snapshot().results[0].outcome, "success");
  assert.match(model.snapshot().error, /一覧を更新できません/);
  assert.ok(!model.snapshot().error.includes("PRIVATE_DB_PATH"));
});
