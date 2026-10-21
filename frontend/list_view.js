import {
  DEFAULT_CONTROLS, courseOptionLabel, deadlineLabel, emptyState,
  groupAssignments, originLabel, statusPresentation, submissionLabel,
} from "./presentation.js";
import { createAssignmentListModel } from "./list_tasks.js";

export function createListView(api, document_ = document) {
  const document = document_;
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
  let currentState;
  const model = createAssignmentListModel(api, update);

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
    checkbox.disabled = currentState.loading || currentState.pendingId !== null || currentState.needsRefresh;
    checkbox.dataset.assignmentId = assignment.id;
    checkbox.setAttribute("aria-label", `${assignment.title} の完了状態`);
    checkbox.addEventListener("change", () => {
      const desired = checkbox.checked ? "completed" : "incomplete";
      checkbox.checked = completed;
      void model.setCompletion(assignment.id, desired);
    });
    label.append(checkbox, textElement("span", "completion-text", completed ? "完了済み" : "未完了"));
    const content = document.createElement("div");
    content.className = "assignment-content";
    const titleLine = document.createElement("div");
    titleLine.className = "title-line";
    titleLine.append(textElement("h3", "assignment-title", assignment.title));
    if (badge) titleLine.append(textElement("span", `status-badge ${badge.className}`, badge.label));
    const submission = submissionLabel(assignment.submission)
      + (assignment.hasSubmissionUrl ? " · 提出URLあり" : "");
    content.append(titleLine, textElement("p", "deadline", deadlineLabel(assignment.deadline, currentState.response?.timeZone)), textElement("p", "submission", submission));
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
  }

  let renderedResponse = null;
  function update(state) {
    const focus = rememberFocus();
    currentState = state;
    writeControls(state.controls);
    if (state.response && state.response !== renderedResponse) {
      render(state.response, state.controls);
      renderedResponse = state.response;
    }
    status.textContent = state.message;
    errorBanner.hidden = state.error === null;
    errorBanner.textContent = state.error ?? "";
    list.setAttribute("aria-busy", String(state.loading || state.pendingId !== null));
    reload.disabled = false; // Also queues a fresh read while a write is in flight.
    for (const input of list.querySelectorAll("input")) {
      input.disabled = state.loading || state.pendingId !== null || state.needsRefresh;
    }
    restoreFocus(focus);
  }

  const load = () => model.reload(readControls());
  form.addEventListener("submit", event => event.preventDefault());
  for (const input of Object.values(controls)) input.addEventListener("change", load);
  reload.addEventListener("click", load);
  reset.addEventListener("click", () => {
    writeControls({ ...DEFAULT_CONTROLS, completion: "all" });
    void load();
  });
  update(model.snapshot());
  return { load, model };
}
