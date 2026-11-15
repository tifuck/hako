//! ASUS ROG earbuds / headsets on the newer report-0xCC protocol (events as
//! `22 xx`, reads `12 xx`): ROG Cetra Open Wireless, Cetra True Wireless II
//! OLED and Delta II X. Link state and battery only.
//!
//! Source: ASUS Gear Link (the Audio Companion service's per-device `.ini`
//! files: usage page 0xFF00, report 0xCC, no `DeprecatedProtocol=703`; its
//! `DeviceProtocolCtrl` reads and the Cetra Open protocol table in the web
//! app): `12 01` link status (byte 4), `12 12` battery from byte 4 (left,
//! right, case; 0xFE while unknown), `12 11` charging (byte 4 bit 0 for the
//! buds, byte 5 bit 0 for the case). The same table lists lighting, sleep
//! timer, voice prompt, ANC and equaliser writes (`51 xx`), but not the value
//! sets those need on these models, so nothing is written.

use super::protocol::{self as p, Transport, VID};
use crate::drivers::DeviceAdapter;
use anyhow::{Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue, SettingsMap};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const REPORT_ID: u8 = 0xcc;
const POLL: Duration = Duration::from_secs(30);
/// While the buds are off or unlinked, look for them more often.
const RELINK_POLL: Duration = Duration::from_secs(5);

/// `12 01`: linked to the dongle (byte 4 != 0).
const LINK: &[u8] = &[0x12, 0x01];
/// `12 12`: battery percent per cell from byte 4.
const BATTERY: &[u8] = &[0x12, 0x12];
/// `12 11`: charging flags from byte 4.
const CHARGING: &[u8] = &[0x12, 0x11];

pub struct EarbudsSpec {
    pub name: &'static str,
    pub pids: &'static [u16],
    /// One label per battery cell, in reply order.
    pub cells: &'static [&'static str],
}

const BUDS: &[&str] = &["Left", "Right", "Case"];

pub const EARBUDS: &[EarbudsSpec] = &[
    EarbudsSpec {
        name: "ASUS ROG Cetra Open Wireless",
        pids: &[0x1cac],
        cells: BUDS,
    },
    EarbudsSpec {
        name: "ASUS ROG Cetra True Wireless II OLED",
        pids: &[0x1d18],
        cells: BUDS,
    },
    EarbudsSpec {
        name: "ASUS ROG Delta II X",
        pids: &[0x1de5],
        cells: &["Headset"],
    },
];

pub fn find(pid: u16) -> Option<&'static EarbudsSpec> {
    EARBUDS.iter().find(|e| e.pids.contains(&pid))
}

/// Battery percent per cell from a `12 12` reply; `None` for 0xFE / out of range.
pub fn parse_battery(resp: &p::Packet, cells: usize) -> Vec<Option<u8>> {
    resp[4..4 + cells]
        .iter()
        .map(|&b| (b <= 100).then_some(b))
        .collect()
}

/// Charging per cell from a `12 11` reply: the buds share byte 4, the case is byte 5.
pub fn parse_charging(resp: &p::Packet, cells: usize) -> Vec<bool> {
    (0..cells)
        .map(|i| resp[if cells > 1 && i == cells - 1 { 5 } else { 4 }] & 1 == 1)
        .collect()
}

pub struct EarbudsAdapter {
    spec: &'static EarbudsSpec,
    id: DeviceId,
    t: Option<Transport>,
    batteries: Vec<BatteryState>,
    online: bool,
    last_poll: Instant,
    last_error: Option<String>,
}

impl EarbudsAdapter {
    pub fn open(spec: &'static EarbudsSpec, pid: u16) -> Result<Self> {
        let (t, path) = Transport::open_page(pid, 0xff00, REPORT_ID)?;
        let mut a = Self {
            spec,
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: path.clone(),
            },
            t: Some(t),
            batteries: spec
                .cells
                .iter()
                .map(|&label| BatteryState {
                    label: label.into(),
                    percent: None,
                    charging: false,
                    present: false,
                })
                .collect(),
            online: false,
            last_poll: Instant::now(),
            last_error: None,
        };
        a.poll();
        info!("{} open via {path} online={}", spec.name, a.online);
        Ok(a)
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        match self.read() {
            Ok(true) => {
                self.online = true;
                self.last_error = None;
            }
            Ok(false) => {
                self.online = false;
                self.last_error = Some("earbuds not linked to the dongle".into());
            }
            Err(e) => {
                self.online = false;
                self.last_error = Some(e.to_string());
            }
        }
        for b in &mut self.batteries {
            b.present = self.online;
            if !self.online {
                b.percent = None;
                b.charging = false;
            }
        }
    }

    /// Whether the buds are linked; fills the battery cells when they are.
    fn read(&mut self) -> Result<bool> {
        let cells = self.spec.cells.len();
        let t = self.t.as_mut().context("device closed")?;
        if t.transact(LINK)?[4] == 0 {
            return Ok(false);
        }
        let level = parse_battery(&t.transact(BATTERY)?, cells);
        let charging = parse_charging(&t.transact(CHARGING)?, cells);
        for ((b, level), charging) in self.batteries.iter_mut().zip(level).zip(charging) {
            b.percent = level;
            b.charging = charging;
        }
        Ok(true)
    }
}

impl DeviceAdapter for EarbudsAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.t.is_some(),
            batteries: self.batteries.clone(),
            settings: SettingsMap::new(),
            controls: Vec::<SettingMeta>::new(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        anyhow::bail!("{} has no {} control", self.spec.name, key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        let every = if self.online { POLL } else { RELINK_POLL };
        if self.last_poll.elapsed() >= every {
            self.poll();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.t = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_and_charging_layouts() {
        let r = p::packet(&[0x12, 0x12, 0, 0, 80, 0xfe, 55]);
        assert_eq!(parse_battery(&r, 3), [Some(80), None, Some(55)]);
        assert_eq!(parse_battery(&r, 1), [Some(80)]);
        // Buds charging, case not.
        let c = p::packet(&[0x12, 0x11, 0, 0, 1, 0]);
        assert_eq!(parse_charging(&c, 3), [true, true, false]);
        assert_eq!(parse_charging(&c, 1), [true]);
        // Case only.
        let c = p::packet(&[0x12, 0x11, 0, 0, 0, 1]);
        assert_eq!(parse_charging(&c, 3), [false, false, true]);
    }

    #[test]
    fn table_is_consistent() {
        for e in EARBUDS {
            assert_eq!(find(e.pids[0]).map(|s| s.name), Some(e.name));
            assert!(matches!(e.cells.len(), 1 | 3));
        }
    }
}
