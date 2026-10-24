//! User settings, stored as a small `key=value` text file.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Record only while a game is detected.
    Games,
    /// Always record the primary display.
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioMode {
    /// Everything playing on the default output device.
    System,
    /// Only the game process (and its children). Needs Windows 10 2004 or newer.
    Game,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderPref {
    /// Hardware encoder if one works, otherwise the Windows software encoder.
    Auto,
    Software,
}

/// Global hotkey: Win32 virtual-key code plus `MOD_*` flags (ALT=1, CONTROL=2, SHIFT=4, WIN=8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub vk: u32,
    pub mods: u32,
}

pub const MOD_ALT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_WIN: u32 = 8;

pub const BUFFER_CHOICES: [u32; 6] = [30, 45, 60, 90, 120, 180];
pub const HEIGHT_CHOICES: [u32; 4] = [0, 1440, 1080, 720];
pub const FPS_CHOICES: [u32; 3] = [30, 60, 120];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub mode: Mode,
    pub buffer_secs: u32,
    /// Output height; 0 = native resolution.
    pub height: u32,
    pub fps: u32,
    pub quality: Quality,
    pub encoder: EncoderPref,
    pub audio: AudioMode,
    pub mic: bool,
    pub hotkey: Hotkey,
    /// Clip folder; empty = `Videos\Replay`.
    pub folder: String,
    pub sound: bool,
    /// Executable names (lower case) that are always treated as games.
    pub games: Vec<String>,
    /// Executable names (lower case) that are never recorded.
    pub ignored: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::Games,
            buffer_secs: 60,
            height: 1080,
            fps: 60,
            quality: Quality::High,
            encoder: EncoderPref::Auto,
            audio: AudioMode::System,
            mic: false,
            hotkey: Hotkey { vk: 0x79, mods: MOD_ALT }, // Alt+F10
            folder: String::new(),
            sound: true,
            games: Vec::new(),
            ignored: Vec::new(),
        }
    }
}

fn list(v: &str) -> Vec<String> {
    let mut out: Vec<String> =
        v.split(';').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    out.sort();
    out.dedup();
    out
}

impl Settings {
    /// Parses settings text. Unknown keys and invalid values fall back to defaults, so a
    /// hand-edited or older file never prevents the app from starting.
    pub fn parse(text: &str) -> Settings {
        let mut s = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let num = v.parse::<u32>().ok();
            match k {
                "mode" => s.mode = if v == "always" { Mode::Always } else { Mode::Games },
                "buffer_seconds" => {
                    if let Some(n) = num.filter(|n| (10..=600).contains(n)) {
                        s.buffer_secs = n;
                    }
                }
                "height" => {
                    if let Some(n) = num.filter(|n| *n == 0 || (240..=4320).contains(n)) {
                        s.height = n;
                    }
                }
                "fps" => {
                    if let Some(n) = num.filter(|n| (10..=240).contains(n)) {
                        s.fps = n;
                    }
                }
                "quality" => {
                    s.quality = match v {
                        "low" => Quality::Low,
                        "medium" => Quality::Medium,
                        "max" => Quality::Max,
                        _ => Quality::High,
                    }
                }
                "encoder" => s.encoder = if v == "software" { EncoderPref::Software } else { EncoderPref::Auto },
                "audio" => {
                    s.audio = match v {
                        "game" => AudioMode::Game,
                        "none" => AudioMode::None,
                        _ => AudioMode::System,
                    }
                }
                "microphone" => s.mic = v == "1" || v == "true",
                "hotkey" => {
                    if let Some((vk, mods)) = v.split_once(',') {
                        let vk = u32::from_str_radix(vk.trim().trim_start_matches("0x"), 16).ok();
                        let mods = mods.trim().parse::<u32>().ok();
                        if let (Some(vk), Some(mods)) = (vk, mods) {
                            if (1..=0xFE).contains(&vk) && mods <= 15 {
                                s.hotkey = Hotkey { vk, mods };
                            }
                        }
                    }
                }
                "folder" => s.folder = v.to_string(),
                "sound" => s.sound = v != "0" && v != "false",
                "games" => s.games = list(v),
                "ignored" => s.ignored = list(v),
                _ => {}
            }
        }
        s
    }

    pub fn serialize(&self) -> String {
        format!(
            "# Replay settings\n\
             mode={}\nbuffer_seconds={}\nheight={}\nfps={}\nquality={}\nencoder={}\naudio={}\n\
             microphone={}\nhotkey=0x{:02X},{}\nfolder={}\nsound={}\ngames={}\nignored={}\n",
            match self.mode {
                Mode::Games => "games",
                Mode::Always => "always",
            },
            self.buffer_secs,
            self.height,
            self.fps,
            match self.quality {
                Quality::Low => "low",
                Quality::Medium => "medium",
                Quality::High => "high",
                Quality::Max => "max",
            },
            match self.encoder {
                EncoderPref::Auto => "auto",
                EncoderPref::Software => "software",
            },
            match self.audio {
                AudioMode::System => "system",
                AudioMode::Game => "game",
                AudioMode::None => "none",
            },
            self.mic as u8,
            self.hotkey.vk,
            self.hotkey.mods,
            self.folder,
            self.sound as u8,
            self.games.join(";"),
            self.ignored.join(";"),
        )
    }

    /// True if a change requires restarting the capture pipeline.
    pub fn pipeline_differs(&self, other: &Settings) -> bool {
        self.height != other.height
            || self.fps != other.fps
            || self.quality != other.quality
            || self.encoder != other.encoder
            || self.audio != other.audio
            || self.mic != other.mic
            || self.buffer_secs != other.buffer_secs
    }
}

