//! Any USB / Bluetooth mouse or keyboard no vendor driver claims.
//!
//! It gets what needs no vendor protocol: host-side remaps and macros (the
//! hub attaches every connected mouse / keyboard to the remap engine) and
//! the battery the kernel already reports under `power_supply`. Classified
//! from sysfs only: the USB boot protocol of the lowest interface, then the
//! product name, then the evdev capabilities of its input nodes.

use super::{DeviceAdapter, DriverCtx, Fallback, Opened};
use crate::hid::{enumerate_hidraw, HidDeviceInfo, BUS_BLUETOOTH, BUS_USB};
use anyhow::{bail, Result};
use hako_core::catalog::{SettingKey, SettingValue, SettingsMap};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Security keys enumerate as keyboards (OTP typing); never offer remaps.
const IGNORED_VENDORS: &[u16] = &[0x1050 /* Yubico */, 0x20a0 /* Nitrokey */];
const BATTERY_POLL: Duration = Duration::from_secs(30);

const KEY_ENTER: usize = 28;
const KEY_A: usize = 30;
const KEY_Z: usize = 44;
const KEY_SPACE: usize = 57;
const BTN_LEFT: usize = 0x110;
const REL_X: usize = 0;
const ABS_X: usize = 0;

pub fn fallback(nodes: &[&HidDeviceInfo]) -> Option<Fallback> {
    let (kind, name) = classify(nodes)?;
    Some(Fallback { kind, name, open })
}

fn classify(nodes: &[&HidDeviceInfo]) -> Option<(DeviceKind, String)> {
    let first = nodes.first()?;
    if IGNORED_VENDORS.contains(&first.vendor_id)
        || !nodes
            .iter()
            .all(|n| matches!(n.bus, BUS_USB | BUS_BLUETOOTH))
    {
        return None;
    }
    let caps: Vec<Caps> = nodes.iter().flat_map(|n| input_caps(&n.sys)).collect();
    let mouse = caps.iter().any(Caps::is_mouse);
    let keyboard = caps.iter().any(Caps::is_keyboard);
    if !mouse && !keyboard {
        return None;
    }
    let lowest = nodes
        .iter()
        .min_by_key(|n| n.interface.unwrap_or(u32::MAX))?;
    let lname = first.name.to_lowercase();
    let kind = match boot_protocol(&lowest.sys) {
        Some(1) if keyboard => DeviceKind::Keyboard,
        Some(2) if mouse => DeviceKind::Mouse,
        _ if mouse && lname.contains("mouse") => DeviceKind::Mouse,
        _ if keyboard && (lname.contains("keyboard") || lname.contains("kbd")) => {
            DeviceKind::Keyboard
        }
        _ if mouse => DeviceKind::Mouse,
        _ => DeviceKind::Keyboard,
    };
    let name = if first.name.trim().is_empty() {
        format!(
            "USB device {:04x}:{:04x}",
            first.vendor_id, first.product_id
        )
    } else {
        first.name.trim().to_string()
    };
    Some((kind, name))
}

/// `bInterfaceProtocol` of the USB interface a HID device sits on
/// (1 = boot keyboard, 2 = boot mouse).
fn boot_protocol(hid_sys: &Path) -> Option<u8> {
    let s = std::fs::read_to_string(hid_sys.parent()?.join("bInterfaceProtocol")).ok()?;
    u8::from_str_radix(s.trim(), 16).ok()
}

/// Capability bitmaps of one input node, from sysfs.
#[derive(Default)]
struct Caps {
    key: Vec<u64>,
    rel: Vec<u64>,
    abs: Vec<u64>,
}

impl Caps {
    fn is_mouse(&self) -> bool {
        bit(&self.key, BTN_LEFT) && bit(&self.rel, REL_X) && !bit(&self.abs, ABS_X)
    }

    fn is_keyboard(&self) -> bool {
        [KEY_ENTER, KEY_A, KEY_Z, KEY_SPACE]
            .iter()
            .all(|&k| bit(&self.key, k))
    }
}

/// Kernel bitmap text: hex words, most significant first.
fn parse_bitmap(s: &str) -> Vec<u64> {
    s.split_whitespace()
        .rev()
        .map(|w| u64::from_str_radix(w, 16).unwrap_or(0))
        .collect()
}

