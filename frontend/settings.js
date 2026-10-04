import { safeErrorMessage } from "./presentation.js";

export function configuredPresentation(configured) {
  if (configured === true) return { label: "設定済み", state: "configured" };
  if (configured === false) return { label: "未設定", state: "missing" };
  return { label: "未確認", state: "unknown" };
}

export function clearSecretInputs(inputs) {
  for (const input of Object.values(inputs)) input.value = "";
}

export function settingsErrorMessage(operation, error) {
  const prefix = {
    status: "設定状態を確認できませんでした。",
    save_moodle: "Moodle設定を保存できませんでした。",
    remove_moodle: "Moodle設定を削除できませんでした。",
    save_webclass: "WebClass認証情報を保存できませんでした。",
    remove_webclass: "WebClass認証情報を削除できませんでした。",
    startup: "起動時の同期設定を保存できませんでした。",
  }[operation];
  return `${prefix} ${safeErrorMessage(error)}`;
}

const mutations = {
  save_moodle: { command: "save_moodle_feed", key: "moodleConfigured", configured: true },
  remove_moodle: { command: "remove_moodle_feed", key: "moodleConfigured", configured: false },
  save_webclass: { command: "save_webclass_credentials", key: "webclassConfigured", configured: true },
  remove_webclass: { command: "remove_webclass_credentials", key: "webclassConfigured", configured: false },
};

// Model state contains only booleans and fixed UI messages, never input/response secrets.
export function createSettingsModel(invoke, onChange = () => {}) {
  const state = { moodleConfigured: null, webclassConfigured: null, syncOnStartup: null, busy: false, error: null, message: "" };
  const snapshot = () => ({ ...state });
  const notify = () => onChange(snapshot());

  async function load() {
    if (state.busy) return false;
    state.busy = true;
    state.error = null;
    state.message = "設定状態を確認しています…";
    notify();
    try {
      const response = await invoke("get_settings_status");
      if (typeof response?.moodleConfigured !== "boolean" || typeof response?.webclassConfigured !== "boolean") {
        throw new Error("Invalid settings status");
      }
      state.moodleConfigured = response.moodleConfigured;
      state.webclassConfigured = response.webclassConfigured;
      state.syncOnStartup = response.syncOnStartup === true;
      state.message = "";
      return true;
    } catch (error) {
      state.moodleConfigured = null;
      state.webclassConfigured = null;
      state.syncOnStartup = null;
      state.error = settingsErrorMessage("status", error);
      state.message = "";
      return false;
    } finally {
      state.busy = false;
      notify();
    }
  }

  async function mutate(operation, inputs = {}) {
    if (state.busy) return false;
    if (!Object.hasOwn(mutations, operation)) throw new Error("Unknown settings operation");
    const mutation = mutations[operation];
    // Arguments stay local to this invocation. No state, browser storage or logs.
    let arguments_;
    if (operation === "save_moodle") arguments_ = { secretUrl: inputs.url.value };
    if (operation === "save_webclass") arguments_ = {
      credentials: { universityId: inputs.universityId.value, password: inputs.password.value },
    };
    clearSecretInputs(inputs);
    state.busy = true;
    state.error = null;
    state.message = mutation.configured ? "保存しています…" : "削除しています…";
    notify();
    try {
      await invoke(mutation.command, arguments_);
      state[mutation.key] = mutation.configured;
      state.message = mutation.configured ? "保存しました。" : "削除しました。";
      return true;
    } catch (error) {
      // A failed replacement retains the last known configured state.
      state.error = settingsErrorMessage(operation, error);
      state.message = "";
      return false;
    } finally {
      if (arguments_?.credentials) {
        arguments_.credentials.universityId = "";
        arguments_.credentials.password = "";
      }
      if (arguments_) arguments_.secretUrl = "";
      arguments_ = undefined;
      clearSecretInputs(inputs);
      state.busy = false;
      notify();
    }
  }

  async function setStartupSync(enabled) {
    if (state.busy || typeof enabled !== "boolean") return false;
    state.busy = true;
    state.error = null;
    state.message = "保存しています…";
    notify();
    try {
      await invoke("set_startup_sync", { enabled });
      state.syncOnStartup = enabled;
      state.message = "保存しました。";
      return true;
    } catch (error) {
      state.error = settingsErrorMessage("startup", error);
      state.message = "";
      return false;
    } finally {
      state.busy = false;
      notify();
    }
  }

  return { snapshot, load, mutate, setStartupSync };
}
