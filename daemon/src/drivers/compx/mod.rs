//! Mice on Compx / ATK chipsets: ATK, VXE, VGN and Lamzu. Battery and
//! charging state on every model; report rate and the active DPI stage on the
//! 16-byte ("COMPX", v1) protocol. Those two are EEPROM writes (every setting
//! of these mice lives on the mouse), so they are `persistent`; everything
//! else is a blocked placeholder. Research: `docs/onboard-writes/compx.md`.
//!
//! Two protocols, both on vendor report 8 and told apart by the report's
//! size (DPIdeck `find_nodes`; OpenATK says the family follows the hardware
//! revision, not the model): 16 bytes = v1 EEPROM read / write, 63 bytes =
//! v2 command frames (read here for battery only). Layouts are reimplemented
//! from, and agree across:
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
//!
//! Only commands 0x04 (battery), 0x07 / 0x08 (EEPROM write / read) and v2
//! 0x07 / 0x0f (battery / charging) are ever sent. Never 0x0d: it erases the
//! firmware (lamzu-cfg #8, OpenATK).

mod protocol;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{descriptor_items, enumerate_hidraw, HidrawDevice};
use anyhow::{bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::merge_setting;
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use protocol::{Row, ADDR_ACTIVE, ADDR_RATE, RATES, REPORT_ID, V2_LOCAL, V2_REMOTE};
use std::time::{Duration, Instant};
use tracing::info;

/// Compx (Lamzu, VXE, VGN) and ATK / MADLIONS vendor IDs.
const COMPX: u16 = 0x3554;
const ATK: u16 = 0x373b;

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

/// Cable PIDs first: with the cable in, the receiver gets no answers
/// (lamzu-cfg #8).
pub const MODELS: &[Model] = &[
    // lamzu-cfg device.rs and issue #1, lamzu-ctl README.
    model(COMPX, "Lamzu Atlantis / Maya", &[0xf50f, 0xf510, 0xf50d]),
    // atk-control docs/PROTOCOL.md (hardware-tested receivers).
    model(COMPX, "VXE mouse (VXE 1K receiver)", &[0xf58a]),
    model(COMPX, "VGN / VXE mouse (Dragonfly 4K receiver)", &[0xf505]),
    model(ATK, "VXE / ATK mouse (MAD 8K receiver)", &[0x1040]),
    // atk-control README and issue #6.
    model(ATK, "ATK Dragonfly A9 Pro Max 2.0", &[0x1264, 0x11dc]),
];

/// The link's top report rate and whether it is a receiver (lamzu-cfg
/// `max_poll_rate`, atk-control `RECEIVER_CEILINGS`).
fn port(vid: u16, pid: u16) -> (i64, bool) {
    match (vid, pid) {
        (COMPX, 0xf510) | (COMPX, 0xf505) => (4000, true),
        (COMPX, 0xf50d) | (COMPX, 0xf58a) => (1000, true),
        (ATK, 0x1040) | (ATK, 0x11dc) => (8000, true),
        _ => (1000, false),
    }
}

/// Why the report rate is not offered on this link, if it is not.
fn rate_block(vid: u16, pid: u16) -> Option<&'static str> {
    (vid == COMPX && pid == 0xf50f).then_some(
        "over the cable this address reads back the wireless rate and the cable stays at \
         1000 Hz, so what a write changes is unclear (lamzu-cfg #1)",
    )
}

const DPI_BLOCKED: &str = "the DPI encoding depends on the sensor (50- or 10-DPI steps, \
per-sensor tables), which the USB ID does not tell; a wrong guess stores another DPI";
const LOD_BLOCKED: &str = "lift-off codes depend on the sensor (1 / 2 mm on most, 0.7 mm \
or 0.1 mm steps on others)";
const DEBOUNCE_BLOCKED: &str = "allowed values differ per model (some refuse 0 ms) and \
between sources (up to 15 or 20 ms)";
const V2_BLOCKED: &str = "no source has tested settings writes on the 64-byte (0xFF05) \
protocol on hardware";

const STAGES: [SettingKey; 5] = [
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
    let model = MODELS
        .iter()
        .find(|m| m.vendor_id == vid && m.product_ids.contains(&pid))
        .with_context(|| format!("no Compx model for {vid:04x}:{pid:04x}"))?;
    let (max_rate, receiver) = port(vid, pid);
    let (info, proto) = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .find_map(|d| {
            let p = proto_of(report_len(&d.report_descriptor(), REPORT_ID)?, receiver)?;
            Some((d, p))
        })
        .with_context(|| format!("{vid:04x}:{pid:04x} has no Compx configuration interface"))?;
    let dev = HidrawDevice::open(&info.path)?;
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
        max_rate,
        rate_block: rate_block(vid, pid),
        row: None,
        battery: None,
        last_read: None,
        last_error: None,
    };
    m.refresh();
    Ok(vec![Box::new(m)])
}

const REFRESH: Duration = Duration::from_secs(60);

struct Mouse {
    name: &'static str,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    proto: Proto,
    max_rate: i64,
    rate_block: Option<&'static str>,
    /// Last rate row read (v1).
    row: Option<Row>,
    /// (percent, charging), kept while the mouse sleeps.
    battery: Option<(u8, bool)>,
    last_read: Option<Instant>,
    last_error: Option<String>,
}

