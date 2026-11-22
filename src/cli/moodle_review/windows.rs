//! Native dialog controls. Candidate data is passed only to this process's window.

mod categories;
mod create;

use std::{
    cell::RefCell,
    ptr::{null, null_mut},
};

use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

use crate::cli::windows_dialog::{
    add_item, control_index, create_controls, interactive_station, open_dialog, send, set_text,
    utf16, visible_text,
};

use super::{CHOICES, Counts, Review};

pub(super) use categories::show_categories;
pub(in crate::cli) use create::{approve_create, choose_create};

const CANDIDATES: i32 = 100;
const DETAILS: i32 = 101;
const CHOICE: i32 = 102;
const MISSING: i32 = 103;
const SUMMARY: i32 = 104;
const FINISHED: isize = 1;
const WINDOW_FAILED: isize = -2;

pub(in crate::cli) fn confirm_fetch(
    require_windows_vpn: bool,
    description: &str,
) -> Result<bool, ()> {
    if !interactive_station() {
        return Err(());
    }
    let vpn = if require_windows_vpn {
        "FortiClientで大学公式VPN hosei が接続中であることを確認してください。\r\n"
    } else {
        ""
    };
    match message(
        null_mut(),
        &format!(
            "{vpn}{description}\r\n\
             開始する場合は OK を押してください。Moodleの取得は最大1回です。"
        ),
        MB_OKCANCEL | MB_ICONINFORMATION | MB_DEFBUTTON2,
    ) {
        IDOK => Ok(true),
        IDCANCEL => Ok(false),
        _ => Err(()),
    }
}

pub(super) fn show(review: &mut Review) -> Result<Option<Counts>, ()> {
    if !interactive_station() {
        return Err(());
    }
    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(());
    }
    let mut state = RefCell::new(Session {
        review,
        finished: None,
        instance,
    });
    let result = open_dialog(instance, "Moodle 課題照合", Some(dialog_proc), &mut state);
    match result {
        FINISHED => state.into_inner().finished.map(Some).ok_or(()),
        0 => Ok(None),
        _ => Err(()),
    }
}

