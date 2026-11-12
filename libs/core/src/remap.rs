//! Host-side mouse button remaps (evdev key codes + turbo).
//!
//! Bindings are applied by the daemon via uinput, not firmware. The UI only
//! edits this catalog over IPC.

use crate::device::DeviceId;
use serde::{Deserialize, Serialize};

/// Linux evdev `KEY_*` / `BTN_*` code (see `<linux/input-event-codes.h>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvdevKey(pub u16);

impl EvdevKey {
    pub const BTN_LEFT: Self = Self(0x110);
    pub const BTN_RIGHT: Self = Self(0x111);
    pub const BTN_MIDDLE: Self = Self(0x112);
    pub const BTN_SIDE: Self = Self(0x113);
    pub const BTN_EXTRA: Self = Self(0x114);
    pub const BTN_FORWARD: Self = Self(0x115);
    pub const BTN_BACK: Self = Self(0x116);
    pub const BTN_TASK: Self = Self(0x117);

    pub fn label(self) -> String {
        key_label(self.0).to_string()
    }

    pub fn is_mouse_button(self) -> bool {
        (0x110..=0x117).contains(&self.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemapMode {
    /// Hold follows the physical button (press/release).
    #[default]
    Tap,
    /// While held, pulse the target at `interval_ms`.
    Turbo,
    /// One press starts pulsing the target every `interval_ms`; the next press stops it.
    Toggle,
    /// Play `steps`; repetition governed by `repeat`.
    Macro,
    /// Swallow the button entirely.
    Disabled,
}

impl RemapMode {
    pub const ALL: [Self; 5] = [
        Self::Tap,
        Self::Turbo,
        Self::Toggle,
        Self::Macro,
        Self::Disabled,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Tap => "Remap",
            Self::Turbo => "Hold to repeat",
            Self::Toggle => "Toggle repeat",
            Self::Macro => "Macro",
            Self::Disabled => "Disabled",
        }
    }

    /// Modes that emit `target` (vs. a macro sequence or nothing).
    pub fn uses_target(self) -> bool {
        matches!(self, Self::Tap | Self::Turbo | Self::Toggle)
    }

    /// Modes that use `interval_ms`.
    pub fn uses_interval(self) -> bool {
        matches!(self, Self::Turbo | Self::Toggle)
    }
}

/// How a macro repeats once triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacroRepeat {
    /// Play once per press.
    #[default]
    Once,
    /// Loop while the button is held (gap = `interval_ms`).
    WhileHeld,
    /// Press starts looping, next press stops (gap = `interval_ms`).
    Toggle,
}

impl MacroRepeat {
    pub const ALL: [Self; 3] = [Self::Once, Self::WhileHeld, Self::Toggle];

    pub fn label(self) -> &'static str {
        match self {
            Self::Once => "Play once",
            Self::WhileHeld => "Repeat while held",
            Self::Toggle => "Toggle loop",
        }
    }
}

/// One recorded key edge. `delay_ms` is the wait *before* this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroStep {
    pub key: EvdevKey,
    pub down: bool,
    #[serde(default)]
    pub delay_ms: u32,
}

pub const MAX_MACRO_STEPS: usize = 256;
pub const MAX_STEP_DELAY_MS: u32 = 10_000;
pub const MIN_INTERVAL_MS: u32 = 10;
pub const MAX_INTERVAL_MS: u32 = 10_000;

