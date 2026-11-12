//! Device drivers, one folder per manufacturer.
//!
//! Adding hardware: export `MODELS` from `drivers/<vendor>/mod.rs` and list it
//! in [`models`]. Discovery, placeholders and remap attach are generic in the hub.

pub mod asus;
pub mod audeze;
pub mod compx;
pub mod coolermaster;
pub mod corsair;
pub mod endgamegear;
pub mod generic;
pub mod glorious;
pub mod hyperx;
pub mod lamparray;
pub mod logitech;
pub mod msi;
pub mod pulsar;
pub mod qmk;
pub mod razer;
pub mod redragon;
pub mod roccat;
pub mod sony;
pub mod steelseries;
pub mod turtlebeach;
pub mod wooting;

use crate::hid::HidDeviceInfo;
use anyhow::{bail, Result};
use hako_audio::AudioService;
use hako_core::catalog::{SettingKey, SettingValue};
use hako_core::config::ConfigPaths;
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use hako_core::protocol::AdapterEvent;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub type EventTx = broadcast::Sender<AdapterEvent>;

/// What `Model::open` returns: every device reachable through the node.
pub type Opened = Result<Vec<Box<dyn DeviceAdapter>>>;

/// Shared services a driver may use when opening a device.
#[derive(Clone)]
pub struct DriverCtx {
    pub paths: ConfigPaths,
    pub audio: Arc<AudioService>,
    /// Devices of this model that are already open and connected. Empty
    /// except when a `multi` model is rescanned: its `open` must skip them.
    pub open: Vec<DeviceId>,
    /// The model's vendor (lets one `open` serve several vendors).
    pub vendor_id: u16,
}

/// One supported product. `product_ids` lists every PID the same physical
/// device can enumerate as (receiver, wired, ...); at most one adapter is open
/// per model. The first PID is canonical: it names the model's remap file.
pub struct Model {
    pub vendor_id: u16,
    pub product_ids: &'static [u16],
    pub kind: DeviceKind,
    pub name: &'static str,
    /// Needs the raw USB node (usbfs control transfers), not just hidraw.
    pub usbfs: bool,
    /// Hosts several devices (a receiver): reopened every [`RESCAN`] while
    /// connected, to pick up new pairings. `open` must then return only
    /// devices not in `ctx.open`.
    pub multi: bool,
    /// Called from a blocking thread with the first matching PID present.
    /// A receiver may host several devices: return one adapter per paired
    /// device (each `DeviceId` may carry the device's own wireless PID).
    /// An empty list means "nothing reachable yet"; it is retried later.
    pub open: fn(pid: u16, ctx: &DriverCtx) -> Opened,
}

impl Model {
    pub fn claims(&self, id: &DeviceId) -> bool {
        id.vendor_id == self.vendor_id && self.product_ids.contains(&id.product_id)
    }

    /// Same registry entry (models are unique by vendor + PID list).
    pub fn same(&self, other: &Model) -> bool {
        self.vendor_id == other.vendor_id && self.product_ids == other.product_ids
    }

    /// PIDs the remap engine should treat as this device: the model's own,
    /// or for a device paired to a multi-device receiver its own PID first
    /// (it names the remap file) plus the receiver's, whose input nodes may
    /// carry its events.
    pub fn remap_pids(&self, id: &DeviceId) -> Vec<u16> {
        if self.claims(id) {
            self.product_ids.to_vec()
        } else {
            std::iter::once(id.product_id)
                .chain(self.product_ids.iter().copied())
                .collect()
        }
    }
}

/// How often a connected `multi` model is reopened for new pairings.
pub const RESCAN: std::time::Duration = std::time::Duration::from_secs(10);

/// A device no model table lists, accepted by a runtime probe.
pub struct Fallback {
    pub kind: DeviceKind,
    pub name: String,
    pub open: fn(pid: u16, ctx: &DriverCtx) -> Opened,
}

/// Probe over every hidraw node of one unlisted VID:PID. Must read sysfs
/// only (no device I/O): it runs for every unknown device on every pass.
pub type Probe = fn(nodes: &[&HidDeviceInfo]) -> Option<Fallback>;

