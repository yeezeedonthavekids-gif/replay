//! The tray application: owns settings, detects games, starts/stops recording sessions,
//! saves clips on the hotkey, and shows notifications.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Controls::{InitCommonControlsEx, LoadIconMetric, ICC_BAR_CLASSES, ICC_HOTKEY_CLASS, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, LIM_SMALL};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::detect::{foreground, primary_monitor, ProcessWatch};
use super::recorder::{self, Session, SessionConfig, Shared, WM_APP_SESSION};
use super::util::{data_dir, default_clip_dir, duration_words, human_bytes, human_secs, init_log, wide, ComGuard, MfGuard};
use super::{clips_ui, settings_ui, sound, ui};
use crate::core::games::{classify, sanitize_filename, Reason};
use crate::core::settings::{buffer_bytes, Mode, Settings};

const WM_APP_TRAY: u32 = WM_APP + 1;
const WM_APP_SAVED: u32 = WM_APP + 2;
const WM_APP_FOREGROUND: u32 = WM_APP + 4;
pub const WM_APP_SHOW: u32 = WM_APP + 5;
pub const WM_APP_SHOW_CLIPS: u32 = WM_APP + 6;

const TIMER_TICK: usize = 1;
/// NIN_SELECT | NINF_KEY: the tray icon was activated with the keyboard.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const HOTKEY_SAVE: i32 = 1;
const TRAY_ID: u32 = 1;

const ID_SAVE: usize = 100;
const ID_CLIPS: usize = 101;
const ID_FOLDER: usize = 102;
const ID_SETTINGS: usize = 103;
const ID_PAUSE: usize = 104;
const ID_ALWAYS: usize = 105;
const ID_IGNORE: usize = 106;
const ID_EXIT: usize = 107;

pub const CLASS: PCWSTR = w!("ReplayTrayWindow");
/// After a game closes its buffer stays saveable for this long.
const RETAIN: Duration = Duration::from_secs(120);

pub const IDI_APP: u16 = 1;
const IDI_IDLE: u16 = 2;
const IDI_REC: u16 = 3;

struct ActiveGame {
    pid: u32,
    exe: String,
    name: String,
    reason: Reason,
    monitor: isize,
    watch: ProcessWatch,
}

struct Retained {
    shared: Arc<Shared>,
    name: String,
    until: Instant,
}

struct CpuMeter {
    last_wall: Instant,
    last_cpu: u64,
    cpus: u32,
    value: f64,
}

impl CpuMeter {
    fn new() -> CpuMeter {
        let mut si = SYSTEM_INFO::default();
        unsafe { GetSystemInfo(&mut si) };
        CpuMeter { last_wall: Instant::now(), last_cpu: process_cpu_time(), cpus: si.dwNumberOfProcessors.max(1), value: 0.0 }
    }

    fn sample(&mut self) {
        let now = Instant::now();
        let cpu = process_cpu_time();
        let wall = now.duration_since(self.last_wall).as_secs_f64();
        if wall > 0.2 {
            let used = cpu.saturating_sub(self.last_cpu) as f64 / 10_000_000.0;
            self.value = used / wall / self.cpus as f64 * 100.0;
            self.last_wall = now;
            self.last_cpu = cpu;
        }
    }
}

fn process_cpu_time() -> u64 {
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_err() {
            return 0;
        }
    }
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    ft(k) + ft(u)
}

fn process_memory() -> u64 {
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe {
        if K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS, pmc.cb).as_bool() {
            if pmc.PrivateUsage > 0 {
                pmc.PrivateUsage as u64
            } else {
                pmc.WorkingSetSize as u64
            }
        } else {
            0
        }
    }
}

