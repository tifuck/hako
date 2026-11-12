//! HyperX mice.
//!
//! - Universal: NGENUITY's "universal" protocol (ngenuity-linux-cli
//!   `docs/protocol/universal.md`): 64-byte `<cmd> <sub> <payload>` reports on
//!   the vendor interface, replies `cmd+1`, `ff` acks, `fb` events. Every write
//!   targets profile slot 0 (live); a mouse power cycle reloads slot 1 (onboard)
//!   into it. Saga Pro IDs, interfaces, the live `32 01 00 ..` table, `40 01`
//!   and `50 02` from HyperX Saga Control `docs/protocol-notes.md`.
//!   Save to device memory (owner opt-in) does what NGENUITY's Save does for
//!   the DPI table and brightness: the live slot-0 value is copied to slot 1
//!   (`32 01 01 00 ..`, `40 01 01 00 ..`). ngenuity-linux-cli (`cli.py`
//!   `target_slots`, tested on the Haste 2 Wireless) and Saga Control
//!   (`protocol.py` `build_dpi_packet` / `save_rgb_profile`, Saga Pro) agree on
//!   both; both slots are read and checked first and slot 1 is read back after.
//!   Lift-off and lighting presets are not saved: the sources disagree on the
//!   lift-off slot and the preset save is untested (docs/onboard-writes/hyperx.md).
//!   DPI stages 1-5 and the active stage are fields of the same live table
//!   (`[5]` enabled-stage mask, `[6]` active stage, 0-based; universal.md
//!   §3.3); changing them only writes slot 0.
//! - Dart: OpenRGB `HyperXPulsefireDartController` layout, `d2 <led> <mode> 08
//!   RGB RGB <brightness> <speed>`; `d0` polling rate and the `51` heartbeat
//!   (battery) from the santeri3700 notes and PlasmaNGenuity; DPI stages
//!   `d3 02 <stage> 02 <DPI/50 u16 LE>` and active stage `d3 00 00 01 <stage>`
//!   from the same notes and their `ngenuity_initialization_2` capture. Volatile until a
//!   `de ff` save, which only one line of reverse engineering documents: shown
//!   as a blocked placeholder, never sent.
//!
//! Surge, FPS Pro, Core, Raid and the original Haste stream their lighting:
//! see `pulsefire.rs`.

use super::{
    exchange, int, lighting_controls, open_node, pad, read_reports, rgb, text, Node, Spec,
};
use super::{HP, KINGSTON};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    Universal {
        /// Per PID: vendor interface, and whether it is the cable (always linked).
        links: &'static [(u16, u32, bool)],
        /// How many [`RATE_CODES`] the mouse offers over its dongle.
        rates: usize,
        /// The `42 02` firmware presets (only tested on the Haste 2 Wireless).
        presets: bool,
    },
    Dart,
}

/// Dongle 03f0:0f98, vendor interface 2 (usage page ff00).
pub(super) const HASTE_2_WIRELESS: Spec<Family> = Spec {
    name: "HyperX Pulsefire Haste 2 Wireless",
    vid: HP,
    pids: &[0x0f98],
    family: Family::Universal {
        links: &[(0x0f98, 2, false)],
        rates: 4,
        presets: true,
    },
};
/// Dongle 06bf (config node on interface 3), cable 04bf (interface 2).
/// 2000 / 4000 Hz only over the dongle.
pub(super) const SAGA_PRO: Spec<Family> = Spec {
    name: "HyperX Pulsefire Saga Pro",
    vid: HP,
    pids: &[0x06bf, 0x04bf],
    family: Family::Universal {
        links: &[(0x06bf, 3, false), (0x04bf, 2, true)],
        rates: 6,
        presets: false,
    },
};
/// Wireless dongle first (interface 2), cable second (interface 1).
pub(super) const DART: Spec<Family> = Spec {
    name: "HyperX Pulsefire Dart",
    vid: KINGSTON,
    pids: &[0x16e1, 0x16e2],
    family: Family::Dart,
};
pub(super) const DART_HP: Spec<Family> = Spec {
    name: "HyperX Pulsefire Dart",
    vid: HP,
    pids: &[0x068e, 0x088e],
    family: Family::Dart,
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[&HASTE_2_WIRELESS, &SAGA_PRO, &DART, &DART_HP];

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, ctx: &DriverCtx) -> Opened {
    let paths = ctx.paths.clone();
    Ok(vec![match spec.family {
        Family::Universal { .. } => Box::new(UniversalMouse::open(spec, pid, paths)?),
        Family::Dart => Box::new(DartMouse::open(spec, pid, paths)?) as Box<dyn DeviceAdapter>,
    }])
}

// ── Universal protocol ──────────────────────────────────────────────────────

/// Profile slot 0 = live. Slot 1 is the onboard profile, written only by the
/// owner's save-to-device mode.
const LIVE: [u8; 2] = [0x00, 0x00];
const SAVED: [u8; 2] = [0x01, 0x00];

/// DPI codes of 200..=26000 DPI (DPI / 50 - 1).
const DPI_CODES: std::ops::RangeInclusive<u16> = 3..=519;

const STAGES: [SettingKey; 5] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];

/// Every setting held in the `32 01` DPI table.
const TABLE_KEYS: [SettingKey; 8] = [
    SettingKey::Dpi,
    SettingKey::ReportRate,
    SettingKey::DpiStageActive,
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];

/// UniversalPollingRate codes.
const RATE_CODES: [(i64, u8); 7] = [
    (125, 0x40),
    (250, 0x20),
    (500, 0x10),
    (1000, 0x08),
    (2000, 0x04),
    (4000, 0x02),
    (8000, 0x01),
];

fn uni(cmd: u8, sub: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = pad(&[cmd, sub], 64);
    p[2..2 + payload.len()].copy_from_slice(payload);
    p
}

fn is_reply(r: &[u8], cmd: u8, sub: u8) -> bool {
    matches!(r, [c, s, ..] if *c == cmd + 1 && *s == sub)
}

/// `33 02`: \[2\] rate, \[3\] stage mask, \[4\] active stage, [5 + 5i] stage i =
/// u16 LE (DPI / 50 - 1) + RGB. Returns (active DPI, rate in Hz).
fn parse_dpi(r: &[u8]) -> Option<(i64, Option<i64>)> {
    let [0x33, 0x02, rate, _, active, ..] = r else {
        return None;
    };
    let at = 5 + 5 * usize::from(*active);
    let code = u16::from_le_bytes(r.get(at..at + 2)?.try_into().ok()?);
    let hz = RATE_CODES
        .iter()
        .find(|(_, c)| c == rate)
        .map(|(hz, _)| *hz);
    Some(((i64::from(code) + 1) * 50, hz))
}

