//! MSI gaming peripherals (VID 0x0db0): Vigor GK30 keyboard and Clutch GM11
//! mouse, listed so they are recognised by name and get host-side remaps.
//! Their lighting is shown but blocked, and nothing is ever written to them.
//!
//! OpenRGB is the only public source for either protocol, and it marks every
//! lighting mode of both "automatic save" (the device stores each change):
//! - GK30 (`Controllers/MSIVigorController/MSIVigorGK30Controller.{h,cpp}`):
//!   interface 1, usage page 0xFF01, 8-byte feature report
//!   `07 ff mode colour-index direction speed brightness 00`.
//! - GM11 (`Controllers/MSIClutchController/MSIClutchGM11Controller/
//!   MSIClutchGM11Controller.{h,cpp}`): interface 2, usage page 0xFF00,
//!   8-byte feature reports whose last byte is `0xff - sum`, preceded by the
//!   handshake `03 03 0f f2 40`.
//!
//! No second project, capture set or vendor note confirms either layout, so
//! the rubric in `docs/onboard-writes/msi.md` blocks them. IDs from
//! `MSIVigorControllerDetect.cpp` and `MSIClutchControllerDetect.cpp`
//! (GPL-2.0, IDs only): <https://gitlab.com/CalcProgrammer1/OpenRGB>.

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::find_hidraw;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};

const VID: u16 = 0x0db0;

const BLOCKED: &str = "every change is saved by the device and only OpenRGB documents the \
                       protocol";

pub const MODELS: &[Model] = &[
    Model {
        vendor_id: VID,
        product_ids: &[0x0b30],
        kind: DeviceKind::Keyboard,
        name: "MSI Vigor GK30",
        usbfs: false,
        multi: false,
        open,
    },
    Model {
        vendor_id: VID,
        product_ids: &[0x0d11],
        kind: DeviceKind::Mouse,
        name: "MSI Clutch GM11",
        usbfs: false,
        multi: false,
        open,
    },
];

/// OpenRGB's modes and ranges for each device.
struct Lighting {
    iface: u32,
    effects: &'static [(&'static str, &'static str)],
    brightness: (f64, f64),
}

fn lighting(pid: u16) -> Option<Lighting> {
    Some(match pid {
        0x0b30 => Lighting {
            iface: 1,
            effects: &[
                ("static", "Static"),
                ("breathing", "Breathing"),
                ("rainbow", "Rainbow"),
                ("meteor", "Meteor"),
                ("ripple", "Ripple"),
                ("dimming", "Dimming"),
                ("off", "Off"),
            ],
            brightness: (1.0, 3.0),
        },
        0x0d11 => Lighting {
            iface: 2,
            effects: &[
                ("static", "Static"),
                ("breathing", "Breathing"),
                ("wave", "Wave"),
                ("radar", "Radar"),
                ("horizon", "Horizon"),
                ("ripple", "Ripple"),
                ("reactive", "Reactive"),
                ("off", "Off"),
            ],
            brightness: (0.0, 255.0),
        },
        _ => return None,
    })
}

fn controls(l: &Lighting) -> Vec<SettingMeta> {
    use SettingKey as K;
    let effects = l
        .effects
        .iter()
        .map(|(v, n)| SettingOption::text(v, n))
        .collect();
    let mut c = vec![
        SettingMeta::dropdown(K::Effect, "Effect", effects),
        SettingMeta::slider(
            K::Brightness,
            "Brightness",
            l.brightness.0,
            l.brightness.1,
            1.0,
        ),
        SettingMeta::dropdown(
            K::Speed,
            "Speed",
            vec![
                SettingOption::text("slow", "Slow"),
                SettingOption::text("medium", "Medium"),
                SettingOption::text("fast", "Fast"),
            ],
        ),
    ];
    for (key, label) in [
        (K::ColorR, "Red"),
        (K::ColorG, "Green"),
        (K::ColorB, "Blue"),
    ] {
        c.push(SettingMeta::color(key, label));
    }
    c.into_iter()
        .map(|m| m.persistent().blocked(BLOCKED))
        .collect()
}

/// Reads sysfs only: the device is never opened.
fn open(pid: u16, _ctx: &DriverCtx) -> Opened {
    let model = MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .with_context(|| format!("no MSI model for {pid:04x}"))?;
    let l = lighting(pid).with_context(|| format!("no lighting layout for {pid:04x}"))?;
    let info = find_hidraw(VID, pid, Some(l.iface)).or_else(|_| find_hidraw(VID, pid, None))?;
    Ok(vec![Box::new(Device {
        name: model.name,
        kind: model.kind,
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        controls: controls(&l),
    })])
}

struct Device {
    name: &'static str,
    kind: DeviceKind,
    id: DeviceId,
    controls: Vec<SettingMeta>,
}

impl DeviceAdapter for Device {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.into(),
            connected: true,
            batteries: vec![],
            settings: SettingsMap::new(),
            controls: self.controls.clone(),
            last_error: None,
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("{} {} is not available: {BLOCKED}", self.name, key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_has_blocked_memory_controls() {
        for m in MODELS {
            assert_eq!(m.vendor_id, VID);
            let pid = m.product_ids[0];
            let l = lighting(pid).unwrap_or_else(|| panic!("{pid:04x} has no layout"));
            let mut d = Device {
                name: m.name,
                kind: m.kind,
                id: DeviceId {
                    vendor_id: VID,
                    product_id: pid,
                    instance: "x".into(),
                },
                controls: controls(&l),
            };
            let snap = d.snapshot();
            assert_eq!(snap.kind, m.kind);
            for c in &snap.controls {
                assert!(c.persistent, "{} {:?}", m.name, c.key);
                assert_eq!(c.blocked.as_deref(), Some(BLOCKED));
                assert!(c.validate(&SettingValue::Int(1)).is_err());
            }
            assert!(d.set(SettingKey::Brightness, SettingValue::Int(1)).is_err());
        }
        assert_eq!(lighting(0x0b30).unwrap().iface, 1);
        assert_eq!(lighting(0x0d11).unwrap().iface, 2);
        assert!(lighting(0x0001).is_none());
    }
}
