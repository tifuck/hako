//! Logitech G / MX over HID++ 2.0, plus the headsets that speak something else.
//!
//! - `pro2`: the PRO 2 LIGHTSPEED (`046d:40a8` via receiver `046d:c543`), its
//!   original adapter, unchanged on the wire. The model also serves the other
//!   slots of its `c543` receiver through the generic adapter.
//! - `device`: one generic adapter for every other HID++ model; features are
//!   found at runtime (FEATURE_SET), so a model entry is just VID:PID + kind +
//!   name.
//! - Receivers (Bolt, Unifying, Nano, LIGHTSPEED) are `multi`: one adapter per
//!   paired slot, `DeviceId` = the device's wireless PID, instance
//!   `hidpp:<slot>`; rescans skip open devices and slots still held, so a
//!   device paired later shows up on the next rescan. A slot whose device was
//!   unpaired or replaced is released by its adapter (see `device.rs`).
//! - Wired devices, Bluetooth devices and headset dongles talk to device index
//!   0xff.
//! - `centurion`, `g930`, `zone`: headsets that are not plain HID++ 2.0.
//! - [`fallback`]: any other 046d device with a HID++ long-report node gets
//!   the generic adapter, named and typed by DEVICE_NAME.
//!
//! HID++ is only spoken on hidraw nodes whose report descriptor (read from
//! sysfs) declares report 0x11, so keyboard/mouse input interfaces never see
//! vendor reports.
//!
//! Sources (IDs and layouts cited, no code copied):
//! - Receiver PIDs and kinds: Solaar `lib/logitech_receiver/base_usb.py`.
//!   `c543` (PRO 2 receiver) is not listed there; its other slots are read
//!   with LIGHTSPEED-style pairing registers, and a receiver that rejects
//!   them simply yields no extra slots.
//! - Wired / headset PIDs: Solaar `descriptors.py`, libratbag
//!   `data/devices/logitech-*.device` (which also pairs each wired-mode PID
//!   with its wireless PID), Linux `drivers/hid/hid-logitech-hidpp.c` device
//!   table, OpenRGB `Controllers/LogitechController` (detectors and the
//!   `LogitechHIDPP20` keyboard tables), HeadsetControl `lib/devices/logitech*`.
//! - Per-model feature sets (the `DOCUMENTED` table): Solaar `docs/devices/*`
//!   (`solaar show` dumps) or the source named next to each entry.
//! - Receiver children: Linux `drivers/hid/hid-logitech-dj.c` (each paired
//!   device becomes a HID device under the receiver's, `046d:<wpid>`, with
//!   HID++ reports when the device has them).
//! - Protocol layouts: see `hidpp.rs`.

mod centurion;
mod device;
mod g930;
mod hidpp;
mod pro2;
mod zone;

use crate::drivers::{DeviceAdapter, DriverCtx, Fallback, Model, Opened};
use crate::hid::{enumerate_hidraw, HidDeviceInfo, HidrawDevice, BUS_USB};
use anyhow::{Context, Result};
use device::{HidppAdapter, Link, VID};
use hako_core::device::DeviceKind;
use std::path::Path;
use tracing::debug;

const BOLT: u16 = 0xc548;
/// 27 MHz receiver: no pairing registers (Solaar reads its devices from udev).
const EX100: u16 = 0xc517;

macro_rules! direct {
    ($kind:ident, $name:literal, $($pid:literal),+) => {
        Model {
            vendor_id: VID,
            product_ids: &[$($pid),+],
            kind: DeviceKind::$kind,
            name: $name,
            usbfs: false,
            multi: false,
            open: open_direct,
        }
    };
}

macro_rules! receiver {
    ($pid:literal, $name:literal) => {
        Model {
            vendor_id: VID,
            product_ids: &[$pid],
            kind: DeviceKind::Other,
            name: $name,
            usbfs: false,
            multi: true,
            open: open_receiver,
        }
    };
}

macro_rules! own {
    ($module:ident, $name:literal, $($pid:literal),+) => {
        Model {
            vendor_id: VID,
            product_ids: &[$($pid),+],
            kind: DeviceKind::Headset,
            name: $name,
            usbfs: false,
            multi: false,
            open: $module::open,
        }
    };
}