fn bit(words: &[u64], n: usize) -> bool {
    words.get(n / 64).is_some_and(|w| w >> (n % 64) & 1 == 1)
}

/// Every `input/inputN` under a HID device.
fn input_caps(hid_sys: &Path) -> Vec<Caps> {
    let Ok(dir) = std::fs::read_dir(hid_sys.join("input")) else {
        return Vec::new();
    };
    dir.flatten()
        .map(|e| {
            let read = |f: &str| {
                parse_bitmap(
                    &std::fs::read_to_string(e.path().join("capabilities").join(f))
                        .unwrap_or_default(),
                )
            };
            Caps {
                key: read("key"),
                rel: read("rel"),
                abs: read("abs"),
            }
        })
        .collect()
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let nodes: Vec<HidDeviceInfo> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == ctx.vendor_id && d.product_id == pid)
        .collect();
    let refs: Vec<&HidDeviceInfo> = nodes.iter().collect();
    let Some((kind, name)) = classify(&refs) else {
        bail!(
            "{:04x}:{pid:04x} is no longer a mouse or keyboard",
            ctx.vendor_id
        );
    };
    let first = &nodes[0];
    let mut a = GenericInput {
        id: DeviceId {
            vendor_id: ctx.vendor_id,
            product_id: pid,
            instance: if first.uniq.is_empty() {
                first.path.display().to_string()
            } else {
                first.uniq.clone()
            },
        },
        kind,
        name,
        hid_dirs: nodes.iter().map(|n| n.sys.clone()).collect(),
        battery: None,
        last_poll: Instant::now(),
    };
    a.battery = a.read_battery();
    Ok(vec![Box::new(a)])
}

pub struct GenericInput {
    id: DeviceId,
    kind: DeviceKind,
    name: String,
    hid_dirs: Vec<PathBuf>,
    battery: Option<BatteryState>,
    last_poll: Instant,
}

impl GenericInput {
    /// The kernel's HID battery (`power_supply/*` under any of the nodes).
    fn read_battery(&self) -> Option<BatteryState> {
        self.hid_dirs.iter().find_map(|d| {
            let ps = std::fs::read_dir(d.join("power_supply"))
                .ok()?
                .flatten()
                .next()?;
            let read = |f: &str| std::fs::read_to_string(ps.path().join(f)).ok();
            let percent = read("capacity").and_then(|s| s.trim().parse::<u8>().ok());
            let status = read("status").unwrap_or_default();
            Some(BatteryState {
                label: "Battery".into(),
                percent: percent.map(|p| p.min(100)),
                charging: status.trim() == "Charging",
                present: true,
            })
        })
    }
}

impl DeviceAdapter for GenericInput {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            connected: true,
            batteries: self.battery.iter().cloned().collect(),
            settings: SettingsMap::new(),
            controls: vec![],
            last_error: None,
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("{} has no {} control", self.name, key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        if self.last_poll.elapsed() >= BATTERY_POLL {
            self.last_poll = Instant::now();
            self.battery = self.read_battery();
        }
        Ok(())
    }

    fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps_are_most_significant_word_first() {
        // A typical mouse: BTN_LEFT..BTN_TASK in word 4, REL_X|REL_Y|REL_WHEEL.
        let key = parse_bitmap("ffff0000 0 0 0 0");
        assert!(bit(&key, BTN_LEFT));
        assert!(!bit(&key, KEY_A));
        let rel = parse_bitmap("143");
        assert!(bit(&rel, REL_X));
        let mouse = Caps {
            key,
            rel,
            abs: vec![],
        };
        assert!(mouse.is_mouse() && !mouse.is_keyboard());
        // A touchpad (absolute X) is not a mouse.
        let pad = Caps {
            abs: parse_bitmap("3"),
            ..mouse
        };
        assert!(!pad.is_mouse());
        // A full keyboard bitmap (word 0 covers KEY_ESC..KEY_?).
        let kbd = Caps {
            key: parse_bitmap("fffffffffffffffe"),
            ..Caps::default()
        };
        assert!(kbd.is_keyboard() && !kbd.is_mouse());
    }
}
