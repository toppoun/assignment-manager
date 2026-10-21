import assert from "node:assert/strict";
import { test } from "node:test";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { createHttpApi, serializeQuery } from "../frontend/http_api.js";
import { createTauriApi } from "../frontend/tauri_api.js";
import { createAssignmentListModel } from "../frontend/list_tasks.js";
import { createListView } from "../frontend/list_view.js";
import { DEFAULT_CONTROLS, buildQueryRequest } from "../frontend/presentation.js";

const assignment = (completion = "incomplete") => ({ id: "a_1", title: "日本語の課題", completion,
  deadline: { kind: "date_only", value: "2026-10-22" }, course: null, status: "today",
  submission: "moodle", hasSubmissionUrl: false });
const list = (items = [assignment()]) => ({ items, courses: [], totalCount: 1,
  evaluatedAt: "2026-10-21T15:00:00.123456789Z", timeZone: "Asia/Tokyo" });
const response = (value, status = 200) => new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });
const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };

test("HTTP defaults and every filter/sort serialize without inventing local filtering", async () => {
  const calls = [];
  const api = createHttpApi(async (url, options) => { calls.push({ url, options }); return response(list()); });
  for (const completion of ["all", "incomplete", "completed"]) {
    for (const deadline of ["all", "overdue", "today", "within_seven_days"]) {
      for (const sort of ["deadline", "course", "title"]) {
        const request = buildQueryRequest({ ...DEFAULT_CONTROLS, completion, deadline, sort });
        assert.deepEqual(await api.listAssignments(request), list());
        const query = new URLSearchParams(calls.at(-1).url.split("?")[1]);
        assert.deepEqual(Object.fromEntries(query), { completion, deadline, sort, courseMode: "all" });
      }
    }
  }
  const defaults = serializeQuery(buildQueryRequest(DEFAULT_CONTROLS));
  assert.equal(defaults, "completion=incomplete&deadline=all&sort=deadline&courseMode=all");
  const opaque = " ID &?日本語/01 ";
  const course = new URLSearchParams(serializeQuery(buildQueryRequest({ ...DEFAULT_CONTROLS, course: `course:${opaque}` })));
  assert.equal(course.get("courseId"), opaque);
  assert.equal(course.get("courseMode"), "course");
  assert.equal(new URLSearchParams(serializeQuery(buildQueryRequest({ ...DEFAULT_CONTROLS, course: "unassigned" }))).has("courseId"), false);
  for (const { options } of calls) {
    assert.equal(options.mode, "same-origin");
    assert.equal(options.credentials, "omit");
    assert.equal(options.cache, "no-store");
    assert.equal(options.redirect, "error");
    assert.ok(options.signal instanceof AbortSignal);
  }
});

test("HTTP completion always sends desired state; repeated PUT has the same URL and body", async () => {
  const calls = [];
  const api = createHttpApi(async (url, options) => {
    calls.push({ url, ...options });
    return response({ id: "a /?日本語", completion: JSON.parse(options.body).completion });
  });
  for (const completion of ["completed", "completed", "incomplete", "incomplete"]) {
    assert.equal(await api.setCompletion("a /?日本語", completion), completion);
  }
  for (const call of calls) {
    assert.equal(call.url, "/api/v1/assignments/a%20%2F%3F%E6%97%A5%E6%9C%AC%E8%AA%9E/completion");
    assert.equal(call.method, "PUT");
    assert.deepEqual(call.headers, { "Content-Type": "application/json", "X-Assignment-Manager": "web-2" });
    assert.deepEqual(Object.keys(JSON.parse(call.body)), ["completion"]);
  }
  assert.equal(calls[0].body, calls[1].body);
  await assert.rejects(api.setCompletion("a", "all"), { code: "invalid_request", outcome: "rejected" });
});

