//! Native Win32 UI helpers shared by the windows: DPI scaling, fonts, controls.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateFontW, DeleteObject, HFONT, HGDIOBJ, CLEARTYPE_QUALITY, DEFAULT_CHARSET, FW_SEMIBOLD,
    OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, FF_DONTCARE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC, EnableWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::util::{from_wide, wide};
use crate::core::settings::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};

/// Static control styles (not exported by the windows crate as constants).
pub const SS_NOPREFIX: u32 = 0x0080;
pub const SS_ENDELLIPSIS: u32 = 0x4000;
/// Trackbar position query (WM_USER).
pub const TBM_GETPOS: u32 = 0x0400;

pub fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default() }
}

pub fn dpi_of(hwnd: HWND) -> u32 {
    let d = unsafe { GetDpiForWindow(hwnd) };
    if d == 0 {
        96
    } else {
        d
    }
}

pub fn scale(v: i32, dpi: u32) -> i32 {
    (v as i64 * dpi as i64 / 96) as i32
}

/// The UI font (Segoe UI at the user's message-font size) and a semibold heading font.
pub struct Fonts {
    pub normal: HFONT,
    pub heading: HFONT,
}

impl Fonts {
    pub fn new(dpi: u32) -> Fonts {
        unsafe {
            let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
            let ok = SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS.0,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut _),
                0,
                dpi,
            )
            .is_ok();
            let normal = if ok {
                CreateFontIndirectW(&ncm.lfMessageFont)
            } else {
                CreateFontW(-scale(12, dpi), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32, w!("Segoe UI"))
            };
            let mut lf = ncm.lfMessageFont;
            if !ok {
                lf.lfHeight = -scale(12, dpi);
                let face = wide("Segoe UI");
                lf.lfFaceName[..face.len()].copy_from_slice(&face);
            }
            lf.lfHeight = (lf.lfHeight as f32 * 1.25) as i32;
            lf.lfWeight = FW_SEMIBOLD.0 as i32;
            let heading = CreateFontIndirectW(&lf);
            Fonts { normal, heading }
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.normal.0));
            let _ = DeleteObject(HGDIOBJ(self.heading.0));
        }
    }
}

pub fn set_font(hwnd: HWND, font: HFONT) {
    unsafe {
        SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
}

pub fn control(parent: HWND, class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, id: i32) -> HWND {
    let t = wide(text);
    unsafe {
        CreateWindowExW(
            ex,
            class,
            PCWSTR(t.as_ptr()),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
            0,
            0,
            10,
            10,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            Some(hinstance()),
            None,
        )
        .unwrap_or_default()
    }
}

pub fn item(parent: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(parent), id).unwrap_or_default() }
}

pub fn place(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

pub fn set_text(hwnd: HWND, s: &str) {
    let t = wide(s);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(t.as_ptr()));
    }
}

pub fn get_text(hwnd: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(hwnd);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(hwnd, &mut buf);
        from_wide(&buf[..got.max(0) as usize])
    }
}

pub fn enable(hwnd: HWND, on: bool) {
    unsafe {
        let _ = EnableWindow(hwnd, on);
    }
}

pub fn send(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(hwnd, msg, Some(WPARAM(w)), Some(LPARAM(l))).0 }
}

pub fn combo_fill(hwnd: HWND, items: &[&str], selected: usize) {
    send(hwnd, CB_RESETCONTENT, 0, 0);
    for s in items {
        let t = wide(s);
        send(hwnd, CB_ADDSTRING, 0, t.as_ptr() as isize);
    }
    send(hwnd, CB_SETCURSEL, selected, 0);
}

pub fn combo_sel(hwnd: HWND) -> usize {
    send(hwnd, CB_GETCURSEL, 0, 0).max(0) as usize
}

pub fn check(hwnd: HWND, on: bool) {
    send(hwnd, BM_SETCHECK, if on { 1 } else { 0 }, 0);
}

pub fn checked(hwnd: HWND) -> bool {
    send(hwnd, BM_GETCHECK, 0, 0) == 1
}

