import {
  DEFAULT_CONTROLS, buildQueryRequest, courseOptionLabel, deadlineLabel, emptyState,
  groupAssignments, operationErrorMessage, originLabel, statusPresentation, submissionLabel,
} from "./presentation.js";
import { createListTaskRunner } from "./list_tasks.js";
import { initializeSync } from "./sync.js";
import { createSyncView } from "./sync_view.js";
import { settingsModel } from "./settings_view.js";

const list = document.querySelector("#assignments");
const empty = document.querySelector("#empty");
const status = document.querySelector("#status");
const count = document.querySelector("#count");
const errorBanner = document.querySelector("#error");
const reload = document.querySelector("#reload");
const reset = document.querySelector("#reset-filters");
const form = document.querySelector("#filters");
const controls = {
  completion: document.querySelector("#completion-filter"),
  deadline: document.querySelector("#deadline-filter"),
  course: document.querySelector("#course-filter"),
  sort: document.querySelector("#sort"),
};
let appliedControls = { ...DEFAULT_CONTROLS };
let busy = false;
let hasSnapshot = false;
let needsRefresh = false;
const runListTask = createListTaskRunner();

function invoke(command, arguments_) {
  return window.__TAURI__.core.invoke(command, arguments_);
}

function readControls() {
  return Object.fromEntries(Object.entries(controls).map(([name, input]) => [name, input.value]));
}

function writeControls(values) {
  for (const [name, input] of Object.entries(controls)) input.value = values[name];
}

function textElement(tag, className, text) {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = text;
  return element;
}

function setBusy(value) {
  busy = value;
  reload.disabled = value;
  reset.disabled = value;
  for (const input of Object.values(controls)) input.disabled = value;
  list.setAttribute("aria-busy", String(value));
  for (const input of list.querySelectorAll("input")) input.disabled = value || needsRefresh;
}

function rememberFocus() {
  const element = document.activeElement;
  const checkboxes = [...list.querySelectorAll("input")];
  return { element, assignmentId: element?.dataset?.assignmentId, index: checkboxes.indexOf(element) };
}

function restoreFocus(focus) {
  if (focus.element === reset && empty.hidden) {
    controls.completion.focus();
  } else if (focus.element?.isConnected && !focus.element.disabled && !focus.element.hidden) {
    focus.element.focus();
  } else if (focus.assignmentId) {
    const checkboxes = [...list.querySelectorAll("input")].filter(input => !input.disabled);
    const next = checkboxes.find(input => input.dataset.assignmentId === focus.assignmentId)
      ?? checkboxes[Math.min(focus.index, checkboxes.length - 1)] ?? controls.completion;
    next.focus();
  } else if (focus.element === reset) {
    controls.completion.focus();
  }
}

function showError(operation, error) {
  errorBanner.textContent = operationErrorMessage(operation, error);
  errorBanner.hidden = false;
}

function updateCourseOptions(courses, selected) {
  const option = (value, text) => {
    const element = textElement("option", "", text);
    element.value = value;
    return element;
  };
  controls.course.replaceChildren(
    option("all", "すべて"), option("unassigned", "未所属"),
    ...courses.map(course => option(`course:${course.id}`, courseOptionLabel(course))),
  );
  if (![...controls.course.options].some(option => option.value === selected)) {
    controls.course.append(option(selected, "選択中のCourse（課題なし）"));
  }
  controls.course.value = selected;
}

function assignmentRow(assignment) {
  const completed = assignment.completion === "completed";
  const badge = statusPresentation(assignment.status, assignment.completion);
  const row = document.createElement("li");
  row.className = `assignment${completed ? " completed" : ""}${badge?.className === "badge-overdue" ? " overdue" : ""}`;
  const label = document.createElement("label");
  label.className = "completion-control";
  const checkbox = document.createElement("input");
  checkbox.type = "checkbox";
  checkbox.checked = completed;
  checkbox.disabled = busy || needsRefresh;
  checkbox.dataset.assignmentId = assignment.id;
  checkbox.setAttribute("aria-label", `${assignment.title} の完了状態`);
  checkbox.addEventListener("change", () => toggle(assignment, checkbox));
  label.append(checkbox, textElement("span", "completion-text", completed ? "完了済み" : "未完了"));
  const content = document.createElement("div");
  content.className = "assignment-content";
  const titleLine = document.createElement("div");
  titleLine.className = "title-line";
  titleLine.append(textElement("h3", "assignment-title", assignment.title));
  if (badge) titleLine.append(textElement("span", `status-badge ${badge.className}`, badge.label));
  const submission = submissionLabel(assignment.submission)
    + (assignment.hasSubmissionUrl ? " · 提出URLあり" : "");
  content.append(titleLine, textElement("p", "deadline", deadlineLabel(assignment.deadline)), textElement("p", "submission", submission));
  row.append(label, content);
  return row;
}

