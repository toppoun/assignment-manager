import { sourceResultPresentation, syncButtonState } from "./sync.js";

function delay(ms, signal) {
  return new Promise(resolve => {
    const finish = () => { clearTimeout(timer); signal.removeEventListener("abort", finish); resolve(); };
    const timer = setTimeout(finish, ms);
    signal.addEventListener("abort", finish, { once: true });
    if (signal.aborted) finish();
  });
}
// One controller per authenticated lifetime, surviving Settings navigation.
export function createWebSyncModel(api, reloadList, onChange = () => {}, {
  wait = delay, newKey = () => globalThis.crypto.randomUUID(), maxPolls = 120, interval = 1500,
} = {}) {
  let active = true, pending = false;
  const lifetime = new AbortController();
  const state = { moodleConfigured: null, webclassConfigured: null, busy: false,
    uncertain: false, runId: null, results: [], message: "同期状態を確認しています…", error: "" };
  let key = null;
  const snapshot = () => ({ ...state, results: state.results.map(r => ({ ...r })) });
  const notify = () => { if (active) onChange(snapshot()); };
  const current = () => active && !lifetime.signal.aborted;
  async function finished(run, reload) {
    state.runId = null; key = null; state.uncertain = false;
    if (run.state === "finished") {
      state.results = [sourceResultPresentation("Moodle", run.report.moodle), sourceResultPresentation("WebClass", run.report.webclass)];
      state.message = "前回の同期処理が終了しました。";
      state.error = "";
    } else {
      state.results = [];
      state.message = "同期処理の結果を取得できませんでした。一覧で保存状態を確認してください。";
      state.error = run.error === "already_running" ? "同期はすでに実行中です。状態を再確認してください。" : "サーバーの同期処理が中断しました。";
    }
    if (reload && run.shouldReload && current()) {
      try { if (await reloadList() === false && current()) state.error = "同期は終了しましたが、一覧を更新できません。再読み込みしてください。"; }
      catch { if (current()) state.error = "同期は終了しましたが、一覧を更新できません。再読み込みしてください。"; }
    }
  }
  async function poll() {
    state.message = "同期中… 完了操作は引き続き利用できます。"; notify();
    for (let i = 0; i < maxPolls && current(); ++i) {
      const run = await api.syncRun(state.runId);
      if (!current()) return false;
      if (run.state !== "running") { await finished(run, true); return true; }
      await wait(interval, lifetime.signal);
    }
    if (current()) {
      state.uncertain = true;
      state.message = "ブラウザの待機時間を超えました。サーバーでは同期が続いている可能性があります。状態を再確認してください。";
    }
    return false;
  }
  async function execute(start) {
    if (!active || pending) return false;
    pending = true; state.busy = true; state.error = ""; notify();
    try {
      if (start && !state.runId) {
        key ??= newKey(); // An ambiguous POST retries this same key, never a new run.
        const accepted = await api.startSync(key);
        if (!current()) return false;
        state.runId = accepted.runId;
      } else if (state.runId) {
        return await poll();
      } else {
        const [flags, status] = await Promise.all([api.settingsStatus(), api.syncStatus()]);
        if (!current()) return false;
        Object.assign(state, flags);
        // Refresh/navigation is read-only. Only an explicit sync-button action
        // retries a lost POST with the retained key (within server retention).
        if (key) {
          state.uncertain = true;
          state.message = "受付結果が不明です。同じ要求を再確認できます。再起動・1時間の保存期限後は重複実行を防げません。一覧も確認してください。";
          return false;
        }
        if (status.running) state.runId = status.activeRunId;
        else {
          state.runId = null; state.uncertain = false;
          if (status.lastRun) await finished(status.lastRun, true);
          else { state.results = []; state.message = "同期履歴はありません。再起動後の過去runは復元されません。"; }
          return true;
        }
      }
      return await poll();
    } catch (error) {
      if (!current()) return false;
      state.uncertain = true;
      state.message = "同期の結果は未確認です。サーバーの処理は継続している可能性があります。";
      state.error = error?.code === "not_found" ? "同期履歴が見つかりません。再起動または保存期限切れの可能性があります。一覧を再読み込みしてください。"
        : error?.code === "rate_limited" ? "同期要求が多すぎます。60秒待って状態を再確認してください。"
        : error?.code === "busy" ? "設定を保存しています。完了後に再試行してください。"
        : "通信を確認して同期状態を再確認してください。";
      // An explicit rejection means the key has not been accepted. A run 404
      // after restart/expiry requires a fresh, deliberate sync action.
      if ((start && error?.outcome === "rejected") || error?.code === "not_found") { key = null; state.runId = null; }
      return false;
    } finally {
      pending = false;
      if (current()) { state.busy = false; notify(); }
    }
  }
  const refresh = () => execute(false);
  function run() {
    if (key && !state.runId) return execute(true);
    if (state.uncertain || state.runId) return refresh();
    if (syncButtonState(state).disabled) return Promise.resolve(false);
    return execute(true);
  }
  return { snapshot, refresh, run, dispose() {
    active = false; lifetime.abort(); key = null;
    Object.assign(state, { runId: null, results: [], message: "", error: "", busy: false, uncertain: false });
  } };
}
export function createWebSyncView(api, reloadList, document_ = document) {
  const get = id => document_.querySelector(`#${id}`);
  const listeners = new AbortController();
  const model = createWebSyncModel(api, reloadList, state => {
    const button = syncButtonState(state);
    get("web-sync-now").disabled = state.busy || (!state.uncertain && button.disabled);
    get("web-sync-now").textContent = state.uncertain ? "状態を再確認" : button.label;
    get("web-sync-refresh").disabled = state.busy;
    get("web-sync-panel").setAttribute("aria-busy", String(state.busy));
    get("web-sync-message").textContent = state.message;
    get("web-sync-error").textContent = state.error;
    get("web-sync-error").hidden = !state.error;
    get("web-sync-results").replaceChildren(...state.results.map(result => {
      const item = document_.createElement("li"); item.textContent = result.message;
      item.className = `sync-result sync-${result.outcome}`; return item;
    }));
  });
  get("web-sync-now").addEventListener("click", () => { void model.run(); }, { signal: listeners.signal });
  get("web-sync-refresh").addEventListener("click", () => { void model.refresh(); }, { signal: listeners.signal });
  return { load: model.refresh, dispose() {
    model.dispose(); listeners.abort();
    for (const id of ["web-sync-message", "web-sync-error"]) get(id).textContent = "";
    get("web-sync-results").replaceChildren();
  } };
}
