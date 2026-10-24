//! Settings window (native Win32 controls, per-monitor DPI aware).

use std::cell::RefCell;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{GetStockObject, SetBkMode, SetTextColor, COLOR_WINDOW, HBRUSH, HDC, TRANSPARENT, WHITE_BRUSH};
use windows::Win32::UI::Controls::{HKM_GETHOTKEY, HKM_SETHOTKEY};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::app;
use super::ui::{self, Fonts};
use super::util::{human_bytes, wide};
use crate::core::settings::*;

const CLASS: PCWSTR = w!("ReplaySettingsWindow");
const TIMER_STATUS: usize = 1;

const ID_MODE: i32 = 10;
const ID_LENGTH: i32 = 11;
const ID_HOTKEY: i32 = 12;
const ID_RES: i32 = 13;
const ID_FPS: i32 = 14;
const ID_QUALITY: i32 = 15;
const ID_ENCODER: i32 = 16;
const ID_ESTIMATE: i32 = 17;
const ID_AUDIO: i32 = 18;
const ID_MIC: i32 = 19;
const ID_FOLDER: i32 = 20;
const ID_BROWSE: i32 = 21;
const ID_OPENFOLDER: i32 = 22;
const ID_SOUND: i32 = 23;
const ID_AUTOSTART: i32 = 24;
const ID_GAMES: i32 = 25;
const ID_ADDGAME: i32 = 26;
const ID_REMOVE: i32 = 27;
const ID_STATUS1: i32 = 28;
const ID_STATUS2: i32 = 29;
const ID_CAPTURE: i32 = 30;
const ID_SAVE: i32 = 1; // IDOK
const ID_CANCEL: i32 = 2; // IDCANCEL

// Static text ids (headings and labels).
const ID_H_RECORDING: i32 = 40;
const ID_H_VIDEO: i32 = 41;
const ID_H_AUDIO: i32 = 42;
const ID_H_CLIPS: i32 = 43;
const ID_H_GAMES: i32 = 44;
const ID_H_STATUS: i32 = 45;
const ID_L_MODE: i32 = 50;
const ID_L_LENGTH: i32 = 51;
const ID_L_HOTKEY: i32 = 52;
const ID_L_RES: i32 = 53;
const ID_L_FPS: i32 = 54;
const ID_L_QUALITY: i32 = 55;
const ID_L_ENCODER: i32 = 56;
const ID_L_AUDIO: i32 = 57;
const ID_L_CAPTURE: i32 = 58;

const LENGTH_LABELS: [&str; 6] = ["30 seconds", "45 seconds", "1 minute", "1.5 minutes", "2 minutes", "3 minutes"];
const RES_LABELS: [&str; 4] = ["Native", "1440p", "1080p", "720p"];
const FPS_LABELS: [&str; 3] = ["30 fps", "60 fps", "120 fps"];
const QUALITY: [Quality; 4] = [Quality::Low, Quality::Medium, Quality::High, Quality::Max];
const AUDIO: [AudioMode; 3] = [AudioMode::System, AudioMode::Game, AudioMode::None];

const CLIENT_W: i32 = 640;
const CLIENT_H: i32 = 560;

