//! AI noise removal: DeepFilterNet 3 (Schröter et al., "DeepFilterNet: Perceptually Motivated
//! Real-Time Speech Enhancement", 2023, via `deep-filter-rt`), or RNNoise (Valin, "A Hybrid
//! DSP/Deep Learning Approach to Real-Time Full-Band Speech Enhancement", 2018, via the
//! `nnnoiseless` port) as the light model. Both are 48 kHz mono models on 10 ms hops: the input
//! is folded to mono and fed through a hop FIFO, and the voice goes to both channels. Latency is
//! one hop plus the model's delay: 40 ms with DeepFilterNet, 20 ms with RNNoise.

use crate::Effect;
use deep_filter_rt::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};
use nnnoiseless::DenoiseState;
use std::panic::{catch_unwind, AssertUnwindSafe};

const RATE: f32 = 48000.0;
const HOP: usize = DenoiseState::FRAME_SIZE;
/// RNNoise's output lags its input by one frame (`rnnoise_lags_one_frame`).
const RNN_DELAY: usize = HOP;
/// RNNoise works on samples in the i16 range.
const RNN_SCALE: f32 = 32768.0;
/// Hops a model replays before it takes over, so it starts warm and in step.
const WARM_HOPS: usize = 6;
/// Recent mono input: the replayed hops, and the dry signal behind a model's delay.
const HISTORY: usize = 2 * WARM_HOPS * HOP;
/// Per-sample smoothing of RNNoise's dry share, about 10 ms.
const BLEND_SMOOTH: f32 = 0.002;

#[derive(Clone, Copy, PartialEq)]
enum Model {
    DeepFilter,
    RnNoise,
}

pub struct Denoise {
    /// `None` at rates other than 48 kHz.
    inner: Option<Box<Inner>>,
}

struct Inner {
    engines: Engines,
    active: Option<Model>,
    history: [f32; HISTORY],
    input: [f32; HOP],
    output: [f32; HOP],
    fade: [f32; HOP],
    pos: usize,
}

struct Engines {
    /// `None` if the model failed to load; RNNoise stands in.
    dfn: Option<DfTract>,
    dfn_reduction: f32,
    rnn: Box<DenoiseState<'static>>,
    rnn_frame: [f32; HOP],
    rnn_dry: f32,
}

impl Denoise {
    pub fn new(rate: f32) -> Self {
        // ponytail: 48 kHz only, other rates pass through; resample around the models for them.
        if rate != RATE {
            return Self { inner: None };
        }
        Self {
            inner: Some(Box::new(Inner {
                engines: Engines {
                    dfn: catch_unwind(load_deep_filter).ok().flatten(),
                    dfn_reduction: 100.0,
                    rnn: DenoiseState::new(),
                    rnn_frame: [0.0; HOP],
                    rnn_dry: 0.0,
                },
                active: None,
                history: [0.0; HISTORY],
                input: [0.0; HOP],
                output: [0.0; HOP],
                fade: [0.0; HOP],
                pos: 0,
            })),
        }
    }
}

impl Effect for Denoise {
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]) {
        let &[reduction_db, model] = controls else {
            return;
        };
        let Some(inner) = &mut self.inner else {
            return;
        };
        let reduction_db = if reduction_db.is_finite() {
            reduction_db.clamp(0.0, 100.0)
        } else {
            100.0
        };
        let model = if model >= 0.5 || inner.engines.dfn.is_none() {
            Model::RnNoise
        } else {
            Model::DeepFilter
        };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let x = 0.5 * (*l + *r);
            inner.input[inner.pos] = if x.is_finite() { x } else { 0.0 };
            *l = inner.output[inner.pos];
            *r = *l;
            inner.pos += 1;
            if inner.pos == HOP {
                inner.pos = 0;
                inner.hop(model, reduction_db);
            }
        }
    }
}

