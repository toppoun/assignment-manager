// Explicit browser transport. Fixed relative URLs, no desktop detection/fallback.
const completions = ["incomplete", "completed"];
const codes = new Set(["invalid_query", "invalid_request", "invalid_host", "cross_origin",
  "not_found", "method_not_allowed", "unsupported_media_type", "payload_too_large", "busy", "unavailable", "internal_error",
  "authentication_failed", "forbidden", "invalid_csrf", "rate_limited", "reauth_required", "reauth_failed", "vault_unavailable", "sync_running"]);
const failure = (code, outcome = "unknown") => ({ code, outcome });

export function serializeQuery(request) {
  const params = new URLSearchParams({ completion: request.completion, deadline: request.deadline,
    sort: request.sort, courseMode: request.course.kind });
  if (request.course.kind === "course") params.set("courseId", request.course.id);
  return params.toString();
}

function validList(value) {
  const strings = (...values) => values.every(v => typeof v === "string");
  const course = c => c && strings(c.id, c.displayLabel)
    && ["moodle", "web_class", "manual", "unbound"].includes(c.origin);
  const deadline = d => d && (d.kind === "none"
    || (d.kind === "date_only" && typeof d.value === "string" && /^[+-]?\d{4,}-\d{2}-\d{2}$/.test(d.value))
    || (d.kind === "date_time" && typeof d.value === "string" && d.value.endsWith("Z") && Number.isFinite(Date.parse(d.value))));
  return value?.timeZone === "Asia/Tokyo" && typeof value.evaluatedAt === "string"
    && value.evaluatedAt.endsWith("Z") && Number.isFinite(Date.parse(value.evaluatedAt))
    && Number.isSafeInteger(value.totalCount) && value.totalCount >= 0
    && Array.isArray(value.courses) && value.courses.every(course)
    && new Set(value.courses.map(c => c.id)).size === value.courses.length
    && Array.isArray(value.items) && value.items.length <= value.totalCount
    && new Set(value.items.map(i => i?.id)).size === value.items.length
    && value.items.every(i => i && strings(i.id, i.title) && deadline(i.deadline)
      && completions.includes(i.completion) && typeof i.hasSubmissionUrl === "boolean"
      && ["unknown", "none", "moodle", "web_class", "external"].includes(i.submission)
      && ["none", "overdue", "today", "upcoming"].includes(i.status)
      && (i.course === null || (course(i.course) && value.courses.some(c => c.id === i.course.id))));
}

const validRunId = id => typeof id === "string" && /^[a-f0-9]{64}$/.test(id);
const sourceFailures = ["network_failed", "authentication_failed", "invalid_configuration", "source_data_invalid", "persistence_failed", "credential_unavailable", "unexpected"];
function validSource(value) {
  return value?.status === "not_configured" || (value?.status === "failed" && sourceFailures.includes(value.error))
    || (value?.status === "success" && ["created", "updated", "unchanged"].every(k => Number.isSafeInteger(value.counts?.[k]) && value.counts[k] >= 0));
}
function validRun(value, terminal = false) {
  if (!validRunId(value?.runId)) return false;
  if (value.state === "running") return !terminal;
  if (typeof value.shouldReload !== "boolean") return false;
  return (value.state === "finished" && validSource(value.report?.moodle) && validSource(value.report?.webclass))
    || (value.state === "failed" && ["job_failed", "already_running"].includes(value.error));
}