struct Win {
    hwnd: HWND,
    fonts: Fonts,
    draft: Settings,
    original_hotkey: Hotkey,
    autostart: bool,
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

/// Re-reads settings from the app (after changes made from the tray menu).
pub fn reload() {
    if let (Some(h), Some(s)) = (hwnd(), app::current_settings()) {
        WIN.with(|w| {
            if let Some(win) = w.borrow_mut().as_mut() {
                win.original_hotkey = s.hotkey;
                win.draft = s;
            }
        });
        populate(h);
    }
}

fn nearest(choices: &[u32], v: u32) -> usize {
    choices.iter().enumerate().min_by_key(|(_, c)| (**c as i64 - v as i64).abs()).map(|(i, _)| i).unwrap_or(0)
}

fn populate(h: HWND) {
    let Some((s, autostart)) = WIN.with(|w| w.borrow().as_ref().map(|w| (w.draft.clone(), w.autostart))) else { return };
    ui::combo_fill(ui::item(h, ID_MODE), &["When a game is running", "Always (whole screen)"], (s.mode == Mode::Always) as usize);
    ui::combo_fill(ui::item(h, ID_CAPTURE), &["Game window only", "Entire display"], (s.capture == CaptureMode::Display) as usize);
    ui::combo_fill(ui::item(h, ID_LENGTH), &LENGTH_LABELS, nearest(&BUFFER_CHOICES, s.buffer_secs));
    ui::send(ui::item(h, ID_HOTKEY), HKM_SETHOTKEY, ui::hotkey_to_control(s.hotkey), 0);
    ui::combo_fill(ui::item(h, ID_RES), &RES_LABELS, HEIGHT_CHOICES.iter().position(|&x| x == s.height).unwrap_or(2));
    ui::combo_fill(ui::item(h, ID_FPS), &FPS_LABELS, nearest(&FPS_CHOICES, s.fps));
    ui::combo_fill(ui::item(h, ID_QUALITY), &["Low", "Medium", "High", "Very high"], QUALITY.iter().position(|&q| q == s.quality).unwrap_or(2));
    ui::combo_fill(ui::item(h, ID_ENCODER), &["Automatic (GPU first)", "Software (CPU only)"], (s.encoder == EncoderPref::Software) as usize);
    ui::combo_fill(ui::item(h, ID_AUDIO), &["All system audio", "Game audio only", "No audio"], AUDIO.iter().position(|&a| a == s.audio).unwrap_or(0));
    ui::check(ui::item(h, ID_MIC), s.mic);
    ui::set_text(ui::item(h, ID_FOLDER), &app::clip_dir(&s).to_string_lossy());
    ui::check(ui::item(h, ID_SOUND), s.sound);
    ui::check(ui::item(h, ID_AUTOSTART), autostart);
    fill_games(h, &s);
    update_estimate(h);
    update_status(h);
}

fn fill_games(h: HWND, s: &Settings) {
    let lb = ui::item(h, ID_GAMES);
    ui::send(lb, LB_RESETCONTENT, 0, 0);
    for g in &s.games {
        let t = wide(&format!("Always record: {g}"));
        ui::send(lb, LB_ADDSTRING, 0, t.as_ptr() as isize);
    }
    for g in &s.ignored {
        let t = wide(&format!("Never record: {g}"));
        ui::send(lb, LB_ADDSTRING, 0, t.as_ptr() as isize);
    }
    if s.games.is_empty() && s.ignored.is_empty() {
        let t = wide("Games are detected automatically");
        ui::send(lb, LB_ADDSTRING, 0, t.as_ptr() as isize);
    }
}

/// Reads the controls into a Settings value.
fn collect(h: HWND) -> Option<Settings> {
    let (mut s, original) = WIN.with(|w| w.borrow().as_ref().map(|w| (w.draft.clone(), w.original_hotkey)))?;
    s.mode = if ui::combo_sel(ui::item(h, ID_MODE)) == 1 { Mode::Always } else { Mode::Games };
    s.capture = if ui::combo_sel(ui::item(h, ID_CAPTURE)) == 1 { CaptureMode::Display } else { CaptureMode::Window };
    s.buffer_secs = BUFFER_CHOICES[ui::combo_sel(ui::item(h, ID_LENGTH)).min(BUFFER_CHOICES.len() - 1)];
    let hk = ui::send(ui::item(h, ID_HOTKEY), HKM_GETHOTKEY, 0, 0) as usize;
    // The hotkey control can't show the Windows key; keep the original if unchanged.
    s.hotkey = if hk == ui::hotkey_to_control(original) { original } else { ui::hotkey_from_control(hk, false) };
    s.height = HEIGHT_CHOICES[ui::combo_sel(ui::item(h, ID_RES)).min(HEIGHT_CHOICES.len() - 1)];
    s.fps = FPS_CHOICES[ui::combo_sel(ui::item(h, ID_FPS)).min(FPS_CHOICES.len() - 1)];
    s.quality = QUALITY[ui::combo_sel(ui::item(h, ID_QUALITY)).min(3)];
    s.encoder = if ui::combo_sel(ui::item(h, ID_ENCODER)) == 1 { EncoderPref::Software } else { EncoderPref::Auto };
    s.audio = AUDIO[ui::combo_sel(ui::item(h, ID_AUDIO)).min(2)];
    s.mic = ui::checked(ui::item(h, ID_MIC));
    s.sound = ui::checked(ui::item(h, ID_SOUND));
    Some(s)
}

fn update_estimate(h: HWND) {
    let Some(s) = collect(h) else { return };
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN).max(640) as u32, GetSystemMetrics(SM_CYSCREEN).max(480) as u32) };
    let (w, hh) = output_size(sw, sh, s.height);
    let bps = video_bitrate(w, hh, s.fps, s.quality);
    let bytes = buffer_bytes(bps, s.audio != AudioMode::None || s.mic, s.buffer_secs);
    ui::set_text(
        ui::item(h, ID_ESTIMATE),
        &format!(
            "A fullscreen game on your main display records at {w}x{hh}, about {} Mbps. The replay buffer uses about {} of memory; a full clip is the same size on disk.",
            (bps + 500_000) / 1_000_000,
            human_bytes(bytes)
        ),
    );
}

