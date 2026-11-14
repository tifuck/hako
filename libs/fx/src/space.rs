//! Time stages: reverb (Jezar's public-domain Freeverb), delay, chorus and
//! flanger.

use crate::dynamics::{flush, Smooth};
use crate::Effect;
use std::f32::consts::TAU;

/// Smoothing time of mix, depth and similar controls, ms.
const SMOOTH_MS: f32 = 20.0;

/// Ring buffer read `delay` samples behind the write head with linear
/// interpolation.
pub(crate) struct DelayLine {
    buf: Vec<f32>,
    mask: usize,
    head: usize,
}

impl DelayLine {
    /// Holds delays up to `max` samples.
    pub(crate) fn new(max: usize) -> Self {
        let len = (max + 2).next_power_of_two();
        Self {
            buf: vec![0.0; len],
            mask: len - 1,
            head: 0,
        }
    }

    /// The input `delay` samples ago (`1` = the last pushed sample).
    pub(crate) fn read(&self, delay: f32) -> f32 {
        let delay = delay.clamp(1.0, (self.mask - 1) as f32);
        let whole = delay as usize;
        let frac = delay - whole as f32;
        let a = self.buf[self.head.wrapping_sub(whole) & self.mask];
        let b = self.buf[self.head.wrapping_sub(whole + 1) & self.mask];
        a + (b - a) * frac
    }

    pub(crate) fn push(&mut self, x: f32) {
        self.buf[self.head] = x;
        self.head = (self.head + 1) & self.mask;
    }
}

/// Freeverb comb tunings at 44.1 kHz, left channel.
const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
/// Freeverb allpass tunings at 44.1 kHz, left channel.
const ALLPASSES: [usize; 4] = [556, 441, 341, 225];
/// Extra samples (at 44.1 kHz) on every right-channel delay.
const SPREAD: usize = 23;
const INPUT_GAIN: f32 = 0.015;

/// Feedback comb with a one-pole low-pass in its loop.
struct Comb {
    buf: Vec<f32>,
    pos: usize,
    store: f32,
}

impl Comb {
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.pos];
        self.store = flush(out * (1.0 - damp) + self.store * damp);
        self.buf[self.pos] = x + self.store * feedback;
        self.pos = (self.pos + 1) % self.buf.len();
        out
    }
}

/// Schroeder allpass, gain 0.5.
struct Allpass {
    buf: Vec<f32>,
    pos: usize,
}

impl Allpass {
    fn process(&mut self, x: f32) -> f32 {
        let delayed = self.buf[self.pos];
        self.buf[self.pos] = flush(x + delayed * 0.5);
        self.pos = (self.pos + 1) % self.buf.len();
        delayed - x
    }
}

/// Freeverb: per channel 8 parallel damped combs into 4 series allpasses,
/// the right channel's delays `SPREAD` samples longer; lengths scaled from
/// 44.1 kHz. `room_pct` sets the comb feedback (0.7..0.98), `damping_pct`
/// their low-pass, `width_pct` how much the wet channels stay apart.
pub struct Reverb {
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<Allpass>; 2],
    width: Smooth,
    mix: Smooth,
}

