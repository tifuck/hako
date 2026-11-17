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
//! - Cooler Master MasterPlus+ 1.9.5 (`CMUOT.dll` with its PDB, static
//!   analysis; findings in `docs/vendor-re/coolermaster.md`): per-command echo
//!   ack (2 s), command order, speed codes, save sequence, device list.
//!
//! `mouse.rs` and `keyboard.rs` hold the pure packet builders; this file holds
//! the model table and the one adapter they share. Lighting writes are
//! volatile and each report is acked by its echo (see `Family::acked`); the
//! `41 01` / `50 55` / `41 00` save (MM712 family `54 81 01`) is only sent
//! by the opt-in `save_to_device` toggle, for the models `Family::save_blocked`
//! lets through. None of the modes used here needs re-streaming, so `tick`
//! does nothing.
//!
//! OpenRGB's per-PID `CMKeyboardV2Controller::Initialize` (`51 00 00 00 <p>`
//! profile select, `56 81 .. <p> .. bb` "magic" and the `12` / `42` / `43`
//! startup packets) is only called from `SetLeds` before Direct per-key
//! frames, never from `SetMode`, whose packets `keyboard.rs` mirrors; it is
//! not sent here (MasterPlus+ has no `12` / `42` / `43` startup packets in its
//! keyboard classes either). Its `51 00` would also switch the live onboard
//! profile.
//!
//! Not listed: MK850 (0069; MasterPlus+ treats it as a bigger-profile class),
//! MM830 / MM831 (own OLED / music-pulse packing), ControlPad and the
//! mousepads / controllers / cooling devices of the same vendor id; MK750
//! (0067), SK630 (0089) and SK650 (008d) stay unlisted: the sources disagree
//! on their family. MM710 / MM311 have no LEDs and are listed for remaps only.

mod keyboard;
mod mouse;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{anyhow, bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const VID: u16 = 0x2516;

/// Both projects read one reply per command; OpenRGB waits up to 250 ms.
const REPLY_WAIT: Duration = Duration::from_millis(250);

/// MasterPlus+ waits up to 2 s for the echo of each command.
const ACK_WAIT: Duration = Duration::from_millis(2000);

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
    model(Mouse, "Cooler Master MM310", &[0x015f]),
    model(Mouse, "Cooler Master MM310 SF6 Chun-Li", &[0x01ec]),
    model(Mouse, "Cooler Master MM712 wired", &[0x01df]),
    model(Mouse, "Cooler Master MM731", &[0x0167]),
    model(Mouse, "Cooler Master MM712 LINE FRIENDS", &[0x01f0]),
    model(Mouse, "Cooler Master MM710", &[0x012f]),
    model(Mouse, "Cooler Master MM311", &[0x0161]),
    model(Keyboard, "Cooler Master CK550 V2", &[0x0145]),
    model(Keyboard, "Cooler Master CK530 V2", &[0x0147]),
    model(Keyboard, "Cooler Master SK622 Black", &[0x0149]),
    model(Keyboard, "Cooler Master SK622 White", &[0x014b]),
    model(Keyboard, "Cooler Master CK550 V1 / CK552", &[0x007f]),
    model(Keyboard, "Cooler Master CK530", &[0x009f]),
    model(Keyboard, "Cooler Master MK730", &[0x008f]),
    model(Keyboard, "Cooler Master SK652", &[0x015d]),
    model(Keyboard, "Cooler Master SK653", &[0x01ab, 0x015b]),
    model(Keyboard, "Cooler Master SK620 Space Grey", &[0x0157]),
    model(Keyboard, "Cooler Master SK620 Silver", &[0x0159]),
    model(Keyboard, "Cooler Master SK631 Black", &[0x008b]),
    model(Keyboard, "Cooler Master SK631 White", &[0x0125]),
    model(Keyboard, "Cooler Master SK651 Black", &[0x0091]),
    model(Keyboard, "Cooler Master SK651 White", &[0x0127]),
    model(Keyboard, "Cooler Master CK570 SF6 Chun-Li", &[0x01e8]),
    model(Keyboard, "Cooler Master CK720", &[0x016b]),
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
    /// Mice without LEDs (MM710, MM311): listed for host-side remaps only.
    NoLight,
}