test("HTTP errors use only safe fixed codes; network, timeout and invalid responses leave write outcome unknown", async () => {
  for (const [status, code, outcome] of [[400, "invalid_request", "rejected"], [404, "not_found", "rejected"],
    [413, "payload_too_large", "rejected"], [415, "unsupported_media_type", "rejected"],
    [500, "internal_error", "unknown"], [503, "busy", "unknown"]]) {
    const api = createHttpApi(async () => response({ error: { code, message: "PRIVATE SQL path" } }, status));
    await assert.rejects(api.setCompletion("a_1", "completed"), error => {
      assert.deepEqual(error, { code, outcome }); return true;
    });
  }
  for (const fetch_ of [
    async () => { throw new Error("PRIVATE network path"); },
    async () => { throw new DOMException("PRIVATE timeout", "TimeoutError"); },
    async () => new Response("PRIVATE broken JSON", { headers: { "Content-Type": "application/json" } }),
    async () => new Response("PRIVATE HTML", { headers: { "Content-Type": "text/html" } }),
    async () => ({ ok: true, headers: new Headers({ "Content-Type": "application/json" }), json: async () => { throw new Error("lost body"); } }),
    async () => response({ id: "wrong", completion: "completed" }),
    async () => response({ id: "a_1", completion: "incomplete" }),
  ]) {
    await assert.rejects(createHttpApi(fetch_).setCompletion("a_1", "completed"), error => {
      assert.equal(error.outcome, "unknown");
      assert.doesNotMatch(JSON.stringify(error), /PRIVATE|SQL|path/); return true;
    });
  }
  await assert.rejects(createHttpApi(async () => response({ error: { code: "PRIVATE" } }, 500)).listAssignments(buildQueryRequest(DEFAULT_CONTROLS)), { code: "http_failed", outcome: "unknown" });
  for (const value of [null, {}, { ...list(), timeZone: "UTC" }, { ...list(), evaluatedAt: "invalid" },
    list([{ ...assignment(), deadline: { kind: "date_time", value: "invalid" } }]), list([{ ...assignment(), status: "invented" }])]) {
    await assert.rejects(createHttpApi(async () => response(value)).listAssignments(buildQueryRequest(DEFAULT_CONTROLS)), { code: "response_invalid", outcome: "unknown" });
  }
});

test("save success followed by GET failure stays saved, blocks stale writes, and recovers on reload", async () => {
  let reads = 0, writes = 0, broken = true;
  const model = createAssignmentListModel({
    listAssignments: async () => { if (++reads > 1 && broken) throw { code: "network_failed" }; return list([assignment(reads > 1 ? "completed" : "incomplete")]); },
    setCompletion: async () => { writes++; return "completed"; },
  });
  await model.reload();
  assert.equal(await model.setCompletion("a_1", "completed"), false);
  assert.equal(model.snapshot().write.kind, "saved");
  assert.match(model.snapshot().error, /完了状態は保存しました/);
  assert.equal(model.snapshot().needsRefresh, true);
  assert.equal(await model.setCompletion("a_1", "incomplete"), false);
  assert.equal(writes, 1);
  broken = false;
  assert.equal(await model.reload(), true);
  assert.equal(model.snapshot().needsRefresh, false);
  assert.equal(model.snapshot().response.items[0].completion, "completed");
});

test("pending writes block duplicate input and refresh using the latest filters and sort", async () => {
  const pending = deferred();
  const queries = [];
  let writes = 0;
  const model = createAssignmentListModel({
    listAssignments: async q => { queries.push(q); return list(); },
    setCompletion: () => { writes++; return pending.promise; },
  });
  await model.reload();
  const saving = model.setCompletion("a_1", "completed");
  assert.equal(model.snapshot().pendingId, "a_1");
  assert.equal(await model.setCompletion("a_1", "incomplete"), false);
  const changed = { ...DEFAULT_CONTROLS, completion: "completed", sort: "title", course: "unassigned", deadline: "today" };
  const reloading = model.reload(changed);
  pending.resolve("completed");
  await Promise.all([saving, reloading]);
  assert.equal(writes, 1);
  assert.deepEqual(queries.at(-1), buildQueryRequest(changed));
  assert.deepEqual(model.snapshot().controls, changed);
  assert.equal(model.snapshot().loading, false);
  assert.equal(model.snapshot().pendingId, null);
});