fn update_status(h: HWND) {
    if let Some((a, b)) = app::status_lines() {
        let s1 = ui::item(h, ID_STATUS1);
        let s2 = ui::item(h, ID_STATUS2);
        if ui::get_text(s1) != a {
            ui::set_text(s1, &a);
        }
        if ui::get_text(s2) != b {
            ui::set_text(s2, &b);
        }
    }
}

fn layout(h: HWND) {
    let dpi = ui::dpi_of(h);
    let s = |v: i32| ui::scale(v, dpi);
    let fonts = WIN.with(|w| w.borrow().as_ref().map(|w| (w.fonts.normal, w.fonts.heading)));
    let Some((normal, heading)) = fonts else { return };
    unsafe {
        let _ = EnumChildWindows(Some(h), Some(set_font_proc), LPARAM(normal.0 as isize));
    }
    for id in [ID_H_RECORDING, ID_H_VIDEO, ID_H_AUDIO, ID_H_CLIPS, ID_H_GAMES, ID_H_STATUS] {
        ui::set_font(ui::item(h, id), heading);
    }

    let (lx, rx, colw, labelw) = (s(20), s(332), s(288), s(112));
    let ctrl_x = lx + labelw;
    let ctrl_w = colw - labelw;
    let row = |id_label: i32, id_ctrl: i32, x: i32, y: i32, cw: i32, lw: i32| {
        ui::place(ui::item(h, id_label), x, y + s(4), lw, s(20));
        ui::place(ui::item(h, id_ctrl), x + lw, y, cw, s(if id_ctrl == ID_HOTKEY { 24 } else { 300 }));
    };

    ui::place(ui::item(h, ID_H_RECORDING), lx, s(16), colw, s(24));
    row(ID_L_MODE, ID_MODE, lx, s(48), ctrl_w, labelw);
    row(ID_L_CAPTURE, ID_CAPTURE, lx, s(80), ctrl_w, labelw);
    row(ID_L_LENGTH, ID_LENGTH, lx, s(112), ctrl_w, labelw);
    row(ID_L_HOTKEY, ID_HOTKEY, lx, s(144), ctrl_w, labelw);

    ui::place(ui::item(h, ID_H_VIDEO), lx, s(184), colw, s(24));
    row(ID_L_RES, ID_RES, lx, s(216), ctrl_w, labelw);
    row(ID_L_FPS, ID_FPS, lx, s(248), ctrl_w, labelw);
    row(ID_L_QUALITY, ID_QUALITY, lx, s(280), ctrl_w, labelw);
    row(ID_L_ENCODER, ID_ENCODER, lx, s(312), ctrl_w, labelw);
    ui::place(ui::item(h, ID_ESTIMATE), lx, s(346), colw, s(64));
    let _ = ctrl_x;

    ui::place(ui::item(h, ID_H_AUDIO), rx, s(16), colw, s(24));
    row(ID_L_AUDIO, ID_AUDIO, rx, s(48), colw - s(70), s(70));
    ui::place(ui::item(h, ID_MIC), rx, s(80), colw, s(22));

    ui::place(ui::item(h, ID_H_CLIPS), rx, s(120), colw, s(24));
    ui::place(ui::item(h, ID_FOLDER), rx, s(152), colw, s(24));
    ui::place(ui::item(h, ID_BROWSE), rx, s(182), s(100), s(26));
    ui::place(ui::item(h, ID_OPENFOLDER), rx + s(108), s(182), s(100), s(26));
    ui::place(ui::item(h, ID_SOUND), rx, s(216), colw, s(22));
    ui::place(ui::item(h, ID_AUTOSTART), rx, s(242), colw, s(22));

    ui::place(ui::item(h, ID_H_GAMES), rx, s(280), colw, s(24));
    ui::place(ui::item(h, ID_GAMES), rx, s(310), colw, s(72));
    ui::place(ui::item(h, ID_ADDGAME), rx, s(388), s(100), s(26));
    ui::place(ui::item(h, ID_REMOVE), rx + s(108), s(388), s(100), s(26));

    ui::place(ui::item(h, ID_H_STATUS), lx, s(430), s(600), s(24));
    ui::place(ui::item(h, ID_STATUS1), lx, s(460), s(600), s(36));
    ui::place(ui::item(h, ID_STATUS2), lx, s(496), s(600), s(20));
    ui::place(ui::item(h, ID_CANCEL), s(CLIENT_W - 20 - 96), s(CLIENT_H - 16 - 28), s(96), s(28));
    ui::place(ui::item(h, ID_SAVE), s(CLIENT_W - 20 - 96 - 8 - 96), s(CLIENT_H - 16 - 28), s(96), s(28));
}