/// Runtime probes, most specific first; `generic` (input-only devices) last.
const FALLBACKS: &[Probe] = &[
    logitech::fallback,
    qmk::fallback,
    lamparray::fallback,
    generic::fallback,
];

/// Vendors whose hidraw nodes get access vendor-wide in the udev rules,
/// because a probe may claim any of their PIDs.
const FALLBACK_VENDORS: &[(u16, &str)] = &[(0x046d, "Logitech HID++ devices not listed above")];

/// The model for an unlisted VID:PID whose nodes a probe accepts. Created
/// once per VID:PID and leaked, so the hub treats it like a table entry.
/// ponytail: one leaked `Model` per distinct unknown device ever seen.
pub fn fallback_model(nodes: &[&HidDeviceInfo]) -> Option<&'static Model> {
    static SEEN: Mutex<Vec<&'static Model>> = Mutex::new(Vec::new());
    let first = nodes.first()?;
    let (vid, pid) = (first.vendor_id, first.product_id);
    let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(m) = seen
        .iter()
        .find(|m| m.vendor_id == vid && m.product_ids == [pid])
    {
        return Some(m);
    }
    let fb = FALLBACKS.iter().find_map(|probe| probe(nodes))?;
    let model: &'static Model = Box::leak(Box::new(Model {
        vendor_id: vid,
        product_ids: Box::leak(Box::new([pid])),
        kind: fb.kind,
        name: Box::leak(fb.name.into_boxed_str()),
        usbfs: false,
        multi: false,
        open: fb.open,
    }));
    seen.push(model);
    Some(model)
}

/// Every vendor's model table, in discovery order.
const VENDORS: [(&str, &[Model]); 18] = [
    ("Razer", razer::MODELS),
    ("Logitech", logitech::MODELS),
    ("SteelSeries", steelseries::MODELS),
    ("Corsair", corsair::MODELS),
    ("HyperX", hyperx::MODELS),
    ("ASUS", asus::MODELS),
    ("ROCCAT", roccat::MODELS),
    ("Sony", sony::MODELS),
    ("Audeze", audeze::MODELS),
    ("Glorious", glorious::MODELS),
    ("Cooler Master", coolermaster::MODELS),
    ("MSI", msi::MODELS),
    ("Redragon", redragon::MODELS),
    ("Wooting", wooting::MODELS),
    ("Turtle Beach headsets", turtlebeach::MODELS),
    ("ATK / VXE / VGN / Lamzu (Compx)", compx::MODELS),
    ("Pulsar", pulsar::MODELS),
    ("Endgame Gear", endgamegear::MODELS),
];

/// Every supported model, in discovery order.
pub fn models() -> impl Iterator<Item = &'static Model> {
    VENDORS.into_iter().flat_map(|(_, m)| m)
}

/// Devices confirmed on real hardware; everything else is implemented from
/// public protocol documentation. Add a model here once someone verifies it.
const HARDWARE_VERIFIED: &[(u16, u16)] = &[(0x1532, 0x026b), (0x046d, 0x40a8), (0x1038, 0x12e5)];

/// Markdown list of every model. `docs/DEVICES.md` must match (a test
/// checks); regenerate with `hakod --list-devices > docs/DEVICES.md`.
pub fn device_list() -> String {
    let mut out = String::from(
        "# Supported devices\n\n\
         Generated by `hakod --list-devices` from `daemon/src/drivers`. Do not edit.\n\n\
         **Verified** devices have been tested on real hardware. All others are implemented\n\
         from public protocol documentation and are untested; reports welcome.\n\
         Features are discovered at runtime and shown in the UI per device.\n",
    );
    for (vendor, models) in VENDORS {
        out +=
            &format!("\n## {vendor}\n\n| Device | Type | USB IDs | Status |\n|---|---|---|---|\n");
        for m in models {
            let ids: Vec<String> = m
                .product_ids
                .iter()
                .map(|p| format!("`{:04x}:{p:04x}`", m.vendor_id))
                .collect();
            let verified = m
                .product_ids
                .iter()
                .any(|&p| HARDWARE_VERIFIED.contains(&(m.vendor_id, p)));
            let kind = match m.kind {
                DeviceKind::Keyboard => "Keyboard",
                DeviceKind::Mouse => "Mouse",
                DeviceKind::Headset => "Headset",
                DeviceKind::Other => "Receiver",
            };
            out += &format!(
                "| {} | {kind} | {} | {} |\n",
                m.name,
                ids.join(" "),
                if verified { "Verified" } else { "Untested" }
            );
        }
    }
    out
}

