//! Trim window: preview a clip (Media Foundation player), mark start and end, and save a
//! lossless cut either as a new clip or in place of the original (the original goes to the
//! Recycle Bin, so replacing is reversible).

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::GetCapture;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::clips_ui;
use super::ui::{self, Fonts};
use super::util::{human_bytes, wide};
use crate::core::clip;

const CLASS: PCWSTR = w!("ReplayTrimWindow");
const VIDEO_CLASS: PCWSTR = w!("ReplayVideoSurface");
const WM_APP_TRIMMED: u32 = WM_APP + 30;
const TIMER_POS: usize = 1;

const ID_VIDEO: i32 = 10;
const ID_SLIDER: i32 = 11;
const ID_PLAY: i32 = 12;
const ID_START: i32 = 13;
const ID_END: i32 = 14;
const ID_TIME: i32 = 15;
const ID_RANGE: i32 = 16;
const ID_SAVE_NEW: i32 = 17;
const ID_REPLACE: i32 = 18;
const ID_CLOSE: i32 = 2; // IDCANCEL

struct Win {
    hwnd: HWND,
    fonts: Fonts,
    path: PathBuf,
    duration_ms: i32,
    start_ms: i32,
    end_ms: i32,
    player: Option<IMFPMediaPlayer>,
    busy: bool,
}

thread_local! {
    static WIN: RefCell<Option<Win>> = const { RefCell::new(None) };
}

fn hwnd() -> Option<HWND> {
    WIN.with(|w| w.try_borrow().ok().and_then(|g| g.as_ref().map(|w| w.hwnd)))
}

pub fn is_dialog_message(msg: &MSG) -> bool {
    match hwnd() {
        Some(h) => unsafe { IsDialogMessageW(h, msg).as_bool() },
        None => false,
    }
}

pub fn close() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

fn fmt_ms(ms: i32) -> String {
    let ms = ms.max(0);
    format!("{}:{:02}.{}", ms / 60_000, (ms / 1000) % 60, (ms % 1000) / 100)
}

fn player() -> Option<IMFPMediaPlayer> {
    WIN.with(|w| w.borrow().as_ref().and_then(|w| w.player.clone()))
}

fn position_ms() -> Option<i32> {
    let p = player()?;
    unsafe {
        let pv = p.GetPosition(&MFP_POSITIONTYPE_100NS).ok()?;
        i64::try_from(&pv).ok().map(|v| (v / 10_000) as i32)
    }
}

fn seek(ms: i32) {
    if let Some(p) = player() {
        unsafe {
            let pv = PROPVARIANT::from(ms.max(0) as i64 * 10_000);
            let _ = p.SetPosition(&MFP_POSITIONTYPE_100NS, &pv);
        }
    }
}

fn is_playing() -> bool {
    player().and_then(|p| unsafe { p.GetState().ok() }).map(|s| s == MFP_MEDIAPLAYER_STATE_PLAYING).unwrap_or(false)
}

fn update_labels(h: HWND) {
    let Some((start, end, dur, busy)) = WIN.with(|w| w.borrow().as_ref().map(|w| (w.start_ms, w.end_ms, w.duration_ms, w.busy))) else { return };
    let slider = ui::item(h, ID_SLIDER);
    ui::send(slider, TBM_SETSELSTART, 0, start as isize);
    ui::send(slider, TBM_SETSELEND, 1, end as isize);
    ui::set_text(
        ui::item(h, ID_RANGE),
        &format!("Keep {} to {} ({:.1} s of {:.1} s)", fmt_ms(start), fmt_ms(end), (end - start) as f64 / 1000.0, dur as f64 / 1000.0),
    );
    let full = start == 0 && end >= dur;
    ui::enable(ui::item(h, ID_SAVE_NEW), !busy && !full && end > start);
    ui::enable(ui::item(h, ID_REPLACE), !busy && !full && end > start);
    ui::set_text(ui::item(h, ID_PLAY), if is_playing() { "Pause" } else { "Play" });
}