impl Inner {
    /// Runs `model` on the hop in `input`, into `output`. A model taking over first replays the
    /// last hops, then fades in over the hop.
    fn hop(&mut self, model: Model, reduction_db: f32) {
        self.history.copy_within(HOP.., 0);
        self.history[HISTORY - HOP..].copy_from_slice(&self.input);
        if self.active == Some(model) {
            self.engines.run(
                model,
                &self.history,
                HISTORY,
                reduction_db,
                &mut self.output,
            );
            return;
        }
        for back in (0..=WARM_HOPS).rev() {
            let end = HISTORY - back * HOP;
            self.engines
                .run(model, &self.history, end, reduction_db, &mut self.fade);
        }
        match self.active {
            Some(old) => {
                self.engines
                    .run(old, &self.history, HISTORY, reduction_db, &mut self.output)
            }
            None => self.output.fill(0.0),
        }
        for (i, (out, new)) in self.output.iter_mut().zip(&self.fade).enumerate() {
            *out += (new - *out) * (i + 1) as f32 / HOP as f32;
        }
        self.active = Some(model);
    }
}

impl Engines {
    /// One hop of `model`: the input is the hop of `history` ending at `end`.
    // ponytail: on the audio thread tract allocates its tensors and takes its (uncontended)
    // executor lock per hop, and easyfft allocates its FFT plan on a thread's first RNNoise
    // frame; fully preallocated inference needs an engine without that.
    fn run(
        &mut self,
        model: Model,
        history: &[f32; HISTORY],
        end: usize,
        reduction_db: f32,
        out: &mut [f32; HOP],
    ) {
        let input = &history[end - HOP..end];
        match (model, &mut self.dfn) {
            (Model::DeepFilter, Some(df)) => {
                if reduction_db != self.dfn_reduction {
                    df.set_atten_lim(reduction_db);
                    self.dfn_reduction = reduction_db;
                }
                let ok = match (
                    ArrayView2::from_shape((1, HOP), input),
                    ArrayViewMut2::from_shape((1, HOP), &mut out[..]),
                ) {
                    (Ok(noisy), Ok(enh)) => {
                        catch_unwind(AssertUnwindSafe(|| df.process(noisy, enh)))
                            .is_ok_and(|r| r.is_ok())
                    }
                    _ => false,
                };
                if !ok {
                    out.copy_from_slice(dry(history, end, df.delay()));
                }
            }
            _ => {
                for (s, x) in self.rnn_frame.iter_mut().zip(input) {
                    *s = x * RNN_SCALE;
                }
                self.rnn.process_frame(out, &self.rnn_frame);
                let target = if reduction_db >= 100.0 {
                    0.0
                } else {
                    10f32.powf(-reduction_db / 20.0)
                };
                for (o, d) in out.iter_mut().zip(dry(history, end, RNN_DELAY)) {
                    self.rnn_dry += (target - self.rnn_dry) * BLEND_SMOOTH;
                    *o = *o / RNN_SCALE * (1.0 - self.rnn_dry) + d * self.rnn_dry;
                }
            }
        }
    }
}

/// The embedded DeepFilterNet 3 with the SNR thresholds of upstream's own LADSPA plugin and CLI
/// (the crate's defaults, -10 / 30 / 20 dB, take about 6 dB off speech in moderate noise).
fn load_deep_filter() -> Option<DfTract> {
    let params = RuntimeParams::default().with_thresholds(-15.0, 35.0, 35.0);
    DfTract::new(DfParams::default(), &params).ok()
}