test("superseded GET responses and their failures cannot overwrite the latest selection", async () => {
  for (const failOld of [false, true]) {
    const old = deferred();
    const started = deferred();
    let reads = 0;
    const changes = [];
    const model = createAssignmentListModel({ listAssignments: () => {
      if (++reads === 1) { started.resolve(); return old.promise; }
      return Promise.resolve(list([]));
    } }, s => changes.push(s));
    const initial = model.reload();
    await started.promise;
    const second = model.reload({ ...DEFAULT_CONTROLS, completion: "completed" });
    const latest = model.reload({ ...DEFAULT_CONTROLS, completion: "all", sort: "title" });
    if (failOld) old.reject({ code: "storage" }); else old.resolve(list());
    await Promise.all([initial, second, latest]);
    assert.equal(reads, 2);
    assert.deepEqual(model.snapshot().response.items, []);
    assert.equal(model.snapshot().controls.completion, "all");
    assert.equal(model.snapshot().error, null);
    assert.ok(changes.every(s => s.response === null || s.response.items.length === 0));
  }
});

test("filter change during post-commit GET discards the old snapshot and runs a fresh query", async () => {
  const refresh = deferred();
  const started = deferred();
  let reads = 0;
  const model = createAssignmentListModel({
    listAssignments: () => { reads++; if (reads === 2) { started.resolve(); return refresh.promise; } return Promise.resolve(list(reads > 2 ? [] : [assignment()])); },
    setCompletion: async () => "completed",
  });
  await model.reload();
  const saving = model.setCompletion("a_1", "completed");
  await started.promise;
  const changed = model.reload({ ...DEFAULT_CONTROLS, completion: "completed" });
  refresh.resolve(list([assignment("completed")]));
  await Promise.all([saving, changed]);
  assert.equal(reads, 3);
  assert.deepEqual(model.snapshot().response.items, []);
  assert.equal(model.snapshot().needsRefresh, false);
});

test("lost PUT response verifies an unfiltered snapshot and never sends a reverse or duplicate write", async () => {
  for (const actual of ["completed", "incomplete"]) {
    const queries = [];
    let writes = 0;
    const model = createAssignmentListModel({
      listAssignments: async q => { queries.push(q); return list([assignment(queries.length === 1 ? "incomplete" : actual)]); },
      setCompletion: async () => { writes++; throw { code: "network_failed", outcome: "unknown" }; },
    });
    await model.reload();
    await model.setCompletion("a_1", "completed");
    assert.equal(writes, 1);
    assert.equal(queries[1].completion, "all");
    assert.deepEqual(queries[1].course, { kind: "all" });
    assert.equal(queries[1].deadline, "all");
    assert.equal(model.snapshot().write.kind, "confirmed");
    assert.equal(model.snapshot().write.completion, actual);
    assert.match(model.snapshot().message, /実際の完了状態を確認/);
    assert.equal(model.snapshot().needsRefresh, false);
  }
});

test("unconfirmed save stays blocked across failed reloads, then reconciles before enabling writes", async () => {
  let reads = 0, broken = true, writes = 0;
  const model = createAssignmentListModel({
    listAssignments: async () => { if (++reads > 1 && broken) throw { code: "network_failed" }; return list([assignment(reads > 1 ? "completed" : "incomplete")]); },
    setCompletion: async () => { writes++; throw { code: "response_invalid", outcome: "unknown" }; },
  });
  await model.reload();
  await model.setCompletion("a_1", "completed");
  assert.match(model.snapshot().error, /保存結果が不明.*保存されている可能性/);
  assert.equal(model.snapshot().pendingId, null);
  await model.reload();
  assert.equal(await model.setCompletion("a_1", "incomplete"), false);
  assert.equal(model.snapshot().needsRefresh, true);
  broken = false;
  await model.reload();
  assert.equal(model.snapshot().write.kind, "confirmed");
  assert.equal(model.snapshot().needsRefresh, false);
  assert.equal(writes, 1);
});

test("server rejection is a fixed error and does not claim an unknown save or retry the mutation", async () => {
  const model = createAssignmentListModel({ listAssignments: async () => list(),
    setCompletion: async () => { throw { code: "not_found", outcome: "rejected", message: "PRIVATE" }; } });
  await model.reload();
  await model.setCompletion("a_1", "completed");
  assert.equal(model.snapshot().write, null);
  assert.match(model.snapshot().message, /拒否/);
  assert.doesNotMatch(model.snapshot().error, /PRIVATE/);
  assert.equal(model.snapshot().needsRefresh, true);
});

