//! Channel vocoder with a built-in carrier (Dudley's channel vocoder): the
//! mono voice runs through `bands` log-spaced band-passes (100 Hz-8 kHz) whose
//! envelope followers scale the same bands of a carrier: a polyBLEP
//! band-limited saw or square (Välimäki & Huovilainen) or white noise. The
//! carrier sits at `carrier_hz` or follows the voice's detected pitch, holding
//! the last one through unvoiced sounds. An automatic gain matches the
//! output's loudness to the input's.

use crate::pitch::{control, mix_target, one_pole, sane_rate, Biquad, Mix, PitchDetector};
use crate::Effect;

const MAX_BANDS: usize = 32;
const LOW_HZ: f32 = 100.0;
const HIGH_HZ: f32 = 8_000.0;
const ATTACK_MS: f32 = 5.0;
const RELEASE_MS: f32 = 30.0;
/// Loudness followers for the automatic gain.
const LEVEL_MS: f32 = 100.0;
const MAX_GAIN: f32 = 30.0;
/// Seed of the noise carrier.
const NOISE_SEED: u32 = 0x9E37_79B9;

/// PolyBLEP residual for a step at phase 0 of a phase in `0..1` moving `dt`
/// per sample.
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let t = t / dt;
        2.0 * t - t * t - 1.0
    } else if t > 1.0 - dt {
        let t = (t - 1.0) / dt;
        t * t + 2.0 * t + 1.0
    } else {
        0.0
    }
}

pub struct Vocoder {
    rate: f32,
    bands: usize,
    voice: [Biquad; MAX_BANDS],
    synth: [Biquad; MAX_BANDS],
    env: [f32; MAX_BANDS],
    attack: f32,
    release: f32,
    phase: f32,
    noise: u32,
    detector: PitchDetector,
    followed: Option<f32>,
    level_coef: f32,
    in_level: f32,
    out_level: f32,
    mix: Mix,
}

impl Vocoder {
    pub fn new(rate: f32) -> Self {
        let rate = sane_rate(rate);
        let mut vocoder = Self {
            rate,
            bands: 0,
            voice: [Biquad::default(); MAX_BANDS],
            synth: [Biquad::default(); MAX_BANDS],
            env: [0.0; MAX_BANDS],
            attack: one_pole(ATTACK_MS, rate),
            release: one_pole(RELEASE_MS, rate),
            phase: 0.0,
            noise: NOISE_SEED,
            detector: PitchDetector::new(rate),
            followed: None,
            level_coef: one_pole(LEVEL_MS, rate),
            in_level: 0.0,
            out_level: 0.0,
            mix: Mix::new(rate),
        };
        vocoder.set_bands(16);
        vocoder
    }

    /// Spreads `n` bands evenly in log frequency, each about one band wide.
    fn set_bands(&mut self, n: usize) {
        let high = HIGH_HZ.min(0.45 * self.rate);
        let step = (high / LOW_HZ).log2() / (n - 1) as f32;
        let q = 1.0 / (2.0 * (std::f32::consts::LN_2 / 2.0 * step).sinh());
        for i in 0..n {
            let band = Biquad::bandpass(self.rate, LOW_HZ * (i as f32 * step).exp2(), q);
            self.voice[i].retune(band);
            self.synth[i].retune(band);
        }
        self.bands = n;
    }

    fn carrier(&mut self, shape: usize, hz: f32) -> f32 {
        if shape == 2 {
            self.noise ^= self.noise << 13;
            self.noise ^= self.noise >> 17;
            self.noise ^= self.noise << 5;
            return self.noise as f32 / u32::MAX as f32 * 2.0 - 1.0;
        }
        let dt = (hz / self.rate).min(0.5);
        let t = self.phase;
        self.phase += dt;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        if shape == 0 {
            2.0 * t - 1.0 - poly_blep(t, dt)
        } else {
            let square = if t < 0.5 { 1.0 } else { -1.0 };
            square + poly_blep(t, dt) - poly_blep((t + 0.5) % 1.0, dt)
        }
    }
}

impl Effect for Vocoder {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[carrier_hz, bands, mix_pct, carrier, follow] = controls else {
            return;
        };
        let fixed_hz = control(carrier_hz, 40.0, 800.0, 110.0);
        let bands = control(bands, 8.0, MAX_BANDS as f32, 16.0).round() as usize;
        if bands != self.bands {
            self.set_bands(bands);
        }
        let shape = control(carrier, 0.0, 2.0, 0.0).round() as usize;
        let follow = control(follow, 0.0, 1.0, 0.0) >= 0.5;
        let target = mix_target(mix_pct);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let x = 0.5 * (*l + *r);
            if follow && self.detector.push(x) {
                if let Some(hz) = self.detector.hz() {
                    self.followed = Some(hz.clamp(40.0, 1_000.0));
                }
            }
            let hz = match self.followed {
                Some(hz) if follow => hz,
                _ => fixed_hz,
            };
            let c = self.carrier(shape, hz);
            let mut y = 0.0;
            for i in 0..self.bands {
                let level = self.voice[i].tick(x).abs();
                let env = &mut self.env[i];
                let coef = if level > *env {
                    self.attack
                } else {
                    self.release
                };
                *env += coef * (level - *env);
                y += self.synth[i].tick(c) * *env;
            }
            self.in_level += self.level_coef * (x * x + 1e-20 - self.in_level);
            self.out_level += self.level_coef * (y * y + 1e-20 - self.out_level);
            let wet = y * (self.in_level / self.out_level).sqrt().min(MAX_GAIN);
            let m = self.mix.next(target);
            *l += (wet - *l) * m;
            *r += (wet - *r) * m;
        }
        self.mix.settle(target);
    }

    fn reset(&mut self) {
        for band in self.voice.iter_mut().chain(&mut self.synth) {
            band.reset();
        }
        self.env = [0.0; MAX_BANDS];
        self.phase = 0.0;
        self.noise = NOISE_SEED;
        self.detector.reset();
        self.followed = None;
        self.in_level = 0.0;
        self.out_level = 0.0;
        self.mix.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pitch::tests::{dominant_hz, run, RATE};

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn pulses(hz: f32, seconds: f32) -> Vec<f32> {
        let period = (RATE / hz) as usize;
        (0..(seconds * RATE) as usize)
            .map(|i| if i % period == 0 { 0.8 } else { 0.0 })
            .collect()
    }

    #[test]
    fn silence_stays_silent() {
        for carrier in [0.0, 1.0, 2.0] {
            let out = run(
                &mut Vocoder::new(RATE),
                &[110.0, 16.0, 100.0, carrier, 0.0],
                &vec![0.0; 48_000],
            );
            assert!(out.iter().all(|s| s.abs() < 1e-6), "carrier {carrier}");
        }
    }

    #[test]
    fn voice_shapes_the_carrier_at_its_pitch() {
        let input = pulses(150.0, 1.5);
        let tail = &input[24_000..];
        for (carrier_hz, follow, want) in
            [(97.0, 0.0, 97.0), (230.0, 0.0, 230.0), (97.0, 1.0, 150.0)]
        {
            let out = run(
                &mut Vocoder::new(RATE),
                &[carrier_hz, 16.0, 100.0, 0.0, follow],
                &input,
            );
            let out = &out[24_000..];
            let level = rms(out) / rms(tail);
            assert!(level > 0.5 && level < 2.0, "level {level}");
            let hz = dominant_hz(out, 60.0, 1000.0);
            assert!(
                (hz / want - 1.0).abs() < 0.02,
                "got {hz} Hz, want {want} Hz"
            );
        }
    }
}
