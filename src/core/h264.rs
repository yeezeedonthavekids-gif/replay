//! Just enough H.264 bitstream handling to turn encoder output (Annex B) into MP4
//! samples (length-prefixed NAL units) and build the `avcC` decoder configuration.

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Splits an Annex B byte stream into NAL units (start codes removed).
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                out.push(trim_trailing_zeros(&data[s..i]));
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(trim_trailing_zeros(&data[s..]));
    }
    out.retain(|n| !n.is_empty());
    out
}

fn trim_trailing_zeros(mut n: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = n {
        n = rest;
    }
    n
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

/// Result of converting one access unit.
#[derive(Default)]
pub struct AccessUnit {
    pub avcc: Vec<u8>,
    pub idr: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// Converts an Annex B access unit to 4-byte-length-prefixed NAL units. Parameter sets
/// and access unit delimiters are pulled out; they belong in the `avcC` box.
pub fn annexb_to_avcc(data: &[u8]) -> AccessUnit {
    let mut au = AccessUnit { avcc: Vec::with_capacity(data.len() + 16), ..Default::default() };
    for nal in nal_units(data) {
        match nal_type(nal) {
            NAL_SPS => au.sps = Some(nal.to_vec()),
            NAL_PPS => au.pps = Some(nal.to_vec()),
            NAL_AUD => {}
            t => {
                if t == NAL_IDR {
                    au.idr = true;
                }
                au.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                au.avcc.extend_from_slice(nal);
            }
        }
    }
    au
}

/// Iterates the NAL units of a length-prefixed (AVCC) sample.
#[cfg(test)]
pub fn avcc_nals(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if data.len() < 4 {
            return None;
        }
        let n = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if 4 + n > data.len() {
            return None;
        }
        let nal = &data[4..4 + n];
        data = &data[4 + n..];
        Some(nal)
    })
}

/// Removes emulation-prevention bytes (00 00 03 -> 00 00).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let v = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(v as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.bits(zeros)? as u64) as u32)
    }
}

#[derive(Debug, PartialEq)]
pub struct SpsInfo {
    pub profile: u8,
    pub constraints: u8,
    pub level: u8,
    pub chroma_format: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
}

pub fn parse_sps(sps: &[u8]) -> Option<SpsInfo> {
    if sps.len() < 4 || nal_type(sps) != NAL_SPS {
        return None;
    }
    let raw = rbsp(&sps[1..]);
    let mut b = Bits { data: &raw, pos: 24 };
    let mut info = SpsInfo {
        profile: raw[0],
        constraints: raw[1],
        level: raw[2],
        chroma_format: 1,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
    };
    b.ue()?; // seq_parameter_set_id
    if matches!(info.profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        info.chroma_format = b.ue()?;
        if info.chroma_format == 3 {
            b.bit()?;
        }
        info.bit_depth_luma_minus8 = b.ue()?;
        info.bit_depth_chroma_minus8 = b.ue()?;
    }
    Some(info)
}

/// Builds an AVCDecoderConfigurationRecord (ISO/IEC 14496-15 §5.3.3).
pub fn avcc_config(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let info = parse_sps(sps)?;
    let mut c = vec![1, info.profile, info.constraints, info.level, 0xFF, 0xE1];
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1);
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    if matches!(info.profile, 100 | 110 | 122 | 144) {
        c.push(0xFC | (info.chroma_format as u8 & 3));
        c.push(0xF8 | (info.bit_depth_luma_minus8 as u8 & 7));
        c.push(0xF8 | (info.bit_depth_chroma_minus8 as u8 & 7));
        c.push(0);
    }
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // SPS from x264, 320x240 High profile level 1.3.
    const SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x0d, 0xac, 0xd9, 0x41, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0x99, 0x60,
    ];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 0];
        let nals = nal_units(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }

    #[test]
    fn converts_access_unit() {
        let mut data = vec![0, 0, 0, 1, 0x09, 0xf0];
        data.extend([0, 0, 0, 1]);
        data.extend(SPS);
        data.extend([0, 0, 0, 1]);
        data.extend(PPS);
        data.extend([0, 0, 1, 0x65, 0xaa, 0xbb]);
        let au = annexb_to_avcc(&data);
        assert!(au.idr);
        assert_eq!(au.sps.as_deref(), Some(SPS));
        assert_eq!(au.pps.as_deref(), Some(PPS));
        assert_eq!(au.avcc, vec![0, 0, 0, 3, 0x65, 0xaa, 0xbb]);
        assert_eq!(avcc_nals(&au.avcc).count(), 1);
    }

    #[test]
    fn parses_high_profile_sps() {
        let info = parse_sps(SPS).unwrap();
        assert_eq!((info.profile, info.level, info.chroma_format), (100, 13, 1));
        let cfg = avcc_config(SPS, PPS).unwrap();
        assert_eq!(&cfg[..6], &[1, 100, 0, 13, 0xFF, 0xE1]);
        assert_eq!(cfg.len(), 6 + 2 + SPS.len() + 1 + 2 + PPS.len() + 4);
    }

    #[test]
    fn strips_emulation_prevention() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]), vec![0, 0, 1, 0, 0, 0]);
    }
}
