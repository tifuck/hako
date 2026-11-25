//! App-wide hotkeys: `hotkeys.toml`, the IPC types and validation.
//!
//! A hotkey is a chord of physical keys / buttons (any devices of the seat)
//! that fires an app action. Voice is the first user; the device Bindings
//! pages show every hotkey that can fire from their device.
//! Design: `docs/superpowers/specs/2026-11-25-voice-stt-design.md` (Hotkeys).

use crate::config::{write_atomic, ConfigPaths};
use crate::remap::{key_label, EvdevKey};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

pub const HOTKEYS_VERSION: u32 = 1;
pub const MAX_CHORD_KEYS: usize = 4;
pub const MAX_HOTKEYS: usize = 32;
pub const MAX_HOTKEY_OPS: usize = 16;
pub const DEFAULT_MIN_HOLD_MS: u32 = 150;
pub const DEFAULT_LATCH_MS: u32 = 400;
/// An activate this soon after a stop is contact chatter, not a press.
pub const CHATTER_MS: u32 = 120;
pub const HOLD_MS_RANGE: (u32, u32) = (0, 2000);
pub const LATCH_MS_RANGE: (u32, u32) = (100, 3000);

pub const KEY_ESC: u16 = 1;
pub const KEY_SPACE: u16 = 57;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;

/// Ctrl / Shift / Alt / Super, either side.
pub fn is_modifier(code: u16) -> bool {
    matches!(
        code,
        KEY_LEFTCTRL
            | KEY_RIGHTCTRL
            | KEY_LEFTSHIFT
            | KEY_RIGHTSHIFT
            | KEY_LEFTALT
            | KEY_RIGHTALT
            | KEY_LEFTMETA
            | KEY_RIGHTMETA
    )
}

/// The left-side code of a modifier (`Right Ctrl` -> `Left Ctrl`); other
/// keys unchanged. Used when a binding accepts either side.
pub fn side_neutral(code: u16) -> u16 {
    match code {
        KEY_RIGHTCTRL => KEY_LEFTCTRL,
        KEY_RIGHTSHIFT => KEY_LEFTSHIFT,
        KEY_RIGHTALT => KEY_LEFTALT,
        KEY_RIGHTMETA => KEY_LEFTMETA,
        c => c,
    }
}

/// What a hotkey does. One registry for hotkeys and (later) Jarvis commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    /// Dictate into the focused field.
    VoiceTranscribe,
    /// Dictate with the alternate profile (`[alt]` in `voice.toml`).
    VoiceTranscribeAlt,
    /// Cancel the recording in progress (only live while one runs).
    VoiceCancel,
    /// Speak one Jarvis command without the wake word.
    JarvisListen,
}