pub struct App {
    hwnd: HWND,
    pub settings: Settings,
    settings_path: PathBuf,
    session: Option<Session>,
    game: Option<ActiveGame>,
    retained: Option<Retained>,
    paused: bool,
    hotkey_ok: bool,
    icon_idle: HICON,
    icon_rec: HICON,
    taskbar_created: u32,
    last_saved: Option<PathBuf>,
    last_save_at: Option<Instant>,
    hook: HWINEVENTHOOK,
    cpu: CpuMeter,
    last_tip: String,
    last_icon_rec: Option<bool>,
    shown_notes: usize,
    shown_error: Option<String>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// Runs `f` with the app state. Returns None instead of panicking if called re-entrantly
/// (e.g. from a window procedure invoked while the state is already borrowed).
pub fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

fn load_icon(id: u16) -> HICON {
    unsafe {
        LoadIconMetric(Some(ui::hinstance()), PCWSTR(id as usize as *const u16), LIM_SMALL)
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default()
    }
}

fn copy_wstr(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

pub fn clip_dir(s: &Settings) -> PathBuf {
    if s.folder.trim().is_empty() {
        default_clip_dir()
    } else {
        PathBuf::from(s.folder.trim())
    }
}

impl App {
    fn notify_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        }
    }

    fn add_tray(&mut self) {
        let mut nid = self.notify_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = self.icon_idle;
        copy_wstr(&mut nid.szTip, "Replay");
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        self.last_tip.clear();
        self.last_icon_rec = None;
    }

