//! Clips library: lists saved clips with play, trim, rename, delete (to Recycle Bin) and
//! show-in-folder actions.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{GetStockObject, SetBkMode, SetTextColor, COLOR_WINDOW, HBRUSH, HDC, TRANSPARENT, WHITE_BRUSH};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DELETE, VK_F2, VK_F5};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::app;
use super::ui::{self, Fonts};
use super::util::{clock, human_bytes, wide};
use super::trim_ui;
use crate::core::games::sanitize_filename;
use crate::core::mp4;

const CLASS: PCWSTR = w!("ReplayClipsWindow");
const WM_APP_LOADED: u32 = WM_APP + 20;

const ID_LIST: i32 = 10;
const ID_SUMMARY: i32 = 11;
const ID_PLAY: i32 = 12;
const ID_TRIM: i32 = 13;
const ID_RENAME: i32 = 14;
const ID_SHOW: i32 = 15;
const ID_DELETE: i32 = 16;
const ID_OPENFOLDER: i32 = 17;
const ID_REFRESH: i32 = 18;

#[derive(Clone)]
pub struct Clip {
    pub path: PathBuf,
    pub game: String,
    pub secs: f64,
    pub size: u64,
    pub modified: SystemTime,
}

impl Clip {
    fn name(&self) -> String {
        self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

struct Win {
    hwnd: HWND,
    fonts: Fonts,
    root: PathBuf,
    clips: Vec<Clip>,
    generation: u64,
}

thread_local! {
    static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
}

fn hwnd() -> Option<HWND> {
    WIN.with(|w| w.try_borrow().ok().and_then(|g| g.as_ref().map(|w| w.hwnd)))
}

pub fn is_dialog_message(msg: &MSG) -> bool {
    if trim_ui::is_dialog_message(msg) {
        return true;
    }
    match hwnd() {
        Some(h) => unsafe { IsDialogMessageW(h, msg).as_bool() },
        None => false,
    }
}

pub fn close() {
    trim_ui::close();
    if let Some(h) = hwnd() {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

/// Rescans the folder if the window is open (called after a clip is saved or trimmed).
pub fn refresh() {
    if let Some(h) = hwnd() {
        scan(h);
    }
}

fn scan_folder(root: &Path) -> Vec<Clip> {
    let mut out = Vec::new();
    let mut visit = |dir: &Path, game: &str| -> Vec<PathBuf> {
        let mut subdirs = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let Ok(meta) = e.metadata() else { continue };
                if meta.is_dir() {
                    subdirs.push(p);
                } else if p.extension().map(|x| x.eq_ignore_ascii_case("mp4")).unwrap_or(false) {
                    let secs = std::fs::File::open(&p).and_then(|mut f| mp4::probe_duration(&mut f)).unwrap_or(0.0);
                    out.push(Clip {
                        path: p,
                        game: game.to_string(),
                        secs,
                        size: meta.len(),
                        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    });
                }
            }
        }
        subdirs
    };
    // Clips live in <root>\<Game>\*.mp4; also show any in the root itself.
    for sub in visit(root, "") {
        let game = sub.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        visit(&sub, &game);
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

fn scan(h: HWND) {
    let Some((root, generation)) = WIN.with(|w| {
        let mut g = w.borrow_mut();
        let win = g.as_mut()?;
        win.generation += 1;
        Some((win.root.clone(), win.generation))
    }) else {
        return;
    };
    ui::set_text(ui::item(h, ID_SUMMARY), &format!("Loading clips from {}...", root.display()));
    let target = h.0 as isize;
    std::thread::spawn(move || {
        let clips = scan_folder(&root);
        let msg = Box::new((generation, clips));
        unsafe {
            if PostMessageW(Some(HWND(target as *mut _)), WM_APP_LOADED, WPARAM(0), LPARAM(Box::into_raw(msg) as isize)).is_err() {
                // Window is gone; the box leaks only in that rare race, which is harmless.
            }
        }
    });
}

fn local_time(t: SystemTime) -> String {
    let Ok(d) = t.duration_since(SystemTime::UNIX_EPOCH) else { return String::new() };
    let ticks = d.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    unsafe {
        let mut utc = SYSTEMTIME::default();
        let mut local = SYSTEMTIME::default();
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err() {
            return String::new();
        }
        format!("{:04}-{:02}-{:02} {:02}:{:02}", local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute)
    }
}

fn list(h: HWND) -> HWND {
    ui::item(h, ID_LIST)
}

fn set_cell(lv: HWND, row: i32, col: i32, text: &str) {
    let mut t = wide(text);
    let mut item = LVITEMW { iSubItem: col, pszText: PWSTR(t.as_mut_ptr()), ..Default::default() };
    ui::send(lv, LVM_SETITEMTEXTW, row as usize, &mut item as *mut _ as isize);
}

fn fill_list(h: HWND) {
    let Some((clips, root)) = WIN.with(|w| w.borrow().as_ref().map(|w| (w.clips.clone(), w.root.clone()))) else { return };
    let lv = list(h);
    ui::send(lv, WM_SETREDRAW, 0, 0);
    ui::send(lv, LVM_DELETEALLITEMS, 0, 0);
    for (i, c) in clips.iter().enumerate() {
        let mut t = wide(&c.name());
        let mut item = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM,
            iItem: i as i32,
            pszText: PWSTR(t.as_mut_ptr()),
            lParam: LPARAM(i as isize),
            ..Default::default()
        };
        let row = ui::send(lv, LVM_INSERTITEMW, 0, &mut item as *mut _ as isize) as i32;
        set_cell(lv, row, 1, &c.game);
        set_cell(lv, row, 2, &if c.secs > 0.0 { clock(c.secs) } else { "?".into() });
        set_cell(lv, row, 3, &human_bytes(c.size));
        set_cell(lv, row, 4, &local_time(c.modified));
    }
    ui::send(lv, WM_SETREDRAW, 1, 0);
    let total: u64 = clips.iter().map(|c| c.size).sum();
    let summary = if clips.is_empty() {
        let hotkey = app::current_settings().map(|s| ui::hotkey_name(s.hotkey)).unwrap_or_else(|| "the hotkey".into());
        format!("No clips yet. Press {hotkey} while playing to save one. Clips are saved to {}", root.display())
    } else {
        format!("{} clips, {} in {}", clips.len(), human_bytes(total), root.display())
    };
    ui::set_text(ui::item(h, ID_SUMMARY), &summary);
    update_buttons(h);
}

fn selected(h: HWND) -> Option<(i32, Clip)> {
    let lv = list(h);
    let row = ui::send(lv, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32;
    if row < 0 {
        return None;
    }
    let mut item = LVITEMW { mask: LVIF_PARAM, iItem: row, ..Default::default() };
    ui::send(lv, LVM_GETITEMW, 0, &mut item as *mut _ as isize);
    let idx = item.lParam.0 as usize;
    WIN.with(|w| w.borrow().as_ref().and_then(|w| w.clips.get(idx).cloned())).map(|c| (row, c))
}

fn update_buttons(h: HWND) {
    let any = selected(h).is_some();
    for id in [ID_PLAY, ID_TRIM, ID_RENAME, ID_SHOW, ID_DELETE] {
        ui::enable(ui::item(h, id), any);
    }
}

fn delete_selected(h: HWND) {
    let Some((row, clip)) = selected(h) else { return };
    let r = ui::message_box(
        h,
        &format!("Move \"{}\" to the Recycle Bin?", clip.name()),
        "Delete clip",
        MB_OKCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2,
    );
    if r != IDOK {
        return;
    }
    if ui::recycle(&clip.path) {
        ui::send(list(h), LVM_DELETEITEM, row as usize, 0);
        scan(h);
    } else {
        ui::message_box(h, "The clip couldn't be moved to the Recycle Bin. It may be open in another program.", "Delete clip", MB_ICONWARNING);
    }
}

/// Handles the end of in-place rename. Returns true to accept the new label.
fn finish_rename(h: HWND, new_text: Option<String>) -> bool {
    let Some(text) = new_text else { return false };
    let Some((_, clip)) = selected(h) else { return false };
    let text = text.trim().to_string();
    if text.is_empty() || text == clip.name() {
        return false;
    }
    let clean = sanitize_filename(&text);
    if clean != text {
        ui::message_box(h, "Clip names can't contain any of these characters: < > : \" / \\ | ? *", "Rename clip", MB_ICONWARNING);
        return false;
    }
    let target = clip.path.with_file_name(format!("{clean}.mp4"));
    if target.exists() {
        ui::message_box(h, "A clip with that name already exists.", "Rename clip", MB_ICONWARNING);
        return false;
    }
    match std::fs::rename(&clip.path, &target) {
        Ok(()) => {
            WIN.with(|w| {
                if let Some(win) = w.borrow_mut().as_mut() {
                    if let Some(c) = win.clips.iter_mut().find(|c| c.path == clip.path) {
                        c.path = target.clone();
                    }
                }
            });
            true
        }
        Err(e) => {
            ui::message_box(h, &format!("Couldn't rename the clip: {e}"), "Rename clip", MB_ICONWARNING);
            false
        }
    }
}

fn layout(h: HWND) {
    let dpi = ui::dpi_of(h);
    let s = |v: i32| ui::scale(v, dpi);
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(h, &mut rc);
    }
    let (cw, ch) = (rc.right, rc.bottom);
    let m = s(12);
    let bh = s(28);
    ui::place(ui::item(h, ID_SUMMARY), m, m, cw - 2 * m, s(20));
    ui::place(list(h), m, m + s(26), cw - 2 * m, ch - m * 3 - s(26) - bh);
    let y = ch - m - bh;
    let mut x = m;
    for (id, wdt) in [(ID_PLAY, 80), (ID_TRIM, 80), (ID_RENAME, 80), (ID_SHOW, 110), (ID_DELETE, 80)] {
        ui::place(ui::item(h, id), x, y, s(wdt), bh);
        x += s(wdt) + s(6);
    }
    ui::place(ui::item(h, ID_REFRESH), cw - m - s(80), y, s(80), bh);
    ui::place(ui::item(h, ID_OPENFOLDER), cw - m - s(80) - s(6) - s(100), y, s(100), bh);
    // Name column takes the remaining width.
    let lv = list(h);
    let fixed = s(130) + s(70) + s(80) + s(130) + s(28);
    ui::send(lv, LVM_SETCOLUMNWIDTH, 0, (cw - 2 * m - fixed).max(s(160)) as isize);
    for (i, wdt) in [(1usize, 130), (2, 70), (3, 80), (4, 130)] {
        ui::send(lv, LVM_SETCOLUMNWIDTH, i, s(wdt) as isize);
    }
}

fn apply_fonts(h: HWND) {
    let Some(font) = WIN.with(|w| w.borrow().as_ref().map(|w| w.fonts.normal)) else { return };
    for id in [ID_LIST, ID_SUMMARY, ID_PLAY, ID_TRIM, ID_RENAME, ID_SHOW, ID_DELETE, ID_OPENFOLDER, ID_REFRESH] {
        ui::set_font(ui::item(h, id), font);
    }
}

fn create_controls(h: HWND) {
    let none = WINDOW_EX_STYLE(0);
    ui::control(h, w!("STATIC"), "", ui::SS_NOPREFIX | ui::SS_ENDELLIPSIS, none, ID_SUMMARY);
    let lv = ui::control(
        h,
        WC_LISTVIEWW,
        "",
        WS_TABSTOP.0 | LVS_REPORT | LVS_SHOWSELALWAYS | LVS_SINGLESEL | LVS_EDITLABELS,
        WS_EX_CLIENTEDGE,
        ID_LIST,
    );
    unsafe {
        let _ = windows::Win32::UI::Controls::SetWindowTheme(lv, w!("Explorer"), PCWSTR::null());
    }
    let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
    ui::send(lv, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
    for (i, title) in ["Name", "Game", "Length", "Size", "Saved"].iter().enumerate() {
        let mut t = wide(title);
        let mut col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
            fmt: if i == 2 || i == 3 { LVCFMT_RIGHT } else { LVCFMT_LEFT },
            cx: 100,
            pszText: PWSTR(t.as_mut_ptr()),
            ..Default::default()
        };
        ui::send(lv, LVM_INSERTCOLUMNW, i, &mut col as *mut _ as isize);
    }
    let button = |id: i32, text: &str| {
        ui::control(h, w!("BUTTON"), text, WS_TABSTOP.0 | BS_PUSHBUTTON as u32, none, id);
    };
    button(ID_PLAY, "Play");
    button(ID_TRIM, "Trim...");
    button(ID_RENAME, "Rename");
    button(ID_SHOW, "Show in folder");
    button(ID_DELETE, "Delete");
    button(ID_OPENFOLDER, "Open folder");
    button(ID_REFRESH, "Refresh");
}

fn on_action(h: HWND, id: i32) {
    match id {
        ID_PLAY => {
            if let Some((_, c)) = selected(h) {
                ui::open_path(&c.path);
            }
        }
        ID_TRIM => {
            if let Some((_, c)) = selected(h) {
                trim_ui::open(c.path, c.secs);
            }
        }
        ID_RENAME => {
            if let Some((row, _)) = selected(h) {
                unsafe {
                    let _ = SetFocus(Some(list(h)));
                }
                ui::send(list(h), LVM_EDITLABELW, row as usize, 0);
            }
        }
        ID_SHOW => {
            if let Some((_, c)) = selected(h) {
                ui::show_in_folder(&c.path);
            }
        }
        ID_DELETE => delete_selected(h),
        ID_OPENFOLDER => {
            if let Some(root) = WIN.with(|w| w.borrow().as_ref().map(|w| w.root.clone())) {
                let _ = std::fs::create_dir_all(&root);
                ui::open_path(&root);
            }
        }
        ID_REFRESH => scan(h),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_controls(h);
            apply_fonts(h);
            layout(h);
            scan(h);
            LRESULT(0)
        }
        WM_SIZE => {
            layout(h);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            let dpi = ui::dpi_of(h);
            mmi.ptMinTrackSize = POINT { x: ui::scale(640, dpi), y: ui::scale(320, dpi) };
            LRESULT(0)
        }
        WM_APP_LOADED => {
            let data = Box::from_raw(lp.0 as *mut (u64, Vec<Clip>));
            let (generation, clips) = *data;
            let current = WIN.with(|w| {
                let mut g = w.borrow_mut();
                let win = g.as_mut()?;
                if win.generation == generation {
                    win.clips = clips;
                    Some(())
                } else {
                    None
                }
            });
            if current.is_some() {
                fill_list(h);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            on_action(h, (wp.0 & 0xFFFF) as i32);
            LRESULT(0)
        }
        WM_NOTIFY => {
            let hdr = &*(lp.0 as *const NMHDR);
            if hdr.idFrom as i32 != ID_LIST {
                return DefWindowProcW(h, msg, wp, lp);
            }
            match hdr.code {
                NM_DBLCLK | NM_RETURN => on_action(h, ID_PLAY),
                LVN_ITEMCHANGED => update_buttons(h),
                LVN_KEYDOWN => {
                    // NMLVKEYDOWN is packed; read the key without forming a reference.
                    let vk = std::ptr::addr_of!((*(lp.0 as *const NMLVKEYDOWN)).wVKey).read_unaligned();
                    match vk {
                        k if k == VK_DELETE.0 => delete_selected(h),
                        k if k == VK_F2.0 => on_action(h, ID_RENAME),
                        k if k == VK_F5.0 => scan(h),
                        _ => {}
                    }
                }
                LVN_ENDLABELEDITW => {
                    let di = &*(lp.0 as *const NMLVDISPINFOW);
                    let text = if di.item.pszText.is_null() { None } else { di.item.pszText.to_string().ok() };
                    return LRESULT(finish_rename(h, text) as isize);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wp.0 as *mut _);
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00505050));
            LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
        }
        WM_DPICHANGED => {
            let dpi = (wp.0 & 0xFFFF) as u32;
            WIN.with(|w| {
                if let Some(win) = w.borrow_mut().as_mut() {
                    win.fonts = Fonts::new(dpi);
                }
            });
            apply_fonts(h);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(h, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            layout(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            trim_ui::close();
            WIN.with(|w| w.borrow_mut().take());
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub fn open(root: PathBuf) {
    if let Some(h) = hwnd() {
        let changed = WIN.with(|w| {
            let mut g = w.borrow_mut();
            let win = g.as_mut()?;
            let changed = win.root != root;
            win.root = root.clone();
            Some(changed)
        });
        if changed == Some(true) {
            scan(h);
        }
        ui::activate(h);
        return;
    }
    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: ui::hinstance(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
            hIcon: LoadIconW(Some(ui::hinstance()), PCWSTR(app::IDI_APP as usize as *const u16)).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let dpi = ui::cursor_monitor_dpi();
        let style = WS_OVERLAPPEDWINDOW;
        let ex = WS_EX_CONTROLPARENT;
        let (x, y, cw, ch) = ui::centered_rect(style, ex, ui::scale(820, dpi), ui::scale(480, dpi), dpi);
        WIN.with(|w| *w.borrow_mut() = Some(Win { hwnd: HWND::default(), fonts: Fonts::new(dpi), root, clips: Vec::new(), generation: 0 }));
        match CreateWindowExW(ex, CLASS, w!("Replay Clips"), style, x, y, cw, ch, None, None, Some(ui::hinstance()), None) {
            Ok(h) => {
                WIN.with(|w| {
                    if let Some(win) = w.borrow_mut().as_mut() {
                        win.hwnd = h;
                    }
                });
                let _ = ShowWindow(h, SW_SHOW);
                ui::activate(h);
            }
            Err(e) => {
                log!("could not open clips window: {e}");
                WIN.with(|w| w.borrow_mut().take());
            }
        }
    }
}
