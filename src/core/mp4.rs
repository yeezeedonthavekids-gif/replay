//! Minimal ISO-BMFF (MP4) writer and reader.
//!
//! The writer produces "fast start" files (`ftyp`, `moov`, `mdat`) so clips begin playing
//! immediately in browsers and Discord. Edit lists carry A/V alignment and frame-accurate
//! trims without re-encoding. The reader understands any non-fragmented MP4 well enough to
//! trim it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub const MOVIE_TIMESCALE: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub size: u32,
    /// Duration in media timescale units (decode time delta to the next sample).
    pub dur: u32,
    /// Composition offset: presentation time = decode time + cts.
    pub cts: i32,
    pub sync: bool,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub kind: TrackKind,
    pub timescale: u32,
    pub width: u32,
    pub height: u32,
    /// Complete sample entry box (`avc1`, `mp4a`, ...), copied verbatim into `stsd`.
    pub sample_entry: Vec<u8>,
    /// Samples in decode order; the first sample decodes at media time 0.
    pub samples: Vec<Sample>,
    /// Media time at which presentation starts (edit list `media_time`).
    pub media_start: i64,
    /// Presented duration in media units; `None` = until the last sample ends.
    pub present_dur: Option<i64>,
    /// Empty time before this track starts, in movie timescale units.
    pub delay: u64,
}

impl Track {
    pub fn media_duration(&self) -> u64 {
        self.samples.iter().map(|s| s.dur as u64).sum()
    }

    fn presented_media(&self) -> i64 {
        self.present_dur.unwrap_or_else(|| {
            let mut end = 0i64;
            let mut dts = 0i64;
            for s in &self.samples {
                end = end.max(dts + s.cts as i64 + s.dur as i64);
                dts += s.dur as i64;
            }
            (end - self.media_start).max(0)
        })
    }

    fn presented_movie(&self) -> u64 {
        scale(self.presented_media().max(0) as u64, self.timescale, MOVIE_TIMESCALE)
    }

    pub fn duration_movie(&self) -> u64 {
        self.delay + self.presented_movie()
    }
}

fn scale(v: u64, from: u32, to: u32) -> u64 {
    ((v as u128 * to as u128 + from as u128 / 2) / from as u128) as u64
}

// ---------------------------------------------------------------------------------------
// Box building

struct BoxWriter {
    buf: Vec<u8>,
    stack: Vec<usize>,
}

impl BoxWriter {
    fn new() -> Self {
        BoxWriter { buf: Vec::new(), stack: Vec::new() }
    }
    fn begin(&mut self, kind: &[u8; 4]) {
        self.stack.push(self.buf.len());
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        self.buf.extend_from_slice(kind);
    }
    fn begin_full(&mut self, kind: &[u8; 4], version: u8, flags: u32) {
        self.begin(kind);
        self.u32(((version as u32) << 24) | (flags & 0xFF_FFFF));
    }
    fn end(&mut self) {
        let start = self.stack.pop().expect("unbalanced box");
        let size = (self.buf.len() - start) as u32;
        self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
    fn matrix(&mut self) {
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(v);
        }
    }
}

/// `avc1` sample entry with `avcC`, square pixels and BT.709 limited-range colour.
pub fn avc1_entry(width: u32, height: u32, avcc: &[u8]) -> Vec<u8> {
    let mut b = BoxWriter::new();
    b.begin(b"avc1");
    b.zeros(6);
    b.u16(1); // data_reference_index
    b.zeros(16); // pre_defined / reserved
    b.u16(width as u16);
    b.u16(height as u16);
    b.u32(0x0048_0000);
    b.u32(0x0048_0000);
    b.u32(0);
    b.u16(1); // frame_count
    let mut name = [0u8; 32];
    let label = b"Replay H.264";
    name[0] = label.len() as u8;
    name[1..1 + label.len()].copy_from_slice(label);
    b.bytes(&name);
    b.u16(0x0018);
    b.u16(0xFFFF);
    b.begin(b"avcC");
    b.bytes(avcc);
    b.end();
    b.begin(b"colr");
    b.bytes(b"nclx");
    b.u16(1); // BT.709 primaries
    b.u16(1); // BT.709 transfer
    b.u16(1); // BT.709 matrix
    b.u8(0); // limited range
    b.end();
    b.begin(b"pasp");
    b.u32(1);
    b.u32(1);
    b.end();
    b.end();
    b.buf
}

