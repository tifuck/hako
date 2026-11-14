//! The stages hako's own LADSPA plugin (`libs/fx`) runs: one table of their
//! controls drives validation, the editors, the filter-chain controls and the
//! plugin's ports. A control's port name is its field name; the plugin's
//! label is the stage's [`AuxEffectKind::id_prefix`].

use crate::ipc::AuxEffectKind;

/// A numeric control.
#[derive(Debug, PartialEq)]
pub struct FxParam {
    /// Field and control port name.
    pub name: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: &'static str,
    /// Logarithmic slider.
    pub log: bool,
}

/// A control picking one of `options`; its value is the option's index.
#[derive(Debug, PartialEq)]
pub struct FxChoice {
    pub name: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub options: &'static [&'static str],
    pub default: u8,
}

const fn param(
    name: &'static str,
    label: &'static str,
    help: &'static str,
    (min, max, default): (f32, f32, f32),
    unit: &'static str,
    log: bool,
) -> FxParam {
    FxParam {
        name,
        label,
        help,
        min,
        max,
        default,
        unit,
        log,
    }
}

const fn mix(default: f32) -> FxParam {
    param(
        "mix_pct",
        "Mix",
        "How much of the effect you hear. 100% is effect only, lower blends in the dry voice.",
        (0.0, 100.0, default),
        "%",
        false,
    )
}

const DENOISE_REDUCTION: FxParam = param(
    "max_reduction_db",
    "Strength",
    "The most the noise is turned down. 100 dB removes everything that is not a voice; lower keeps a little of the room so the voice sounds natural.",
    (0.0, 100.0, 100.0),
    "dB",
    false,
);
const DENOISE_MODEL: FxChoice = FxChoice {
    name: "model",
    label: "Model",
    help: "DeepFilterNet 3 removes the most noise (keyboards, fans, voices in the room) and adds about 40 ms delay. RNNoise is lighter on the CPU and adds about 20 ms, but leaves more noise. Both run on the processor, no GPU needed.",
    options: &["DeepFilterNet 3 (best)", "RNNoise (light)"],
    default: 0,
};

const DEESS_THRESHOLD: FxParam = param(
    "threshold_db",
    "Threshold",
    "Hiss louder than this in the sibilance range gets turned down.",
    (-60.0, 0.0, -30.0),
    "dB",
    false,
);
const DEESS_FREQ: FxParam = param(
    "freq_hz",
    "Frequency",
    "Where the S and T sounds start. Only the sound above this is turned down.",
    (2000.0, 12000.0, 6000.0),
    "Hz",
    true,
);
const DEESS_REDUCTION: FxParam = param(
    "max_reduction_db",
    "Max reduction",
    "The most the sibilance is turned down.",
    (0.0, 24.0, 12.0),
    "dB",
    false,
);

const LIMIT_CEILING: FxParam = param(
    "ceiling_db",
    "Ceiling",
    "Nothing gets louder than this. Stops sudden shouts from clipping.",
    (-24.0, 0.0, -1.0),
    "dB",
    false,
);
const LIMIT_RELEASE: FxParam = param(
    "release_ms",
    "Release",
    "How fast the level comes back after a peak.",
    (1.0, 1000.0, 50.0),
    "ms",
    true,
);

const DIST_DRIVE: FxParam = param(
    "drive_db",
    "Drive",
    "How hard the sound is pushed into saturation. Low values add warmth, high values crunch.",
    (0.0, 40.0, 12.0),
    "dB",
    false,
);
const DIST_TONE: FxParam = param(
    "tone_hz",
    "Tone",
    "Cuts the harsh highs the distortion adds above this frequency.",
    (500.0, 16000.0, 6000.0),
    "Hz",
    true,
);

const PITCH_SEMITONES: FxParam = param(
    "semitones",
    "Pitch",
    "Moves the voice up or down. 12 semitones is one octave. Lower the mix to sing in harmony with yourself.",
    (-12.0, 12.0, -4.0),
    "st",
    false,
);