    fn balloon(&self, title: &str, text: &str, warning: bool) {
        let mut nid = self.notify_data();
        nid.uFlags = NIF_INFO;
        copy_wstr(&mut nid.szInfoTitle, title);
        copy_wstr(&mut nid.szInfo, text);
        nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO } | NIIF_NOSOUND;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn register_hotkey(&mut self) -> bool {
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_SAVE);
            let h = self.settings.hotkey;
            self.hotkey_ok =
                RegisterHotKey(Some(self.hwnd), HOTKEY_SAVE, HOT_KEY_MODIFIERS(h.mods) | MOD_NOREPEAT, h.vk).is_ok();
        }
        if !self.hotkey_ok {
            log!("hotkey {} is already registered by another program", ui::hotkey_name(self.settings.hotkey));
        }
        self.hotkey_ok
    }

    fn target_name(&self) -> String {
        match &self.game {
            Some(g) => g.name.clone(),
            None => "Desktop".into(),
        }
    }

    fn start_session(&mut self, monitor: isize, pid: Option<u32>) {
        if let Some(s) = self.session.take() {
            s.stop();
        }
        let s = &self.settings;
        let cfg = SessionConfig {
            monitor,
            height: s.height,
            fps: s.fps,
            quality: s.quality,
            encoder: s.encoder,
            audio: s.audio,
            mic: s.mic,
            game_pid: pid,
            buffer_secs: s.buffer_secs,
            notify: self.hwnd.0 as isize,
        };
        self.session = Some(Session::start(cfg));
        self.retained = None;
        self.shown_notes = 0;
        self.shown_error = None;
    }

    fn stop_session(&mut self, retain_as: Option<String>) {
        if let Some(s) = self.session.take() {
            let shared = s.stop();
            if let Some(name) = retain_as {
                if shared.buffered_secs() >= 1.0 {
                    self.retained = Some(Retained { shared, name, until: Instant::now() + RETAIN });
                }
            }
        }
    }

    /// Game detection and session lifecycle. Cheap: a handful of syscalls.
    fn detect(&mut self) {
        if self.retained.as_ref().map(|r| Instant::now() > r.until).unwrap_or(false) {
            self.retained = None;
        }
        if self.paused {
            return;
        }
        match self.settings.mode {
            Mode::Always => {
                if self.game.is_some() {
                    self.game = None;
                    self.stop_session(None);
                }
                if self.session.is_none() {
                    self.start_session(primary_monitor().0 as isize, None);
                }
            }
            Mode::Games => {
                if self.game.is_none() && self.session.is_some() {
                    self.stop_session(None); // switched away from "always" mode
                }
                if let Some(g) = &self.game {
                    if !g.watch.alive() {
                        log!("game exited: {}", g.name);
                        let name = g.name.clone();
                        self.game = None;
                        self.stop_session(Some(name));
                    } else if let Some(fg) = foreground() {
                        if fg.pid == g.pid && fg.monitor.0 as isize != g.monitor {
                            log!("game moved to another display; restarting capture");
                            let (pid, mon) = (g.pid, fg.monitor.0 as isize);
                            if let Some(g) = self.game.as_mut() {
                                g.monitor = mon;
                            }
                            self.start_session(mon, Some(pid));
                        }
                    }
                }
                if self.game.is_none() {
                    if let Some(fg) = foreground() {
                        if let Some(m) = classify(&fg.exe, fg.fullscreen, &self.settings.games, &self.settings.ignored) {
                            if let Some(watch) = ProcessWatch::new(fg.pid) {
                                log!("game detected: {} ({}, {:?})", m.name, m.exe, m.reason);
                                let monitor = fg.monitor.0 as isize;
                                self.game = Some(ActiveGame { pid: fg.pid, exe: m.exe, name: m.name, reason: m.reason, monitor, watch });
                                self.start_session(monitor, Some(fg.pid));
                            }
                        }
                    }
                }
            }
        }
    }

    fn headline(&self) -> String {
        if self.paused {
            return "Paused".into();
        }
        if let Some(s) = &self.session {
            if let Some(e) = s.shared.error.lock().unwrap().as_ref() {
                return format!("Problem: {e}");
            }
            return format!("Recording {}", self.target_name());
        }
        if let Some(r) = &self.retained {
            let left = r.until.saturating_duration_since(Instant::now()).as_secs();
            return format!("{} closed; its replay can be saved for {}", r.name, human_secs(left as f64));
        }
        match self.settings.mode {
            Mode::Games => "Waiting for a game".into(),
            Mode::Always => "Starting".into(),
        }
    }

    fn update_tray(&mut self) {
        let recording = self.session.as_ref().map(|s| s.shared.error.lock().unwrap().is_none()).unwrap_or(false);
        let mut tip = format!("Replay: {}", self.headline());
        if tip.chars().count() > 120 {
            tip = tip.chars().take(117).collect::<String>() + "...";
        }
        if tip == self.last_tip && self.last_icon_rec == Some(recording) {
            return;
        }
        let mut nid = self.notify_data();
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = if recording { self.icon_rec } else { self.icon_idle };
        copy_wstr(&mut nid.szTip, &tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        self.last_tip = tip;
        self.last_icon_rec = Some(recording);
    }

    /// Shows new session errors/notes once each.
    fn on_session_event(&mut self) {
        let Some(s) = &self.session else { return };
        let err = s.shared.error.lock().unwrap().clone();
        if let Some(e) = &err {
            if self.shown_error.as_ref() != Some(e) {
                self.balloon("Replay can't record right now", e, true);
            }
        }
        self.shown_error = err;
        let notes = s.shared.notes.lock().unwrap().clone();
        if notes.len() > self.shown_notes {
            self.balloon("Replay", &notes[self.shown_notes..].join("\n"), true);
            self.shown_notes = notes.len();
        }
        self.update_tray();
    }

    fn saveable(&self) -> Option<(Arc<Shared>, String)> {
        if let Some(s) = &self.session {
            return Some((s.shared.clone(), self.target_name()));
        }
        self.retained.as_ref().map(|r| (r.shared.clone(), r.name.clone()))
    }

    fn save_clip(&mut self) {
        if self.last_save_at.map(|t| t.elapsed() < Duration::from_millis(800)).unwrap_or(false) {
            return; // ignore key bounce / double presses
        }
        let Some((shared, name)) = self.saveable() else {
            sound::error();
            let hint = match self.settings.mode {
                Mode::Games if self.paused => "Recording is paused. Resume it from the tray menu.".to_string(),
                Mode::Games => "Replay records automatically while a game is running. Turn on \"Always record\" in the tray menu to record the desktop.".to_string(),
                Mode::Always => "Recording is starting; try again in a moment.".to_string(),
            };
            self.balloon("Nothing to save yet", &hint, true);
            return;
        };
        if shared.buffered_secs() < 1.0 {
            sound::error();
            let why = shared.error.lock().unwrap().clone().unwrap_or_else(|| "Recording just started; try again in a moment.".into());
            self.balloon("Nothing to save yet", &why, true);
            return;
        }
        self.last_save_at = Some(Instant::now());
        let seconds = self.settings.buffer_secs;
        let dir = clip_dir(&self.settings).join(sanitize_filename(&name));
        let estimate = buffer_bytes(shared.stats.bitrate.load(Ordering::Relaxed), shared.audio.lock().unwrap().is_some(), seconds);
        let hwnd = self.hwnd.0 as isize;
        let st = unsafe { GetLocalTime() };
        let stamp = format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond);
        let base = format!("{} {}", sanitize_filename(&name), stamp);
        std::thread::spawn(move || {
            let result = (|| -> Result<(PathBuf, crate::core::clip::ClipInfo), String> {
                std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
                let free = free_space(&dir);
                if let Some(free) = free {
                    if free < estimate + (64 << 20) {
                        return Err(format!("Not enough free disk space in {} ({} free).", dir.display(), human_bytes(free)));
                    }
                }
                let mut path = dir.join(format!("{base}.mp4"));
                let mut n = 2;
                while path.exists() {
                    path = dir.join(format!("{base} ({n}).mp4"));
                    n += 1;
                }
                let info = recorder::save(&shared, seconds, &path).map_err(|e| format!("Couldn't write the clip: {e}"))?;
                Ok((path, info))
            })();
            let outcome = Box::new(SaveOutcome { name, result });
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_SAVED, WPARAM(0), LPARAM(Box::into_raw(outcome) as isize));
            }
        });
    }

    fn on_saved(&mut self, o: SaveOutcome) {
        match o.result {
            Ok((path, info)) => {
                log!("saved {} ({:.1} s, {} bytes)", path.display(), info.seconds, info.bytes);
                if self.settings.sound {
                    sound::saved();
                }
                self.balloon(
                    &format!("Saved {} of {}", human_secs(info.seconds), o.name),
                    &format!("{} · Click to show in folder", human_bytes(info.bytes)),
                    false,
                );
                self.last_saved = Some(path);
                clips_ui::refresh();
            }
            Err(e) => {
                log!("save failed: {e}");
                sound::error();
                self.balloon("Couldn't save the clip", &e, true);
            }
        }
    }

    fn save_settings(&self) -> Result<(), String> {
        let _ = std::fs::create_dir_all(self.settings_path.parent().unwrap_or(Path::new(".")));
        let tmp = self.settings_path.with_extension("tmp");
        std::fs::write(&tmp, self.settings.serialize())
            .and_then(|_| std::fs::rename(&tmp, &self.settings_path))
            .map_err(|e| format!("Couldn't save settings to {}: {e}", self.settings_path.display()))
    }

    fn apply(&mut self, new: Settings) -> Result<(), String> {
        if new.hotkey != self.settings.hotkey || !self.hotkey_ok {
            let old = self.settings.hotkey;
            self.settings.hotkey = new.hotkey;
            if !self.register_hotkey() {
                self.settings.hotkey = old;
                self.register_hotkey();
                return Err(format!(
                    "{} is already used by another program. Choose a different hotkey.",
                    ui::hotkey_name(new.hotkey)
                ));
            }
        }
        let restart = new.pipeline_differs(&self.settings);
        self.settings = new;
        self.save_settings()?;
        if restart {
            if let Some(s) = &self.session {
                let (monitor, pid) = (s.cfg.monitor, s.cfg.game_pid);
                self.start_session(monitor, pid);
            }
        }
        self.detect();
        self.update_tray();
        Ok(())
    }

    fn shutdown(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_TICK);
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_SAVE);
            if !self.hook.is_invalid() {
                let _ = UnhookWinEvent(self.hook);
            }
            let nid = self.notify_data();
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
        self.stop_session(None);
        self.retained = None;
    }

    fn build_menu(&self) -> HMENU {
        unsafe {
            let m = CreatePopupMenu().unwrap_or_default();
            let add = |flags: MENU_ITEM_FLAGS, id: usize, text: &str| {
                let t = wide(text);
                let _ = AppendMenuW(m, flags, id, PCWSTR(t.as_ptr()));
            };
            add(MF_STRING | MF_GRAYED, 0, &self.headline());
            add(MF_SEPARATOR, 0, "");
            let save_label = if self.hotkey_ok {
                format!("Save clip\t{}", ui::hotkey_name(self.settings.hotkey))
            } else {
                "Save clip".to_string()
            };
            add(if self.saveable().is_some() { MF_STRING } else { MF_STRING | MF_GRAYED }, ID_SAVE, &save_label);
            add(MF_STRING, ID_CLIPS, "Clips...");
            add(MF_STRING, ID_FOLDER, "Open clips folder");
            add(MF_SEPARATOR, 0, "");
            add(
                if self.settings.mode == Mode::Always { MF_STRING | MF_CHECKED } else { MF_STRING },
                ID_ALWAYS,
                "Always record (whole screen)",
            );
            if let Some(g) = &self.game {
                if g.reason == Reason::Fullscreen {
                    add(MF_STRING, ID_IGNORE, &format!("Don't record {}", g.exe));
                }
            }
            add(MF_STRING, ID_PAUSE, if self.paused { "Resume recording" } else { "Pause recording" });
            add(MF_STRING, ID_SETTINGS, "Settings...");
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, ID_EXIT, "Exit Replay");
            m
        }
    }

    /// Status shown live in the Settings window.
    pub fn status_lines(&self) -> (String, String) {
        let mut first = self.headline();
        let mut second = String::new();
        let shared = self.session.as_ref().map(|s| s.shared.clone()).or_else(|| self.retained.as_ref().map(|r| r.shared.clone()));
        if let Some(sh) = shared {
            let st = &sh.stats;
            let (w, h) = (st.width.load(Ordering::Relaxed), st.height.load(Ordering::Relaxed));
            let enc = st.encoder.lock().unwrap().clone();
            if w > 0 && self.session.is_some() && sh.error.lock().unwrap().is_none() {
                first = format!("{first} at {w}x{h}, {} fps, {} Mbps ({enc})", self.settings.fps, st.bitrate.load(Ordering::Relaxed) / 1_000_000);
            }
            let ring_bytes = sh.ring.lock().unwrap().bytes() as u64;
            second = format!(
                "Replay buffer {} ({}) · {} frames dropped · ",
                human_secs(sh.buffered_secs()),
                human_bytes(ring_bytes),
                st.dropped.load(Ordering::Relaxed)
            );
            let notes = sh.notes.lock().unwrap();
            if !notes.is_empty() {
                first = format!("{first}. {}", notes.join(". "));
            }
        }
        second.push_str(&format!("Replay is using {:.1}% CPU and {} of memory", self.cpu.value, human_bytes(process_memory())));
        (first, second)
    }
}

