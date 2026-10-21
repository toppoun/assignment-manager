import { createWebApp } from "./web_auth.js";

const app = createWebApp();
void app.start();
window.addEventListener("pageshow", event => { if (event.persisted) void app.resume(); });