const TUNE_SPEED: FxParam = param(
    "speed_ms",
    "Retune speed",
    "How fast notes snap to the scale. 0 ms gives the hard robotic effect, 50 ms and up sounds natural.",
    (0.0, 200.0, 10.0),
    "ms",
    false,
);
const TUNE_KEY: FxChoice = FxChoice {
    name: "key",
    label: "Key",
    help: "The key of the song. Ignored by the chromatic scale.",
    options: &[
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ],
    default: 0,
};
const TUNE_SCALE: FxChoice = FxChoice {
    name: "scale",
    label: "Scale",
    help: "The notes the voice snaps to. Chromatic uses all twelve.",
    options: &["Chromatic", "Major", "Minor", "Pentatonic"],
    default: 0,
};

const VOCODER_CARRIER_HZ: FxParam = param(
    "carrier_hz",
    "Carrier pitch",
    "The note the vocoder sings at. Ignored while it follows your voice.",
    (40.0, 800.0, 110.0),
    "Hz",
    true,
);
const VOCODER_BANDS: FxParam = param(
    "bands",
    "Bands",
    "More bands make words clearer, fewer sound more synthetic.",
    (8.0, 32.0, 16.0),
    "",
    false,
);
const VOCODER_CARRIER: FxChoice = FxChoice {
    name: "carrier",
    label: "Carrier",
    help: "The synth sound your voice shapes. Noise gives a whisper.",
    options: &["Saw", "Square", "Noise"],
    default: 0,
};
const VOCODER_FOLLOW: FxChoice = FxChoice {
    name: "follow",
    label: "Pitch",
    help: "Sing at one fixed note, or follow the pitch of your voice.",
    options: &["Fixed", "Follow voice"],
    default: 0,
};

const ROBOT_FREQ: FxParam = param(
    "freq_hz",
    "Frequency",
    "The tone your voice is modulated with. Low values sound metallic, high values bell-like.",
    (20.0, 1000.0, 50.0),
    "Hz",
    true,
);

const CRUSH_BITS: FxParam = param(
    "bits",
    "Bits",
    "Fewer bits add more grit and noise.",
    (2.0, 16.0, 8.0),
    "",
    false,
);
const CRUSH_DOWNSAMPLE: FxParam = param(
    "downsample",
    "Downsample",
    "Holds every sample this many times, for an old-console, low-rate sound.",
    (1.0, 32.0, 4.0),
    "×",
    false,
);

const REVERB_ROOM: FxParam = param(
    "room_pct",
    "Room size",
    "Small values sound like a booth, large ones like a hall.",
    (0.0, 100.0, 50.0),
    "%",
    false,
);
const REVERB_DAMPING: FxParam = param(
    "damping_pct",
    "Damping",
    "How fast the high frequencies die away: soft rooms damp more than tiled ones.",
    (0.0, 100.0, 50.0),
    "%",
    false,
);
const REVERB_WIDTH: FxParam = param(
    "width_pct",
    "Width",
    "Stereo spread of the reflections.",
    (0.0, 100.0, 100.0),
    "%",
    false,
);

const DELAY_TIME: FxParam = param(
    "time_ms",
    "Time",
    "Time between echoes.",
    (1.0, 2000.0, 300.0),
    "ms",
    true,
);
const DELAY_FEEDBACK: FxParam = param(
    "feedback_pct",
    "Feedback",
    "How much of each echo comes back again. Higher values repeat longer.",
    (0.0, 95.0, 35.0),
    "%",
    false,
);

const MOD_RATE: FxParam = param(
    "rate_hz",
    "Rate",
    "Speed of the sweep.",
    (0.05, 5.0, 0.8),
    "Hz",
    true,
);
const CHORUS_DEPTH: FxParam = param(
    "depth_ms",
    "Depth",
    "How far the copies drift from your voice. More depth sounds wider and more detuned.",
    (0.5, 10.0, 3.0),
    "ms",
    false,
);
const FLANGER_DEPTH: FxParam = param(
    "depth_ms",
    "Depth",
    "How far the sweep reaches.",
    (0.1, 5.0, 2.0),
    "ms",
    false,
);
const FLANGER_FEEDBACK: FxParam = param(
    "feedback_pct",
    "Feedback",
    "Makes the sweep sharper and more resonant. Negative values sound hollow.",
    (-95.0, 95.0, 50.0),
    "%",
    false,
);

