//! Mice on Compx / ATK chipsets: ATK, VXE, VGN and Lamzu. Battery and
//! charging state on every model; on the 16-byte ("COMPX", v1) protocol also
//! report rate, DPI stages, lift-off, click debounce and angle snapping.
//! Those are EEPROM (flash) writes (every setting of these mice lives on the
//! mouse), so they are `persistent`; the rest is a blocked placeholder.
//! Research: `docs/onboard-writes/compx.md`, `docs/vendor-re/compx.md`.
//!
//! Two protocols, both on vendor report 8 and told apart by the report's
//! size (DPIdeck `find_nodes`; OpenATK says the family follows the hardware
//! revision, not the model): 16 bytes = v1 EEPROM read / write, 63 bytes =
//! v2 command frames (read here for battery and link state only). Layouts
//! are reimplemented from, and agree across:
//! - lamzu-cfg (MIT / Apache-2.0) `src/atlantis.rs`, `src/atlantis/hid.rs`,
//!   `src/device.rs`, issue #1: USB captures of Lamzu's own app; Lamzu IDs
//!   <https://github.com/LeadSun/lamzu-cfg>
//! - atk-control (MIT) `atk_control/protocol.py`, `controller.py`,
//!   `docs/PROTOCOL.md`, issue #6: from the ATK HUB WebHID driver, tested on
//!   the MAD 8K / VXE 1K / VGN 4K receivers; A9 IDs
//!   <https://github.com/NasirA1991/atk-control>
//! - DPIdeck (CC0; tables MIT from OpenATK) `atk.py`
//!   <https://github.com/Soulmanyes/dpideck> and OpenATK `docs/PROTOCOL.md`
//!   <https://github.com/SunnybeeXD/OpenATK>
//! - lamzu-ctl (MIT) `lamzu_ctl.py`, README: Lamzu IDs and battery layout
//!   <https://github.com/anany1702/lamzu-ctl>
//! - the vendors' own software, read statically (`docs/vendor-re/compx.md`):
//!   ATK HUB 3.2.33 and VGN HUB web drivers, Lamzu Crystal Wireless 2.1.0.43.
//!   They give the full EEPROM map, the online / link-type / CID-MID
//!   commands, the unsolicited status report, the DPI codecs per sensor and
//!   the allowed lift-off and debounce values; the device tables in
//!   `tables.rs` come from them.
//!
//! Only commands 0x01 (link type), 0x03 (online), 0x04 (battery), 0x07 /
//! 0x08 (EEPROM write / read), 0x10 (CID / MID) and v2 0x07 / 0x0f / 0x23 /
//! 0x81 (battery / charging / online) are ever sent. Never 0x09, 0x0b, 0x0c,
//! 0x0d, 0x11, 0xf0 (factory reset, VID/PID, descriptor string, firmware
//! update, MTK mode, KBC id: 0x0d erased firmware, lamzu-cfg #8).

mod dpi;
mod protocol;
mod tables;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{descriptor_items, enumerate_hidraw, HidrawDevice};
use anyhow::{bail, ensure, Context, Result};
use dpi::{Sensor, STAGES};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::merge_setting;
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use protocol::{Link, Row, ADDR_ACTIVE, ADDR_RATE, RATES, REPORT_ID, V2_LOCAL, V2_REMOTE};
use std::time::{Duration, Instant};
use tables::Info;
use tracing::{info, warn};

/// Compx (Lamzu, VXE, VGN), ATK / MADLIONS and the newer VGN vendor IDs.
const COMPX: u16 = 0x3554;
const ATK: u16 = 0x373b;
const VGN: u16 = 0x391d;

const fn model(vendor_id: u16, name: &'static str, product_ids: &'static [u16]) -> Model {
    Model {
        vendor_id,
        product_ids,
        kind: DeviceKind::Mouse,
        name,
        usbfs: false,
        multi: false,
        open,
    }
}

pub use tables::MODELS;

/// What the tables know about a PID (the receivers' sensor comes later, from
/// the paired mouse's CID / MID).
fn info_of(vid: u16, pid: u16) -> &'static Info {
    const UNKNOWN: Info = Info {
        vendor_id: 0,
        product_id: 0,
        sensor: None,
        receiver: false,
        v2: false,
        no_zero_debounce: false,
        rate_hint: 1000,
    };
    tables::INFO
        .iter()
        .find(|i| i.vendor_id == vid && i.product_id == pid)
        .unwrap_or(&UNKNOWN)
}

