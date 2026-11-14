//! Pitch stages: pitch shift and auto-tune, plus the pitch detector, shifter
//! and biquad the vocoder shares.
//!
//! Detector: YIN (de Cheveigné & Kawahara 2002: cumulative mean normalised
//! difference, absolute threshold, parabolic interpolation) on a ~12 kHz
//! decimated copy, every 5 ms. Shifter: a delay-line (Doppler) shifter whose
//! read tap slides by `1 - ratio` samples per sample; when it leaves its
//! window it jumps by a whole number of detected pitch periods under a short
//! crossfade, so splices land in phase (pitch-synchronous, a fixed jump when
//! unvoiced). Biquads: RBJ Audio EQ Cookbook.
//!
//! Both stages process the mono sum and send it to both channels. `mix`
//! blends in the dry voice un-delayed while the shifted voice runs ~16 ms
//! late: lowering the mix sings in harmony with yourself.

use crate::Effect;
use std::f32::consts::PI;

const VOICE_MIN_HZ: f32 = 70.0;
const VOICE_MAX_HZ: f32 = 1000.0;
/// Detector analysis rate the input is decimated towards.
const ANALYSIS_HZ: f32 = 12_000.0;
/// YIN absolute threshold: a dip below it is a pitch period.
const YIN_THRESHOLD: f32 = 0.2;
/// Below this RMS the detector reports silence.
const SILENCE_RMS: f32 = 0.002;
const MIX_MS: f32 = 20.0;

/// Clamps the host's sample rate to something the buffers are sized for.
pub(crate) fn sane_rate(rate: f32) -> f32 {
    if rate.is_finite() {
        rate.clamp(8_000.0, 384_000.0)
    } else {
        48_000.0
    }
}

/// `v` clamped to `min..=max`, `default` when not finite.
pub(crate) fn control(v: f32, min: f32, max: f32, default: f32) -> f32 {
    if v.is_finite() {
        v.clamp(min, max)
    } else {
        default
    }
}

/// One-pole smoothing coefficient for a time constant of `ms` (0: instant).
pub(crate) fn one_pole(ms: f32, rate: f32) -> f32 {
    if ms <= 0.0 {
        1.0
    } else {
        1.0 - (-1000.0 / (ms * rate)).exp()
    }
}

/// Biquad, transposed direct form II.
#[derive(Clone, Copy, Default)]
pub(crate) struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: f32,
    s2: f32,
}

impl Biquad {
    fn from(b: [f32; 3], a: [f32; 3]) -> Self {
        Self {
            b0: b[0] / a[0],
            b1: b[1] / a[0],
            b2: b[2] / a[0],
            a1: a[1] / a[0],
            a2: a[2] / a[0],
            s1: 0.0,
            s2: 0.0,
        }
    }

    /// `(cos w0, alpha)` for a cookbook filter at `hz`.
    fn shape(rate: f32, hz: f32, q: f32) -> (f32, f32) {
        let w0 = 2.0 * PI * hz.min(0.49 * rate) / rate;
        (w0.cos(), w0.sin() / (2.0 * q))
    }

    pub(crate) fn lowpass(rate: f32, hz: f32, q: f32) -> Self {
        let (c, alpha) = Self::shape(rate, hz, q);
        let b1 = 1.0 - c;
        Self::from(
            [b1 / 2.0, b1, b1 / 2.0],
            [1.0 + alpha, -2.0 * c, 1.0 - alpha],
        )
    }

    /// Band-pass with 0 dB peak gain.
    pub(crate) fn bandpass(rate: f32, hz: f32, q: f32) -> Self {
        let (c, alpha) = Self::shape(rate, hz, q);
        Self::from([alpha, 0.0, -alpha], [1.0 + alpha, -2.0 * c, 1.0 - alpha])
    }

    /// Takes `other`'s coefficients, keeping this filter's state.
    pub(crate) fn retune(&mut self, other: Self) {
        *self = Self {
            s1: self.s1,
            s2: self.s2,
            ..other
        };
    }