const MIX_FULL: FxParam = mix(100.0);
const MIX_HALF: FxParam = mix(50.0);
const MIX_REVERB: FxParam = mix(25.0);
const MIX_DELAY: FxParam = mix(30.0);

impl AuxEffectKind {
    /// Every stage the UI offers, with defaults, in menu order.
    pub fn all() -> Vec<Self> {
        let mut all = vec![
            Self::NoiseSuppression {
                max_reduction_db: 0.0,
                model: 0,
            },
            Self::default_gate(),
            Self::DeEsser {
                threshold_db: 0.0,
                freq_hz: 0.0,
                max_reduction_db: 0.0,
            },
            Self::default_compressor(),
            Self::Limiter {
                ceiling_db: 0.0,
                release_ms: 0.0,
            },
            Self::default_equalizer(),
            Self::Distortion {
                drive_db: 0.0,
                tone_hz: 0.0,
                mix_pct: 0.0,
            },
            Self::PitchShift {
                semitones: 0.0,
                mix_pct: 0.0,
            },
            Self::AutoTune {
                speed_ms: 0.0,
                mix_pct: 0.0,
                key: 0,
                scale: 0,
            },
            Self::Vocoder {
                carrier_hz: 0.0,
                bands: 0.0,
                mix_pct: 0.0,
                carrier: 0,
                follow: 0,
            },
            Self::Robot {
                freq_hz: 0.0,
                mix_pct: 0.0,
            },
            Self::Bitcrusher {
                bits: 0.0,
                downsample: 0.0,
                mix_pct: 0.0,
            },
            Self::Reverb {
                room_pct: 0.0,
                damping_pct: 0.0,
                width_pct: 0.0,
                mix_pct: 0.0,
            },
            Self::Delay {
                time_ms: 0.0,
                feedback_pct: 0.0,
                mix_pct: 0.0,
            },
            Self::Chorus {
                rate_hz: 0.0,
                depth_ms: 0.0,
                mix_pct: 0.0,
            },
            Self::Flanger {
                rate_hz: 0.0,
                depth_ms: 0.0,
                feedback_pct: 0.0,
                mix_pct: 0.0,
            },
        ];
        for kind in &mut all {
            for (p, value) in kind.fx_params_mut() {
                *value = p.default;
            }
            for (c, value) in kind.fx_choices_mut() {
                *value = c.default;
            }
        }
        all
    }

