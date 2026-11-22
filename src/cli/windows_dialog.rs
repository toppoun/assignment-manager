use std::{
    mem::size_of,
    ptr::{from_mut, null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{HINSTANCE, HWND, RECT},
    System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
    },
    UI::WindowsAndMessaging::*,
};
pub(super) fn open_dialog<T>(
    instance: HINSTANCE,
    title: &str,
    proc: DLGPROC,
    state: &mut T,
) -> isize {
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

pub(super) type Control = (i32, windows_sys::core::PCWSTR, &'static str, u32, [i32; 4]);

pub(super) fn create_controls(window: HWND, instance: HINSTANCE, controls: &[Control]) -> bool {
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

pub(super) fn visible_text(value: &str) -> String {
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

pub(super) fn add_item(window: HWND, id: i32, event: u32, text: &str) -> bool {
    let text = utf16(text);
    send(window, id, event, 0, text.as_ptr() as isize) >= 0
}

pub(super) fn control_index(window: HWND, id: i32, event: u32) -> Option<usize> {
    usize::try_from(send(window, id, event, 0, 0)).ok()
}

pub(super) fn send(window: HWND, id: i32, event: u32, word: usize, long: isize) -> isize {
    unsafe { SendDlgItemMessageW(window, id, event, word, long) }
}

pub(super) fn set_text(window: HWND, id: i32, value: &str) {
    let value = utf16(value);
    unsafe { SetDlgItemTextW(window, id, value.as_ptr()) };
}

pub(super) fn utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

pub(super) fn interactive_station() -> bool {
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