export function createHttpApi(fetch_ = (...args) => globalThis.fetch(...args), onUnauthorized = () => {}) {
  let csrf = null;
  let generation = 0;
  const forget = () => { csrf = null; ++generation; };
  async function json(path, options = {}, noContent = false, expectedStatus = null) {
    const ticket = generation;
    let response;
    try {
      response = await fetch_(path, { mode: "same-origin", credentials: "same-origin", cache: "no-store",
        redirect: "error", signal: AbortSignal.timeout(10000), ...options });
    } catch { throw failure("network_failed"); }
    if (ticket !== generation) throw failure("authentication_failed", "rejected");
    if (response.status === 401) {
      forget(); onUnauthorized();
      throw failure("authentication_failed", "rejected");
    }
    if (noContent && response.status === 204) return;
    let value;
    try {
      if (!/^application\/json(?:;|$)/i.test(response.headers.get("content-type") ?? "")) throw null;
      value = await response.json();
    } catch {
      if (response.status === 403) { forget(); onUnauthorized(); throw failure("authentication_failed", "rejected"); }
      throw failure("response_invalid");
    }
    if (ticket !== generation) throw failure("authentication_failed", "rejected");
    if (response.status === 403 && !["reauth_required", "reauth_failed"].includes(value?.error?.code)) {
      forget(); onUnauthorized(); throw failure("authentication_failed", "rejected");
    }
    if (!response.ok) {
      const code = codes.has(value?.error?.code) ? value.error.code : "http_failed";
      // 5xx may be returned by an intermediary after the server committed.
      throw failure(code, response.status >= 400 && response.status < 500 ? "rejected" : "unknown");
    }
    if (expectedStatus !== null && response.status !== expectedStatus) throw failure("response_invalid");
    if (noContent) throw failure("response_invalid");
    return value;
  }
  const mutation = (method, body, token = csrf) => ({ method,
    headers: { "Content-Type": "application/json", "X-Assignment-Manager": "web-3a", ...(token ? { "X-CSRF-Token": token } : {}) },
    body: JSON.stringify(body) });
  function acceptSession(value) {
    if (value?.authenticated !== true || typeof value.userId !== "string"
        || !/^[a-f0-9]{64}$/.test(value.csrfToken) || !Number.isSafeInteger(value.expiresAt)
        || !Number.isSafeInteger(value.absoluteExpiresAt)) throw failure("response_invalid");
    csrf = value.csrfToken;
    return value;
  }
  return {
    clearSession: forget,
    async session() { return acceptSession(await json("/api/v1/session")); },
    async login(login, password) {
      forget();
      return acceptSession(await json("/api/v1/auth/login", mutation("POST", { login, password }, null)));
    },
    async logout() {
      const token = csrf;
      forget(); // Invalidate outstanding reads before issuing logout.
      try { await json("/api/v1/auth/logout", mutation("POST", {}, token)); }
      finally { forget(); }
    },
    async startSync(key) {
      if (typeof key !== "string" || !/^[A-Za-z0-9._-]{1,128}$/.test(key)) throw failure("invalid_request", "rejected");
      const options = mutation("POST", {});
      options.headers["Idempotency-Key"] = key;
      const value = await json("/api/v1/sync", options, false, 202);
      if (!validRunId(value?.runId) || !["running", "finished", "failed"].includes(value.state)
          || value.statusUrl !== `/api/v1/sync/runs/${value.runId}`) throw failure("response_invalid");
      return { runId: value.runId, state: value.state };
    },
    async syncStatus() {
      const value = await json("/api/v1/sync/status");
      if (typeof value?.running !== "boolean" || (value.running ? !validRunId(value.activeRunId) : value.activeRunId !== null)
          || (value.lastRun !== null && !validRun(value.lastRun, true))) throw failure("response_invalid");
      return value;
    },
    async syncRun(id) {
      if (!validRunId(id)) throw failure("invalid_request", "rejected");
      const value = await json(`/api/v1/sync/runs/${id}`);
      if (!validRun(value) || value.runId !== id) throw failure("response_invalid");
      return value;
    },
    async reauthenticate(password) { await json("/api/v1/auth/reauth", mutation("POST", { password }), true); },
    async settingsStatus() {
      const value = await json("/api/v1/settings/status");
      if (typeof value?.moodleConfigured !== "boolean" || typeof value?.webclassConfigured !== "boolean"
          || Object.keys(value).length !== 2) throw failure("response_invalid");
      return { moodleConfigured: value.moodleConfigured, webclassConfigured: value.webclassConfigured };
    },
    async saveMoodle(calendarUrl) { await json("/api/v1/settings/moodle", mutation("PUT", { calendarUrl }), true); },
    async saveWebclass(universityId, password) { await json("/api/v1/settings/webclass", mutation("PUT", { universityId, password }), true); },
    async removeMoodle() { await json("/api/v1/settings/moodle", mutation("DELETE", {}), true); },
    async removeWebclass() { await json("/api/v1/settings/webclass", mutation("DELETE", {}), true); },
    async listAssignments(request) {
      const value = await json(`/api/v1/assignments?${serializeQuery(request)}`);
      if (!validList(value)) throw failure("response_invalid");
      return value;
    },
    async setCompletion(id, completion) {
      if (typeof id !== "string" || !id.trim() || !completions.includes(completion)) {
        throw failure("invalid_request", "rejected");
      }
      const value = await json(`/api/v1/assignments/${encodeURIComponent(id)}/completion`, mutation("PUT", { completion }));
      if (value?.id !== id || value.completion !== completion) throw failure("response_invalid");
      return value.completion;
    },
  };
}
