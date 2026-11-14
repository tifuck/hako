//! Cooler Master (VID 0x2516): MasterMouse mice and MasterKeys / CK / SK / MK
//! keyboards. Whole-device lighting: effect, colour, brightness, speed and
//! wave direction.
//!
//! Reimplemented from public notes; only IDs and byte layouts were taken from:
//! - OpenRGB `Controllers/CoolerMasterController` (GPL-2.0, layouts only):
//!   `CoolerMasterControllerDetect.cpp` (PIDs, interfaces),
//!   `CMMMController/CMMMController.{h,cpp}` + `RGBController_CMMMController.{h,cpp}`
//!   (MM530 / MM720 / MM730), `CMMM711Controller/*`, `CMMM712Controller/*`
//!   and `MM712protocol.txt`, `CMKeyboardController/CMKeyboardDevices.h`,
//!   `CMKeyboardAbstractController.cpp`, `CMKeyboardV1Controller.cpp`,
//!   `CMKeyboardV2Controller.cpp`: <https://gitlab.com/CalcProgrammer1/OpenRGB>
//! - libcmmk `src/libcmmk.c`, `include/libcmmk/libcmmk.h` (LGPL-3.0, layouts
//!   only): MasterKeys Pro control modes, effect ids, wave directions and the
//!   0x00-0x50 speed range: <https://github.com/chmod222/libcmmk>
//!
//! `mouse.rs` and `keyboard.rs` hold the pure packet builders; this file holds
//! the model table and the one adapter they share. Every write is volatile:
//! the `50 55` (mice, keyboards) and `54 81 01` (MM712) save commands are never
//! sent, and saved values are re-sent whenever the device (re)connects. None of
//! the modes used here needs re-streaming, so `tick` does nothing.
//!
//! OpenRGB's per-PID `CMKeyboardV2Controller::Initialize` (`51 00 00 00 <p>`
//! profile select, `56 81 .. <p> .. bb` "magic" and the `12` / `42` / `43`
//! startup packets) is only called from `SetLeds` before Direct per-key
//! frames, never from `SetMode`, whose packets `keyboard.rs` mirrors; it is
//! not sent here. Its `51 00` would also switch the live onboard profile.
//!
//! Not listed: SK620 Black / White (0157 / 0159) and MK850 (0069) are
//! registered for OpenRGB's V2 detector but missing from its controller
//! switch, so OpenRGB never drives them; MK750 (0067), SK630 (0089) and
//! SK650 (008d) are V2 in OpenRGB but listed by libcmmk, whose protocol is
//! V1: the sources disagree on the family.
//!
//! Saving to device memory is shown as a blocked `save_to_device` control:
//! each save command has a single source or an unclear target (see
//! `docs/onboard-writes/coolermaster.md` and `Family::save_blocked`).

mod keyboard;
mod mouse;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{anyhow, bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::Duration;
use tracing::{info, warn};

const VID: u16 = 0x2516;

/// Both projects read one reply per command; OpenRGB waits up to 250 ms.
const REPLY_WAIT: Duration = Duration::from_millis(250);

const fn model(kind: DeviceKind, name: &'static str, product_ids: &'static [u16]) -> Model {
    Model {
        vendor_id: VID,
        product_ids,
        kind,
        name,
        usbfs: false,
        multi: false,
        open,
    }
}

use DeviceKind::{Keyboard, Mouse};

/// Most popular first. Each colour variant is its own USB product.
pub const MODELS: &[Model] = &[
    model(Mouse, "Cooler Master MM711", &[0x0101]),
    model(Mouse, "Cooler Master MM712", &[0x0169]),
    model(Mouse, "Cooler Master MM720", &[0x0141]),
    model(Mouse, "Cooler Master MM730", &[0x0165]),
    model(Mouse, "Cooler Master MM530", &[0x0065]),
    model(Keyboard, "Cooler Master CK550 V2", &[0x0145]),
    model(Keyboard, "Cooler Master CK530 V2", &[0x0147]),
    model(Keyboard, "Cooler Master SK622 Black", &[0x0149]),
    model(Keyboard, "Cooler Master SK622 White", &[0x014b]),
    model(Keyboard, "Cooler Master CK550 V1 / CK552", &[0x007f]),
    model(Keyboard, "Cooler Master CK530", &[0x009f]),
    model(Keyboard, "Cooler Master MK730", &[0x008f]),
    model(Keyboard, "Cooler Master SK652", &[0x015d]),
    model(Keyboard, "Cooler Master SK653", &[0x01ab]),
    model(Keyboard, "Cooler Master MasterKeys Pro L", &[0x003b]),
    model(Keyboard, "Cooler Master MasterKeys Pro L White", &[0x0047]),
    model(Keyboard, "Cooler Master MasterKeys Pro S", &[0x003c]),
];

/// Lighting protocol of one model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// MM530 / MM720 / MM730; the effect command byte (`2c` MM5xx, `2b` MM7xx).
    MasterMouse(u8),
    Mm711,
    Mm712,
    /// MasterKeys Pro L / S (OpenRGB `CMKeyboardV1Controller`, libcmmk).
    KeyboardV1,
    /// CK / SK / MK keyboards (OpenRGB `CMKeyboardV2Controller`).
    KeyboardV2,
}

