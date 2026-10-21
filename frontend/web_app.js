import { createWebApp } from "./web_auth.js";

const app = createWebApp();
void app.start();
window.addEventListener("pageshow", event => { if (event.persisted) void app.resume(); });
window.addEventListener("pagehide", () => app.suspend());
document.addEventListener("visibilitychange", () => { if (document.hidden) app.clearSecrets(); });