/// AAC-LC AudioSpecificConfig.
pub fn aac_asc(sample_rate: u32, channels: u16) -> Vec<u8> {
    const RATES: [u32; 13] =
        [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
    let idx = RATES.iter().position(|&r| r == sample_rate).unwrap_or(3) as u16;
    let v: u16 = (2 << 11) | (idx << 7) | ((channels & 0xF) << 3);
    v.to_be_bytes().to_vec()
}

/// `mp4a` sample entry with an `esds` carrying the AudioSpecificConfig.
pub fn mp4a_entry(sample_rate: u32, channels: u16, asc: &[u8], avg_bitrate: u32) -> Vec<u8> {
    fn desc(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
        out.push(tag);
        let n = body.len() as u32;
        // Fixed four-byte length form, as written by most muxers.
        out.extend_from_slice(&[
            0x80 | ((n >> 21) & 0x7F) as u8,
            0x80 | ((n >> 14) & 0x7F) as u8,
            0x80 | ((n >> 7) & 0x7F) as u8,
            (n & 0x7F) as u8,
        ]);
        out.extend_from_slice(body);
    }
    let mut dsi = Vec::new();
    desc(&mut dsi, 0x05, asc);
    let mut dcd_body = vec![0x40, 0x15, 0, 0x18, 0]; // AAC, audio stream, 6 KiB buffer
    dcd_body.extend_from_slice(&avg_bitrate.to_be_bytes()); // max bitrate
    dcd_body.extend_from_slice(&avg_bitrate.to_be_bytes());
    dcd_body.extend_from_slice(&dsi);
    let mut dcd = Vec::new();
    desc(&mut dcd, 0x04, &dcd_body);
    let mut es_body = vec![0, 0, 0]; // ES_ID, flags
    es_body.extend_from_slice(&dcd);
    desc(&mut es_body, 0x06, &[0x02]);
    let mut es = Vec::new();
    desc(&mut es, 0x03, &es_body);

    let mut b = BoxWriter::new();
    b.begin(b"mp4a");
    b.zeros(6);
    b.u16(1);
    b.zeros(8);
    b.u16(channels);
    b.u16(16);
    b.u32(0);
    b.u32(sample_rate << 16);
    b.begin_full(b"esds", 0, 0);
    b.bytes(&es);
    b.end();
    b.end();
    b.buf
}

struct Chunk {
    track: usize,
    first: usize,
    count: usize,
    bytes: u64,
    time: f64,
}

/// Groups samples into ~0.5 s chunks and interleaves tracks by time.
fn plan_chunks(tracks: &[Track]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        let limit = (t.timescale as u64) / 2;
        let mut dts = 0u64;
        let mut cur: Option<(Chunk, u64)> = None;
        for (si, s) in t.samples.iter().enumerate() {
            match &mut cur {
                Some((c, start)) if dts - *start < limit => {
                    c.count += 1;
                    c.bytes += s.size as u64;
                }
                _ => {
                    if let Some((c, _)) = cur.take() {
                        chunks.push(c);
                    }
                    let time = dts as f64 / t.timescale as f64 + t.delay as f64 / MOVIE_TIMESCALE as f64;
                    cur = Some((Chunk { track: ti, first: si, count: 1, bytes: s.size as u64, time }, dts));
                }
            }
            dts += s.dur as u64;
        }
        if let Some((c, _)) = cur {
            chunks.push(c);
        }
    }
    chunks.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap().then(a.track.cmp(&b.track)));
    chunks
}