impl HotkeyAction {
    pub const ALL: [Self; 4] = [
        Self::VoiceTranscribe,
        Self::VoiceTranscribeAlt,
        Self::VoiceCancel,
        Self::JarvisListen,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::VoiceTranscribe => "Dictate",
            Self::VoiceTranscribeAlt => "Dictate (alternate)",
            Self::VoiceCancel => "Cancel dictation",
            Self::JarvisListen => "Jarvis: listen",
        }
    }

    /// The page that owns the action (shown on device Bindings pages).
    pub fn feature(self) -> &'static str {
        match self {
            Self::JarvisListen => "Jarvis",
            _ => "Voice",
        }
    }

    /// Cancel has no hold / toggle behaviour: it fires on press.
    pub fn is_momentary(self) -> bool {
        self == Self::VoiceCancel
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyMode {
    /// Start on press, stop on release.
    Hold,
    /// Start on one press, stop on the next.
    Toggle,
    /// Start on press; a long press stops on release (push-to-talk), a tap
    /// keeps going until the next press.
    #[default]
    Smart,
}

impl HotkeyMode {
    pub const ALL: [Self; 3] = [Self::Smart, Self::Hold, Self::Toggle];

    pub fn label(self) -> &'static str {
        match self {
            Self::Hold => "Hold",
            Self::Toggle => "Toggle",
            Self::Smart => "Smart",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassThrough {
    /// Read without a grab: the chord also reaches the focused app.
    #[default]
    Listen,
    /// Grab the keyboard and swallow the chord; everything else is forwarded.
    Block,
}

/// Which devices a hotkey listens to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HotkeyScope {
    /// Every keyboard and mouse of the seat; a chord may span devices.
    #[default]
    Any,
    /// One model (remap files are per model too).
    Device { vendor_id: u16, product_id: u16 },
}

impl HotkeyScope {
    pub fn covers(&self, vendor_id: u16, product_id: u16) -> bool {
        match self {
            Self::Any => true,
            Self::Device {
                vendor_id: v,
                product_id: p,
            } => *v == vendor_id && *p == product_id,
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _) | (_, Self::Any) => true,
            (a, b) => a == b,
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_min_hold() -> u32 {
    DEFAULT_MIN_HOLD_MS
}
fn default_latch() -> u32 {
    DEFAULT_LATCH_MS
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    /// Stable id (UI edits by id); `[a-z0-9_-]`, 1..=32 chars.
    pub id: String,
    pub action: HotkeyAction,
    /// Physical keys / buttons, modifiers first, the trigger last. Order is
    /// what the user pressed; matching treats modifiers as a set.
    pub keys: Vec<EvdevKey>,
    #[serde(default)]
    pub mode: HotkeyMode,
    #[serde(default)]
    pub pass: PassThrough,
    #[serde(default)]
    pub scope: HotkeyScope,
    /// Off (default): `Left Ctrl` and `Right Ctrl` are the same key.
    #[serde(default)]
    pub exact_side: bool,
    /// Hold mode: a shorter press is discarded as a brush.
    #[serde(default = "default_min_hold")]
    pub min_hold_ms: u32,
    /// Smart mode: held at least this long = push-to-talk.
    #[serde(default = "default_latch")]
    pub latch_ms: u32,
    /// Also fire when a key of the chord is remapped on that device (the
    /// user chose "both" for the conflict).
    #[serde(default)]
    pub with_remaps: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl HotkeyBinding {
    pub fn new(id: &str, action: HotkeyAction, keys: &[u16]) -> Self {
        Self {
            id: id.into(),
            action,
            keys: keys.iter().map(|k| EvdevKey(*k)).collect(),
            mode: HotkeyMode::default(),
            pass: PassThrough::default(),
            scope: HotkeyScope::Any,
            exact_side: false,
            min_hold_ms: DEFAULT_MIN_HOLD_MS,
            latch_ms: DEFAULT_LATCH_MS,
            with_remaps: false,
            enabled: true,
        }
    }

    /// The chord as matched: side-neutral unless `exact_side`.
    pub fn chord(&self) -> BTreeSet<u16> {
        self.keys
            .iter()
            .map(|k| {
                if self.exact_side {
                    k.0
                } else {
                    side_neutral(k.0)
                }
            })
            .collect()
    }

    /// The key whose press completes the chord: the last non-modifier, or
    /// the last key of a modifier-only chord.
    pub fn trigger(&self) -> Option<EvdevKey> {
        self.keys
            .iter()
            .rev()
            .find(|k| !is_modifier(k.0))
            .or(self.keys.last())
            .copied()
    }

    pub fn modifier_only(&self) -> bool {
        self.keys.iter().all(|k| is_modifier(k.0))
    }

    /// `Ctrl + Space`.
    pub fn chord_label(&self) -> String {
        chord_label(&self.keys, self.exact_side)
    }
}

/// `Ctrl + Space`; with `exact_side`, `Right Ctrl + Space`.
pub fn chord_label(keys: &[EvdevKey], exact_side: bool) -> String {
    keys.iter()
        .map(|k| {
            let code = if exact_side { k.0 } else { side_neutral(k.0) };
            let label = key_label(code);
            if exact_side {
                label.to_string()
            } else {
                label.strip_prefix("Left ").unwrap_or(label).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// `hotkeys.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HotkeysFile {
    pub version: u32,
    #[serde(default)]
    pub bindings: Vec<HotkeyBinding>,
    /// Keys this version does not know: kept when the file is rewritten.
    #[serde(flatten)]
    pub extra: toml::Table,
}

impl Default for HotkeysFile {
    /// `Ctrl + Space` dictates (Smart, Listen); `Esc` cancels.
    fn default() -> Self {
        Self {
            version: HOTKEYS_VERSION,
            bindings: vec![
                HotkeyBinding::new(
                    "dictate",
                    HotkeyAction::VoiceTranscribe,
                    &[KEY_LEFTCTRL, KEY_SPACE],
                ),
                HotkeyBinding::new("cancel", HotkeyAction::VoiceCancel, &[KEY_ESC]),
            ],
            extra: toml::Table::new(),
        }
    }
}

fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

impl HotkeysFile {
    /// Every rule the daemon and the UI agree on; the error names the binding.
    pub fn validate(&self) -> Result<(), String> {
        if self.bindings.len() > MAX_HOTKEYS {
            return Err(format!("at most {MAX_HOTKEYS} hotkeys"));
        }
        let mut ids = BTreeSet::new();
        for b in &self.bindings {
            let name = format!("{} ({})", b.action.label(), b.chord_label());
            if !valid_id(&b.id) {
                return Err(format!("{name}: invalid id {:?}", b.id));
            }
            if !ids.insert(b.id.as_str()) {
                return Err(format!("{name}: duplicate id {:?}", b.id));
            }
            if b.keys.is_empty() || b.keys.len() > MAX_CHORD_KEYS {
                return Err(format!("{name}: a chord has 1 to {MAX_CHORD_KEYS} keys"));
            }
            if b.chord().len() != b.keys.len() {
                return Err(format!("{name}: the same key twice"));
            }
            if b.keys.iter().any(|k| k.0 == 0 || k.0 > 0x2ff) {
                return Err(format!("{name}: not a key or button"));
            }
            if b.action != HotkeyAction::VoiceCancel && b.keys == [EvdevKey(KEY_ESC)] {
                return Err(format!("{name}: Esc alone is reserved for cancel"));
            }
            let (lo, hi) = HOLD_MS_RANGE;
            if !(lo..=hi).contains(&b.min_hold_ms) {
                return Err(format!("{name}: minimum hold must be {lo}..={hi} ms"));
            }
            let (lo, hi) = LATCH_MS_RANGE;
            if !(lo..=hi).contains(&b.latch_ms) {
                return Err(format!("{name}: latch threshold must be {lo}..={hi} ms"));
            }
        }
        let enabled: Vec<_> = self.bindings.iter().filter(|b| b.enabled).collect();
        for (i, a) in enabled.iter().enumerate() {
            for b in &enabled[i + 1..] {
                if a.scope.overlaps(&b.scope) && side_neutral_set(a) == side_neutral_set(b) {
                    return Err(format!(
                        "{} and {} use the same chord ({})",
                        a.action.label(),
                        b.action.label(),
                        a.chord_label()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Hotkeys that can fire from this model, enabled or not.
    pub fn for_device(
        &self,
        vendor_id: u16,
        product_id: u16,
    ) -> impl Iterator<Item = &HotkeyBinding> {
        self.bindings
            .iter()
            .filter(move |b| b.scope.covers(vendor_id, product_id))
    }
}

fn side_neutral_set(b: &HotkeyBinding) -> BTreeSet<u16> {
    b.keys.iter().map(|k| side_neutral(k.0)).collect()
}

/// One edit, from the UI (Voice page or a device Bindings page).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HotkeyOp {
    /// Add, or replace the binding with the same id.
    Set {
        binding: HotkeyBinding,
    },
    Remove {
        id: String,
    },
}

/// Applies `ops` in order to a copy; the result is validated as a whole.
pub fn apply_hotkey_ops(file: &HotkeysFile, ops: &[HotkeyOp]) -> Result<HotkeysFile, String> {
    if ops.len() > MAX_HOTKEY_OPS {
        return Err(format!("at most {MAX_HOTKEY_OPS} edits at once"));
    }
    let mut next = file.clone();
    for op in ops {
        match op {
            HotkeyOp::Set { binding } => {
                match next.bindings.iter_mut().find(|b| b.id == binding.id) {
                    Some(b) => *b = binding.clone(),
                    None => next.bindings.push(binding.clone()),
                }
            }
            HotkeyOp::Remove { id } => {
                let before = next.bindings.len();
                next.bindings.retain(|b| &b.id != id);
                if next.bindings.len() == before {
                    return Err(format!("no hotkey {id:?}"));
                }
            }
        }
    }
    next.validate()?;
    Ok(next)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchHotkeysParams {
    pub ops: Vec<HotkeyOp>,
}

/// Where a binding's presses come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeySource {
    /// Kernel input events (every keyboard / mouse hako can read).
    Evdev,
    /// The desktop's GlobalShortcuts portal (a device hako cannot read).
    Portal,
    /// Nothing can see the chord: only `hako voice ...` / IPC.
    Cli,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HotkeyConflict {
    /// A key of the chord is remapped on this device.
    Remap {
        device: String,
        vendor_id: u16,
        product_id: u16,
        key: EvdevKey,
    },
    /// The desktop has a shortcut on the same chord (GNOME / KDE only).
    Desktop { shortcut: String },
}

/// One binding as the daemon sees it right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HotkeyStatus {
    pub id: String,
    pub source: HotkeySource,
    /// The action's feature is on (Voice enabled, ...): presses do something.
    pub active: bool,
    /// Connected devices that can fire it (names).
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<HotkeyConflict>,
    /// Why it cannot fire (unreadable devices, grab failed, ...).
    #[serde(default)]
    pub problems: Vec<String>,
}

/// An input device as the hotkey reader sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputDeviceInfo {
    pub name: String,
    pub node: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub readable: bool,
}

/// `GetHotkeys` / `PatchHotkeys` reply and `HotkeysUpdated` payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HotkeysState {
    pub file: HotkeysFile,
    #[serde(default)]
    pub status: Vec<HotkeyStatus>,
    #[serde(default)]
    pub inputs: Vec<InputDeviceInfo>,
    /// `hotkeys.toml` could not be read: edits are refused until it is fixed.
    #[serde(default)]
    pub load_error: Option<String>,
}

/// Live state of a `StartHotkeyCapture` session (`HotkeyCapture` events).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HotkeyCaptureState {
    /// Keys down right now, in press order.
    pub held: Vec<EvdevKey>,
    /// The chord at its largest so far; final once `done`.
    pub chord: Vec<EvdevKey>,
    pub done: bool,
    pub cancelled: bool,
}

/// Serialises every read-modify-write of `hotkeys.toml` in this process.
static HOTKEYS_LOCK: Mutex<()> = Mutex::new(());

pub fn hotkeys_path(paths: &ConfigPaths) -> PathBuf {
    paths.root.join("hotkeys.toml")
}

/// Defaults when the file does not exist (nothing is written). An
/// unparsable file is an error, never replaced by defaults.
pub fn load_hotkeys(paths: &ConfigPaths) -> Result<HotkeysFile> {
    let path = hotkeys_path(paths);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HotkeysFile::default()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let file: HotkeysFile =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if file.version > HOTKEYS_VERSION {
        bail!(
            "{} is version {} (this hako reads {HOTKEYS_VERSION})",
            path.display(),
            file.version
        );
    }
    Ok(file)
}

/// Load, edit, validate and save `hotkeys.toml` as one step.
pub fn update_hotkeys(
    paths: &ConfigPaths,
    edit: impl FnOnce(&HotkeysFile) -> Result<HotkeysFile, String>,
) -> Result<HotkeysFile> {
    let _guard = HOTKEYS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let file = load_hotkeys(paths)?;
    let next = edit(&file).map_err(anyhow::Error::msg)?;
    next.validate().map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(&paths.root)?;
    write_atomic(&hotkeys_path(paths), &toml::to_string_pretty(&next)?)?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_file_is_valid_and_round_trips() {
        let f = HotkeysFile::default();
        f.validate().unwrap();
        let text = toml::to_string_pretty(&f).unwrap();
        assert_eq!(toml::from_str::<HotkeysFile>(&text).unwrap(), f);
        assert_eq!(f.bindings[0].chord_label(), "Ctrl + Space");
    }

    #[test]
    fn validation_rejects_bad_chords() {
        let mut f = HotkeysFile::default();
        f.bindings[0].keys = vec![EvdevKey(KEY_ESC)];
        assert!(f.validate().unwrap_err().contains("Esc alone"));

        let mut f = HotkeysFile::default();
        f.bindings[0].keys = vec![EvdevKey(KEY_LEFTCTRL), EvdevKey(KEY_LEFTCTRL)];
        assert!(f.validate().unwrap_err().contains("same key"));

        let mut f = HotkeysFile::default();
        f.bindings[0].keys = vec![];
        assert!(f.validate().is_err());

        // Right Ctrl + Space is the same chord when either side counts.
        let mut f = HotkeysFile::default();
        let mut alt = HotkeyBinding::new(
            "alt",
            HotkeyAction::VoiceTranscribeAlt,
            &[KEY_RIGHTCTRL, KEY_SPACE],
        );
        f.bindings.push(alt.clone());
        assert!(f.validate().unwrap_err().contains("same chord"));
        // ... but not on another device scope pair that cannot overlap.
        alt.scope = HotkeyScope::Device {
            vendor_id: 1,
            product_id: 2,
        };
        f.bindings[0].scope = HotkeyScope::Device {
            vendor_id: 3,
            product_id: 4,
        };
        f.bindings[2] = alt;
        f.validate().unwrap();
    }

    #[test]
    fn ops_apply_all_or_nothing() {
        let f = HotkeysFile::default();
        let mut b = f.bindings[0].clone();
        b.mode = HotkeyMode::Hold;
        let next = apply_hotkey_ops(
            &f,
            &[
                HotkeyOp::Set { binding: b },
                HotkeyOp::Remove {
                    id: "cancel".into(),
                },
            ],
        )
        .unwrap();
        assert_eq!(next.bindings.len(), 1);
        assert_eq!(next.bindings[0].mode, HotkeyMode::Hold);
        assert!(apply_hotkey_ops(&f, &[HotkeyOp::Remove { id: "x".into() }]).is_err());
    }

    #[test]
    fn trigger_and_modifier_only() {
        let b = HotkeyBinding::new(
            "a",
            HotkeyAction::VoiceTranscribe,
            &[KEY_LEFTCTRL, KEY_SPACE],
        );
        assert_eq!(b.trigger(), Some(EvdevKey(KEY_SPACE)));
        assert!(!b.modifier_only());
        let m = HotkeyBinding::new(
            "b",
            HotkeyAction::VoiceTranscribe,
            &[KEY_LEFTCTRL, KEY_LEFTMETA],
        );
        assert_eq!(m.trigger(), Some(EvdevKey(KEY_LEFTMETA)));
        assert!(m.modifier_only());
    }

    #[test]
    fn scope_covers() {
        let s = HotkeyScope::Device {
            vendor_id: 0x1532,
            product_id: 0x026b,
        };
        assert!(s.covers(0x1532, 0x026b));
        assert!(!s.covers(0x1532, 0x0000));
        assert!(HotkeyScope::Any.covers(1, 2));
    }
}