pub const MODELS: &[Model] = &[
    pro2::MODEL,
    // Receivers (Solaar base_usb.py).
    receiver!(0xc548, "Logitech Bolt Receiver"),
    receiver!(0xc52b, "Logitech Unifying Receiver"),
    receiver!(0xc532, "Logitech Unifying Receiver"),
    receiver!(0xc539, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc53a, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc53d, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc53f, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc541, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc545, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc547, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc54d, "Logitech LIGHTSPEED Receiver"),
    receiver!(0xc52f, "Logitech Nano Receiver"),
    receiver!(0xc518, "Logitech Nano Receiver"),
    receiver!(0xc51a, "Logitech Nano Receiver"),
    receiver!(0xc51b, "Logitech Nano Receiver"),
    receiver!(0xc521, "Logitech Nano Receiver"),
    receiver!(0xc525, "Logitech Nano Receiver"),
    receiver!(0xc526, "Logitech Nano Receiver"),
    receiver!(0xc52e, "Logitech Nano Receiver"),
    receiver!(0xc531, "Logitech Nano Receiver"),
    receiver!(0xc534, "Logitech Nano Receiver"),
    receiver!(0xc535, "Logitech Nano Receiver"),
    receiver!(0xc537, "Logitech Nano Receiver"),
    receiver!(0xc517, "Logitech EX100 Receiver 27 MHz"),
    // Mice, wired or wireless-on-cable (feature sources in DOCUMENTED).
    direct!(Mouse, "Logitech PRO X SUPERLIGHT 2", 0xc09b),
    direct!(Mouse, "Logitech PRO 2 LIGHTSPEED (wired)", 0xc09a),
    direct!(Mouse, "Logitech PRO X SUPERLIGHT", 0xc094),
    direct!(Mouse, "Logitech G502 X PLUS", 0xc095),
    direct!(Mouse, "Logitech G502 X", 0xc099),
    direct!(Mouse, "Logitech G502 HERO", 0xc08b),
    direct!(Mouse, "Logitech G502 LIGHTSPEED", 0xc08d),
    direct!(Mouse, "Logitech G502 Proteus Spectrum", 0xc332),
    direct!(Mouse, "Logitech G502 Proteus Core", 0xc07d),
    direct!(Mouse, "Logitech G203 LIGHTSYNC", 0xc092, 0xc09d),
    direct!(Mouse, "Logitech G203 Prodigy", 0xc084),
    direct!(Mouse, "Logitech G Pro Wireless", 0xc088),
    direct!(Mouse, "Logitech G Pro", 0xc085),
    direct!(Mouse, "Logitech G Pro HERO", 0xc08c),
    direct!(Mouse, "Logitech G703 LIGHTSPEED", 0xc087),
    direct!(Mouse, "Logitech G903 LIGHTSPEED HERO", 0xc091),
    direct!(Mouse, "Logitech G403 HERO", 0xc08f),
    direct!(Mouse, "Logitech G303 Daedalus Apex", 0xc080),
    // Keyboards.
    direct!(Keyboard, "Logitech G915 X", 0xc359),
    direct!(Keyboard, "Logitech G915", 0xc33e),
    direct!(Keyboard, "Logitech G915 TKL", 0xc343),
    direct!(Keyboard, "Logitech G815", 0xc33f),
    direct!(Keyboard, "Logitech G515 LIGHTSPEED TKL", 0xc355),
    direct!(Keyboard, "Logitech PRO X RAPID", 0xc35b),
    direct!(Keyboard, "Logitech G PRO Keyboard", 0xc339),
    direct!(Keyboard, "Logitech G512 / G513", 0xc33c),
    direct!(Keyboard, "Logitech G512", 0xc342),
    direct!(Keyboard, "Logitech G213 Prodigy", 0xc336),
    direct!(Keyboard, "Logitech G810", 0xc337),
    direct!(Keyboard, "Logitech G810", 0xc331),
    direct!(Keyboard, "Logitech G610", 0xc338),
    direct!(Keyboard, "Logitech G610", 0xc333),
    direct!(Keyboard, "Logitech G910", 0xc335),
    direct!(Keyboard, "Logitech G910 Orion Spark", 0xc32b),
    // Headsets (USB dongle; the HID++ device is the headset itself).
    own!(centurion, "Logitech PRO X 2 LIGHTSPEED", 0x0af7),
    own!(centurion, "Logitech PRO X 3 LIGHTSPEED", 0x0b45),
    own!(centurion, "Logitech G522 LIGHTSPEED", 0x0b18),
    direct!(Headset, "Logitech G PRO X 2", 0x0afb),
    direct!(Headset, "Logitech G PRO X 2", 0x0afc),
    direct!(Headset, "Logitech PRO X Wireless", 0x0aba),
    direct!(Headset, "Logitech G PRO X", 0x0aaa),
    direct!(Headset, "Logitech G PRO", 0x0aa7),
    direct!(Headset, "Logitech G733", 0x0ab5),
    direct!(Headset, "Logitech G733", 0x0afe),
    direct!(Headset, "Logitech G733", 0x0b1f),
    direct!(Headset, "Logitech G535", 0x0ac4),
    direct!(Headset, "Logitech G935", 0x0a87),
    direct!(Headset, "Logitech G933", 0x0a5b),
    direct!(Headset, "Logitech G635", 0x0a89),
    direct!(Headset, "Logitech G633", 0x0a5c),
    direct!(Headset, "Logitech G432", 0x0a9c),
    direct!(Headset, "Logitech G431", 0x0a9b),
    direct!(Headset, "Logitech G433", 0x0a6d),
    own!(g930, "Logitech G930", 0x0a1f),
    own!(zone, "Logitech Zone Wired", 0x0aad),
    own!(zone, "Logitech Zone 750", 0x0ade),
];

