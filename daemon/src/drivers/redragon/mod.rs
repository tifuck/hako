//! Redragon mice on the Holtek USB controller (VID 0x04d9): listed so they are
//! recognised by name and get host-side remaps. Their lighting is shown but
//! blocked, and nothing is ever written to them.
//!
//! These mice have no volatile lighting command: OpenRGB's
//! `RedragonMouseController` writes device memory directly (feature report
//! 0x02, `f3 <addr lo> <addr hi> <len>` with data at byte 8, colour / mode at
//! 0x0449, active profile at 0x002c, then `f1 02 04`), and marks every mode
//! "automatic save". mouse_m908 (`include/m908|m711|m715/data.cpp`) uses the
//! same framing and agrees on the colour / mode record for M711, M715 and
//! M908, but stores it per onboard profile (0x0449 + 8 × profile), encodes
//! wave differently, and its README / issues report settings writes that
//! left sibling models unusable. See `docs/onboard-writes/redragon.md`.
//!
//! PIDs and names: OpenRGB `Controllers/RedragonController/
//! RedragonControllerDetect.cpp` (GPL-2.0, IDs only); mouse_m908 README
//! (GPL-3.0, IDs only) where it names the same PID differently.
//! <https://gitlab.com/CalcProgrammer1/OpenRGB>,
//! <https://github.com/dokutan/mouse_m908>

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::find_hidraw;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};

const VID: u16 = 0x04d9;

/// Interface of the vendor collection (usage page 0xFFA0).
const IFACE: u32 = 2;

const fn model(name: &'static str, product_ids: &'static [u16]) -> Model {
    Model {
        vendor_id: VID,
        product_ids,
        kind: DeviceKind::Mouse,
        name,
        usbfs: false,
        multi: false,
        open,
    }
}

pub const MODELS: &[Model] = &[
    model("Redragon M711 Cobra", &[0xfc30]),
    model("Redragon M908 Impact", &[0xfc4d]),
    model("Redragon M715 Dagger", &[0xfc39]),
    model("Redragon M716 Inquisitor", &[0xfc3a]),
    model("Redragon M602 / M607 Griffin", &[0xfc38]),
    model("Redragon M808 Storm", &[0xfc5f]),
    model("Redragon M801 Sniper", &[0xfc58]),
    model("Redragon M810 Taipan", &[0xfa7e]),
    model("Redragon M987 Reaping", &[0xfc69]),
    model("Redragon M921 Azzinoth", &[0xfc40]),
    model("Redragon M711-FPS-1 Cobra FPS", &[0xfc62]),
    model("Redragon M901-K-2 Perdition", &[0xfc5e]),
];

/// Why lighting is not offered for `pid`.
fn blocked_reason(pid: u16) -> &'static str {
    match pid {
        // OpenRGB and mouse_m908 both cover these PIDs.
        0xfc30 | 0xfc39 | 0xfc4d => {
            "every change is written to onboard profile memory; sources disagree on the \
             profile slot and the wave code"
        }
        _ => {
            "every change is written to onboard memory; only OpenRGB covers this model and \
             the layout differs between Redragon models"
        }
    }
}

/// OpenRGB's mode list for these mice.
const EFFECTS: [(&str, &str); 5] = [
    ("static", "Static"),
    ("wave", "Wave"),
    ("breathing", "Breathing"),
    ("rainbow", "Rainbow"),
    ("flashing", "Flashing"),
];

fn controls(pid: u16) -> Vec<SettingMeta> {
    let reason = blocked_reason(pid);
    let effects = EFFECTS
        .iter()
        .map(|(v, l)| SettingOption::text(v, l))
        .collect();
    let mut c = vec![SettingMeta::dropdown(SettingKey::Effect, "Effect", effects)];
    for (key, label) in [
        (SettingKey::ColorR, "Red"),
        (SettingKey::ColorG, "Green"),
        (SettingKey::ColorB, "Blue"),
    ] {
        c.push(SettingMeta::color(key, label));
    }
    c.into_iter()
        .map(|m| m.persistent().blocked(reason))
        .collect()
}

/// Reads sysfs only: the mouse is never opened.
fn open(pid: u16, _ctx: &DriverCtx) -> Opened {
    let model = MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .with_context(|| format!("no Redragon model for {pid:04x}"))?;
    let info = find_hidraw(VID, pid, Some(IFACE)).or_else(|_| find_hidraw(VID, pid, None))?;
    Ok(vec![Box::new(Mouse {
        name: model.name,
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
    })])
}

struct Mouse {
    name: &'static str,
    id: DeviceId,
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
            connected: true,
            batteries: vec![],
            settings: SettingsMap::new(),
            controls: controls(self.id.product_id),
            last_error: None,
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!(
            "{} {} is not available: {}",
            self.name,
            key.as_str(),
            blocked_reason(self.id.product_id)
        )
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn models_are_unique_holtek_mice() {
        let mut seen = HashSet::new();
        for m in MODELS {
            assert_eq!(m.vendor_id, VID);
            assert_eq!(m.kind, DeviceKind::Mouse);
            for &pid in m.product_ids {
                assert!(seen.insert(pid), "{pid:04x} listed twice");
            }
        }
        assert_eq!(seen.len(), 12);
    }

    #[test]
    fn every_control_is_a_blocked_memory_write() {
        for m in MODELS {
            let pid = m.product_ids[0];
            let mut a = Mouse {
                name: m.name,
                id: DeviceId {
                    vendor_id: VID,
                    product_id: pid,
                    instance: "x".into(),
                },
            };
            let snap = a.snapshot();
            assert!(!snap.controls.is_empty());
            for c in &snap.controls {
                assert!(c.persistent, "{} {:?}", m.name, c.key);
                assert_eq!(c.blocked.as_deref(), Some(blocked_reason(pid)));
                assert!(c.validate(&SettingValue::Int(0)).is_err());
            }
            assert!(a
                .set(SettingKey::Effect, SettingValue::Text("static".into()))
                .is_err());
        }
        assert_ne!(blocked_reason(0xfc4d), blocked_reason(0xfa7e));
    }
}