unsafe extern "system" fn set_font_proc(child: HWND, lp: LPARAM) -> windows::core::BOOL {
    ui::set_font(child, windows::Win32::Graphics::Gdi::HFONT(lp.0 as *mut _));
    true.into()
}

fn create_controls(h: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CBS_DROPDOWNLIST, ES_AUTOHSCROLL, ES_READONLY, LBS_NOINTEGRALHEIGHT, LBS_NOTIFY};
    use super::ui::SS_NOPREFIX;
    let none = WINDOW_EX_STYLE(0);
    let tab = WS_TABSTOP.0;
    let label = |id: i32, text: &str| {
        ui::control(h, w!("STATIC"), text, SS_NOPREFIX, none, id);
    };
    label(ID_H_RECORDING, "Recording");
    label(ID_H_VIDEO, "Video");
    label(ID_H_AUDIO, "Audio");
    label(ID_H_CLIPS, "Clips");
    label(ID_H_GAMES, "Games");
    label(ID_H_STATUS, "Status");
    label(ID_L_MODE, "Record");
    label(ID_L_CAPTURE, "Capture");
    label(ID_L_LENGTH, "Replay length");
    label(ID_L_HOTKEY, "Save hotkey");
    label(ID_L_RES, "Resolution");
    label(ID_L_FPS, "Frame rate");
    label(ID_L_QUALITY, "Quality");
    label(ID_L_ENCODER, "Encoder");
    label(ID_L_AUDIO, "Record");
    let combo = |id: i32| {
        ui::control(h, w!("COMBOBOX"), "", tab | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0, none, id);
    };
    combo(ID_MODE);
    combo(ID_CAPTURE);
    combo(ID_LENGTH);
    ui::control(h, w!("msctls_hotkey32"), "", tab, none, ID_HOTKEY);
    combo(ID_RES);
    combo(ID_FPS);
    combo(ID_QUALITY);
    combo(ID_ENCODER);
    label(ID_ESTIMATE, "");
    combo(ID_AUDIO);
    let checkbox = |id: i32, text: &str| {
        ui::control(h, w!("BUTTON"), text, tab | BS_AUTOCHECKBOX as u32, none, id);
    };
    checkbox(ID_MIC, "Include my microphone");
    ui::control(h, w!("EDIT"), "", tab | ES_READONLY as u32 | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE, ID_FOLDER);
    let button = |id: i32, text: &str, def: bool| {
        ui::control(h, w!("BUTTON"), text, tab | if def { BS_DEFPUSHBUTTON } else { BS_PUSHBUTTON } as u32, none, id);
    };
    button(ID_BROWSE, "Change...", false);
    button(ID_OPENFOLDER, "Open folder", false);
    checkbox(ID_SOUND, "Play a sound when a clip is saved");
    checkbox(ID_AUTOSTART, "Start Replay when I sign in to Windows");
    ui::control(h, w!("LISTBOX"), "", tab | LBS_NOTIFY as u32 | LBS_NOINTEGRALHEIGHT as u32 | WS_VSCROLL.0, WS_EX_CLIENTEDGE, ID_GAMES);
    button(ID_ADDGAME, "Add game...", false);
    button(ID_REMOVE, "Remove", false);
    label(ID_STATUS1, "");
    label(ID_STATUS2, "");
    button(ID_SAVE, "Save", true);
    button(ID_CANCEL, "Cancel", false);
}

