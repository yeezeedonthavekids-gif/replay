//! A recording session: one video thread (capture -> convert -> encode) and one audio
//! thread, both feeding the shared replay ring.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::COINIT_MULTITHREADED;
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
    INFINITE, TIMER_ALL_ACCESS,
};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use super::audio::{self, SourceKind};
use super::capture::{open_gpu_for_monitor, Capture};
use super::encoder::{EncodedFrame, H264Encoder};
use super::util::{err_text, now_hns, ComGuard};
use crate::core::clip::{self, ClipInfo, VideoParams};
use crate::core::h264;
use crate::core::ring::{Packet, PacketRing, HNS};
use crate::core::settings::{output_size, video_bitrate, AudioMode, EncoderPref, Quality};

/// Posted to the app window when a session's status changes (error, note, encoder info).
pub const WM_APP_SESSION: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 3;

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub monitor: isize,
    pub height: u32,
    pub fps: u32,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub audio: AudioMode,
    pub mic: bool,
    pub game_pid: Option<u32>,
    /// Game window to capture on its own; 0 = capture the whole monitor.
    pub window: isize,
    pub buffer_secs: u32,
    pub notify: isize,
}

#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub dropped: AtomicU64,
    pub video_bytes: AtomicU64,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub bitrate: AtomicU32,
    pub encoder: Mutex<String>,
    /// "game window" or "whole display".
    pub source: Mutex<&'static str>,
}

pub struct Shared {
    pub ring: Mutex<PacketRing>,
    pub video: Mutex<Option<VideoParams>>,
    pub audio: Mutex<Option<clip::AudioParams>>,
    pub stop: AtomicBool,
    pub stats: Stats,
    pub start_hns: i64,
    /// Last error (video) — cleared when recording recovers.
    pub error: Mutex<Option<String>>,
    /// Non-fatal notes such as "Microphone unavailable".
    pub notes: Mutex<Vec<String>>,
    notify: isize,
}

impl Shared {
    fn post(&self) {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.notify as *mut _)), WM_APP_SESSION, WPARAM(0), LPARAM(0));
        }
    }

    pub fn set_error(&self, e: Option<String>) {
        let mut g = self.error.lock().unwrap();
        if *g != e {
            *g = e;
            drop(g);
            self.post();
        }
    }

    pub fn set_note(&self, n: &str) {
        let mut g = self.notes.lock().unwrap();
        if !g.iter().any(|x| x == n) {
            g.push(n.to_string());
            drop(g);
            self.post();
        }
    }

    /// Seconds of video currently saveable.
    pub fn buffered_secs(&self) -> f64 {
        self.ring.lock().unwrap().video_span() as f64 / HNS as f64
    }
}

pub struct Session {
    pub shared: Arc<Shared>,
    /// Each thread drops its `Sender` on exit, which lets `stop` wait with a timeout.
    threads: Vec<(JoinHandle<()>, mpsc::Receiver<()>)>,
    pub cfg: SessionConfig,
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> (JoinHandle<()>, mpsc::Receiver<()>) {
    let (tx, rx) = mpsc::channel::<()>();
    let handle = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _done = tx;
            f()
        })
        .expect("spawn thread");
    (handle, rx)
}

/// Memory cap for the ring: a generous multiple of what the bitrate implies, so a
/// misbehaving encoder can never consume unbounded memory.
fn ring_cap(cfg: &SessionConfig) -> usize {
    let bps = video_bitrate(3840, 2160, cfg.fps, cfg.quality) as u64;
    (bps / 8 * (cfg.buffer_secs as u64 + 2) * 2).min(2 << 30) as usize
}

impl Session {
    pub fn start(cfg: SessionConfig) -> Session {
        let shared = Arc::new(Shared {
            ring: Mutex::new(PacketRing::new(cfg.buffer_secs as i64 * HNS, ring_cap(&cfg))),
            video: Mutex::new(None),
            audio: Mutex::new(None),
            stop: AtomicBool::new(false),
            stats: Stats::default(),
            start_hns: now_hns(),
            error: Mutex::new(None),
            notes: Mutex::new(Vec::new()),
            notify: cfg.notify,
        });
        let mut threads = Vec::new();
        {
            let shared = shared.clone();
            let cfg = cfg.clone();
            threads.push(spawn("replay-video", move || video_main(shared, cfg)));
        }
        let sources: Vec<SourceKind> = match (cfg.audio, cfg.game_pid) {
            (AudioMode::None, _) => vec![],
            (AudioMode::Game, Some(pid)) => vec![SourceKind::Process(pid)],
            _ => vec![SourceKind::System],
        };
        let mut sources = sources;
        if cfg.mic {
            sources.push(SourceKind::Mic);
        }
        if !sources.is_empty() {
            let shared = shared.clone();
            threads.push(spawn("replay-audio", move || {
                let _com = ComGuard::new(COINIT_MULTITHREADED);
                audio::run(shared, sources)
            }));
        }
        log!("session started: {:?}", cfg);
        Session { shared, threads, cfg }
    }