/// Why the report rate is not offered on this link, if it is not.
fn rate_block(vid: u16, pid: u16) -> Option<&'static str> {
    (vid == COMPX && pid == 0xf50f).then_some(
        "over the cable this address reads back the wireless rate and the cable stays at \
         1000 Hz, so what a write changes is unclear (lamzu-cfg #1)",
    )
}

const NO_SENSOR: &str = "the sensor of this mouse is unknown (its CID / MID is not in the vendor \
tables, or the receiver has no mouse right now), and the DPI and lift-off encodings depend on it";
const DPI_3955: &str = "the PAW3955 stores DPI in 6-byte records whose high bits the vendors' \
tools read differently (ATK HUB 16 bit, VGN HUB 18 bit)";
const LOD_3955: &str = "the PAW3955 has an 11-step lift-off scale that only the VGN HUB documents";
const V2_BLOCKED: &str = "no source has tested settings writes on the 64-byte (0xFF05) \
protocol on hardware";

/// Click debounce values the vendors' apps offer (ms); `noZeroKeyDebounce`
/// models drop the 0.
const DEBOUNCE: [i64; 7] = [0, 1, 2, 4, 8, 15, 20];

const ADDR_LOD: u16 = 0x000a;
const ADDR_DEBOUNCE: u16 = 0x00a9;
const ADDR_ANGLE: u16 = 0x00af;

const STAGE_KEYS: [SettingKey; STAGES as usize] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proto {
    V1,
    /// v2 with its `target` byte.
    V2(u8),
}

/// Byte size of `id`'s input report in a HID report descriptor.
fn report_len(desc: &[u8], id: u8) -> Option<u32> {
    let (mut rid, mut size, mut count) = (0, 0, 0);
    for (tag, v) in descriptor_items(desc) {
        match tag {
            0x84 => rid = v,
            0x74 => size = v,
            0x94 => count = v,
            0x80 if rid == u32::from(id) => return Some(size * count / 8),
            _ => {}
        }
    }
    None
}

