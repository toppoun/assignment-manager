import { clearSecretInputs, configuredPresentation, createSettingsModel } from "./settings.js";

const listView = document.querySelector("#list-view");
const settingsView = document.querySelector("#settings-view");
const listButton = document.querySelector("#show-list");
const settingsButton = document.querySelector("#show-settings");
const reload = document.querySelector("#settings-reload");
const errorBanner = document.querySelector("#settings-error");
const message = document.querySelector("#settings-message");
const moodleInputs = { url: document.querySelector("#moodle-url") };
const webclassInputs = {
  universityId: document.querySelector("#webclass-id"), password: document.querySelector("#webclass-password"),
};
const allInputs = { ...moodleInputs, ...webclassInputs };
const sources = {
  moodle: { status: document.querySelector("#moodle-status"), save: document.querySelector("#moodle-save"), remove: document.querySelector("#moodle-remove"), inputs: moodleInputs },
  webclass: { status: document.querySelector("#webclass-status"), save: document.querySelector("#webclass-save"), remove: document.querySelector("#webclass-remove"), inputs: webclassInputs },
};
let pendingFocus = null;
const model = createSettingsModel((command, arguments_) => window.__TAURI__.core.invoke(command, arguments_), render);

function render(state) {
  if (state.busy && pendingFocus === null) pendingFocus = document.activeElement;
  reload.disabled = state.busy;
  settingsView.setAttribute("aria-busy", String(state.busy));
  message.textContent = state.message;
  errorBanner.hidden = state.error === null;
  errorBanner.textContent = state.error ?? "";
  for (const [name, source] of Object.entries(sources)) {
    const configured = state[`${name}Configured`];
    const presentation = configuredPresentation(configured);
    source.status.textContent = presentation.label;
    source.status.dataset.state = presentation.state;
    source.save.textContent = configured ? "置き換えて保存" : "保存";
    source.save.disabled = state.busy || configured === null;
    source.remove.disabled = state.busy || configured !== true;
    for (const input of Object.values(source.inputs)) input.disabled = state.busy || configured === null;
  }
  if (!state.busy && pendingFocus) {
    if (!settingsView.hidden && (document.activeElement === document.body || document.activeElement === pendingFocus)) {
      (pendingFocus.isConnected && !pendingFocus.disabled ? pendingFocus : reload).focus();
    }
    pendingFocus = null;
  }
}

function showView(settings) {
  clearSecretInputs(allInputs);
  listView.hidden = settings;
  settingsView.hidden = !settings;
  listButton.setAttribute("aria-pressed", String(!settings));
  settingsButton.setAttribute("aria-pressed", String(settings));
  if (settings) void model.load();
}

listButton.addEventListener("click", () => showView(false));
settingsButton.addEventListener("click", () => showView(true));
reload.addEventListener("click", () => { clearSecretInputs(allInputs); void model.load(); });
document.querySelector("#moodle-form").addEventListener("submit", event => {
  event.preventDefault();
  void model.mutate("save_moodle", moodleInputs);
});
document.querySelector("#webclass-form").addEventListener("submit", event => {
  event.preventDefault();
  void model.mutate("save_webclass", webclassInputs);
});
sources.moodle.remove.addEventListener("click", () => { clearSecretInputs(moodleInputs); void model.mutate("remove_moodle"); });
sources.webclass.remove.addEventListener("click", () => { clearSecretInputs(webclassInputs); void model.mutate("remove_webclass"); });
window.addEventListener("pagehide", () => clearSecretInputs(allInputs));
render(model.snapshot());
