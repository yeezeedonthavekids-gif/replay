//! Turns packets from the replay ring into an MP4 file, and trims existing MP4 files
//! without re-encoding.

use super::mp4::{self, rescale, Sample, Track, TrackKind, MOVIE_TIMESCALE};
use super::ring::{ClipPackets, HNS};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const VIDEO_TIMESCALE: u32 = 90_000;

#[derive(Clone, Debug)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub avcc: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate: u32,
    /// PCM frames per encoded packet (1024 for AAC-LC).
    pub frame_len: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ClipInfo {
    pub seconds: f64,
    pub bytes: u64,
}

/// Writes to `<path>.partial` and renames on success, so a crash or full disk never leaves a
/// truncated file that looks like a real clip.
fn write_atomically(path: &Path, f: impl FnOnce(&mut BufWriter<File>) -> io::Result<u64>) -> io::Result<u64> {
    let mut tmp = PathBuf::from(path);
    tmp.as_mut_os_string().push(".partial");
    let result = (|| {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        let n = f(&mut w)?;
        w.flush()?;
        w.get_ref().sync_all()?;
        Ok(n)
    })();
    match result {
        Ok(n) => {
            std::fs::rename(&tmp, path)?;
            Ok(n)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn video_track(clip: &ClipPackets, vp: &VideoParams) -> Track {
    let pts: Vec<i64> = clip.video.iter().map(|p| rescale(p.pts, HNS as u32, VIDEO_TIMESCALE)).collect();
    // Decode timestamps: the sorted presentation times, shifted so dts <= pts everywhere.
    let mut sorted = pts.clone();
    sorted.sort_unstable();
    let shift = sorted.iter().zip(&pts).map(|(s, p)| s - p).max().unwrap_or(0).max(0);
    let dts: Vec<i64> = sorted.iter().map(|s| s - shift).collect();
    let base = dts[0];
    let samples = clip
        .video
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let dur = match dts.get(i + 1) {
                Some(next) => next - dts[i],
                None => rescale(p.dur, HNS as u32, VIDEO_TIMESCALE),
            };
            Sample { size: p.data.len() as u32, dur: dur.max(1) as u32, cts: (pts[i] - dts[i]) as i32, sync: p.key }
        })
        .collect();
    Track {
        kind: TrackKind::Video,
        timescale: VIDEO_TIMESCALE,
        width: vp.width,
        height: vp.height,
        sample_entry: mp4::avc1_entry(vp.width, vp.height, &vp.avcc),
        samples,
        media_start: sorted[0] - base,
        present_dur: None,
        delay: 0,
    }
}

/// Writes the clip. Video defines the clip's time origin; audio is aligned with edit lists.
pub fn write_clip(
    path: &Path,
    clip: &ClipPackets,
    video: Option<&VideoParams>,
    audio: Option<&AudioParams>,
) -> io::Result<ClipInfo> {
    let mut tracks = Vec::new();
    let mut sources: Vec<&[super::ring::Packet]> = Vec::new();

    let mut origin = None;
    let mut end = None;
    if let (Some(vp), false) = (video, clip.video.is_empty()) {
        tracks.push(video_track(clip, vp));
        sources.push(&clip.video);
        origin = clip.video.iter().map(|p| p.pts).min();
        end = clip.video.iter().map(|p| p.pts + p.dur).max();
    }
    if let (Some(ap), false) = (audio, clip.audio.is_empty()) {
        let sr = ap.sample_rate;
        let a0 = clip.audio[0].pts;
        let t0 = origin.unwrap_or(a0);
        let (delay, media_start) = if a0 >= t0 {
            (rescale(a0 - t0, HNS as u32, MOVIE_TIMESCALE) as u64, 0)
        } else {
            (0, rescale(t0 - a0, HNS as u32, sr))
        };
        let total = clip.audio.len() as i64 * ap.frame_len as i64;
        let present_dur = end.map(|e| {
            let want = rescale(e - a0.max(t0), HNS as u32, sr);
            want.clamp(0, (total - media_start).max(0))
        });
        tracks.push(Track {
            kind: TrackKind::Audio,
            timescale: sr,
            width: 0,
            height: 0,
            sample_entry: mp4::mp4a_entry(sr, ap.channels, &mp4::aac_asc(sr, ap.channels), ap.bitrate),
            samples: clip
                .audio
                .iter()
                .map(|p| Sample { size: p.data.len() as u32, dur: ap.frame_len, cts: 0, sync: true })
                .collect(),
            media_start,
            present_dur,
            delay,
        });
        sources.push(&clip.audio);
    }
    if tracks.is_empty() {
        return Err(io::Error::new(io::ErrorKind::Other, "nothing has been recorded yet"));
    }
    let seconds = tracks.iter().map(|t| t.duration_movie()).max().unwrap_or(0) as f64 / MOVIE_TIMESCALE as f64;
    let bytes = write_atomically(path, |w| {
        mp4::write_mp4(w, &tracks, |t, s, out| out.write_all(&sources[t][s].data))
    })?;
    Ok(ClipInfo { seconds, bytes })
}

/// Cuts `[start, end)` (seconds) out of `src` into `dst` without re-encoding.
///
/// Video is cut at the keyframe at or before `start`; the edit list then starts playback
/// exactly at `start`, so the result is frame-accurate in players that honour edit lists
/// (browsers, Discord, VLC, Windows Media Player, ffmpeg) and at worst begins up to one
/// keyframe interval early elsewhere.
pub fn trim(src: &Path, dst: &Path, start: f64, end: f64) -> io::Result<ClipInfo> {
    let invalid = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_string());
    if !(start >= 0.0 && end > start) {
        return Err(invalid("the end of the trim must be after its start"));
    }
    let mut f = File::open(src)?;
    let movie = mp4::read_movie(&mut f)?;
    let mut tracks = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new(); // (input track, first sample)

    for (ti, it) in movie.tracks.iter().enumerate() {
        let t = &it.track;
        let ts = t.timescale as f64;
        let delay_s = t.delay as f64 / MOVIE_TIMESCALE as f64;
        let mut dts = Vec::with_capacity(t.samples.len());
        let mut acc = 0i64;
        for s in &t.samples {
            dts.push(acc);
            acc += s.dur as i64;
        }
        let pres = |i: usize| delay_s + (dts[i] + t.samples[i].cts as i64 - t.media_start) as f64 / ts;
        let track_end = delay_s
            + match t.present_dur {
                Some(d) => d as f64 / ts,
                None => (acc - t.media_start) as f64 / ts,
            };
        let s = start.max(delay_s);
        let e = end.min(track_end);
        if e <= s || t.samples.is_empty() {
            continue;
        }
        let n = t.samples.len();
        let first = match t.kind {
            TrackKind::Video => {
                let mut k = None;
                for i in (0..n).filter(|&i| t.samples[i].sync) {
                    if k.is_none() || pres(i) <= s + 1e-9 {
                        k = Some(i);
                    }
                    if pres(i) > s {
                        break;
                    }
                }
                k.unwrap_or(0)
            }
            TrackKind::Audio => {
                let i = (0..n).find(|&i| pres(i) + t.samples[i].dur as f64 / ts > s).unwrap_or(n - 1);
                i.saturating_sub(1) // one frame of decoder priming
            }
        };
        let Some(last) = (first..n).rev().find(|&i| pres(i) < e) else { continue };

        let (media_start, delay) = if start >= delay_s {
            let m = ((start - delay_s) * ts).round() as i64 + t.media_start;
            (m - dts[first], 0)
        } else {
            (t.media_start - dts[first], ((delay_s - start) * MOVIE_TIMESCALE as f64).round() as u64)
        };
        tracks.push(Track {
            kind: t.kind,
            timescale: t.timescale,
            width: t.width,
            height: t.height,
            sample_entry: t.sample_entry.clone(),
            samples: t.samples[first..=last].to_vec(),
            media_start: media_start.max(0),
            present_dur: Some(((e - s) * ts).round() as i64),
            delay,
        });
        ranges.push((ti, first));
    }
    if tracks.is_empty() {
        return Err(invalid("the selected range contains no audio or video"));
    }
    let seconds = tracks.iter().map(|t| t.duration_movie()).max().unwrap_or(0) as f64 / MOVIE_TIMESCALE as f64;
    let mut buf = Vec::new();
    let bytes = write_atomically(dst, |w| {
        mp4::write_mp4(w, &tracks, |t, s, out| {
            let (ti, first) = ranges[t];
            let it = &movie.tracks[ti];
            let size = it.track.samples[first + s].size as usize;
            buf.resize(size, 0);
            f.seek(SeekFrom::Start(it.offsets[first + s]))?;
            f.read_exact(&mut buf)?;
            out.write_all(&buf)
        })
    })?;
    Ok(ClipInfo { seconds, bytes })
}