/// Output size for a source, preserving aspect ratio; never upscales; even dimensions.
pub fn output_size(src_w: u32, src_h: u32, target_h: u32) -> (u32, u32) {
    let even = |v: u32| (v.max(2)) & !1;
    if target_h == 0 || target_h >= src_h || src_h == 0 {
        return (even(src_w), even(src_h));
    }
    let w = (src_w as u64 * target_h as u64 + src_h as u64 / 2) / src_h as u64;
    (even(w as u32), even(target_h))
}

/// Target video bitrate in bits/s. Scales with pixel rate, sub-linearly with frame rate
/// (higher frame rates have smaller differences between frames).
pub fn video_bitrate(width: u32, height: u32, fps: u32, quality: Quality) -> u32 {
    let bpp = match quality {
        Quality::Low => 0.06,
        Quality::Medium => 0.09,
        Quality::High => 0.14,
        Quality::Max => 0.22,
    };
    let fps_factor = 30.0 * (fps.max(1) as f64 / 30.0).powf(0.75);
    let bps = width as f64 * height as f64 * fps_factor * bpp;
    bps.clamp(1_000_000.0, 80_000_000.0) as u32
}

pub const AUDIO_BITRATE: u32 = 160_000;

/// Estimated memory used by the replay buffer, in bytes.
pub fn buffer_bytes(video_bps: u32, audio: bool, seconds: u32) -> u64 {
    let bps = video_bps as u64 + if audio { AUDIO_BITRATE as u64 } else { 0 };
    // One extra keyframe interval is always retained.
    bps / 8 * (seconds as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut s = Settings::default();
        s.mode = Mode::Always;
        s.audio = AudioMode::Game;
        s.mic = true;
        s.hotkey = Hotkey { vk: 0x77, mods: MOD_CONTROL | MOD_SHIFT };
        s.folder = r"D:\Clips = good".into();
        s.games = vec!["a.exe".into(), "b game.exe".into()];
        assert_eq!(Settings::parse(&s.serialize()), s);
    }

    #[test]
    fn bad_values_fall_back() {
        let s = Settings::parse("fps=9999\nbuffer_seconds=abc\nhotkey=zz,1\nquality=ultra\nnonsense");
        assert_eq!(s, Settings::default());
        assert_eq!(Settings::parse("games= B.exe ; a.exe;b.exe;").games, vec!["a.exe", "b.exe"]);
    }

    #[test]
    fn sizes() {
        assert_eq!(output_size(2560, 1440, 1080), (1920, 1080));
        assert_eq!(output_size(1920, 1080, 1440), (1920, 1080));
        assert_eq!(output_size(3440, 1440, 1080), (2580, 1080));
        assert_eq!(output_size(1366, 768, 720), (1280, 720));
        assert_eq!(output_size(1365, 767, 0), (1364, 766));
    }

    #[test]
    fn bitrates_are_sane() {
        let b = video_bitrate(1920, 1080, 60, Quality::High);
        assert!((14_000_000..20_000_000).contains(&b), "{b}");
        assert!(video_bitrate(1280, 720, 30, Quality::Low) >= 1_000_000);
        assert!(video_bitrate(3840, 2160, 120, Quality::Max) <= 80_000_000);
        // 60 s of 1080p60 High is roughly 120 MB.
        let mb = buffer_bytes(b, true, 60) / 1_000_000;
        assert!((100..160).contains(&mb), "{mb}");
    }
}
