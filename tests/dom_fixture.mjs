import { DEFAULT_CONTROLS } from "../frontend/presentation.js";
// Minimal DOM surface used by the view; any HTML assignment fails the test.
export function documentFixture() {
  const elements = [];
  class Element {
    children = []; dataset = {}; listeners = {}; attributes = {}; value = ""; hidden = false; isConnected = true;
    constructor(tag) { this.tagName = tag; elements.push(this); }
    set innerHTML(_) { throw new Error("unsafe HTML rendering"); }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this.attributes[name] = value; }
    addEventListener(name, fn) { this.listeners[name] = fn; }
    querySelectorAll(tag) { return this.children.flatMap(c => [c, ...c.querySelectorAll(tag)]).filter(c => c.tagName === tag); }
    get options() { return this.children; }
    focus() { doc.activeElement = this; }
    click() { return this.listeners.click?.(); }
  }
  const ids = Object.fromEntries(["assignments", "empty", "status", "count", "error", "reload", "reset-filters", "filters",
    "completion-filter", "deadline-filter", "course-filter", "sort", "empty-title", "empty-description",
    "web-sync-panel", "web-sync-now", "web-sync-refresh", "web-sync-message", "web-sync-error", "web-sync-results", "view-navigation", "settings-view", "show-list", "show-settings",
    "settings-message", "settings-error", "reauth-status", "moodle-status", "webclass-status", "reauth-form", "reauth-submit", "reauth-password", "settings-reload", "moodle-form", "webclass-form", "moodle-save", "moodle-remove", "webclass-save", "webclass-remove", "moodle-url", "webclass-id", "webclass-password",
    "auth-status", "login-view", "list-view", "web-login", "web-password", "login-form", "login-submit", "logout", "session-retry", "retry-logout"].map(id => [id, new Element("div")]));
  for (const [key, value] of Object.entries(DEFAULT_CONTROLS)) ids[key === "sort" ? "sort" : `${key}-filter`].value = value;
  const doc = { querySelector: selector => ids[selector.slice(1)], createElement: tag => new Element(tag), activeElement: null };
  return { doc, ids, elements };
}
