//! `libhako_fx.so`: the mixer stages PipeWire's filter-chain loads as LADSPA
//! plugins. One plugin per stage hako-core lists in `hako_core::fx`, labelled
//! with its id prefix: stereo ports `In L`, `In R`, `Out L`, `Out R`, then
//! its controls in `AuxEffectKind::fx_controls` order (values verbatim:
//! choices are option indices).

use hako_core::AuxEffectKind;
use std::ffi::{c_char, c_int, c_ulong, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

mod character;
mod denoise;
mod dynamics;
mod pitch;
mod space;
mod vocoder;

/// One running stage.
pub trait Effect: Send {
    /// `controls`: the stage's values in port order, read every block (they
    /// change live). `left` / `right` hold the input and receive the output.
    fn process(&mut self, controls: &[f32], left: &mut [f32], right: &mut [f32]);

    /// Forgets everything heard so far (delay lines, tails, envelopes,
    /// smoothers, model state): the next block runs as on a new instance.
    fn reset(&mut self);

    /// Work `activate` runs on the host's main thread before the `reset` it
    /// asks for, while `process` may still run: building what `reset` swaps
    /// in, so the audio thread neither allocates nor frees. Asked once, when
    /// the plugin is instantiated.
    fn reset_preparer(&mut self) -> Option<Box<dyn Fn() + Send + Sync>> {
        None
    }
}

/// The effect a plugin `label` runs at `rate` Hz.
pub fn effect(label: &str, rate: f32) -> Option<Box<dyn Effect>> {
    Some(match label {
        "denoise" => Box::new(denoise::Denoise::new(rate)),
        "deesser" => Box::new(dynamics::DeEsser::new(rate)),
        "limiter" => Box::new(dynamics::Limiter::new(rate)),
        "distortion" => Box::new(character::Distortion::new(rate)),
        "robot" => Box::new(character::Robot::new(rate)),
        "bitcrusher" => Box::new(character::Bitcrusher::new(rate)),
        "pitch" => Box::new(pitch::PitchShift::new(rate)),
        "autotune" => Box::new(pitch::AutoTune::new(rate)),
        "vocoder" => Box::new(vocoder::Vocoder::new(rate)),
        "reverb" => Box::new(space::Reverb::new(rate)),
        "delay" => Box::new(space::Delay::new(rate)),
        "chorus" => Box::new(space::Chorus::new(rate)),
        "flanger" => Box::new(space::Flanger::new(rate)),
        _ => return None,
    })
}

// LADSPA 1.1 (ladspa.h), the parts a plugin library provides.
const PORT_INPUT: c_int = 0x1;
const PORT_OUTPUT: c_int = 0x2;
const PORT_CONTROL: c_int = 0x4;
const PORT_AUDIO: c_int = 0x8;
const HINT_BOUNDED: c_int = 0x1 | 0x2;
const HINT_LOGARITHMIC: c_int = 0x10;
const HINT_INTEGER: c_int = 0x20;
const PROPERTY_HARD_RT_CAPABLE: c_int = 0x4;
const AUDIO_PORTS: [&str; 4] = ["In L", "In R", "Out L", "Out R"];
/// Arbitrary base for the plugins' LADSPA unique IDs.
const UNIQUE_ID_BASE: c_ulong = 0x4861_6b00;

#[repr(C)]
pub struct RangeHint {
    hint: c_int,
    lower: f32,
    upper: f32,
}

#[repr(C)]
pub struct Descriptor {
    unique_id: c_ulong,
    label: *const c_char,
    properties: c_int,
    name: *const c_char,
    maker: *const c_char,
    copyright: *const c_char,
    port_count: c_ulong,
    port_descriptors: *const c_int,
    port_names: *const *const c_char,
    port_range_hints: *const RangeHint,
    implementation_data: *mut c_void,
    instantiate: unsafe extern "C" fn(*const Descriptor, c_ulong) -> *mut c_void,
    connect_port: unsafe extern "C" fn(*mut c_void, c_ulong, *mut f32),
    activate: Option<unsafe extern "C" fn(*mut c_void)>,
    run: unsafe extern "C" fn(*mut c_void, c_ulong),
    run_adding: Option<unsafe extern "C" fn(*mut c_void, c_ulong)>,
    set_run_adding_gain: Option<unsafe extern "C" fn(*mut c_void, f32)>,
    deactivate: Option<unsafe extern "C" fn(*mut c_void)>,
    cleanup: unsafe extern "C" fn(*mut c_void),
}

/// Built once and never freed, so its pointers stay valid for the host.
struct Descriptors(Vec<Descriptor>);
// SAFETY: the descriptors and everything they point to are immutable.
unsafe impl Send for Descriptors {}
unsafe impl Sync for Descriptors {}

fn leak_str(s: &str) -> *const c_char {
    CString::new(s).expect("no NUL in names").into_raw()
}

fn leak<T>(v: Vec<T>) -> *const T {
    Box::leak(v.into_boxed_slice()).as_ptr()
}

fn descriptors() -> &'static [Descriptor] {
    static DESCRIPTORS: OnceLock<Descriptors> = OnceLock::new();
    &DESCRIPTORS
        .get_or_init(|| {
            let mut all = Vec::new();
            for mut kind in AuxEffectKind::all() {
                let Some(label) = kind.fx_label() else {
                    continue;
                };
                let audio = [
                    PORT_INPUT | PORT_AUDIO,
                    PORT_INPUT | PORT_AUDIO,
                    PORT_OUTPUT | PORT_AUDIO,
                    PORT_OUTPUT | PORT_AUDIO,
                ];
                let mut kinds = audio.to_vec();
                let mut names: Vec<_> = AUDIO_PORTS.iter().map(|n| leak_str(n)).collect();
                let mut hints: Vec<_> = AUDIO_PORTS
                    .iter()
                    .map(|_| RangeHint {
                        hint: 0,
                        lower: 0.0,
                        upper: 0.0,
                    })
                    .collect();
                for (p, _) in kind.fx_params_mut() {
                    kinds.push(PORT_INPUT | PORT_CONTROL);
                    names.push(leak_str(p.name));
                    hints.push(RangeHint {
                        hint: HINT_BOUNDED | if p.log { HINT_LOGARITHMIC } else { 0 },
                        lower: p.min,
                        upper: p.max,
                    });
                }
                for (c, _) in kind.fx_choices_mut() {
                    kinds.push(PORT_INPUT | PORT_CONTROL);
                    names.push(leak_str(c.name));
                    hints.push(RangeHint {
                        hint: HINT_BOUNDED | HINT_INTEGER,
                        lower: 0.0,
                        upper: (c.options.len() - 1) as f32,
                    });
                }
                all.push(Descriptor {
                    unique_id: UNIQUE_ID_BASE + all.len() as c_ulong,
                    label: leak_str(label),
                    properties: PROPERTY_HARD_RT_CAPABLE,
                    name: leak_str(&format!("hako {}", kind.label())),
                    maker: leak_str("hako"),
                    copyright: leak_str("MIT"),
                    port_count: kinds.len() as c_ulong,
                    port_descriptors: leak(kinds),
                    port_names: leak(names),
                    port_range_hints: leak(hints),
                    implementation_data: std::ptr::null_mut(),
                    instantiate,
                    connect_port,
                    activate: Some(activate),
                    run,
                    run_adding: None,
                    set_run_adding_gain: None,
                    deactivate: None,
                    cleanup,
                });
            }
            Descriptors(all)
        })
        .0
}