/// `33 02`: enabled-stage mask, active stage (0-based) and the DPI of
/// stages 1-5 (`None` when disabled).
fn parse_stages(r: &[u8]) -> Option<(u8, u8, [Option<i64>; 5])> {
    let [0x33, 0x02, _, mask, active, ..] = r else {
        return None;
    };
    let mut stages = [None; 5];
    for i in (0..STAGES.len()).filter(|&i| mask & (1 << i) != 0) {
        let at = 5 + 5 * i;
        let code = u16::from_le_bytes(r.get(at..at + 2)?.try_into().ok()?);
        stages[i] = Some((i64::from(code) + 1) * 50);
    }
    Some((*mask, *active, stages))
}

/// What a table write changes; `None` keeps the mouse's value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct TableEdit {
    /// DPI of stages 1-5.
    stages: [Option<i64>; 5],
    /// 0-based; must be enabled in the mask.
    active: Option<u8>,
    /// DPI of the (new) active stage, applied after `stages`.
    dpi: Option<i64>,
    hz: Option<i64>,
}

/// Live `32 01` table rebuilt from a `33 02` reply: mask and colors kept,
/// the edited fields replaced.
fn dpi_write(r: &[u8], edit: &TableEdit) -> Option<Vec<u8>> {
    table_write(r, LIVE, edit)
}

/// [`dpi_write`] into profile `slot`.
fn table_write(r: &[u8], slot: [u8; 2], edit: &TableEdit) -> Option<Vec<u8>> {
    let [0x33, 0x02, rate, mask, active, ..] = r else {
        return None;
    };
    if edit.active.is_some_and(|a| a >= 8 || mask & (1 << a) == 0) {
        return None;
    }
    let active = edit.active.unwrap_or(*active);
    let mut p = uni(0x32, 0x01, &slot);
    p[4] = edit
        .hz
        .and_then(|hz| RATE_CODES.iter().find(|(h, _)| *h == hz))
        .map_or(*rate, |(_, c)| *c);
    p[5] = *mask;
    p[6] = active;
    // Eleven 5-byte stage slots.
    p[7..62].copy_from_slice(r.get(5..60)?);
    let stages = (0..).zip(edit.stages).filter_map(|(i, d)| Some((i, d?)));
    for (i, dpi) in stages.chain(edit.dpi.map(|d| (usize::from(active), d))) {
        let at = 7 + 5 * i;
        let code = u16::try_from(dpi / 50 - 1).ok()?;
        p.get_mut(at..at + 2)?.copy_from_slice(&code.to_le_bytes());
    }
    Some(p)
}

/// Keep `dpi` equal to the active stage's DPI after `key` changed.
fn link_dpi(s: &mut SettingsMap, key: SettingKey) {
    use SettingKey::*;
    let Some(&stage) = get_setting(s, DpiStageActive)
        .and_then(|v| v.as_int())
        .and_then(|n| STAGES.get(usize::try_from(n - 1).ok()?))
    else {
        return;
    };
    let (from, to) = match key {
        Dpi => (Dpi, stage),
        k if k == stage || k == DpiStageActive => (stage, Dpi),
        _ => return,
    };
    if let Some(v) = get_setting(s, from).cloned() {
        merge_setting(s, to, v);
    }
}

/// A `33 02` reply that is a plausible DPI table: known rate code, the active
/// stage enabled, every enabled stage's DPI in range.
fn check_table(r: &[u8]) -> Result<()> {
    let &[0x33, 0x02, rate, mask, active, ..] = r else {
        bail!("not a DPI table reply");
    };
    if !RATE_CODES.iter().any(|&(_, c)| c == rate) {
        bail!("unknown report-rate code {rate:02x}");
    }
    let mask = u16::from(mask);
    if active >= 8 || mask & (1 << active) == 0 {
        bail!("active stage {active} is not enabled (mask {mask:02x})");
    }
    for i in (0..8).filter(|i| mask & (1 << i) != 0) {
        let at = 5 + 5 * i;
        let code = u16::from_le_bytes(r.get(at..at + 2).context("short DPI table")?.try_into()?);
        if !DPI_CODES.contains(&code) {
            bail!("stage {} holds DPI code {code}", i + 1);
        }
    }
    Ok(())
}

/// Send a request, return its `cmd+1, sub` reply (if one arrived).
type Query<'a> = dyn FnMut(&[u8]) -> Result<Option<Vec<u8>>> + 'a;

/// NGENUITY's Save for the DPI table and report rate: the live table copied
/// to slot 1. Both tables are read and checked first; slot 1 is read back.
fn store_table(q: &mut Query) -> Result<()> {
    let live = q(&uni(0x32, 0x02, &[0]))?.context("no live DPI table")?;
    check_table(&live).context("live DPI table")?;
    let saved = q(&uni(0x32, 0x02, &[1]))?.context("no onboard DPI table")?;
    check_table(&saved).context("onboard DPI table")?;
    let w = table_write(&live, SAVED, &TableEdit::default()).context("bad DPI table")?;
    q(&w)?;
    let back = q(&uni(0x32, 0x02, &[1]))?.context("onboard DPI table not read back")?;
    if back.get(2..60) != w.get(4..62) {
        bail!("the onboard DPI table reads back different from what was written");
    }
    Ok(())
}

/// NGENUITY's Save for brightness: the live value copied to slot 1, read back.
fn store_brightness(q: &mut Query) -> Result<()> {
    fn read(q: &mut Query, slot: u8) -> Result<u8> {
        match q(&uni(0x40, 0x02, &[slot]))?.as_deref() {
            Some([0x41, 0x02, b, ..]) => Ok(*b),
            _ => bail!("no brightness reply for profile slot {slot}"),
        }
    }
    let b = read(q, 0)?;
    read(q, 1)?;
    q(&brightness_raw(SAVED, b))?;
    let back = read(q, 1)?;
    if back != b {
        bail!("onboard brightness reads back {back:02x}, wrote {b:02x}");
    }
    Ok(())
}

/// `37 02`: \[2\] angle snap, \[3\] angle tune, \[4\] tune enable, \[5\] lift-off
/// (00 = 1 mm, 02 = 2 mm), \[6\] motion sync.
fn parse_lift(r: &[u8]) -> Option<i64> {
    match r {
        [0x37, 0x02, _, _, _, lift, ..] => Some(if *lift == 0x02 { 2 } else { 1 }),
        _ => None,
    }
}

fn lift_write(r: &[u8], mm: i64) -> Option<Vec<u8>> {
    let [0x37, 0x02, ..] = r else {
        return None;
    };
    let mut p = uni(0x36, 0x01, &LIVE);
    p[4..9].copy_from_slice(r.get(2..7)?);
    p[7] = if mm == 2 { 0x02 } else { 0x00 };
    Some(p)
}

