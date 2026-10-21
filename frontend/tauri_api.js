// The desktop entrypoint selects this adapter explicitly; it never uses fetch.
export function invokeTauri(command, arguments_) {
  return window.__TAURI__.core.invoke(command, arguments_);
}

export function createTauriApi(invoke = invokeTauri) {
  return {
    listAssignments: request => invoke("list_assignments", { request }),
    // Desktop retains its existing atomic toggle command. The shared view blocks
    // repeated clicks and stale rows; HTTP uses the separate explicit set API.
    async setCompletion(id, _desired) {
      try { return await invoke("toggle_assignment_completion", { assignmentId: id }); }
      catch (error) { throw { code: error?.code, outcome: "rejected" }; }
    },
  };
}