fn family(pid: u16) -> Option<Family> {
    Some(match pid {
        0x0065 => Family::MasterMouse(0x2c),
        0x0141 | 0x0165 => Family::MasterMouse(0x2b),
        0x0101 => Family::Mm711,
        0x0169 => Family::Mm712,
        0x003b | 0x0047 | 0x003c => Family::KeyboardV1,
        0x0145 | 0x0147 | 0x0149 | 0x014b | 0x007f | 0x009f | 0x008f | 0x015d | 0x01ab => {
            Family::KeyboardV2
        }
        _ => return None,
    })
}

impl Family {
    /// USB interface of the vendor collection (usage page 0xFF00, MM712 0xFF0A).
    fn iface(self) -> u32 {
        if self == Family::Mm712 {
            3
        } else {
            1
        }
    }

    fn effects(self) -> &'static [Effect] {
        match self {
            Family::KeyboardV1 => keyboard::V1_EFFECTS,
            Family::KeyboardV2 => keyboard::V2_EFFECTS,
            _ => mouse::EFFECTS,
        }
    }

    /// Sent once after opening, before any lighting command.
    fn init(self) -> Vec<Report> {
        match self {
            Family::Mm711 => vec![report(&[0x41, 0x80])],
            Family::Mm712 => vec![report(&[0x44, 0x81, 0x02])],
            _ => vec![],
        }
    }

    fn packets(self, look: &Look) -> Vec<Report> {
        match self {
            Family::MasterMouse(cmd) => mouse::master_packets(cmd, look),
            Family::Mm711 => mouse::mm711_packets(look),
            Family::Mm712 => mouse::mm712_packets(look),
            Family::KeyboardV1 => keyboard::v1_packets(look),
            Family::KeyboardV2 => keyboard::v2_packets(look),
        }
    }

    /// Why the family's save command is not offered.
    fn save_blocked(self) -> &'static str {
        match self {
            Family::MasterMouse(_) | Family::Mm711 => {
                "only OpenRGB documents the 50 55 save command for this mouse"
            }
            Family::Mm712 => {
                "only OpenRGB documents the 54 81 01 save, and its code and notes frame it \
                 differently"
            }
            Family::KeyboardV1 | Family::KeyboardV2 => {
                "which onboard profile 50 55 overwrites, and what it stores in this \
                 lighting mode, is undocumented"
            }
        }
    }

    fn has_color(self, effect: Effect) -> bool {
        match effect {
            Effect::Static | Effect::Breathing => true,
            // The V2 wave sends no colour; the V1 wave starts from one.
            Effect::Wave => self == Family::KeyboardV1,
            _ => false,
        }
    }

    /// Direction choices for `effect`; empty when it has none.
    fn directions(self, effect: Effect) -> &'static [Dir] {
        match effect {
            Effect::Wave => &[Dir::Left, Dir::Right, Dir::Up, Dir::Down],
            Effect::Circle => &[Dir::Left, Dir::Right],
            _ => &[],
        }
    }
}

/// One 65-byte output report: report id 0, the command, zero fill.
type Report = [u8; 65];

fn report(bytes: &[u8]) -> Report {
    let mut r = [0u8; 65];
    r[1..=bytes.len()].copy_from_slice(bytes);
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    Static,
    Breathing,
    Cycle,
    Wave,
    Circle,
    Off,
}