/// HID++ features each direct model is documented to expose, and where from.
/// Wired-mode PIDs of wireless mice use the dump of the wireless PID that
/// libratbag / Solaar list as the same device. HeadsetControl rows: its
/// fixed feature indexes match the Solaar dump of a family member, or its
/// request has the named feature's function layout.
#[cfg(test)]
const DOCUMENTED: &[(u16, &[u16], &str)] = &[
    (
        0xc09b,
        &[0x1004, 0x2202, 0x8061, 0x8100],
        "kernel hid-logitech-hidpp.c C09B (Pro X Superlight 2); Solaar PRO X 2 40A9",
    ),
    (
        0xc09a,
        &[0x2202, 0x8061, 0x8071],
        "kernel hid-logitech-hidpp.c C09A (PRO 2 LIGHTSPEED over USB); pro2.rs (hardware-verified 40A8)",
    ),
    (
        0xc094,
        &[0x1004, 0x2201, 0x8060, 0x8070, 0x8100],
        "Solaar PRO X Wireless 4093; descriptors.py usbid C094",
    ),
    (
        0xc095,
        &[0x1004, 0x2201, 0x8060, 0x8071, 0x8081, 0x8100],
        "Solaar G502 X PLUS 4099; libratbag g502-x-plus",
    ),
    (0xc099, &[0x2201, 0x8060, 0x8100], "Solaar G502 X C099"),
    (
        0xc08b,
        &[0x2201, 0x8060, 0x8070, 0x8100],
        "Solaar G502 SE Hero Gaming Mouse C08B",
    ),
    (
        0xc08d,
        &[0x1001, 0x2201, 0x8060, 0x8070, 0x8100],
        "Solaar G502 Lightspeed 407F; libratbag g502-hero-wireless",
    ),
    (
        0xc332,
        &[0x2201, 0x8060, 0x8070, 0x8100],
        "Solaar G502 Proteus Spectrum Optical Mouse C332",
    ),
    (
        0xc07d,
        &[0x2201, 0x8060, 0x8100],
        "Solaar G502 Gaming Mouse C07D",
    ),
    (
        0xc092,
        &[0x8071, 0x8081],
        "OpenRGB LogitechG203LController (0x8071 SetSWControl fn 5, 0x8081 fn 1 / FrameEnd fn 7); libratbag g102-g203",
    ),
    (
        0xc084,
        &[0x8070],
        "OpenRGB DetectLogitechMouseG203 (0x8070 setZoneEffect fn 3); libratbag g102-g203",
    ),
    (
        0xc088,
        &[0x1001, 0x2201, 0x8060, 0x8070, 0x8100],
        "Solaar G Pro Wireless 4079; libratbag g-pro-wireless",
    ),
    (
        0xc085,
        &[0x8070],
        "OpenRGB DetectLogitechMouseGPRO (0x8070 setZoneEffect fn 3); libratbag g-pro",
    ),
    (
        0xc08c,
        &[0x8070],
        "OpenRGB DetectLogitechMouseGPRO (G Pro HERO); libratbag g-pro",
    ),
    (
        0xc087,
        &[0x1001, 0x2201, 0x8060, 0x8070, 0x8100],
        "Solaar G703 4070; libratbag g703",
    ),
    (
        0xc091,
        &[0x1001, 0x2201, 0x8060, 0x8070, 0x8071, 0x8100],
        "Solaar G903 LIGHTSPEED 4087; libratbag g903-hero",
    ),
    (
        0xc08f,
        &[0x8070],
        "OpenRGB DetectLogitechMouseG403 (0x8070 setZoneEffect fn 3); libratbag g403-hero",
    ),
    (
        0xc080,
        &[0x8070],
        "OpenRGB DetectLogitechMouseG303 (0x8070 setZoneEffect fn 3); libratbag g303",
    ),
    (
        0xc359,
        &[0x8081],
        "OpenRGB LogitechHIDPP20 known_kb_layout_edits C359 (0x8081 per-key branch)",
    ),
    (
        0xc33e,
        &[0x1001, 0x8040, 0x8060, 0x8071, 0x8081, 0x8100],
        "Solaar G915 C33E",
    ),
    (
        0xc343,
        &[0x1001, 0x8040, 0x8060, 0x8071, 0x8081, 0x8100],
        "Solaar G915 TKL 408E; descriptors.py usbid C343",
    ),
    (
        0xc33f,
        &[0x8040, 0x8060, 0x8071, 0x8081, 0x8100],
        "Solaar G815 Mechanical Keyboard C33F",
    ),
    (
        0xc355,
        &[0x1004, 0x8040, 0x8071, 0x8081],
        "Solaar G515 LS TKL 40B4; OpenRGB LogitechHIDPP20Controller.h pairs 40B4/C355",
    ),
    (
        0xc35b,
        &[0x8081],
        "OpenRGB LogitechHIDPP20 known_kb_layout_edits C35B (0x8081 per-key branch)",
    ),
    (
        0xc339,
        &[0x8070, 0x8080],
        "OpenRGB known_kb_8080_strips C339 (0x8070 boards with 0x8080); Solaar descriptors.py C339",
    ),
    (
        0xc33c,
        &[0x8070, 0x8080],
        "OpenRGB known_kb_8080_strips C33C; Solaar descriptors.py G512 C33C; libratbag g513",
    ),
    (0xc342, &[0x8070, 0x8080], "OpenRGB known_kb_8080_strips C342"),
    (
        0xc336,
        &[0x8060, 0x8070],
        "Solaar G213 Prodigy Gaming Keyboard (USB id C336)",
    ),
    (0xc337, &[0x8070, 0x8080], "OpenRGB known_kb_8080_strips C337"),
    (0xc331, &[0x8070, 0x8080], "OpenRGB known_kb_8080_strips C331"),
    (0xc338, &[0x8070, 0x8080], "OpenRGB known_kb_8080_strips C338"),
    (0xc333, &[0x8070, 0x8080], "OpenRGB known_kb_8080_strips C333"),
    (
        0xc335,
        &[0x8070, 0x8080],
        "OpenRGB known_kb_8080_strips C335; libratbag g910",
    ),
    (0xc32b, &[0x8070, 0x8080], "OpenRGB known_kb_8080_strips C32B"),
    (
        0x0afb,
        &[0x1f20, 0x8300],
        "HeadsetControl logitech_gpro.hpp (idx 6 battery, idx 5 sidetone = Solaar 0ABA layout)",
    ),
    (0x0afc, &[0x1f20, 0x8300], "HeadsetControl logitech_gpro.hpp"),
    (
        0x0aba,
        &[0x1f20, 0x8300],
        "Solaar PRO X Wireless Gaming Headset 0ABA",
    ),
    (0x0aaa, &[0x1f20, 0x8300], "HeadsetControl logitech_gpro.hpp"),
    (0x0aa7, &[0x1f20, 0x8300], "HeadsetControl logitech_gpro.hpp"),
    (
        0x0ab5,
        &[0x1f20, 0x8070, 0x8300],
        "Solaar G733 Gaming Headset 0AB5",
    ),
    (
        0x0afe,
        &[0x1f20, 0x8070, 0x8300],
        "Solaar G733 Gaming Headset 0AFE",
    ),
    (
        0x0b1f,
        &[0x1f20, 0x8070, 0x8300],
        "HeadsetControl logitech_g633_g933_935.hpp (idx 8/7/4 = Solaar G733 0AB5 layout)",
    ),
    (
        0x0ac4,
        &[0x1f20, 0x8300],
        "Solaar G535 Wireless Gaming Headset 0AC4",
    ),
    (
        0x0a87,
        &[0x1f20, 0x8070, 0x8300],
        "Solaar G935 Gaming Headset 0A87",
    ),
    (
        0x0a5b,
        &[0x1f20, 0x8070, 0x8300],
        "Solaar G933 Gaming Wireless Headset 0A5B",
    ),
    (
        0x0a89,
        &[0x1f20, 0x8070, 0x8300],
        "HeadsetControl logitech_g633_g933_935.hpp (G933 family layout)",
    ),
    (
        0x0a5c,
        &[0x1f20, 0x8070, 0x8300],
        "HeadsetControl logitech_g633_g933_935.hpp (G933 family layout)",
    ),
    (
        0x0a9c,
        &[0x8300],
        "HeadsetControl logitech_g432.hpp (sidetone = 0x8300 fn 1, 0..100)",
    ),
    (0x0a9b, &[0x8300], "HeadsetControl logitech_g432.hpp"),
    (0x0a6d, &[0x8300], "HeadsetControl logitech_g432.hpp"),
];

