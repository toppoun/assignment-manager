const dateTimeFormat = new Intl.DateTimeFormat(undefined, {
  year: "numeric", month: "2-digit", day: "2-digit",
  hour: "2-digit", minute: "2-digit", timeZoneName: "short",
});

export const DEFAULT_CONTROLS = Object.freeze({
  completion: "incomplete", deadline: "all", course: "all", sort: "deadline",
});

export function buildQueryRequest(controls) {
  if (!["all", "incomplete", "completed"].includes(controls.completion)
      || !["all", "overdue", "within_seven_days", "today"].includes(controls.deadline)
      || !["deadline", "course", "title"].includes(controls.sort)) {
    throw new Error("Invalid controls");
  }
  let course;
  if (controls.course === "all" || controls.course === "unassigned") {
    course = { kind: controls.course };
  } else if (controls.course.startsWith("course:") && controls.course.length > 7) {
    course = { kind: "course", id: controls.course.slice(7) };
  } else {
    throw new Error("Invalid course selection");
  }
  return { completion: controls.completion, course, deadline: controls.deadline, sort: controls.sort };
}

// Server group order comes from core Course sort. Retain core item order inside groups.
export function groupAssignments(items, courses) {
  const groups = new Map(courses.map(course => [course.id, { course, items: [] }]));
  const unassigned = { course: null, items: [] };
  for (const item of items) {
    if (item.course === null) {
      unassigned.items.push(item);
    } else {
      const group = groups.get(item.course.id);
      if (!group) throw new Error("Missing course metadata");
      group.items.push(item);
    }
  }
  const result = [...groups.values()].filter(group => group.items.length > 0);
  if (unassigned.items.length) result.push(unassigned);
  return result;
}

export function emptyState(response) {
  if (response.totalCount === 0) {
    return { title: "課題はまだありません", description: "保存されている課題は0件です。", filtered: false };
  }
  if (response.items.length === 0) {
    return { title: "条件に一致する課題はありません", description: "絞り込み条件を変更してみてください。", filtered: true };
  }
  return null;
}

export function deadlineLabel(deadline, timeZone) {
  switch (deadline.kind) {
    case "date_only":
      // Format only; never turn a calendar date into an instant or evaluate it.
      return `締切: ${deadline.value.replace(/^([+-]?\d+)-(\d{2})-(\d{2})$/, "$1/$2/$3")}（日付のみ）`;
    case "date_time":
      return `締切: ${(timeZone ? new Intl.DateTimeFormat("ja-JP", {
        year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit",
        timeZone, timeZoneName: "short", hourCycle: "h23",
      }) : dateTimeFormat).format(new Date(deadline.value))}（日時）`;
    case "none":
      return "期限なし";
    default:
      return "締切を表示できません";
  }
}

export function courseLabel(course) {
  if (!course) return "未所属";
  const prefix = course.origin === "moodle" ? "Course表示ラベル" : "Course";
  return `${prefix}: ${course.displayLabel}`;
}

export function originLabel(course) {
  if (!course) return null;
  return {
    moodle: "Moodle", web_class: "WebClass", manual: "手動", unbound: "手動 / 連携なし",
  }[course.origin] ?? "連携なし";
}

export function courseOptionLabel(course) {
  return `${course.displayLabel} · ${originLabel(course)}${course.origin === "moodle" ? "（表示ラベル）" : ""}`;
}

// No clock or deadline comparison: Rust supplies the semantic status.
export function statusPresentation(status, completion) {
  if (completion === "completed") return null;
  const labels = { overdue: "期限切れ", today: "今日", upcoming: "7日以内" };
  return Object.hasOwn(labels, status) ? { label: labels[status], className: `badge-${status}` } : null;
}

export function submissionLabel(submission) {
  return {
    unknown: "提出先: 未確認", none: "提出不要", moodle: "提出先: Moodle",
    web_class: "提出先: WebClass", external: "提出先: 外部",
  }[submission] ?? "提出先: 未確認";
}

export function safeErrorMessage(error) {
  const messages = {
    data_directory: "アプリの保存先を準備できませんでした。",
    database_startup: "保存データを開けませんでした。アプリを再起動してください。",
    storage: "保存データを読み書きできませんでした。",
    not_found: "課題が見つかりません。一覧を再読み込みしてください。",
    invalid_assignment_id: "課題を指定できませんでした。",
    invalid_query: "一覧の条件を指定できませんでした。",
    credential_unavailable: "OSの認証情報ストアを利用できませんでした。",
    credential_read_failed: "保存済みの認証情報を確認できませんでした。",
    credential_write_failed: "認証情報を保存できませんでした。",
    credential_delete_failed: "認証情報を削除できませんでした。",
    settings_read_failed: "設定ファイルを読み込めませんでした。ファイルは変更していません。",
    settings_write_failed: "設定ファイルを保存できませんでした。",
    invalid_moodle_feed: "Moodle URLはユーザー名・passwordを含まないHTTPS URLを入力してください。",
    invalid_credentials: "大学IDとpasswordの両方を入力してください。",
    busy: "処理が混み合っています。少し待って再試行してください。",
    unavailable: "アプリの処理を実行できませんでした。再起動してください。",
    network_failed: "通信できませんでした。serverの起動状態を確認してください。",
    response_invalid: "serverの応答を確認できませんでした。",
    cross_origin: "同一originからの要求だけを受け付けます。",
    internal_error: "保存データの処理を完了できませんでした。",
  };
  return Object.hasOwn(messages, error?.code)
    ? messages[error.code]
    : "処理を完了できませんでした。再試行してください。";
}

export function operationErrorMessage(operation, error) {
  const prefixes = {
    query: "一覧の取得に失敗しました。",
    toggle: "完了状態を変更できませんでした。",
    refresh_after_toggle: "完了状態は保存しましたが、一覧を更新できませんでした。再読み込みしてください。",
    completion_unknown: "保存結果が不明です。保存されている可能性があります。状態を確認するため再読み込みしてください。",
  };
  return `${prefixes[operation]} ${safeErrorMessage(error)}`;
}
