//! Native dialog controls. Candidate data is passed only to this process's window.

mod categories;
mod create;

use std::{
    cell::RefCell,
    mem::size_of,
    ptr::{from_mut, null, null_mut},
};

use windows_sys::{
    Win32::{
        Foundation::{HINSTANCE, HWND, RECT},
        System::{
            LibraryLoader::GetModuleHandleW,
            StationsAndDesktops::{
                GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
            },
        },
        UI::WindowsAndMessaging::*,
    },
    core::w,
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

fn open_dialog<T>(instance: HINSTANCE, title: &str, proc: DLGPROC, state: &mut T) -> isize {
    // Windows requires the template buffer to be aligned on a DWORD boundary.
    #[repr(align(4))]
    struct Template([u16; 128]);
    let mut template = Template([0; 128]);
    let style = WS_POPUP
        | WS_CAPTION
        | WS_SYSMENU
        | DS_MODALFRAME as u32
        | DS_CENTER as u32
        | DS_SETFONT as u32;
    let header = [
        style as u16,
        (style >> 16) as u16,
        0,
        0,
        0,
        0,
        0,
        390,
        249,
        0,
        0,
    ];
    for (slot, value) in template.0.iter_mut().zip(
        header
            .into_iter()
            .chain(title.encode_utf16())
            .chain([0, 9])
            .chain("Segoe UI".encode_utf16())
            .chain([0]),
    ) {
        *slot = value;
    }
    // The dialog owns no Rust data. This pointer stays live until its modal loop returns.
    unsafe {
        DialogBoxIndirectParamW(
            instance,
            template.0.as_ptr().cast(),
            null_mut(),
            proc,
            from_mut(state) as isize,
        )
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

type Control = (i32, windows_sys::core::PCWSTR, &'static str, u32, [i32; 4]);

fn create_controls(window: HWND, instance: HINSTANCE, controls: &[Control]) -> bool {
    for &(id, class, caption, style, [x, y, width, height]) in controls {
        let mut rect = RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        };
        if unsafe { MapDialogRect(window, &mut rect) } == 0 {
            return false;
        }
        let caption = utf16(caption);
        let child = unsafe {
            CreateWindowExW(
                0,
                class,
                caption.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                window,
                id as isize as HMENU,
                instance,
                null(),
            )
        };
        if child.is_null() {
            return false;
        }
        unsafe {
            let font = SendMessageW(window, WM_GETFONT, 0, 0);
            SendMessageW(child, WM_SETFONT, font as usize, 0);
        }
    }

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

fn visible_text(value: &str) -> String {
    if [b"http://".as_slice(), b"https://".as_slice()]
        .into_iter()
        .any(|scheme| {
            value
                .as_bytes()
                .windows(scheme.len())
                .any(|part| part.eq_ignore_ascii_case(scheme))
        })
    {
        return "URLを含む値は非表示".into();
    }
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn add_item(window: HWND, id: i32, event: u32, text: &str) -> bool {
    let text = utf16(text);
    send(window, id, event, 0, text.as_ptr() as isize) >= 0
}

fn control_index(window: HWND, id: i32, event: u32) -> Option<usize> {
    usize::try_from(send(window, id, event, 0, 0)).ok()
}

fn send(window: HWND, id: i32, event: u32, word: usize, long: isize) -> isize {
    unsafe { SendDlgItemMessageW(window, id, event, word, long) }
}

fn set_text(window: HWND, id: i32, value: &str) {
    let value = utf16(value);
    unsafe { SetDlgItemTextW(window, id, value.as_ptr()) };
}

fn message(window: HWND, text: &str, style: MESSAGEBOX_STYLE) -> i32 {
    let text = utf16(text);
    unsafe { MessageBoxW(window, text.as_ptr(), w!("Moodle 課題照合"), style) }
}

fn utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn interactive_station() -> bool {
    let station = unsafe { GetProcessWindowStation() };
    let mut flags = USEROBJECTFLAGS::default();
    let inspected = unsafe {
        !station.is_null()
            && GetUserObjectInformationW(
                station,
                UOI_FLAGS,
                from_mut(&mut flags).cast(),
                size_of::<USEROBJECTFLAGS>() as u32,
                null_mut(),
            ) != 0
    };
    inspected && flags.dwFlags & WSF_VISIBLE as u32 != 0
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