fn tick(h: HWND) {
    let dur = WIN.with(|w| w.borrow().as_ref().map(|w| w.duration_ms)).unwrap_or(0);
    let has_player = player().is_some();
    ui::enable(ui::item(h, ID_PLAY), has_player);
    let pos = if has_player { position_ms() } else { Some(ui::send(ui::item(h, ID_SLIDER), ui::TBM_GETPOS, 0, 0) as i32) };
    if let Some(pos) = pos {
        let slider = ui::item(h, ID_SLIDER);
        // Don't fight the user while they drag the thumb.
        if unsafe { GetCapture() } != slider {
            ui::send(slider, TBM_SETPOS, 1, pos as isize);
        }
        ui::set_text(ui::item(h, ID_TIME), &format!("{} / {}", fmt_ms(pos), fmt_ms(dur)));
    }
    ui::set_text(ui::item(h, ID_PLAY), if is_playing() { "Pause" } else { "Play" });
}

fn set_mark(h: HWND, start: bool) {
    let pos = position_ms().unwrap_or_else(|| ui::send(ui::item(h, ID_SLIDER), ui::TBM_GETPOS, 0, 0) as i32);
    WIN.with(|w| {
        if let Some(win) = w.borrow_mut().as_mut() {
            if start {
                win.start_ms = pos.clamp(0, win.duration_ms);
                if win.end_ms <= win.start_ms {
                    win.end_ms = win.duration_ms;
                }
            } else {
                win.end_ms = pos.clamp(0, win.duration_ms);
                if win.end_ms <= win.start_ms {
                    win.start_ms = 0;
                }
            }
        }
    });
    update_labels(h);
}

fn unique_path(p: &std::path::Path, suffix: &str) -> PathBuf {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    let mut candidate = dir.join(format!("{stem} {suffix}.mp4"));
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem} {suffix} {n}.mp4"));
        n += 1;
    }
    candidate
}

struct TrimDone {
    result: Result<(PathBuf, clip::ClipInfo), String>,
}

fn save(h: HWND, replace: bool) {
    let Some((src, start, end)) = WIN.with(|w| w.borrow().as_ref().map(|w| (w.path.clone(), w.start_ms, w.end_ms))) else { return };
    if replace {
        let r = ui::message_box(
            h,
            "Replace the original clip with the trimmed version? The original is moved to the Recycle Bin.",
            "Trim clip",
            MB_OKCANCEL | MB_ICONQUESTION,
        );
        if r != IDOK {
            return;
        }
        // The player keeps the file open; release it before replacing.
        if let Some(p) = WIN.with(|w| w.borrow_mut().as_mut().and_then(|w| w.player.take())) {
            unsafe {
                let _ = p.Shutdown();
            }
        }
    }
    WIN.with(|w| {
        if let Some(win) = w.borrow_mut().as_mut() {
            win.busy = true;
        }
    });
    update_labels(h);
    ui::set_text(ui::item(h, ID_RANGE), "Saving...");
    let target = h.0 as isize;
    std::thread::spawn(move || {
        let result = (|| -> Result<(PathBuf, clip::ClipInfo), String> {
            let out = if replace { unique_path(&src, "trim-temp") } else { unique_path(&src, "(trimmed)") };
            let info = clip::trim(&src, &out, start as f64 / 1000.0, end as f64 / 1000.0).map_err(|e| e.to_string())?;
            if replace {
                if !ui::recycle(&src) {
                    let _ = std::fs::remove_file(&out);
                    return Err("The original clip couldn't be moved to the Recycle Bin.".into());
                }
                std::fs::rename(&out, &src).map_err(|e| format!("Couldn't rename the trimmed clip: {e}"))?;
                return Ok((src, info));
            }
            Ok((out, info))
        })();
        let msg = Box::new(TrimDone { result });
        unsafe {
            let _ = PostMessageW(Some(HWND(target as *mut _)), WM_APP_TRIMMED, WPARAM(replace as usize), LPARAM(Box::into_raw(msg) as isize));
        }
    });
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
    let bottom_rows = bh * 2 + s(36) + m * 3;
    ui::place(ui::item(h, ID_VIDEO), 0, 0, cw, (ch - bottom_rows).max(s(100)));
    let y0 = ch - bottom_rows + m;
    ui::place(ui::item(h, ID_SLIDER), m, y0, cw - 2 * m, s(32));
    let y1 = y0 + s(36);
    ui::place(ui::item(h, ID_PLAY), m, y1, s(80), bh);
    ui::place(ui::item(h, ID_START), m + s(86), y1, s(90), bh);
    ui::place(ui::item(h, ID_END), m + s(182), y1, s(90), bh);
    ui::place(ui::item(h, ID_TIME), m + s(284), y1 + s(5), s(140), s(20));
    ui::place(ui::item(h, ID_RANGE), m + s(430), y1 + s(5), cw - m * 2 - s(430), s(20));
    let y2 = y1 + bh + m;
    ui::place(ui::item(h, ID_CLOSE), cw - m - s(90), y2, s(90), bh);
    ui::place(ui::item(h, ID_REPLACE), cw - m - s(90) - s(6) - s(130), y2, s(130), bh);
    ui::place(ui::item(h, ID_SAVE_NEW), cw - m - s(90) - s(12) - s(260), y2, s(130), bh);
    if let Some(p) = player() {
        unsafe {
            let _ = p.UpdateVideo();
        }
    }
}

