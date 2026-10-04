import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { test } from "node:test";
import {
  DEFAULT_CONTROLS, buildQueryRequest, deadlineLabel, courseLabel, courseOptionLabel,
  emptyState, groupAssignments, operationErrorMessage, originLabel, safeErrorMessage,
  statusPresentation, submissionLabel,
} from "../frontend/presentation.js";

test("DateOnly and absent deadlines remain visibly different", () => {
  assert.equal(deadlineLabel({ kind: "date_only", value: "2026-10-04" }), "締切: 2026/10/04（日付のみ）");
  assert.equal(deadlineLabel({ kind: "date_only", value: "-0001-10-04" }), "締切: -0001/10/04（日付のみ）");
  assert.equal(deadlineLabel({ kind: "none" }), "期限なし");
});

test("DateTime uses the OS local timezone, while DateOnly never shifts dates", () => {
  const moduleUrl = new URL("../frontend/presentation.js", import.meta.url).href;
  const script = `
    import { deadlineLabel } from ${JSON.stringify(moduleUrl)};
    const instant = "2026-10-04T01:00:00Z";
    const expected = new Intl.DateTimeFormat(undefined, {
      year: "numeric", month: "2-digit", day: "2-digit",
      hour: "2-digit", minute: "2-digit", timeZoneName: "short",
    }).format(new Date(instant));
    console.log(JSON.stringify({
      datetime: deadlineLabel({kind: "date_time", value: instant}),
      expected: "締切: " + expected + "（日時）",
      date: deadlineLabel({kind: "date_only", value: "2026-10-04"}),
    }));
  `;
  const labels = ["Asia/Tokyo", "America/Los_Angeles"].map((timezone) => {
    const result = JSON.parse(execFileSync(process.execPath, ["--input-type=module", "-e", script], {
      env: { ...process.env, TZ: timezone }, encoding: "utf8",
    }));
    assert.equal(result.datetime, result.expected);
    assert.equal(result.date, "締切: 2026/10/04（日付のみ）");
    return result.datetime;
  });
  assert.notEqual(labels[0], labels[1]);
});

test("Course display labels preserve origin and describe Moodle as a display label", () => {
  const label = "同じ科目ラベル";
  assert.equal(courseLabel({ displayLabel: label, origin: "moodle" }), `Course表示ラベル: ${label}`);
  assert.equal(courseLabel({ displayLabel: label, origin: "web_class" }), `Course: ${label}`);
  assert.equal(originLabel({ origin: "moodle" }), "Moodle");
  assert.equal(originLabel({ origin: "web_class" }), "WebClass");
  assert.equal(originLabel({ origin: "manual" }), "手動");
  assert.equal(originLabel({ origin: "unbound" }), "手動 / 連携なし");
  assert.equal(courseLabel(null), "未所属");
});

test("Grouping keeps same-label and same-origin courses separate by internal ID", () => {
  const courses = [
    { id: "c1", displayLabel: "同じ名前", origin: "moodle" },
    { id: "c2", displayLabel: "同じ名前", origin: "web_class" },
    { id: "c3", displayLabel: "同じ名前", origin: "moodle" },
    { id: "unused", displayLabel: "未表示", origin: "manual" },
  ];
  const items = [
    { id: "unassigned-1", course: null },
    { id: "c2-first", course: courses[1] },
    { id: "c1-first", course: courses[0] },
    { id: "c1-second", course: courses[0] },
    { id: "c3-first", course: courses[2] },
    { id: "unassigned-2", course: null },
  ];
  const result = groupAssignments(items, courses);
  assert.deepEqual(result.map(group => group.course?.id ?? null), ["c1", "c2", "c3", null]);
  assert.deepEqual(result[0].items.map(item => item.id), ["c1-first", "c1-second"]);
  assert.deepEqual(result[3].items.map(item => item.id), ["unassigned-1", "unassigned-2"]);
  assert.notEqual(courseOptionLabel(courses[0]), courseOptionLabel(courses[1]));
  assert.match(courseOptionLabel(courses[0]), /表示ラベル/);
});