    pub(crate) fn tick(&mut self, x: f32) -> f32 {
        // Keeps the state out of denormals once the input goes silent.
        let x = x + 1e-20;
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }

    pub(crate) fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }
}

/// YIN pitch detector for the voice range on a decimated copy of the input.
pub(crate) struct PitchDetector {
    rate: f32,
    lowpass: [Biquad; 2],
    decimate: usize,
    phase: usize,
    ring: Vec<f32>,
    pos: usize,
    frame: Vec<f32>,
    diff: Vec<f32>,
    cmnd: Vec<f32>,
    window: usize,
    min_lag: usize,
    max_lag: usize,
    hop: usize,
    since: usize,
    hz: Option<f32>,
}

impl PitchDetector {
    pub(crate) fn new(rate: f32) -> Self {
        let rate = sane_rate(rate);
        let decimate = (rate / ANALYSIS_HZ).round().max(1.0) as usize;
        let low = rate / decimate as f32;
        let cutoff = (0.25 * low).min(3_000.0);
        let min_lag = (low / VOICE_MAX_HZ).floor().max(2.0) as usize;
        let max_lag = (low / VOICE_MIN_HZ).ceil() as usize + 1;
        let window = max_lag;
        let len = window + max_lag + 1;
        Self {
            rate,
            lowpass: [
                Biquad::lowpass(rate, cutoff, 0.54),
                Biquad::lowpass(rate, cutoff, 1.31),
            ],
            decimate,
            phase: 0,
            ring: vec![0.0; len],
            pos: 0,
            frame: vec![0.0; len],
            diff: vec![0.0; max_lag + 2],
            cmnd: vec![0.0; max_lag + 2],
            window,
            min_lag,
            max_lag,
            hop: (low * 0.005).round().max(1.0) as usize,
            since: 0,
            hz: None,
        }
    }

    /// Feeds one input sample; true when a new estimate is ready.
    pub(crate) fn push(&mut self, x: f32) -> bool {
        let x = self.lowpass[0].tick(x);
        let x = self.lowpass[1].tick(x);
        self.phase += 1;
        if self.phase < self.decimate {
            return false;
        }
        self.phase = 0;
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) % self.ring.len();
        self.since += 1;
        if self.since < self.hop {
            return false;
        }
        self.since = 0;
        self.hz = self.analyse();
        true
    }

    /// Forgets the input heard so far.
    pub(crate) fn reset(&mut self) {
        for lowpass in &mut self.lowpass {
            lowpass.reset();
        }
        self.phase = 0;
        self.ring.fill(0.0);
        self.pos = 0;
        self.since = 0;
        self.hz = None;
    }

    /// The last estimate in Hz, `None` when unvoiced or silent.
    pub(crate) fn hz(&self) -> Option<f32> {
        self.hz
    }

    /// The last estimate as a period in input samples.
    pub(crate) fn period(&self) -> Option<f32> {
        self.hz.map(|hz| self.rate / hz)
    }

    fn analyse(&mut self) -> Option<f32> {
        let (older, newer) = self.ring.split_at(self.pos);
        self.frame[..newer.len()].copy_from_slice(newer);
        self.frame[newer.len()..].copy_from_slice(older);
        let f = &self.frame;
        let energy = f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32;
        if energy < SILENCE_RMS * SILENCE_RMS {
            return None;
        }
        let w = self.window;
        let mut total = 0.0;
        self.cmnd[0] = 1.0;
        for tau in 1..=self.max_lag + 1 {
            let d: f32 = f[..w]
                .iter()
                .zip(&f[tau..tau + w])
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
            self.diff[tau] = d;
            total += d;
            self.cmnd[tau] = if total > 0.0 {
                d * tau as f32 / total
            } else {
                1.0
            };
        }
        let mut tau = (self.min_lag..=self.max_lag).find(|&t| self.cmnd[t] < YIN_THRESHOLD)?;
        while tau < self.max_lag && self.cmnd[tau + 1] < self.cmnd[tau] {
            tau += 1;
        }
        let (a, b, c) = (self.diff[tau - 1], self.diff[tau], self.diff[tau + 1]);
        let curve = a - 2.0 * b + c;
        let offset = if curve > 0.0 {
            (0.5 * (a - c) / curve).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let period = (tau as f32 + offset) * self.decimate as f32;
        Some(self.rate / period)
    }
}

