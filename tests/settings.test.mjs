import assert from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";
import { configuredPresentation, createSettingsModel, settingsErrorMessage } from "../frontend/settings.js";

const URL = "https://example.invalid/?token=PRIVATE_MOODLE_8D1";
const ID = "PRIVATE_UNIVERSITY_ID_8D1";
const PASSWORD = "PRIVATE_PASSWORD_8D1";
const markers = [URL, ID, PASSWORD];
const inputs = () => ({ url: { value: URL }, universityId: { value: ID }, password: { value: PASSWORD } });

test("Configured presentation distinguishes saved, missing and unavailable", () => {
  assert.equal(configuredPresentation(true).label, "設定済み");
  assert.equal(configuredPresentation(false).label, "未設定");
  assert.equal(configuredPresentation(null).label, "未確認");
});

test("Status model whitelists flags and never retains extra secret response fields", async () => {
  const model = createSettingsModel(async () => ({ moodleConfigured: true, webclassConfigured: false, url: URL, universityId: ID, password: PASSWORD }));
  await model.load();
  assert.equal(model.snapshot().moodleConfigured, true);
  const state = JSON.stringify(model.snapshot());
  for (const marker of markers) assert.ok(!state.includes(marker));
});

test("Save requests stay transient and clear all input values without repopulation", async () => {
  const calls = [];
  const states = [];
  const model = createSettingsModel(async (command, args) => {
    calls.push({ command, args: structuredClone(args) });
    return { moodleConfigured: false, webclassConfigured: false };
  }, state => states.push(state));
  await model.load();
  const moodle = { url: { value: URL } };
  assert.equal(await model.mutate("save_moodle", moodle), true);
  assert.equal(moodle.url.value, "");
  const webclass = { universityId: { value: ID }, password: { value: PASSWORD } };
  assert.equal(await model.mutate("save_webclass", webclass), true);
  assert.deepEqual(webclass, { universityId: { value: "" }, password: { value: "" } });
  assert.deepEqual(calls[1], { command: "save_moodle_feed", args: { secretUrl: URL } });
  assert.deepEqual(calls[2], { command: "save_webclass_credentials", args: { credentials: { universityId: ID, password: PASSWORD } } });
  assert.equal(model.snapshot().moodleConfigured, true);
  assert.equal(model.snapshot().webclassConfigured, true);
  for (const marker of markers) assert.ok(!JSON.stringify(states).includes(marker));
});

test("Inputs are cleared while IPC is pending and transient arguments released afterwards", async () => {
  let finish;
  let argumentReference;
  const model = createSettingsModel((_command, args) => {
    argumentReference = args;
    return new Promise(resolve => { finish = resolve; });
  });
  const fields = { universityId: { value: ID }, password: { value: PASSWORD } };
  const pending = model.mutate("save_webclass", fields);
  assert.equal(fields.password.value, "");
  assert.equal(fields.universityId.value, "");
  assert.equal(model.snapshot().busy, true);
  for (const marker of markers) assert.ok(!JSON.stringify(model.snapshot()).includes(marker));
  finish();
  await pending;
  assert.equal(argumentReference.credentials.password, "");
  assert.equal(argumentReference.credentials.universityId, "");
});

test("Remove requests contain no secret or arbitrary credential identity", async () => {
  const calls = [];
  const model = createSettingsModel(async (command, args) => {
    calls.push({ command, args });
    return { moodleConfigured: true, webclassConfigured: true };
  });
  await model.load();
  await model.mutate("remove_moodle");
  await model.mutate("remove_webclass");
  assert.deepEqual(calls.slice(1), [{ command: "remove_moodle_feed", args: undefined }, { command: "remove_webclass_credentials", args: undefined }]);
  assert.equal(model.snapshot().moodleConfigured, false);
  assert.equal(model.snapshot().webclassConfigured, false);
});

test("Validation/backend failures are fixed messages and preserve known configured state", async () => {
  const model = createSettingsModel(async command => {
    if (command === "get_settings_status") return { moodleConfigured: true, webclassConfigured: true };
    throw { code: "invalid_credentials", message: markers.join(" ") };
  });
  await model.load();
  const fields = inputs();
  assert.equal(await model.mutate("save_webclass", fields), false);
  assert.equal(model.snapshot().webclassConfigured, true);
  assert.match(model.snapshot().error, /両方を入力/);
  assert.equal(fields.password.value, "");
  assert.match(settingsErrorMessage("save_moodle", { code: "invalid_moodle_feed", message: URL }), /HTTPS/);
  for (const operation of ["status", "save_moodle", "save_webclass", "remove_moodle", "remove_webclass"]) {
    for (const marker of markers) assert.ok(!settingsErrorMessage(operation, { message: markers.join(" ") }).includes(marker));
  }
});

test("Read failure reports unknown status without silently claiming secrets are missing", async () => {
  const model = createSettingsModel(async () => { throw { code: "credential_read_failed", message: PASSWORD }; });
  assert.equal(await model.load(), false);
  assert.equal(model.snapshot().moodleConfigured, null);
  assert.equal(model.snapshot().webclassConfigured, null);
  assert.match(model.snapshot().error, /確認できません/);
  assert.ok(!model.snapshot().error.includes(PASSWORD));
});

test("Settings UI uses password fields and contains no browser persistence or logging", () => {
  const html = readFileSync(new globalThis.URL("../frontend/index.html", import.meta.url), "utf8");
  assert.match(html, /id="webclass-password"[^>]*type="password"/);
  assert.match(html, /id="moodle-url"[^>]*type="password"/);
  const config = JSON.parse(readFileSync(new globalThis.URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  assert.equal(config.app.windows[0].incognito, true);
  assert.equal(config.app.windows[0].generalAutofillEnabled, false);
  for (const path of ["settings.js", "settings_view.js"]) {
    const source = readFileSync(new globalThis.URL(`../frontend/${path}`, import.meta.url), "utf8");
    assert.doesNotMatch(source, /localStorage|sessionStorage|indexedDB|console\./);
  }
});