fn write_moov(tracks: &[Track], chunks: &[Chunk], offsets: &[u64], co64: bool) -> Vec<u8> {
    let mut b = BoxWriter::new();
    let movie_dur = tracks.iter().map(|t| t.duration_movie()).max().unwrap_or(0);
    b.begin(b"moov");
    b.begin_full(b"mvhd", 0, 0);
    b.u32(0);
    b.u32(0);
    b.u32(MOVIE_TIMESCALE);
    b.u32(movie_dur as u32);
    b.u32(0x0001_0000);
    b.u16(0x0100);
    b.zeros(10);
    b.matrix();
    b.zeros(24);
    b.u32(tracks.len() as u32 + 1);
    b.end();

    for (ti, t) in tracks.iter().enumerate() {
        let video = t.kind == TrackKind::Video;
        b.begin(b"trak");
        b.begin_full(b"tkhd", 0, 3);
        b.u32(0);
        b.u32(0);
        b.u32(ti as u32 + 1);
        b.u32(0);
        b.u32(t.duration_movie() as u32);
        b.zeros(8);
        b.u16(0); // layer
        b.u16(0); // alternate group
        b.u16(if video { 0 } else { 0x0100 });
        b.u16(0);
        b.matrix();
        b.u32(if video { t.width << 16 } else { 0 });
        b.u32(if video { t.height << 16 } else { 0 });
        b.end();

        b.begin(b"edts");
        b.begin_full(b"elst", 0, 0);
        b.u32(if t.delay > 0 { 2 } else { 1 });
        if t.delay > 0 {
            b.u32(t.delay as u32);
            b.i32(-1);
            b.u32(0x0001_0000);
        }
        b.u32(t.presented_movie() as u32);
        b.i32(t.media_start as i32);
        b.u32(0x0001_0000);
        b.end();
        b.end();

        b.begin(b"mdia");
        b.begin_full(b"mdhd", 0, 0);
        b.u32(0);
        b.u32(0);
        b.u32(t.timescale);
        b.u32(t.media_duration() as u32);
        b.u16(0x55C4); // 'und'
        b.u16(0);
        b.end();
        b.begin_full(b"hdlr", 0, 0);
        b.u32(0);
        b.bytes(if video { b"vide" } else { b"soun" });
        b.zeros(12);
        b.bytes(if video { b"VideoHandler\0" } else { b"SoundHandler\0" });
        b.end();
        b.begin(b"minf");
        if video {
            b.begin_full(b"vmhd", 0, 1);
            b.zeros(8);
        } else {
            b.begin_full(b"smhd", 0, 0);
            b.zeros(4);
        }
        b.end();
        b.begin(b"dinf");
        b.begin_full(b"dref", 0, 0);
        b.u32(1);
        b.begin_full(b"url ", 0, 1);
        b.end();
        b.end();
        b.end();

        b.begin(b"stbl");
        b.begin_full(b"stsd", 0, 0);
        b.u32(1);
        b.bytes(&t.sample_entry);
        b.end();

        // stts: run-length decode durations
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for s in &t.samples {
            match runs.last_mut() {
                Some((n, d)) if *d == s.dur => *n += 1,
                _ => runs.push((1, s.dur)),
            }
        }
        b.begin_full(b"stts", 0, 0);
        b.u32(runs.len() as u32);
        for (n, d) in &runs {
            b.u32(*n);
            b.u32(*d);
        }
        b.end();

        if t.samples.iter().any(|s| s.cts != 0) {
            let negative = t.samples.iter().any(|s| s.cts < 0);
            let mut runs: Vec<(u32, i32)> = Vec::new();
            for s in &t.samples {
                match runs.last_mut() {
                    Some((n, c)) if *c == s.cts => *n += 1,
                    _ => runs.push((1, s.cts)),
                }
            }
            b.begin_full(b"ctts", if negative { 1 } else { 0 }, 0);
            b.u32(runs.len() as u32);
            for (n, c) in &runs {
                b.u32(*n);
                b.i32(*c);
            }
            b.end();
        }

        if t.samples.iter().any(|s| !s.sync) {
            let syncs: Vec<u32> = t
                .samples
                .iter()
                .enumerate()
                .filter(|(_, s)| s.sync)
                .map(|(i, _)| i as u32 + 1)
                .collect();
            b.begin_full(b"stss", 0, 0);
            b.u32(syncs.len() as u32);
            for s in syncs {
                b.u32(s);
            }
            b.end();
        }

        let my_chunks: Vec<(usize, &Chunk)> =
            chunks.iter().enumerate().filter(|(_, c)| c.track == ti).collect();
        let mut stsc: Vec<(u32, u32)> = Vec::new();
        for (n, (_, c)) in my_chunks.iter().enumerate() {
            if stsc.last().map(|&(_, cnt)| cnt) != Some(c.count as u32) {
                stsc.push((n as u32 + 1, c.count as u32));
            }
        }
        b.begin_full(b"stsc", 0, 0);
        b.u32(stsc.len() as u32);
        for (first, count) in stsc {
            b.u32(first);
            b.u32(count);
            b.u32(1);
        }
        b.end();

        b.begin_full(b"stsz", 0, 0);
        b.u32(0);
        b.u32(t.samples.len() as u32);
        for s in &t.samples {
            b.u32(s.size);
        }
        b.end();

        if co64 {
            b.begin_full(b"co64", 0, 0);
            b.u32(my_chunks.len() as u32);
            for (ci, _) in &my_chunks {
                b.u64(offsets[*ci]);
            }
        } else {
            b.begin_full(b"stco", 0, 0);
            b.u32(my_chunks.len() as u32);
            for (ci, _) in &my_chunks {
                b.u32(offsets[*ci] as u32);
            }
        }
        b.end();

        b.end(); // stbl
        b.end(); // minf
        b.end(); // mdia
        b.end(); // trak
    }
    b.end(); // moov
    b.buf
}

