//! HyperX (VID 0x0951 Kingston era, 0x03f0 HP era): keyboards, mice and headsets.
//!
//! Reimplemented from public notes; only IDs and byte layouts were taken from:
//! - HeadsetControl `lib/devices/hyperx_*.hpp` (GPL, layouts only):
//!   <https://github.com/Sapd/HeadsetControl/tree/master/lib/devices>
//! - HyperHeadset `src/devices/*.rs` (MIT): <https://github.com/LennardKittner/HyperHeadset>
//! - ngenuity-linux-cli `docs/protocol/universal.md` and
//!   `docs/devices/pulsefire-haste-2-wireless.md`: <https://github.com/PMarinov1994/ngenuity-linux-cli>
//! - OpenRGB `Controllers/HyperXMouseController`, `Controllers/HyperXKeyboardController`
//!   (GPL, layouts only): <https://gitlab.com/CalcProgrammer1/OpenRGB>
//! - Pulsefire Dart RE notes (CC0; writes are volatile until a `de ff` save, which
//!   we never send): <https://github.com/santeri3700/hyperx_pulsefire_dart_reverse_engineering>
//! - PlasmaNGenuity `plasmangenuity/drivers/hyperx/protocol.py` (MIT, Dart heartbeat):
//!   <https://github.com/radoslavchobanov/PlasmaNGenuity>
//! - HyperX Saga Control `docs/protocol-notes.md`, `hyperx_saga_control/protocol.py`
//!   (MIT, Saga Pro IDs and live writes): <https://github.com/notwaterbtl/hyperx-saga-control>
//!
//! `headset.rs`, `mouse.rs`, `pulsefire.rs` and `keyboard.rs` each hold pure packet
//! builders and parsers and a spec table (one entry per model); `direct.rs` is the
//! shared adapter for host-streamed lighting. The only onboard write is the
//! universal-protocol mice's slot-1 copy of the DPI table and brightness,
//! behind the owner's save-to-device opt-in (`mouse.rs`); other documented
//! profile writes are blocked placeholders (docs/onboard-writes/hyperx.md).
//! Firmware is never written.

mod direct;
mod headset;
mod keyboard;
mod mouse;
mod pulsefire;

use crate::drivers::Model;
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingsMap};
use hako_core::config::get_setting;
use hako_core::device::DeviceKind;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

const KINGSTON: u16 = 0x0951;
const HP: u16 = 0x03f0;

/// One model: the data a family's adapter needs besides the protocol itself.
struct Spec<F: 'static> {
    name: &'static str,
    vid: u16,
    /// Every PID the device enumerates as; the first names its settings file.
    pids: &'static [u16],
    family: F,
}

macro_rules! model {
    ($kind:ident, $module:ident :: $spec:ident) => {
        Model {
            vendor_id: $module::$spec.vid,
            product_ids: $module::$spec.pids,
            kind: DeviceKind::$kind,
            name: $module::$spec.name,
            usbfs: false,
            multi: false,
            open: |pid, ctx| $module::open(&$module::$spec, pid, ctx),
        }
    };
}

pub const MODELS: &[Model] = &[
    model!(Headset, headset::CLOUD_II),
    model!(Headset, headset::CLOUD_II_HP),
    model!(Headset, headset::CLOUD_III),
    model!(Headset, headset::CLOUD_III_S),
    model!(Headset, headset::CLOUD_II_CORE),
    model!(Headset, headset::CLOUD_ALPHA),
    model!(Headset, headset::CLOUD_ALPHA_2),
    model!(Headset, headset::CLOUD_FLIGHT),
    model!(Headset, headset::CLOUD_FLIGHT_HP),
    model!(Headset, headset::CLOUD_FLIGHT_S),
    model!(Headset, headset::CLOUD_III_WIRED),
    model!(Headset, headset::CLOUD_ALPHA_S),
    model!(Mouse, mouse::HASTE_2_WIRELESS),
    model!(Mouse, mouse::SAGA_PRO),
    model!(Mouse, mouse::DART),
    model!(Mouse, mouse::DART_HP),
    model!(Mouse, pulsefire::HASTE),
    model!(Mouse, pulsefire::HASTE_HP),
    model!(Mouse, pulsefire::SURGE),
    model!(Mouse, pulsefire::SURGE_HP),
    model!(Mouse, pulsefire::CORE),
    model!(Mouse, pulsefire::CORE_HP),
    model!(Mouse, pulsefire::FPS_PRO),
    model!(Mouse, pulsefire::RAID),
    model!(Keyboard, keyboard::ORIGINS),
    model!(Keyboard, keyboard::ORIGINS_HP),
    model!(Keyboard, keyboard::ORIGINS_CORE),
    model!(Keyboard, keyboard::ORIGINS_CORE_HP),
    model!(Keyboard, keyboard::ORIGINS_60),
    model!(Keyboard, keyboard::ORIGINS_60_HP),
    model!(Keyboard, keyboard::ORIGINS_65_HP),
    model!(Keyboard, keyboard::ORIGINS_2_65),
    model!(Keyboard, keyboard::ELITE_2_KB),
    model!(Keyboard, keyboard::ELITE_2_HP),
    model!(Keyboard, keyboard::ELITE_RGB),
    model!(Keyboard, keyboard::FPS_RGB),
    model!(Keyboard, keyboard::EVE_1800),
];

