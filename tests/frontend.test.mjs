import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { test } from "node:test";
import {
  deadlineLabel, courseLabel, originLabel, safeErrorMessage, submissionLabel,
} from "../frontend/presentation.js";

test("DateOnly and absent deadlines remain visibly different", () => {
  assert.equal(deadlineLabel({ kind: "date_only", value: "2026-10-04" }), "締切: 2026-10-04（日付のみ）");
  assert.equal(deadlineLabel({ kind: "none" }), "締切なし");
});

test("DateTime uses the OS local timezone, while DateOnly never shifts dates", () => {
  const moduleUrl = new URL("../frontend/presentation.js", import.meta.url).href;
  const script = `
    import { deadlineLabel } from ${JSON.stringify(moduleUrl)};
    const instant = "2026-10-04T01:00:00Z";
    const expected = new Intl.DateTimeFormat(undefined, {
      year: "numeric", month: "2-digit", day: "2-digit",
      hour: "2-digit", minute: "2-digit", second: "2-digit", timeZoneName: "short",
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
    assert.equal(result.date, "締切: 2026-10-04（日付のみ）");
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
  assert.equal(courseLabel(null), "Course未設定");
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