    /// Stops capture. The returned state still holds the buffer, so clips can be saved after
    /// the game has closed.
    pub fn stop(self) -> Arc<Shared> {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Never let a driver call that hangs freeze the app: wait a bounded time, then detach.
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        for (handle, done) in self.threads {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match done.recv_timeout(left) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    log!("thread '{}' did not stop in time; detaching it", handle.thread().name().unwrap_or("?"));
                }
                _ => {
                    let _ = handle.join();
                }
            }
        }
        log!("session stopped");
        self.shared
    }
}

/// High-resolution sleep for frame pacing (falls back to a 1 ms system timer).
struct Pacer {
    timer: HANDLE,
    high_res: bool,
}

impl Pacer {
    fn new() -> Pacer {
        unsafe {
            match CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) {
                Ok(h) => Pacer { timer: h, high_res: true },
                Err(_) => {
                    timeBeginPeriod(1);
                    let h = CreateWaitableTimerExW(None, None, 0, TIMER_ALL_ACCESS.0).unwrap_or_default();
                    Pacer { timer: h, high_res: false }
                }
            }
        }
    }

    fn sleep(&self, hns: i64) {
        if hns <= 0 {
            return;
        }
        unsafe {
            let due = -hns;
            if !self.timer.is_invalid() && SetWaitableTimer(self.timer, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.timer, INFINITE);
            } else {
                std::thread::sleep(Duration::from_nanos(hns as u64 * 100));
            }
        }
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        unsafe {
            if !self.timer.is_invalid() {
                let _ = windows::Win32::Foundation::CloseHandle(self.timer);
            }
            if !self.high_res {
                timeEndPeriod(1);
            }
        }
    }
}

