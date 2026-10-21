import { configuredPresentation, clearSecretInputs } from "./settings.js";

const errorMessage = error => ({
  reauth_required: "変更前にWeb専用passwordで再認証してください。",
  reauth_failed: "Web専用passwordを確認してください。",
  rate_limited: "認証試行が多すぎます。60秒待ってください。",
  invalid_request: "入力内容と長さを確認してください。MoodleはHTTPS URLが必要です。",
  vault_unavailable: "認証情報ストアを利用できません。管理者に確認してください。",
}[error?.code] ?? "処理を確認できません。通信を確認して状態を再取得してください。");

// Web mutations have ambiguous network outcomes and session-bound reauth.
// Desktop keeps its existing invoke model. Both reuse safe presentation/clearing.
export function createWebSettingsView(api, document_ = document) {
  const get = id => document_.querySelector(`#${id}`);
  const inputs = Object.fromEntries(["moodle-url", "webclass-id", "webclass-password", "reauth-password"].map(id => [id, get(id)]));
  const listeners = new AbortController();
  let active = true;
  let generation = 0;
  const state = { moodleConfigured: null, webclassConfigured: null, busy: false, reauthenticated: false, message: "", error: "", outcome: null };
  const snapshot = () => ({ ...state });
  const clear = () => clearSecretInputs(inputs);
  function render() {
    if (!active) return;
    get("settings-view").setAttribute("aria-busy", String(state.busy));
    get("settings-message").textContent = state.message;
    get("settings-error").textContent = state.error;
    get("settings-error").hidden = !state.error;
    get("reauth-status").textContent = state.reauthenticated ? "本人確認済みです。有効期間は5分です。" : "設定変更には本人確認が必要です。";
    get("reauth-submit").disabled = state.busy;
    get("settings-reload").disabled = state.busy;
    inputs["reauth-password"].disabled = state.busy;
    for (const name of ["moodle", "webclass"]) {
      const configured = state[`${name}Configured`];
      const presentation = configuredPresentation(configured);
      get(`${name}-status`).textContent = presentation.label;
      get(`${name}-status`).dataset.state = presentation.state;
      get(`${name}-save`).textContent = configured ? "置き換えて保存" : "保存";
      get(`${name}-save`).disabled = state.busy || !state.reauthenticated || configured === null;
      get(`${name}-remove`).disabled = state.busy || !state.reauthenticated || configured !== true;
    }
    for (const id of ["moodle-url", "webclass-id", "webclass-password"]) inputs[id].disabled = state.busy || !state.reauthenticated;
    // Rendering never writes a secret input value or restores submitted text.
  }
  const current = ticket => active && ticket === generation;
  async function status(ticket) {
    try {
      const flags = await api.settingsStatus();
      if (!current(ticket)) return false;
      state.moodleConfigured = flags.moodleConfigured;
      state.webclassConfigured = flags.webclassConfigured;
      return true;
    } catch {
      if (current(ticket)) { state.moodleConfigured = null; state.webclassConfigured = null; }
      return false;
    }
  }
  async function load() {
    if (!active || state.busy) return false;
    clear(); state.busy = true; state.error = ""; render();
    const ticket = ++generation;
    try {
      const confirmed = await status(ticket);
      if (!current(ticket)) return false;
      if (!confirmed) state.error = "設定状態を確認できません。";
      return confirmed;
    } finally { if (current(ticket)) { state.busy = false; render(); } }
  }
  async function reauthenticate() {
    if (!active || state.busy) { clear(); return false; }
    state.busy = true; state.error = ""; state.reauthenticated = false;
    const ticket = ++generation;
    const operation = api.reauthenticate(inputs["reauth-password"].value);
    clear(); render();
    try {
      await operation;
      if (!current(ticket)) return false;
      state.reauthenticated = true; state.message = "本人確認が完了しました。合成の認証情報だけを入力してください。";
      return true;
    } catch (error) {
      if (current(ticket)) { state.error = errorMessage(error); state.message = ""; }
      return false;
    } finally { if (current(ticket)) { clear(); state.busy = false; render(); } }
  }
  async function mutate(operation) {
    if (!active || state.busy || !state.reauthenticated) { clear(); return false; }
    const ticket = ++generation;
    state.busy = true; state.error = ""; state.message = "処理しています…"; state.outcome = null;
    // Adapter serializes immediately. Only the transient request owns input text.
    const request = {
      save_moodle: () => api.saveMoodle(inputs["moodle-url"].value),
      save_webclass: () => api.saveWebclass(inputs["webclass-id"].value, inputs["webclass-password"].value),
      remove_moodle: () => api.removeMoodle(), remove_webclass: () => api.removeWebclass(),
    }[operation]();
    clear(); render();
    try {
      await request;
      if (!current(ticket)) return false;
      state.outcome = "saved";
      state.message = operation.startsWith("remove") ? "削除しました。" : "保存しました。";
      if (!await status(ticket) && current(ticket)) state.error = "操作は成功しました。設定状態を確認できません。";
      return true;
    } catch (error) {
      if (!current(ticket)) return false;
      if (error?.code === "reauth_required") state.reauthenticated = false;
      state.outcome = error?.outcome === "rejected" ? "rejected" : "unknown";
      state.message = "";
      if (state.outcome === "unknown") {
        state.error = "操作結果が不明です。状態を再取得します。設定済み表示だけでは置換の成否は確認できません。自動再送は行いません。";
        await status(ticket);
      } else state.error = errorMessage(error);
      return false;
    } finally { if (current(ticket)) { clear(); state.busy = false; render(); } }
  }
  const on = (id, event, fn) => get(id).addEventListener(event, fn, { signal: listeners.signal });
  on("reauth-form", "submit", event => { event.preventDefault(); void reauthenticate(); });
  on("settings-reload", "click", () => { void load(); });
  for (const name of ["moodle", "webclass"]) {
    on(`${name}-form`, "submit", event => { event.preventDefault(); void mutate(`save_${name}`); });
    on(`${name}-remove`, "click", () => { void mutate(`remove_${name}`); });
  }
  function dispose() {
    active = false; ++generation; listeners.abort(); clear();
    Object.assign(state, { moodleConfigured: null, webclassConfigured: null, reauthenticated: false, message: "", error: "", busy: false, outcome: null });
    for (const id of ["settings-message", "settings-error", "reauth-status", "moodle-status", "webclass-status"]) get(id).textContent = "";
  }
  clear(); render();
  return { load, mutate, reauthenticate, clear, dispose, snapshot };
}