impl Reverb {
    pub fn new(rate: f32) -> Self {
        let scale = rate / 44100.0;
        let len =
            |n: usize, ch: usize| (((n + ch * SPREAD) as f32 * scale).round() as usize).max(1);
        Self {
            combs: [0, 1].map(|ch| {
                COMBS
                    .iter()
                    .map(|&n| Comb {
                        buf: vec![0.0; len(n, ch)],
                        pos: 0,
                        store: 0.0,
                    })
                    .collect()
            }),
            allpasses: [0, 1].map(|ch| {
                ALLPASSES
                    .iter()
                    .map(|&n| Allpass {
                        buf: vec![0.0; len(n, ch)],
                        pos: 0,
                    })
                    .collect()
            }),
            width: Smooth::new(rate, SMOOTH_MS),
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Reverb {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[room_pct, damping_pct, width_pct, mix_pct] = controls else {
            return;
        };
        let feedback = 0.7 + 0.28 * room_pct.clamp(0.0, 100.0) / 100.0;
        let damp = 0.4 * damping_pct.clamp(0.0, 100.0) / 100.0;
        let width = width_pct.clamp(0.0, 100.0) / 100.0;
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let input = (*l + *r) * INPUT_GAIN;
            let [wet_l, wet_r] = [0, 1].map(|ch| {
                let sum: f32 = self.combs[ch]
                    .iter_mut()
                    .map(|c| c.process(input, feedback, damp))
                    .sum();
                self.allpasses[ch].iter_mut().fold(sum, |x, a| a.process(x))
            });
            let w = self.width.next(width);
            let (same, cross) = (0.5 + w / 2.0, (1.0 - w) / 2.0);
            let m = self.mix.next(mix);
            *l = *l * (1.0 - m) + (wet_l * same + wet_r * cross) * m;
            *r = *r * (1.0 - m) + (wet_r * same + wet_l * cross) * m;
        }
    }
}

/// Longest delay time, ms.
const DELAY_MAX_MS: f32 = 2000.0;

/// Stereo echo with feedback. Time changes glide the read position (tape
/// style) instead of jumping, so they never click.
pub struct Delay {
    rate: f32,
    lines: [DelayLine; 2],
    time: Smooth,
    mix: Smooth,
}

impl Delay {
    pub fn new(rate: f32) -> Self {
        let max = (DELAY_MAX_MS / 1000.0 * rate).ceil() as usize;
        Self {
            rate,
            lines: [DelayLine::new(max), DelayLine::new(max)],
            time: Smooth::new(rate, 100.0),
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Delay {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[time_ms, feedback_pct, mix_pct] = controls else {
            return;
        };
        let delay = time_ms.clamp(1.0, DELAY_MAX_MS) / 1000.0 * self.rate;
        let feedback = feedback_pct.clamp(0.0, 95.0) / 100.0;
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let d = self.time.next(delay);
            let m = self.mix.next(mix);
            for (s, line) in [l, r].into_iter().zip(&mut self.lines) {
                let echo = line.read(d);
                line.push(flush(*s + echo * feedback));
                *s = *s * (1.0 - m) + echo * m;
            }
        }
    }
}

/// Base delay of the chorus voices, ms.
const CHORUS_BASE_MS: f32 = 12.0;
const CHORUS_VOICES: usize = 3;

/// Three voices delayed `CHORUS_BASE_MS` plus up to `depth_ms`, each swept
/// by the same sine a third of a cycle apart; the right channel's sweep is
/// a quarter cycle ahead for width.
pub struct Chorus {
    rate: f32,
    lines: [DelayLine; 2],
    /// LFO phase in cycles, `0..1`.
    phase: f32,
    depth: Smooth,
    mix: Smooth,
}

impl Chorus {
    pub fn new(rate: f32) -> Self {
        let max = ((CHORUS_BASE_MS + 10.0) / 1000.0 * rate).ceil() as usize + 1;
        Self {
            rate,
            lines: [DelayLine::new(max), DelayLine::new(max)],
            phase: 0.0,
            depth: Smooth::new(rate, SMOOTH_MS),
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Chorus {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[rate_hz, depth_ms, mix_pct] = controls else {
            return;
        };
        let step = rate_hz.clamp(0.05, 5.0) / self.rate;
        let ms = self.rate / 1000.0;
        let depth = depth_ms.clamp(0.5, 10.0) * ms;
        let base = CHORUS_BASE_MS * ms;
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let half_depth = self.depth.next(depth) / 2.0;
            let m = self.mix.next(mix);
            for (ch, (s, line)) in [l, r].into_iter().zip(&mut self.lines).enumerate() {
                let wet: f32 = (0..CHORUS_VOICES)
                    .map(|v| {
                        let at = self.phase + v as f32 / CHORUS_VOICES as f32 + ch as f32 / 4.0;
                        line.read(base + half_depth * (1.0 + (TAU * at).sin()))
                    })
                    .sum();
                line.push(*s);
                *s = *s * (1.0 - m) + wet / CHORUS_VOICES as f32 * m;
            }
            self.phase = (self.phase + step).fract();
        }
    }
}

/// Base delay of the flanger sweep, ms.
const FLANGER_BASE_MS: f32 = 0.5;

/// A delay of `FLANGER_BASE_MS` plus up to `depth_ms`, swept by a sine
/// (right channel a quarter cycle ahead), fed back by `feedback_pct`. The
/// loop input is scaled by `√(1 - feedback²)` so the wet level of a broadband
/// voice stays about the same at any feedback.
pub struct Flanger {
    rate: f32,
    lines: [DelayLine; 2],
    phase: f32,
    depth: Smooth,
    feedback: Smooth,
    mix: Smooth,
}

impl Flanger {
    pub fn new(rate: f32) -> Self {
        let max = ((FLANGER_BASE_MS + 5.0) / 1000.0 * rate).ceil() as usize + 1;
        Self {
            rate,
            lines: [DelayLine::new(max), DelayLine::new(max)],
            phase: 0.0,
            depth: Smooth::new(rate, SMOOTH_MS),
            feedback: Smooth::new(rate, SMOOTH_MS),
            mix: Smooth::new(rate, SMOOTH_MS),
        }
    }
}

impl Effect for Flanger {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[rate_hz, depth_ms, feedback_pct, mix_pct] = controls else {
            return;
        };
        let step = rate_hz.clamp(0.05, 5.0) / self.rate;
        let ms = self.rate / 1000.0;
        let depth = depth_ms.clamp(0.1, 5.0) * ms;
        let base = FLANGER_BASE_MS * ms;
        let feedback = feedback_pct.clamp(-95.0, 95.0) / 100.0;
        let mix = mix_pct.clamp(0.0, 100.0) / 100.0;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let half_depth = self.depth.next(depth) / 2.0;
            let fb = self.feedback.next(feedback);
            let input_gain = (1.0 - fb * fb).sqrt();
            let m = self.mix.next(mix);
            for (ch, (s, line)) in [l, r].into_iter().zip(&mut self.lines).enumerate() {
                let at = self.phase + ch as f32 / 4.0;
                let wet = line.read(base + half_depth * (1.0 + (TAU * at).sin()));
                line.push(flush(*s * input_gain + wet * fb));
                *s = *s * (1.0 - m) + wet * m;
            }
            self.phase = (self.phase + step).fract();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamics::{run, sine};