impl Effect {
    fn as_str(self) -> &'static str {
        match self {
            Effect::Static => "static",
            Effect::Breathing => "breathing",
            Effect::Cycle => "cycle",
            Effect::Wave => "wave",
            Effect::Circle => "circle",
            Effect::Off => "off",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Effect::Static => "Static",
            Effect::Breathing => "Breathing",
            Effect::Cycle => "Color cycle",
            Effect::Wave => "Wave",
            Effect::Circle => "Circle spectrum",
            Effect::Off => "Off",
        }
    }

    fn has_speed(self) -> bool {
        !matches!(self, Effect::Static | Effect::Off)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Dir::Left => "left",
            Dir::Right => "right",
            Dir::Up => "up",
            Dir::Down => "down",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Dir::Left => "Left",
            Dir::Right => "Right",
            Dir::Up => "Up",
            Dir::Down => "Down",
        }
    }
}

/// The user's lighting choice, decoded for the packet builders.
#[derive(Clone, Copy, Debug)]
struct Look {
    effect: Effect,
    /// 0..=100 %.
    brightness: u8,
    /// 0 slow, 1 medium, 2 fast.
    speed: usize,
    dir: Dir,
    rgb: [u8; 3],
}

impl Look {
    fn new(family: Family, s: &SettingsMap) -> Self {
        let text = |k| get_setting(s, k).and_then(|v| v.as_str()).unwrap_or("");
        let int = |k, d| {
            get_setting(s, k)
                .and_then(|v| v.as_int())
                .unwrap_or(d)
                .clamp(0, 255) as u8
        };
        let effects = family.effects();
        Self {
            effect: effects
                .iter()
                .copied()
                .find(|e| e.as_str() == text(SettingKey::Effect))
                .unwrap_or(effects[0]),
            brightness: int(SettingKey::Brightness, 100).min(100),
            speed: match text(SettingKey::Speed) {
                "slow" => 0,
                "fast" => 2,
                _ => 1,
            },
            dir: [Dir::Left, Dir::Right, Dir::Up, Dir::Down]
                .into_iter()
                .find(|d| d.as_str() == text(SettingKey::Direction))
                .unwrap_or(Dir::Left),
            rgb: [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB].map(|k| int(k, 255)),
        }
    }
}

