import {
  courseLabel, deadlineLabel, originLabel, safeErrorMessage, submissionLabel,
} from "./presentation.js";

const list = document.querySelector("#assignments");
const empty = document.querySelector("#empty");
const status = document.querySelector("#status");
const errorBanner = document.querySelector("#error");
const reload = document.querySelector("#reload");
let assignments = [];
let busy = false;

function invoke(command, arguments_) {
  return window.__TAURI__.core.invoke(command, arguments_);
}

function textElement(tag, className, text) {
  const element = document.createElement(tag);
  element.className = className;
  // Titles and source labels are untrusted display text, never HTML.
  element.textContent = text;
  return element;
}

function setBusy(value) {
  busy = value;
  reload.disabled = value;
  list.setAttribute("aria-busy", String(value));
  for (const input of list.querySelectorAll("input")) input.disabled = value;
}

function showError(error) {
  errorBanner.textContent = safeErrorMessage(error);
  errorBanner.hidden = false;
}

function render() {
  const rows = assignments.map((assignment) => {
    const completed = assignment.completion === "completed";
    const row = document.createElement("li");
    row.className = `assignment${completed ? " completed" : ""}`;
    const label = document.createElement("label");
    label.className = "completion-control";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = completed;
    checkbox.disabled = busy;
    checkbox.setAttribute("aria-label", `${assignment.title} の完了状態`);
    checkbox.addEventListener("change", () => toggle(assignment, checkbox));
    label.append(checkbox, textElement("span", "completion-text", completed ? "完了" : "未完了"));

    const content = document.createElement("div");
    content.className = "assignment-content";
    const course = document.createElement("div");
    course.className = "course-line";
    course.append(textElement("span", "course-label", courseLabel(assignment.course)));
    const origin = originLabel(assignment.course);
    if (origin) course.append(textElement("span", "origin", origin));
    const submission = submissionLabel(assignment.submission)
      + (assignment.hasSubmissionUrl ? " · 提出URLあり" : "");
    content.append(
      textElement("h2", "assignment-title", assignment.title),
      textElement("p", "deadline", deadlineLabel(assignment.deadline)),
      course,
      textElement("p", "submission", submission),
    );
    row.append(label, content);
    return row;
  });
  // Flat list preserves each Assignment/Course ID. No name-based merging.
  list.replaceChildren(...rows);
  empty.hidden = assignments.length !== 0;
  status.textContent = assignments.length ? `${assignments.length}件の課題` : "";
}

async function load() {
  if (busy) return;
  setBusy(true);
  errorBanner.hidden = true;
  empty.hidden = true;
  status.textContent = "課題を読み込んでいます…";
  try {
    assignments = await invoke("list_assignments");
    render();
  } catch (error) {
    showError(error);
    status.textContent = assignments.length ? `${assignments.length}件の課題（前回の表示）` : "";
  } finally {
    setBusy(false);
  }
}

async function toggle(assignment, checkbox) {
  // Native checkbox changes on click. Restore it until the DB confirms commit.
  checkbox.checked = assignment.completion === "completed";
  if (busy) return;
  setBusy(true);
  errorBanner.hidden = true;
  status.textContent = "完了状態を保存しています…";
  try {
    const completion = await invoke("toggle_assignment_completion", {
      assignmentId: assignment.id,
    });
    assignment.completion = completion;
    // Use the committed state directly; no second query can turn a successful
    // toggle into an ambiguous retry that accidentally flips it again.
    render();
    status.textContent = completion === "completed" ? "完了として保存しました。" : "未完了として保存しました。";
  } catch (error) {
    showError(error);
    status.textContent = "完了状態を変更できませんでした。";
  } finally {
    setBusy(false);
  }
}

reload.addEventListener("click", load);
void load();