    const RATE: f32 = 48000.0;

    fn impulse(n: usize) -> Vec<f32> {
        let mut x = vec![0.0; n];
        x[0] = 1.0;
        x
    }

    fn noise(n: usize) -> Vec<f32> {
        let mut seed = 1u32;
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn reverb_tail_decays_and_grows_with_room() {
        let input = impulse(6 * RATE as usize);
        let tail = |room: f32| {
            let out = run(&mut Reverb::new(RATE), &[room, 50.0, 100.0, 100.0], &input);
            assert!(out.iter().all(|s| s.is_finite()));
            let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let end = out.iter().rposition(|s| s.abs() > peak * 1e-3).unwrap();
            end as f32 / RATE
        };
        let (small, large) = (tail(20.0), tail(90.0));
        assert!(small > 0.2 && small < 2.0, "{small} s");
        assert!(large > 2.0 * small && large < 5.5, "{large} s vs {small} s");
    }

    #[test]
    fn delay_repeats_an_impulse_with_feedback() {
        let out = run(
            &mut Delay::new(RATE),
            &[100.0, 50.0, 100.0],
            &impulse(16000),
        );
        for (i, s) in out.iter().enumerate() {
            let expected = match i {
                4800 => 1.0,
                9600 => 0.5,
                14400 => 0.25,
                _ => 0.0,
            };
            assert!((s - expected).abs() < 1e-6, "{i}: {s}");
        }
    }

    #[test]
    fn chorus_voices_sit_in_their_delay_range() {
        let out = run(&mut Chorus::new(RATE), &[0.8, 10.0, 100.0], &impulse(4800));
        let ms = RATE / 1000.0;
        let range =
            (CHORUS_BASE_MS * ms) as usize - 1..=((CHORUS_BASE_MS + 10.0) * ms) as usize + 1;
        assert!(out
            .iter()
            .enumerate()
            .all(|(i, s)| range.contains(&i) || *s == 0.0));
        assert!((out.iter().sum::<f32>() - 1.0).abs() < 0.05);

        let input = sine(1000.0, 0.5, RATE, RATE as usize);
        let out = run(&mut Chorus::new(RATE), &[0.8, 3.0, 50.0], &input);
        let diff: Vec<f32> = out.iter().zip(&input).map(|(o, i)| o - i).collect();
        assert!(rms(&diff) > 0.05);
        assert!(out.iter().all(|s| s.abs() <= 0.5 + 1e-3));
    }

    #[test]
    fn flanger_sweeps_and_stays_stable_at_full_feedback() {
        let out = run(
            &mut Flanger::new(RATE),
            &[0.8, 2.0, 0.0, 100.0],
            &impulse(480),
        );
        let first = out.iter().position(|s| *s != 0.0).unwrap() as f32 / RATE * 1000.0;
        assert!(
            (FLANGER_BASE_MS..FLANGER_BASE_MS + 2.0).contains(&first),
            "{first} ms"
        );

        let input = noise(10 * RATE as usize);
        for feedback in [95.0, -95.0] {
            let out = run(&mut Flanger::new(RATE), &[0.8, 2.0, feedback, 50.0], &input);
            assert!(out.iter().all(|s| s.is_finite() && s.abs() < 4.0));
            let second = RATE as usize;
            let (start, end) = (rms(&out[..second]), rms(&out[out.len() - second..]));
            assert!(end < 2.0 * start, "{feedback}: {start} -> {end}");
            let diff: Vec<f32> = out.iter().zip(&input).map(|(o, i)| o - i).collect();
            assert!(rms(&diff) > 0.1 * rms(&input));
        }
    }

    /// Prints each stage's realtime factor on its defaults (release build:
    /// `cargo test --release -p hako-fx -- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn classic_stages_realtime_factor() {
        let labels = [
            "deesser",
            "limiter",
            "distortion",
            "robot",
            "bitcrusher",
            "reverb",
            "delay",
            "chorus",
            "flanger",
        ];
        let seconds = 60;
        let input = noise(seconds * RATE as usize);
        for (label, controls) in hako_core::AuxEffectKind::all()
            .into_iter()
            .filter_map(|k| k.fx_controls())
            .filter(|(label, _)| labels.contains(label))
        {
            let controls: Vec<f32> = controls.iter().map(|(_, v)| *v).collect();
            let mut fx = crate::effect(label, RATE).unwrap();
            let (mut l, mut r) = (input.clone(), input.clone());
            let start = std::time::Instant::now();
            for (l, r) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
                fx.process(&controls, l, r);
            }
            let elapsed = start.elapsed().as_secs_f64();
            println!(
                "{label}: {:.0}x realtime ({:.3}% of a core)",
                seconds as f64 / elapsed,
                elapsed / seconds as f64 * 100.0
            );
        }
    }
}