/// Delay-line pitch shifter with pitch-synchronous splices.
pub(crate) struct Shifter {
    buf: Vec<f32>,
    mask: usize,
    pos: usize,
    delay: f64,
    old: f64,
    /// Old tap's gain by samples left in the crossfade.
    fade: Vec<f32>,
    fading: usize,
    min: f64,
    span: f64,
    max_read: f64,
}

impl Shifter {
    pub(crate) fn new(rate: f32) -> Self {
        let rate = sane_rate(rate);
        let fade_len = (0.006 * rate) as usize;
        let min = fade_len as f64 + 4.0;
        // Wider than the longest voice period, so a jump always fits one.
        let span = (0.020 * rate).ceil() as f64;
        let len = (min + span) as usize + 2 * fade_len + 8;
        let len = len.next_power_of_two();
        Self {
            buf: vec![0.0; len],
            mask: len - 1,
            pos: 0,
            delay: Self::start(min, span),
            old: 0.0,
            fade: (0..fade_len)
                .map(|i| 0.5 - 0.5 * (PI * (i + 1) as f32 / (fade_len + 1) as f32).cos())
                .collect(),
            fading: 0,
            min,
            span,
            max_read: (len - 4) as f64,
        }
    }

    /// The read tap's delay at the start: the middle of its window.
    fn start(min: f64, span: f64) -> f64 {
        (min + span / 2.0).round()
    }

    /// Forgets the input heard so far.
    pub(crate) fn reset(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
        self.delay = Self::start(self.min, self.span);
        self.old = 0.0;
        self.fading = 0;
    }

    /// One sample in, one out shifted by `ratio` (0.5..=2). `period`: the
    /// input's pitch period in samples, `None` when unvoiced.
    pub(crate) fn tick(&mut self, x: f32, ratio: f32, period: Option<f32>) -> f32 {
        self.pos = (self.pos + 1) & self.mask;
        self.buf[self.pos] = x;
        let slope = 1.0 - f64::from(ratio.clamp(0.5, 2.0));
        let below = self.delay < self.min;
        if self.fading == 0 && (below || self.delay > self.min + self.span) {
            let jump = match period.map(f64::from) {
                Some(p) if p >= 1.0 && p <= self.span => (self.span / p).floor() * p,
                _ => self.span,
            };
            self.old = self.delay;
            self.delay += if below { jump } else { -jump };
            self.fading = self.fade.len();
        }
        let mut y = self.read(self.delay);
        if self.fading > 0 {
            let g = self.fade[self.fading - 1];
            y += (self.read(self.old) - y) * g;
            self.old += slope;
            self.fading -= 1;
        }
        self.delay += slope;
        y
    }

    /// Catmull-Rom interpolated read `delay` samples back.
    fn read(&self, delay: f64) -> f32 {
        let d = delay.clamp(1.0, self.max_read);
        let i = d as usize;
        let t = (d - i as f64) as f32;
        let at = |k: usize| self.buf[self.pos.wrapping_sub(k) & self.mask];
        let (xm1, x0, x1, x2) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        let c1 = 0.5 * (x1 - xm1);
        let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
        let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
        ((c3 * t + c2) * t + c1) * t + x0
    }
}

/// Smoothed dry/wet blend shared by the voice stages.
pub(crate) struct Mix {
    value: f32,
    coef: f32,
}

