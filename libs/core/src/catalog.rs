//! Canonical SettingKey catalog and values.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// One line per key: `Variant = "name" | "legacy alias"... => Group`.
macro_rules! setting_keys {
    ($($variant:ident = $name:literal $(| $alias:literal)* => $group:ident,)*) => {
        /// Stable, device-agnostic setting identifiers used in IPC, TOML, and UI.
        /// A driver exposes whichever keys its hardware has (see `SettingMeta`).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum SettingKey { $($variant,)* }

        impl SettingKey {
            pub const ALL: &'static [SettingKey] = &[$(Self::$variant,)*];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name,)* }
            }

            /// Current names plus legacy aliases from older config files.
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($name $(| $alias)* => Some(Self::$variant),)* _ => None }
            }

            /// UI section; the client builds one tab per group present.
            pub fn group(self) -> SettingGroup {
                match self { $(Self::$variant => SettingGroup::$group,)* }
            }
        }
    };
}

setting_keys! {
    Dpi = "dpi" | "mouse_dpi" => Performance,
    ReportRate = "report_rate" | "mouse_report_rate" => Performance,
    AngleSnapping = "angle_snapping" => Performance,
    LiftOffDistance = "lift_off_distance" => Performance,
    GameMode = "game_mode" => Performance,
    Effect = "effect" | "kb_effect" | "mouse_effect" => Lighting,
    Brightness = "brightness" | "kb_brightness" => Lighting,
    Direction = "direction" | "kb_direction" => Lighting,
    Speed = "speed" | "kb_speed" | "mouse_speed" => Lighting,
    ColorR = "color_r" | "kb_color_r" | "mouse_color_r" => Lighting,
    ColorG = "color_g" | "kb_color_g" | "mouse_color_g" => Lighting,
    ColorB = "color_b" | "kb_color_b" | "mouse_color_b" => Lighting,
    ChatMixGame = "chat_mix_game" => Audio,
    ChatMixChat = "chat_mix_chat" => Audio,
    Volume = "volume" => Audio,
    EqPreset = "eq_preset" => Audio,
    EqBand1 = "eq_band_1" => Audio,
    EqBand2 = "eq_band_2" => Audio,
    EqBand3 = "eq_band_3" => Audio,
    EqBand4 = "eq_band_4" => Audio,
    EqBand5 = "eq_band_5" => Audio,
    EqBand6 = "eq_band_6" => Audio,
    EqBand7 = "eq_band_7" => Audio,
    EqBand8 = "eq_band_8" => Audio,
    EqBand9 = "eq_band_9" => Audio,
    EqBand10 = "eq_band_10" => Audio,
    NoiseCancelling = "noise_cancelling" => Audio,
    AuxEnabled = "aux_enabled" => LineIn,
    AuxCapturePercent = "aux_capture_percent" => LineIn,
    MicVolume = "mic_volume" => Microphone,
    MicMute = "mic_mute" => Microphone,
    MicSideTone = "mic_side_tone" => Microphone,
    MicLedBrightness = "mic_led_brightness" => Microphone,
    PmShutdown = "pm_shutdown" => Power,
    WirelessModeRange = "wireless_mode_range" => Power,
    VoicePrompts = "voice_prompts" => Audio,
    VolumeLimiter = "volume_limiter" => Audio,
    NoiseCancellingStartup = "noise_cancelling_startup" => Audio,
    NoiseCancellingButton = "noise_cancelling_button" => Audio,
    MicNoiseFilter = "mic_noise_filter" => Microphone,
    MicAttached = "mic_attached" => Microphone,
    BluetoothOnPower = "bluetooth_on_power" => Power,
    DpiStage1 = "dpi_stage_1" => Performance,
    DpiStage2 = "dpi_stage_2" => Performance,
    DpiStage3 = "dpi_stage_3" => Performance,
    DpiStage4 = "dpi_stage_4" => Performance,
    DpiStage5 = "dpi_stage_5" => Performance,
    DpiStageActive = "dpi_stage_active" => Performance,
    BluetoothCallVolume = "bluetooth_call_volume" => Audio,
    ActuationPoint = "actuation_point" => Performance,
    Debounce = "debounce" => Performance,
    OnboardWrites = "onboard_writes" => Memory,
    SaveToDevice = "save_to_device" => Memory,
}

impl Serialize for SettingKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SettingKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("unknown setting {s:?}")))
    }
}