/// Which hidraw node of a device carries its vendor protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Node {
    /// USB interface number, when the source names it.
    Iface(u32),
    /// The node whose report descriptor declares this report ID; the lowest
    /// interface when none does (HyperHeadset uses the first node on Linux).
    Report(u8),
    /// The node with a top-level application collection of this usage page
    /// and usage (OpenRGB's page/usage detectors).
    Usage(u16, u16),
}

/// Open the protocol node; returns the device and a stable instance key.
fn open_node(vid: u16, pid: u16, node: Node) -> Result<(HidrawDevice, String)> {
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .collect();
    nodes.sort_by_key(|d| d.interface.unwrap_or(u32::MAX));
    let info = match node {
        Node::Iface(i) => nodes.into_iter().find(|d| d.interface == Some(i)),
        Node::Report(id) => match nodes.iter().position(|d| declares_report(&d.path, id)) {
            Some(i) => Some(nodes.swap_remove(i)),
            None => nodes.into_iter().next(),
        },
        Node::Usage(page, usage) => nodes
            .into_iter()
            .find(|d| has_application(&d.report_descriptor(), page, usage)),
    }
    .with_context(|| format!("no hidraw node for {vid:04x}:{pid:04x} ({node:?})"))?;
    let dev = HidrawDevice::open(&info.path)?;
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq
    };
    Ok((dev, instance))
}

/// Reads the descriptor from sysfs; the device node itself is not touched.
fn declares_report(dev_path: &Path, id: u8) -> bool {
    let Some(name) = dev_path.file_name() else {
        return false;
    };
    let desc = Path::new("/sys/class/hidraw")
        .join(name)
        .join("device/report_descriptor");
    std::fs::read(desc).is_ok_and(|d| crate::hid::report_ids(&d).contains(&id))
}

