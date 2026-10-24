//! The short "clip saved" chime, synthesized at startup (no audio files shipped).

use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::UI::WindowsAndMessaging::MB_ICONWARNING;

fn chime() -> &'static [u8] {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    WAV.get_or_init(|| {
        const RATE: u32 = 44_100;
        // Two soft rising notes (E6 then B6), each with a fast attack and smooth decay.
        let notes = [(1318.5f32, 0.0f32, 0.09f32), (1975.5, 0.075, 0.16)];
        let total = (RATE as f32 * 0.25) as usize;
        let mut pcm = vec![0f32; total];
        for (freq, start, len) in notes {
            let s0 = (start * RATE as f32) as usize;
            let n = (len * RATE as f32) as usize;
            for i in 0..n.min(total.saturating_sub(s0)) {
                let t = i as f32 / RATE as f32;
                let attack = (t / 0.004).min(1.0);
                let decay = (-t * 28.0).exp();
                pcm[s0 + i] += (2.0 * std::f32::consts::PI * freq * t).sin() * attack * decay * 0.22;
            }
        }
        let data: Vec<u8> = pcm.iter().flat_map(|v| ((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
        let mut wav = Vec::with_capacity(44 + data.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&RATE.to_le_bytes());
        wav.extend_from_slice(&(RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    })
}

pub fn saved() {
    let wav = chime();
    unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

pub fn error() {
    unsafe {
        let _ = MessageBeep(MB_ICONWARNING);
    }
}