/// UI section a setting belongs to, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingGroup {
    Performance,
    Lighting,
    Audio,
    LineIn,
    Microphone,
    Power,
    /// The opt-in for settings saved to the device's onboard memory.
    Memory,
}

impl SettingGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Performance => "Performance",
            Self::Lighting => "Lighting",
            Self::Audio => "Audio",
            Self::LineIn => "Line-in",
            Self::Microphone => "Microphone",
            Self::Power => "Power",
            Self::Memory => "Device memory",
        }
    }
}

/// Rename legacy keys (`kb_brightness` -> `brightness`); unknown keys are kept.
pub fn normalize_key(key: String) -> String {
    SettingKey::parse(&key).map_or(key, |k| k.as_str().to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl SettingValue {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(*v),
            Self::Float(v) => Some(*v as i64),
            _ => None,
        }
    }
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(v) => Some(*v),
            Self::Int(v) => Some(*v as f64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(v) => Some(v),
            _ => None,
        }
    }
}

pub type SettingsMap = BTreeMap<String, SettingValue>;

/// One choice in a `Dropdown` control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingOption {
    pub value: SettingValue,
    pub label: String,
}

impl SettingOption {
    pub fn int(value: i64, label: &str) -> Self {
        Self {
            value: SettingValue::Int(value),
            label: label.into(),
        }
    }
    pub fn text(value: &str, label: &str) -> Self {
        Self {
            value: SettingValue::Text(value.into()),
            label: label.into(),
        }
    }
}

/// UI control metadata attached to a setting.
///
/// This is the single source of truth the UI renders from: the daemon owns the
/// real hardware ranges/steps/choices, and the client draws whatever it is told.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingMeta {
    pub key: SettingKey,
    pub label: String,
    pub kind: ControlKind,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub read_only: bool,
    /// Named choices for `Dropdown` controls (empty otherwise).
    #[serde(default)]
    pub options: Vec<SettingOption>,
    /// Optional display unit, e.g. `Hz`, `dpi`, `%`.
    #[serde(default)]
    pub unit: Option<String>,
    /// Supported but not relevant right now (e.g. color while the effect is
    /// `spectrum`): still settable, not drawn.
    #[serde(default)]
    pub hidden: bool,
    /// Writing it changes the device's onboard memory (flash / EEPROM /
    /// profile). The daemon refuses it until the owner turns on
    /// `onboard_writes` for the device.
    #[serde(default)]
    pub persistent: bool,
    /// Known but not offered, with the reason (e.g. the write layout is not
    /// confirmed by enough sources). Drawn disabled; every write is refused.
    #[serde(default)]
    pub blocked: Option<String>,
    /// Shown in a confirmation dialog before a toggle is switched on.
    #[serde(default)]
    pub warning: Option<String>,
}

impl SettingMeta {
    fn base(key: SettingKey, label: &str, kind: ControlKind) -> Self {
        Self {
            key,
            label: label.into(),
            kind,
            min: None,
            max: None,
            step: None,
            read_only: false,
            options: Vec::new(),
            unit: None,
            hidden: false,
            persistent: false,
            blocked: None,
            warning: None,
        }
    }

    pub fn slider(key: SettingKey, label: &str, min: f64, max: f64, step: f64) -> Self {
        Self {
            min: Some(min),
            max: Some(max),
            step: Some(step),
            ..Self::base(key, label, ControlKind::Slider)
        }
    }

    pub fn toggle(key: SettingKey, label: &str) -> Self {
        Self::base(key, label, ControlKind::Toggle)
    }

    pub fn dropdown(key: SettingKey, label: &str, options: Vec<SettingOption>) -> Self {
        Self {
            options,
            ..Self::base(key, label, ControlKind::Dropdown)
        }
    }

    /// A single RGB color channel; the UI groups the R/G/B trio into one picker.
    pub fn color(key: SettingKey, label: &str) -> Self {
        Self {
            min: Some(0.0),
            max: Some(255.0),
            step: Some(1.0),
            ..Self::base(key, label, ControlKind::ColorChannel)
        }
    }

    pub fn readonly(key: SettingKey, label: &str, min: f64, max: f64) -> Self {
        Self {
            min: Some(min),
            max: Some(max),
            read_only: true,
            ..Self::base(key, label, ControlKind::ReadOnly)
        }
    }

