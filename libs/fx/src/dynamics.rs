//! Level stages: de-esser and limiter, plus the small filters the other
//! stages share (one-pole smoother, RBJ "Audio EQ Cookbook" biquad).

use crate::Effect;
use std::f32::consts::PI;

/// One-pole smoother for control values; snaps to the first target, and onto
/// the target once close, so it never goes denormal.
pub(crate) struct Smooth {
    value: f32,
    coef: f32,
}

impl Smooth {
    pub(crate) fn new(rate: f32, ms: f32) -> Self {
        Self {
            value: f32::NAN,
            coef: 1.0 - (-1000.0 / (ms * rate.max(1.0))).exp(),
        }
    }

    pub(crate) fn next(&mut self, target: f32) -> f32 {
        if self.value.is_nan() {
            self.value = target;
        }
        self.value += self.coef * (target - self.value);
        if (self.value - target).abs() < 1e-6 {
            self.value = target;
        }
        self.value
    }

    /// Snaps to the next target again.
    pub(crate) fn reset(&mut self) {
        self.value = f32::NAN;
    }
}

/// `exp(-1 / (ms · rate))`: per-sample decay of a `ms` time constant.
pub(crate) fn decay(rate: f32, ms: f32) -> f32 {
    (-1000.0 / (ms.max(0.001) * rate.max(1.0))).exp()
}

/// `0` for values too small to matter, so feedback paths never go denormal.
pub(crate) fn flush(x: f32) -> f32 {
    if x.abs() < 1e-15 {
        0.0
    } else {
        x
    }
}

/// Stereo second-order filter (transposed direct form II).
pub(crate) struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [[f32; 2]; 2],
    /// `(freq, high-pass)` the coefficients are for.
    tuned: (f32, bool),
}

impl Biquad {
    pub(crate) fn new() -> Self {
        Self {
            b: [1.0, 0.0, 0.0],
            a: [0.0; 2],
            z: [[0.0; 2]; 2],
            tuned: (0.0, false),
        }
    }

    /// Butterworth (Q = 1/√2) low- or high-pass at `freq` Hz.
    pub(crate) fn tune(&mut self, rate: f32, freq: f32, high: bool) {
        if self.tuned == (freq, high) {
            return;
        }
        self.tuned = (freq, high);
        let w = 2.0 * PI * freq.clamp(10.0, 0.45 * rate) / rate;
        let alpha = w.sin() * std::f32::consts::FRAC_1_SQRT_2;
        let cos = w.cos();
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high {
            ((1.0 + cos) / 2.0, -(1.0 + cos))
        } else {
            ((1.0 - cos) / 2.0, 1.0 - cos)
        };
        self.b = [b0 / a0, b1 / a0, b0 / a0];
        self.a = [-2.0 * cos / a0, (1.0 - alpha) / a0];
    }

    pub(crate) fn process(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let y = self.b[0] * x + z[0];
        z[0] = flush(self.b[1] * x - self.a[0] * y + z[1]);
        z[1] = flush(self.b[2] * x - self.a[1] * y);
        y
    }

    pub(crate) fn reset(&mut self) {
        self.z = [[0.0; 2]; 2];
    }
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Split-band de-esser. The low band is a low-pass at `freq_hz`, the high
/// band its exact complement (`x - low`), so the bands sum back to the input
/// bit for bit while nothing is reduced. A separate high-pass at `freq_hz`
/// feeds the stereo-linked detector (1 ms attack, 60 ms release), which
/// keeps loud low vowels from triggering it; only the high band is turned
/// down, by `level - threshold` up to `max_reduction_db`.
pub struct DeEsser {
    rate: f32,
    split: Biquad,
    detect: Biquad,
    env: f32,
    gain: f32,
    attack: f32,
    release: f32,
}

impl DeEsser {
    pub fn new(rate: f32) -> Self {
        Self {
            rate,
            split: Biquad::new(),
            detect: Biquad::new(),
            env: 0.0,
            gain: 1.0,
            attack: decay(rate, 1.0),
            release: decay(rate, 60.0),
        }
    }
}

impl Effect for DeEsser {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[threshold_db, freq_hz, max_reduction_db] = controls else {
            return;
        };
        let freq = freq_hz.clamp(2000.0, 12000.0);
        self.split.tune(self.rate, freq, false);
        self.detect.tune(self.rate, freq, true);
        let threshold = db_to_gain(threshold_db.clamp(-60.0, 0.0));
        let floor = db_to_gain(-max_reduction_db.clamp(0.0, 24.0));
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let peak = self
                .detect
                .process(0, *l)
                .abs()
                .max(self.detect.process(1, *r).abs());
            let coef = if peak > self.env {
                self.attack
            } else {
                self.release
            };
            self.env = flush(peak + coef * (self.env - peak));
            let target = if self.env > threshold {
                (threshold / self.env).max(floor)
            } else {
                1.0
            };
            self.gain = target + self.attack * (self.gain - target);
            for (ch, s) in [l, r].into_iter().enumerate() {
                let low = self.split.process(ch, *s);
                if self.gain < 1.0 {
                    *s = low + (*s - low) * self.gain;
                }
            }
        }
    }

    fn reset(&mut self) {
        self.split.reset();
        self.detect.reset();
        self.env = 0.0;
        self.gain = 1.0;
    }
}

/// Lookahead the limiter delays the signal by: its latency.
pub(crate) const LIMITER_LOOKAHEAD_MS: f32 = 1.5;