function render(response, selected) {
  const groups = groupAssignments(response.items, response.courses);
  const sections = groups.map((group, index) => {
    const section = document.createElement("section");
    section.className = "course-group";
    const heading = document.createElement("div");
    heading.className = "group-header";
    const title = textElement("h2", "group-title", group.course?.displayLabel ?? "未所属");
    title.id = `course-group-${index}`;
    section.setAttribute("aria-labelledby", title.id);
    heading.append(title);
    if (group.course) {
      heading.append(textElement("span", "origin", originLabel(group.course)));
      if (group.course.origin === "moodle") heading.append(textElement("span", "label-note", "表示ラベル"));
    }
    heading.append(textElement("span", "group-count", `${group.items.length}件`));
    const rows = document.createElement("ul");
    rows.className = "assignment-list";
    rows.append(...group.items.map(assignmentRow));
    section.append(heading, rows);
    return section;
  });
  list.replaceChildren(...sections);
  updateCourseOptions(response.courses, selected.course);
  const emptyMessage = emptyState(response);
  empty.hidden = emptyMessage === null;
  if (emptyMessage) {
    document.querySelector("#empty-title").textContent = emptyMessage.title;
    document.querySelector("#empty-description").textContent = emptyMessage.description;
    reset.hidden = !emptyMessage.filtered;
  }
  count.textContent = `${response.items.length} / ${response.totalCount}件の課題`;
  hasSnapshot = true;
  needsRefresh = false;
  appliedControls = { ...selected };
}

async function query(selected) {
  return invoke("list_assignments", { request: buildQueryRequest(selected) });
}

function load() { return runListTask(loadNow); }

async function loadNow() {
  const focus = rememberFocus();
  const selected = readControls();
  setBusy(true);
  errorBanner.hidden = true;
  status.textContent = "一覧を更新しています…";
  try {
    render(await query(selected), selected);
    status.textContent = "";
  } catch (error) {
    writeControls(appliedControls);
    showError("query", error);
    status.textContent = hasSnapshot ? "前回取得した一覧を表示しています。" : "";
  } finally {
    setBusy(false);
    restoreFocus(focus);
  }
}

function toggle(assignment, checkbox) {
  checkbox.checked = assignment.completion === "completed";
  if (busy || needsRefresh) return;
  return runListTask(() => toggleNow(assignment));
}

async function toggleNow(assignment) {
  const focus = rememberFocus();
  setBusy(true);
  errorBanner.hidden = true;
  status.textContent = "完了状態を保存しています…";
  let committed = false;
  try {
    await invoke("toggle_assignment_completion", { assignmentId: assignment.id });
    committed = true;
    render(await query(appliedControls), appliedControls);
    status.textContent = "完了状態を保存し、一覧を更新しました。";
  } catch (error) {
    if (committed) {
      // Prevent a second click on a stale row from flipping the saved state again.
      needsRefresh = true;
      showError("refresh_after_toggle", error);
      status.textContent = "一覧の再読み込みが必要です。";
    } else {
      showError("toggle", error);
      status.textContent = "完了状態は変更されていません。";
    }
  } finally {
    setBusy(false);
    restoreFocus(focus);
  }
}

form.addEventListener("submit", event => event.preventDefault());
for (const input of Object.values(controls)) input.addEventListener("change", load);
reload.addEventListener("click", load);
reset.addEventListener("click", () => {
  writeControls({ ...DEFAULT_CONTROLS, completion: "all" });
  void load();
});
const syncModel = createSyncView(invoke, load);
void initializeSync(load, async () => {
  await settingsModel.load();
  return settingsModel.snapshot();
}, syncModel);
