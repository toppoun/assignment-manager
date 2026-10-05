import { safeErrorMessage } from "./presentation.js";

const failures = {
  network_failed: "通信に失敗しました。接続を確認して再試行してください。",
  authentication_failed: "認証に失敗しました。設定の認証情報を確認してください。",
  invalid_configuration: "設定を確認してください。",
  source_data_invalid: "サービスのデータを読み取れませんでした。",
  persistence_failed: "取得したデータを保存できませんでした。",
  credential_unavailable: "保存済みの認証情報を利用できませんでした。",
  unexpected: "同期を完了できませんでした。",
};

export function sourceResultPresentation(source, result) {
  if (result?.status === "not_configured") return { source, outcome: "missing", message: `${source} 未設定` };
  if (result?.status === "failed") return { source, outcome: "failed",
    message: `${source} ${Object.hasOwn(failures, result.error) ? failures[result.error] : failures.unexpected}` };
  if (result?.status === "success") {
    const counts = result.counts;
    const suffix = ["created", "updated", "unchanged"].every(key => Number.isSafeInteger(counts?.[key]) && counts[key] >= 0)
      ? `（新規 ${counts.created}・更新 ${counts.updated}・変更なし ${counts.unchanged}）` : "";
    return { source, outcome: "success", message: `${source} 同期完了${suffix}` };
  }
  throw new Error("Invalid sync result");
}

export function syncButtonState(state) {
  const configured = state.moodleConfigured === true || state.webclassConfigured === true;
  return { disabled: state.busy || !configured, label: state.busy ? "同期中…" : "同期" };
}

export function createSyncModel(invoke, reloadList, onChange = () => {}) {
  const state = { moodleConfigured: null, webclassConfigured: null, syncOnStartup: false,
    busy: false, results: [], message: "設定状態を確認しています…", error: null };
  const snapshot = () => ({ ...state, results: state.results.map(result => ({ ...result })) });
  const notify = () => onChange(snapshot());

  function configure(flags) {
    for (const key of ["moodleConfigured", "webclassConfigured"]) {
      state[key] = typeof flags?.[key] === "boolean" ? flags[key] : null;
    }
    state.syncOnStartup = flags?.syncOnStartup === true;
    if (!state.busy && state.results.length === 0) state.message =
      state.moodleConfigured === null || state.webclassConfigured === null ? "設定状態を確認できません。設定画面で状態を確認してください。"
        : syncButtonState(state).disabled ? "同期するサービスが設定されていません。設定画面から追加できます。" : "";
    notify();
  }

  async function execute(command) {
    if (state.busy) return false;
    const idleMessage = state.message;
    state.busy = true;
    state.results = [];
    state.error = null;
    state.message = "同期中… ローカルの課題一覧と完了操作は引き続き利用できます。";
    notify();
    let reload = false;
    try {
      const response = await invoke(command);
      if (command === "run_startup_sync" && response === null) {
        state.message = idleMessage;
        return false;
      }
      if (response?.status === "already_running") {
        state.message = "同期はすでに実行中です。";
        return false;
      }
      if (response?.status !== "completed") throw new Error("Invalid sync response");
      reload = response.moodle?.status !== "not_configured" || response.webclass?.status !== "not_configured";
      state.results = [sourceResultPresentation("Moodle", response.moodle), sourceResultPresentation("WebClass", response.webclass)];
      state.message = reload ? "同期処理が終了しました。" : "同期するサービスが設定されていません。";
      return true;
    } catch (error) {
      state.error = `同期を完了できませんでした。 ${safeErrorMessage(error)}`;
      state.message = "ローカルの課題一覧は引き続き利用できます。";
      // A worker may have committed data before an unexpected command failure.
      reload = true;
      return false;
    } finally {
      notify();
      try { if (reload) await reloadList(); }
      catch (error) {
        state.error = `同期後の一覧を更新できませんでした。 ${safeErrorMessage(error)}`;
      }
      finally { state.busy = false; notify(); }
    }
  }

  async function run() {
    if (syncButtonState(state).disabled) return false;
    return execute("sync_now");
  }
  // Eligibility and the once guard belong to the application handle, across views.
  async function startup() { return execute("run_startup_sync"); }
  return { snapshot, configure, run, startup };
}

// Explicit ordering keeps the first local list independent of network success.
export async function initializeSync(loadLocalList, loadSettings, model) {
  await loadLocalList();
  model.configure(await loadSettings());
  return model.startup();
}