fn family(pid: u16) -> Option<Family> {
    Some(match pid {
        0x0065 => Family::MasterMouse(0x2c),
        0x0141 | 0x0165 | 0x015f | 0x01ec | 0x01df => Family::MasterMouse(0x2b),
        0x0101 => Family::Mm711,
        0x0169 | 0x0167 | 0x01f0 => Family::Mm712,
        0x012f | 0x0161 => Family::NoLight,
        0x003b | 0x0047 | 0x003c => Family::KeyboardV1,
        0x0145 | 0x0147 | 0x0149 | 0x014b | 0x007f | 0x009f | 0x008f | 0x015d | 0x01ab | 0x015b
        | 0x0157 | 0x0159 | 0x008b | 0x0125 | 0x0091 | 0x0127 | 0x01e8 | 0x016b => {
            Family::KeyboardV2
        }
        _ => return None,
    })
}

impl Family {
    /// USB interface of the vendor collection (usage page 0xFF00, MM712 family
    /// 0xFF0A). The wired MM712 is listed with interface 0 by Cooler Master's
    /// own device table.
    fn iface(self, pid: u16) -> u32 {
        match (self, pid) {
            (Family::Mm712, _) => 3,
            (_, 0x01df) => 0,
            _ => 1,
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

    /// The first two bytes a reply must start with: MasterPlus+ waits for the
    /// echo of the command and sub-command, except on the MM712 family whose
    /// reply opcode is the request's plus one (`54 81` -> `55 81`).
    fn ack_filter(self, r: &Report) -> [u8; 2] {
        let op = if self == Family::Mm712 {
            r[1].wrapping_add(1)
        } else {
            r[1]
        };
        [op, r[2]]
    }

    /// The save sequence, each report acked like any other: `41 01`, `50 55`,
    /// `41 00` (config mode, save the active profile, leave); MM712 family
    /// `54 81 01`.
    fn save_packets(self) -> Vec<Report> {
        match self {
            Family::Mm712 => vec![report(&[0x54, 0x81, 0x01])],
            Family::NoLight => vec![],
            _ => vec![
                report(&[0x41, 0x01]),
                report(&[0x50, 0x55]),
                report(&[0x41, 0x00]),
            ],
        }
    }

    fn packets(self, look: &Look) -> Vec<Report> {
        match self {
            Family::MasterMouse(cmd) => mouse::master_packets(cmd, look),
            Family::Mm711 => mouse::mm711_packets(look),
            Family::Mm712 => mouse::mm712_packets(look),
            Family::KeyboardV1 => keyboard::v1_packets(look),
            Family::KeyboardV2 => keyboard::v2_packets(look),
            Family::NoLight => vec![],
        }
    }

    /// Why saving is not offered for `pid`; `None` where MasterPlus+'s code for
    /// that exact model (`saveConfig`) and a second source (OpenRGB, whose
    /// `50 55` / `54 81 01` it matches) agree on the sequence, or, for the
    /// keyboards, MasterPlus+'s own class for that exact model.
    fn save_blocked(self, pid: u16) -> Option<&'static str> {
        match (self, pid) {
            (Family::Mm711, _)
            | (Family::MasterMouse(0x2b), 0x0141 | 0x0165)
            | (Family::Mm712, 0x0169)
            | (Family::KeyboardV2, 0x0149 | 0x014b | 0x01ab | 0x015b) => None,
            (Family::MasterMouse(0x2c), _) => Some(
                "MM530 is not covered by Cooler Master's current software; only OpenRGB \
                 documents its 50 55 save",
            ),
            (Family::KeyboardV1, _) => Some(
                "MasterKeys Pro keyboards are not covered by Cooler Master's current software; \
                 which onboard profile 50 55 overwrites is undocumented for them",
            ),
            _ => {
                Some("only Cooler Master's own software documents the save sequence for this model")
            }
        }
    }

    /// Models MasterPlus+ drives answer every command; the rest were only
    /// seen through OpenRGB / libcmmk, which read a reply without checking it.
    fn acked(self, pid: u16) -> bool {
        !matches!(
            (self, pid),
            (Family::MasterMouse(0x2c), _)
                | (Family::KeyboardV1, _)
                | (_, 0x007f | 0x009f | 0x008f)
        )
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
fn controls(family: Family, pid: u16, look: &Look) -> Vec<SettingMeta> {
    use SettingKey as K;
    if family == Family::NoLight {
        return vec![];
    }
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
    let mut save =
        SettingMeta::toggle(K::SaveToDevice, "Save changes to device memory").persistent();
    if let Some(why) = family.save_blocked(pid) {
        save = save.blocked(why);
    }
    c.push(save);
    c
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let model = MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .with_context(|| format!("no Cooler Master model for {pid:04x}"))?;
    let family = family(pid).with_context(|| format!("no lighting family for {pid:04x}"))?;
    let info = find_hidraw(VID, pid, Some(family.iface(pid)))?;
    let dev = HidrawDevice::open(&info.path)?;
    let mut settings =
        hako_core::config::load_device_settings(&ctx.paths, VID, model.product_ids[0])?;
    // Only a look the user chose is restored; a fresh device keeps its own.
    let restore = get_setting(&settings, SettingKey::Effect).is_some();
    let known = controls(family, pid, &Look::new(family, &settings));
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
        dev: Some(Box::new(dev)),
        settings,
        last_error: None,
        paths: ctx.paths.clone(),
        acked: family.acked(pid),
    };
    let init = family.init();
    let result = a
        .send(&init)
        .and_then(|()| if restore { a.apply(false) } else { Ok(()) });
    if let Err(e) = result {
        warn!("{}: restore lighting: {e}", model.name);
        a.last_error = Some(e.to_string());
    }
    info!("{} open via {}", model.name, info.path.display());
    Ok(vec![Box::new(a)])
}

/// Report transport, so the exchanges can be tested without hardware.
trait Link: Send {
    fn write(&mut self, report: &[u8]) -> Result<()>;
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
}

impl Link for HidrawDevice {
    fn write(&mut self, report: &[u8]) -> Result<()> {
        HidrawDevice::write(self, report)
    }

    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        HidrawDevice::read_timeout(self, buf, timeout)
    }
}

struct Lighting {
    name: &'static str,
    kind: DeviceKind,
    family: Family,
    /// Names the settings file.
    canonical: u16,
    id: DeviceId,
    dev: Option<Box<dyn Link>>,
    settings: SettingsMap,
    last_error: Option<String>,
    paths: ConfigPaths,
    /// Every report must be answered with its echo ([`Family::acked`]).
    acked: bool,
}

/// A reply (report id 0 may or may not be included) starting with `want`.
fn is_ack(reply: &[u8], want: [u8; 2]) -> bool {
    reply.starts_with(&want) || (reply.first() == Some(&0) && reply[1..].starts_with(&want))
}

impl Lighting {
    /// Write each report and wait for its reply. A model MasterPlus+ drives
    /// must echo the command within [`ACK_WAIT`] (other reports, such as
    /// button events, are skipped); for the rest the reply is not checked and
    /// a device that does not answer is not an error, a failed read is.
    fn send(&mut self, reports: &[Report]) -> Result<()> {
        let (family, acked) = (self.family, self.acked);
        let dev = self.dev.as_mut().context("not connected")?;
        let mut buf = [0u8; 65];
        for r in reports {
            dev.write(r)?;
            if !acked {
                if let Err(e) = dev.read_timeout(&mut buf, REPLY_WAIT) {
                    // A timeout is a plain error, a failed read an I/O one.
                    if e.downcast_ref::<std::io::Error>().is_some() {
                        return Err(e);
                    }
                }
                continue;
            }
            let want = family.ack_filter(r);
            let end = Instant::now() + ACK_WAIT;
            loop {
                let left = end.saturating_duration_since(Instant::now());
                match dev.read_timeout(&mut buf, left) {
                    Ok(n) if is_ack(&buf[..n], want) => break,
                    Ok(_) if !left.is_zero() => {}
                    Err(e) if e.downcast_ref::<std::io::Error>().is_some() => return Err(e),
                    _ => bail!(
                        "no reply to {:02x} {:02x} within {} ms",
                        r[1],
                        r[2],
                        ACK_WAIT.as_millis()
                    ),
                }
            }
        }
        Ok(())
    }