/// Codes the daemon's virtual device can emit: keyboard and consumer keys plus
/// mouse buttons. Joystick / gamepad / tablet `BTN_*` ranges stay out so udev
/// doesn't classify the virtual device as one of those.
pub fn emittable(code: u16) -> bool {
    matches!(code, 1..=0xff | 0x110..=0x117 | 0x160..=0x2bf) && !(0x220..=0x223).contains(&code)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemapBinding {
    pub source: EvdevKey,
    pub target: EvdevKey,
    pub mode: RemapMode,
    /// Repeat period (Turbo/Toggle) or gap between macro loops, in ms.
    #[serde(default = "default_interval")]
    pub interval_ms: u32,
    /// Macro sequence (only for [`RemapMode::Macro`]).
    #[serde(default)]
    pub steps: Vec<MacroStep>,
    #[serde(default)]
    pub repeat: MacroRepeat,
}

fn default_interval() -> u32 {
    50
}

impl RemapBinding {
    /// A plain remap of `source` to `target`.
    pub fn tap(source: EvdevKey, target: EvdevKey) -> Self {
        Self {
            source,
            target,
            mode: RemapMode::Tap,
            interval_ms: default_interval(),
            steps: Vec::new(),
            repeat: MacroRepeat::Once,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.source.0 == 0 {
            return Err("source key required".into());
        }
        if self.mode.uses_target() && !emittable(self.target.0) {
            return Err(format!("target key {} cannot be emitted", self.target.0));
        }
        if self.source == self.target && self.mode == RemapMode::Tap {
            return Err("tap remap to the same button is a no-op".into());
        }
        let loops = self.mode.uses_interval()
            || (self.mode == RemapMode::Macro && self.repeat != MacroRepeat::Once);
        if loops && !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&self.interval_ms) {
            return Err(format!(
                "interval must be {MIN_INTERVAL_MS}..={MAX_INTERVAL_MS} ms"
            ));
        }
        if self.mode == RemapMode::Macro {
            if self.steps.is_empty() || self.steps.len() > MAX_MACRO_STEPS {
                return Err(format!("macro needs 1..={MAX_MACRO_STEPS} steps"));
            }
            if self
                .steps
                .iter()
                .any(|s| !emittable(s.key.0) || s.delay_ms > MAX_STEP_DELAY_MS)
            {
                return Err(format!(
                    "macro steps need an emittable key and a delay of at most {MAX_STEP_DELAY_MS} ms"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureRole {
    /// Next mouse-button press on this device becomes the source.
    Source,
    /// Next key or mouse-button press (any input device) becomes the target.
    Target,
    /// Record every key / button edge (with timing) into `DeviceRemaps::recorded`
    /// until `CancelCapture { keep: true }`. Esc stops without being recorded.
    Record,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceRemaps {
    pub device_id: Option<DeviceId>,
    #[serde(default)]
    pub bindings: Vec<RemapBinding>,
    #[serde(default)]
    pub capturing: Option<CaptureRole>,
    /// Source being assigned when `capturing == Target`.
    #[serde(default)]
    pub capture_source: Option<EvdevKey>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// True when the daemon currently grabs the physical device.
    #[serde(default)]
    pub grabbed: bool,
    /// Result of the last `CaptureRole::Record` session (empty until one finishes).
    #[serde(default)]
    pub recorded: Vec<MacroStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRemapParams {
    pub device_id: DeviceId,
    pub binding: RemapBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearRemapParams {
    pub device_id: DeviceId,
    pub source: EvdevKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartCaptureParams {
    pub device_id: DeviceId,
    pub role: CaptureRole,
    /// Required when `role == Target` (which source the next key is for).
    #[serde(default)]
    pub source: Option<EvdevKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelCaptureParams {
    pub device_id: DeviceId,
    /// For a `Record` session: keep what was recorded (stop) instead of discarding.
    #[serde(default)]
    pub keep: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemapCaptured {
    pub device_id: DeviceId,
    pub role: CaptureRole,
    pub key: EvdevKey,
    pub source: Option<EvdevKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemapFile {
    pub version: u32,
    #[serde(default)]
    pub bindings: Vec<RemapBinding>,
}

pub fn clamp_interval(ms: u32) -> u32 {
    ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
}

/// Human-readable Linux key/button name.
pub fn key_label(code: u16) -> &'static str {
    match code {
        0x110 => "Left click",
        0x111 => "Right click",
        0x112 => "Middle click",
        0x113 => "Side button (back)",
        0x114 => "Side button (forward)",
        0x115 => "Forward",
        0x116 => "Back",
        0x117 => "Task",
        1 => "Esc",
        2 => "1",
        3 => "2",
        4 => "3",
        5 => "4",
        6 => "5",
        7 => "6",
        8 => "7",
        9 => "8",
        10 => "9",
        11 => "0",
        12 => "-",
        13 => "=",
        14 => "Backspace",
        15 => "Tab",
        16 => "Q",
        17 => "W",
        18 => "E",
        19 => "R",
        20 => "T",
        21 => "Y",
        22 => "U",
        23 => "I",
        24 => "O",
        25 => "P",
        26 => "[",
        27 => "]",
        28 => "Enter",
        29 => "Left Ctrl",
        30 => "A",
        31 => "S",
        32 => "D",
        33 => "F",
        34 => "G",
        35 => "H",
        36 => "J",
        37 => "K",
        38 => "L",
        39 => ";",
        40 => "'",
        41 => "`",
        42 => "Left Shift",
        43 => "\\",
        44 => "Z",
        45 => "X",
        46 => "C",
        47 => "V",
        48 => "B",
        49 => "N",
        50 => "M",
        51 => ",",
        52 => ".",
        53 => "/",
        54 => "Right Shift",
        56 => "Left Alt",
        57 => "Space",
        58 => "Caps Lock",
        59 => "F1",
        60 => "F2",
        61 => "F3",
        62 => "F4",
        63 => "F5",
        64 => "F6",
        65 => "F7",
        66 => "F8",
        67 => "F9",
        68 => "F10",
        87 => "F11",
        88 => "F12",
        97 => "Right Ctrl",
        99 => "SysRq",
        100 => "Right Alt",
        102 => "Home",
        103 => "Up",
        104 => "Page Up",
        105 => "Left",
        106 => "Right",
        107 => "End",
        108 => "Down",
        109 => "Page Down",
        110 => "Insert",
        111 => "Delete",
        125 => "Left Super",
        126 => "Right Super",
        127 => "Menu",
        70 => "Scroll Lock",
        119 => "Pause",
        113 => "Mute",
        114 => "Volume down",
        115 => "Volume up",
        163 => "Next track",
        164 => "Play/Pause",
        165 => "Previous track",
        _ => "Key",
    }
}

/// Side buttons we always expose in the mouse panel, even before a capture.
pub fn default_mouse_sources() -> [EvdevKey; 2] {
    [EvdevKey::BTN_SIDE, EvdevKey::BTN_EXTRA]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turbo_interval_rejected_out_of_range() {
        let b = RemapBinding {
            mode: RemapMode::Turbo,
            interval_ms: 1,
            ..RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(30)) // A
        };
        assert!(b.validate().is_err());
    }

    #[test]
    fn targets_must_be_emittable() {
        let joystick = RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(0x120)); // BTN_TRIGGER
        assert!(joystick.validate().is_err());
        assert!(RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(0x300))
            .validate()
            .is_err());
        assert!(RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(0x2be))
            .validate()
            .is_ok());
        assert!(!emittable(0x221) && emittable(KEY_A) && emittable(0x117));
    }

    const KEY_A: u16 = 30;

    #[test]
    fn tap_same_button_rejected() {
        let b = RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey::BTN_SIDE);
        assert!(b.validate().is_err());
    }

    #[test]
    fn macro_needs_steps_and_old_toml_loads() {
        let mut b = RemapBinding {
            mode: RemapMode::Macro,
            ..RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(0))
        };
        assert!(b.validate().is_err());
        b.steps = vec![
            MacroStep {
                key: EvdevKey(30),
                down: true,
                delay_ms: 0,
            },
            MacroStep {
                key: EvdevKey(30),
                down: false,
                delay_ms: 20,
            },
        ];
        assert!(b.validate().is_ok());

        // v1 remap files have no steps/repeat fields.
        let old: RemapFile = toml::from_str(
            "version = 1\n[[bindings]]\nsource = 275\ntarget = 30\nmode = \"turbo\"\ninterval_ms = 50\n",
        )
        .unwrap();
        assert_eq!(old.bindings[0].repeat, MacroRepeat::Once);
        assert!(old.bindings[0].steps.is_empty());
    }

    #[test]
    fn labels_for_side_buttons() {
        assert!(EvdevKey::BTN_SIDE.label().to_lowercase().contains("side"));
        assert_eq!(key_label(30), "A");
    }
}
