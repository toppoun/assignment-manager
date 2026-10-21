import { DEFAULT_CONTROLS, buildQueryRequest, operationErrorMessage } from "./presentation.js";

// A sync refresh waits for an active query/write instead of being dropped as busy.
export function createListTaskRunner() {
  let current = null;
  return function run(task) {
    if (current) return current.then(() => run(task), () => run(task));
    current = Promise.resolve(task()).finally(() => { current = null; });
    return current;
  };
}

// Shared controller: serialized local operations plus query generations. Controls
// remain usable during I/O; a superseded response can never replace the current view.
export function createAssignmentListModel(api, onChange = () => {}) {
  const run = createListTaskRunner();
  const state = { controls: { ...DEFAULT_CONTROLS }, response: null, loading: false,
    pendingId: null, needsRefresh: false, error: null, message: "", write: null };
  let generation = 0;
  let appliedGeneration = -1;
  const snapshot = () => ({ ...state, controls: { ...state.controls } });
  const notify = () => onChange(snapshot());

  async function reconcile() {
    if (state.write?.kind !== "unknown") return;
    // An active filter may hide the assignment. Confirm against an unfiltered
    // snapshot before allowing any further write; never send a compensating toggle.
    const all = await api.listAssignments(buildQueryRequest({ ...DEFAULT_CONTROLS, completion: "all" }));
    const item = all.items.find(item => item.id === state.write.id);
    if (!item) throw { code: "not_found" };
    state.write = { ...state.write, kind: "confirmed", completion: item.completion };
  }

  async function loadNow(ticket) {
    if (ticket !== generation || ticket === appliedGeneration) return false;
    const selected = { ...state.controls };
    state.loading = true;
    state.error = null;
    state.message = "一覧を更新しています…";
    notify();
    try {
      await reconcile();
      if (ticket !== generation) return false;
      const response = await api.listAssignments(buildQueryRequest(selected));
      if (ticket !== generation) return false;
      state.response = response;
      state.needsRefresh = false;
      appliedGeneration = ticket;
      state.message = state.write?.kind === "saved" ? "完了状態を保存し、一覧を更新しました。"
        : state.write?.kind === "confirmed" ? `実際の完了状態を確認しました（${state.write.completion === "completed" ? "完了済み" : "未完了"}）。` : "";
      state.error = null;
      return true;
    } catch (error) {
      if (ticket !== generation) return false;
      const operation = state.write?.kind === "saved" ? "refresh_after_toggle"
        : state.write?.kind === "unknown" ? "completion_unknown" : "query";
      state.error = operationErrorMessage(operation, error);
      state.message = state.needsRefresh ? "状態を確認するまで完了操作を停止しています。再読み込みしてください。"
        : state.response ? "前回取得した一覧を表示しています。再読み込みしてください。" : "";
      // A stale snapshot may no longer match the selected filters or remote state.
      state.needsRefresh = true;
      return false;
    } finally {
      state.loading = ticket !== generation;
      notify();
    }
  }

  function reload(controls = state.controls) {
    buildQueryRequest(controls);
    if (!state.needsRefresh && state.pendingId === null) state.write = null;
    state.controls = { ...controls };
    const ticket = ++generation;
    state.loading = true;
    notify();
    return run(() => loadNow(ticket));
  }

  function setCompletion(id, desired) {
    if (state.pendingId !== null || state.loading || state.needsRefresh
        || !["completed", "incomplete"].includes(desired)
        || !state.response?.items.some(item => item.id === id)) return Promise.resolve(false);
    state.pendingId = id; // Set synchronously, before enqueue, to block double input.
    state.needsRefresh = true;
    state.error = null;
    state.write = null;
    state.message = "完了状態を保存しています…";
    ++generation;
    notify();
    return run(async () => {
      try {
        await api.setCompletion(id, desired);
        state.write = { kind: "saved", id };
      } catch (error) {
        if (error?.outcome === "rejected") {
          state.needsRefresh = true;
          state.error = operationErrorMessage("toggle", error);
          state.message = "保存要求が拒否されました。一覧を再読み込みしてください。";
          // Retain the error until a subsequent explicit refresh.
          return false;
        }
        state.write = { kind: "unknown", id };
        state.message = "保存結果が不明です。実際の状態を確認しています…";
        notify();
      } finally {
        // Cleared below after reconciliation, including a failed read.
        if (!state.write) { state.pendingId = null; notify(); }
      }
      try { return await loadNow(generation); }
      finally { state.pendingId = null; notify(); }
    });
  }
  return { snapshot, reload, setCompletion };
}