struct SaveOutcome {
    name: String,
    result: Result<(PathBuf, crate::core::clip::ClipInfo), String>,
}

fn free_space(dir: &Path) -> Option<u64> {
    let w = wide(&dir.to_string_lossy());
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None).ok()? };
    Some(free)
}

// ---------------------------------------------------------------------------------------
// Start with Windows

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Replay");

pub fn autostart_enabled() -> bool {
    unsafe {
        let mut size = 0u32;
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok()
    }
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None)
            .ok()
            .map_err(|e| format!("Couldn't open the startup registry key: {e}"))?;
        let r = if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let cmd = wide(&format!("\"{}\" --background", exe.display()));
            let bytes = std::slice::from_raw_parts(cmd.as_ptr() as *const u8, cmd.len() * 2);
            RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes)).ok()
        } else {
            match RegDeleteValueW(key, RUN_VALUE) {
                e if e == ERROR_FILE_NOT_FOUND => Ok(()),
                e => e.ok(),
            }
        };
        let _ = RegCloseKey(key);
        r.map_err(|e| format!("Couldn't change the startup setting: {e}"))
    }
}

// ---------------------------------------------------------------------------------------
// Public entry points used by the other windows

pub fn apply_settings(new: Settings) -> Result<(), String> {
    with_app(|a| a.apply(new)).unwrap_or_else(|| Err("Replay is busy; try again.".into()))
}