/// Writes a complete MP4. `fetch(track, sample, out)` must write exactly `size` bytes of
/// the requested sample.
pub fn write_mp4<W: Write>(
    out: &mut W,
    tracks: &[Track],
    mut fetch: impl FnMut(usize, usize, &mut W) -> io::Result<()>,
) -> io::Result<u64> {
    let chunks = plan_chunks(tracks);
    let mdat_payload: u64 = chunks.iter().map(|c| c.bytes).sum();

    let mut ftyp = BoxWriter::new();
    ftyp.begin(b"ftyp");
    ftyp.bytes(b"isom");
    ftyp.u32(0x200);
    ftyp.bytes(b"isomiso2avc1mp41");
    ftyp.end();

    let large = mdat_payload + 8 > u32::MAX as u64;
    let mdat_header: u64 = if large { 16 } else { 8 };
    let co64 = large || mdat_payload > (u32::MAX as u64) / 2;

    // The moov size does not depend on offset values, so lay it out once with zeros.
    let zero = vec![0u64; chunks.len()];
    let moov_len = write_moov(tracks, &chunks, &zero, co64).len() as u64;
    let mut offsets = Vec::with_capacity(chunks.len());
    let mut pos = ftyp.buf.len() as u64 + moov_len + mdat_header;
    for c in &chunks {
        offsets.push(pos);
        pos += c.bytes;
    }
    let moov = write_moov(tracks, &chunks, &offsets, co64);
    debug_assert_eq!(moov.len() as u64, moov_len);

    out.write_all(&ftyp.buf)?;
    out.write_all(&moov)?;
    if large {
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&(mdat_payload + 16).to_be_bytes())?;
    } else {
        out.write_all(&((mdat_payload + 8) as u32).to_be_bytes())?;
        out.write_all(b"mdat")?;
    }
    for c in &chunks {
        for si in c.first..c.first + c.count {
            fetch(c.track, si, out)?;
        }
    }
    Ok(pos)
}

