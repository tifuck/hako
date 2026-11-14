//! Character stages: distortion, robot (ring modulator) and bitcrusher.

use crate::dynamics::{Biquad, Smooth};
use crate::Effect;
use std::f32::consts::TAU;

/// Smoothing time of gains and mix, ms.
const SMOOTH_MS: f32 = 20.0;

/// `tanh` soft clipper after `drive_db` of gain, then a low-pass at
/// `tone_hz`. The output is scaled so a -12 dBFS peak leaves at about the
/// level it came in, whatever the drive.
pub struct Distortion {
    rate: f32,
    tone: Biquad,
    drive: Smooth,
    makeup: Smooth,
    mix: Smooth,
}

/// Peak level the makeup gain keeps steady (-12 dBFS).
const DRIVE_REF: f32 = 0.25;

impl Distortion {
    pub fn new(rate: f32) -> Self {
        Self {
            rate,
            tone: Biquad::new(),
            drive: Smooth::new(rate, SMOOTH_MS),
            makeup: Smooth::new(rate, SMOOTH_MS),
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Distortion {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[drive_db, tone_hz, mix_pct] = controls else {
            return;
        };
        let drive = 10f32.powf(drive_db.clamp(0.0, 40.0) / 20.0);
        let makeup = DRIVE_REF / (drive * DRIVE_REF).tanh();
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        self.tone
            .tune(self.rate, tone_hz.clamp(500.0, 16000.0), false);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let (g, k, m) = (
                self.drive.next(drive),
                self.makeup.next(makeup),
                self.mix.next(mix),
            );
            for (ch, s) in [l, r].into_iter().enumerate() {
                let wet = self.tone.process(ch, (*s * g).tanh() * k);
                *s = *s * (1.0 - m) + wet * m;
            }
        }
    }

    fn reset(&mut self) {
        self.tone.reset();
        self.drive.reset();
        self.makeup.reset();
        self.mix.reset();
    }
}

/// Ring modulator: the voice times a sine at `freq_hz`.
pub struct Robot {
    rate: f32,
    /// Oscillator phase in cycles, `0..1`.
    phase: f32,
    mix: Smooth,
}

impl Robot {
    pub fn new(rate: f32) -> Self {
        Self {
            rate,
            phase: 0.0,
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Robot {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[freq_hz, mix_pct] = controls else {
            return;
        };
        let step = freq_hz.clamp(20.0, 1000.0) / self.rate;
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let carrier = (TAU * self.phase).sin();
            self.phase = (self.phase + step).fract();
            let m = self.mix.next(mix);
            for s in [l, r] {
                *s = *s * (1.0 - m) + *s * carrier * m;
            }
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.mix.reset();
    }
}

/// Quantises to `bits` (a signed integer of that width; fractional bits
/// give in-between step sizes) and holds every `downsample`-th sample.
pub struct Bitcrusher {
    held: [f32; 2],
    count: usize,
    mix: Smooth,
}

impl Bitcrusher {
    pub fn new(rate: f32) -> Self {
        Self {
            held: [0.0; 2],
            count: 0,
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Bitcrusher {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[bits, downsample, mix_pct] = controls else {
            return;
        };
        let half = 2f32.powf(bits.clamp(2.0, 16.0) - 1.0);
        let hold = downsample.clamp(1.0, 32.0).round() as usize;
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            if self.count == 0 {
                for (held, s) in self.held.iter_mut().zip([*l, *r]) {
                    *held = (s * half).round().clamp(-half, half - 1.0) / half;
                }
            }
            self.count += 1;
            if self.count >= hold {
                self.count = 0;
            }
            let m = self.mix.next(mix);
            for (s, held) in [l, r].into_iter().zip(self.held) {
                *s = *s * (1.0 - m) + held * m;
            }
        }
    }

    fn reset(&mut self) {
        self.held = [0.0; 2];
        self.count = 0;
        self.mix.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamics::{level_at, run, sine};

    const RATE: f32 = 48000.0;

    #[test]
    fn distortion_adds_harmonics_and_stays_bounded() {
        let input = sine(1000.0, 0.5, RATE, RATE as usize);
        let out = run(&mut Distortion::new(RATE), &[24.0, 16000.0, 100.0], &input);
        let fundamental = level_at(&out, 1000.0, RATE);
        let third = level_at(&out, 3000.0, RATE);
        assert!(third > 0.1 * fundamental, "{third} vs {fundamental}");
        assert!(out.iter().all(|s| s.abs() < 1.0));
    }

    #[test]
    fn robot_moves_a_tone_to_sum_and_difference() {
        let input = sine(1000.0, 0.5, RATE, RATE as usize);
        let out = run(&mut Robot::new(RATE), &[50.0, 100.0], &input);
        for f in [950.0, 1050.0] {
            let level = level_at(&out, f, RATE);
            assert!((level - 0.25).abs() < 0.01, "{f} Hz: {level}");
        }
        assert!(level_at(&out, 1000.0, RATE) < 0.005);
    }

    #[test]
    fn bitcrusher_quantises_and_holds() {
        let input = sine(440.0, 0.9, RATE, 4800);
        let out = run(&mut Bitcrusher::new(RATE), &[2.0, 1.0, 100.0], &input);
        let mut levels = out.clone();
        levels.sort_unstable_by(f32::total_cmp);
        levels.dedup();
        assert_eq!(levels.len(), 4);

        let out = run(&mut Bitcrusher::new(RATE), &[16.0, 4.0, 100.0], &input);
        for block in out.chunks(4) {
            assert!(block.iter().all(|s| *s == block[0]));
        }
        assert!((out[400] - input[400]).abs() < 1e-4);
    }
}