    /// Heading of the add-stage menu section it is listed under.
    pub fn group(&self) -> &'static str {
        match self {
            Self::NoiseSuppression { .. } | Self::NoiseGate { .. } | Self::DeEsser { .. } => {
                "Clean up"
            }
            Self::Compressor { .. } | Self::Limiter { .. } => "Dynamics",
            Self::Equalizer { .. } | Self::Distortion { .. } => "Tone",
            Self::PitchShift { .. } | Self::AutoTune { .. } => "Pitch",
            Self::Vocoder { .. } | Self::Robot { .. } | Self::Bitcrusher { .. } => "Character",
            Self::Reverb { .. }
            | Self::Delay { .. }
            | Self::Chorus { .. }
            | Self::Flanger { .. } => "Space",
        }
    }

    /// The plugin label of a stage hako's LADSPA plugin runs.
    pub fn fx_label(&self) -> Option<&'static str> {
        let builtin = matches!(
            self,
            Self::NoiseGate { .. } | Self::Compressor { .. } | Self::Equalizer { .. }
        );
        (!builtin).then(|| self.id_prefix())
    }

    /// Numeric controls in port order (before the choices).
    pub fn fx_params_mut(&mut self) -> Vec<(&'static FxParam, &mut f32)> {
        match self {
            Self::NoiseGate { .. } | Self::Compressor { .. } | Self::Equalizer { .. } => vec![],
            Self::NoiseSuppression {
                max_reduction_db, ..
            } => vec![(&DENOISE_REDUCTION, max_reduction_db)],
            Self::DeEsser {
                threshold_db,
                freq_hz,
                max_reduction_db,
            } => vec![
                (&DEESS_THRESHOLD, threshold_db),
                (&DEESS_FREQ, freq_hz),
                (&DEESS_REDUCTION, max_reduction_db),
            ],
            Self::Limiter {
                ceiling_db,
                release_ms,
            } => vec![(&LIMIT_CEILING, ceiling_db), (&LIMIT_RELEASE, release_ms)],
            Self::Distortion {
                drive_db,
                tone_hz,
                mix_pct,
            } => vec![
                (&DIST_DRIVE, drive_db),
                (&DIST_TONE, tone_hz),
                (&MIX_FULL, mix_pct),
            ],
            Self::PitchShift { semitones, mix_pct } => {
                vec![(&PITCH_SEMITONES, semitones), (&MIX_FULL, mix_pct)]
            }
            Self::AutoTune {
                speed_ms, mix_pct, ..
            } => vec![(&TUNE_SPEED, speed_ms), (&MIX_FULL, mix_pct)],
            Self::Vocoder {
                carrier_hz,
                bands,
                mix_pct,
                ..
            } => vec![
                (&VOCODER_CARRIER_HZ, carrier_hz),
                (&VOCODER_BANDS, bands),
                (&MIX_FULL, mix_pct),
            ],
            Self::Robot { freq_hz, mix_pct } => vec![(&ROBOT_FREQ, freq_hz), (&MIX_FULL, mix_pct)],
            Self::Bitcrusher {
                bits,
                downsample,
                mix_pct,
            } => vec![
                (&CRUSH_BITS, bits),
                (&CRUSH_DOWNSAMPLE, downsample),
                (&MIX_FULL, mix_pct),
            ],
            Self::Reverb {
                room_pct,
                damping_pct,
                width_pct,
                mix_pct,
            } => vec![
                (&REVERB_ROOM, room_pct),
                (&REVERB_DAMPING, damping_pct),
                (&REVERB_WIDTH, width_pct),
                (&MIX_REVERB, mix_pct),
            ],
            Self::Delay {
                time_ms,
                feedback_pct,
                mix_pct,
            } => vec![
                (&DELAY_TIME, time_ms),
                (&DELAY_FEEDBACK, feedback_pct),
                (&MIX_DELAY, mix_pct),
            ],
            Self::Chorus {
                rate_hz,
                depth_ms,
                mix_pct,
            } => vec![
                (&MOD_RATE, rate_hz),
                (&CHORUS_DEPTH, depth_ms),
                (&MIX_HALF, mix_pct),
            ],
            Self::Flanger {
                rate_hz,
                depth_ms,
                feedback_pct,
                mix_pct,
            } => vec![
                (&MOD_RATE, rate_hz),
                (&FLANGER_DEPTH, depth_ms),
                (&FLANGER_FEEDBACK, feedback_pct),
                (&MIX_HALF, mix_pct),
            ],
        }
    }

    /// Choice controls in port order (after the numeric ones).
    pub fn fx_choices_mut(&mut self) -> Vec<(&'static FxChoice, &mut u8)> {
        match self {
            Self::NoiseSuppression { model, .. } => vec![(&DENOISE_MODEL, model)],
            Self::AutoTune { key, scale, .. } => vec![(&TUNE_KEY, key), (&TUNE_SCALE, scale)],
            Self::Vocoder {
                carrier, follow, ..
            } => vec![(&VOCODER_CARRIER, carrier), (&VOCODER_FOLLOW, follow)],
            _ => vec![],
        }
    }

    /// `(plugin label, [(port, value)])` of a stage hako's plugin runs.
    pub fn fx_controls(&self) -> Option<(&'static str, Vec<(&'static str, f32)>)> {
        let label = self.fx_label()?;
        let mut kind = self.clone();
        let mut controls: Vec<_> = kind
            .fx_params_mut()
            .into_iter()
            .map(|(p, value)| (p.name, *value))
            .collect();
        controls.extend(
            kind.fx_choices_mut()
                .into_iter()
                .map(|(c, value)| (c.name, f32::from(*value))),
        );
        Some((label, controls))
    }

    /// This stage with `trim_db` of gain moved from before it to after it,
    /// sounding the same; `None` when it cannot (level-dependent stages
    /// without a threshold to move, or one that would leave its range).
    pub fn level_shifted(&self, trim_db: f32) -> Option<Self> {
        let mut kind = self.clone();
        let (threshold, min, max) = match &mut kind {
            Self::NoiseGate { threshold_db, .. }
            | Self::Compressor { threshold_db, .. }
            | Self::DeEsser { threshold_db, .. } => (threshold_db, -60.0, 0.0),
            Self::Limiter { ceiling_db, .. } => (ceiling_db, -24.0, 0.0),
            Self::Equalizer { .. }
            | Self::PitchShift { .. }
            | Self::Vocoder { .. }
            | Self::Robot { .. }
            | Self::Reverb { .. }
            | Self::Delay { .. }
            | Self::Chorus { .. }
            | Self::Flanger { .. } => return Some(kind),
            Self::NoiseSuppression { .. }
            | Self::AutoTune { .. }
            | Self::Distortion { .. }
            | Self::Bitcrusher { .. } => return None,
        };
        *threshold -= trim_db;
        (min..=max).contains(threshold).then_some(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{AuxEffect, AuxEffectChain};

    #[test]
    fn control_names_are_the_field_names() {
        for kind in AuxEffectKind::all() {
            let Some((_, controls)) = kind.fx_controls() else {
                continue;
            };
            let serde_json::Value::Object(fields) = serde_json::to_value(&kind).unwrap() else {
                unreachable!()
            };
            let mut fields: Vec<_> = fields.keys().filter(|k| *k != "type").cloned().collect();
            let mut names: Vec<_> = controls.iter().map(|(n, _)| n.to_string()).collect();
            fields.sort();
            names.sort();
            assert_eq!(fields, names, "{}", kind.label());
        }
    }

    #[test]
    fn defaults_are_valid_and_labels_unique() {
        let all = AuxEffectKind::all();
        let chain = AuxEffectChain {
            effects: all
                .iter()
                .map(|kind| AuxEffect {
                    id: kind.id_prefix().into(),
                    enabled: true,
                    kind: kind.clone(),
                })
                .collect(),
        };
        chain.validate().unwrap();
        let mut prefixes: Vec<_> = all.iter().map(|k| k.id_prefix()).collect();
        prefixes.sort();
        prefixes.dedup();
        assert_eq!(prefixes.len(), all.len());
        for kind in &all {
            let mut kind = kind.clone();
            for (p, _) in kind.fx_params_mut() {
                assert!(p.min <= p.default && p.default <= p.max, "{}", p.name);
            }
        }
    }

    #[test]
    fn out_of_range_and_unknown_choices_are_refused() {
        let mut chain = AuxEffectChain {
            effects: vec![AuxEffect {
                id: "tune".into(),
                enabled: true,
                kind: AuxEffectKind::all()
                    .into_iter()
                    .find(|k| matches!(k, AuxEffectKind::AutoTune { .. }))
                    .unwrap(),
            }],
        };
        if let AuxEffectKind::AutoTune { scale, .. } = &mut chain.effects[0].kind {
            *scale = 4;
        }
        assert!(chain.validate().is_err());
        if let AuxEffectKind::AutoTune {
            scale, speed_ms, ..
        } = &mut chain.effects[0].kind
        {
            *scale = 3;
            *speed_ms = f32::NAN;
        }
        assert!(chain.validate().is_err());
    }

    #[test]
    fn level_shift_moves_thresholds_and_refuses_nonlinear_stages() {
        let limiter = AuxEffectKind::Limiter {
            ceiling_db: -1.0,
            release_ms: 50.0,
        };
        // A ceiling of +2 dB is out of range.
        assert_eq!(limiter.level_shifted(-3.0), None);
        assert_eq!(
            limiter.level_shifted(3.0),
            Some(AuxEffectKind::Limiter {
                ceiling_db: -4.0,
                release_ms: 50.0
            })
        );
        let reverb = AuxEffectKind::all()
            .into_iter()
            .find(|k| matches!(k, AuxEffectKind::Reverb { .. }))
            .unwrap();
        assert_eq!(reverb.level_shifted(6.0).as_ref(), Some(&reverb));
        let denoise = AuxEffectKind::all().remove(0);
        assert_eq!(denoise.level_shifted(1.0), None);
    }
}