/// Markdown report of every hidraw node and what handles it, for support
/// requests: an unsupported device's IDs, interfaces and HID layout.
pub fn diagnose() -> Result<String> {
    let nodes = crate::hid::enumerate_hidraw()?;
    let mut out = String::from(
        "# hako device report\n\n\
         Attach this to a device-support request. Descriptors come from sysfs;\n\
         no device was written to.\n\n\
         | Node | USB ID | Bus | Iface | Name | Handled by | Report IDs | Usage pages |\n\
         |---|---|---|---|---|---|---|---|\n",
    );
    for n in &nodes {
        let same: Vec<&HidDeviceInfo> = nodes
            .iter()
            .filter(|d| d.vendor_id == n.vendor_id && d.product_id == n.product_id)
            .collect();
        let handler = match models()
            .find(|m| m.vendor_id == n.vendor_id && m.product_ids.contains(&n.product_id))
        {
            Some(m) => m.name.to_string(),
            None => fallback_model(&same).map_or("**unsupported**".into(), |m| {
                format!("{} (runtime probe)", m.name)
            }),
        };
        let desc = n.report_descriptor();
        let hex = |mut v: Vec<String>| {
            v.sort();
            v.dedup();
            if v.is_empty() {
                "-".into()
            } else {
                v.join(" ")
            }
        };
        out += &format!(
            "| {} | `{:04x}:{:04x}` | {:#04x} | {} | {} | {handler} | {} | {} |\n",
            n.path.display(),
            n.vendor_id,
            n.product_id,
            n.bus,
            n.interface.map_or("-".into(), |i| i.to_string()),
            n.name.replace('|', "/"),
            hex(crate::hid::report_ids(&desc)
                .iter()
                .map(|r| format!("{r:02x}"))
                .collect()),
            hex(crate::hid::usage_pages(&desc)
                .iter()
                .map(|u| format!("{u:04x}"))
                .collect()),
        );
    }
    Ok(out)
}

/// udev rules for every model. `udev/70-hako.rules` must match (a test
/// checks); regenerate with `hakod --udev-rules > udev/70-hako.rules`.
/// The file sorts before `73-seat-late.rules`, which applies `uaccess`.
pub fn udev_rules() -> String {
    let access = r#"MODE="0660", GROUP="plugdev", TAG+="uaccess""#;
    // Raw USB / input / uinput: the active seat user only, no group.
    let seat = r#"MODE="0660", TAG+="uaccess""#;
    let mut out = String::from(
        "# Generated by `hakod --udev-rules` from daemon/src/drivers. Do not edit.\n\
         # hidraw: GROUP=plugdev + MODE=0660, plus a seat ACL (uaccess).\n\
         # USB control nodes and remap input nodes: seat ACL only.\n",
    );
    for m in models() {
        out += &format!("\n# {}\n", m.name);
        for pid in m.product_ids {
            let (v, p) = (format!("{:04x}", m.vendor_id), format!("{pid:04x}"));
            out += &format!(
                "KERNEL==\"hidraw*\", SUBSYSTEM==\"hidraw\", ATTRS{{idVendor}}==\"{v}\", ATTRS{{idProduct}}==\"{p}\", {access}\n"
            );
            if m.usbfs {
                out += &format!(
                    "SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{v}\", ATTR{{idProduct}}==\"{p}\", {seat}\n"
                );
            }
            // Remaps read and grab the device's input nodes (receivers carry
            // their paired devices' input).
            if m.kind != DeviceKind::Headset {
                out += &format!(
                    "KERNEL==\"event*\", SUBSYSTEM==\"input\", ATTRS{{idVendor}}==\"{v}\", ATTRS{{idProduct}}==\"{p}\", TAG+=\"uaccess\"\n"
                );
            }
        }
    }
    for (vid, what) in FALLBACK_VENDORS {
        out += &format!(
            "\n# {what}\nKERNEL==\"hidraw*\", SUBSYSTEM==\"hidraw\", ATTRS{{idVendor}}==\"{vid:04x}\", {access}\n"
        );
    }
    // QMK raw HID (usage page 0xFF60, usage 0x61) on any vendor's keyboard.
    out += &format!(
        "\n# QMK / VIA keyboards: raw HID interface (usage page 0xFF60, usage 0x61).\n\
         KERNEL==\"hidraw*\", SUBSYSTEM==\"hidraw\", PROGRAM=\"/bin/sh -c 'LC_ALL=C grep -qaP \\\"\\x06\\x60\\xff\\x09\\x61\\\" /sys%p/device/report_descriptor'\", {access}\n"
    );
    // HID LampArray (usage page 0x59, usage 0x01) in the spec's encoding.
    out += &format!(
        "\n# HID LampArray / Dynamic Lighting devices (usage page 0x59, usage 0x01).\n\
         KERNEL==\"hidraw*\", SUBSYSTEM==\"hidraw\", PROGRAM=\"/bin/sh -c 'LC_ALL=C grep -qaP \\\"\\x05\\x59\\x09\\x01\\xa1\\x01\\\" /sys%p/device/report_descriptor'\", {access}\n"
    );
    out += &format!(
        "\n# Host-side remaps and macros inject through uinput.\n\
         KERNEL==\"uinput\", SUBSYSTEM==\"misc\", {seat}, OPTIONS+=\"static_node=uinput\"\n"
    );
    out
}

