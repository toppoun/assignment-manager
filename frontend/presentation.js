const dateTimeFormat = new Intl.DateTimeFormat(undefined, {
  year: "numeric", month: "2-digit", day: "2-digit",
  hour: "2-digit", minute: "2-digit", second: "2-digit",
  timeZoneName: "short",
});

export function deadlineLabel(deadline) {
  switch (deadline.kind) {
    case "date_only":
      // Keep a calendar date as a calendar date; never parse it as UTC midnight.
      return `締切: ${deadline.value}（日付のみ）`;
    case "date_time":
      return `締切: ${dateTimeFormat.format(new Date(deadline.value))}（日時）`;
    case "none":
      return "締切なし";
    default:
      return "締切を表示できません";
  }
}

export function courseLabel(course) {
  if (!course) return "Course未設定";
  const prefix = course.origin === "moodle" ? "Course表示ラベル" : "Course";
  return `${prefix}: ${course.displayLabel}`;
}

export function originLabel(course) {
  if (!course) return null;
  return {
    moodle: "Moodle",
    web_class: "WebClass",
    manual: "手動",
    unbound: "手動 / 連携なし",
  }[course.origin] ?? "連携なし";
}

export function submissionLabel(submission) {
  return {
    unknown: "提出先: 未確認",
    none: "提出不要",
    moodle: "提出先: Moodle",
    web_class: "提出先: WebClass",
    external: "提出先: 外部",
  }[submission] ?? "提出先: 未確認";
}

// Never display an arbitrary rejection or its message, even for IPC/framework errors.
export function safeErrorMessage(error) {
  const messages = {
    data_directory: "アプリの保存先を準備できませんでした。",
    database_startup: "保存データを開けませんでした。アプリを再起動してください。",
    storage: "保存データを読み書きできませんでした。",
    not_found: "課題が見つかりません。一覧を再読み込みしてください。",
    invalid_assignment_id: "課題を指定できませんでした。",
    busy: "処理が混み合っています。少し待って再試行してください。",
    unavailable: "アプリの処理を実行できませんでした。再起動してください。",
  };
  return Object.hasOwn(messages, error?.code)
    ? messages[error.code]
    : "処理を完了できませんでした。再試行してください。";
}