/// Receivers whose paired devices this driver opens through the receiver
/// (the hid-logitech-dj children of these are theirs).
fn serves_slots(pid: u16) -> bool {
    pid == pro2::RX_PID
        || (pid != EX100
            && MODELS
                .iter()
                .any(|m| m.kind == DeviceKind::Other && m.product_ids.contains(&pid)))
}

/// Claims an unlisted Logitech VID:PID that speaks HID++ 2.0 (sysfs only).
pub fn fallback(nodes: &[&HidDeviceInfo]) -> Option<Fallback> {
    let first = nodes.first()?;
    if first.vendor_id != VID
        || !nodes
            .iter()
            .any(|n| crate::hid::report_ids(&n.report_descriptor()).contains(&0x11))
    {
        return None;
    }
    // An unknown receiver: device index 0xff would talk to the receiver.
    if nodes
        .iter()
        .any(|n| hid_driver(n).as_deref() == Some("logitech-djreceiver"))
    {
        return None;
    }
    let name = match first.name.trim() {
        "" => format!("Logitech device {:04x}", first.product_id),
        n => n.to_string(),
    };
    // A device paired to a receiver also shows up as its own HID device
    // under the receiver's; the receiver's slot adapter already serves it.
    // Claiming it keeps the generic input driver from listing it twice.
    // ponytail: a slot whose pairing register cannot be read (old Nano
    // receivers) gets no adapter at all; open it through the child if needed.
    if nodes
        .iter()
        .any(|n| receiver_parent(n).is_some_and(serves_slots))
    {
        return Some(Fallback {
            kind: DeviceKind::Other,
            name,
            open: |_, _| Ok(Vec::new()),
        });
    }
    Some(Fallback {
        kind: guess_kind(nodes),
        name,
        open: open_unlisted,
    })
}