// ---------------------------------------------------------------------------------------
// Reading

#[derive(Debug)]
pub struct InTrack {
    pub track: Track,
    /// Absolute file offset of every sample.
    pub offsets: Vec<u64>,
}

#[derive(Debug)]
pub struct Movie {
    pub tracks: Vec<InTrack>,
}

fn rd_u32(b: &[u8], at: usize) -> io::Result<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(bad)
}
fn rd_u64(b: &[u8], at: usize) -> io::Result<u64> {
    b.get(at..at + 8)
        .map(|s| u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or_else(bad)
}
fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed MP4")
}

/// Iterates child boxes: yields (type, body, whole box).
fn children(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8], &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos + 8 > data.len() {
            return None;
        }
        let mut size = rd_u32(data, pos).ok()? as u64;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
        let mut header = 8;
        if size == 1 {
            size = rd_u64(data, pos + 8).ok()?;
            header = 16;
        } else if size == 0 {
            size = (data.len() - pos) as u64;
        }
        let end = pos.checked_add(size as usize)?;
        if size < header as u64 || end > data.len() {
            return None;
        }
        let item = (kind, &data[pos + header..end], &data[pos..end]);
        pos = end;
        Some(item)
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).find(|(k, _, _)| k == kind).map(|(_, b, _)| b)
}

/// Locates and reads the `moov` box without reading media data.
fn read_moov(f: &mut File) -> io::Result<Vec<u8>> {
    let len = f.metadata()?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[0..4].try_into().unwrap()) as u64;
        let mut header = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header {
            return Err(bad());
        }
        if &h[4..8] == b"moov" {
            if size > 256 << 20 {
                return Err(bad());
            }
            let mut buf = vec![0u8; (size - header) as usize];
            f.read_exact(&mut buf)?;
            return Ok(buf);
        }
        pos += size;
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "no moov box (not a finished MP4)"))
}

