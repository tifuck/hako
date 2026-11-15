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
//! Redragon's own Holtek tool ("REDRAGON Gaming Mouse", `RDCfg.exe`, static
//! analysis, `docs/vendor-re/redragon.md`) confirms the framing (feature report
//! id 2, 16 bytes: `f3` write, `f1 02 01` commit with 100 ms wait, `f5` batch
//! markers) and gives the model list, with the bcdDevice range that tells the
//! hardware revisions of one PID apart. It addresses the LED records at 0x46
//! (mode) and 0x48-0x70 (per mode), not at 0x449, and its data layout is not
//! decoded, so no lighting write is offered for any model.
//!
//! PIDs and names: Redragon's tool (its `Mouse_List`), OpenRGB `Controllers/RedragonController/
//! RedragonControllerDetect.cpp` (GPL-2.0, IDs only); mouse_m908 README
//! (GPL-3.0, IDs only) where it names the same PID differently, and for the
//! 04d9 mice OpenRGB does not list (M719 onwards in `MODELS`; fc2a is also
//! "Redragon M709" in usb.ids).
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
    model("Redragon M711 Cobra / M711-FPS", &[0xfc30]),
    model("Redragon M908 Impact", &[0xfc4d]),
    model("Redragon M715 Dagger", &[0xfc39]),
    model("Redragon M716 Inquisitor", &[0xfc3a]),
    model("Redragon M602 / M607 Griffin", &[0xfc38]),
    model("Redragon M808 Storm / M998-RGB", &[0xfc5f]),
    model("Redragon M801 Sniper / M912-RGB", &[0xfc58]),
    // Redragon's tool also lists 25a7:fa7e (M810RGB-PRO, SinoWealth-class OEM
    // family): another chip, not listed. 04d9:fa7e is OpenRGB's only.
    model("Redragon M810 Taipan", &[0xfa7e]),
    model("Redragon M987 Reaping", &[0xfc69]),
    model("Redragon M901 Perdition / M921 Azzinoth", &[0xfc40]),
    model("Redragon M711 Cobra FPS (2710-G)", &[0xfc62]),
    model("Redragon M901-K-2 Perdition", &[0xfc5e]),
    model("Redragon M719 Invader", &[0xfc4f]),
    model("Redragon M709 Tiger", &[0xfc2a]),
    model("Redragon M721-Pro Lonewolf2", &[0xfc5c]),
    model("Redragon M990 Legend", &[0xfc0f, 0xfc41]),
    model("Redragon M702-2 Phoenix", &[0xfc2f]),
    model("Redragon M802 Titanoboa 2", &[0xfc42]),
    model("Redragon M612-RGB Predator", &[0xfc61]),
    model("Redragon M901 Perdition", &[0xfc02]),
    model("Redragon M910 Ranger", &[0xfc49]),
    model("Redragon M909-RGB Emperor", &[0xfc4c]),
    model("Redragon M907-RGB Inspirit 2", &[0xfc3c]),
    model("Redragon M701 Lavawolf", &[0xfc3e]),
    model("Redragon M902-RGB Samsara 2", &[0xfc3f]),
    model("Redragon M801 Mammoth", &[0xfc56]),
    model("Redragon M720-RGB Vampire", &[0xfc5d]),
    model("Redragon mouse 2732 (fc3b)", &[0xfc3b]),
    model("Redragon mouse 2866 (fc60)", &[0xfc60]),
    model("Redragon mouse 2890 (fc65)", &[0xfc65]),
    model("Redragon mouse 2825-A (fc66)", &[0xfc66]),
    model("Redragon mouse 2903 (fc6b)", &[0xfc6b]),
    model("Redragon mouse 2913-A (fc6d)", &[0xfc6d]),
    model("Redragon mouse 2928 (fc6e)", &[0xfc6e]),
    model("Redragon mouse 2713 (fc6f)", &[0xfc6f]),
    model("Redragon mouse 2931-A (fc70)", &[0xfc70]),
    model("Redragon G881-B / 2935 (fc71)", &[0xfc71]),
    model("Redragon mouse 2915 (fc75)", &[0xfc75]),
];

/// PIDs only mouse_m908 lists.
const M908_ONLY: [u16; 10] = [
    0xfc4f, 0xfc2a, 0xfc5c, 0xfc0f, 0xfc41, 0xfc2f, 0xfc42, 0xfc61, 0xfc02, 0xfc49,
];

/// PIDs only Redragon's own tool lists (`docs/vendor-re/redragon.md`).
const VENDOR_ONLY: [u16; 17] = [
    0xfc3b, 0xfc3c, 0xfc3e, 0xfc3f, 0xfc4c, 0xfc56, 0xfc5d, 0xfc60, 0xfc65, 0xfc66, 0xfc6b, 0xfc6d,
    0xfc6e, 0xfc6f, 0xfc70, 0xfc71, 0xfc75,
];

/// Why lighting is not offered for `pid`.
fn blocked_reason(pid: u16) -> &'static str {
    match pid {
        // OpenRGB and mouse_m908 both cover these PIDs.
        0xfc30 | 0xfc39 | 0xfc4d => {
            "every change is written to onboard profile memory; sources disagree on the \
             profile slot and the wave code, and Redragon's own tool uses a different record map"
        }
        p if VENDOR_ONLY.contains(&p) => {
            "every change is written to onboard memory; only Redragon's own tool lists this \
             model and its record layout is not decoded"
        }
        p if M908_ONLY.contains(&p) => {
            "every change is written to onboard memory; only mouse_m908 covers this model and \
             the layout differs between Redragon models"
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
        assert_eq!(seen.len(), 39);
        assert!(VENDOR_ONLY.iter().all(|p| seen.contains(p)));
        assert!(M908_ONLY.iter().all(|p| seen.contains(p)));
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
        assert_ne!(blocked_reason(0xfc4f), blocked_reason(0xfa7e));
        assert_ne!(blocked_reason(0xfc4c), blocked_reason(0xfa7e));
    }
}