/// NGENUITY sends floor(percent * 255 / 100).
fn brightness_write(pct: i64) -> Vec<u8> {
    brightness_raw(LIVE, (pct.clamp(0, 100) * 255 / 100) as u8)
}

/// `40 01 <slot u16> <brightness 0-255>`.
fn brightness_raw(slot: [u8; 2], b: u8) -> Vec<u8> {
    uni(0x40, 0x01, &[slot[0], slot[1], b])
}

fn parse_brightness(r: &[u8]) -> Option<i64> {
    match r {
        [0x41, 0x02, b, ..] => Some((i64::from(*b) * 100 + 127) / 255),
        _ => None,
    }
}

/// Firmware preset duration: one byte, lower is faster (50 = NGENUITY default).
fn duration(speed: &str) -> u8 {
    match speed {
        "slow" => 150,
        "fast" => 10,
        _ => 50,
    }
}

/// `42 02` firmware presets that work live on the Haste 2 Wireless: 0 off,
/// 1 solid, 2 breath, 4 color cycle; \[8\] = 01 one color / fe spectrum.
fn preset_write(effect: &str, [r, g, b]: [u8; 3], speed: &str) -> Vec<u8> {
    let d = duration(speed);
    let payload: &[u8] = match effect {
        "off" => &[0, 0, 0],
        "breathing" => &[0, 0, 2, d, 0, 0, 1, r, g, b],
        "spectrum" => &[0, 0, 4, d, 0, 0, 0xfe],
        _ => &[0, 0, 1, 0, 0, 0, 1, r, g, b],
    };
    uni(0x42, 0x02, payload)
}

fn parse_effect(r: &[u8]) -> Option<&'static str> {
    match r {
        [0x43, 0x03, mode, ..] => match mode {
            0 => Some("off"),
            1 => Some("static"),
            2 => Some("breathing"),
            4 => Some("spectrum"),
            // 6 = FrameLight (NGENUITY's stored software effect).
            _ => None,
        },
        _ => None,
    }
}

const UNI_EFFECTS: [(&str, &str); 4] = [
    ("off", "Off"),
    ("static", "Static"),
    ("breathing", "Breathing"),
    ("spectrum", "Spectrum"),
];

/// Stage sliders (stages outside `mask` hidden) and the active-stage choice
/// among the enabled ones; nothing for an empty mask.
fn stage_controls(mask: u8, max_dpi: f64) -> Vec<SettingMeta> {
    let on: Vec<i64> = (1..=STAGES.len() as i64)
        .filter(|n| mask & (1 << (n - 1)) != 0)
        .collect();
    if on.is_empty() {
        return Vec::new();
    }
    let mut c: Vec<_> = STAGES
        .iter()
        .zip(1..)
        .map(|(&key, n)| {
            SettingMeta::slider(key, &format!("DPI stage {n}"), 200.0, max_dpi, 50.0)
                .unit("dpi")
                .hidden(!on.contains(&n))
        })
        .collect();
    c.push(SettingMeta::dropdown(
        SettingKey::DpiStageActive,
        "Active DPI stage",
        on.iter()
            .map(|&n| SettingOption::int(n, &n.to_string()))
            .collect(),
    ));
    c
}