fn on_save(h: HWND) {
    let Some(new) = collect(h) else { return };
    if !ui::hotkey_is_reasonable(new.hotkey) {
        ui::message_box(
            h,
            "Choose a hotkey that includes Ctrl or Alt, or use a function key (F1-F24), so it doesn't trigger while you type or play.",
            "Replay",
            MB_ICONWARNING,
        );
        return;
    }
    let want_autostart = ui::checked(ui::item(h, ID_AUTOSTART));
    let had_autostart = WIN.with(|w| w.borrow().as_ref().map(|w| w.autostart)).unwrap_or(false);
    if want_autostart != had_autostart {
        if let Err(e) = app::set_autostart(want_autostart) {
            ui::message_box(h, &e, "Replay", MB_ICONWARNING);
            return;
        }
    }
    if let Err(e) = app::apply_settings(new) {
        ui::message_box(h, &e, "Replay", MB_ICONWARNING);
        return;
    }
    unsafe {
        let _ = DestroyWindow(h);
    }
}

fn on_remove(h: HWND) {
    let idx = ui::send(ui::item(h, ID_GAMES), LB_GETCURSEL, 0, 0);
    if idx < 0 {
        return;
    }
    let idx = idx as usize;
    let draft = WIN.with(|w| {
        let mut g = w.borrow_mut();
        let win = g.as_mut()?;
        let d = &mut win.draft;
        if idx < d.games.len() {
            d.games.remove(idx);
        } else if idx - d.games.len() < d.ignored.len() {
            let i = idx - d.games.len();
            d.ignored.remove(i);
        }
        Some(d.clone())
    });
    if let Some(d) = draft {
        fill_games(h, &d);
    }
}

