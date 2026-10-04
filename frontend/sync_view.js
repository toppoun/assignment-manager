import { createSyncModel, syncButtonState } from "./sync.js";
import { settingsModel } from "./settings_view.js";

export function createSyncView(invoke, reloadList) {
  const button = document.querySelector("#sync-now");
  const panel = document.querySelector("#sync-panel");
  const message = document.querySelector("#sync-message");
  const error = document.querySelector("#sync-error");
  const results = document.querySelector("#sync-results");
  const model = createSyncModel(invoke, reloadList, state => {
    const presentation = syncButtonState(state);
    button.disabled = presentation.disabled;
    button.textContent = presentation.label;
    panel.setAttribute("aria-busy", String(state.busy));
    message.textContent = state.message;
    error.hidden = state.error === null;
    error.textContent = state.error ?? "";
    results.replaceChildren(...state.results.map(result => {
      const item = document.createElement("li");
      item.className = `sync-result sync-${result.outcome}`;
      item.textContent = result.message;
      return item;
    }));
  });
  button.addEventListener("click", () => { void model.run(); });
  document.addEventListener("settings-status-changed", event => model.configure(event.detail));
  model.configure(settingsModel.snapshot());
  return model;
}