/// `rates`: how many [`RATE_CODES`] to offer; `presets`: lighting effects
/// besides brightness; `mask`: the enabled DPI stages last read.
fn universal_controls(effect: &str, rates: usize, presets: bool, mask: u8) -> Vec<SettingMeta> {
    use SettingKey::*;
    // Haste 2 Wireless and Saga Pro: 200-26000 DPI in steps of 50.
    let mut c = vec![SettingMeta::slider(Dpi, "DPI", 200.0, 26000.0, 50.0).unit("dpi")];
    c.extend(stage_controls(mask, 26000.0));
    c.extend([
        SettingMeta::dropdown(
            ReportRate,
            "Report rate",
            RATE_CODES[..rates]
                .iter()
                .map(|&(hz, _)| SettingOption::int(hz, &format!("{hz} Hz")))
                .collect(),
        ),
        SettingMeta::dropdown(
            LiftOffDistance,
            "Lift-off",
            vec![SettingOption::int(1, "1 mm"), SettingOption::int(2, "2 mm")],
        ),
    ]);
    if presets {
        c.extend(lighting_controls(
            &UNI_EFFECTS,
            effect,
            &["breathing", "spectrum"],
            &["static", "breathing"],
        ));
    } else {
        c.push(SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"));
    }
    c.push(
        SettingMeta::toggle(
            SaveToDevice,
            "Save DPI, report rate and brightness to device memory",
        )
        .persistent(),
    );
    c
}

/// Settings the save-to-device mode stores in slot 1.
fn saved_by(key: SettingKey) -> bool {
    matches!(
        key,
        SettingKey::Dpi | SettingKey::ReportRate | SettingKey::Brightness
    )
}

const REPLY_WAIT: Duration = Duration::from_millis(300);
const POLL: Duration = Duration::from_secs(30);

struct UniversalMouse {
    spec: &'static Spec<Family>,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    /// On the cable: always linked, no connection query.
    cabled: bool,
    rates: usize,
    presets: bool,
    /// What the user chose; re-applied whenever the mouse links up.
    user: SettingsMap,
    settings: SettingsMap,
    battery: BatteryState,
    online: bool,
    sync: bool,
    /// Enabled DPI stages from the last table read (0 = not read yet).
    mask: u8,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl UniversalMouse {
    fn open(spec: &'static Spec<Family>, pid: u16, paths: ConfigPaths) -> Result<Self> {
        let Family::Universal {
            links,
            rates,
            presets,
        } = spec.family
        else {
            bail!("{} is not a universal-protocol mouse", spec.name);
        };
        let &(_, iface, cabled) = links
            .iter()
            .find(|l| l.0 == pid)
            .with_context(|| format!("{} has no PID {pid:04x}", spec.name))?;
        let (dev, instance) = open_node(spec.vid, pid, Node::Iface(iface))?;
        info!("{} open via {}", spec.name, dev.path.display());
        let user = hako_core::config::load_device_settings(&paths, spec.vid, spec.pids[0])?;
        Ok(Self {
            spec,
            id: DeviceId {
                vendor_id: spec.vid,
                product_id: pid,
                instance,
            },
            dev: Some(dev),
            cabled,
            // A cable caps the rate at 1000 Hz (Saga Control).
            rates: if cabled { rates.min(4) } else { rates },
            presets,
            settings: user.clone(),
            user,
            battery: BatteryState {
                label: "Mouse".into(),
                percent: None,
                charging: false,
                present: false,
            },
            online: cabled,
            sync: true,
            mask: 0,
            last_poll: None,
            failures: 0,
            last_error: None,
            paths,
        })
    }

    /// Send `req` and return its `cmd+1, sub` reply if one arrives in time.
    fn query(&mut self, req: &[u8]) -> Result<Option<Vec<u8>>> {
        let (cmd, sub) = (req[0], req[1]);
        let dev = self.dev.as_mut().context("not connected")?;
        let reports = exchange(dev, req, REPLY_WAIT, |r| is_reply(r, cmd, sub))?;
        for r in &reports {
            self.event(r);
        }
        Ok(reports.into_iter().find(|r| is_reply(r, cmd, sub)))
    }

    fn event(&mut self, r: &[u8]) {
        match r {
            [0xfb, 0x0a, s, ..] | [0x53, 0x01, s, ..] => {
                let online = *s == 0x01;
                // The mouse sends 51 02 by itself after waking.
                if online && !self.online {
                    self.sync = true;
                }
                self.online = online;
                self.battery.present = online;
            }
            // DPI button: fb 08 <stage> <code u16 LE>.
            [0xfb, 0x08, stage, lo, hi, ..] => {
                let dpi = SettingValue::Int((i64::from(u16::from_le_bytes([*lo, *hi])) + 1) * 50);
                let s = &mut self.settings;
                merge_setting(s, SettingKey::Dpi, dpi.clone());
                if let Some(&key) = STAGES.get(usize::from(*stage)) {
                    merge_setting(s, key, dpi);
                    merge_setting(
                        s,
                        SettingKey::DpiStageActive,
                        SettingValue::Int(i64::from(*stage) + 1),
                    );
                }
            }
            // [3]: 0 not charging, 1 charging, 2 full, 3 error, ff unknown.
            [0x51, 0x02, level, status, ..] if *level <= 100 => {
                self.battery.present = true;
                self.battery.percent = Some(*level);
                self.battery.charging = *status == 0x01;
            }
            _ => {}
        }
    }

    fn apply_key(&mut self, key: SettingKey) -> Result<()> {
        use SettingKey::*;
        let s = &self.settings;
        let packet = match key {
            k if TABLE_KEYS.contains(&k) => {
                let get = |k| get_setting(s, k).and_then(|v| v.as_int());
                let edit = TableEdit {
                    stages: STAGES.map(get),
                    active: get(DpiStageActive).and_then(|n| u8::try_from(n - 1).ok()),
                    dpi: get(Dpi),
                    hz: get(ReportRate),
                };
                let reply = self
                    .query(&uni(0x32, 0x02, &[0]))?
                    .context("no DPI table from the mouse")?;
                dpi_write(&reply, &edit).context("bad DPI table")?
            }
            LiftOffDistance => {
                let mm = int(s, LiftOffDistance, 1);
                let reply = self
                    .query(&uni(0x36, 0x02, &[0]))?
                    .context("no sensor settings from the mouse")?;
                lift_write(&reply, mm).context("bad sensor settings")?
            }
            Brightness => brightness_write(int(s, Brightness, 100)),
            Effect | Speed | ColorR | ColorG | ColorB if self.presets => {
                preset_write(text(s, Effect, "static"), rgb(s), text(s, Speed, "medium"))
            }
            _ => bail!("unsupported setting {}", key.as_str()),
        };
        // Wait for the mouse's own reply (e.g. 43 02) so RF does not drop the next packet.
        self.query(&packet)?;
        Ok(())
    }

    fn saving(&self) -> bool {
        get_setting(&self.user, SettingKey::SaveToDevice).and_then(|v| v.as_bool()) == Some(true)
    }

    /// Copy the live value(s) behind `key` to the onboard slot (`None`: all).
    fn store(&mut self, key: Option<SettingKey>) -> Result<()> {
        use SettingKey::*;
        let mut q = |req: &[u8]| self.query(req);
        if matches!(key, None | Some(Dpi | ReportRate)) {
            store_table(&mut q)?;
        }
        if matches!(key, None | Some(Brightness)) {
            store_brightness(&mut q)?;
        }
        Ok(())
    }

    /// Re-apply the user's choices, then read the live state back.
    fn resync(&mut self) -> Result<()> {
        use SettingKey::*;
        let has = |k: SettingKey| get_setting(&self.user, k).is_some();
        let lighting = self.presets && [Effect, Speed, ColorR, ColorG, ColorB].into_iter().any(has);
        let pending: Vec<_> = [
            (Dpi, TABLE_KEYS.into_iter().any(has)),
            (LiftOffDistance, has(LiftOffDistance)),
            (Brightness, has(Brightness)),
            (Effect, lighting),
        ]
        .into_iter()
        .filter_map(|(k, want)| want.then_some(k))
        .collect();
        for k in pending {
            self.apply_key(k)?;
        }
        // Read requests take the profile slot as one byte.
        let reads: &[(u8, u8)] = if self.presets {
            &[(0x32, 0x02), (0x36, 0x02), (0x40, 0x02), (0x42, 0x03)]
        } else {
            &[(0x32, 0x02), (0x36, 0x02), (0x40, 0x02)]
        };
        for &(cmd, sub) in reads {
            let Some(r) = self.query(&uni(cmd, sub, &[0]))? else {
                continue;
            };
            let s = &mut self.settings;
            if let Some((dpi, hz)) = parse_dpi(&r) {
                merge_setting(s, Dpi, SettingValue::Int(dpi));
                if let Some(hz) = hz {
                    merge_setting(s, ReportRate, SettingValue::Int(hz));
                }
            }
            if let Some((mask, active, stages)) = parse_stages(&r) {
                self.mask = mask;
                for (key, dpi) in STAGES.into_iter().zip(stages) {
                    if let Some(dpi) = dpi {
                        merge_setting(s, key, SettingValue::Int(dpi));
                    }
                }
                if usize::from(active) < STAGES.len() {
                    merge_setting(s, DpiStageActive, SettingValue::Int(i64::from(active) + 1));
                }
            }
            if let Some(mm) = parse_lift(&r) {
                merge_setting(s, LiftOffDistance, SettingValue::Int(mm));
            }
            if let Some(b) = parse_brightness(&r) {
                merge_setting(s, Brightness, SettingValue::Int(b));
            }
            if let Some(e) = parse_effect(&r) {
                merge_setting(s, Effect, SettingValue::Text(e.into()));
            }
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<()> {
        let dev = self.dev.as_mut().context("not connected")?;
        // Link, DPI-button and battery events arrive unsolicited.
        for r in read_reports(dev, Duration::ZERO, |_| false) {
            self.event(&r);
        }
        if self.last_poll.is_none_or(|t| t.elapsed() >= POLL) {
            self.last_poll = Some(Instant::now());
            if !self.cabled {
                self.query(&uni(0x52, 0x01, &[]))?;
            }
            if self.online {
                self.query(&uni(0x50, 0x02, &[]))?;
            }
        }
        if self.online && self.sync {
            self.sync = false;
            self.resync()?;
        }
        Ok(())
    }
}

impl DeviceAdapter for UniversalMouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: universal_controls(
                text(&self.settings, SettingKey::Effect, ""),
                self.rates,
                self.presets,
                // All five until the table is read (e.g. a profile applied to a
                // sleeping mouse); `table_write` refuses a disabled active stage.
                if self.mask == 0 { 0x1f } else { self.mask },
            ),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        // Turning save mode on stores the live table and brightness once;
        // off writes nothing.
        if key == SettingKey::SaveToDevice && value.as_bool() == Some(true) {
            if !self.online {
                bail!("the mouse is asleep or off: wake it to save to its memory");
            }
            self.store(None)?;
        }
        for s in [&mut self.user, &mut self.settings] {
            // Chosen stages imply the DPI; a stale one would land in the active stage.
            if key == SettingKey::DpiStageActive || STAGES.contains(&key) {
                s.remove(SettingKey::Dpi.as_str());
            }
            merge_setting(s, key, value.clone());
            link_dpi(s, key);
        }
        hako_core::config::save_device_settings(
            &self.paths,
            self.spec.vid,
            self.spec.pids[0],
            &self.user,
        )?;
        if key == SettingKey::SaveToDevice {
            return Ok(());
        }
        let store = self.saving() && saved_by(key);
        if !self.online {
            // An asleep mouse gets everything on its next link-up, which never saves.
            if store {
                bail!("the mouse is asleep: changed when it wakes, but not saved to its memory");
            }
            return Ok(());
        }
        self.apply_key(key)?;
        if store {
            self.store(Some(key))
                .context("changed, but not saved to device memory")?;
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() {
            return Ok(());
        }
        match self.poll() {
            Ok(()) => {
                self.failures = 0;
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(e.to_string());
                if self.failures >= 3 {
                    self.dev = None;
                }
                Err(e)
            }
        }
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

// ── Pulsefire Dart ──────────────────────────────────────────────────────────

const DART_LED_LOGO: u8 = 0x00;
const DART_LED_SCROLL: u8 = 0x10;
const DART_LED_ALL: u8 = 0x20;

/// 65 bytes: report id 0, then `d2 <led> <mode> 08 RGB RGB <brightness 0-100> <speed>`.
fn dart_packet(led: u8, mode: u8, [r, g, b]: [u8; 3], brightness: u8, speed: u8) -> [u8; 65] {
    let mut p = [0u8; 65];
    p[1..13].copy_from_slice(&[0xd2, led, mode, 0x08, r, g, b, r, g, b, brightness, speed]);
    p
}

/// Modes 00 static, 12 cycle, 20 breathing, 30 reactive; speed 0x64 slowest
/// .. 0x00 fastest. Color modes go per LED (scroll wheel, then logo).
fn dart_packets(effect: &str, rgb: [u8; 3], brightness: i64, speed: &str) -> Vec<[u8; 65]> {
    let b = brightness.clamp(0, 100) as u8;
    let spd = match speed {
        "slow" => 0x64,
        "fast" => 0x00,
        _ => 0x32,
    };
    let (mode, spd) = match effect {
        "spectrum" => return vec![dart_packet(DART_LED_ALL, 0x12, rgb, b, spd)],
        "breathing" => (0x20, spd),
        "reactive" => (0x30, spd),
        _ => (0x00, 0x00),
    };
    vec![
        dart_packet(DART_LED_SCROLL, mode, rgb, b, spd),
        dart_packet(DART_LED_LOGO, mode, rgb, b, spd),
    ]
}

/// 65 bytes: report id 0, then `cmd` and its payload.
fn dart_cmd(cmd: &[u8]) -> [u8; 65] {
    let mut p = [0u8; 65];
    p[1..1 + cmd.len()].copy_from_slice(cmd);
    p
}

/// `d0 00 00 01 <code>`: 0 = 125, 1 = 250, 2 = 500, 3 = 1000 Hz.
const DART_RATES: [i64; 4] = [125, 250, 500, 1000];

fn dart_rate(hz: i64) -> Option<[u8; 65]> {
    let code = DART_RATES.iter().position(|&r| r == hz)?;
    Some(dart_cmd(&[0xd0, 0x00, 0x00, 0x01, code as u8]))
}

/// `d3 02 <stage 0-4> 02 <DPI / 50, u16 LE>`.
fn dart_stage(stage: u8, dpi: i64) -> [u8; 65] {
    let [lo, hi] = ((dpi / 50).clamp(1, 0xffff) as u16).to_le_bytes();
    dart_cmd(&[0xd3, 0x02, stage, 0x02, lo, hi])
}

/// `d3 00 00 01 <stage 0-4>`: select the active stage.
fn dart_active(stage: u8) -> [u8; 65] {
    dart_cmd(&[0xd3, 0x00, 0x00, 0x01, stage])
}

/// Heartbeat reply `51 00 00 03 <battery %> <1 = charging> ..`.
fn parse_heartbeat(r: &[u8]) -> Option<(u8, bool)> {
    match r {
        [0x51, _, _, _, level, chg, ..] if *level <= 100 => Some((*level, *chg == 0x01)),
        _ => None,
    }
}

const DART_EFFECTS: [(&str, &str); 4] = [
    ("static", "Static"),
    ("breathing", "Breathing"),
    ("spectrum", "Spectrum"),
    ("reactive", "Reactive"),
];

/// `de ff` (santeri3700 notes; OpenRGB `HyperXPulsefireDartController::Save`
/// by the same author): one line of reverse engineering, byte 1 unexplained.
const DART_SAVE_BLOCKED: &str = "only one reverse-engineering source documents the save (de ff) \
     and what its second byte selects is unknown";

fn dart_controls(effect: &str) -> Vec<SettingMeta> {
    // Five stages up to 16000 DPI (santeri3700); the enabled-stage mask
    // (`d3 01`) cannot be read back, so it is left as the mouse has it.
    let mut c = stage_controls(0x1f, 16000.0);
    c.push(SettingMeta::dropdown(
        SettingKey::ReportRate,
        "Report rate",
        DART_RATES
            .iter()
            .map(|&hz| SettingOption::int(hz, &format!("{hz} Hz")))
            .collect(),
    ));
    c.extend(lighting_controls(
        &DART_EFFECTS,
        effect,
        &["breathing", "spectrum", "reactive"],
        &["static", "breathing", "reactive"],
    ));
    c.push(
        SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory")
            .persistent()
            .blocked(DART_SAVE_BLOCKED),
    );
    c
}

struct DartMouse {
    spec: &'static Spec<Family>,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    battery: BatteryState,
    last_poll: Option<Instant>,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl DartMouse {
    fn open(spec: &'static Spec<Family>, pid: u16, paths: ConfigPaths) -> Result<Self> {
        // OpenRGB: dongle interface 2 (usage page ff00), cable interface 1 (ff13).
        let iface = if pid == spec.pids[0] { 2 } else { 1 };
        let (dev, instance) = open_node(spec.vid, pid, Node::Iface(iface))?;
        info!("{} open via {}", spec.name, dev.path.display());
        let mut a = Self {
            spec,
            id: DeviceId {
                vendor_id: spec.vid,
                product_id: pid,
                instance,
            },
            dev: Some(dev),
            settings: hako_core::config::load_device_settings(&paths, spec.vid, spec.pids[0])?,
            battery: BatteryState {
                label: "Mouse".into(),
                percent: None,
                charging: false,
                present: false,
            },
            last_poll: None,
            last_error: None,
            paths,
        };
        // Everything is volatile: restore the saved choices on every connect.
        let keys = STAGES.into_iter().chain([
            SettingKey::DpiStageActive,
            SettingKey::ReportRate,
            SettingKey::Effect,
        ]);
        for key in keys {
            if get_setting(&a.settings, key).is_some() {
                if let Err(e) = a.apply(key) {
                    warn!("{} restore {}: {e}", spec.name, key.as_str());
                    a.last_error = Some(e.to_string());
                }
            }
        }
        Ok(a)
    }

    fn apply(&mut self, key: SettingKey) -> Result<()> {
        let s = &self.settings;
        let single = if key == SettingKey::ReportRate {
            let hz = int(s, SettingKey::ReportRate, 1000);
            Some(dart_rate(hz).with_context(|| format!("unsupported rate {hz}"))?)
        } else if key == SettingKey::DpiStageActive {
            Some(dart_active((int(s, key, 1) - 1).clamp(0, 4) as u8))
        } else {
            STAGES
                .iter()
                .position(|&k| k == key)
                .map(|i| dart_stage(i as u8, int(s, key, 800)))
        };
        if let Some(p) = single {
            return self.dev.as_mut().context("not connected")?.write(&p);
        }
        let packets = dart_packets(
            text(s, SettingKey::Effect, "static"),
            rgb(s),
            int(s, SettingKey::Brightness, 100),
            text(s, SettingKey::Speed, "medium"),
        );
        let dev = self.dev.as_mut().context("not connected")?;
        for p in packets {
            dev.write(&p)?;
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

impl DeviceAdapter for DartMouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: dart_controls(text(&self.settings, SettingKey::Effect, "static")),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        merge_setting(&mut self.settings, key, value);
        let result = self.apply(key);
        self.last_error = result.as_ref().err().map(|e| e.to_string());
        hako_core::config::save_device_settings(
            &self.paths,
            self.spec.vid,
            self.spec.pids[0],
            &self.settings,
        )?;
        result
    }

    /// Heartbeat every [`POLL`]; silence means the mouse is off or asleep.
    fn tick(&mut self) -> Result<()> {
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        if self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return Ok(());
        }
        self.last_poll = Some(Instant::now());
        let reports = exchange(dev, &dart_cmd(&[0x51]), REPLY_WAIT, |r| {
            parse_heartbeat(r).is_some()
        })?;
        let b = &mut self.battery;
        match reports.iter().find_map(|r| parse_heartbeat(r)) {
            Some((level, charging)) => {
                (b.present, b.percent, b.charging) = (true, Some(level), charging)
            }
            None => (b.present, b.percent, b.charging) = (false, None, false),
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `33 02` read back from a Haste 2 Wireless (ngenuity-linux-cli §5.4).
    const TABLE: [u8; 30] = [
        0x33, 0x02, 0x08, 0x1f, 0x02, 0x07, 0x00, 0xff, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0xff,
        0x1f, 0x00, 0xff, 0xff, 0x00, 0x3f, 0x00, 0x00, 0xff, 0x00, 0x7f, 0x00, 0xff, 0xff, 0xff,
    ];

    #[test]
    fn dpi_table_read_and_rewrite() {
        let reply = pad(&TABLE, 64);
        assert_eq!(parse_dpi(&reply), Some((1600, Some(1000))));
        let edit = TableEdit {
            dpi: Some(3600),
            hz: Some(250),
            ..TableEdit::default()
        };
        let w = dpi_write(&reply, &edit).unwrap();
        assert_eq!(w[..7], [0x32, 0x01, 0x00, 0x00, 0x20, 0x1f, 0x02]);
        // Stage 2 = 3600 / 50 - 1 = 0x47; the others and every color kept.
        let mut stages = TABLE[5..20].to_vec();
        stages[10] = 0x47;
        assert_eq!(w[7..22], stages[..]);
        assert_eq!(w[22..32], TABLE[20..30]);
        assert_eq!(w.len(), 64);
        // Unchanged rewrite reproduces the table.
        assert_eq!(
            dpi_write(&reply, &TableEdit::default()).unwrap()[4..32],
            reply[2..30]
        );
    }

    #[test]
    fn dpi_stages_read_and_rewrite() {
        let reply = pad(&TABLE, 64);
        // Mask 1f, active stage index 2: 400, 800, 1600, 3200, 6400 DPI.
        let (mask, active, stages) = parse_stages(&reply).unwrap();
        assert_eq!((mask, active), (0x1f, 2));
        assert_eq!(stages, [400, 800, 1600, 3200, 6400].map(Some));
        // Stage 4 to 1000 DPI (code 19) and stage 1 active; the rest kept.
        let mut edit = TableEdit {
            active: Some(0),
            ..TableEdit::default()
        };
        edit.stages[3] = Some(1000);
        let w = dpi_write(&reply, &edit).unwrap();
        assert_eq!(w[..7], [0x32, 0x01, 0x00, 0x00, 0x08, 0x1f, 0x00]);
        assert_eq!(w[22..24], [19, 0]);
        assert_eq!(w[7..22], TABLE[5..20]);
        assert_eq!(w[24..32], TABLE[22..30]);
        // `dpi` lands in the new active stage, after the stage edits.
        edit.dpi = Some(900);
        assert_eq!(dpi_write(&reply, &edit).unwrap()[7..9], [17, 0]);
        // A stage outside the mask cannot become active.
        let mut two = reply.clone();
        two[3] = 0x03;
        assert_eq!(
            parse_stages(&two).unwrap().2,
            [Some(400), Some(800), None, None, None]
        );
        let to_third = TableEdit {
            active: Some(2),
            ..TableEdit::default()
        };
        assert!(dpi_write(&two, &to_third).is_none());
        let c = stage_controls(0x03, 26000.0);
        let pick = c
            .iter()
            .find(|m| m.key == SettingKey::DpiStageActive)
            .unwrap();
        assert!(pick.validate(&SettingValue::Int(2)).is_ok());
        assert!(pick.validate(&SettingValue::Int(3)).is_err());
        assert!(
            c.iter()
                .find(|m| m.key == SettingKey::DpiStage3)
                .unwrap()
                .hidden
        );
        assert!(stage_controls(0, 26000.0).is_empty());
    }

    #[test]
    fn dpi_follows_the_active_stage() {
        let int = SettingValue::Int;
        let mut s = SettingsMap::new();
        merge_setting(&mut s, SettingKey::DpiStage2, int(800));
        merge_setting(&mut s, SettingKey::DpiStage3, int(1600));
        merge_setting(&mut s, SettingKey::DpiStageActive, int(3));
        link_dpi(&mut s, SettingKey::DpiStageActive);
        assert_eq!(get_setting(&s, SettingKey::Dpi), Some(&int(1600)));
        merge_setting(&mut s, SettingKey::Dpi, int(2000));
        link_dpi(&mut s, SettingKey::Dpi);
        assert_eq!(get_setting(&s, SettingKey::DpiStage3), Some(&int(2000)));
        merge_setting(&mut s, SettingKey::DpiStage2, int(500));
        link_dpi(&mut s, SettingKey::DpiStage2);
        assert_eq!(get_setting(&s, SettingKey::Dpi), Some(&int(2000)));
    }

    #[test]
    fn sensor_brightness_and_presets() {
        let reply = pad(&[0x37, 0x02, 0x00, 0x00, 0x00, 0x02], 64);
        assert_eq!(parse_lift(&reply), Some(2));
        assert_eq!(
            lift_write(&reply, 1).unwrap(),
            pad(&[0x36, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], 64)
        );
        assert_eq!(
            lift_write(&reply, 2).unwrap()[..9],
            [0x36, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00]
        );
        // Captured values: 1 % -> 02, 51 % -> 82, 100 % -> ff.
        assert_eq!(brightness_write(1)[..5], [0x40, 0x01, 0x00, 0x00, 0x02]);
        assert_eq!(brightness_write(51)[4], 0x82);
        assert_eq!(brightness_write(100)[4], 0xff);
        assert_eq!(parse_brightness(&[0x41, 0x02, 0x82]), Some(51));
        assert_eq!(parse_brightness(&[0x41, 0x02, 0xff]), Some(100));
        // Live preset tests from the Haste 2 Wireless notes.
        assert_eq!(
            preset_write("off", [0; 3], "medium"),
            pad(&[0x42, 0x02, 0, 0, 0], 64)
        );
        assert_eq!(
            preset_write("static", [0xff, 0, 0], "medium"),
            pad(&[0x42, 0x02, 0, 0, 0x01, 0, 0, 0, 0x01, 0xff, 0, 0], 64)
        );
        assert_eq!(
            preset_write("breathing", [0, 0xff, 0], "medium"),
            pad(&[0x42, 0x02, 0, 0, 0x02, 0x32, 0, 0, 0x01, 0, 0xff, 0], 64)
        );
        assert_eq!(
            preset_write("spectrum", [0; 3], "medium"),
            pad(&[0x42, 0x02, 0, 0, 0x04, 0x32, 0, 0, 0xfe], 64)
        );
        assert_eq!(parse_effect(&[0x43, 0x03, 0x06, 0x32, 0x00]), None);
        assert_eq!(parse_effect(&[0x43, 0x03, 0x02]), Some("breathing"));
        assert!(is_reply(&[0x43, 0x02], 0x42, 0x02));
        assert!(!is_reply(&[0xff, 0x01], 0x42, 0x02));
    }

    /// A universal-protocol mouse with a live (0) and an onboard (1) slot.
    struct TwoSlots {
        table: [Vec<u8>; 2],
        bright: [u8; 2],
        drop_writes: bool,
        writes: Vec<Vec<u8>>,
    }

    impl TwoSlots {
        fn new() -> Self {
            let mut saved = pad(&TABLE, 64);
            saved[10] = 0x1f; // stage 2 differs on the onboard slot
            Self {
                table: [pad(&TABLE, 64), saved],
                bright: [0x82, 0xff],
                drop_writes: false,
                writes: vec![],
            }
        }

        fn query(&mut self, req: &[u8]) -> Result<Option<Vec<u8>>> {
            let keep = !self.drop_writes;
            Ok(Some(match *req {
                [0x32, 0x02, slot, ..] => self.table[usize::from(slot)].clone(),
                [0x40, 0x02, slot, ..] => pad(&[0x41, 0x02, self.bright[usize::from(slot)]], 64),
                [0x32, 0x01, slot, 0x00, ..] => {
                    self.writes.push(req.to_vec());
                    if keep {
                        let t = &mut self.table[usize::from(slot)];
                        t[2..60].copy_from_slice(&req[4..62]);
                    }
                    pad(&[0x33, 0x01], 64)
                }
                [0x40, 0x01, slot, 0x00, b, ..] => {
                    self.writes.push(req.to_vec());
                    if keep {
                        self.bright[usize::from(slot)] = b;
                    }
                    pad(&[0x41, 0x01], 64)
                }
                _ => bail!("unexpected {:02x?}", &req[..4]),
            }))
        }
    }

    #[test]
    fn save_copies_live_slot_to_onboard_slot() {
        let mut m = TwoSlots::new();
        store_table(&mut |r: &[u8]| m.query(r)).unwrap();
        // ngenuity-linux-cli encode_dpi_table(slot=1) / Saga Control
        // build_dpi_packet(profile=True): 32 01 01 00 <rate> <mask> <active> <stages>.
        assert_eq!(m.writes.len(), 1);
        assert_eq!(m.writes[0][..7], [0x32, 0x01, 0x01, 0x00, 0x08, 0x1f, 0x02]);
        assert_eq!(m.writes[0][7..32], TABLE[5..30]);
        assert_eq!(m.table[1], m.table[0]);

        store_brightness(&mut |r: &[u8]| m.query(r)).unwrap();
        assert_eq!(m.writes[1][..6], [0x40, 0x01, 0x01, 0x00, 0x82, 0x00]);
        assert_eq!(m.bright, [0x82, 0x82]);
    }

    #[test]
    fn save_aborts_on_bad_tables_and_checks_read_back() {
        // Invalid onboard table (unknown rate code): nothing written.
        let mut m = TwoSlots::new();
        m.table[1][2] = 0x03;
        assert!(store_table(&mut |r: &[u8]| m.query(r)).is_err());
        assert!(m.writes.is_empty());
        // Invalid live table (enabled stage with DPI code 0): nothing written.
        let mut m = TwoSlots::new();
        m.table[0][5] = 0x00;
        assert!(store_table(&mut |r: &[u8]| m.query(r)).is_err());
        assert!(m.writes.is_empty());
        // Active stage not enabled.
        let mut m = TwoSlots::new();
        m.table[0][4] = 0x06;
        assert!(store_table(&mut |r: &[u8]| m.query(r)).is_err());
        // A write the mouse dropped is reported, not assumed.
        let mut m = TwoSlots::new();
        m.drop_writes = true;
        let err = store_table(&mut |r: &[u8]| m.query(r)).unwrap_err();
        assert!(err.to_string().contains("reads back"), "{err}");
        assert!(store_brightness(&mut |r: &[u8]| m.query(r)).is_err());
        // No reply at all.
        assert!(store_table(&mut |_: &[u8]| Ok(None)).is_err());
    }

    #[test]
    fn save_controls() {
        for presets in [true, false] {
            let c = universal_controls("static", 4, presets, 0x1f);
            let save = c
                .iter()
                .find(|c| c.key == SettingKey::SaveToDevice)
                .unwrap();
            assert!(save.persistent && save.blocked.is_none());
        }
        assert!(saved_by(SettingKey::Dpi) && saved_by(SettingKey::Brightness));
        assert!(!saved_by(SettingKey::LiftOffDistance) && !saved_by(SettingKey::Effect));
        let dart = dart_controls("static");
        let save = dart
            .iter()
            .find(|c| c.key == SettingKey::SaveToDevice)
            .unwrap();
        assert!(save.persistent && save.blocked.is_some());
    }

    #[test]
    fn dart_packets_match_layout() {
        let p = dart_packets("static", [1, 2, 3], 100, "fast");
        assert_eq!(p.len(), 2);
        assert_eq!(
            p[0][..13],
            [0x00, 0xd2, 0x10, 0x00, 0x08, 1, 2, 3, 1, 2, 3, 0x64, 0x00]
        );
        assert_eq!(p[1][2], DART_LED_LOGO);
        let p = dart_packets("breathing", [1, 2, 3], 50, "slow");
        assert_eq!(p[0][3..13], [0x20, 0x08, 1, 2, 3, 1, 2, 3, 50, 0x64]);
        let p = dart_packets("spectrum", [0; 3], 100, "medium");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0][1..5], [0xd2, 0x20, 0x12, 0x08]);
        assert_eq!(p[0][12], 0x32);
        assert!(p.iter().all(|p| p.len() == 65));
    }

    #[test]
    fn every_model_has_controls() {
        assert!(!universal_controls("static", 4, true, 0).is_empty());
        for (e, _) in UNI_EFFECTS {
            assert!(universal_controls(e, 4, true, 0)
                .iter()
                .find(|c| c.key == SettingKey::Effect)
                .unwrap()
                .validate(&SettingValue::Text(e.into()))
                .is_ok());
        }
        for (e, _) in DART_EFFECTS {
            let c = dart_controls(e);
            let color = c.iter().find(|c| c.key == SettingKey::ColorR).unwrap();
            assert_eq!(color.hidden, e == "spectrum");
        }
        // Saga Pro: brightness only; cable capped at 1000 Hz.
        let saga = universal_controls("", 6, false, 0x0f);
        assert!(saga.iter().all(|c| !matches!(
            c.key,
            SettingKey::Effect | SettingKey::ColorR | SettingKey::Speed
        )));
        let rate = saga
            .iter()
            .find(|c| c.key == SettingKey::ReportRate)
            .unwrap();
        assert!(rate.validate(&SettingValue::Int(4000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(8000)).is_err());
        let Family::Universal { links, .. } = SAGA_PRO.family else {
            unreachable!()
        };
        assert_eq!(links.len(), SAGA_PRO.pids.len());
        assert!(links.iter().all(|l| SAGA_PRO.pids.contains(&l.0)));
    }

    #[test]
    fn dart_rate_and_heartbeat() {
        assert_eq!(
            dart_rate(1000).unwrap()[..7],
            [0x00, 0xd0, 0x00, 0x00, 0x01, 0x03, 0x00]
        );
        assert_eq!(dart_rate(125).unwrap()[5], 0x00);
        assert!(dart_rate(2000).is_none());
        // Heartbeat example from the santeri3700 notes.
        assert_eq!(
            parse_heartbeat(&[0x51, 0, 0, 0x03, 57, 0x01, 0x0f, 0x8b, 0x10]),
            Some((57, true))
        );
        assert_eq!(parse_heartbeat(&[0x51, 0, 0, 0x03, 0xff, 0]), None);
        let c = dart_controls("static");
        let rate = c.iter().find(|c| c.key == SettingKey::ReportRate).unwrap();
        assert!(rate.validate(&SettingValue::Int(500)).is_ok());
    }

    #[test]
    fn dart_dpi_stages_match_capture() {
        // santeri3700 ngenuity_initialization_2: stage 1 = 800, stage 2 = 1000 DPI,
        // then stage 1 selected.
        assert_eq!(
            dart_stage(0, 800)[..7],
            [0x00, 0xd3, 0x02, 0x00, 0x02, 0x10, 0x00]
        );
        assert_eq!(
            dart_stage(1, 1000)[1..7],
            [0xd3, 0x02, 0x01, 0x02, 0x14, 0x00]
        );
        // Notes: 16000 DPI = 0x0140 -> 40 01.
        assert_eq!(dart_stage(4, 16000)[5..7], [0x40, 0x01]);
        assert_eq!(dart_active(0)[..6], [0x00, 0xd3, 0x00, 0x00, 0x01, 0x00]);
        let c = dart_controls("static");
        let stage = c.iter().find(|m| m.key == SettingKey::DpiStage5).unwrap();
        assert!(stage.validate(&SettingValue::Int(16000)).is_ok());
        assert!(stage.validate(&SettingValue::Int(16050)).is_err());
        let active = c
            .iter()
            .find(|m| m.key == SettingKey::DpiStageActive)
            .unwrap();
        assert!(active.validate(&SettingValue::Int(5)).is_ok());
    }

    /// Read-only probe: connection, battery and the live DPI table.
    #[test]
    #[ignore]
    fn hw_haste_2_wireless_read() {
        let paths = ConfigPaths::xdg().unwrap();
        let mut m = UniversalMouse::open(&HASTE_2_WIRELESS, 0x0f98, paths).unwrap();
        m.query(&uni(0x52, 0x01, &[])).unwrap();
        m.query(&uni(0x50, 0x02, &[])).unwrap();
        let t = m.query(&uni(0x32, 0x02, &[0])).unwrap();
        eprintln!(
            "online={} battery={:?} dpi={:?}",
            m.online,
            m.battery,
            t.as_deref().and_then(parse_dpi)
        );
    }
}