/// The LADSPA entry point.
#[no_mangle]
pub extern "C" fn ladspa_descriptor(index: c_ulong) -> *const Descriptor {
    usize::try_from(index)
        .ok()
        .and_then(|i| descriptors().get(i))
        .map_or(std::ptr::null(), |d| d as *const Descriptor)
}

struct Instance {
    effect: Box<dyn Effect>,
    ports: Vec<*mut f32>,
    controls: Vec<f32>,
    /// The controls' table defaults, standing in for non-finite values.
    defaults: Vec<f32>,
    /// `Effect::reset_preparer`, run by `activate`.
    prepare: Option<Box<dyn Fn() + Send + Sync>>,
    /// Set by `activate`: the next `run` resets the effect first.
    reset: AtomicBool,
}

unsafe extern "C" fn instantiate(desc: *const Descriptor, rate: c_ulong) -> *mut c_void {
    // SAFETY: the host passes one of our descriptors.
    let desc = unsafe { &*desc };
    // SAFETY: `label` is a NUL-terminated string we leaked.
    let label = unsafe { std::ffi::CStr::from_ptr(desc.label) };
    let Some(label) = label.to_str().ok() else {
        return std::ptr::null_mut();
    };
    let (Some(mut effect), Some((_, defaults))) = (
        effect(label, rate as f32),
        AuxEffectKind::all()
            .into_iter()
            .filter_map(|k| k.fx_controls())
            .find(|(l, _)| *l == label),
    ) else {
        return std::ptr::null_mut();
    };
    let ports = desc.port_count as usize;
    let defaults: Vec<f32> = defaults.into_iter().map(|(_, v)| v).collect();
    Box::into_raw(Box::new(Instance {
        prepare: effect.reset_preparer(),
        effect,
        ports: vec![std::ptr::null_mut(); ports],
        controls: defaults.clone(),
        defaults,
        reset: AtomicBool::new(false),
    }))
    .cast()
}