    /// Send the look; with `save`, followed by the family's save sequence.
    fn apply(&mut self, save: bool) -> Result<()> {
        let packets = self.family.packets(&Look::new(self.family, &self.settings));
        self.send(&packets)?;
        if save {
            self.save()?;
        }
        Ok(())
    }

    /// The family's save sequence. Config mode (`41 01`) is left again
    /// (`41 00`) whatever failed after entering it.
    fn save(&mut self) -> Result<()> {
        let packets = self.family.save_packets();
        for (i, p) in packets.iter().enumerate() {
            let Err(e) = self.send(std::slice::from_ref(p)) else {
                continue;
            };
            if packets.len() == 3 && i < 2 {
                if let Err(l) = self.send(&packets[2..]) {
                    warn!("{}: leave config mode: {l:#}", self.name);
                }
            }
            return Err(e);
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, VID, self.canonical, &self.settings)
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
            controls: controls(
                self.family,
                self.id.product_id,
                &Look::new(self.family, &self.settings),
            ),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let pid = self.id.product_id;
        let save_on = get_setting(&self.settings, SettingKey::SaveToDevice)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if key == SettingKey::SaveToDevice {
            if value.as_bool() == Some(true) {
                if let Some(why) = self.family.save_blocked(pid) {
                    bail!("{}: {why}", self.name);
                }
                // Turning it on stores the current look once; if that fails
                // the mode stays off.
                if let Err(e) = self.apply(true) {
                    self.last_error = Some(format!("{e:#}"));
                    return Err(e);
                }
            }
            merge_setting(&mut self.settings, key, value);
            return self.persist().context("applied, but not saved");
        }
        let save = save_on && self.family.save_blocked(pid).is_none();
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        if let Err(e) = self.apply(save) {
            // Some of the reports (the save too) may have gone out: send the
            // old look again the same way.
            self.settings = before;
            let e = match self.apply(save) {
                Ok(()) => e,
                Err(r) => anyhow!("{e:#}; restoring the previous look failed too: {r:#}"),
            };
            self.last_error = Some(format!("{e:#}"));
            return Err(e);
        }
        self.last_error = None;
        if let Err(e) = self.persist() {
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

    /// A model of each family that MasterPlus+ also drives.
    fn rep_pid(f: Family) -> u16 {
        match f {
            Family::MasterMouse(0x2c) => 0x0065,
            Family::MasterMouse(_) => 0x0141,
            Family::Mm711 => 0x0101,
            Family::Mm712 => 0x0169,
            Family::KeyboardV1 => 0x003b,
            Family::KeyboardV2 => 0x0149,
            Family::NoLight => 0x012f,
        }
    }

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
                for c in controls(f, rep_pid(f), &Look::new(f, &s)) {
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
        let c = controls(
            Family::KeyboardV2,
            0x0149,
            &Look::new(Family::KeyboardV2, &s),
        );
        let hidden = |k| c.iter().find(|m| m.key == k).unwrap().hidden;
        assert!(!hidden(SettingKey::Direction) && hidden(SettingKey::ColorR));
        let c = controls(
            Family::KeyboardV1,
            0x003b,
            &Look::new(Family::KeyboardV1, &s),
        );
        assert!(
            !c.iter()
                .find(|m| m.key == SettingKey::ColorR)
                .unwrap()
                .hidden
        );
        let c = controls(Family::Mm712, 0x0169, &Look::new(Family::Mm712, &s));
        assert!(c.iter().all(|m| m.key != SettingKey::Direction));
    }

    #[test]
    fn save_is_offered_only_where_the_vendor_code_covers_the_model() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        let open = |pid| {
            let f = family(pid).unwrap();
            let c = controls(f, pid, &Look::new(f, &s));
            let save = c.into_iter().find(|m| m.key == SettingKey::SaveToDevice);
            save.map(|m| {
                assert!(m.persistent);
                m.blocked.is_none()
            })
        };
        for pid in [
            0x0101, 0x0141, 0x0165, 0x0169, 0x0149, 0x014b, 0x01ab, 0x015b,
        ] {
            assert_eq!(open(pid), Some(true), "{pid:04x}");
        }
        for pid in [
            0x0065, 0x015f, 0x01ec, 0x01df, 0x0167, 0x01f0, 0x0145, 0x0157, 0x01e8,
        ] {
            assert_eq!(open(pid), Some(false), "{pid:04x}");
        }
        for pid in [0x003b, 0x0047, 0x003c, 0x007f] {
            assert_eq!(open(pid), Some(false), "{pid:04x}");
        }
        assert_eq!(open(0x012f), None, "no LEDs, no controls");
        assert_eq!(open(0x0161), None);
    }

    #[test]
    fn lighting_packets_never_save_and_save_sequences_match_the_vendor() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        for pid in MODELS.iter().flat_map(|m| m.product_ids.iter().copied()) {
            let f = family(pid).unwrap();
            let mut packets = f.init();
            for &e in f.effects() {
                let mut l = Look::new(f, &s);
                l.effect = e;
                packets.extend(f.packets(&l));
            }
            for p in packets {
                assert_ne!(p[1..3], [0x50, 0x55], "{pid:04x} sends a save");
                assert_ne!(p[1..4], [0x54, 0x81, 0x01], "{pid:04x} sends a save");
            }
        }
        let bytes = |f: Family| -> Vec<Vec<u8>> {
            f.save_packets().iter().map(|r| r[..4].to_vec()).collect()
        };
        assert_eq!(
            bytes(Family::Mm711),
            [[0, 0x41, 0x01, 0], [0, 0x50, 0x55, 0], [0, 0x41, 0x00, 0]]
        );
        assert_eq!(bytes(Family::KeyboardV2), bytes(Family::MasterMouse(0x2b)));
        // MM712: one report, with the report id in front (OpenRGB's code omits it).
        assert_eq!(bytes(Family::Mm712), [[0, 0x54, 0x81, 0x01]]);
        assert!(Family::NoLight.save_packets().is_empty());
    }

