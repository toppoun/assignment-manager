import { createListView } from "./list_view.js";
import { createTauriApi, invokeTauri } from "./tauri_api.js";
import { initializeSync } from "./sync.js";
import { createSyncView } from "./sync_view.js";
import { settingsModel } from "./settings_view.js";

const { load } = createListView(createTauriApi());
const syncModel = createSyncView(invokeTauri, load);
void initializeSync(load, async () => {
  await settingsModel.load();
  return settingsModel.snapshot();
}, syncModel);