/// Whether a report descriptor opens a top-level application collection
/// (`a1 01`) under this usage page and usage.
fn has_application(desc: &[u8], page: u16, usage: u16) -> bool {
    let (mut cur_page, mut cur_usage, mut depth) = (0, 0, 0u32);
    for (tag, value) in crate::hid::descriptor_items(desc) {
        match tag {
            0x04 => cur_page = value,
            0x08 => cur_usage = value,
            0xa0 => {
                if depth == 0
                    && value == 1
                    && cur_page == u32::from(page)
                    && cur_usage == u32::from(usage)
                {
                    return true;
                }
                depth += 1;
            }
            0xc0 => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// `bytes` zero-padded to `len`.
fn pad(bytes: &[u8], len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    v[..bytes.len()].copy_from_slice(bytes);
    v
}

/// Read input reports until one satisfies `stop` or `timeout` passes; returns
/// every report read. A zero timeout drains what is already queued.
fn read_reports(
    dev: &mut HidrawDevice,
    timeout: Duration,
    stop: impl Fn(&[u8]) -> bool,
) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    // Bounded so a device streaming reports cannot pin the worker.
    for _ in 0..64 {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(n) = dev.read_timeout(&mut buf, left) else {
            break;
        };
        out.push(buf[..n].to_vec());
        if stop(&buf[..n]) {
            break;
        }
    }
    out
}

/// Write `req`, then [`read_reports`].
fn exchange(
    dev: &mut HidrawDevice,
    req: &[u8],
    timeout: Duration,
    stop: impl Fn(&[u8]) -> bool,
) -> Result<Vec<Vec<u8>>> {
    dev.write(req)?;
    Ok(read_reports(dev, timeout, stop))
}

/// HIDIOCGINPUT (`_IOC(_IOC_READ|_IOC_WRITE, 'H', 0x0a, len)`); `buf[0]` is the report id.
fn get_input_report(file: &File, buf: &mut [u8]) -> Result<()> {
    let req = crate::hid::hidioc(0x0a, buf.len());
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), req, buf.as_mut_ptr()) };
    if ret < 0 {
        bail!("HIDIOCGINPUT: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn int(s: &SettingsMap, key: SettingKey, default: i64) -> i64 {
    get_setting(s, key)
        .and_then(|v| v.as_int())
        .unwrap_or(default)
}

fn text<'a>(s: &'a SettingsMap, key: SettingKey, default: &'a str) -> &'a str {
    get_setting(s, key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
}

/// Saved color, HyperX red by default.
fn rgb(s: &SettingsMap) -> [u8; 3] {
    [
        int(s, SettingKey::ColorR, 255),
        int(s, SettingKey::ColorG, 0),
        int(s, SettingKey::ColorB, 0),
    ]
    .map(|c| c.clamp(0, 255) as u8)
}

/// Effect -> Brightness -> Speed -> Color, irrelevant ones hidden (same order
/// as the other drivers).
fn lighting_controls(
    effects: &[(&str, &str)],
    effect: &str,
    with_speed: &[&str],
    with_color: &[&str],
) -> Vec<SettingMeta> {
    use SettingKey::*;
    let has_color = with_color.contains(&effect);
    let mut c = vec![
        SettingMeta::dropdown(
            Effect,
            "Effect",
            effects
                .iter()
                .map(|(v, l)| SettingOption::text(v, l))
                .collect(),
        ),
        SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"),
        SettingMeta::dropdown(
            Speed,
            "Speed",
            vec![
                SettingOption::text("slow", "Slow"),
                SettingOption::text("medium", "Medium"),
                SettingOption::text("fast", "Fast"),
            ],
        )
        .hidden(!with_speed.contains(&effect)),
    ];
    for (key, label) in [(ColorR, "Red"), (ColorG, "Green"), (ColorB, "Blue")] {
        c.push(SettingMeta::color(key, label).hidden(!has_color));
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_is_registered_once() {
        let names: Vec<_> = MODELS.iter().map(|m| (m.vendor_id, m.name)).collect();
        assert_eq!(
            MODELS.len(),
            headset::ALL.len() + mouse::ALL.len() + pulsefire::ALL.len() + keyboard::ALL.len()
        );
        for (vid, name) in [
            headset::ALL
                .iter()
                .map(|s| (s.vid, s.name))
                .collect::<Vec<_>>(),
            mouse::ALL.iter().map(|s| (s.vid, s.name)).collect(),
            pulsefire::ALL.iter().map(|s| (s.vid, s.name)).collect(),
            keyboard::ALL.iter().map(|s| (s.vid, s.name)).collect(),
        ]
        .concat()
        {
            assert!(names.contains(&(vid, name)), "{name} not in MODELS");
        }
        for m in MODELS {
            assert!(
                matches!(m.vendor_id, KINGSTON | HP),
                "{} has a foreign VID",
                m.name
            );
        }
    }

    #[test]
    fn finds_top_level_application_collections() {
        // Mouse: page 1 usage 2 application, a physical pointer inside.
        let mouse = [
            0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0xc0, 0xc0,
        ];
        assert!(has_application(&mouse, 0x0001, 0x0002));
        assert!(!has_application(&mouse, 0x0001, 0x0001));
        // Vendor collection after a keyboard one.
        let two = [
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0, 0x06, 0x01, 0xff, 0x09, 0x01, 0xa1, 0x01,
            0xc0,
        ];
        assert!(has_application(&two, 0xff01, 0x0001));
        assert!(has_application(&two, 0x0001, 0x0006));
    }

    #[test]
    fn pad_and_color_defaults() {
        assert_eq!(pad(&[1, 2], 4), vec![1, 2, 0, 0]);
        let mut s = SettingsMap::new();
        assert_eq!(rgb(&s), [255, 0, 0]);
        hako_core::config::merge_setting(
            &mut s,
            SettingKey::ColorG,
            hako_core::catalog::SettingValue::Int(300),
        );
        assert_eq!(rgb(&s), [255, 255, 0]);
    }
}