fn proto_of(len: u32, receiver: bool) -> Option<Proto> {
    match len {
        16 => Some(Proto::V1),
        63 => Some(Proto::V2(if receiver { V2_REMOTE } else { V2_LOCAL })),
        _ => None,
    }
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let vid = ctx.vendor_id;
    let model = tables::MODELS
        .iter()
        .find(|m| m.vendor_id == vid && m.product_ids.contains(&pid))
        .with_context(|| format!("no Compx model for {vid:04x}:{pid:04x}"))?;
    let facts = info_of(vid, pid);
    let (info, proto) = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .find_map(|d| {
            let p = proto_of(
                report_len(&d.report_descriptor(), REPORT_ID)?,
                facts.receiver,
            )?;
            Some((d, p))
        })
        .with_context(|| format!("{vid:04x}:{pid:04x} has no Compx configuration interface"))?;
    let dev = HidrawDevice::open(&info.path)?;
    if facts.v2 != matches!(proto, Proto::V2(_)) {
        warn!(
            "{vid:04x}:{pid:04x}: the vendor tables say {} but the node looks like {proto:?}",
            if facts.v2 { "v2" } else { "v1" }
        );
    }
    info!(
        "{} open via {} ({proto:?})",
        model.name,
        info.path.display()
    );
    let mut m = Mouse {
        name: model.name,
        id: DeviceId {
            vendor_id: vid,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        dev: Some(dev),
        proto,
        facts,
        sensor: facts.sensor,
        identified: false,
        link_type: None,
        rate_block: rate_block(vid, pid),
        row: None,
        stages: [None; STAGES as usize],
        lod: None,
        debounce: None,
        angle: None,
        battery: None,
        battery_read: None,
        online: None,
        checked_online: None,
        want_refresh: false,
        last_read: None,
        last_error: None,
    };
    m.refresh();
    Ok(vec![Box::new(m)])
}

const REFRESH: Duration = Duration::from_secs(60);
/// How often a receiver is asked whether its mouse is awake (the vendor's
/// own app uses the same 5 s).
const ONLINE_POLL: Duration = Duration::from_secs(5);
/// A battery level this old (several failed refreshes) is shown as absent.
const BATTERY_STALE: Duration = Duration::from_secs(300);

/// Whether a battery read at `at` still counts at `now`.
fn fresh(at: Option<Instant>, now: Instant) -> bool {
    at.is_some_and(|t| now.saturating_duration_since(t) < BATTERY_STALE)
}

struct Mouse {
    name: &'static str,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    proto: Proto,
    facts: &'static Info,
    /// From the paired mouse's CID / MID, else the PID table.
    sensor: Option<Sensor>,
    /// Link type and CID / MID have been read (v1).
    identified: bool,
    link_type: Option<u8>,
    rate_block: Option<&'static str>,
    /// Last rate row read (v1).
    row: Option<Row>,
    /// DPI of stages 1..=5 as stored (v1; `None`: not read or not decodable).
    stages: [Option<i64>; STAGES as usize],
    lod: Option<u8>,
    debounce: Option<u8>,
    angle: Option<bool>,
    /// (percent, charging), kept while the mouse sleeps.
    battery: Option<(u8, bool)>,
    /// When `battery` was last read.
    battery_read: Option<Instant>,
    /// Receivers: the mouse is linked and awake. `None`: unknown.
    online: Option<bool>,
    checked_online: Option<Instant>,
    /// A status report or a wake-up asks for an early refresh.
    want_refresh: bool,
    last_read: Option<Instant>,
    last_error: Option<String>,
}

impl Mouse {
    /// The link's top report rate: from the link type the mouse reports,
    /// else what its name says.
    fn max_rate(&self) -> i64 {
        self.link_type
            .and_then(protocol::link_ceiling)
            .unwrap_or(self.facts.rate_hint)
    }

    /// Link type, CID / MID and from them the sensor (v1).
    fn identify(&mut self, dev: &mut HidrawDevice) -> Result<()> {
        self.link_type = protocol::read_link(dev).ok().map(|(_, _, t)| t);
        let (cid, mid) = protocol::read_cid_mid(dev)?;
        if let Some(&(_, _, s)) = tables::CID_MID
            .iter()
            .find(|&&(c, m, _)| c == cid && m == mid)
        {
            self.sensor = Some(s);
        }
        self.identified = true;
        Ok(())
    }

    fn refresh(&mut self) {
        self.last_read = Some(Instant::now());
        let Some(mut dev) = self.dev.take() else {
            return;
        };
        let read = self.read_all(&mut dev);
        self.dev = Some(dev);
        self.last_error = read.err().map(|e| format!("{e:#}"));
    }

    fn read_all(&mut self, dev: &mut HidrawDevice) -> Result<()> {
        let proto = self.proto;
        self.battery = Some(match proto {
            Proto::V1 => protocol::read_battery(dev)?,
            Proto::V2(target) => protocol::read_v2_battery(dev, target)?,
        });
        self.battery_read = Some(Instant::now());
        if proto == Proto::V1 {
            self.row = Some(protocol::read_row(dev)?);
            // A receiver without a mouse cannot say; retried next time.
            let identified = if self.identified {
                Ok(())
            } else {
                self.identify(dev)
            };
            self.read_extras(dev)?;
            identified.context("could not identify the sensor (tried again on the next read)")?;
        }
        Ok(())
    }

    /// Stages, lift-off, debounce, angle snapping.
    fn read_extras(&mut self, dev: &mut HidrawDevice) -> Result<()> {
        let count = self.row.map_or(0, |r| r[1]);
        if let Some(sensor) = self.sensor {
            for i in 0..STAGES.min(count) {
                let d = protocol::read_bytes(dev, dpi::stage_addr(i), dpi::RECORD)?;
                self.stages[usize::from(i)] = dpi::decode(sensor, &d).map(|(x, y)| (x + y) / 2);
            }
            if sensor.lift_off().is_some() {
                self.lod = protocol::read_pair(dev, ADDR_LOD)?;
            }
            if sensor.angle_snapping() {
                self.angle = protocol::read_pair(dev, ADDR_ANGLE)?.map(|v| v == 1);
            }
        }
        self.debounce = protocol::read_pair(dev, ADDR_DEBOUNCE)?;
        Ok(())
    }

    /// Unsolicited reports (a status change on v1, sleep / wake on v2) that
    /// arrived since the last exchange.
    fn poll_events(&mut self) {
        let Some(dev) = self.dev.as_mut() else {
            return;
        };
        let mut buf = [0u8; 128];
        for _ in 0..8 {
            let Ok(n) = dev.recv(&mut buf, Duration::ZERO) else {
                break;
            };
            let raw = &buf[..n];
            if let Some(flags) = protocol::status_flags(raw) {
                // DPI, rate, profile and battery changes: read everything.
                self.want_refresh |= flags & 0x47 != 0;
            } else if let Some(awake) = protocol::v2_wake_event(raw) {
                self.online = Some(awake);
                self.want_refresh |= awake;
            }
        }
    }

    /// Ask a receiver whether its mouse is awake.
    fn check_online(&mut self) {
        self.checked_online = Some(Instant::now());
        let Some(dev) = self.dev.as_mut() else {
            return;
        };
        let asked = match self.proto {
            Proto::V1 => protocol::read_online(dev),
            Proto::V2(_) => protocol::read_v2_online(dev),
        };
        // No answer says nothing about the mouse: keep what we knew.
        let Ok(up) = asked else {
            return;
        };
        if up && self.online != Some(true) {
            self.want_refresh = true;
        }
        self.online = Some(up);
    }

    fn settings(&self) -> SettingsMap {
        let mut s = SettingsMap::new();
        if let Some([rate, _, active]) = self.row {
            if let Some(hz) = protocol::rate_hz(rate) {
                merge_setting(&mut s, SettingKey::ReportRate, SettingValue::Int(hz));
            }
            merge_setting(
                &mut s,
                SettingKey::DpiStageActive,
                SettingValue::Int(i64::from(active) + 1),
            );
        }
        for (key, dpi) in STAGE_KEYS.into_iter().zip(self.stages) {
            if let Some(v) = dpi {
                merge_setting(&mut s, key, SettingValue::Int(v));
            }
        }
        if let Some(v) = self.lod {
            merge_setting(
                &mut s,
                SettingKey::LiftOffDistance,
                SettingValue::Int(v.into()),
            );
        }
        if let Some(v) = self.debounce {
            merge_setting(&mut s, SettingKey::Debounce, SettingValue::Int(v.into()));
        }
        if let Some(v) = self.angle {
            merge_setting(&mut s, SettingKey::AngleSnapping, SettingValue::Bool(v));
        }
        s
    }

    fn debounce_options(&self) -> Vec<SettingOption> {
        let mut set: Vec<i64> = DEBOUNCE
            .into_iter()
            .filter(|&v| v != 0 || !self.facts.no_zero_debounce)
            .collect();
        // Whatever the mouse holds stays selectable.
        if let Some(v) = self.debounce.map(i64::from).filter(|v| !set.contains(v)) {
            set.push(v);
            set.sort_unstable();
        }
        set.into_iter()
            .map(|v| SettingOption::int(v, &format!("{v} ms")))
            .collect()
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        let current = self.row.and_then(|r| protocol::rate_hz(r[0]));
        let ceiling = self.max_rate();
        let rates = RATES
            .iter()
            .filter(|&&(hz, _)| hz <= ceiling || Some(hz) == current)
            .map(|&(hz, _)| SettingOption::int(hz, &format!("{hz} Hz")))
            .collect();
        let rate = SettingMeta::dropdown(ReportRate, "Report rate", rates).persistent();
        let count = self.row.map_or(8, |r| i64::from(r[1]));
        let active = SettingMeta::dropdown(
            DpiStageActive,
            "Active DPI stage",
            (1..=count)
                .map(|i| SettingOption::int(i, &i.to_string()))
                .collect(),
        )
        .persistent();
        let mut c = vec![
            match self.rate_block {
                Some(reason) => rate.blocked(reason),
                None => rate,
            },
            active,
        ];
        let (lo, hi, step) = self.sensor.map_or((50, 26_000, 50), Sensor::range);
        for (i, key) in STAGE_KEYS.into_iter().enumerate() {
            let label = format!("DPI stage {}", i + 1);
            let meta = SettingMeta::slider(key, &label, lo as f64, hi as f64, step as f64)
                .unit("dpi")
                .persistent();
            c.push(match self.sensor {
                None => meta.blocked(NO_SENSOR),
                Some(s) if !s.writable() => meta.blocked(DPI_3955),
                Some(_) if i as i64 >= count => meta.blocked("the mouse has fewer DPI stages"),
                Some(_) => meta,
            });
        }
        let lod = |opts: Vec<SettingOption>| {
            SettingMeta::dropdown(LiftOffDistance, "Lift-off distance", opts).persistent()
        };
        c.push(match self.sensor {
            None => lod(vec![
                SettingOption::int(1, "1 mm"),
                SettingOption::int(2, "2 mm"),
            ])
            .blocked(NO_SENSOR),
            Some(s) => match s.lift_off() {
                Some(set) => lod(set.iter().map(|&(v, l)| SettingOption::int(v, l)).collect()),
                None => lod(vec![SettingOption::int(1, "1 mm")]).blocked(LOD_3955),
            },
        });
        c.push(
            SettingMeta::dropdown(Debounce, "Click debounce", self.debounce_options())
                .unit("ms")
                .persistent(),
        );
        if self.sensor.is_some_and(Sensor::angle_snapping) {
            c.push(SettingMeta::toggle(AngleSnapping, "Angle snapping").persistent());
        }
        if let Proto::V2(_) = self.proto {
            c = c.into_iter().map(|m| m.blocked(V2_BLOCKED)).collect();
        }
        c
    }

    fn write(&mut self, key: SettingKey, value: &SettingValue) -> Result<()> {
        if let Some(reason) = self
            .controls()
            .into_iter()
            .find(|m| m.key == key)
            .and_then(|m| m.blocked)
        {
            bail!("{} is not available: {reason}", key.as_str());
        }
        let sensor = self.sensor;
        let count = self.row.map_or(8, |r| r[1]);
        let debounce_options = self.debounce_options();
        let dev = self.dev.as_mut().context("not connected")?;
        match key {
            SettingKey::ReportRate | SettingKey::DpiStageActive => {
                let v = value.as_int().context("expected a number")?;
                let (addr, code) = if key == SettingKey::ReportRate {
                    let code = RATES
                        .iter()
                        .find(|&&(hz, _)| hz == v)
                        .with_context(|| format!("{v} Hz is not a report rate"))?
                        .1;
                    (ADDR_RATE, code)
                } else {
                    ensure!((1..=8).contains(&v), "stage {v} is out of range");
                    (ADDR_ACTIVE, (v - 1) as u8)
                };
                let row = protocol::write_pair(dev, addr, code, |row| {
                    ensure!(
                        addr != ADDR_ACTIVE || code < row[1],
                        "the mouse has {} DPI stages",
                        row[1]
                    );
                    Ok(())
                })?;
                self.row = Some(row);
            }
            SettingKey::Debounce => {
                let v = value.as_int().context("expected a number")?;
                ensure!(
                    debounce_options
                        .iter()
                        .any(|o| o.value == SettingValue::Int(v)),
                    "{v} ms is not a debounce time of this mouse"
                );
                let v = v as u8;
                self.debounce = Some(protocol::write_pair_at(dev, ADDR_DEBOUNCE, v, |_| Ok(()))?);
            }
            SettingKey::LiftOffDistance => {
                let v = value.as_int().context("expected a number")?;
                let set = sensor
                    .and_then(Sensor::lift_off)
                    .context("no lift-off here")?;
                ensure!(
                    set.iter().any(|&(c, _)| c == v),
                    "lift-off code {v} is not offered by this sensor"
                );
                self.lod = Some(protocol::write_pair_at(dev, ADDR_LOD, v as u8, |old| {
                    ensure!(
                        set.iter().any(|&(c, _)| c == i64::from(old)),
                        "the stored lift-off code {old} is unknown for this sensor"
                    );
                    Ok(())
                })?);
            }
            SettingKey::AngleSnapping => {
                let on = value.as_bool().context("expected on / off")?;
                let v = protocol::write_pair_at(dev, ADDR_ANGLE, u8::from(on), |old| {
                    ensure!(old <= 1, "the stored angle snapping value {old} is unknown");
                    Ok(())
                })?;
                self.angle = Some(v == 1);
            }
            k => {
                let i = STAGE_KEYS
                    .iter()
                    .position(|&s| s == k)
                    .with_context(|| format!("{} is not a {} setting", k.as_str(), self.name))?;
                let sensor = sensor.context("sensor unknown")?;
                ensure!((i as u8) < count, "the mouse has {count} DPI stages");
                let dpi = value.as_int().context("expected a number")?;
                let rec = dpi::encode(sensor, dpi, dpi).with_context(|| {
                    format!("this sensor cannot store {dpi} DPI exactly (steps and range)")
                })?;
                protocol::write_record(dev, dpi::stage_addr(i as u8), rec, |old| {
                    // The stored record must decode and encode back to itself.
                    let (x, y) = dpi::decode(sensor, old)
                        .context("not writing: the stored DPI record is not recognised")?;
                    ensure!(
                        dpi::encode(sensor, x, y).is_some_and(|r| r == old),
                        "not writing: the stored DPI record does not match the sensor's codec"
                    );
                    Ok(())
                })?;
                self.stages[i] = Some(dpi);
            }
        }
        Ok(())
    }
}

impl DeviceAdapter for Mouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: self
                .battery
                .map(|(percent, charging)| BatteryState {
                    label: "Mouse".into(),
                    percent: Some(percent),
                    charging,
                    present: self.online != Some(false) && fresh(self.battery_read, Instant::now()),
                })
                .into_iter()
                .collect(),
            settings: self.settings(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        match self.write(key, &value) {
            Ok(()) => {
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.last_error = Some(format!("{e:#}"));
                Err(e)
            }
        }
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() {
            return Ok(());
        }
        self.poll_events();
        if self.facts.receiver
            && self
                .checked_online
                .is_none_or(|t| t.elapsed() >= ONLINE_POLL)
        {
            self.check_online();
        }
        // A sleeping mouse answers nothing: skip the 400 ms timeouts.
        if self.online != Some(false)
            && (self.want_refresh || self.last_read.is_none_or(|t| t.elapsed() >= REFRESH))
        {
            self.want_refresh = false;
            self.refresh();
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
    use std::collections::HashSet;

    fn mouse(vid: u16, pid: u16, proto: Proto, row: Option<Row>) -> Mouse {
        let facts = info_of(vid, pid);
        Mouse {
            name: "test",
            id: DeviceId {
                vendor_id: vid,
                product_id: pid,
                instance: "x".into(),
            },
            dev: None,
            proto,
            facts,
            sensor: facts.sensor,
            identified: false,
            link_type: None,
            rate_block: rate_block(vid, pid),
            row,
            stages: [None; STAGES as usize],
            lod: None,
            debounce: None,
            angle: None,
            battery: Some((50, false)),
            battery_read: Some(Instant::now()),
            online: None,
            checked_online: None,
            want_refresh: false,
            last_read: None,
            last_error: None,
        }
    }

    #[test]
    fn models_are_unique_and_every_link_is_known() {
        let mut seen = HashSet::new();
        for m in MODELS {
            assert!([COMPX, ATK, VGN].contains(&m.vendor_id));
            for &pid in m.product_ids {
                assert!(seen.insert((m.vendor_id, pid)), "{pid:04x} listed twice");
                assert!(
                    tables::INFO
                        .iter()
                        .any(|i| i.vendor_id == m.vendor_id && i.product_id == pid),
                    "{pid:04x} has no facts"
                );
            }
        }
        // Pulsar's Nordic receivers share VID 0x3554; never claim them. The
        // bootloader / recovery PIDs are not mice to talk to either.
        for pid in [0xf507, 0xf508, 0xf509, 0xf2ff, 0xf400, 0xf410] {
            assert!(!seen.contains(&(COMPX, pid)));
        }
        for (vid, pid) in [
            (COMPX, 0xf517),
            (COMPX, 0xf502),
            (VGN, 0x1a1a),
            (ATK, 0x1014),
            (ATK, 0x1375),
        ] {
            assert!(seen.contains(&(vid, pid)), "{vid:04x}:{pid:04x}");
        }
        assert!(info_of(COMPX, 0xf510).receiver);
        assert_eq!(info_of(COMPX, 0xf510).rate_hint, 4000);
        assert!(!info_of(COMPX, 0xf50f).receiver);
        assert_eq!(info_of(ATK, 0x1040).rate_hint, 8000);
        assert_eq!(info_of(ATK, 0x1264).sensor, Some(Sensor::Paw3950Ultra));
        assert!(info_of(ATK, 0x11dc).v2 && info_of(ATK, 0x11dc).receiver);
    }

    #[test]
    fn report_size_picks_the_protocol() {
        // Vendor page 0xFF02, report 8: 16-byte input and output (atk-control).
        let v1 = [
            0x06, 0x02, 0xff, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x08, 0x75, 0x08, 0x95, 0x10, 0x81,
            0x02, 0x91, 0x02, 0xc0,
        ];
        assert_eq!(report_len(&v1, REPORT_ID), Some(16));
        assert_eq!(proto_of(16, true), Some(Proto::V1));
        let mut v2 = v1;
        v2[1] = 0x05;
        v2[12] = 0x3f;
        assert_eq!(report_len(&v2, REPORT_ID), Some(63));
        assert_eq!(proto_of(63, true), Some(Proto::V2(V2_REMOTE)));
        assert_eq!(proto_of(63, false), Some(Proto::V2(V2_LOCAL)));
        assert_eq!(report_len(&v1, 0x09), None);
        assert_eq!(proto_of(8, true), None);
    }

    #[test]
    fn v1_offers_what_the_sources_agree_on() {
        let mut m = mouse(COMPX, 0xf510, Proto::V1, Some([0x40, 3, 1]));
        let c = m.controls();
        for meta in &c {
            assert!(meta.persistent, "{:?}", meta.key);
        }
        let blocked = |c: &[SettingMeta], k: SettingKey| {
            c.iter().find(|m| m.key == k).unwrap().blocked.is_some()
        };
        // Lamzu has no known sensor: rate, active stage and debounce only.
        assert!(!blocked(&c, SettingKey::ReportRate));
        assert!(!blocked(&c, SettingKey::DpiStageActive));
        assert!(!blocked(&c, SettingKey::Debounce));
        assert!(blocked(&c, SettingKey::DpiStage1));
        assert!(blocked(&c, SettingKey::LiftOffDistance));
        let rate = &c[0];
        // Up to the receiver's 4000 Hz, plus the stored 8000 Hz.
        assert!(rate.validate(&SettingValue::Int(4000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(8000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(2000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(300)).is_err());
        // The mouse's own link type decides: an 8K receiver offers 8000.
        m.row = Some([0x01, 3, 1]);
        m.link_type = Some(5);
        assert!(m.controls()[0].validate(&SettingValue::Int(8000)).is_ok());
        m.link_type = Some(0);
        assert!(m.controls()[0].validate(&SettingValue::Int(2000)).is_err());
        let active = &m.controls()[1];
        assert!(active.validate(&SettingValue::Int(3)).is_ok());
        assert!(active.validate(&SettingValue::Int(4)).is_err());
        let s = m.snapshot();
        assert_eq!(s.batteries[0].percent, Some(50));
        assert!(s.batteries[0].present);
        // 1K receiver: nothing above 1000 Hz.
        let m = mouse(COMPX, 0xf58a, Proto::V1, Some([0x01, 4, 0]));
        assert!(m.controls()[0].validate(&SettingValue::Int(2000)).is_err());
    }

    #[test]
    fn a_known_sensor_unblocks_dpi_lift_off_and_angle_snapping() {
        // ATK F1 PRO: PAW3395.
        let mut m = mouse(COMPX, 0xf502, Proto::V1, Some([0x01, 3, 0]));
        m.stages = [Some(800), Some(1600), Some(3200), None, None];
        m.lod = Some(1);
        m.debounce = Some(4);
        m.angle = Some(false);
        let c = m.controls();
        let get = |k: SettingKey| c.iter().find(|m| m.key == k).unwrap();
        assert!(get(SettingKey::DpiStage1).blocked.is_none());
        assert!(get(SettingKey::DpiStage3).blocked.is_none());
        assert!(
            get(SettingKey::DpiStage4).blocked.is_some(),
            "the mouse has three stages"
        );
        assert_eq!(get(SettingKey::DpiStage1).max, Some(30_000.0));
        assert!(get(SettingKey::LiftOffDistance).blocked.is_none());
        assert!(get(SettingKey::AngleSnapping).blocked.is_none());
        let s = m.snapshot().settings;
        assert_eq!(s.get("dpi_stage_2"), Some(&SettingValue::Int(1600)));
        assert_eq!(s.get("lift_off_distance"), Some(&SettingValue::Int(1)));
        assert_eq!(s.get("debounce"), Some(&SettingValue::Int(4)));
        assert_eq!(s.get("angle_snapping"), Some(&SettingValue::Bool(false)));
    }

    #[test]
    fn debounce_is_the_vendors_discrete_set() {
        let m = mouse(COMPX, 0xf502, Proto::V1, None);
        let vals: Vec<_> = m
            .debounce_options()
            .iter()
            .map(|o| o.value.as_int().unwrap())
            .collect();
        assert_eq!(vals, [0, 1, 2, 4, 8, 15, 20]);
        // A model that refuses 0 ms (ATK `noZeroKeyDebounce`) drops it; a
        // stored value outside the set stays selectable.
        let mut nz = mouse(COMPX, 0xf50f, Proto::V1, None);
        nz.debounce = Some(3);
        let vals: Vec<_> = nz
            .debounce_options()
            .iter()
            .map(|o| o.value.as_int().unwrap())
            .collect();
        assert_eq!(vals, [1, 2, 3, 4, 8, 15, 20]);
        let c = m.controls();
        let meta = c.iter().find(|c| c.key == SettingKey::Debounce).unwrap();
        assert!(meta.validate(&SettingValue::Int(3)).is_err());
        assert!(meta.validate(&SettingValue::Int(8)).is_ok());
    }

    #[test]
    fn pixart_3955_keeps_dpi_and_lift_off_blocked() {
        // VGN Dragonfly 3 Master GT.
        let m = mouse(VGN, 0x101d, Proto::V1, Some([0x01, 4, 0]));
        assert_eq!(m.sensor, Some(Sensor::Paw3955));
        let c = m.controls();
        for k in [SettingKey::DpiStage1, SettingKey::LiftOffDistance] {
            assert!(c.iter().find(|m| m.key == k).unwrap().blocked.is_some());
        }
    }

    #[test]
    fn battery_goes_absent_when_reads_stop_or_the_mouse_sleeps() {
        let t0 = Instant::now();
        assert!(fresh(Some(t0), t0 + REFRESH));
        assert!(!fresh(Some(t0), t0 + BATTERY_STALE));
        assert!(!fresh(None, t0));
        let mut m = mouse(COMPX, 0xf58a, Proto::V1, None);
        assert!(m.snapshot().batteries[0].present);
        m.online = Some(false);
        let b = &m.snapshot().batteries[0];
        assert!(!b.present, "asleep: shown at once");
        assert_eq!(b.percent, Some(50), "the last level is kept");
    }

    #[test]
    fn blocked_settings_are_refused_before_any_io() {
        let mut m = mouse(COMPX, 0xf50f, Proto::V1, None);
        assert!(m.controls()[0].blocked.is_some(), "wired Lamzu rate");
        let e = m
            .set(SettingKey::ReportRate, SettingValue::Int(500))
            .unwrap_err();
        assert!(e.to_string().contains("lamzu-cfg #1"), "{e}");
        // Active stage reaches the device (here: not connected).
        let e = m
            .set(SettingKey::DpiStageActive, SettingValue::Int(1))
            .unwrap_err();
        assert!(e.to_string().contains("not connected"), "{e}");
        let e = m
            .set(SettingKey::DpiStage1, SettingValue::Int(800))
            .unwrap_err();
        assert!(e.to_string().contains("sensor"), "{e}");
        let mut m = mouse(ATK, 0x11dc, Proto::V2(V2_REMOTE), None);
        assert!(m
            .controls()
            .iter()
            .all(|c| c.persistent && c.blocked.is_some()));
        let e = m
            .set(SettingKey::ReportRate, SettingValue::Int(1000))
            .unwrap_err();
        assert!(e.to_string().contains("0xFF05"), "{e}");
    }

    #[test]
    fn debounce_and_stage_values_are_checked_before_io() {
        let mut m = mouse(COMPX, 0xf502, Proto::V1, Some([0x01, 3, 0]));
        // Not connected, but the value check comes first for a bad value only
        // once a device exists; here both reach "not connected".
        let e = m
            .set(SettingKey::Debounce, SettingValue::Int(3))
            .unwrap_err();
        assert!(e.to_string().contains("not connected"), "{e}");
        let e = m
            .set(SettingKey::DpiStage4, SettingValue::Int(800))
            .unwrap_err();
        assert!(e.to_string().contains("not available"), "{e}");
    }
}