unsafe extern "C" fn connect_port(handle: *mut c_void, port: c_ulong, data: *mut f32) {
    // SAFETY: `handle` came from `instantiate`.
    let instance = unsafe { &mut *handle.cast::<Instance>() };
    if let Some(slot) = instance.ports.get_mut(port as usize) {
        *slot = data;
    }
}

/// PipeWire's filter-chain calls `deactivate` + `activate` from its main
/// thread whenever one of its streams pauses (an idle strip, a suspended
/// device), so stale tails must not play when it wakes. 0.3.x does not stop
/// the data thread first, so the reset itself waits for the next `run`.
unsafe extern "C" fn activate(handle: *mut c_void) {
    let instance = handle.cast::<Instance>();
    // SAFETY: `handle` came from `instantiate`; `run` borrows `reset` shared
    // only and never touches `prepare`.
    let (prepare, reset) = unsafe { (&(*instance).prepare, &(*instance).reset) };
    if let Some(prepare) = prepare {
        prepare();
    }
    reset.store(true, Ordering::Release);
}

unsafe extern "C" fn run(handle: *mut c_void, samples: c_ulong) {
    let instance = handle.cast::<Instance>();
    // SAFETY: `handle` came from `instantiate`; field by field, as `activate`
    // may set `reset` meanwhile.
    let (effect, ports, controls, defaults, reset) = unsafe {
        (
            &mut (*instance).effect,
            &(*instance).ports,
            &mut (*instance).controls,
            &(*instance).defaults,
            &(*instance).reset,
        )
    };
    if reset.swap(false, Ordering::Acquire) {
        effect.reset();
    }
    let n = samples as usize;
    let [in_l, in_r, out_l, out_r] = [0, 1, 2, 3].map(|i| ports[i]);
    if [in_l, in_r, out_l, out_r].iter().any(|p| p.is_null()) {
        return;
    }
    for ((value, port), default) in controls.iter_mut().zip(&ports[4..]).zip(defaults) {
        if !port.is_null() {
            // SAFETY: a connected control port points at one f32.
            let v = unsafe { **port };
            // `f32::clamp` keeps NaN (and panics on a NaN bound): it would
            // stick in the stages' delay lines, phases and filters.
            *value = if v.is_finite() { v } else { *default };
        }
    }
    // SAFETY: the host connected `n`-sample buffers; inputs may alias outputs
    // (in-place), so copy with memmove semantics before taking the outputs.
    let (left, right) = unsafe {
        std::ptr::copy(in_l, out_l, n);
        std::ptr::copy(in_r, out_r, n);
        (
            std::slice::from_raw_parts_mut(out_l, n),
            std::slice::from_raw_parts_mut(out_r, n),
        )
    };
    // One NaN / Inf would stick in every stage's filters and feedback paths.
    for s in left.iter_mut().chain(right.iter_mut()) {
        *s = if s.is_finite() {
            s.clamp(-16.0, 16.0)
        } else {
            0.0
        };
    }
    effect.process(controls, left, right);
    // Nor may a stage hand one on to the next.
    for s in left.iter_mut().chain(right.iter_mut()) {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
}

unsafe extern "C" fn cleanup(handle: *mut c_void) {
    // SAFETY: `handle` came from `instantiate` and is not used again.
    drop(unsafe { Box::from_raw(handle.cast::<Instance>()) });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Every stage hako-core runs in the plugin is a descriptor with the
    /// ports the filter-chain config names, and runs through the C ABI on
    /// its defaults without producing NaN or blowing up.
    #[test]
    fn every_fx_stage_is_a_working_plugin() {
        let stages: Vec<_> = AuxEffectKind::all()
            .into_iter()
            .filter_map(|k| k.fx_controls())
            .collect();
        assert!(!stages.is_empty());
        let mut index = 0;
        for (label, controls) in &stages {
            let desc = ladspa_descriptor(index);
            assert!(!desc.is_null(), "{label}");
            let desc = unsafe { &*desc };
            assert_eq!(unsafe { CStr::from_ptr(desc.label) }.to_str(), Ok(*label));
            let names: Vec<_> = (0..desc.port_count as usize)
                .map(|i| {
                    unsafe { CStr::from_ptr(*desc.port_names.add(i)) }
                        .to_str()
                        .unwrap()
                })
                .collect();
            let expected: Vec<_> = AUDIO_PORTS
                .iter()
                .copied()
                .chain(controls.iter().map(|(n, _)| *n))
                .collect();
            assert_eq!(names, expected);

            let handle = unsafe { (desc.instantiate)(desc, 48000) };
            assert!(!handle.is_null(), "{label}");
            let n = 1024;
            let input: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
            let (mut in_l, mut in_r) = (input.clone(), input.clone());
            let (mut out_l, mut out_r) = (vec![0.0; n], vec![0.0; n]);
            let mut values: Vec<f32> = controls.iter().map(|(_, v)| *v).collect();
            unsafe {
                (desc.connect_port)(handle, 0, in_l.as_mut_ptr());
                (desc.connect_port)(handle, 1, in_r.as_mut_ptr());
                (desc.connect_port)(handle, 2, out_l.as_mut_ptr());
                (desc.connect_port)(handle, 3, out_r.as_mut_ptr());
                for (i, v) in values.iter_mut().enumerate() {
                    (desc.connect_port)(handle, 4 + i as c_ulong, v);
                }
                for _ in 0..50 {
                    (desc.run)(handle, n as c_ulong);
                }
                (desc.cleanup)(handle);
            }
            assert!(
                out_l
                    .iter()
                    .chain(&out_r)
                    .all(|s| s.is_finite() && s.abs() < 4.0),
                "{label}"
            );
            index += 1;
        }
        assert!(ladspa_descriptor(index).is_null());
    }

    /// A control port the host never connects runs on its table default.
    #[test]
    fn unconnected_controls_start_at_their_defaults() {
        let stages = AuxEffectKind::all()
            .into_iter()
            .filter_map(|k| k.fx_controls());
        for (index, (label, controls)) in stages.enumerate() {
            let desc = unsafe { &*ladspa_descriptor(index as c_ulong) };
            let handle = unsafe { (desc.instantiate)(desc, 48000) };
            let defaults: Vec<f32> = controls.iter().map(|(_, v)| *v).collect();
            let instance = unsafe { &*handle.cast::<Instance>() };
            assert_eq!(instance.controls, defaults, "{label}");
            unsafe { (desc.cleanup)(handle) };
        }
    }

    /// Runs plugin `index` on `values` through the C ABI: 100 blocks of a vowel plus sibilance,
    /// with NaN and ±Inf samples in block 10 if `poison`; the left output.
    fn run_poisoned(index: c_ulong, values: &[f32], poison: bool) -> Vec<f32> {
        let desc = unsafe { &*ladspa_descriptor(index) };
        let handle = unsafe { (desc.instantiate)(desc, 48000) };
        let n = 512;
        let mut values = values.to_vec();
        let mut out = Vec::new();
        for block in 0..100 {
            let mut l: Vec<f32> = (block * n..(block + 1) * n)
                .map(|i| {
                    let t = i as f32 / 48000.0;
                    0.3 * (std::f32::consts::TAU * 300.0 * t).sin()
                        + 0.2 * (std::f32::consts::TAU * 8000.0 * t).sin()
                })
                .collect();
            if poison && block == 10 {
                l[5..8].copy_from_slice(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]);
            }
            let mut r = l.clone();
            unsafe {
                (desc.connect_port)(handle, 0, l.as_mut_ptr());
                (desc.connect_port)(handle, 1, r.as_mut_ptr());
                (desc.connect_port)(handle, 2, l.as_mut_ptr());
                (desc.connect_port)(handle, 3, r.as_mut_ptr());
                for (i, v) in values.iter_mut().enumerate() {
                    (desc.connect_port)(handle, 4 + i as c_ulong, v);
                }
                (desc.run)(handle, n as c_ulong);
            }
            out.extend(l);
        }
        unsafe { (desc.cleanup)(handle) };
        out
    }

    /// Runs `blocks` 512-sample blocks of `signal` (by sample index) in both channels through
    /// `handle` on `values`; the left output.
    fn run_blocks(
        desc: &Descriptor,
        handle: *mut c_void,
        values: &mut [f32],
        blocks: usize,
        signal: impl Fn(usize) -> f32,
    ) -> Vec<f32> {
        let n = 512;
        let mut out = Vec::new();
        for block in 0..blocks {
            let mut l: Vec<f32> = (block * n..(block + 1) * n).map(&signal).collect();
            let mut r = l.clone();
            unsafe {
                (desc.connect_port)(handle, 0, l.as_mut_ptr());
                (desc.connect_port)(handle, 1, r.as_mut_ptr());
                (desc.connect_port)(handle, 2, l.as_mut_ptr());
                (desc.connect_port)(handle, 3, r.as_mut_ptr());
                for (i, v) in values.iter_mut().enumerate() {
                    (desc.connect_port)(handle, 4 + i as c_ulong, v);
                }
                (desc.run)(handle, n as c_ulong);
            }
            out.extend(l);
        }
        out
    }

    /// After `deactivate` + `activate` (PipeWire's reset when a strip pauses) a stage has
    /// forgotten what it heard: silence comes out silent, and a voice comes out exactly as from
    /// a new instance.
    #[test]
    fn activate_resets_every_stage() {
        let voice = |i: usize| {
            let t = i as f32 / 48000.0;
            0.3 * (std::f32::consts::TAU * 300.0 * t).sin()
                + 0.2 * (std::f32::consts::TAU * 8000.0 * t).sin()
        };
        let loud = |i: usize| {
            let t = i as f32 / 48000.0;
            0.8 * (std::f32::consts::TAU * 180.0 * t).sin() + 0.1 * (i as f32 * 1.7).sin()
        };
        let stages = AuxEffectKind::all()
            .into_iter()
            .filter_map(|k| k.fx_controls());
        for (index, (label, controls)) in stages.enumerate() {
            let desc = unsafe { &*ladspa_descriptor(index as c_ulong) };
            let activate = desc.activate.expect(label);
            let defaults: Vec<f32> = controls.iter().map(|(_, v)| *v).collect();
            // Both models, and their pass-through at 0 dB.
            let variants = match label {
                "denoise" => vec![defaults, vec![100.0, 1.0], vec![0.0, 0.0]],
                _ => vec![defaults],
            };
            for mut values in variants {
                let fresh = unsafe { (desc.instantiate)(desc, 48000) };
                unsafe { activate(fresh) };
                let expected = run_blocks(desc, fresh, &mut values, 40, voice);
                unsafe { (desc.cleanup)(fresh) };

                let handle = unsafe { (desc.instantiate)(desc, 48000) };
                let reset = || unsafe {
                    if let Some(deactivate) = desc.deactivate {
                        deactivate(handle);
                    }
                    activate(handle);
                };
                unsafe { activate(handle) };
                run_blocks(desc, handle, &mut values, 40, loud);
                reset();
                let silence = run_blocks(desc, handle, &mut values, 20, |_| 0.0);
                let loudest = silence.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                assert!(loudest < 1e-6, "{label} {values:?}: {loudest} after reset");
                run_blocks(desc, handle, &mut values, 40, loud);
                reset();
                let out = run_blocks(desc, handle, &mut values, 40, voice);
                unsafe { (desc.cleanup)(handle) };
                assert!(
                    out == expected,
                    "{label} {values:?}: differs from a new instance"
                );
            }
        }
    }

    /// A non-finite input sample neither sticks in a stage nor stops it working: the output
    /// stays finite and, once the stage settled, sounds as without it.
    #[test]
    fn a_nan_sample_does_not_poison_a_stage() {
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let stages = AuxEffectKind::all()
            .into_iter()
            .filter_map(|k| k.fx_controls());
        for (index, (label, controls)) in stages.enumerate() {
            let values: Vec<f32> = controls.iter().map(|(_, v)| *v).collect();
            let poisoned = run_poisoned(index as c_ulong, &values, true);
            let clean = run_poisoned(index as c_ulong, &values, false);
            assert!(poisoned.iter().all(|s| s.is_finite()), "{label}");
            let tail = 80 * 512..;
            let (poisoned, clean) = (rms(&poisoned[tail.clone()]), rms(&clean[tail]));
            assert!(
                (poisoned - clean).abs() <= 0.02 * clean + 1e-4,
                "{label}: {poisoned} vs {clean}"
            );
        }
    }

    /// Instantiates plugin `label` at `rate` Hz and runs blocks of the `sizes` given of a
    /// vowel plus sibilance in both channels through the C ABI, block `i` on `values(i)`; the
    /// left output.
    fn run_label(
        label: &str,
        rate: c_ulong,
        sizes: &[usize],
        values: impl Fn(usize) -> Vec<f32>,
    ) -> Vec<f32> {
        let desc = descriptors()
            .iter()
            .find(|d| unsafe { CStr::from_ptr(d.label) }.to_str() == Ok(label))
            .expect(label);
        let handle = unsafe { (desc.instantiate)(desc, rate) };
        assert!(!handle.is_null(), "{label}");
        let mut out = Vec::new();
        for (block, &n) in sizes.iter().enumerate() {
            let mut l: Vec<f32> = (out.len()..out.len() + n)
                .map(|i| {
                    let t = i as f32 / rate as f32;
                    0.3 * (std::f32::consts::TAU * 300.0 * t).sin()
                        + 0.2 * (std::f32::consts::TAU * 8000.0 * t).sin()
                })
                .collect();
            let mut r = l.clone();
            let mut values = values(block);
            unsafe {
                (desc.connect_port)(handle, 0, l.as_mut_ptr());
                (desc.connect_port)(handle, 1, r.as_mut_ptr());
                (desc.connect_port)(handle, 2, l.as_mut_ptr());
                (desc.connect_port)(handle, 3, r.as_mut_ptr());
                for (i, v) in values.iter_mut().enumerate() {
                    (desc.connect_port)(handle, 4 + i as c_ulong, v);
                }
                (desc.run)(handle, n as c_ulong);
            }
            out.extend(l);
        }
        unsafe { (desc.cleanup)(handle) };
        out
    }

    /// A NaN or infinite control value (a host bug, a corrupt config) neither sticks in a
    /// stage nor stops it working: the output stays finite and, once the control is back on
    /// its default, sounds as a new instance that only ever had the default.
    #[test]
    fn a_non_finite_control_does_not_poison_a_stage() {
        let rms = |x: &[f32]| (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt();
        let n = 512;
        // One thread per stage keeps the suite fast.
        std::thread::scope(|scope| {
            for (label, controls) in AuxEffectKind::all()
                .into_iter()
                .filter_map(|k| k.fx_controls())
            {
                scope.spawn(move || {
                    let defaults: Vec<f32> = controls.iter().map(|(_, v)| *v).collect();
                    // 5 poisoned blocks, then 2 s; denoise guards its controls itself and is
                    // slow to load and run in a debug build, so it gets a short tail.
                    let sizes = vec![n; 5 + if label == "denoise" { 7 } else { 188 }];
                    let last = sizes.len() * n - n..;
                    let expected = run_label(label, 48000, &sizes, |_| defaults.clone());
                    let expected = rms(&expected[last.clone()]);
                    for (port, bad) in
                        (0..defaults.len()).flat_map(|p| [f32::NAN, f32::INFINITY].map(|b| (p, b)))
                    {
                        let out = run_label(label, 48000, &sizes, |block| {
                            let mut values = defaults.clone();
                            if block < 5 {
                                values[port] = bad;
                            }
                            values
                        });
                        let name = controls[port].0;
                        assert!(out.iter().all(|s| s.is_finite()), "{label} {name}={bad}");
                        let got = rms(&out[last.clone()]);
                        assert!(
                            (got - expected).abs() <= 0.02 * expected + 1e-4,
                            "{label} {name}={bad}: {got} vs {expected}"
                        );
                    }
                });
            }
        });
    }

    /// Every stage stays finite and bounded with all controls at their minimum or maximum, at
    /// common rates and on odd block sizes, and a nonsense rate does not panic.
    #[test]
    fn every_stage_survives_extreme_controls_rates_and_block_sizes() {
        let sizes: Vec<usize> = [0]
            .into_iter()
            .chain([1; 480])
            .chain([333; 15])
            .chain([4096; 2])
            .chain([0])
            .collect();
        std::thread::scope(|scope| {
            for mut kind in AuxEffectKind::all() {
                let Some(label) = kind.fx_label() else {
                    continue;
                };
                let (mut lows, mut highs) = (Vec::new(), Vec::new());
                for (p, _) in kind.fx_params_mut() {
                    lows.push(p.min);
                    highs.push(p.max);
                }
                for (c, _) in kind.fx_choices_mut() {
                    lows.push(0.0);
                    highs.push((c.options.len() - 1) as f32);
                }
                let defaults: Vec<f32> = kind
                    .fx_controls()
                    .unwrap()
                    .1
                    .iter()
                    .map(|(_, v)| *v)
                    .collect();
                let sizes = &sizes;
                scope.spawn(move || {
                    for rate in [44100, 48000, 96000] {
                        for values in [&lows, &highs] {
                            let out = run_label(label, rate, sizes, |_| values.clone());
                            assert!(
                                out.iter().all(|s| s.is_finite() && s.abs() < 16.0),
                                "{label} at {rate} Hz on {values:?}"
                            );
                        }
                    }
                    for rate in [0.0, 1.0] {
                        let mut fx = effect(label, rate).unwrap();
                        let (mut l, mut r) = (vec![0.5; 1024], vec![-0.5; 1024]);
                        fx.process(&defaults, &mut l, &mut r);
                        assert!(
                            l.iter().chain(&r).all(|s| s.is_finite()),
                            "{label} at {rate} Hz"
                        );
                    }
                });
            }
        });
    }

    /// Each stage reads its controls by position, so the ports must stay in this order
    /// whatever order the hako-core table lists them in.
    #[test]
    fn control_ports_are_in_the_order_the_stages_read_them() {
        let expected: [(&str, &[&str]); 13] = [
            ("denoise", &["max_reduction_db", "model"]),
            ("deesser", &["threshold_db", "freq_hz", "max_reduction_db"]),
            ("limiter", &["ceiling_db", "release_ms"]),
            ("distortion", &["drive_db", "tone_hz", "mix_pct"]),
            ("pitch", &["semitones", "mix_pct"]),
            ("autotune", &["speed_ms", "mix_pct", "key", "scale"]),
            (
                "vocoder",
                &["carrier_hz", "bands", "mix_pct", "carrier", "follow"],
            ),
            ("robot", &["freq_hz", "mix_pct"]),
            ("bitcrusher", &["bits", "downsample", "mix_pct"]),
            (
                "reverb",
                &["room_pct", "damping_pct", "width_pct", "mix_pct"],
            ),
            ("delay", &["time_ms", "feedback_pct", "mix_pct"]),
            ("chorus", &["rate_hz", "depth_ms", "mix_pct"]),
            (
                "flanger",
                &["rate_hz", "depth_ms", "feedback_pct", "mix_pct"],
            ),
        ];
        assert_eq!(descriptors().len(), expected.len());
        for (label, controls) in expected {
            let desc = descriptors()
                .iter()
                .find(|d| unsafe { CStr::from_ptr(d.label) }.to_str() == Ok(label))
                .expect(label);
            let names: Vec<_> = (0..desc.port_count as usize)
                .map(|i| {
                    unsafe { CStr::from_ptr(*desc.port_names.add(i)) }
                        .to_str()
                        .unwrap()
                })
                .collect();
            let expected: Vec<_> = AUDIO_PORTS.iter().chain(controls).copied().collect();
            assert_eq!(names, expected, "{label}");
        }
    }
}