    /// Builder: attach a display unit.
    pub fn unit(mut self, unit: &str) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// Builder: keep the control settable but undrawn.
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Builder: the write is saved to the device's onboard memory.
    pub fn persistent(mut self) -> Self {
        self.persistent = true;
        self
    }

    /// Builder: shown but not offered, for `reason`.
    pub fn blocked(mut self, reason: &str) -> Self {
        self.blocked = Some(reason.into());
        self
    }

    /// Builder: confirmation text before a toggle is switched on.
    pub fn warning(mut self, text: &str) -> Self {
        self.warning = Some(text.into());
        self
    }

    /// Check a value against this control's declared type, range and choices.
    pub fn validate(&self, value: &SettingValue) -> Result<(), String> {
        let key = self.key.as_str();
        if self.read_only || self.kind == ControlKind::ReadOnly {
            return Err(format!("{key} is read-only"));
        }
        if let Some(reason) = &self.blocked {
            return Err(format!("{key} is not available: {reason}"));
        }
        match self.kind {
            ControlKind::Toggle => match value {
                SettingValue::Bool(_) => Ok(()),
                _ => Err(format!("{key} expects true/false")),
            },
            ControlKind::Dropdown => {
                if self.options.iter().any(|o| &o.value == value) {
                    Ok(())
                } else {
                    Err(format!("{value:?} is not a choice for {key}"))
                }
            }
            ControlKind::Slider | ControlKind::ColorChannel | ControlKind::ReadOnly => {
                let whole = [self.min, self.max, self.step]
                    .iter()
                    .all(|v| v.is_none_or(|v| v.fract() == 0.0));
                let n = match value {
                    SettingValue::Int(n) => *n as f64,
                    SettingValue::Float(_) if whole => {
                        return Err(format!("{key} expects a whole number"))
                    }
                    SettingValue::Float(f) if f.is_finite() => *f,
                    _ => return Err(format!("{key} expects a finite number")),
                };
                let (min, max) = (self.min.unwrap_or(f64::MIN), self.max.unwrap_or(f64::MAX));
                if n < min || n > max {
                    Err(format!("{key} out of range {min}..={max}"))
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Toggle,
    Slider,
    Dropdown,
    ColorChannel,
    ReadOnly,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_roundtrip_and_legacy_names_migrate() {
        for &k in SettingKey::ALL {
            assert_eq!(SettingKey::parse(k.as_str()), Some(k));
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(serde_json::from_str::<SettingKey>(&json).unwrap(), k);
        }
        assert_eq!(SettingKey::parse("kb_color_r"), Some(SettingKey::ColorR));
        assert_eq!(SettingKey::parse("mouse_effect"), Some(SettingKey::Effect));
        assert_eq!(normalize_key("mouse_dpi".into()), "dpi");
        assert_eq!(normalize_key("something_else".into()), "something_else");
    }

    #[test]
    fn meta_validation_follows_declared_control() {
        let slider = SettingMeta::slider(SettingKey::MicVolume, "Mic", 1.0, 10.0, 1.0);
        assert!(slider.validate(&SettingValue::Int(5)).is_ok());
        assert!(slider.validate(&SettingValue::Int(11)).is_err());
        assert!(slider.validate(&SettingValue::Float(f64::NAN)).is_err());
        assert!(
            slider.validate(&SettingValue::Float(5.0)).is_err(),
            "int slider needs Int"
        );
        assert!(slider.validate(&SettingValue::Bool(true)).is_err());

        let rate = SettingMeta::dropdown(
            SettingKey::ReportRate,
            "Rate",
            vec![
                SettingOption::int(500, "500"),
                SettingOption::int(1000, "1000"),
            ],
        );
        assert!(rate.validate(&SettingValue::Int(1000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(750)).is_err());

        let toggle = SettingMeta::toggle(SettingKey::AuxEnabled, "AUX");
        assert!(toggle.validate(&SettingValue::Bool(false)).is_ok());
        assert!(toggle.validate(&SettingValue::Int(1)).is_err());

        let ro = SettingMeta::readonly(SettingKey::ChatMixGame, "Game", 0.0, 1.0);
        assert!(ro.validate(&SettingValue::Float(0.5)).is_err());
        // Hidden controls stay settable.
        let color = SettingMeta::color(SettingKey::ColorR, "Red").hidden(true);
        assert!(color.validate(&SettingValue::Int(255)).is_ok());
        assert!(color.validate(&SettingValue::Int(256)).is_err());
    }
}