fn parse_track(trak: &[u8]) -> io::Result<Option<InTrack>> {
    let mdia = child(trak, b"mdia").ok_or_else(bad)?;
    let hdlr = child(mdia, b"hdlr").ok_or_else(bad)?;
    let kind = match hdlr.get(8..12) {
        Some(b"vide") => TrackKind::Video,
        Some(b"soun") => TrackKind::Audio,
        _ => return Ok(None),
    };
    let mdhd = child(mdia, b"mdhd").ok_or_else(bad)?;
    let timescale = if mdhd[0] == 1 { rd_u32(mdhd, 20)? } else { rd_u32(mdhd, 12)? };
    if timescale == 0 {
        return Err(bad());
    }
    let (mut width, mut height) = (0, 0);
    if let Some(tkhd) = child(trak, b"tkhd") {
        let at = if tkhd[0] == 1 { 88 } else { 76 };
        width = rd_u32(tkhd, at)? >> 16;
        height = rd_u32(tkhd, at + 4)? >> 16;
    }
    let mut media_start = 0i64;
    let mut delay_movie = 0u64;
    let mut present: Option<u64> = None;
    if let Some(elst) = child(trak, b"edts").and_then(|e| child(e, b"elst")) {
        let v1 = elst[0] == 1;
        let n = rd_u32(elst, 4)? as usize;
        let step = if v1 { 20 } else { 12 };
        for i in 0..n {
            let at = 8 + i * step;
            let (dur, mt) = if v1 {
                (rd_u64(elst, at)?, rd_u64(elst, at + 8)? as i64)
            } else {
                (rd_u32(elst, at)? as u64, rd_u32(elst, at + 4)? as i32 as i64)
            };
            if mt == -1 {
                delay_movie += dur;
            } else {
                media_start = mt;
                present = Some(dur);
                break;
            }
        }
    }

    let stbl = child(child(mdia, b"minf").ok_or_else(bad)?, b"stbl").ok_or_else(bad)?;
    let stsd = child(stbl, b"stsd").ok_or_else(bad)?;
    if rd_u32(stsd, 4)? != 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "multiple sample descriptions"));
    }
    let (_, _, entry) = children(&stsd[8..]).next().ok_or_else(bad)?;

    let stsz = child(stbl, b"stsz").ok_or_else(bad)?;
    let fixed = rd_u32(stsz, 4)?;
    let count = rd_u32(stsz, 8)? as usize;
    if count > 50_000_000 {
        return Err(bad());
    }
    let mut samples = vec![Sample { sync: true, ..Default::default() }; count];
    for (i, s) in samples.iter_mut().enumerate() {
        s.size = if fixed != 0 { fixed } else { rd_u32(stsz, 12 + i * 4)? };
    }

    let stts = child(stbl, b"stts").ok_or_else(bad)?;
    let mut i = 0;
    for e in 0..rd_u32(stts, 4)? as usize {
        let n = rd_u32(stts, 8 + e * 8)?;
        let d = rd_u32(stts, 12 + e * 8)?;
        for _ in 0..n {
            if let Some(s) = samples.get_mut(i) {
                s.dur = d;
            }
            i += 1;
        }
    }
    if let Some(ctts) = child(stbl, b"ctts") {
        let mut i = 0;
        for e in 0..rd_u32(ctts, 4)? as usize {
            let n = rd_u32(ctts, 8 + e * 8)?;
            let c = rd_u32(ctts, 12 + e * 8)? as i32;
            for _ in 0..n {
                if let Some(s) = samples.get_mut(i) {
                    s.cts = c;
                }
                i += 1;
            }
        }
    }
    if let Some(stss) = child(stbl, b"stss") {
        samples.iter_mut().for_each(|s| s.sync = false);
        for e in 0..rd_u32(stss, 4)? as usize {
            let idx = rd_u32(stss, 8 + e * 4)? as usize;
            if let Some(s) = idx.checked_sub(1).and_then(|i| samples.get_mut(i)) {
                s.sync = true;
            }
        }
    }

    let chunk_offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        (0..rd_u32(stco, 4)? as usize).map(|i| rd_u32(stco, 8 + i * 4).map(u64::from)).collect::<io::Result<_>>()?
    } else {
        let co64 = child(stbl, b"co64").ok_or_else(bad)?;
        (0..rd_u32(co64, 4)? as usize).map(|i| rd_u64(co64, 8 + i * 8)).collect::<io::Result<_>>()?
    };
    let stsc = child(stbl, b"stsc").ok_or_else(bad)?;
    let entries: Vec<(usize, usize)> = (0..rd_u32(stsc, 4)? as usize)
        .map(|e| Ok((rd_u32(stsc, 8 + e * 12)? as usize, rd_u32(stsc, 12 + e * 12)? as usize)))
        .collect::<io::Result<_>>()?;
    let mut offsets = Vec::with_capacity(count);
    let mut si = 0usize;
    for (ci, &base) in chunk_offsets.iter().enumerate() {
        let chunk_no = ci + 1;
        let per = entries.iter().rev().find(|(first, _)| *first <= chunk_no).map(|e| e.1).unwrap_or(0);
        let mut off = base;
        for _ in 0..per {
            if si >= count {
                break;
            }
            offsets.push(off);
            off += samples[si].size as u64;
            si += 1;
        }
    }
    if offsets.len() != count {
        return Err(bad());
    }

    let present_dur = present.map(|d| scale(d, MOVIE_TIMESCALE, timescale) as i64);
    Ok(Some(InTrack {
        track: Track {
            kind,
            timescale,
            width,
            height,
            sample_entry: entry.to_vec(),
            samples,
            media_start,
            present_dur,
            delay: delay_movie,
        },
        offsets,
    }))
}