struct Session<'a> {
    review: &'a mut Review,
    finished: Option<Counts>,
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

    let address = if event == WM_INITDIALOG {
        // The user-data slot belongs to this dialog instance.
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, parameter) };
        parameter
    } else if event == WM_COMMAND {
        unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) }
    } else {
        return 0;
    };
    let Some(state) = (unsafe { (address as *const RefCell<Session<'_>>).as_ref() }) else {
        return 0;
    };
    // Control updates can deliver nested notifications to this dialog.
    let Ok(mut state) = state.try_borrow_mut() else {
        return 0;
    };
    if event == WM_INITDIALOG {
        if !initialize(window, &state) {
            unsafe { EndDialog(window, WINDOW_FAILED) };
        }
        return 1;
    }

    let id = (command & 0xffff) as i32;
    let notification = (command >> 16) as u32;
    match (id, notification) {
        (CANDIDATES, LBN_SELCHANGE) => show_selected(window, state.review),
        (CHOICE, CBN_SELCHANGE) => {
            let selected = control_index(window, CANDIDATES, LB_GETCURSEL);
            let choice = control_index(window, CHOICE, CB_GETCURSEL);
            if let (Some(row), Some((choice, _))) = (
                selected.and_then(|index| state.review.rows.get_mut(index)),
                choice.and_then(|index| CHOICES.get(index)),
            ) {
                row.choice = *choice;
            }
            update_summary(window, state.review);
        }
        (MISSING, EN_CHANGE) => update_summary(window, state.review),
        (IDOK, 0) => {
            if let Some(counts) = state.review.finish(missing_count(window)) {
                state.finished = Some(counts);
                unsafe { EndDialog(window, FINISHED) };
            } else {
                message(
                    window,
                    "未確認の候補を確認し、取得漏れの件数を入力してください。漏れがなければ 0 を入力します。",
                    MB_OK | MB_ICONINFORMATION,
                );
            }
        }
        (IDCANCEL, 0) => {
            unsafe { EndDialog(window, 0) };
        }
        _ => return 0,
    }
    1
}

fn initialize(window: HWND, state: &Session<'_>) -> bool {
    // Rectangles use dialog units so the operating system scales to the dialog font.
    let controls = [
        (
            0,
            w!("STATIC"),
            "左から候補を選び、Moodle画面と見比べて判定を選んでください。",
            0,
            [8, 7, 373, 13],
        ),
        (
            CANDIDATES,
            w!("LISTBOX"),
            "",
            WS_BORDER | WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32,
            [8, 23, 149, 149],
        ),
        (
            DETAILS,
            w!("EDIT"),
            "",
            WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | (ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32,
            [164, 23, 217, 112],
        ),
        (
            0,
            w!("STATIC"),
            "選択した候補の判定",
            0,
            [164, 140, 217, 12],
        ),
        (
            CHOICE,
            w!("COMBOBOX"),
            "",
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            [164, 155, 217, 74],
        ),
        (0, w!("STATIC"), "取得漏れの件数", 0, [8, 180, 74, 13]),
        (
            MISSING,
            w!("EDIT"),
            "",
            WS_BORDER | WS_TABSTOP | ES_NUMBER as u32,
            [84, 178, 42, 14],
        ),
        (
            0,
            w!("STATIC"),
            "Moodle画面にだけある対象活動を数え、漏れがなければ 0 を入力します。",
            0,
            [133, 180, 250, 13],
        ),
        (SUMMARY, w!("STATIC"), "", 0, [8, 198, 373, 26]),
        (
            IDOK,
            w!("BUTTON"),
            "集計して閉じる",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            [242, 225, 75, 16],
        ),
        (
            IDCANCEL,
            w!("BUTTON"),
            "中止",
            WS_TABSTOP,
            [326, 225, 55, 16],
        ),
    ];
    if !create_controls(window, state.instance, &controls) {
        return false;
    }
    for (index, row) in state.review.rows.iter().enumerate() {
        let preview: String = visible_text(&row.activity).chars().take(38).collect();
        if !add_item(
            window,
            CANDIDATES,
            LB_ADDSTRING,
            &format!("{:02}  {preview}", index + 1),
        ) {
            return false;
        }
    }
    for (_, label) in CHOICES {
        if !add_item(window, CHOICE, CB_ADDSTRING, label) {
            return false;
        }
    }
    send(window, CANDIDATES, LB_SETCURSEL, 0, 0);
    show_selected(window, state.review);
    true
}

fn show_selected(window: HWND, review: &Review) {
    let selected =
        control_index(window, CANDIDATES, LB_GETCURSEL).and_then(|index| review.rows.get(index));
    if let Some(row) = selected {
        let course = visible_text(row.course.as_deref().unwrap_or("分類なし"));
        let activity = visible_text(&row.activity);
        let repeated = if row.possible_activity_events > 1 {
            format!(
                "\r\n同じ授業名・活動名で期限イベント {} 件。Moodle画面で重複を確認してください。",
                row.possible_activity_events
            )
        } else {
            String::new()
        };
        set_text(
            window,
            DETAILS,
            &format!(
                "授業  {course}\r\n活動  {activity}\r\n種別  {}\r\n期限  {}{repeated}",
                row.kind_label(),
                row.deadline_jst,
            ),
        );
        send(window, CHOICE, CB_SETCURSEL, row.choice as usize, 0);
    } else {
        set_text(
            window,
            DETAILS,
            "課題候補はありません。Moodle画面で取得漏れを確認してください。",
        );
        send(window, CHOICE, CB_SETCURSEL, 0, 0);
    }
    update_summary(window, review);
}

fn update_summary(window: HWND, review: &Review) {
    let missing = missing_count(window);
    let counts = review.counts(missing.unwrap_or(0));
    let missing = missing.map_or_else(|| "未入力".to_owned(), |value| value.to_string());
    set_text(
        window,
        SUMMARY,
        &format!(
            "候補 {}・未確認 {}・一致 {}・候補余り {}・取得漏れ {missing}\r\n\
         重複 {}・期限差 {}・授業差 {}・UID重複 0・同名複数候補 {} 組",
            review.rows.len(),
            counts.pending,
            counts.matched,
            counts.extra,
            counts.duplicates,
            counts.deadline_diff,
            counts.course_diff,
            review.repeated_groups,
        ),
    );
}

fn missing_count(window: HWND) -> Option<usize> {
    let mut buffer = [0; 7];
    let length = unsafe { GetWindowTextLengthW(GetDlgItem(window, MISSING)) };
    if !(1..buffer.len() as i32).contains(&length) {
        return None;
    }
    let size =
        unsafe { GetDlgItemTextW(window, MISSING, buffer.as_mut_ptr(), buffer.len() as i32) };
    let input = String::from_utf16(buffer.get(..size as usize)?).ok()?;
    if !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    input.parse().ok()
}

fn message(window: HWND, text: &str, style: MESSAGEBOX_STYLE) -> i32 {
    let text = utf16(text);
    unsafe { MessageBoxW(window, text.as_ptr(), w!("Moodle 課題照合"), style) }
}

#[cfg(test)]
#[test]
fn source_labels_cannot_inject_form_rows_or_bidi_overrides() {
    assert_eq!(visible_text("活動\r\n期限\u{202e}\0"), "活動  期限  ");
    assert_eq!(
        visible_text("授業 (HTTPS://example.invalid/calendar?token=synthetic)"),
        "URLを含む値は非表示"
    );
}