impl Mouse {
    fn refresh(&mut self) {
        self.last_read = Some(Instant::now());
        let Some(dev) = self.dev.as_mut() else {
            return;
        };
        let proto = self.proto;
        let read = (|| -> Result<()> {
            self.battery = Some(match proto {
                Proto::V1 => protocol::read_battery(dev)?,
                Proto::V2(target) => protocol::read_v2_battery(dev, target)?,
            });
            if proto == Proto::V1 {
                self.row = Some(protocol::read_row(dev)?);
            }
            Ok(())
        })();
        self.last_error = read.err().map(|e| e.to_string());
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
        s
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        let current = self.row.and_then(|r| protocol::rate_hz(r[0]));
        let rates = RATES
            .iter()
            .filter(|&&(hz, _)| hz <= self.max_rate || Some(hz) == current)
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
        for (i, key) in STAGES.into_iter().enumerate() {
            let label = format!("DPI stage {}", i + 1);
            c.push(
                SettingMeta::slider(key, &label, 50.0, 26000.0, 50.0)
                    .unit("dpi")
                    .persistent()
                    .blocked(DPI_BLOCKED),
            );
        }
        c.push(
            SettingMeta::dropdown(
                LiftOffDistance,
                "Lift-off distance",
                vec![SettingOption::int(1, "1 mm"), SettingOption::int(2, "2 mm")],
            )
            .persistent()
            .blocked(LOD_BLOCKED),
        );
        c.push(
            SettingMeta::slider(Debounce, "Click debounce", 0.0, 20.0, 1.0)
                .unit("ms")
                .persistent()
                .blocked(DEBOUNCE_BLOCKED),
        );
        if let Proto::V2(_) = self.proto {
            c = c.into_iter().map(|m| m.blocked(V2_BLOCKED)).collect();
        }
        c
    }

    fn write(&mut self, key: SettingKey, value: &SettingValue) -> Result<Row> {
        if let Some(reason) = self
            .controls()
            .into_iter()
            .find(|m| m.key == key)
            .and_then(|m| m.blocked)
        {
            bail!("{} is not available: {reason}", key.as_str());
        }
        let v = value.as_int().context("expected a number")?;
        let (addr, code) = match key {
            SettingKey::ReportRate => (
                ADDR_RATE,
                RATES
                    .iter()
                    .find(|&&(hz, _)| hz == v)
                    .with_context(|| format!("{v} Hz is not a report rate"))?
                    .1,
            ),
            SettingKey::DpiStageActive => {
                ensure!((1..=8).contains(&v), "stage {v} is out of range");
                (ADDR_ACTIVE, (v - 1) as u8)
            }
            _ => bail!("{} is not a {} setting", key.as_str(), self.name),
        };
        let dev = self.dev.as_mut().context("not connected")?;
        protocol::write_pair(dev, addr, code, |row| {
            ensure!(
                addr != ADDR_ACTIVE || code < row[1],
                "the mouse has {} DPI stages",
                row[1]
            );
            Ok(())
        })
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
                    present: true,
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
            Ok(row) => {
                self.row = Some(row);
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
        if self.dev.is_some() && self.last_read.is_none_or(|t| t.elapsed() >= REFRESH) {
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
        Mouse {
            name: "test",
            id: DeviceId {
                vendor_id: vid,
                product_id: pid,
                instance: "x".into(),
            },
            dev: None,
            proto,
            max_rate: port(vid, pid).0,
            rate_block: rate_block(vid, pid),
            row,
            battery: Some((50, false)),
            last_read: None,
            last_error: None,
        }
    }

    #[test]
    fn models_are_unique_and_every_link_is_known() {
        let mut seen = HashSet::new();
        for m in MODELS {
            assert!([COMPX, ATK].contains(&m.vendor_id));
            for &pid in m.product_ids {
                assert!(seen.insert((m.vendor_id, pid)), "{pid:04x} listed twice");
            }
        }
        // Pulsar's Nordic receivers share VID 0x3554; never claim them.
        for pid in [0xf507, 0xf508, 0xf509] {
            assert!(!seen.contains(&(COMPX, pid)));
        }
        assert_eq!(port(COMPX, 0xf510), (4000, true));
        assert_eq!(port(COMPX, 0xf50f), (1000, false));
        assert_eq!(port(ATK, 0x1040), (8000, true));
        assert_eq!(port(ATK, 0x1264), (1000, false));
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
    fn v1_offers_rate_and_active_stage_as_memory_writes() {
        let m = mouse(COMPX, 0xf510, Proto::V1, Some([0x40, 3, 1]));
        let c = m.controls();
        for meta in &c {
            assert!(meta.persistent, "{:?}", meta.key);
            let offered = [SettingKey::ReportRate, SettingKey::DpiStageActive].contains(&meta.key);
            assert_eq!(meta.blocked.is_none(), offered, "{:?}", meta.key);
        }
        let rate = &c[0];
        // Up to the receiver's 4000 Hz, plus the stored 8000 Hz.
        assert!(rate.validate(&SettingValue::Int(4000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(8000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(2000)).is_ok());
        assert!(rate.validate(&SettingValue::Int(300)).is_err());
        let active = &c[1];
        assert!(active.validate(&SettingValue::Int(3)).is_ok());
        assert!(active.validate(&SettingValue::Int(4)).is_err());
        let s = m.snapshot();
        assert_eq!(
            s.settings.get("report_rate"),
            Some(&SettingValue::Int(8000))
        );
        assert_eq!(
            s.settings.get("dpi_stage_active"),
            Some(&SettingValue::Int(2))
        );
        assert_eq!(s.batteries[0].percent, Some(50));
        // 1K receiver: nothing above 1000 Hz.
        let m = mouse(COMPX, 0xf58a, Proto::V1, Some([0x01, 4, 0]));
        assert!(m.controls()[0].validate(&SettingValue::Int(2000)).is_err());
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
}