fn defaults(s: &mut SettingsMap) {
    for (k, v) in [
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::Brightness, SettingValue::Int(100)),
        (SettingKey::Speed, SettingValue::Text("medium".into())),
        (SettingKey::Direction, SettingValue::Text("left".into())),
        (SettingKey::ColorR, SettingValue::Int(255)),
        (SettingKey::ColorG, SettingValue::Int(255)),
        (SettingKey::ColorB, SettingValue::Int(255)),
    ] {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

/// Effect -> Brightness -> Direction -> Speed -> Color, irrelevant ones hidden.
fn controls(family: Family, look: &Look) -> Vec<SettingMeta> {
    use SettingKey as K;
    let effect = look.effect;
    let mut c = vec![
        SettingMeta::dropdown(
            K::Effect,
            "Effect",
            family
                .effects()
                .iter()
                .map(|e| SettingOption::text(e.as_str(), e.label()))
                .collect(),
        ),
        SettingMeta::slider(K::Brightness, "Brightness", 0.0, 100.0, 1.0)
            .unit("%")
            .hidden(effect == Effect::Off),
    ];
    if family.effects().contains(&Effect::Wave) {
        let dirs = match family.directions(effect) {
            [] => family.directions(Effect::Wave),
            d => d,
        };
        c.push(
            SettingMeta::dropdown(
                K::Direction,
                "Direction",
                dirs.iter()
                    .map(|d| SettingOption::text(d.as_str(), d.label()))
                    .collect(),
            )
            .hidden(family.directions(effect).is_empty()),
        );
    }
    c.push(
        SettingMeta::dropdown(
            K::Speed,
            "Speed",
            vec![
                SettingOption::text("slow", "Slow"),
                SettingOption::text("medium", "Medium"),
                SettingOption::text("fast", "Fast"),
            ],
        )
        .hidden(!effect.has_speed()),
    );
    for (key, label) in [
        (K::ColorR, "Red"),
        (K::ColorG, "Green"),
        (K::ColorB, "Blue"),
    ] {
        c.push(SettingMeta::color(key, label).hidden(!family.has_color(effect)));
    }
    c.push(
        SettingMeta::toggle(K::SaveToDevice, "Save changes to device memory")
            .persistent()
            .blocked(family.save_blocked()),
    );
    c
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let model = MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .with_context(|| format!("no Cooler Master model for {pid:04x}"))?;
    let family = family(pid).with_context(|| format!("no lighting family for {pid:04x}"))?;
    let info = find_hidraw(VID, pid, Some(family.iface()))?;
    let dev = HidrawDevice::open(&info.path)?;
    let mut settings =
        hako_core::config::load_device_settings(&ctx.paths, VID, model.product_ids[0])?;
    // Only a look the user chose is restored; a fresh device keeps its own.
    let restore = get_setting(&settings, SettingKey::Effect).is_some();
    let known = controls(family, &Look::new(family, &settings));
    crate::drivers::sanitize_saved(&mut settings, &known);
    defaults(&mut settings);
    let mut a = Lighting {
        name: model.name,
        kind: model.kind,
        family,
        canonical: model.product_ids[0],
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        dev: Some(dev),
        settings,
        last_error: None,
        paths: ctx.paths.clone(),
    };
    let init = family.init();
    let result = a
        .send(&init)
        .and_then(|()| if restore { a.apply() } else { Ok(()) });
    if let Err(e) = result {
        warn!("{}: restore lighting: {e}", model.name);
        a.last_error = Some(e.to_string());
    }
    info!("{} open via {}", model.name, info.path.display());
    Ok(vec![Box::new(a)])
}

struct Lighting {
    name: &'static str,
    kind: DeviceKind,
    family: Family,
    /// Names the settings file.
    canonical: u16,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl Lighting {
    /// Write each report and wait for its reply (contents unused; a device
    /// that does not answer is not an error, a failed read is).
    fn send(&mut self, reports: &[Report]) -> Result<()> {
        let dev = self.dev.as_mut().context("not connected")?;
        let mut buf = [0u8; 65];
        for r in reports {
            dev.write(r)?;
            if let Err(e) = dev.read_timeout(&mut buf, REPLY_WAIT) {
                // A timeout is a plain error, a failed read an I/O one.
                if e.downcast_ref::<std::io::Error>().is_some() {
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn apply(&mut self) -> Result<()> {
        let packets = self.family.packets(&Look::new(self.family, &self.settings));
        self.send(&packets)
    }
}

impl DeviceAdapter for Lighting {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: controls(self.family, &Look::new(self.family, &self.settings)),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if key == SettingKey::SaveToDevice {
            // Never offered; switching it off needs no device write.
            if value.as_bool() == Some(false) {
                return Ok(());
            }
            bail!("{}: {}", self.name, self.family.save_blocked());
        }
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        if let Err(e) = self.apply() {
            // Some of the reports may have gone out: send the old look again.
            self.settings = before;
            let e = match self.apply() {
                Ok(()) => e,
                Err(r) => anyhow!("{e:#}; restoring the previous look failed too: {r:#}"),
            };
            self.last_error = Some(format!("{e:#}"));
            return Err(e);
        }
        self.last_error = None;
        let saved = hako_core::config::save_device_settings(
            &self.paths,
            VID,
            self.canonical,
            &self.settings,
        );
        if let Err(e) = saved {
            let e = e.context("applied, but not saved");
            self.last_error = Some(format!("{e:#}"));
            return Err(e);
        }
        Ok(())
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_model_has_a_family_of_its_kind() {
        let mut seen = HashSet::new();
        for m in MODELS {
            assert_eq!(m.vendor_id, VID);
            for &pid in m.product_ids {
                assert!(seen.insert(pid), "{pid:04x} listed twice");
                let f = family(pid).unwrap_or_else(|| panic!("{pid:04x} has no family"));
                let kb = matches!(f, Family::KeyboardV1 | Family::KeyboardV2);
                assert_eq!(kb, m.kind == DeviceKind::Keyboard, "{}", m.name);
            }
        }
        assert_eq!(family(0x0097), None, "MM531 has no captured layout");
    }

    #[test]
    fn default_controls_validate() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        for f in [
            Family::MasterMouse(0x2b),
            Family::Mm711,
            Family::Mm712,
            Family::KeyboardV1,
            Family::KeyboardV2,
        ] {
            for &e in f.effects() {
                merge_setting(
                    &mut s,
                    SettingKey::Effect,
                    SettingValue::Text(e.as_str().into()),
                );
                for c in controls(f, &Look::new(f, &s)) {
                    if let Some(v) = get_setting(&s, c.key) {
                        if !c.hidden {
                            assert!(c.validate(v).is_ok(), "{f:?} {e:?} {:?}", c.key);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn look_decodes_and_falls_back() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        let l = Look::new(Family::KeyboardV2, &s);
        assert_eq!(
            (l.effect, l.brightness, l.speed, l.dir),
            (Effect::Static, 100, 1, Dir::Left)
        );
        assert_eq!(l.rgb, [255, 255, 255]);
        merge_setting(
            &mut s,
            SettingKey::Effect,
            SettingValue::Text("wave".into()),
        );
        merge_setting(&mut s, SettingKey::Speed, SettingValue::Text("fast".into()));
        assert_eq!(Look::new(Family::KeyboardV2, &s).speed, 2);
        // Mice have no wave: the family's first effect applies.
        assert_eq!(Look::new(Family::Mm711, &s).effect, Effect::Static);
        let c = controls(Family::KeyboardV2, &Look::new(Family::KeyboardV2, &s));
        let hidden = |k| c.iter().find(|m| m.key == k).unwrap().hidden;
        assert!(!hidden(SettingKey::Direction) && hidden(SettingKey::ColorR));
        let c = controls(Family::KeyboardV1, &Look::new(Family::KeyboardV1, &s));
        assert!(
            !c.iter()
                .find(|m| m.key == SettingKey::ColorR)
                .unwrap()
                .hidden
        );
        let c = controls(Family::Mm712, &Look::new(Family::Mm712, &s));
        assert!(c.iter().all(|m| m.key != SettingKey::Direction));
    }

    #[test]
    fn saving_is_blocked_and_never_sent() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        for f in [
            Family::MasterMouse(0x2c),
            Family::MasterMouse(0x2b),
            Family::Mm711,
            Family::Mm712,
            Family::KeyboardV1,
            Family::KeyboardV2,
        ] {
            let c = controls(f, &Look::new(f, &s));
            let save = c
                .iter()
                .find(|m| m.key == SettingKey::SaveToDevice)
                .unwrap();
            assert!(save.persistent && save.blocked.is_some(), "{f:?}");
            assert!(c
                .iter()
                .all(|m| m.key == SettingKey::SaveToDevice || !m.persistent));
            let mut packets = f.init();
            for &e in f.effects() {
                let mut l = Look::new(f, &s);
                l.effect = e;
                packets.extend(f.packets(&l));
            }
            for p in packets {
                assert_ne!(p[1..3], [0x50, 0x55], "{f:?} sends a save");
                assert_ne!(p[1..4], [0x54, 0x81, 0x01], "{f:?} sends a save");
            }
        }
    }

    #[test]
    fn failed_write_rolls_back_and_reports_the_failed_restore() {
        let dir = std::env::temp_dir().join(format!("hako-cm-test-{}", std::process::id()));
        let mut settings = SettingsMap::new();
        defaults(&mut settings);
        let mut a = Lighting {
            name: "Test",
            kind: DeviceKind::Mouse,
            family: Family::Mm711,
            canonical: 0x0101,
            id: DeviceId {
                vendor_id: VID,
                product_id: 0x0101,
                instance: "t".into(),
            },
            dev: None,
            settings: settings.clone(),
            last_error: None,
            paths: ConfigPaths {
                root: dir.clone(),
                config_toml: dir.join("config.toml"),
                devices_dir: dir.join("devices"),
                profiles_dir: dir.join("profiles"),
                remaps_dir: dir.join("remaps"),
            },
        };
        let e = a
            .set(SettingKey::Brightness, SettingValue::Int(10))
            .unwrap_err();
        assert!(e.to_string().contains("failed too"), "{e}");
        assert_eq!(a.settings, settings);
        assert!(a.last_error.is_some());
        assert!(!dir.exists(), "nothing saved");
    }

    #[test]
    fn report_prefixes_id_zero() {
        let r = report(&[0x41, 0x80]);
        assert_eq!(r[..3], [0x00, 0x41, 0x80]);
        assert!(r[3..].iter().all(|&b| b == 0));
        assert_eq!(Family::Mm712.init(), vec![report(&[0x44, 0x81, 0x02])]);
        assert_eq!(Family::Mm712.iface(), 3);
        assert_eq!(Family::KeyboardV2.iface(), 1);
    }
}