fn on_add_game(h: HWND) {
    let Some(path) = ui::pick_exe(h) else { return };
    let Some(exe) = std::path::Path::new(&path).file_name().map(|f| f.to_string_lossy().to_lowercase()) else { return };
    let draft = WIN.with(|w| {
        let mut g = w.borrow_mut();
        let d = &mut g.as_mut()?.draft;
        d.ignored.retain(|x| *x != exe);
        if !d.games.contains(&exe) {
            d.games.push(exe.clone());
            d.games.sort();
        }
        Some(d.clone())
    });
    if let Some(d) = draft {
        fill_games(h, &d);
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_controls(h);
            layout(h);
            populate(h);
            SetTimer(Some(h), TIMER_STATUS, 1000, None);
            LRESULT(0)
        }
        DM_GETDEFID => LRESULT(((DC_HASDEFID as isize) << 16) | ID_SAVE as isize),
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as i32;
            let code = ((wp.0 >> 16) & 0xFFFF) as u32;
            match id {
                ID_SAVE => on_save(h),
                ID_CANCEL => {
                    let _ = DestroyWindow(h);
                }
                ID_BROWSE => {
                    if let Some(dir) = ui::pick_folder(h, "Choose where clips are saved") {
                        WIN.with(|w| {
                            if let Some(win) = w.borrow_mut().as_mut() {
                                win.draft.folder = dir.clone();
                            }
                        });
                        ui::set_text(ui::item(h, ID_FOLDER), &dir);
                    }
                }
                ID_OPENFOLDER => {
                    let dir = std::path::PathBuf::from(ui::get_text(ui::item(h, ID_FOLDER)));
                    let _ = std::fs::create_dir_all(&dir);
                    ui::open_path(&dir);
                }
                ID_ADDGAME => on_add_game(h),
                ID_REMOVE => on_remove(h),
                ID_RES | ID_FPS | ID_QUALITY | ID_LENGTH | ID_AUDIO if code == CBN_SELCHANGE => update_estimate(h),
                ID_MIC => update_estimate(h),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_STATUS => {
            update_status(h);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wp.0 as *mut _);
            let id = GetDlgCtrlID(HWND(lp.0 as *mut _));
            SetBkMode(hdc, TRANSPARENT);
            if matches!(id, ID_ESTIMATE | ID_STATUS2) {
                SetTextColor(hdc, COLORREF(0x00606060));
            }
            if id == ID_FOLDER {
                return DefWindowProcW(h, msg, wp, lp);
            }
            LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
        }
        WM_DPICHANGED => {
            let dpi = (wp.0 & 0xFFFF) as u32;
            WIN.with(|w| {
                if let Some(win) = w.borrow_mut().as_mut() {
                    win.fonts = Fonts::new(dpi);
                }
            });
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(h, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            layout(h);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(h), TIMER_STATUS);
            WIN.with(|w| w.borrow_mut().take());
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub fn open(_owner: HWND, settings: Settings) {
    if let Some(h) = hwnd() {
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
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let ex = WS_EX_CONTROLPARENT;
        let (x, y, cw, ch) = ui::centered_rect(style, ex, ui::scale(CLIENT_W, dpi), ui::scale(CLIENT_H, dpi), dpi);
        let original_hotkey = settings.hotkey;
        WIN.with(|w| {
            *w.borrow_mut() = Some(Win {
                hwnd: HWND::default(),
                fonts: Fonts::new(dpi),
                draft: settings,
                original_hotkey,
                autostart: app::autostart_enabled(),
            })
        });
        let created = CreateWindowExW(ex, CLASS, w!("Replay Settings"), style, x, y, cw, ch, None, None, Some(ui::hinstance()), None);
        match created {
            Ok(h) => {
                WIN.with(|w| {
                    if let Some(win) = w.borrow_mut().as_mut() {
                        win.hwnd = h;
                    }
                });
                // Fonts depend on the window's actual monitor DPI.
                let real = ui::dpi_of(h);
                if real != dpi {
                    WIN.with(|w| {
                        if let Some(win) = w.borrow_mut().as_mut() {
                            win.fonts = Fonts::new(real);
                        }
                    });
                    layout(h);
                }
                let _ = ShowWindow(h, SW_SHOW);
                ui::activate(h);
            }
            Err(e) => {
                log!("could not open settings: {e}");
                WIN.with(|w| w.borrow_mut().take());
            }
        }
    }
}