/// Name of the kernel driver bound to a HID device (sysfs `driver` link).
fn hid_driver(n: &HidDeviceInfo) -> Option<String> {
    let link = std::fs::read_link(n.sys.join("driver")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// PID of the Logitech receiver whose HID device is this one's parent
/// (`.../0003:046D:C52B.0003/0003:046D:4082.0004`).
fn receiver_parent(n: &HidDeviceInfo) -> Option<u16> {
    let dir = n.sys.parent()?.file_name()?.to_str()?;
    parse_hid_dir(dir).and_then(|(vid, pid)| (vid == VID).then_some(pid))
}

/// (VID, PID) from a HID device directory name `BBBB:VVVV:PPPP.NNNN`.
fn parse_hid_dir(dir: &str) -> Option<(u16, u16)> {
    let mut it = dir.split(':');
    let (_bus, vid, rest) = (it.next()?, it.next()?, it.next()?);
    let (pid, _) = rest.split_once('.')?;
    Some((
        u16::from_str_radix(vid, 16).ok()?,
        u16::from_str_radix(pid, 16).ok()?,
    ))
}

/// Kind until DEVICE_NAME answers: an audio interface on the same USB device
/// means a headset; else the first application collection of the lowest
/// interface (Generic Desktop mouse 0x02 / keyboard 0x06).
fn guess_kind(nodes: &[&HidDeviceInfo]) -> DeviceKind {
    if nodes
        .iter()
        .any(|n| n.bus == BUS_USB && has_audio_interface(&n.sys))
    {
        return DeviceKind::Headset;
    }
    let Some(lowest) = nodes.iter().min_by_key(|n| n.interface.unwrap_or(u32::MAX)) else {
        return DeviceKind::Other;
    };
    match first_application(&lowest.report_descriptor()) {
        Some((0x01, 0x02)) => DeviceKind::Mouse,
        Some((0x01, 0x06)) => DeviceKind::Keyboard,
        _ => DeviceKind::Other,
    }
}

/// (usage page, usage) of the first application collection.
fn first_application(desc: &[u8]) -> Option<(u32, u32)> {
    let (mut page, mut usage) = (0, None);
    for (tag, v) in crate::hid::descriptor_items(desc) {
        match tag {
            0x04 => page = v,
            // A 4-byte usage carries its own page.
            0x08 if v > 0xffff => usage = Some((v >> 16, v & 0xffff)),
            0x08 => usage = Some((page, v)),
            0xa0 if v == 1 => return usage,
            _ => {}
        }
    }
    None
}

/// Any interface of the USB device holding `hid_sys` is audio class (0x01).
fn has_audio_interface(hid_sys: &Path) -> bool {
    let Some(usb) = hid_sys.parent().and_then(Path::parent) else {
        return false;
    };
    let Ok(dir) = std::fs::read_dir(usb) else {
        return false;
    };
    dir.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("bInterfaceClass")).is_ok_and(|c| c.trim() == "01")
    })
}