pub fn status_lines() -> Option<(String, String)> {
    with_app(|a| a.status_lines())
}

pub fn current_settings() -> Option<Settings> {
    with_app(|a| a.settings.clone())
}

fn open_settings() {
    if let Some((hwnd, s)) = with_app(|a| (a.hwnd, a.settings.clone())) {
        settings_ui::open(hwnd, s);
    }
}

fn open_clips() {
    if let Some(dir) = with_app(|a| clip_dir(&a.settings)) {
        clips_ui::open(dir);
    }
}

fn on_command(id: usize) {
    let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed) as *mut _);
    match id {
        ID_SAVE => {
            with_app(|a| a.save_clip());
        }
        ID_CLIPS => open_clips(),
        ID_FOLDER => {
            if let Some(dir) = with_app(|a| clip_dir(&a.settings)) {
                let _ = std::fs::create_dir_all(&dir);
                ui::open_path(&dir);
            }
        }
        ID_SETTINGS => open_settings(),
        ID_PAUSE => {
            with_app(|a| {
                a.paused = !a.paused;
                if a.paused {
                    let name = a.target_name();
                    a.game = None;
                    a.stop_session(Some(name));
                } else {
                    a.detect();
                }
                a.update_tray();
            });
        }
        ID_ALWAYS => {
            if let Some(mut s) = current_settings() {
                s.mode = if s.mode == Mode::Always { Mode::Games } else { Mode::Always };
                if let Err(e) = apply_settings(s) {
                    ui::message_box(hwnd, &e, "Replay", MB_ICONWARNING);
                }
                settings_ui::reload();
            }
        }
        ID_IGNORE => {
            let ignored = with_app(|a| {
                let exe = a.game.as_ref().map(|g| g.exe.to_lowercase())?;
                a.settings.ignored.push(exe.clone());
                a.settings.ignored.sort();
                a.settings.ignored.dedup();
                a.game = None;
                a.stop_session(None);
                let _ = a.save_settings();
                a.update_tray();
                Some(exe)
            })
            .flatten();
            if let Some(exe) = ignored {
                log!("added {exe} to the never-record list");
                settings_ui::reload();
            }
        }
        ID_EXIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

fn show_tray_menu(hwnd: HWND) {
    let Some(menu) = with_app(|a| a.build_menu()) else { return };
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(menu, (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY).0, pt.x, pt.y, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        if cmd.0 != 0 {
            on_command(cmd.0 as usize);
        }
    }
}

unsafe extern "system" fn on_foreground_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    let h = MAIN_HWND.load(Ordering::Relaxed);
    if h != 0 {
        let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_FOREGROUND, WPARAM(0), LPARAM(0));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == TIMER_TICK => {
            with_app(|a| {
                a.cpu.sample();
                a.detect();
                a.update_tray();
            });
            LRESULT(0)
        }
        WM_APP_FOREGROUND => {
            with_app(|a| {
                a.detect();
                a.update_tray();
            });
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY_SAVE => {
            with_app(|a| a.save_clip());
            LRESULT(0)
        }
        WM_APP_SAVED => {
            let outcome = Box::from_raw(lp.0 as *mut SaveOutcome);
            with_app(|a| a.on_saved(*outcome));
            LRESULT(0)
        }
        WM_APP_SESSION => {
            with_app(|a| a.on_session_event());
            LRESULT(0)
        }
        WM_APP_SHOW => {
            open_settings();
            LRESULT(0)
        }
        WM_APP_SHOW_CLIPS => {
            open_clips();
            LRESULT(0)
        }
        WM_APP_TRAY => {
            let event = (lp.0 & 0xFFFF) as u32;
            match event {
                WM_CONTEXTMENU => show_tray_menu(hwnd),
                NIN_SELECT | NIN_KEYSELECT => open_clips(),
                NIN_BALLOONUSERCLICK => {
                    if let Some(Some(p)) = with_app(|a| a.last_saved.clone()) {
                        ui::show_in_folder(&p);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            with_app(|a| a.shutdown());
            settings_ui::close();
            clips_ui::close();
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wp.0 != 0 {
                with_app(|a| a.shutdown());
            }
            LRESULT(0)
        }
        _ => {
            let recreated = with_app(|a| {
                if msg == a.taskbar_created && msg != 0 {
                    // Explorer restarted: the tray icon must be added again.
                    a.add_tray();
                    a.update_tray();
                    true
                } else {
                    false
                }
            });
            if recreated == Some(true) {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

/// `background`: started at sign-in (never opens windows). `clips`: open the Clips window.
pub fn run(background: bool, clips: bool) -> i32 {
    init_log(false);
    log!("Replay {} starting", env!("CARGO_PKG_VERSION"));
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let _com = ComGuard::new(COINIT_APARTMENTTHREADED);
    let _mf = MfGuard::new();
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_HOTKEY_CLASS | ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }

    // Single instance: a second launch opens the running instance's settings.
    let _mutex = unsafe { CreateMutexW(None, true, w!("Local\\ReplayInstantReplayRecorder")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            if let Ok(h) = FindWindowW(CLASS, PCWSTR::null()) {
                if !background {
                    let msg = if clips { WM_APP_SHOW_CLIPS } else { WM_APP_SHOW };
                    let _ = PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0));
                }
            }
        }
        return 0;
    }

    let settings_path = data_dir().join("settings.ini");
    let first_run = !settings_path.exists();
    let settings = std::fs::read_to_string(&settings_path).map(|t| Settings::parse(&t)).unwrap_or_default();

    let hwnd = unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: ui::hinstance(),
            lpszClassName: CLASS,
            hIcon: load_icon(IDI_APP),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Replay"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(ui::hinstance()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                log!("could not create the main window: {e}");
                return 1;
            }
        }
    };
    MAIN_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    let app = App {
        hwnd,
        settings,
        settings_path,
        session: None,
        game: None,
        retained: None,
        paused: false,
        hotkey_ok: false,
        icon_idle: load_icon(IDI_IDLE),
        icon_rec: load_icon(IDI_REC),
        taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        last_saved: None,
        last_save_at: None,
        hook: HWINEVENTHOOK::default(),
        cpu: CpuMeter::new(),
        last_tip: String::new(),
        last_icon_rec: None,
        shown_notes: 0,
        shown_error: None,
    };
    APP.with(|c| *c.borrow_mut() = Some(app));

    with_app(|a| {
        a.add_tray();
        let hotkey_ok = a.register_hotkey();
        unsafe {
            SetTimer(Some(hwnd), TIMER_TICK, 1000, None);
            a.hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(on_foreground_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
        }
        if !hotkey_ok {
            a.balloon(
                "Hotkey unavailable",
                &format!("{} is used by another program. Choose a different hotkey in Settings.", ui::hotkey_name(a.settings.hotkey)),
                true,
            );
        } else if first_run {
            let mode = match a.settings.mode {
                Mode::Games => "while you play",
                Mode::Always => "continuously",
            };
            a.balloon(
                "Replay is running",
                &format!(
                    "It records {mode} and keeps the last {}. Press {} to save a clip. Right-click this icon for options.",
                    duration_words(a.settings.buffer_secs),
                    ui::hotkey_name(a.settings.hotkey)
                ),
                false,
            );
        }
        if first_run {
            let _ = a.save_settings();
        }
        a.detect();
        a.update_tray();
    });
    if !background && (first_run || clips) {
        let msg = if clips { WM_APP_SHOW_CLIPS } else { WM_APP_SHOW };
        unsafe {
            let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
        }
    }

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if settings_ui::is_dialog_message(&msg) || clips_ui::is_dialog_message(&msg) {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    APP.with(|c| c.borrow_mut().take());
    log!("Replay exited");
    0
}