pub fn read_movie(f: &mut File) -> io::Result<Movie> {
    let moov = read_moov(f)?;
    let mut tracks = Vec::new();
    for (k, body, _) in children(&moov) {
        if &k == b"trak" {
            if let Some(t) = parse_track(body)? {
                tracks.push(t);
            }
        }
    }
    Ok(Movie { tracks })
}

/// Reads only the movie duration (fast: touches just the `moov` box).
pub fn probe_duration(f: &mut File) -> io::Result<f64> {
    let moov = read_moov(f)?;
    let mvhd = child(&moov, b"mvhd").ok_or_else(bad)?;
    let (ts, dur) = if mvhd[0] == 1 {
        (rd_u32(mvhd, 20)?, rd_u64(mvhd, 24)?)
    } else {
        (rd_u32(mvhd, 12)?, rd_u32(mvhd, 16)? as u64)
    };
    Ok(if ts > 0 { dur as f64 / ts as f64 } else { 0.0 })
}

pub fn rescale(v: i64, from: u32, to: u32) -> i64 {
    ((v as i128 * to as i128 + (from as i128 / 2) * v.signum() as i128) / from as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_for_48k_stereo() {
        assert_eq!(aac_asc(48000, 2), vec![0x11, 0x90]);
        assert_eq!(aac_asc(44100, 2), vec![0x12, 0x10]);
    }

    #[test]
    fn round_trips_through_reader() {
        let samples: Vec<Sample> = (0..100)
            .map(|i| Sample { size: 10 + i, dur: 1500, cts: if i % 3 == 1 { 3000 } else { 0 }, sync: i % 30 == 0 })
            .collect();
        let video = Track {
            kind: TrackKind::Video,
            timescale: 90000,
            width: 1920,
            height: 1080,
            sample_entry: avc1_entry(1920, 1080, &[1, 100, 0, 40, 0xFF, 0xE1, 0, 0, 1, 0, 0]),
            samples: samples.clone(),
            media_start: 1500,
            present_dur: None,
            delay: 0,
        };
        let audio = Track {
            kind: TrackKind::Audio,
            timescale: 48000,
            width: 0,
            height: 0,
            sample_entry: mp4a_entry(48000, 2, &aac_asc(48000, 2), 160_000),
            samples: (0..50).map(|_| Sample { size: 7, dur: 1024, cts: 0, sync: true }).collect(),
            media_start: 0,
            present_dur: None,
            delay: 20,
        };
        let dir = std::env::temp_dir().join(format!("replay-mp4-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.mp4");
        let tracks = [video, audio];
        let mut f = File::create(&path).unwrap();
        write_mp4(&mut f, &tracks, |t, s, out| {
            let n = tracks[t].samples[s].size as usize;
            out.write_all(&vec![(t * 100 + s % 100) as u8; n])
        })
        .unwrap();
        drop(f);

        let mut f = File::open(&path).unwrap();
        let movie = read_movie(&mut f).unwrap();
        assert_eq!(movie.tracks.len(), 2);
        let v = &movie.tracks[0];
        assert_eq!(v.track.samples, samples);
        assert_eq!(v.track.media_start, 1500);
        assert_eq!((v.track.width, v.track.height), (1920, 1080));
        assert_eq!(movie.tracks[1].track.delay, 20);
        // Every sample's bytes are where the reader says they are.
        for (ti, t) in movie.tracks.iter().enumerate() {
            for (si, &off) in t.offsets.iter().enumerate() {
                let mut b = [0u8; 1];
                f.seek(SeekFrom::Start(off)).unwrap();
                f.read_exact(&mut b).unwrap();
                assert_eq!(b[0], (ti * 100 + si % 100) as u8);
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