/// hidraw nodes of `pid` that declare HID++ long reports (0x11), lowest
/// interface first, with whether short reports (0x10) exist too.
fn hidpp_nodes(pid: u16) -> Result<Vec<(HidDeviceInfo, bool)>> {
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == VID && d.product_id == pid)
        .filter_map(|d| {
            let name = d.path.file_name()?.to_owned();
            let desc = std::fs::read(
                Path::new("/sys/class/hidraw")
                    .join(name)
                    .join("device/report_descriptor"),
            )
            .ok()?;
            let ids = crate::hid::report_ids(&desc);
            ids.contains(&0x11).then(|| (d, ids.contains(&0x10)))
        })
        .collect();
    nodes.sort_by_key(|(d, _)| d.interface);
    Ok(nodes)
}

/// The generic adapter on `pid`'s first HID++ node, device index 0xff.
fn open_hidpp(pid: u16, ctx: &DriverCtx, kind: DeviceKind, name: Option<&str>) -> Opened {
    let (info, _) = hidpp_nodes(pid)?
        .into_iter()
        .next()
        .context("no HID++ interface")?;
    let dev = HidrawDevice::open(&info.path)?;
    let a = HidppAdapter::new(ctx.paths.clone(), dev, Link::Direct, pid, kind, name)?;
    Ok(vec![Box::new(a) as Box<dyn DeviceAdapter>])
}

fn open_direct(pid: u16, ctx: &DriverCtx) -> Opened {
    let model = MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .context("unknown model")?;
    open_hidpp(pid, ctx, model.kind, Some(model.name))
}

/// An unlisted device: kind guessed from sysfs until DEVICE_NAME answers.
fn open_unlisted(pid: u16, ctx: &DriverCtx) -> Opened {
    let nodes: Vec<HidDeviceInfo> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == VID && d.product_id == pid)
        .collect();
    let kind = guess_kind(&nodes.iter().collect::<Vec<_>>());
    open_hidpp(pid, ctx, kind, None)
}

fn open_receiver(pid: u16, ctx: &DriverCtx) -> Opened {
    receiver_slots(pid, ctx, |_, _| false)
}

