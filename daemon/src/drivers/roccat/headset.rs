//! ROCCAT Elo 7.1 USB and Elo 7.1 Air headsets: LED color, and the Air's
//! auto-off timer.
//!
//! Layouts from HeadsetControl `roccat_elo_7_1_usb.hpp` / `roccat_elo_7_1_air.hpp`
//! and OpenRGB `RoccatEloController` (`@save :x:`, i.e. volatile). Lighting is
//! the output-report sequence `ff 02`, `ff 03 ..`, `ff 04 00 00 R G B`, `ff 01`
//! (16-byte reports on the USB model, 64-byte on the Air); "off" is black, as
//! HeadsetControl sends it. The headsets need a pause after every report
//! (40 ms USB, 75 ms Air). Neither source documents battery or sidetone
//! reports for these models.

use super::{nodes, open_iface, open_info, read_until, Adapter, Lights, Spec, ROCCAT};
use crate::drivers::{DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue};
use hako_core::device::DeviceKind;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// Wired: interface 3 (OpenRGB), 16-byte reports.
    Usb,
    /// Wireless: first interface (HeadsetControl's default), 64-byte reports.
    Air,
}

pub(super) const ELO_AIR: Spec<Family> = Spec {
    name: "ROCCAT Elo 7.1 Air",
    vid: ROCCAT,
    pids: &[0x3a37],
    family: Family::Air,
};
pub(super) const ELO_USB: Spec<Family> = Spec {
    name: "ROCCAT Elo 7.1 USB",
    vid: ROCCAT,
    pids: &[0x3a34],
    family: Family::Usb,
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[&ELO_AIR, &ELO_USB];

pub(super) const ELO_EFFECTS: &[(&str, &str)] = &[("static", "Static"), ("off", "Off")];

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, ctx: &DriverCtx) -> Opened {
    let (dev, instance) = match spec.family {
        Family::Usb => open_iface(spec.vid, pid, 3)?,
        Family::Air => open_info(
            nodes(spec.vid, pid)?
                .first()
                .with_context(|| format!("no hidraw node for {:04x}:{pid:04x}", spec.vid))?,
        )?,
    };
    let lights = Elo {
        dev,
        family: spec.family,
    };
    Adapter::open(
        spec,
        DeviceKind::Headset,
        pid,
        instance,
        Box::new(lights),
        ctx,
    )
}

impl Family {
    fn len(self) -> usize {
        match self {
            Family::Usb => 16,
            Family::Air => 64,
        }
    }

    fn pause(self) -> Duration {
        Duration::from_millis(match self {
            Family::Usb => 40,
            Family::Air => 75,
        })
    }
}

fn report(f: Family, bytes: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; f.len()];
    v[..bytes.len()].copy_from_slice(bytes);
    v
}

fn light_reports(f: Family, [r, g, b]: [u8; 3]) -> Vec<Vec<u8>> {
    let mode: &[u8] = match f {
        Family::Usb => &[0xff, 0x03, 0x00, 0x01],
        Family::Air => &[0xff, 0x03, 0x00, 0x01, 0x00, 0x01],
    };
    vec![
        report(f, &[0xff, 0x02]),
        report(f, mode),
        report(f, &[0xff, 0x04, 0x00, 0x00, r, g, b]),
        report(f, &[0xff, 0x01]),
    ]
}

/// Air auto-off: `a1 04 06 54` + seconds (big-endian u16), 0..=60 minutes.
fn inactive_report(minutes: u8) -> Vec<u8> {
    let [hi, lo] = (u16::from(minutes) * 60).to_be_bytes();
    report(Family::Air, &[0xa1, 0x04, 0x06, 0x54, hi, lo])
}

fn extra_controls(f: Family) -> Vec<SettingMeta> {
    match f {
        Family::Air => {
            vec![
                SettingMeta::slider(SettingKey::PmShutdown, "Auto-off", 0.0, 60.0, 1.0).unit("min"),
            ]
        }
        Family::Usb => Vec::new(),
    }
}

struct Elo {
    dev: HidrawDevice,
    family: Family,
}

impl Elo {
    fn send(&mut self, r: &[u8]) -> Result<()> {
        self.dev.write(r)?;
        std::thread::sleep(self.family.pause());
        Ok(())
    }
}

impl Lights for Elo {
    fn effects(&self) -> &'static [(&'static str, &'static str)] {
        ELO_EFFECTS
    }

    fn apply(&mut self, effect: &str, color: [u8; 3]) -> Result<()> {
        let color = if effect == "static" { color } else { [0; 3] };
        for r in light_reports(self.family, color) {
            self.send(&r)?;
        }
        Ok(())
    }

    fn extra_controls(&self) -> Vec<SettingMeta> {
        extra_controls(self.family)
    }

    fn set_extra(&mut self, key: SettingKey, value: &SettingValue) -> Result<()> {
        let minutes = match (self.family, key, value.as_int()) {
            (Family::Air, SettingKey::PmShutdown, Some(m @ 0..=60)) => m as u8,
            _ => bail!("{} is not supported", key.as_str()),
        };
        self.dev.write(&inactive_report(minutes))?;
        // HeadsetControl reads one reply before pausing.
        read_until(&mut self.dev, Duration::from_millis(500), |_| true);
        std::thread::sleep(self.family.pause());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_sequences() {
        let usb = light_reports(Family::Usb, [1, 2, 3]);
        assert!(usb.iter().all(|r| r.len() == 16));
        assert_eq!(usb[0][..2], [0xff, 0x02]);
        assert_eq!(usb[1][..4], [0xff, 0x03, 0x00, 0x01]);
        assert_eq!(usb[2][..8], [0xff, 0x04, 0, 0, 1, 2, 3, 0]);
        assert_eq!(usb[3][..3], [0xff, 0x01, 0]);
        let air = light_reports(Family::Air, [0xff; 3]);
        assert!(air.iter().all(|r| r.len() == 64));
        assert_eq!(air[1][..6], [0xff, 0x03, 0x00, 0x01, 0x00, 0x01]);
        assert_eq!(air[2][..7], [0xff, 0x04, 0, 0, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn inactive_time() {
        let r = inactive_report(60);
        assert_eq!(r.len(), 64);
        // 3600 s = 0x0e10.
        assert_eq!(r[..7], [0xa1, 0x04, 0x06, 0x54, 0x0e, 0x10, 0]);
        assert_eq!(inactive_report(0)[4..6], [0, 0]);
    }

    #[test]
    fn controls_validate() {
        let c = extra_controls(Family::Air);
        assert!(c[0].validate(&SettingValue::Int(60)).is_ok());
        assert!(c[0].validate(&SettingValue::Int(61)).is_err());
        assert!(extra_controls(Family::Usb).is_empty());
    }
}
