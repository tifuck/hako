//! IPC request/response/event types (JSON over length-prefixed frames).

use crate::catalog::{SettingKey, SettingValue};
use crate::device::{DeviceId, DeviceSnapshot, HubState};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Every IPC method. The JSON wire name is the variant name (`"method":"Set"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    Ping,
    Status,
    Health,
    GetState,
    ListDevices,
    GetSnapshot,
    Set,
    /// Several values for one device, written together (a colour's R, G, B).
    SetMany,
    ListProfiles,
    ApplyProfile,
    SaveProfile,
    /// `ProfileNameParams`; also forgets it as the last profile.
    DeleteProfile,
    ResetAudio,
    GetConfig,
    SetConfig,
    GetRemaps,
    SetRemap,
    ClearRemap,
    StartCapture,
    CancelCapture,
    SubscribeEvents,
    /// Like `SubscribeEvents`, for `Meters` frames of `SubscribeMetersParams`.
    SubscribeMeters,
    /// `MixerState` (`crate::mixer`).
    GetMixer,
    /// `PatchMixerParams`; returns the new `MixerState`.
    PatchMixer,
    /// `NetState` (`crate::net`).
    GetNet,
    /// `PatchNetParams`; returns the new `NetState`.
    PatchNet,
}

/// Server-pushed event types (`"type":"DeviceUpdated"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    DeviceUpdated,
    DeviceConnected,
    DeviceDisconnected,
    RemapsUpdated,
    RemapCaptured,
    Error,
    DaemonShuttingDown,
    /// ~30 Hz on a `SubscribeMeters` connection; payload `MeterLevels`.
    Meters,
    /// The mixer file or its status changed; payload `MixerState`.
    MixerUpdated,
    /// LAN audio peers, requests or link stats changed; payload `NetState`.
    NetUpdated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    pub id: serde_json::Value,
    pub method: Method,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub id: serde_json::Value,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcErrorBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEvent {
    #[serde(rename = "type")]
    pub kind: EventKind,
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl IpcResponse {
    /// A result that cannot be serialised is an error, never `ok` with no result.
    pub fn ok(id: serde_json::Value, result: impl Serialize) -> Self {
        match serde_json::to_value(result) {
            Ok(result) => Self {
                id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(e) => Self::err(id, "io", format!("the result could not be serialised: {e}")),
        }
    }

    pub fn err(id: serde_json::Value, code: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(IpcErrorBody {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetParams {
    pub device_id: DeviceId,
    pub key: SettingKey,
    pub value: SettingValue,
}

/// All checked before any is written; the device applies them together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetManyParams {
    pub device_id: DeviceId,
    pub values: Vec<(SettingKey, SettingValue)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileNameParams {
    pub name: String,
}

/// What a level meter measures; the daemon resolves it to a PipeWire node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeterTargetSpec {
    /// Legacy (the headset's line-in page): resolves to nothing; line-in
    /// is a mixer input now.
    AuxInput,
    /// Legacy, like `AuxInput`.
    AuxOutput,
    /// A node by `node.name`; `monitor_sink` meters what a sink plays.
    Node { name: String, monitor_sink: bool },
    /// A mixer input strip, after its fader (`crate::mixer::Input::id`).
    MixerInput { id: String },
    /// A mixer output master, after its fader.
    MixerOutput { id: String },
}

/// Most targets one meter subscription may hold.
pub const MAX_METER_TARGETS: usize = 32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeMetersParams {
    pub targets: Vec<MeterTargetSpec>,
}

/// One `Meters` frame: peak since the previous frame per target, in
/// subscription order (linear, L/R; 0 for a target that is not available).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeterLevels {
    pub levels: Vec<[f32; 2]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotParams {
    #[serde(default)]
    pub device_id: Option<DeviceId>,
}

/// Ordered AUX processing stages. Starts empty; the user adds stages from
/// the headset page.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AuxEffectChain {
    pub effects: Vec<AuxEffect>,
}

/// Most stages an AUX chain may hold.
pub const MAX_AUX_EFFECTS: usize = 16;

impl AuxEffectChain {
    pub fn validate(&self) -> Result<(), String> {
        if self.effects.len() > MAX_AUX_EFFECTS {
            return Err(format!(
                "AUX effect chain cannot contain more than {MAX_AUX_EFFECTS} effects"
            ));
        }

        let mut ids = HashSet::with_capacity(self.effects.len());
        for effect in &self.effects {
            if effect.id.is_empty()
                || !effect
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            {
                return Err(format!("invalid AUX effect ID {:?}", effect.id));
            }
            if !ids.insert(effect.id.as_str()) {
                return Err(format!("duplicate AUX effect ID {:?}", effect.id));
            }

            match &effect.kind {
                AuxEffectKind::NoiseGate {
                    threshold_db,
                    reduction_db,
                    attack_ms,
                    hold_ms,
                    release_ms,
                    detect_hpf_hz,
                } => {
                    validate_finite_range("noise gate threshold", *threshold_db, -60.0, 0.0)?;
                    validate_finite_range("noise gate reduction", *reduction_db, -72.0, 0.0)?;
                    validate_finite_range("noise gate attack", *attack_ms, 0.0, 200.0)?;
                    validate_finite_range("noise gate hold", *hold_ms, 0.0, 1000.0)?;
                    validate_finite_range("noise gate release", *release_ms, 1.0, 2000.0)?;
                    validate_finite_range(
                        "noise gate detector filter",
                        *detect_hpf_hz,
                        0.0,
                        1000.0,
                    )?;
                }
                AuxEffectKind::Compressor {
                    threshold_db,
                    ratio,
                    attack_ms,
                    release_ms,
                    knee_db,
                    makeup_db,
                } => {
                    validate_finite_range("compressor threshold", *threshold_db, -60.0, 0.0)?;
                    validate_finite_range("compressor ratio", *ratio, 1.0, 20.0)?;
                    validate_finite_range("compressor attack", *attack_ms, 0.0, 200.0)?;
                    validate_finite_range("compressor release", *release_ms, 1.0, 2000.0)?;
                    validate_finite_range("compressor knee", *knee_db, -24.0, 0.0)?;
                    validate_finite_range("compressor makeup", *makeup_db, 0.0, 24.0)?;
                }
                AuxEffectKind::Equalizer { bands } => {
                    if bands.is_empty() || bands.len() > MAX_EQ_BANDS {
                        return Err(format!("equalizer needs 1..={MAX_EQ_BANDS} bands"));
                    }
                    for band in bands {
                        validate_finite_range("EQ frequency", band.freq_hz, 20.0, 20000.0)?;
                        validate_finite_range("EQ gain", band.gain_db, -24.0, 24.0)?;
                        validate_finite_range("EQ Q", band.q, 0.1, 10.0)?;
                    }
                }
                kind => {
                    let label = kind.label();
                    let mut kind = kind.clone();
                    for (p, value) in kind.fx_params_mut() {
                        validate_finite_range(
                            &format!("{label} {}", p.label),
                            *value,
                            p.min,
                            p.max,
                        )?;
                    }
                    for (c, value) in kind.fx_choices_mut() {
                        if usize::from(*value) >= c.options.len() {
                            return Err(format!("{label} {}: no option {value}", c.label));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn validate_finite_range(name: &str, value: f32, min: f32, max: f32) -> Result<(), String> {
    if !value.is_finite() || value < min || value > max {
        Err(format!("{name} must be finite and in {min}..={max}"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuxEffect {
    pub id: String,
    pub enabled: bool,
    pub kind: AuxEffectKind,
}

pub const MAX_EQ_BANDS: usize = 8;

/// One processing stage. Time constants are milliseconds, levels are dB.
/// Fields added after v1 carry serde defaults so older config.toml files load.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuxEffectKind {
    NoiseGate {
        threshold_db: f32,
        /// Attenuation applied while the gate is closed.
        #[serde(default = "d_gate_reduction")]
        reduction_db: f32,
        #[serde(default = "d_gate_attack")]
        attack_ms: f32,
        #[serde(default = "d_gate_hold")]
        hold_ms: f32,
        #[serde(default = "d_release")]
        release_ms: f32,
        /// High-pass on the gate's detector only (0 = off): low rumble/hum no
        /// longer opens the gate; the audio itself is not filtered.
        #[serde(default = "d_gate_detect_hpf")]
        detect_hpf_hz: f32,
    },
    Compressor {
        threshold_db: f32,
        ratio: f32,
        #[serde(default = "d_comp_attack")]
        attack_ms: f32,
        #[serde(default = "d_release")]
        release_ms: f32,
        #[serde(default = "d_comp_knee")]
        knee_db: f32,
        #[serde(default)]
        makeup_db: f32,
    },
    /// Parametric EQ, bands applied in order.
    Equalizer {
        bands: Vec<EqBand>,
    },
    // The stages below run in hako's own LADSPA plugin (`libs/fx`); their
    // fields, ranges and help live in `crate::fx`. Choices are option indices.
    NoiseSuppression {
        max_reduction_db: f32,
        model: u8,
    },
    DeEsser {
        threshold_db: f32,
        freq_hz: f32,
        max_reduction_db: f32,
    },
    Limiter {
        ceiling_db: f32,
        release_ms: f32,
    },
    Distortion {
        drive_db: f32,
        tone_hz: f32,
        mix_pct: f32,
    },
    PitchShift {
        semitones: f32,
        mix_pct: f32,
    },
    AutoTune {
        speed_ms: f32,
        mix_pct: f32,
        key: u8,
        scale: u8,
    },
    Vocoder {
        carrier_hz: f32,
        bands: f32,
        mix_pct: f32,
        carrier: u8,
        follow: u8,
    },
    Robot {
        freq_hz: f32,
        mix_pct: f32,
    },
    Bitcrusher {
        bits: f32,
        downsample: f32,
        mix_pct: f32,
    },
    Reverb {
        room_pct: f32,
        damping_pct: f32,
        width_pct: f32,
        mix_pct: f32,
    },
    Delay {
        time_ms: f32,
        feedback_pct: f32,
        mix_pct: f32,
    },
    Chorus {
        rate_hz: f32,
        depth_ms: f32,
        mix_pct: f32,
    },
    Flanger {
        rate_hz: f32,
        depth_ms: f32,
        feedback_pct: f32,
        mix_pct: f32,
    },
}

fn d_gate_reduction() -> f32 {
    -72.0
}
fn d_gate_attack() -> f32 {
    1.0
}
fn d_gate_hold() -> f32 {
    5.0
}
fn d_gate_detect_hpf() -> f32 {
    120.0
}
fn d_release() -> f32 {
    150.0
}
fn d_comp_attack() -> f32 {
    5.0
}
fn d_comp_knee() -> f32 {
    -6.0
}

impl AuxEffectKind {
    pub fn default_gate() -> Self {
        Self::NoiseGate {
            threshold_db: -45.0,
            reduction_db: d_gate_reduction(),
            attack_ms: d_gate_attack(),
            hold_ms: d_gate_hold(),
            release_ms: d_release(),
            detect_hpf_hz: d_gate_detect_hpf(),
        }
    }

    pub fn default_compressor() -> Self {
        Self::Compressor {
            threshold_db: -20.0,
            ratio: 3.0,
            attack_ms: d_comp_attack(),
            release_ms: d_release(),
            knee_db: d_comp_knee(),
            makeup_db: 0.0,
        }
    }

    /// Flat 5-band EQ: low shelf, three bells, high shelf.
    pub fn default_equalizer() -> Self {
        let band = |kind, freq_hz, q| EqBand {
            kind,
            freq_hz,
            gain_db: 0.0,
            q,
        };
        Self::Equalizer {
            bands: vec![
                band(EqBandKind::LowShelf, 100.0, 0.707),
                band(EqBandKind::Peaking, 250.0, 1.0),
                band(EqBandKind::Peaking, 1000.0, 1.0),
                band(EqBandKind::Peaking, 4000.0, 1.0),
                band(EqBandKind::HighShelf, 8000.0, 0.707),
            ],
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::NoiseGate { .. } => "Noise gate",
            Self::Compressor { .. } => "Compressor",
            Self::Equalizer { .. } => "Equalizer",
            Self::NoiseSuppression { .. } => "AI noise removal",
            Self::DeEsser { .. } => "De-esser",
            Self::Limiter { .. } => "Limiter",
            Self::Distortion { .. } => "Distortion",
            Self::PitchShift { .. } => "Pitch shift",
            Self::AutoTune { .. } => "Auto-tune",
            Self::Vocoder { .. } => "Vocoder",
            Self::Robot { .. } => "Robot",
            Self::Bitcrusher { .. } => "Bitcrusher",
            Self::Reverb { .. } => "Reverb",
            Self::Delay { .. } => "Delay",
            Self::Chorus { .. } => "Chorus",
            Self::Flanger { .. } => "Flanger",
        }
    }

    /// Stable prefix for generated effect IDs.
    pub fn id_prefix(&self) -> &'static str {
        match self {
            Self::NoiseGate { .. } => "gate",
            Self::Compressor { .. } => "compressor",
            Self::Equalizer { .. } => "eq",
            Self::NoiseSuppression { .. } => "denoise",
            Self::DeEsser { .. } => "deesser",
            Self::Limiter { .. } => "limiter",
            Self::Distortion { .. } => "distortion",
            Self::PitchShift { .. } => "pitch",
            Self::AutoTune { .. } => "autotune",
            Self::Vocoder { .. } => "vocoder",
            Self::Robot { .. } => "robot",
            Self::Bitcrusher { .. } => "bitcrusher",
            Self::Reverb { .. } => "reverb",
            Self::Delay { .. } => "delay",
            Self::Chorus { .. } => "chorus",
            Self::Flanger { .. } => "flanger",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EqBandKind {
    Peaking,
    LowShelf,
    HighShelf,
    HighPass,
    LowPass,
}

impl EqBandKind {
    pub const ALL: [Self; 5] = [
        Self::LowShelf,
        Self::Peaking,
        Self::HighShelf,
        Self::HighPass,
        Self::LowPass,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Peaking => "Bell",
            Self::LowShelf => "Low shelf",
            Self::HighShelf => "High shelf",
            Self::HighPass => "High pass",
            Self::LowPass => "Low pass",
        }
    }

    /// Pass filters ignore gain.
    pub fn has_gain(self) -> bool {
        !matches!(self, Self::HighPass | Self::LowPass)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct EqBand {
    pub kind: EqBandKind,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// User-editable subset of the daemon app config, surfaced to the Settings page.
/// Regex/discovery internals stay in the daemon; the UI only sees these knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    /// Daemon log level (`error` / `warn` / `info` / `debug` / `trace`).
    /// Applied on next daemon start unless `RUST_LOG` or `--log-level` is set.
    pub log_level: String,
    /// ChatMix takes over the default sink when it comes up.
    pub chatmix_set_default_sink: bool,
    /// Legacy (line-in is a mixer input now): kept so older UIs and
    /// `config.toml` files round-trip; nothing reads it.
    #[serde(default)]
    pub aux_set_default_sink: bool,
    /// Legacy, like `aux_set_default_sink`.
    #[serde(default)]
    pub aux_auto_enable: bool,
    /// The old line-in processing chain: only the migration to the mixer
    /// reads it (the line-in strip's starting chain).
    #[serde(default)]
    pub aux_effects: AuxEffectChain,
    /// Absolute config directory, for display only (ignored on write).
    #[serde(default)]
    pub config_dir: String,
    /// UI color theme id (`hako_ui::theme`); empty = default.
    #[serde(default)]
    pub theme: String,
    /// The UI looks for new GitHub releases (release builds only).
    #[serde(default = "default_true")]
    pub check_updates: bool,
    /// `node.latency` preset of hako's audio nodes: an [`AUDIO_LATENCIES`] name.
    #[serde(default = "default_audio_latency")]
    pub audio_latency: String,
}

fn default_true() -> bool {
    true
}

/// Audio latency presets: (setting value, samples at 48 kHz).
pub const AUDIO_LATENCIES: [(&str, u32); 3] = [("low", 128), ("balanced", 256), ("safe", 512)];

pub fn default_audio_latency() -> String {
    "balanced".into()
}

/// Samples for an `audio_latency` setting value; `None` if unknown.
pub fn audio_latency_samples(name: &str) -> Option<u32> {
    AUDIO_LATENCIES
        .iter()
        .find(|(preset, _)| *preset == name)
        .map(|&(_, samples)| samples)
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            chatmix_set_default_sink: true,
            aux_set_default_sink: false,
            aux_auto_enable: false,
            aux_effects: AuxEffectChain::default(),
            config_dir: String::new(),
            theme: String::new(),
            check_updates: true,
            audio_latency: default_audio_latency(),
        }
    }
}

// Re-export convenience for handlers
pub type FullState = HubState;
pub type Snap = DeviceSnapshot;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unserialisable_result_is_an_error() {
        let bad: std::collections::HashMap<(u8, u8), u8> = [((1, 2), 3)].into();
        let resp = IpcResponse::ok(serde_json::json!(7), bad);
        assert!(!resp.ok && resp.result.is_none());
        assert_eq!(resp.error.unwrap().code, "io");
        assert!(IpcResponse::ok(serde_json::json!(7), 1).ok);
    }
}