/// One adapter per paired slot (asleep devices included: they are pinged
/// until they answer). Skipped: devices in `ctx.open`, slots an open adapter
/// still holds (a replaced device waits until the old adapter lets go),
/// wireless PIDs another model claims, and whatever `skip(slot, wpid)` says.
/// A receiver without a HID++ long-report node has nothing to offer.
fn receiver_slots(pid: u16, ctx: &DriverCtx, skip: impl Fn(u8, u16) -> bool) -> Opened {
    let Some((info, short)) = hidpp_nodes(pid)?.into_iter().next() else {
        debug!("receiver {pid:04x}: no HID++ long-report node");
        return Ok(Vec::new());
    };
    let bolt = pid == BOLT;
    let mut rx = HidrawDevice::open(&info.path)?;
    let mut out: Vec<Box<dyn DeviceAdapter>> = Vec::new();
    for slot in 1..=6u8 {
        if slot_held(&ctx.open, slot) {
            continue;
        }
        let sub = hidpp::pairing_sub(bolt, slot);
        let Ok(r) = hidpp::read_long_register(&mut rx, hidpp::REG_RECEIVER_INFO, sub, short) else {
            continue;
        };
        let Some((wpid, kind)) = hidpp::parse_pairing(bolt, &r) else {
            continue;
        };
        if skip(slot, wpid)
            || crate::drivers::models().any(|m| m.vendor_id == VID && m.product_ids.contains(&wpid))
        {
            continue;
        }
        let dev = HidrawDevice::open(&info.path)?;
        out.push(Box::new(HidppAdapter::new(
            ctx.paths.clone(),
            dev,
            Link::Slot { slot, bolt, short },
            wpid,
            hidpp::kind_from_hidpp10(kind),
            None,
        )?));
    }
    Ok(out)
}