test("web DateOnly and DateTime JST display do not change with the browser OS timezone", () => {
  const moduleUrl = new URL("../frontend/presentation.js", import.meta.url).href;
  const script = `import { deadlineLabel } from ${JSON.stringify(moduleUrl)};
    console.log(JSON.stringify([
      deadlineLabel({kind: "date_only", value: "2026-10-22"}, "Asia/Tokyo"),
      deadlineLabel({kind: "date_time", value: "2026-10-21T15:00:00.123Z"}, "Asia/Tokyo"),
    ]));`;
  const labels = ["Asia/Tokyo", "America/Los_Angeles", "UTC"].map(TZ => JSON.parse(execFileSync(process.execPath,
    ["--input-type=module", "-e", script], { env: { ...process.env, TZ }, encoding: "utf8" })));
  assert.deepEqual(labels[0], labels[1]);
  assert.deepEqual(labels[0], labels[2]);
  assert.equal(labels[0][0], "締切: 2026/10/22（日付のみ）");
  assert.match(labels[0][1], /2026\/10\/22 00:00 JST/);
});

test("desktop adapter preserves invoke command names/arguments and never falls back to HTTP", async () => {
  const calls = [];
  const api = createTauriApi(async (command, args) => { calls.push({ command, args }); return command === "list_assignments" ? list() : "completed"; });
  const request = buildQueryRequest(DEFAULT_CONTROLS);
  await api.listAssignments(request);
  assert.equal(await api.setCompletion("a_1", "completed"), "completed");
  assert.deepEqual(calls, [{ command: "list_assignments", args: { request } }, { command: "toggle_assignment_completion", args: { assignmentId: "a_1" } }]);
  await assert.rejects(createTauriApi(async () => { throw { code: "storage", message: "PRIVATE" }; }).setCompletion("a_1", "completed"), { code: "storage", outcome: "rejected" });
  const source = readFileSync(new URL("../frontend/tauri_api.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /\bfetch\s*\(/);
  // Resolve the *entire* browser module graph, not just the entrypoint text.
  const seen = new Set();
  function visit(file) {
    if (seen.has(file)) return;
    seen.add(file);
    const source = readFileSync(new URL(`../frontend/${file}`, import.meta.url), "utf8");
    assert.doesNotMatch(source, /__TAURI__|invokeTauri|tauri_api|settings_view|sync_view/);
    for (const match of source.matchAll(/from "\.\/(.*?)"/g)) visit(match[1]);
  }
  visit("web_app.js");
  const web = readFileSync(new URL("../frontend/web.html", import.meta.url), "utf8");
  assert.doesNotMatch(web, /id="(?:show-settings|sync-now|settings-view)"/);
  assert.match(web, /合成データを使ったローカルデモ/);
});

// Minimal DOM surface used by the view; any HTML assignment fails the test.
function documentFixture() {
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
  }
  const ids = Object.fromEntries(["assignments", "empty", "status", "count", "error", "reload", "reset-filters", "filters",
    "completion-filter", "deadline-filter", "course-filter", "sort", "empty-title", "empty-description"].map(id => [id, new Element("div")]));
  for (const [key, value] of Object.entries(DEFAULT_CONTROLS)) ids[key === "sort" ? "sort" : `${key}-filter`].value = value;
  const doc = { querySelector: selector => ids[selector.slice(1)], createElement: tag => new Element(tag), activeElement: null };
  return { doc, ids, elements };
}

test("shared view renders malicious titles as literal text and sends checkbox desired state", async () => {
  const { doc, ids, elements } = documentFixture();
  const title = '<img src=x onerror="globalThis.pwned=true">日本語';
  const calls = [];
  const view = createListView({ listAssignments: async () => list([{ ...assignment(), title }]),
    setCompletion: async (id, desired) => { calls.push({ id, desired }); return desired; } }, doc);
  await view.load();
  assert.equal(elements.find(e => e.tagName === "h3").textContent, title);
  assert.equal(elements.filter(e => e.tagName === "img").length, 0);
  assert.equal(ids.count.textContent, "1 / 1件の課題");
  assert.equal(ids.error.hidden, true);
  const checkbox = ids.assignments.querySelectorAll("input")[0];
  checkbox.checked = true;
  checkbox.listeners.change();
  assert.equal(checkbox.checked, false); // Keep committed state until confirmed by GET.
  assert.equal(checkbox.disabled, true);
  checkbox.listeners.change(); // Even synthetic duplicate input cannot enqueue another write.
  await view.model.reload();
  assert.deepEqual(calls, [{ id: "a_1", desired: "completed" }]);
});
