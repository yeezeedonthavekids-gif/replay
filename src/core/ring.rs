//! In-memory ring of *encoded* packets. Holding compressed packets instead of raw
//! frames is what makes a 3-minute replay buffer cost ~100 MB instead of ~40 GB.

use std::collections::VecDeque;
use std::sync::Arc;

/// 100-nanosecond units per second (the Media Foundation / WASAPI clock unit).
pub const HNS: i64 = 10_000_000;

#[derive(Clone, Debug)]
pub struct Packet {
    /// Presentation time in 100 ns units since the session clock origin.
    pub pts: i64,
    pub dur: i64,
    pub key: bool,
    pub data: Arc<[u8]>,
}

/// Packets selected for one saved clip. Video is in decode order.
#[derive(Default)]
pub struct ClipPackets {
    pub video: Vec<Packet>,
    pub audio: Vec<Packet>,
}

pub struct PacketRing {
    video: VecDeque<Packet>,
    audio: VecDeque<Packet>,
    bytes: usize,
    window: i64,
    max_bytes: usize,
    newest_video: i64,
    newest_audio: i64,
}

impl PacketRing {
    /// `window` is how much history (100 ns units) must remain saveable.
    pub fn new(window: i64, max_bytes: usize) -> Self {
        PacketRing {
            video: VecDeque::new(),
            audio: VecDeque::new(),
            bytes: 0,
            window,
            max_bytes,
            newest_video: i64::MIN,
            newest_audio: i64::MIN,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn clear(&mut self) {
        self.video.clear();
        self.audio.clear();
        self.bytes = 0;
        self.newest_video = i64::MIN;
        self.newest_audio = i64::MIN;
    }

    /// Drops everything with a timestamp before `pts` (used when the video encoder
    /// restarts so old and new streams never mix in one clip).
    pub fn clear_video(&mut self) {
        for p in self.video.drain(..) {
            self.bytes -= p.data.len();
        }
        self.newest_video = i64::MIN;
    }

    /// Seconds of video currently held.
    pub fn video_span(&self) -> i64 {
        match (self.video.front(), self.video.back()) {
            (Some(a), Some(b)) => b.pts + b.dur - a.pts,
            _ => 0,
        }
    }

    pub fn push_video(&mut self, p: Packet) {
        // The stream must start on a keyframe or the head of the buffer is undecodable.
        if self.video.is_empty() && !p.key {
            return;
        }
        self.newest_video = self.newest_video.max(p.pts);
        self.bytes += p.data.len();
        self.video.push_back(p);
        self.evict();
    }

    pub fn push_audio(&mut self, p: Packet) {
        self.newest_audio = self.newest_audio.max(p.pts);
        self.bytes += p.data.len();
        self.audio.push_back(p);
        self.evict();
    }

    /// Index of the keyframe that starts the second GOP, if any.
    fn second_gop(&self) -> Option<usize> {
        self.video.iter().skip(1).position(|p| p.key).map(|i| i + 1)
    }

    fn drop_video_front(&mut self, n: usize) {
        for p in self.video.drain(..n) {
            self.bytes -= p.data.len();
        }
    }

    fn evict(&mut self) {
        if !self.video.is_empty() {
            let target = self.newest_video - self.window;
            // Keep the newest keyframe at or before `target`; everything before it is unreachable.
            while let Some(i) = self.second_gop() {
                let over_budget = self.bytes > self.max_bytes;
                if self.video[i].pts <= target || over_budget {
                    self.drop_video_front(i);
                } else {
                    break;
                }
            }
            // Audio is kept slightly longer than the oldest video so cuts always have sound.
            let floor = self.video.front().map(|p| p.pts - HNS / 2).unwrap_or(i64::MIN);
            while let Some(a) = self.audio.front() {
                if a.pts < floor {
                    self.bytes -= a.data.len();
                    self.audio.pop_front();
                } else {
                    break;
                }
            }
        } else {
            let floor = self.newest_audio - self.window - HNS / 2;
            while let Some(a) = self.audio.front() {
                if a.pts < floor || self.bytes > self.max_bytes {
                    self.bytes -= a.data.len();
                    self.audio.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Selects the last `seconds` of the buffer. The clip starts on the newest keyframe at
    /// or before the requested start, so it is at least `seconds` long when that much exists.
    pub fn snapshot(&self, seconds: i64) -> ClipPackets {
        let mut clip = ClipPackets::default();
        if self.video.is_empty() {
            let from = self.newest_audio - seconds * HNS;
            clip.audio = self.audio.iter().filter(|a| a.pts >= from).cloned().collect();
            return clip;
        }
        let end = self.video.iter().map(|p| p.pts + p.dur).max().unwrap_or(0);
        let target = end - seconds * HNS;
        let mut start = None;
        for (i, p) in self.video.iter().enumerate() {
            if !p.key {
                continue;
            }
            if start.is_none() || p.pts <= target {
                start = Some(i);
            }
            if p.pts > target {
                break;
            }
        }
        let start = start.unwrap_or(0);
        let start_pts = self.video[start].pts;
        clip.video = self.video.iter().skip(start).cloned().collect();
        // One extra AAC frame before the cut primes the decoder; the edit list hides it.
        let first_audio = self
            .audio
            .iter()
            .position(|a| a.pts + a.dur > start_pts)
            .map(|i| i.saturating_sub(1));
        if let Some(i) = first_audio {
            clip.audio = self.audio.iter().skip(i).filter(|a| a.pts < end).cloned().collect();
        }
        clip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vid(pts: i64, key: bool) -> Packet {
        Packet { pts, dur: HNS / 10, key, data: Arc::from(vec![0u8; 100]) }
    }

    fn aud(pts: i64) -> Packet {
        Packet { pts, dur: HNS / 20, key: true, data: Arc::from(vec![0u8; 10]) }
    }

    /// 10 fps, keyframe every second, audio at 20 packets/s.
    fn fill(ring: &mut PacketRing, seconds: i64) {
        for f in 0..seconds * 10 {
            let t = f * HNS / 10;
            ring.push_video(vid(t, f % 10 == 0));
            ring.push_audio(aud(t));
            ring.push_audio(aud(t + HNS / 20));
        }
    }

    #[test]
    fn keeps_window_plus_one_gop() {
        let mut ring = PacketRing::new(5 * HNS, usize::MAX);
        fill(&mut ring, 20);
        let span = ring.video_span();
        assert!(span >= 5 * HNS && span <= 7 * HNS, "span {span}");
        assert!(ring.video.front().unwrap().key);
        let expected: usize = ring.video.iter().map(|p| p.data.len()).sum::<usize>()
            + ring.audio.iter().map(|p| p.data.len()).sum::<usize>();
        assert_eq!(ring.bytes(), expected);
    }

    #[test]
    fn snapshot_starts_on_keyframe_and_covers_request() {
        let mut ring = PacketRing::new(10 * HNS, usize::MAX);
        fill(&mut ring, 30);
        let clip = ring.snapshot(5);
        assert!(clip.video[0].key);
        let first = clip.video[0].pts;
        let last = clip.video.last().unwrap();
        let len = last.pts + last.dur - first;
        assert!(len >= 5 * HNS && len < 6 * HNS, "len {len}");
        assert!(clip.audio[0].pts <= first);
        assert!(clip.audio[1].pts + clip.audio[1].dur > first);
    }

    #[test]
    fn snapshot_longer_than_buffer_returns_everything() {
        let mut ring = PacketRing::new(60 * HNS, usize::MAX);
        fill(&mut ring, 3);
        let clip = ring.snapshot(60);
        assert_eq!(clip.video.len(), 30);
        assert_eq!(clip.video[0].pts, 0);
    }

    #[test]
    fn byte_cap_drops_whole_gops() {
        let mut ring = PacketRing::new(60 * HNS, 2_500);
        fill(&mut ring, 10);
        assert!(ring.video.front().unwrap().key);
        // Never evicts the last GOP even if it alone exceeds the cap.
        assert!(ring.video.len() >= 10);
        assert!(ring.video_span() <= 3 * HNS);
    }

    #[test]
    fn ignores_leading_non_keyframes() {
        let mut ring = PacketRing::new(5 * HNS, usize::MAX);
        ring.push_video(vid(0, false));
        assert_eq!(ring.bytes(), 0);
        ring.push_video(vid(1, true));
        assert_eq!(ring.video.len(), 1);
    }
}