/// An open adapter already serves receiver slot `slot`.
fn slot_held(open: &[hako_core::device::DeviceId], slot: u8) -> bool {
    let instance = format!("hidpp:{slot}");
    open.iter().any(|id| id.instance == instance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::device::DeviceId;

    /// Features the generic adapter turns into a control or a battery.
    const USED: &[u16] = &[
        0x1000, 0x1001, 0x1004, 0x1f20, 0x2201, 0x2202, 0x8040, 0x8060, 0x8061, 0x8070, 0x8071,
        0x8081, 0x8300,
    ];

    /// PIDs served by a module with its own protocol, not the HID++ adapter.
    fn own_protocol(pid: u16) -> bool {
        centurion::spec(pid).is_some() || g930::PIDS.contains(&pid) || zone::PIDS.contains(&pid)
    }

    #[test]
    fn model_table_is_consistent() {
        let mut pids = std::collections::HashSet::new();
        for m in MODELS {
            assert_eq!(m.vendor_id, VID);
            assert!(!m.usbfs, "{} must not need usbfs", m.name);
            for &pid in m.product_ids {
                assert!(pids.insert(pid), "{pid:04x} listed twice");
            }
        }
        // PRO 2 entry preserved: same PIDs, same order (first names its remap file).
        assert_eq!(MODELS[0].product_ids, &[0x40a8, 0xc543]);
        assert_eq!(MODELS[0].name, "Logitech PRO 2 LIGHTSPEED");
        assert_eq!(MODELS[0].kind, DeviceKind::Mouse);
        // Receivers (PRO 2 included, for its other slots) are rescanned.
        assert!(MODELS[0].multi);
        for m in MODELS.iter().filter(|m| m.kind == DeviceKind::Other) {
            assert!(m.multi, "{}", m.name);
        }
        for m in MODELS[1..].iter().filter(|m| m.kind != DeviceKind::Other) {
            assert!(!m.multi, "{}", m.name);
        }
    }

    #[test]
    fn every_direct_model_has_a_control_or_battery() {
        let direct: Vec<_> = MODELS[1..]
            .iter()
            .filter(|m| m.kind != DeviceKind::Other && !own_protocol(m.product_ids[0]))
            .collect();
        assert_eq!(direct.len(), DOCUMENTED.len());
        for m in direct {
            let pid = m.product_ids[0];
            let (_, feats, src) = DOCUMENTED
                .iter()
                .find(|(p, _, _)| *p == pid)
                .unwrap_or_else(|| panic!("{} ({pid:04x}) has no documented features", m.name));
            assert!(!src.is_empty());
            assert!(
                feats.iter().any(|f| USED.contains(f)),
                "{} exposes nothing Hako uses",
                m.name
            );
        }
    }

    #[test]
    fn own_protocol_headsets_are_listed() {
        let own: Vec<u16> = MODELS
            .iter()
            .flat_map(|m| m.product_ids.iter().copied())
            .filter(|&p| own_protocol(p))
            .collect();
        assert_eq!(own, [0x0af7, 0x0b45, 0x0b18, 0x0a1f, 0x0aad, 0x0ade]);
    }

    #[test]
    fn receivers_are_known_hidpp_receivers() {
        let receivers: Vec<u16> = MODELS
            .iter()
            .filter(|m| m.kind == DeviceKind::Other)
            .map(|m| m.product_ids[0])
            .collect();
        // Every KNOWN_RECEIVERS entry with VID 046d in Solaar base_usb.py.
        assert_eq!(
            receivers,
            [
                0xc548, 0xc52b, 0xc532, 0xc539, 0xc53a, 0xc53d, 0xc53f, 0xc541, 0xc545, 0xc547,
                0xc54d, 0xc52f, 0xc518, 0xc51a, 0xc51b, 0xc521, 0xc525, 0xc526, 0xc52e, 0xc531,
                0xc534, 0xc535, 0xc537, 0xc517
            ]
        );
        assert!(serves_slots(0xc52b) && serves_slots(0xc543));
        assert!(!serves_slots(EX100) && !serves_slots(0xc08b));
    }

    #[test]
    fn receiver_children_are_recognised() {
        assert_eq!(parse_hid_dir("0003:046D:C52B.0003"), Some((0x046d, 0xc52b)));
        assert_eq!(parse_hid_dir("1-2:1.2"), None);
        assert_eq!(parse_hid_dir("usb2"), None);
        let child = HidDeviceInfo {
            path: "/dev/hidraw9".into(),
            vendor_id: VID,
            product_id: 0x4082,
            interface: Some(2),
            uniq: String::new(),
            bus: BUS_USB,
            name: "Logitech Wireless Mouse PID:4082".into(),
            sys: "/sys/devices/pci0000:00/usb1/1-2/1-2:1.2/0003:046D:C52B.0003/0003:046D:4082.0004"
                .into(),
        };
        assert_eq!(receiver_parent(&child), Some(0xc52b));
        let direct = HidDeviceInfo {
            sys: "/sys/devices/pci0000:00/usb1/1-2/1-2:1.1/0003:046D:C08B.0005".into(),
            ..child
        };
        assert_eq!(receiver_parent(&direct), None);
    }

    #[test]
    fn kind_from_first_application_collection() {
        // Usage Page (Generic Desktop), Usage (Mouse), Collection (Application).
        assert_eq!(
            first_application(&[0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00]),
            Some((0x01, 0x02))
        );
        // Keyboard, after a physical collection that does not count.
        assert_eq!(
            first_application(&[0x05, 0x01, 0xa1, 0x00, 0xc0, 0x09, 0x06, 0xa1, 0x01]),
            Some((0x01, 0x06))
        );
        // Vendor page 0xff43 with a 4-byte usage.
        assert_eq!(
            first_application(&[0x0b, 0x02, 0x02, 0x43, 0xff, 0xa1, 0x01]),
            Some((0xff43, 0x0202))
        );
        assert_eq!(first_application(&[]), None);
    }

    #[test]
    fn rescans_skip_slots_still_held() {
        let open = [DeviceId {
            vendor_id: VID,
            product_id: 0x4082,
            instance: "hidpp:2".into(),
        }];
        assert!(slot_held(&open, 2));
        assert!(!slot_held(&open, 1));
        assert!(!slot_held(&[], 2));
    }

    #[test]
    #[ignore = "needs a Logitech HID++ device plugged in"]
    fn hardware_lists_hidpp_nodes() {
        for m in MODELS {
            for &pid in m.product_ids {
                let nodes = hidpp_nodes(pid).unwrap();
                if !nodes.is_empty() {
                    println!("{} {pid:04x}: {:?}", m.name, nodes);
                }
            }
        }
    }

    /// Opens the PRO 2 through its receiver model exactly as discovery does
    /// (reads, plus the saved-settings restore the daemon also performs).
    #[test]
    #[ignore = "needs the PRO 2 LIGHTSPEED receiver 046d:c543 plugged in"]
    fn hardware_pro2_receiver_opens() {
        let ctx = DriverCtx {
            paths: hako_core::config::ConfigPaths::xdg().unwrap(),
            audio: std::sync::Arc::new(hako_audio::AudioService::new()),
            open: Vec::new(),
            vendor_id: VID,
        };
        let adapters = (pro2::MODEL.open)(pro2::RX_PID, &ctx).unwrap();
        for a in &adapters {
            let s = a.snapshot();
            println!(
                "{} {:?} connected={} batteries={:?} controls={} err={:?}",
                s.id.key(),
                s.name,
                s.connected,
                s.batteries,
                s.controls.len(),
                s.last_error
            );
        }
        let pro2 = adapters[0].snapshot();
        assert_eq!(pro2.name, "Logitech PRO 2 LIGHTSPEED");
        assert!(pro2.connected);
    }
}