/// The hop of `history` `delay` samples before the one ending at `end`.
fn dry(history: &[f32; HISTORY], end: usize, delay: usize) -> &[f32] {
    let start = end.saturating_sub(HOP + delay);
    &history[start..start + HOP]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{PI, TAU};
    use std::time::Instant;

    fn noise(len: usize, peak: f32) -> Vec<f32> {
        let mut state = 0x2545_f491_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32 * 2.0 - 1.0) * peak
            })
            .collect()
    }

    /// A vowel: a 120 Hz pulse train through three formant resonators, in 250 ms syllables.
    fn vowel(len: usize) -> Vec<f32> {
        let formants = [(700.0, 80.0), (1220.0, 90.0), (2600.0, 120.0)];
        let mut states = [[0f32; 2]; 3];
        (0..len)
            .map(|i| {
                let pulse = if i % 400 == 0 { 1.0 } else { 0.0 };
                let mut y = 0.0;
                for ((freq, bw), s) in formants.iter().zip(&mut states) {
                    let r = (-PI * bw / RATE).exp();
                    let v = pulse + 2.0 * r * (TAU * freq / RATE).cos() * s[0] - r * r * s[1];
                    s[1] = s[0];
                    s[0] = v;
                    y += v;
                }
                let envelope = (0.5 - 0.5 * (TAU * 4.0 * i as f32 / RATE).cos()).powi(2);
                0.02 * y * envelope
            })
            .collect()
    }

    fn controls(reduction_db: f32, model: Model) -> [f32; 2] {
        [reduction_db, (model == Model::RnNoise) as u8 as f32]
    }

    /// Runs `input` (both channels) through in blocks of `block` samples; returns the left output.
    fn run(fx: &mut Denoise, controls: [f32; 2], input: &[f32], block: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        for chunk in input.chunks(block) {
            let (mut l, mut r) = (chunk.to_vec(), chunk.to_vec());
            fx.process(&controls, &mut l, &mut r);
            assert_eq!(l, r);
            out.extend(l);
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Noise at -20 dBFS RMS, white or through a one-pole low-pass at `lowpass`.
    fn noise_20db(len: usize, lowpass: f32) -> Vec<f32> {
        let mut y = 0.0;
        let raw: Vec<f32> = noise(len, 1.0)
            .into_iter()
            .map(|x| {
                y = lowpass * y + (1.0 - lowpass) * x;
                y
            })
            .collect();
        let gain = 0.1 / rms(&raw);
        raw.iter().map(|x| x * gain).collect()
    }

    /// Attenuation of `input` once the model settled, in dB.
    fn noise_reduction(model: Model, reduction_db: f32, input: &[f32]) -> f32 {
        let mut fx = Denoise::new(RATE);
        let out = run(&mut fx, controls(reduction_db, model), input, 512);
        let settled = 2 * RATE as usize..;
        20.0 * (rms(&input[settled.clone()]) / rms(&out[settled])).log10()
    }

    /// RNNoise gets fan-like noise: loud white noise is outside what it was trained on (it
    /// takes about 3 dB off white noise at -20 dBFS, against 30 dB at -30 dBFS).
    #[test]
    fn removes_noise_down_to_the_strength() {
        let len = 4 * RATE as usize;
        let white = noise_20db(len, 0.0);
        let full = noise_reduction(Model::DeepFilter, 100.0, &white);
        assert!(full > 20.0, "DeepFilterNet: {full} dB");
        let limited = noise_reduction(Model::DeepFilter, 6.0, &white);
        assert!(
            (5.0..7.0).contains(&limited),
            "DeepFilterNet at 6 dB: {limited} dB"
        );
        let fan = noise_20db(len, 0.9);
        let full = noise_reduction(Model::RnNoise, 100.0, &fan);
        assert!(full > 15.0, "RNNoise: {full} dB");
        let limited = noise_reduction(Model::RnNoise, 6.0, &fan);
        assert!(
            (4.5..7.5).contains(&limited),
            "RNNoise at 6 dB: {limited} dB"
        );
    }

    #[test]
    fn rnnoise_lags_one_frame() {
        let mut rnn = DenoiseState::new();
        let input = vowel(200 * HOP);
        let mut out = vec![0.0; input.len()];
        for (o, i) in out
            .as_chunks_mut::<HOP>()
            .0
            .iter_mut()
            .zip(input.as_chunks::<HOP>().0)
        {
            let scaled: Vec<f32> = i.iter().map(|x| x * RNN_SCALE).collect();
            rnn.process_frame(o, &scaled);
        }
        let range = 50 * HOP..input.len() - 4 * HOP;
        let lag = (0..4 * HOP)
            .max_by(|&a, &b| {
                let corr =
                    |lag: usize| -> f32 { range.clone().map(|i| input[i] * out[i + lag]).sum() };
                corr(a).total_cmp(&corr(b))
            })
            .unwrap();
        assert_eq!(lag, RNN_DELAY);
    }

    /// At 0 dB both models pass the input through, delayed by the latency they report, whatever
    /// the block size.
    #[test]
    fn latency_is_constant_and_as_reported() {
        let dfn_delay = Denoise::new(RATE)
            .inner
            .unwrap()
            .engines
            .dfn
            .unwrap()
            .delay();
        assert_eq!(dfn_delay, 1440);
        let mut input = vec![0.0; RATE as usize / 2];
        for at in [5000, 13_337, 20_000] {
            input[at] = 0.5;
        }
        for (model, latency) in [
            (Model::DeepFilter, HOP + dfn_delay),
            (Model::RnNoise, HOP + RNN_DELAY),
        ] {
            for block in [1, 64, 333, 1024, 8192] {
                let out = run(&mut Denoise::new(RATE), controls(0.0, model), &input, block);
                assert_eq!(out.len(), input.len());
                for (i, (&o, &x)) in out[latency..].iter().zip(&input).enumerate() {
                    assert!(
                        (o - x).abs() < 1e-3,
                        "block {block}, sample {i}: {o} vs {x}"
                    );
                }
            }
        }
    }

    #[test]
    fn other_rates_pass_through() {
        let mut fx = Denoise::new(44100.0);
        let (mut l, mut r) = (noise(1000, 0.5), vowel(1000));
        let (dry_l, dry_r) = (l.clone(), r.clone());
        fx.process(&controls(100.0, Model::DeepFilter), &mut l, &mut r);
        assert_eq!((l, r), (dry_l, dry_r));
    }

    /// CPU time of this thread in seconds, so other load doesn't skew the factor.
    fn cpu_seconds() -> f64 {
        let stat = std::fs::read_to_string("/proc/thread-self/schedstat").unwrap();
        let ns: u64 = stat.split_whitespace().next().unwrap().parse().unwrap();
        ns as f64 / 1e9
    }

    fn rss_kib() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
        pages * 4
    }

    /// `cargo test --release -p hako-fx -- --ignored --nocapture denoise`
    #[test]
    #[ignore]
    fn denoise_benchmark() {
        let rss = rss_kib();
        let start = Instant::now();
        let mut fx = Denoise::new(RATE);
        println!(
            "new: {:.0} ms, {} MiB",
            start.elapsed().as_secs_f64() * 1000.0,
            (rss_kib() - rss) / 1024
        );
        let seconds = 20;
        let input: Vec<f32> = vowel(seconds * RATE as usize)
            .iter()
            .zip(noise_20db(seconds * RATE as usize, 0.0))
            .map(|(v, n)| v + 0.1 * n)
            .collect();
        for model in [Model::DeepFilter, Model::RnNoise, Model::DeepFilter] {
            let (mut l, mut r) = (input.clone(), input.clone());
            let mut blocks: Vec<f64> = Vec::new();
            let start = cpu_seconds();
            for (l, r) in l.chunks_mut(256).zip(r.chunks_mut(256)) {
                let block = Instant::now();
                fx.process(&controls(100.0, model), l, r);
                blocks.push(block.elapsed().as_secs_f64() * 1000.0);
            }
            let total = cpu_seconds() - start;
            // the first hop ends in the second block
            let takeover = blocks[0] + blocks[1];
            blocks.sort_by(f64::total_cmp);
            println!(
                "{}: {:.0}x realtime (CPU time), 256-sample blocks (wall): takeover \
                 {takeover:.2} ms, p99.9 {:.2} ms, max {:.2} ms",
                if model == Model::DeepFilter {
                    "DeepFilterNet 3"
                } else {
                    "RNNoise"
                },
                seconds as f64 / total,
                blocks[blocks.len() * 999 / 1000],
                blocks[blocks.len() - 1]
            );
        }
    }
}