    #[test]
    fn acks_follow_the_vendor_filters() {
        let r = report(&[0x51, 0x2b, 0x00]);
        assert_eq!(Family::Mm711.ack_filter(&r), [0x51, 0x2b]);
        assert_eq!(
            Family::KeyboardV2.ack_filter(&report(&[0x41, 0x80])),
            [0x41, 0x80]
        );
        // Wireless family: reply opcode is the request's plus one.
        assert_eq!(
            Family::Mm712.ack_filter(&report(&[0x54, 0x81, 0x01])),
            [0x55, 0x81]
        );
        assert_eq!(
            Family::Mm712.ack_filter(&report(&[0x4c, 0x81, 0x04])),
            [0x4d, 0x81]
        );
        assert!(is_ack(&[0x41, 0x80, 0, 0], [0x41, 0x80]));
        assert!(is_ack(&[0, 0x41, 0x80], [0x41, 0x80]), "report id echoed");
        assert!(!is_ack(&[0x41, 0x00], [0x41, 0x80]));
        assert!(!is_ack(&[0x41], [0x41, 0x80]));
        // Models only seen through OpenRGB / libcmmk are not required to answer.
        for pid in [0x0065, 0x003b, 0x007f, 0x009f, 0x008f] {
            assert!(!family(pid).unwrap().acked(pid), "{pid:04x}");
        }
        for pid in [0x0101, 0x0141, 0x0169, 0x0149, 0x01ab, 0x0157] {
            assert!(family(pid).unwrap().acked(pid), "{pid:04x}");
        }
    }

