use std::ptr::null;

use assignment_manager::webclass::WebClassCourse;
use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

use crate::cli::windows_dialog::{
    add_item, control_index, create_controls, open_dialog, send, set_text, visible_text,
};

const COURSES: i32 = 100;
const DETAILS: i32 = 101;
const WINDOW_FAILED: isize = -2;

struct Session<'a> {
    courses: &'a [WebClassCourse],
    instance: HINSTANCE,
}

pub(super) fn show(courses: &[WebClassCourse]) -> Result<(), ()> {
    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(());
    }
    let mut session = Session { courses, instance };
    match open_dialog(
        instance,
        "WebClass 授業一覧",
        Some(dialog_proc),
        &mut session,
    ) {
        0 => Ok(()),
        _ => Err(()),
    }
}

unsafe extern "system" fn dialog_proc(
    window: HWND,
    event: u32,
    command: usize,
    parameter: isize,
) -> isize {
    match event {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, parameter) };
            let session = unsafe { &*(parameter as *const Session<'_>) };
            if !initialize(window, session) {
                unsafe { EndDialog(window, WINDOW_FAILED) };
            }
        }
        WM_CLOSE => {
            unsafe { EndDialog(window, 0) };
        }
        WM_COMMAND => {
            let id = (command & 0xffff) as i32;
            let notification = (command >> 16) as u32;
            match (id, notification) {
                (IDOK | IDCANCEL, 0) => {
                    unsafe { EndDialog(window, 0) };
                }
                (COURSES, LBN_SELCHANGE) => {
                    let address = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) };
                    if let Some(session) = unsafe { (address as *const Session<'_>).as_ref() } {
                        show_selected(window, session.courses);
                    }
                }
                _ => return 0,
            }
        }
        _ => return 0,
    }
    1
}

fn initialize(window: HWND, session: &Session<'_>) -> bool {
    let controls = [
        (
            0,
            w!("STATIC"),
            "WebClassが返した授業を表示します。年度・学期・曜日・時限は取得値です。",
            0,
            [8, 8, 373, 12],
        ),
        (
            COURSES,
            w!("LISTBOX"),
            "",
            WS_BORDER | WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32,
            [8, 26, 155, 192],
        ),
        (
            DETAILS,
            w!("EDIT"),
            "",
            WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | (ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32,
            [170, 26, 212, 187],
        ),
        (
            IDOK,
            w!("BUTTON"),
            "閉じる",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            [306, 224, 76, 16],
        ),
    ];
    if !create_controls(window, session.instance, &controls) {
        return false;
    }
    for course in session.courses {
        let preview: String = visible_text(&course.group_name).chars().take(50).collect();
        if !add_item(window, COURSES, LB_ADDSTRING, &preview) {
            return false;
        }
    }
    send(window, COURSES, LB_SETCURSEL, 0, 0);
    show_selected(window, session.courses);
    true
}

fn show_selected(window: HWND, courses: &[WebClassCourse]) {
    if let Some(course) =
        control_index(window, COURSES, LB_GETCURSEL).and_then(|index| courses.get(index))
    {
        set_text(window, DETAILS, &details(course));
    }
}

fn source_value(value: &str) -> String {
    match value.trim() {
        "" => "（空欄・意味未確認）".into(),
        "0" => format!("{}（意味未確認）", visible_text(value)),
        _ => visible_text(value),
    }
}

fn details(course: &WebClassCourse) -> String {
    let english = course
        .group_name_en
        .as_deref()
        .map_or_else(|| "（未提供）".into(), source_value);
    format!(
        "授業名  {}\r\n英語名  {english}\r\ngroup_id  {}\r\n\r\n年度  {}\r\n学期  {}\r\n曜日  {}\r\n時限  {}",
        source_value(&course.group_name),
        visible_text(&course.group_id),
        source_value(&course.year.to_string()),
        source_value(&course.semester),
        source_value(&course.day_of_week),
        source_value(&course.class_order),
    )
}

#[cfg(test)]
#[test]
fn review_displays_observed_fields_without_interpreting_zeros_or_children() {
    let mut course = assignment_manager::webclass::parse_courses(include_str!(
        "../../../../fixtures/webclass/courses_edge_cases.json"
    ))
    .unwrap()
    .remove(0);
    course.semester.clear();
    course.children = "unexpected private field".into();
    let displayed = details(&course);
    assert!(displayed.contains("授業名  共通演習"));
    assert!(displayed.contains("group_id  fixture-course-004"));
    assert!(displayed.contains("年度  2026"));
    assert!(displayed.contains("英語名  （未提供）"));
    assert!(displayed.contains("学期  （空欄・意味未確認）"));
    assert!(displayed.contains("曜日  0（意味未確認）"));
    assert!(displayed.contains("時限  0（意味未確認）"));
    assert!(!displayed.contains(&course.children));
}