/// Human-readable hotkey, e.g. "Alt+F10".
pub fn hotkey_name(h: Hotkey) -> String {
    let mut parts = Vec::new();
    if h.mods & MOD_CONTROL != 0 {
        parts.push("Ctrl".to_string());
    }
    if h.mods & MOD_ALT != 0 {
        parts.push("Alt".to_string());
    }
    if h.mods & MOD_SHIFT != 0 {
        parts.push("Shift".to_string());
    }
    if h.mods & MOD_WIN != 0 {
        parts.push("Win".to_string());
    }
    let key = match h.vk {
        0x70..=0x87 => format!("F{}", h.vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(h.vk).map(String::from).unwrap_or_default(),
        _ => unsafe {
            let mut scan = MapVirtualKeyW(h.vk, MAPVK_VK_TO_VSC);
            // Navigation keys need the extended-key bit to be named correctly.
            if matches!(h.vk, 0x21..=0x2E) {
                scan |= 0x100;
            }
            let mut buf = [0u16; 32];
            let n = GetKeyNameTextW((scan << 16) as i32, &mut buf);
            if n > 0 {
                from_wide(&buf[..n as usize])
            } else {
                format!("Key {:#04x}", h.vk)
            }
        },
    };
    parts.push(key);
    parts.join("+")
}

/// msctls_hotkey32 encoding <-> RegisterHotKey modifiers.
pub fn hotkey_to_control(h: Hotkey) -> usize {
    let mut f = 0u32;
    if h.mods & MOD_SHIFT != 0 {
        f |= 1;
    }
    if h.mods & MOD_CONTROL != 0 {
        f |= 2;
    }
    if h.mods & MOD_ALT != 0 {
        f |= 4;
    }
    if matches!(h.vk, 0x21..=0x2E) {
        f |= 8; // HOTKEYF_EXT
    }
    ((f << 8) | (h.vk & 0xFF)) as usize
}

pub fn hotkey_from_control(v: usize, keep_win: bool) -> Hotkey {
    let vk = (v & 0xFF) as u32;
    let f = ((v >> 8) & 0xFF) as u32;
    let mut mods = 0;
    if f & 1 != 0 {
        mods |= MOD_SHIFT;
    }
    if f & 2 != 0 {
        mods |= MOD_CONTROL;
    }
    if f & 4 != 0 {
        mods |= MOD_ALT;
    }
    if keep_win {
        mods |= MOD_WIN;
    }
    Hotkey { vk, mods }
}

/// A hotkey without modifiers is only acceptable for keys games rarely use.
pub fn hotkey_is_reasonable(h: Hotkey) -> bool {
    h.vk != 0 && (h.mods & (MOD_ALT | MOD_CONTROL | MOD_WIN) != 0 || matches!(h.vk, 0x70..=0x87 | 0x13 | 0x91 | 0x2D))
}

pub fn message_box(owner: HWND, text: &str, title: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let t = wide(text);
    let c = wide(title);
    unsafe { MessageBoxW(Some(owner), PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), style) }
}

/// Brings an existing window to the front (restoring it if minimized).
pub fn activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Centers a window of the given client size on the monitor with the cursor and returns
/// the outer rect (x, y, w, h).
pub fn centered_rect(style: WINDOW_STYLE, ex: WINDOW_EX_STYLE, client_w: i32, client_h: i32, dpi: u32) -> (i32, i32, i32, i32) {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
    unsafe {
        let mut r = RECT { left: 0, top: 0, right: client_w, bottom: client_h };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let wa = mi.rcWork;
        (wa.left + ((wa.right - wa.left) - w).max(0) / 2, wa.top + ((wa.bottom - wa.top) - h).max(0) / 2, w, h)
    }
}

/// DPI of the monitor under the cursor (for sizing a window before it exists).
pub fn cursor_monitor_dpi() -> u32 {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_ok() {
            x
        } else {
            96
        }
    }
}

/// Opens a folder picker; returns the chosen path.
pub fn pick_folder(owner: HWND, title: &str) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH};
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let t = wide(title);
        let _ = dlg.SetTitle(PCWSTR(t.as_ptr()));
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = super::util::pwstr_to_string(p);
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        Some(s)
    }
}

/// Opens a file picker for executables; returns the chosen path.
pub fn pick_exe(owner: HWND) -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, SIGDN_FILESYSPATH};
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM).ok()?;
        let name = wide("Programs");
        let spec = wide("*.exe");
        let filters = [COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) }];
        let _ = dlg.SetFileTypes(&filters);
        let t = wide("Choose the game's .exe");
        let _ = dlg.SetTitle(PCWSTR(t.as_ptr()));
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = super::util::pwstr_to_string(p);
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        Some(s)
    }
}

/// Opens Explorer with `path` selected.
pub fn show_in_folder(path: &std::path::Path) {
    use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};
    let w = wide(&path.to_string_lossy());
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(w.as_ptr()));
        if !pidl.is_null() {
            let _ = SHOpenFolderAndSelectItems(pidl, None, 0);
            ILFree(Some(pidl));
        } else if let Some(parent) = path.parent() {
            open_path(parent);
        }
    }
}

/// Opens a file or folder with its default handler.
pub fn open_path(path: &std::path::Path) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    let w = wide(&path.to_string_lossy());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(w.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Moves a file to the Recycle Bin (recoverable).
pub fn recycle(path: &std::path::Path) -> bool {
    use windows::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};
    // SHFileOperation takes a double-NUL-terminated list.
    let mut from: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
    from.extend_from_slice(&[0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    unsafe { SHFileOperationW(&mut op) == 0 && !op.fAnyOperationsAborted.as_bool() }
}