test("Grouping follows supplied core group order without locale or assignment sorting", () => {
  const courses = [{ id: "z", displayLabel: "Zulu" }, { id: "a", displayLabel: "Alpha" }];
  const items = [{ id: "a1", course: courses[1] }, { id: "z2", course: courses[0] }, { id: "z1", course: courses[0] }];
  const result = groupAssignments(items, courses);
  assert.deepEqual(result.map(group => group.course.id), ["z", "a"]);
  assert.deepEqual(result[0].items.map(item => item.id), ["z2", "z1"]);
  assert.deepEqual(groupAssignments([], courses), []);
});

test("Filter requests contain only typed selection and exact internal CourseId", () => {
  assert.deepEqual(buildQueryRequest(DEFAULT_CONTROLS), {
    completion: "incomplete", course: { kind: "all" }, deadline: "all", sort: "deadline",
  });
  for (const completion of ["all", "incomplete", "completed"]) {
    for (const deadline of ["all", "overdue", "within_seven_days", "today"]) {
      const request = buildQueryRequest({ ...DEFAULT_CONTROLS, completion, deadline, course: "course: ID 01 " });
      assert.deepEqual(request, { completion, course: { kind: "course", id: " ID 01 " }, deadline, sort: "deadline" });
      assert.deepEqual(Object.keys(request).sort(), ["completion", "course", "deadline", "sort"]);
    }
  }
  assert.deepEqual(buildQueryRequest({ ...DEFAULT_CONTROLS, course: "unassigned" }).course, { kind: "unassigned" });
  assert.deepEqual(buildQueryRequest({ ...DEFAULT_CONTROLS, course: "course:all" }).course, { kind: "course", id: "all" });
  assert.throws(() => buildQueryRequest({ ...DEFAULT_CONTROLS, course: "科目名" }));
  assert.throws(() => buildQueryRequest({ ...DEFAULT_CONTROLS, deadline: "SQL" }));
});

test("All sort choices go to Rust unchanged with no invented direction", () => {
  for (const sort of ["deadline", "course", "title"]) {
    const request = buildQueryRequest({ ...DEFAULT_CONTROLS, sort });
    assert.equal(request.sort, sort);
    assert.equal(Object.hasOwn(request, "direction"), false);
  }
  assert.throws(() => buildQueryRequest({ ...DEFAULT_CONTROLS, sort: "descending" }));
});

test("Empty DB and empty filter results have different states", () => {
  assert.equal(emptyState({ totalCount: 0, items: [] }).title, "課題はまだありません");
  assert.equal(emptyState({ totalCount: 3, items: [] }).title, "条件に一致する課題はありません");
  assert.equal(emptyState({ totalCount: 3, items: [] }).filtered, true);
  assert.equal(emptyState({ totalCount: 0, items: [] }).filtered, false);
  assert.equal(emptyState({ totalCount: 3, items: [{}] }), null);
});

test("Status is presentation of server tags and Completed never highlights overdue", () => {
  for (const [status, label] of [["overdue", "期限切れ"], ["today", "今日"], ["upcoming", "7日以内"]]) {
    assert.deepEqual(statusPresentation(status, "incomplete"), { label, className: `badge-${status}` });
    assert.equal(statusPresentation(status, "completed"), null);
  }
  assert.equal(statusPresentation("none", "incomplete"), null);
  assert.equal(statusPresentation("__proto__", "incomplete"), null);
});

test("Query, toggle and post-commit refresh failures have distinct safe messages", () => {
  const unsafe = { code: "storage", message: "SELECT private/path token=secret" };
  const messages = ["query", "toggle", "refresh_after_toggle"].map(operation => operationErrorMessage(operation, unsafe));
  assert.equal(new Set(messages).size, 3);
  for (const message of messages) assert.doesNotMatch(message, /SELECT|private|token|secret/);
  assert.match(messages[2], /完了状態は保存しました/);
});

test("All five submission states have different labels", () => {
  const labels = ["unknown", "none", "moodle", "web_class", "external"].map(submissionLabel);
  assert.equal(new Set(labels).size, 5);
});

test("Rejected IPC messages and unknown error codes cannot leak arbitrary causes", () => {
  const secret = "private/path SELECT scope external-id https://example.invalid?token=secret";
  for (const error of [undefined, null, secret, { message: secret }, { code: "__proto__", message: secret }, { code: "toString", message: secret }]) {
    assert.equal(safeErrorMessage(error), "処理を完了できませんでした。再試行してください。");
  }
  assert.equal(safeErrorMessage({ code: "storage", message: secret }), "保存データを読み書きできませんでした。");
});