impl Mix {
    pub(crate) fn new(rate: f32) -> Self {
        Self {
            value: 1.0,
            coef: one_pole(MIX_MS, sane_rate(rate)),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.value = 1.0;
    }

    /// Mix for the next sample towards `mix_pct`.
    pub(crate) fn next(&mut self, target: f32) -> f32 {
        self.value += self.coef * (target - self.value);
        self.value
    }

    /// Snaps onto `target` once close, so the smoother never goes denormal.
    pub(crate) fn settle(&mut self, target: f32) {
        if (self.value - target).abs() < 1e-6 {
            self.value = target;
        }
    }
}

/// The mix control as 0..=1.
pub(crate) fn mix_target(mix_pct: f32) -> f32 {
    control(mix_pct, 0.0, 100.0, 100.0) / 100.0
}

pub struct PitchShift {
    detector: PitchDetector,
    shifter: Shifter,
    mix: Mix,
}

impl PitchShift {
    pub fn new(rate: f32) -> Self {
        Self {
            detector: PitchDetector::new(rate),
            shifter: Shifter::new(rate),
            mix: Mix::new(rate),
        }
    }
}

impl Effect for PitchShift {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[semitones, mix_pct] = controls else {
            return;
        };
        let ratio = (control(semitones, -12.0, 12.0, 0.0) / 12.0).exp2();
        let target = mix_target(mix_pct);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let x = 0.5 * (*l + *r);
            self.detector.push(x);
            let wet = self.shifter.tick(x, ratio, self.detector.period());
            let m = self.mix.next(target);
            *l += (wet - *l) * m;
            *r += (wet - *r) * m;
        }
        self.mix.settle(target);
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.shifter.reset();
        self.mix.reset();
    }
}

/// Scale degrees in semitones above the key.
const SCALES: [&[u8]; 4] = [
    &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    &[0, 2, 4, 5, 7, 9, 11],
    &[0, 2, 3, 5, 7, 8, 10],
    &[0, 2, 4, 7, 9],
];
/// How much closer (semitones) a new note must be before the held one lets go.
const NOTE_HYSTERESIS: f32 = 0.1;
/// Slowest return to the unshifted voice when the pitch is lost.
const UNVOICED_MS: f32 = 50.0;

/// Whether MIDI `note` is in `scale` of `key`.
fn in_scale(note: f32, key: u8, scale: &[u8]) -> bool {
    let degree = (note as i32 - i32::from(key)).rem_euclid(12) as u8;
    scale.contains(&degree)
}

/// The note of `scale` in `key` nearest to MIDI pitch `midi`.
fn nearest_note(midi: f32, key: u8, scale: &[u8]) -> f32 {
    let key = f32::from(key);
    let base = ((midi - key) / 12.0).floor() * 12.0 + key;
    let mut best = f32::MAX;
    for octave in [-12.0, 0.0, 12.0] {
        for &degree in scale {
            let note = base + octave + f32::from(degree);
            if (note - midi).abs() < (best - midi).abs() {
                best = note;
            }
        }
    }
    best
}

pub struct AutoTune {
    rate: f32,
    detector: PitchDetector,
    shifter: Shifter,
    mix: Mix,
    /// Current correction in semitones.
    shift: f32,
    /// Correction the detector last asked for.
    target: f32,
    note: Option<f32>,
}

impl AutoTune {
    pub fn new(rate: f32) -> Self {
        Self {
            rate: sane_rate(rate),
            detector: PitchDetector::new(rate),
            shifter: Shifter::new(rate),
            mix: Mix::new(rate),
            shift: 0.0,
            target: 0.0,
            note: None,
        }
    }

    fn retarget(&mut self, key: u8, scale: &[u8]) {
        let Some(hz) = self.detector.hz() else {
            self.note = None;
            self.target = 0.0;
            return;
        };
        let midi = 69.0 + 12.0 * (hz / 440.0).log2();
        let mut note = nearest_note(midi, key, scale);
        if let Some(held) = self.note {
            if in_scale(held, key, scale)
                && (held - midi).abs() < (note - midi).abs() + NOTE_HYSTERESIS
            {
                note = held;
            }
        }
        self.note = Some(note);
        self.target = note - midi;
    }
}