pub trait DeviceAdapter: Send {
    fn id(&self) -> &DeviceId;
    fn snapshot(&self) -> DeviceSnapshot;
    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()>;
    fn tick(&mut self) -> Result<()>;
    fn shutdown(&mut self);
    /// How often `tick` runs. Shorten it (e.g. ~50 ms) for devices whose
    /// lighting must be re-sent continuously; may change between ticks.
    fn tick_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(1)
    }
}

/// Shown when a known device is plugged in but hidraw cannot be opened (usually permissions).
pub struct PlaceholderAdapter {
    id: DeviceId,
    kind: DeviceKind,
    name: String,
    error: String,
}

impl PlaceholderAdapter {
    pub fn permission_denied(
        id: DeviceId,
        kind: DeviceKind,
        name: &str,
        detail: impl ToString,
    ) -> Self {
        let detail = detail.to_string();
        let error = if detail.contains("Permission denied") || detail.contains("permission") {
            format!(
                "Cannot open device ({detail}). Install udev rules:\n  sudo install -m 644 udev/70-hako.rules /etc/udev/rules.d/\n  sudo udevadm control --reload-rules && sudo udevadm trigger\nThen unplug/replug and restart hakod."
            )
        } else {
            detail
        };
        Self {
            id,
            kind,
            name: name.into(),
            error,
        }
    }
}

impl DeviceAdapter for PlaceholderAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            connected: false,
            batteries: vec![],
            settings: Default::default(),
            controls: vec![],
            last_error: Some(self.error.clone()),
        }
    }

    fn set(&mut self, _key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("{}", self.error)
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn device_list_file_matches_driver_registry() {
        assert_eq!(
            include_str!("../../../docs/DEVICES.md"),
            super::device_list(),
            "run: cargo run -p hakod -- --list-devices > docs/DEVICES.md"
        );
    }

    #[test]
    fn udev_rules_file_matches_driver_registry() {
        assert_eq!(
            include_str!("../../../udev/70-hako.rules"),
            super::udev_rules(),
            "run: cargo run -p hakod -- --udev-rules > udev/70-hako.rules"
        );
    }

    #[test]
    fn no_two_models_claim_the_same_usb_id() {
        let mut seen = std::collections::HashSet::new();
        for m in super::models() {
            assert!(!m.product_ids.is_empty(), "{} lists no PIDs", m.name);
            for &pid in m.product_ids {
                assert!(
                    seen.insert((m.vendor_id, pid)),
                    "{:04x}:{pid:04x} claimed twice ({})",
                    m.vendor_id,
                    m.name
                );
            }
        }
    }
}