#[cfg(test)]
mod tests {
    //! These tests generate real H.264/AAC with the system ffmpeg and check the output with
    //! ffprobe. They are skipped when ffmpeg is not installed.
    use super::*;
    use crate::core::h264;
    use crate::core::ring::{Packet, PacketRing};
    use std::process::Command;
    use std::sync::Arc;

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("replay-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ffmpeg(args: &[&str]) {
        let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
        assert!(st.success(), "ffmpeg {args:?} failed");
    }

    /// Returns (stream durations, decoded frame counts, stderr) after decoding fully.
    fn probe(path: &Path) -> (String, String) {
        let o = Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-show_entries"])
            .arg("stream=codec_type,nb_read_frames,width,height:format=duration")
            .args(["-of", "compact"])
            .arg(path)
            .output()
            .unwrap();
        (String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
    }

    fn decode_errors(path: &Path) -> String {
        let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-f", "null", "-"]).output().unwrap();
        String::from_utf8_lossy(&o.stderr).into()
    }

    fn duration(report: &str) -> f64 {
        report
            .split(['|', '\n'])
            .find_map(|kv| kv.strip_prefix("duration="))
            .and_then(|v| v.trim().parse().ok())
            .unwrap()
    }

    /// Splits ADTS AAC into raw frames.
    fn adts_frames(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 7 <= data.len() {
            assert_eq!(data[i], 0xFF);
            let len = (((data[i + 3] & 3) as usize) << 11) | ((data[i + 4] as usize) << 3) | ((data[i + 5] >> 5) as usize);
            let header = if data[i + 1] & 1 == 0 { 9 } else { 7 };
            out.push(data[i + header..i + len].to_vec());
            i += len;
        }
        out
    }

    /// Simulates the recorder: Annex B access units + raw AAC frames go through the ring,
    /// get snapshotted, and are written as an MP4.
    #[test]
    fn ring_snapshot_to_valid_mp4() {
        if !have_ffmpeg() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tmpdir("clip");
        let h264_path = dir.join("v.h264");
        let aac_path = dir.join("a.aac");
        ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=60", "-t", "20", "-pix_fmt", "yuv420p",
            "-c:v", "libx264", "-preset", "ultrafast", "-bf", "0", "-g", "60", "-x264-params", "aud=1",
            "-f", "h264", h264_path.to_str().unwrap()]);
        ffmpeg(&["-f", "lavfi", "-i", "sine=f=440:r=48000", "-t", "20", "-ac", "2", "-c:a", "aac", "-b:a", "160k",
            "-f", "adts", aac_path.to_str().unwrap()]);