impl Effect for AutoTune {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[speed_ms, mix_pct, key, scale] = controls else {
            return;
        };
        let speed = control(speed_ms, 0.0, 200.0, 10.0);
        let key = control(key, 0.0, 11.0, 0.0).round() as u8;
        let scale = SCALES[control(scale, 0.0, 3.0, 0.0).round() as usize];
        let voiced = one_pole(speed, self.rate);
        let unvoiced = one_pole(speed.max(UNVOICED_MS), self.rate);
        let target = mix_target(mix_pct);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let x = 0.5 * (*l + *r);
            if self.detector.push(x) {
                self.retarget(key, scale);
            }
            let coef = if self.note.is_some() {
                voiced
            } else {
                unvoiced
            };
            self.shift += coef * (self.target - self.shift);
            let ratio = (self.shift / 12.0).exp2();
            let wet = self.shifter.tick(x, ratio, self.detector.period());
            let m = self.mix.next(target);
            *l += (wet - *l) * m;
            *r += (wet - *r) * m;
        }
        if (self.shift - self.target).abs() < 1e-6 {
            self.shift = self.target;
        }
        self.mix.settle(target);
    }

    fn reset(&mut self) {
        self.detector.reset();
        self.shifter.reset();
        self.mix.reset();
        self.shift = 0.0;
        self.target = 0.0;
        self.note = None;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const RATE: f32 = 48_000.0;

    /// Runs `input` (both channels) through `fx` in 256-sample blocks.
    pub(crate) fn run(fx: &mut dyn Effect, controls: &[f32], input: &[f32]) -> Vec<f32> {
        let mut left = input.to_vec();
        let mut right = input.to_vec();
        for (l, r) in left.chunks_mut(256).zip(right.chunks_mut(256)) {
            fx.process(controls, l, r);
        }
        assert_eq!(left, right);
        left
    }

    pub(crate) fn sine(hz: f32, seconds: f32) -> Vec<f32> {
        (0..(seconds * RATE) as usize)
            .map(|i| 0.5 * (2.0 * PI * hz * i as f32 / RATE).sin())
            .collect()
    }

    /// Fundamental of `x` in Hz from its normalised autocorrelation: the
    /// first peak within 10% of the best one in `min_hz..max_hz`.
    pub(crate) fn dominant_hz(x: &[f32], min_hz: f32, max_hz: f32) -> f32 {
        let lo = (RATE / max_hz) as usize;
        let hi = (RATE / min_hz) as usize + 1;
        let n = x.len() - hi - 1;
        let r: Vec<f32> = (0..=hi + 1)
            .map(|tau| {
                let (mut xy, mut xx, mut yy) = (0.0f64, 0.0f64, 0.0f64);
                for i in 0..n {
                    let (a, b) = (f64::from(x[i]), f64::from(x[i + tau]));
                    xy += a * b;
                    xx += a * a;
                    yy += b * b;
                }
                (xy / (xx * yy).sqrt().max(1e-30)) as f32
            })
            .collect();
        let best = r[lo..=hi].iter().cloned().fold(f32::MIN, f32::max);
        let mut tau = (lo..=hi).find(|&t| r[t] >= 0.9 * best).unwrap();
        while tau < hi && r[tau + 1] > r[tau] {
            tau += 1;
        }
        let (a, b, c) = (r[tau - 1], r[tau], r[tau + 1]);
        let offset = 0.5 * (a - c) / (a - 2.0 * b + c);
        RATE / (tau as f32 + offset)
    }

    fn assert_near(got: f32, want: f32, tolerance: f32) {
        assert!(
            ((got - want) / want).abs() < tolerance,
            "got {got} Hz, want {want} Hz"
        );
    }

    #[test]
    fn detector_finds_the_voice_pitch() {
        for hz in [80.0, 230.0, 900.0] {
            let mut d = PitchDetector::new(RATE);
            for s in sine(hz, 0.2) {
                d.push(s);
            }
            assert_near(d.hz().unwrap(), hz, 0.002);
        }
        let mut d = PitchDetector::new(RATE);
        for _ in 0..9600 {
            d.push(0.0);
        }
        assert_eq!(d.hz(), None);
    }

    #[test]
    fn pitch_shift_moves_a_sine_by_octaves() {
        let input = sine(220.0, 1.5);
        for (semitones, want) in [(12.0, 440.0), (-12.0, 110.0), (7.0, 329.63)] {
            let out = run(&mut PitchShift::new(RATE), &[semitones, 100.0], &input);
            assert_near(dominant_hz(&out[24_000..], 60.0, 1000.0), want, 0.01);
        }
        // 0 st is a plain delay.
        let out = run(&mut PitchShift::new(RATE), &[0.0, 100.0], &input);
        let lag = (0..2400)
            .find(|&k| {
                out[k..]
                    .iter()
                    .zip(&input)
                    .all(|(a, b)| (a - b).abs() < 1e-5)
            })
            .expect("delayed copy of the input");
        assert!(lag as f32 / RATE < 0.040, "{lag} samples");
    }

    #[test]
    fn auto_tune_snaps_to_the_scale() {
        let input = sine(230.0, 1.5);
        let chromatic = run(&mut AutoTune::new(RATE), &[0.0, 100.0, 0.0, 0.0], &input);
        assert_near(
            dominant_hz(&chromatic[24_000..], 60.0, 1000.0),
            233.08,
            0.005,
        );
        let c_major = run(&mut AutoTune::new(RATE), &[0.0, 100.0, 0.0, 1.0], &input);
        assert_near(dominant_hz(&c_major[24_000..], 60.0, 1000.0), 220.0, 0.005);
    }

    #[test]
    fn auto_tune_speed_glides_the_correction() {
        let input = sine(230.0, 1.5);
        let full = 58.0 - (69.0 + 12.0 * (230.0f32 / 440.0).log2());
        let shift_after = |speed: f32, seconds: f32| {
            let mut fx = AutoTune::new(RATE);
            run(
                &mut fx,
                &[speed, 100.0, 0.0, 0.0],
                &input[..(seconds * RATE) as usize],
            );
            fx.shift / full
        };
        assert!(shift_after(0.0, 0.1) > 0.99);
        let early = shift_after(200.0, 0.1);
        assert!(early > 0.05 && early < 0.5, "{early}");
        assert!(shift_after(200.0, 1.5) > 0.95);
        // The output pitch moves the same way.
        let mut fx = AutoTune::new(RATE);
        let out = run(&mut fx, &[200.0, 100.0, 0.0, 0.0], &input);
        let start = dominant_hz(&out[2_400..9_600], 60.0, 1000.0);
        let end = dominant_hz(&out[60_000..], 60.0, 1000.0);
        assert!(start < 231.5, "{start}");
        assert_near(end, 233.08, 0.005);
    }

    /// `cargo test --release -p hako-fx -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pitch_stage_realtime_factors() {
        let seconds = 20.0;
        let voice: Vec<f32> = sine(180.0, seconds)
            .iter()
            .zip(sine(361.0, seconds))
            .map(|(a, b)| a + 0.3 * b)
            .collect();
        let stages: [(&str, Box<dyn Effect>, &[f32]); 4] = [
            ("pitch", Box::new(PitchShift::new(RATE)), &[-4.0, 100.0]),
            (
                "autotune",
                Box::new(AutoTune::new(RATE)),
                &[10.0, 100.0, 0.0, 1.0],
            ),
            (
                "vocoder",
                Box::new(crate::vocoder::Vocoder::new(RATE)),
                &[110.0, 16.0, 100.0, 0.0, 0.0],
            ),
            (
                "vocoder 32 follow",
                Box::new(crate::vocoder::Vocoder::new(RATE)),
                &[110.0, 32.0, 100.0, 0.0, 1.0],
            ),
        ];
        for (name, mut fx, controls) in stages {
            let start = std::time::Instant::now();
            let out = run(fx.as_mut(), controls, &voice);
            let elapsed = start.elapsed().as_secs_f32();
            assert!(out.iter().all(|s| s.is_finite()));
            println!(
                "{name}: {:.0}x realtime ({:.2}% of a core)",
                seconds / elapsed,
                100.0 * elapsed / seconds
            );
        }
    }
}