fn video_main(shared: Arc<Shared>, cfg: SessionConfig) {
    let _com = ComGuard::new(COINIT_MULTITHREADED);
    let mut failures = 0u32;
    while !shared.stop.load(Ordering::Relaxed) {
        match run_video(&shared, &cfg, &mut failures) {
            Ok(()) => break,
            Err(e) => {
                failures += 1;
                log!("video pipeline error (attempt {failures}): {e}");
                shared.set_error(Some(e));
                shared.ring.lock().unwrap().clear_video();
                *shared.video.lock().unwrap() = None;
                let wait = Duration::from_millis((500 * failures as u64).min(5000));
                let until = std::time::Instant::now() + wait;
                while std::time::Instant::now() < until && !shared.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

fn handle_frames(shared: &Shared, frames: &mut Vec<EncodedFrame>, enc: &H264Encoder, w: u32, h: u32, frame_dur: i64) {
    for f in frames.drain(..) {
        let au = h264::annexb_to_avcc(&f.data);
        if au.avcc.is_empty() {
            continue;
        }
        let key = f.key || au.idr;
        if key {
            let (sps, pps) = match (au.sps, au.pps) {
                (Some(s), Some(p)) => (Some(s), Some(p)),
                _ => {
                    // Encoders that only signal parameter sets in the media type.
                    let mut s = None;
                    let mut p = None;
                    if shared.video.lock().unwrap().is_none() {
                        if let Some(hdr) = enc.sequence_header() {
                            for nal in h264::nal_units(&hdr) {
                                match h264::nal_type(nal) {
                                    h264::NAL_SPS => s = Some(nal.to_vec()),
                                    h264::NAL_PPS => p = Some(nal.to_vec()),
                                    _ => {}
                                }
                            }
                        }
                    }
                    (s, p)
                }
            };
            if let (Some(sps), Some(pps)) = (sps, pps) {
                if let Some(avcc) = h264::avcc_config(&sps, &pps) {
                    let mut v = shared.video.lock().unwrap();
                    if v.as_ref().map(|p| p.avcc != avcc).unwrap_or(true) {
                        if v.is_some() {
                            // Parameters changed mid-stream: old packets can't share a file with new ones.
                            shared.ring.lock().unwrap().clear_video();
                        }
                        *v = Some(VideoParams { width: w, height: h, avcc });
                    }
                }
            }
        }
        if shared.video.lock().unwrap().is_none() {
            continue; // can't use packets before we know the stream parameters
        }
        shared.stats.video_bytes.fetch_add(au.avcc.len() as u64, Ordering::Relaxed);
        shared.ring.lock().unwrap().push_video(Packet { pts: f.pts, dur: frame_dur, key, data: Arc::from(au.avcc) });
    }
}

fn run_video(shared: &Shared, cfg: &SessionConfig, failures: &mut u32) -> Result<(), String> {
    let monitor = HMONITOR(cfg.monitor as *mut _);
    log!("video: opening GPU");
    let (gpu, output) = open_gpu_for_monitor(monitor).map_err(|e| format!("Could not open the GPU: {}", err_text(&e)))?;
    let adapter = gpu.adapter_name.clone();
    log!("video: starting screen capture on {adapter}");
    let display_error = |e: windows::core::Error| {
        if e.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED {
            format!("This display can't be captured on {adapter} (hybrid-GPU laptops: set Replay to the GPU that drives this display in Windows Graphics settings)")
        } else {
            format!("Screen capture unavailable: {}", err_text(&e))
        }
    };
    let mut cap = if cfg.window != 0 {
        let hwnd = windows::Win32::Foundation::HWND(cfg.window as *mut _);
        match Capture::new_window(gpu, output.clone(), hwnd) {
            Ok(c) => {
                log!("video: capturing the game window only");
                c
            }
            Err(e) => {
                // Older Windows (before 1903) or a window that refuses capture.
                log!("video: window capture unavailable ({}); capturing the whole display", err_text(&e));
                shared.set_note("Game-window capture isn't available here, so the whole display is recorded");
                let (gpu, output) = open_gpu_for_monitor(monitor).map_err(|e| format!("Could not open the GPU: {}", err_text(&e)))?;
                Capture::new_display(gpu, output).map_err(display_error)?
            }
        }
    } else {
        Capture::new_display(gpu, output).map_err(display_error)?
    };
    let (src_w, src_h) = cap.source_size().map_err(|e| err_text(&e))?;
    let (w, h) = output_size(src_w, src_h, cfg.height);
    let fps = cfg.fps.max(1);
    let bitrate = video_bitrate(w, h, fps, cfg.quality);
    log!("video: source {src_w}x{src_h}, encoding {w}x{h}@{fps}");
    let mut enc = H264Encoder::new(&cap.gpu, w, h, fps, bitrate, cfg.encoder == EncoderPref::Auto)?;
    cap.set_output(w, h, fps).map_err(|e| format!("Could not allocate video frames: {}", err_text(&e)))?;

    *shared.stats.encoder.lock().unwrap() =
        format!("{}{}", enc.name, if enc.hardware { "" } else { " (software)" });
    *shared.stats.source.lock().unwrap() = if cap.is_window() { "game window only" } else { "whole display" };
    shared.stats.width.store(w, Ordering::Relaxed);
    shared.stats.height.store(h, Ordering::Relaxed);
    shared.stats.bitrate.store(bitrate, Ordering::Relaxed);
    if !enc.hardware && cfg.encoder == EncoderPref::Auto {
        shared.set_note("No hardware encoder available; using the software encoder (higher CPU use)");
    }
    shared.set_error(None);
    shared.post();

    let pacer = Pacer::new();
    let frame_dur = HNS / fps as i64;
    let mut tick = (now_hns() - shared.start_hns) / frame_dur + 1;
    let mut frames = Vec::new();
    let running_since = std::time::Instant::now();

    while !shared.stop.load(Ordering::Relaxed) {
        let due = tick * frame_dur;
        pacer.sleep(due - (now_hns() - shared.start_hns));
        let late = now_hns() - shared.start_hns - due;
        if late > frame_dur * 2 {
            // Fell behind (system stall): skip the missed frame slots instead of bursting.
            let skip = late / frame_dur;
            shared.stats.dropped.fetch_add(skip as u64, Ordering::Relaxed);
            tick += skip;
        }
        let pts = tick * frame_dur;
        tick += 1;

        cap.grab().map_err(|e| format!("Screen capture failed: {}", err_text(&e)))?;
        let tex = cap.convert().map_err(|e| format!("Video conversion failed: {}", err_text(&e)))?;
        if let Some(tex) = tex {
            let accepted = enc.submit(&tex, pts, frame_dur, &mut frames).map_err(|e| format!("Encoder failed: {}", err_text(&e)))?;
            if accepted {
                shared.stats.frames.fetch_add(1, Ordering::Relaxed);
            } else {
                shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        enc.poll(&mut frames).map_err(|e| format!("Encoder failed: {}", err_text(&e)))?;
        handle_frames(shared, &mut frames, &enc, w, h, frame_dur);

        // After 30 s of healthy recording, the next failure retries quickly again.
        if *failures > 0 && running_since.elapsed() > Duration::from_secs(30) {
            *failures = 0;
        }
    }
    Ok(())
}

/// Writes the last `seconds` of `shared` to `path`.
pub fn save(shared: &Shared, seconds: u32, path: &Path) -> std::io::Result<ClipInfo> {
    let clip = shared.ring.lock().unwrap().snapshot(seconds as i64);
    let video = shared.video.lock().unwrap().clone();
    let audio = shared.audio.lock().unwrap().clone();
    clip::write_clip(path, &clip, video.as_ref(), audio.as_ref())
}
