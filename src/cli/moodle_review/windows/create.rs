use std::{ptr::null, ptr::null_mut};

use assignment_manager::{
    domain::Deadline,
    notion::{MoodleLiveCreateChoices, SyncAssignment},
};
use chrono::FixedOffset;
use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

use super::{
    CANDIDATES, DETAILS, WINDOW_FAILED, add_item, control_index, create_controls,
    interactive_station, message, open_dialog, set_text, visible_text,
};

pub(in crate::cli) fn choose_create(
    choices: MoodleLiveCreateChoices<'_>,
) -> Result<Option<usize>, ()> {
    if !interactive_station() {
        return Err(());
    }
    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(());
    }
    let mut session = Session { choices, instance };
    let result = open_dialog(
        instance,
        "Moodle 1件作成の選択",
        Some(dialog_proc),
        &mut session,
    );
    match result {
        0 => Ok(None),
        selected if selected > 0 && selected as usize <= session.choices.eligible.len() => {
            Ok(Some(selected as usize - 1))
        }
        _ => Err(()),
    }
}

pub(in crate::cli) fn approve_create(assignment: &SyncAssignment, held: usize) -> bool {
    interactive_station()
        && message(
            null_mut(),
            &format!(
                "次の課題1件を検証用Notionに作成します。残り {held} 件は保留します。\r\n\r\n{}\r\n\r\n作成を承認する場合は「はい」を選んでください。",
                details(assignment),
            ),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
}

struct Session<'a> {
    choices: MoodleLiveCreateChoices<'a>,
    instance: HINSTANCE,
}

unsafe extern "system" fn dialog_proc(
    window: HWND,
    event: u32,
    command: usize,
    parameter: isize,
) -> isize {
    if event == WM_CLOSE {
        unsafe { EndDialog(window, 0) };
        return 1;
    }
    let address = match event {
        WM_INITDIALOG => {
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, parameter) };
            parameter
        }
        WM_COMMAND => unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) },
        _ => return 0,
    };
    let Some(session) = (unsafe { (address as *const Session<'_>).as_ref() }) else {
        return 0;
    };
    if event == WM_INITDIALOG {
        if !initialize(window, session) {
            unsafe { EndDialog(window, WINDOW_FAILED) };
        }
        return 1;
    }
    let index = control_index(window, CANDIDATES, LB_GETCURSEL);
    match ((command & 0xffff) as i32, (command >> 16) as u32) {
        (CANDIDATES, LBN_SELCHANGE) => {
            if let Some(row) = index.and_then(|index| session.choices.eligible.get(index)) {
                set_text(window, DETAILS, &details(row));
            }
        }
        (IDOK, 0) => {
            if let Some(index) = index.filter(|index| *index < session.choices.eligible.len()) {
                unsafe { EndDialog(window, (index + 1) as isize) };
            }
        }
        (IDCANCEL, 0) => {
            unsafe { EndDialog(window, 0) };
        }
        _ => return 0,
    }
    1
}

const HEADING: i32 = 106;

fn initialize(window: HWND, session: &Session<'_>) -> bool {
    let controls = [
        (HEADING, w!("STATIC"), "", 0, [8, 8, 373, 18]),
        (
            CANDIDATES,
            w!("LISTBOX"),
            "",
            WS_BORDER | WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32,
            [8, 30, 151, 184],
        ),
        (
            DETAILS,
            w!("EDIT"),
            "作成する候補を左から1件選んでください。",
            WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | (ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32,
            [166, 30, 215, 184],
        ),
        (
            IDOK,
            w!("BUTTON"),
            "確認へ進む",
            WS_TABSTOP,
            [223, 225, 79, 16],
        ),
        (
            IDCANCEL,
            w!("BUTTON"),
            "中止",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            [308, 225, 73, 16],
        ),
    ];
    if !create_controls(window, session.instance, &controls) {
        return false;
    }
    let choices = &session.choices;
    set_text(
        window,
        HEADING,
        &format!(
            "全候補 {} 件・作成可能 {} 件・検証用DBの既存課題 {} 件",
            choices.candidate_count,
            choices.eligible.len(),
            choices.existing_rows
        ),
    );
    choices
        .eligible
        .iter()
        .enumerate()
        .all(|(index, assignment)| {
            let label: String = visible_text(&assignment.title).chars().take(38).collect();
            add_item(
                window,
                CANDIDATES,
                LB_ADDSTRING,
                &format!("{}  {label}", index + 1),
            )
        })
}

fn details(assignment: &SyncAssignment) -> String {
    let course = assignment
        .course_key
        .as_ref()
        .map_or("", |course| course.id.as_str());
    let deadline = match &assignment.deadline {
        Some(Deadline::DateTime(value)) => {
            let jst = FixedOffset::east_opt(9 * 60 * 60).expect("fixed JST offset");
            value
                .with_timezone(&jst)
                .format("%Y-%m-%d %H:%M:%S JST")
                .to_string()
        }
        Some(Deadline::DateOnly(value)) => value.to_string(),
        None => "期限なし".into(),
    };
    format!(
        "授業  {}\r\n課題  {}\r\n期限  {deadline}",
        visible_text(course),
        visible_text(&assignment.title)
    )
}
