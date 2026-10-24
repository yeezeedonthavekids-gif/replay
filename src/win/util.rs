//! Small Windows helpers: strings, clock, logging, paths, COM/MF lifetime.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_LITE, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

pub unsafe fn pwstr_to_string(p: PWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        p.to_string().unwrap_or_default()
    }
}

/// Wall clock in 100 ns units (QPC based; same clock WASAPI uses for packet positions).
pub fn now_hns() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    });
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    ((c as i128 * 10_000_000) / freq as i128) as i64
}

pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = pwstr_to_string(p);
        CoTaskMemFree(Some(p.0 as *const _));
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
}

/// `%LOCALAPPDATA%\Replay` (settings and log).
pub fn data_dir() -> PathBuf {
    let base = known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("Replay")
}

pub fn default_clip_dir() -> PathBuf {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_Videos)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Videos")))
        .unwrap_or_else(std::env::temp_dir)
        .join("Replay")
}

// ---------------------------------------------------------------------------------------
// Logging

static LOG: Mutex<Option<File>> = Mutex::new(None);
static ECHO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn init_log(echo_to_console: bool) {
    ECHO.store(echo_to_console, std::sync::atomic::Ordering::Relaxed);
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("replay.log");
    let too_big = std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false);
    let file = if too_big {
        File::create(&path)
    } else {
        OpenOptions::new().create(true).append(true).open(&path)
    };
    if let Ok(f) = file {
        *LOG.lock().unwrap() = Some(f);
    }
}

pub fn log_line(msg: &str) {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {}\n",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds, msg
    );
    if ECHO.load(std::sync::atomic::Ordering::Relaxed) {
        eprint!("{line}");
    }
    if let Ok(mut g) = LOG.lock() {
        if let Some(f) = g.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::win::util::log_line(&format!($($t)*)) };
}

// ---------------------------------------------------------------------------------------
// COM / Media Foundation lifetime

pub struct ComGuard(bool);

impl ComGuard {
    pub fn new(mode: COINIT) -> ComGuard {
        let ok = unsafe { CoInitializeEx(None, mode).is_ok() };
        ComGuard(ok)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub struct MfGuard(bool);

impl MfGuard {
    pub fn new() -> MfGuard {
        MfGuard(unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).is_ok() })
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                let _ = MFShutdown();
            }
        }
    }
}

/// Formats an HRESULT error compactly for logs and UI.
pub fn err_text(e: &windows::core::Error) -> String {
    let msg = e.message();
    let msg = msg.trim();
    if msg.is_empty() {
        format!("0x{:08X}", e.code().0 as u32)
    } else {
        format!("{msg} (0x{:08X})", e.code().0 as u32)
    }
}

pub fn human_bytes(n: u64) -> String {
    if n == 0 {
        "0 KB".into()
    } else if n >= 1 << 30 {
        format!("{:.1} GB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.0} MB", n as f64 / (1u64 << 20) as f64)
    } else {
        format!("{:.0} KB", (n as f64 / 1024.0).max(1.0))
    }
}

/// "30 seconds", "1 minute", "1.5 minutes" (for settings-style durations).
pub fn duration_words(secs: u32) -> String {
    match secs {
        s if s < 60 => format!("{s} seconds"),
        60 => "1 minute".into(),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{} minutes", s as f64 / 60.0),
    }
}

/// Clock-style length: "0:31", "1:02".
pub fn clock(s: f64) -> String {
    let s = s.round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn human_secs(s: f64) -> String {
    let s = s.round() as u64;
    if s >= 60 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}
