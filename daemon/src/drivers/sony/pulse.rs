//! Sony PULSE 3D (CFI-ZWH1) dongle: battery only. The dongle answers no
//! queries; it sends an 8-byte status report `b0 .. .. <level> <flags> ..` when
//! something changes: level 0-100 (steps of 10) or 0x80 while charging, flag
//! 0x08 while the headset is on (HeadsetControl `sony_pulse_3d.hpp`).

use super::open_node;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{bail, Result};
use hako_core::catalog::{SettingKey, SettingValue};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::Duration;
use tracing::info;

const STATUS: u8 = 0xb0;

pub(super) struct Spec {
    pub name: &'static str,
    pub pids: &'static [u16],
}

pub(super) const PULSE_3D: Spec = Spec {
    name: "Sony PULSE 3D",
    pids: &[0x0d5e],
};

/// Fold a status report into `b`; false if `r` is not one.
fn apply(b: &mut BatteryState, r: &[u8]) -> bool {
    let [STATUS, _, _, level, flags, _, _, _, ..] = *r else {
        return false;
    };
    b.present = flags & 0x08 != 0;
    match level {
        0x80 => b.charging = true,
        0..=100 => (b.charging, b.percent) = (false, Some(level)),
        _ => {}
    }
    true
}

pub(super) fn open(spec: &'static Spec, pid: u16, _ctx: &DriverCtx) -> Opened {
    // Interface 3; the status collection is the one declaring report 0xb0.
    let (dev, instance) = open_node(pid, |d| crate::hid::report_ids(d).contains(&STATUS))?;
    info!("{} open via {}", spec.name, dev.path.display());
    Ok(vec![Box::new(PulseAdapter {
        spec,
        id: DeviceId {
            vendor_id: super::SONY,
            product_id: pid,
            instance,
        },
        dev: Some(dev),
        battery: BatteryState {
            label: "Headset".into(),
            percent: None,
            charging: false,
            present: false,
        },
    })])
}

struct PulseAdapter {
    spec: &'static Spec,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    battery: BatteryState,
}

impl DeviceAdapter for PulseAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: Default::default(),
            controls: vec![],
            last_error: None,
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("unsupported setting {}", key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        let mut buf = [0u8; 64];
        for _ in 0..64 {
            let Ok(n) = dev.read_timeout(&mut buf, Duration::ZERO) else {
                break;
            };
            apply(&mut self.battery, &buf[..n]);
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

    #[test]
    fn status_reports_update_the_battery() {
        let mut b = BatteryState {
            label: "Headset".into(),
            percent: None,
            charging: false,
            present: false,
        };
        assert!(apply(&mut b, &[0xb0, 0, 0, 70, 0x08, 0, 0, 0]));
        assert_eq!((b.percent, b.charging, b.present), (Some(70), false, true));
        // Charging keeps the last level.
        assert!(apply(&mut b, &[0xb0, 0, 0, 0x80, 0x08, 0, 0, 0]));
        assert_eq!((b.percent, b.charging), (Some(70), true));
        assert!(apply(&mut b, &[0xb0, 0, 0, 0x80, 0x00, 0, 0, 0]));
        assert!(!b.present);
        // Media-key reports and short reads are ignored.
        assert!(!apply(&mut b, &[0x01, 0, 0, 50, 0x08, 0, 0, 0]));
        assert!(!apply(&mut b, &[0xb0, 0, 0, 50]));
        assert_eq!(b.percent, Some(70));
    }
}
