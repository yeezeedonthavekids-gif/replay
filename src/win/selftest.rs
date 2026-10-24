//! `replay.exe --selftest [seconds] [out.mp4]`: records the primary display for a few
//! seconds without any UI, saves a clip, and prints a report. Used to verify the capture
//! and encoding pipeline on a given machine (and in CI).

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::COINIT_MULTITHREADED;
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

use super::detect::primary_monitor;
use super::recorder::{save, Session, SessionConfig};
use super::util::{init_log, ComGuard, MfGuard};
use crate::core::settings::{AudioMode, EncoderPref, Quality};

pub fn run(args: &[String]) -> i32 {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    init_log(true);
    let _com = ComGuard::new(COINIT_MULTITHREADED);
    let _mf = MfGuard::new();
    let seconds: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(5).clamp(1, 120);
    let out = args.get(1).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("replay-selftest.mp4"));
    let software = args.iter().any(|a| a == "--software");

    let cfg = SessionConfig {
        monitor: primary_monitor().0 as isize,
        height: 720,
        fps: 30,
        quality: Quality::Medium,
        encoder: if software { EncoderPref::Software } else { EncoderPref::Auto },
        audio: AudioMode::System,
        mic: false,
        game_pid: None,
        window: 0,
        buffer_secs: 60,
        notify: 0,
    };
    log!("selftest: recording {seconds} s");
    let session = Session::start(cfg);
    let started = Instant::now();
    // Pump messages like the tray app's UI thread does while the session runs.
    while started.elapsed() < Duration::from_secs(seconds) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let shared = session.stop();
    let st = &shared.stats;
    log!(
        "selftest: encoder='{}' frames={} dropped={} size={}x{} buffered={:.1}s",
        st.encoder.lock().unwrap(),
        st.frames.load(Ordering::Relaxed),
        st.dropped.load(Ordering::Relaxed),
        st.width.load(Ordering::Relaxed),
        st.height.load(Ordering::Relaxed),
        shared.buffered_secs()
    );
    if let Some(e) = shared.error.lock().unwrap().as_ref() {
        log!("selftest: error: {e}");
    }
    for n in shared.notes.lock().unwrap().iter() {
        log!("selftest: note: {n}");
    }
    match save(&shared, 60, &out) {
        Ok(info) => {
            log!("selftest: wrote {} ({:.2} s, {} bytes)", out.display(), info.seconds, info.bytes);
            0
        }
        Err(e) => {
            log!("selftest: FAILED to write clip: {e}");
            1
        }
    }
}