    #[test]
    fn interfaces() {
        assert_eq!(family(0x0169).unwrap().iface(0x0169), 3);
        assert_eq!(family(0x0167).unwrap().iface(0x0167), 3);
        assert_eq!(family(0x01df).unwrap().iface(0x01df), 0);
        assert_eq!(family(0x015f).unwrap().iface(0x015f), 1);
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
            acked: true,
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

    /// Echoes every report except `[1..3] == drop`; `sent` records them.
    struct Fake {
        drop: [u8; 2],
        sent: std::sync::Arc<std::sync::Mutex<Vec<[u8; 2]>>>,
        replies: std::collections::VecDeque<Report>,
    }

    impl Link for Fake {
        fn write(&mut self, report: &[u8]) -> Result<()> {
            let r: Report = report.try_into()?;
            self.sent.lock().unwrap().push([r[1], r[2]]);
            if r[1..3] != self.drop {
                self.replies.push_back(r);
            }
            Ok(())
        }
        fn read_timeout(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
            let r = self.replies.pop_front().context("timeout")?;
            buf[..64].copy_from_slice(&r[1..]);
            Ok(64)
        }
    }

    #[test]
    fn a_failed_save_leaves_config_mode_and_restores_by_saving() {
        let dir = std::env::temp_dir().join(format!("hako-cm-save-{}", std::process::id()));
        let mut settings = SettingsMap::new();
        defaults(&mut settings);
        merge_setting(
            &mut settings,
            SettingKey::SaveToDevice,
            SettingValue::Bool(true),
        );
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
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
            dev: Some(Box::new(Fake {
                drop: [0x50, 0x55],
                sent: sent.clone(),
                replies: Default::default(),
            })),
            settings: settings.clone(),
            last_error: None,
            acked: true,
            paths: ConfigPaths {
                root: dir.clone(),
                config_toml: dir.join("config.toml"),
                devices_dir: dir.join("devices"),
                profiles_dir: dir.join("profiles"),
                remaps_dir: dir.join("remaps"),
            },
        };
        assert!(a
            .set(SettingKey::Brightness, SettingValue::Int(10))
            .is_err());
        assert_eq!(a.settings, settings);
        let sent = sent.lock().unwrap();
        let saves: Vec<usize> = (0..sent.len())
            .filter(|&i| sent[i] == [0x50, 0x55])
            .collect();
        // The change, then the restore: each save is followed by `41 00`.
        assert_eq!(saves.len(), 2, "{sent:02x?}");
        for i in saves {
            assert_eq!(sent.get(i + 1), Some(&[0x41, 0x00]), "{sent:02x?}");
        }
        assert!(!dir.exists(), "nothing saved");
    }

    #[test]
    fn report_prefixes_id_zero() {
        let r = report(&[0x41, 0x80]);
        assert_eq!(r[..3], [0x00, 0x41, 0x80]);
        assert!(r[3..].iter().all(|&b| b == 0));
        assert_eq!(Family::Mm712.init(), vec![report(&[0x44, 0x81, 0x02])]);
        assert_eq!(Family::Mm712.iface(0x0169), 3);
        assert_eq!(Family::KeyboardV2.iface(0x0149), 1);
    }
}