        let stream = std::fs::read(&h264_path).unwrap();
        // Group NAL units into access units at each AUD.
        let mut aus: Vec<Vec<u8>> = Vec::new();
        for nal in h264::nal_units(&stream) {
            if h264::nal_type(nal) == h264::NAL_AUD || aus.is_empty() {
                aus.push(Vec::new());
            }
            let au = aus.last_mut().unwrap();
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(nal);
        }
        let frame = HNS / 60;
        let mut ring = PacketRing::new(10 * HNS, usize::MAX);
        let mut params = None;
        let audio = adts_frames(&std::fs::read(&aac_path).unwrap());
        let mut ai = 0;
        for (i, au) in aus.iter().enumerate() {
            let conv = h264::annexb_to_avcc(au);
            if let (Some(sps), Some(pps), None) = (&conv.sps, &conv.pps, &params) {
                params = Some(VideoParams { width: 640, height: 360, avcc: h264::avcc_config(sps, pps).unwrap() });
            }
            let pts = i as i64 * frame;
            ring.push_video(Packet { pts, dur: frame, key: conv.idr, data: Arc::from(conv.avcc) });
            while ai < audio.len() && (ai as i64 * 1024 * HNS / 48000) <= pts {
                ring.push_audio(Packet {
                    pts: ai as i64 * 1024 * HNS / 48000,
                    dur: 1024 * HNS / 48000,
                    key: true,
                    data: Arc::from(audio[ai].clone()),
                });
                ai += 1;
            }
        }
        let clip = ring.snapshot(5);
        let out = dir.join("clip.mp4");
        let ap = AudioParams { sample_rate: 48000, channels: 2, bitrate: 160_000, frame_len: 1024 };
        let info = write_clip(&out, &clip, params.as_ref(), Some(&ap)).unwrap();
        assert!(info.seconds >= 5.0 && info.seconds < 6.1, "{info:?}");

