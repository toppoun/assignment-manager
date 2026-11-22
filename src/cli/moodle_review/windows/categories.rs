use super::{
    CANDIDATES, CHOICE, DETAILS, FINISHED, SUMMARY, WINDOW_FAILED, add_item, control_index,
    create_controls, interactive_station, open_dialog, send, set_text, visible_text,
};
use crate::cli::moodle_review::{CATEGORY_CHOICES, Categories, CategoryAction};
use std::{cell::RefCell, ptr::null};
use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

const REVIEW_ASSIGNMENTS: i32 = 105;
const OPEN_ASSIGNMENTS: isize = 2;

pub(in crate::cli::moodle_review) fn show_categories(
    categories: &mut Categories,
) -> Result<CategoryAction, ()> {
    if !interactive_station() {
        return Err(());
    }
    let instance = unsafe { GetModuleHandleW(null()) };
    if instance.is_null() {
        return Err(());
    }
    let mut session = RefCell::new(Session {
        categories,
        instance,
    });
    match open_dialog(
        instance,
        "Moodle 授業カテゴリ",
        Some(dialog_proc),
        &mut session,
    ) {
        FINISHED | 0 => Ok(CategoryAction::Finish),
        OPEN_ASSIGNMENTS => Ok(CategoryAction::ReviewAssignments),
        _ => Err(()),
    }
}

struct Session<'a> {
    categories: &'a mut Categories,
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
    let Some(session) = (unsafe { (address as *const RefCell<Session<'_>>).as_ref() }) else {
        return 0;
    };
    let Ok(mut session) = session.try_borrow_mut() else {
        return 0;
    };
    if event == WM_INITDIALOG {
        if !initialize(window, &session) {
            unsafe { EndDialog(window, WINDOW_FAILED) };
        }
        return 1;
    }
    let categories = &mut *session.categories;
    let id = (command & 0xffff) as i32;
    let notification = (command >> 16) as u32;
    match (id, notification) {
        (CANDIDATES, LBN_SELCHANGE) => show_selected(window, categories),
        (CHOICE, CBN_SELCHANGE) => {
            if let (Some(row), Some((choice, _))) = (
                control_index(window, CANDIDATES, LB_GETCURSEL)
                    .and_then(|index| categories.rows.get_mut(index)),
                control_index(window, CHOICE, CB_GETCURSEL)
                    .and_then(|index| CATEGORY_CHOICES.get(index)),
            ) {
                row.choice = *choice;
            }
            update_summary(window, categories);
        }
        (IDOK, 0) => {
            unsafe { EndDialog(window, FINISHED) };
        }
        (IDCANCEL, 0) => {
            unsafe { EndDialog(window, 0) };
        }
        (REVIEW_ASSIGNMENTS, 0) => {
            unsafe { EndDialog(window, OPEN_ASSIGNMENTS) };
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
            "カテゴリごとの候補数を確認し、検証用授業ページの対応状況を選んでください。",
            0,
            [8, 7, 373, 14],
        ),
        (
            CANDIDATES,
            w!("LISTBOX"),
            "",
            WS_BORDER | WS_TABSTOP | WS_VSCROLL | LBS_NOTIFY as u32,
            [8, 24, 150, 150],
        ),
        (
            DETAILS,
            w!("EDIT"),
            "",
            WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | (ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32,
            [164, 24, 217, 80],
        ),
        (
            0,
            w!("STATIC"),
            "実授業と同じ授業の検証用ページだけを選びます。\r\nPhase 3Dの合成授業は対応先に含めません。",
            0,
            [164, 110, 217, 38],
        ),
        (
            CHOICE,
            w!("COMBOBOX"),
            "",
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            [164, 153, 217, 75],
        ),
        (SUMMARY, w!("STATIC"), "", 0, [8, 183, 373, 35]),
        (
            REVIEW_ASSIGNMENTS,
            w!("BUTTON"),
            "課題を個別照合",
            WS_TABSTOP,
            [190, 225, 88, 16],
        ),
        (
            IDOK,
            w!("BUTTON"),
            "集計して閉じる",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            [288, 225, 93, 16],
        ),
    ];
    if !create_controls(window, session.instance, &controls) {
        return false;
    }
    for row in &session.categories.rows {
        let preview: String = visible_text(&row.name).chars().take(38).collect();
        if !add_item(
            window,
            CANDIDATES,
            LB_ADDSTRING,
            &format!("{} 件  {preview}", row.candidates),
        ) {
            return false;
        }
    }
    for (_, label) in CATEGORY_CHOICES {
        if !add_item(window, CHOICE, CB_ADDSTRING, label) {
            return false;
        }
    }
    send(window, CANDIDATES, LB_SETCURSEL, 0, 0);
    show_selected(window, session.categories);
    true
}

fn show_selected(window: HWND, categories: &Categories) {
    let row = control_index(window, CANDIDATES, LB_GETCURSEL)
        .and_then(|index| categories.rows.get(index));
    if let Some(row) = row {
        set_text(
            window,
            DETAILS,
            &format!(
                "候補 {} 件\r\n\r\n{}",
                row.candidates,
                visible_text(&row.name)
            ),
        );
        send(window, CHOICE, CB_SETCURSEL, row.choice as usize, 0);
    } else {
        set_text(
            window,
            DETAILS,
            "対応先を確認できる授業カテゴリはありません。",
        );
    }
    update_summary(window, categories);
}

fn update_summary(window: HWND, categories: &Categories) {
    let counts = categories.mapping_counts();
    let classified = categories.classified();
    set_text(
        window,
        SUMMARY,
        &format!(
            "候補 {} 件・固有カテゴリ {} 件・カテゴリ未確定 {} 件\r\n既存の対応先 {} 件・新規が必要 {} 件・曖昧 {} 件・未確認 {} 件",
            classified + categories.unclassified,
            categories.rows.len(),
            categories.unclassified,
            counts.existing,
            counts.new_pages,
            counts.ambiguous,
            counts.pending,
        ),
    );
}