fn apply_fonts(h: HWND) {
    let Some(font) = WIN.with(|w| w.borrow().as_ref().map(|w| w.fonts.normal)) else { return };
    for id in [ID_SLIDER, ID_PLAY, ID_START, ID_END, ID_TIME, ID_RANGE, ID_SAVE_NEW, ID_REPLACE, ID_CLOSE] {
        ui::set_font(ui::item(h, id), font);
    }
}

unsafe extern "system" fn video_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(h, &mut ps);
            match player() {
                Some(p) => {
                    let _ = p.UpdateVideo();
                }
                None => {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(h, &mut rc);
                    FillRect(hdc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
                    SetBkMode(hdc, TRANSPARENT);
                    SetTextColor(hdc, COLORREF(0x00B0B0B0));
                    let mut t = wide("Preview unavailable. You can still trim using the slider.");
                    let n = t.len() - 1;
                    DrawTextW(hdc, &mut t[..n], &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                }
            }
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let none = WINDOW_EX_STYLE(0);
            ui::control(h, VIDEO_CLASS, "", 0, none, ID_VIDEO);
            let slider = ui::control(h, TRACKBAR_CLASSW, "", WS_TABSTOP.0 | TBS_HORZ | TBS_NOTICKS | TBS_ENABLESELRANGE, none, ID_SLIDER);
            let button = |id: i32, text: &str| {
                ui::control(h, w!("BUTTON"), text, WS_TABSTOP.0 | BS_PUSHBUTTON as u32, none, id);
            };
            button(ID_PLAY, "Play");
            button(ID_START, "Set start");
            button(ID_END, "Set end");
            ui::control(h, w!("STATIC"), "", ui::SS_NOPREFIX, none, ID_TIME);
            ui::control(h, w!("STATIC"), "", ui::SS_NOPREFIX, none, ID_RANGE);
            button(ID_SAVE_NEW, "Save as new clip");
            button(ID_REPLACE, "Replace original");
            button(ID_CLOSE, "Close");
            let dur = WIN.with(|w| w.borrow().as_ref().map(|w| w.duration_ms)).unwrap_or(0);
            ui::send(slider, TBM_SETRANGEMIN, 0, 0);
            ui::send(slider, TBM_SETRANGEMAX, 1, dur as isize);
            ui::send(slider, TBM_SETLINESIZE, 0, 100);
            ui::send(slider, TBM_SETPAGESIZE, 0, 1000);
            apply_fonts(h);
            layout(h);
            update_labels(h);
            tick(h);
            SetTimer(Some(h), TIMER_POS, 100, None);
            LRESULT(0)
        }
        WM_SIZE => {
            layout(h);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp.0 as *mut MINMAXINFO);
            let dpi = ui::dpi_of(h);
            mmi.ptMinTrackSize = POINT { x: ui::scale(720, dpi), y: ui::scale(420, dpi) };
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_POS => {
            tick(h);
            LRESULT(0)
        }
        WM_HSCROLL => {
            if HWND(lp.0 as *mut _) == ui::item(h, ID_SLIDER) {
                let pos = ui::send(ui::item(h, ID_SLIDER), ui::TBM_GETPOS, 0, 0) as i32;
                seek(pos);
                let dur = WIN.with(|w| w.borrow().as_ref().map(|w| w.duration_ms)).unwrap_or(0);
                ui::set_text(ui::item(h, ID_TIME), &format!("{} / {}", fmt_ms(pos), fmt_ms(dur)));
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wp.0 & 0xFFFF) as i32 {
                ID_PLAY => {
                    if let Some(p) = player() {
                        if is_playing() {
                            let _ = p.Pause();
                        } else {
                            let _ = p.Play();
                        }
                    }
                    update_labels(h);
                }
                ID_START => set_mark(h, true),
                ID_END => set_mark(h, false),
                ID_SAVE_NEW => save(h, false),
                ID_REPLACE => save(h, true),
                ID_CLOSE => {
                    let _ = DestroyWindow(h);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_TRIMMED => {
            let done = Box::from_raw(lp.0 as *mut TrimDone);
            WIN.with(|w| {
                if let Some(win) = w.borrow_mut().as_mut() {
                    win.busy = false;
                }
            });
            match done.result {
                Ok((path, info)) => {
                    clips_ui::refresh();
                    let name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                    ui::message_box(h, &format!("Saved \"{name}\" ({:.1} s, {}).", info.seconds, human_bytes(info.bytes)), "Trim clip", MB_ICONINFORMATION);
                    if wp.0 != 0 {
                        let _ = DestroyWindow(h);
                        return LRESULT(0);
                    }
                }
                Err(e) => {
                    ui::message_box(h, &format!("Couldn't trim the clip: {e}"), "Trim clip", MB_ICONWARNING);
                    if wp.0 != 0 {
                        // The player was released for replacing; the window can't preview anymore.
                        let _ = DestroyWindow(h);
                        return LRESULT(0);
                    }
                }
            }
            update_labels(h);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wp.0 as *mut _);
            SetBkMode(hdc, TRANSPARENT);
            LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(h), TIMER_POS);
            if let Some(win) = WIN.with(|w| w.borrow_mut().take()) {
                if let Some(p) = win.player {
                    let _ = p.Shutdown();
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub fn open(path: PathBuf, secs: f64) {
    if let Some(h) = hwnd() {
        let same = WIN.with(|w| w.borrow().as_ref().map(|w| w.path == path)).unwrap_or(false);
        if same {
            ui::activate(h);
            return;
        }
        close();
    }
    let duration_ms = if secs > 0.0 {
        (secs * 1000.0) as i32
    } else {
        std::fs::File::open(&path).and_then(|mut f| crate::core::mp4::probe_duration(&mut f)).map(|s| (s * 1000.0) as i32).unwrap_or(0)
    };
    if duration_ms <= 0 {
        ui::message_box(HWND::default(), "This clip can't be trimmed because its length couldn't be read.", "Trim clip", MB_ICONWARNING);
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
            hIcon: LoadIconW(Some(ui::hinstance()), PCWSTR(super::app::IDI_APP as usize as *const u16)).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let vc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(video_proc),
            hInstance: ui::hinstance(),
            lpszClassName: VIDEO_CLASS,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        RegisterClassExW(&vc);

        let dpi = ui::cursor_monitor_dpi();
        let style = WS_OVERLAPPEDWINDOW;
        let ex = WS_EX_CONTROLPARENT;
        let (x, y, cw, ch) = ui::centered_rect(style, ex, ui::scale(860, dpi), ui::scale(600, dpi), dpi);
        let title = wide(&format!("Trim: {}", path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()));
        WIN.with(|w| {
            *w.borrow_mut() = Some(Win {
                hwnd: HWND::default(),
                fonts: Fonts::new(dpi),
                path: path.clone(),
                duration_ms,
                start_ms: 0,
                end_ms: duration_ms,
                player: None,
                busy: false,
            })
        });
        let h = match CreateWindowExW(ex, CLASS, PCWSTR(title.as_ptr()), style, x, y, cw, ch, None, None, Some(ui::hinstance()), None) {
            Ok(h) => h,
            Err(e) => {
                log!("could not open trim window: {e}");
                WIN.with(|w| w.borrow_mut().take());
                return;
            }
        };
        WIN.with(|w| {
            if let Some(win) = w.borrow_mut().as_mut() {
                win.hwnd = h;
            }
        });
        let url = wide(&path.to_string_lossy());
        let mut p: Option<IMFPMediaPlayer> = None;
        match MFPCreateMediaPlayer(PCWSTR(url.as_ptr()), false, MFP_CREATION_OPTIONS(0), None, Some(ui::item(h, ID_VIDEO)), Some(&mut p)) {
            Ok(()) => {
                WIN.with(|w| {
                    if let Some(win) = w.borrow_mut().as_mut() {
                        win.player = p;
                    }
                });
            }
            Err(e) => log!("preview unavailable: {}", super::util::err_text(&e)),
        }
        let _ = ShowWindow(h, SW_SHOW);
        ui::activate(h);
    }
}