        let (report, err) = probe(&out);
        assert!(err.trim().is_empty(), "ffprobe errors: {err}");
        assert!(report.contains("codec_type=video|width=640|height=360"), "{report}");
        assert!(report.contains("codec_type=audio"), "{report}");
        let d = duration(&report);
        assert!((d - info.seconds).abs() < 0.1, "duration {d} vs {}", info.seconds);
        let errs = decode_errors(&out);
        assert!(errs.trim().is_empty(), "decode errors: {errs}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Trims an ffmpeg-made MP4 that uses B-frames (non-zero composition offsets).
    #[test]
    fn trim_is_frame_accurate_with_bframes() {
        if !have_ffmpeg() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tmpdir("trim");
        let src = dir.join("src.mp4");
        ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30", "-f", "lavfi", "-i", "sine=f=300:r=48000",
            "-t", "12", "-pix_fmt", "yuv420p", "-c:v", "libx264", "-preset", "ultrafast", "-bf", "3", "-g", "45",
            "-c:a", "aac", "-shortest", src.to_str().unwrap()]);
        let dst = dir.join("cut.mp4");
        let info = trim(&src, &dst, 2.5, 7.0).unwrap();
        assert!((info.seconds - 4.5).abs() < 0.05, "{info:?}");

        let (report, err) = probe(&dst);
        assert!(err.trim().is_empty(), "ffprobe errors: {err}");
        let errs = decode_errors(&dst);
        assert!(errs.trim().is_empty(), "decode errors: {errs}");
        // ffmpeg honours the edit list, so the number of presented frames equals 4.5 s * 30.
        let frames: i64 = report
            .lines()
            .find(|l| l.contains("codec_type=video"))
            .and_then(|l| l.split('|').find_map(|kv| kv.strip_prefix("nb_read_frames=")))
            .and_then(|v| v.trim().parse().ok())
            .unwrap();
        assert!((frames - 135).abs() <= 1, "frames {frames}: {report}");

        // Trimming a trimmed file still works (edit lists compose).
        let dst2 = dir.join("cut2.mp4");
        let info2 = trim(&dst, &dst2, 1.0, 3.0).unwrap();
        assert!((info2.seconds - 2.0).abs() < 0.05, "{info2:?}");
        let errs = decode_errors(&dst2);
        assert!(errs.trim().is_empty(), "decode errors: {errs}");

        assert!(trim(&src, &dir.join("bad.mp4"), 5.0, 5.0).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
