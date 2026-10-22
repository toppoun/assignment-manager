import { createListView } from "./list_view.js";
import { createHttpApi } from "./http_api.js";
import { createWebSyncView } from "./web_sync.js";
import { createWebSettingsView } from "./web_settings.js";

export function createWebApp(document_ = document, fetch_ = (...args) => globalThis.fetch(...args)) {
  const get = id => document_.querySelector(`#${id}`);
  let view = null;
  let syncView = null;
  let settingsVisible = false;
  let authenticated = false;
  let generation = 0;
  let pending = false;
  let logoutUnconfirmed = false;
  const api = createHttpApi(fetch_, signedOut);
  function signedOut() {
    ++generation;
    authenticated = false;
    syncView?.dispose(); syncView = null;
    get("web-sync-panel").hidden = true;
    get("view-navigation").hidden = true;
    get("settings-view").hidden = true;
    view?.dispose(); view = null;
    get("list-view").hidden = true;
    get("login-view").hidden = false;
    get("web-password").value = "";
    get("web-login").value = "";
  }
  async function signedIn() {
    view?.dispose();
    get("auth-status").textContent = "";
    get("login-view").hidden = true;
    authenticated = true;
    get("view-navigation").hidden = false;
    syncView?.dispose();
    syncView = createWebSyncView(api, () => settingsVisible ? Promise.resolve(true) : view?.load(), document_);
    get("web-sync-panel").hidden = false;
    await showView(false);
  }
  async function showView(settings) {
    if (!authenticated) return;
    view?.dispose();
    settingsVisible = settings;
    get("list-view").hidden = settings;
    get("settings-view").hidden = !settings;
    get("show-list").setAttribute("aria-pressed", String(!settings));
    get("show-settings").setAttribute("aria-pressed", String(settings));
    view = settings ? createWebSettingsView(api, document_) : createListView(api, document_);
    await view.load();
    if (!settings && authenticated) void syncView?.load();
  }
  get("show-list").addEventListener("click", () => showView(false));
  get("show-settings").addEventListener("click", () => showView(true));
  const message = error => error?.code === "rate_limited" ? "ログイン試行が多すぎます。60秒待ってください。"
    : error?.code === "authentication_failed" ? "ログインIDまたはpasswordを確認してください。"
    : "通信を確認して、もう一度お試しください。";
  async function refreshSession() {
    if (pending || logoutUnconfirmed) return;
    const ticket = generation;
    try { await api.session(); if (ticket === generation) await signedIn(); }
    catch (error) {
      signedOut();
      get("auth-status").textContent = error?.code === "authentication_failed" ? "Webアカウントでログインしてください。" : message(error);
    }
  }
  get("login-form").addEventListener("submit", async event => {
    event.preventDefault();
    if (pending) return;
    pending = true;
    get("login-submit").disabled = true;
    const ticket = generation;
    try {
      // Read once into the request, then clear the input before waiting. No
      // password is kept in application state or browser storage.
      const operation = api.login(get("web-login").value, get("web-password").value);
      get("web-password").value = "";
      await operation;
      if (ticket === generation) { logoutUnconfirmed = false; await signedIn(); }
    } catch (error) { get("auth-status").textContent = message(error); }
    finally { get("web-password").value = ""; pending = false; get("login-submit").disabled = logoutUnconfirmed; }
  });
  get("logout").addEventListener("click", async () => {
    if (pending) return;
    pending = true;
    signedOut(); // Clear DOM and invalidate the list before the network wait.
    get("login-submit").disabled = true;
    try { await api.logout(); logoutUnconfirmed = false; get("auth-status").textContent = "ログアウトしました。"; }
    catch { logoutUnconfirmed = true; get("auth-status").textContent = "ログアウトを確認できません。通信復旧後にログアウトを再試行してください。"; }
    finally { pending = false; get("login-submit").disabled = logoutUnconfirmed; get("retry-logout").hidden = !logoutUnconfirmed; }
  });
  get("retry-logout").addEventListener("click", async () => {
    if (pending) return;
    // Refresh only the CSRF token, without showing assignment data.
    try { await api.session(); logoutUnconfirmed = false; get("logout").click(); }
    catch (error) {
      if (error?.code === "authentication_failed") { logoutUnconfirmed = false; get("retry-logout").hidden = true; get("login-submit").disabled = false; }
      else get("auth-status").textContent = message(error);
    }
  });
  get("session-retry").addEventListener("click", refreshSession);
  // A back/forward cache restore must re-check the server before showing data.
  const resume = () => { signedOut(); api.clearSession(); return refreshSession(); };
  return { start: refreshSession, resume, suspend: () => { signedOut(); api.clearSession(); }, clearSecrets: () => { get("web-password").value = ""; view?.clear?.(); } };
}
