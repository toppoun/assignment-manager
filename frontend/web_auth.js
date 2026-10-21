import { createListView } from "./list_view.js";
import { createHttpApi } from "./http_api.js";

export function createWebApp(document_ = document, fetch_ = (...args) => globalThis.fetch(...args)) {
  const get = id => document_.querySelector(`#${id}`);
  let view = null;
  let generation = 0;
  let pending = false;
  let logoutUnconfirmed = false;
  const api = createHttpApi(fetch_, signedOut);
  function signedOut() {
    ++generation;
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
    get("list-view").hidden = false;
    view = createListView(api, document_);
    await view.load();
  }
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
  return { start: refreshSession, resume };
}
