//! Linear-interpolating resampler to 48 kHz stereo for audio devices that run at another
//! rate (most shared-mode Windows devices already run at 48 kHz, which is a pass-through).

use std::collections::VecDeque;

pub const RATE: u32 = 48_000;

pub struct Resampler {
    step: f64,
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    pub fn new(rate: u32) -> Self {
        Resampler { step: rate as f64 / RATE as f64, pos: 0.0, last: [0.0; 2] }
    }

    /// Converts interleaved stereo at the source rate, appending 48 kHz frames to `out`.
    pub fn run(&mut self, input: &[f32], out: &mut VecDeque<f32>) -> usize {
        let n = input.len() / 2;
        if n == 0 {
            return 0;
        }
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend(input);
            return n;
        }
        let frame = |i: isize, last: [f32; 2]| -> [f32; 2] {
            if i < 0 {
                last
            } else {
                [input[i as usize * 2], input[i as usize * 2 + 1]]
            }
        };
        let mut produced = 0;
        // Positions are relative to this block; index -1 is the previous block's last frame.
        while (self.pos.floor() as isize) + 1 < n as isize {
            let i0 = self.pos.floor() as isize;
            let t = (self.pos - i0 as f64) as f32;
            let a = frame(i0, self.last);
            let b = frame(i0 + 1, self.last);
            out.push_back(a[0] + (b[0] - a[0]) * t);
            out.push_back(a[1] + (b[1] - a[1]) * t);
            produced += 1;
            self.pos += self.step;
        }
        self.pos -= n as f64;
        self.last = frame(n as isize - 1, self.last);
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_rate() {
        let mut r = Resampler::new(44_100);
        let mut out = VecDeque::new();
        let input = vec![0.5f32; 44_100 * 2];
        let n = r.run(&input, &mut out);
        assert!((n as i64 - 48_000).abs() <= 2, "{n}");
        assert!(out.iter().skip(4).all(|v| (*v - 0.5).abs() < 1e-6));
    }
}
