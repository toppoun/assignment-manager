// Explicit browser transport. Fixed relative URLs, no desktop detection/fallback.
const completions = ["incomplete", "completed"];
const codes = new Set(["invalid_query", "invalid_request", "invalid_host", "cross_origin",
  "not_found", "method_not_allowed", "unsupported_media_type", "payload_too_large", "busy", "unavailable", "internal_error"]);
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

export function createHttpApi(fetch_ = (...args) => globalThis.fetch(...args)) {
  async function json(path, options = {}) {
    let response;
    try {
      response = await fetch_(path, { mode: "same-origin", credentials: "omit", cache: "no-store",
        redirect: "error", signal: AbortSignal.timeout(10000), ...options });
    } catch { throw failure("network_failed"); }
    let value;
    try {
      if (!/^application\/json(?:;|$)/i.test(response.headers.get("content-type") ?? "")) throw null;
      value = await response.json();
    } catch { throw failure("response_invalid"); }
    if (!response.ok) {
      const code = codes.has(value?.error?.code) ? value.error.code : "http_failed";
      // 5xx may be returned by an intermediary after the server committed.
      throw failure(code, response.status >= 400 && response.status < 500 ? "rejected" : "unknown");
    }
    return value;
  }
  return {
    async listAssignments(request) {
      const value = await json(`/api/v1/assignments?${serializeQuery(request)}`);
      if (!validList(value)) throw failure("response_invalid");
      return value;
    },
    async setCompletion(id, completion) {
      if (typeof id !== "string" || !id.trim() || !completions.includes(completion)) {
        throw failure("invalid_request", "rejected");
      }
      const value = await json(`/api/v1/assignments/${encodeURIComponent(id)}/completion`, {
        method: "PUT", headers: { "Content-Type": "application/json", "X-Assignment-Manager": "web-2" },
        body: JSON.stringify({ completion }),
      });
      if (value?.id !== id || value.completion !== completion) throw failure("response_invalid");
      return value.completion;
    },
  };
}