/// Stereo-linked lookahead peak limiter. Each sample needs gain
/// `ceiling / peak`; the minimum over the lookahead window, released
/// exponentially, then box-averaged over the same window, reaches every
/// peak's gain by the time the delayed peak leaves, so no output sample
/// exceeds the ceiling. Latency: `LIMITER_LOOKAHEAD_MS`.
pub struct Limiter {
    rate: f32,
    /// Lookahead in samples (`n`); every ring holds `n + 1`.
    n: usize,
    pos: usize,
    delay: [Vec<f32>; 2],
    needed: Vec<f32>,
    released: Vec<f32>,
    sum: f64,
    held: f32,
}

impl Limiter {
    pub fn new(rate: f32) -> Self {
        let n = ((LIMITER_LOOKAHEAD_MS / 1000.0 * rate).round() as usize).max(1);
        Self {
            rate,
            n,
            pos: 0,
            delay: [vec![0.0; n + 1], vec![0.0; n + 1]],
            needed: vec![1.0; n + 1],
            released: vec![1.0; n + 1],
            sum: (n + 1) as f64,
            held: 1.0,
        }
    }
}

impl Effect for Limiter {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[ceiling_db, release_ms] = controls else {
            return;
        };
        let ceiling = db_to_gain(ceiling_db.clamp(-24.0, 0.0));
        let release = decay(self.rate, release_ms.clamp(1.0, 1000.0));
        let len = self.n + 1;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let peak = l.abs().max(r.abs());
            self.needed[self.pos] = if peak > ceiling { ceiling / peak } else { 1.0 };
            // ponytail: O(lookahead) window minimum (~72 compares at 48 kHz); a monotonic deque if lookahead grows.
            let min = self.needed.iter().copied().fold(1.0, f32::min);
            self.held = if min < self.held {
                min
            } else {
                min + release * (self.held - min)
            };
            self.sum += f64::from(self.held) - f64::from(self.released[self.pos]);
            self.released[self.pos] = self.held;
            let gain = (self.sum / len as f64) as f32;
            let oldest = (self.pos + 1) % len;
            for (s, delay) in [l, r].into_iter().zip(&mut self.delay) {
                delay[self.pos] = *s;
                *s = delay[oldest] * gain;
            }
            self.pos = oldest;
        }
    }

    fn reset(&mut self) {
        self.pos = 0;
        for delay in &mut self.delay {
            delay.fill(0.0);
        }
        self.needed.fill(1.0);
        self.released.fill(1.0);
        self.sum = (self.n + 1) as f64;
        self.held = 1.0;
    }
}

/// Amplitude of the `freq` Hz component of `signal` (single-bin DFT).
#[cfg(test)]
pub(crate) fn level_at(signal: &[f32], freq: f32, rate: f32) -> f32 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, s) in signal.iter().enumerate() {
        let ph = 2.0 * std::f64::consts::PI * f64::from(freq) * i as f64 / f64::from(rate);
        re += f64::from(*s) * ph.cos();
        im += f64::from(*s) * ph.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / signal.len() as f64) as f32
}

#[cfg(test)]
pub(crate) fn sine(freq: f32, amp: f32, rate: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| amp * (2.0 * PI * freq * i as f32 / rate).sin())
        .collect()
}

/// Runs `fx` on `input` in both channels, in 256-sample blocks; the left output.
#[cfg(test)]
pub(crate) fn run(fx: &mut dyn Effect, controls: &[f32], input: &[f32]) -> Vec<f32> {
    let (mut l, mut r) = (input.to_vec(), input.to_vec());
    for (l, r) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
        fx.process(controls, l, r);
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;

    #[test]
    fn smooth_settles_on_the_target() {
        let mut s = Smooth::new(RATE, 20.0);
        s.next(1.0);
        let last = (0..RATE as usize).map(|_| s.next(0.0)).last();
        assert_eq!(last, Some(0.0));
    }

    #[test]
    fn deesser_turns_down_only_loud_sibilance() {
        let n = RATE as usize;
        let tail = n / 2..n;
        let controls = [-30.0, 3000.0, 12.0];
        let hiss = sine(8000.0, 0.3, RATE, n);
        let out = run(&mut DeEsser::new(RATE), &controls, &hiss);
        let reduction = 20.0 * (0.3 / level_at(&out[tail.clone()], 8000.0, RATE)).log10();
        assert!((9.0..16.0).contains(&reduction), "{reduction} dB");

        let vowel = sine(300.0, 0.3, RATE, n);
        let out = run(&mut DeEsser::new(RATE), &controls, &vowel);
        let change = 20.0 * (level_at(&out[tail], 300.0, RATE) / 0.3).log10();
        assert!(change.abs() < 0.1, "{change} dB");

        let quiet = sine(8000.0, 0.01, RATE, n);
        let out = run(&mut DeEsser::new(RATE), &controls, &quiet);
        assert!(out.iter().zip(&quiet).all(|(o, i)| (o - i).abs() < 1e-6));
    }

    #[test]
    fn limiter_holds_the_ceiling() {
        let n = RATE as usize;
        let lookahead = (LIMITER_LOOKAHEAD_MS / 1000.0 * RATE) as usize;
        let ceiling = 10f32.powf(-1.0 / 20.0);
        let mut input = sine(997.0, 2.0, RATE, n);
        input[n / 2] = 8.0;
        let out = run(&mut Limiter::new(RATE), &[-1.0, 50.0], &input);
        let peak = out[lookahead..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= ceiling * 1.0116, "{peak}");
        assert!(peak > ceiling * 0.9, "{peak}");

        let quiet = sine(997.0, 0.5, RATE, n);
        let out = run(&mut Limiter::new(RATE), &[-1.0, 50.0], &quiet);
        assert!(out[lookahead..]
            .iter()
            .zip(&quiet)
            .all(|(o, i)| (o - i).abs() < 1e-6));
    }
}
