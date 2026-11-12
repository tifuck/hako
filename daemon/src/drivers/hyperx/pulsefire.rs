//! Pulsefire Surge, FPS Pro, Core, Raid and the original Pulsefire Haste.
//!
//! Layouts and IDs from OpenRGB `Controllers/HyperXMouseController/`
//! (`HyperXMouseControllerDetect.cpp`, `HyperXPulsefire{Surge,FPSPro,Raid,Haste}Controller`).
//! Their only documented lighting path is "direct" mode, which OpenRGB
//! re-sends every 50 ms (1 s on the Raid); `direct.rs` streams it. No public
//! source documents a volatile DPI or report-rate path for these mice, so
//! those stay on the mouse's own buttons.

use super::direct::{self, Frames};
use super::{pad, Node, Spec, HP, KINGSTON};
use crate::drivers::{DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::Result;
use hako_core::device::DeviceKind;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    /// 264-byte feature report 7: `07 14 00 a0`, 32 R / G / B planes at
    /// 0x08 / 0x28 / 0x48 (LED strip), logo RGB at 0x6c.
    Surge,
    /// 264-byte feature report 7: `07 0a R G B 00 00 00 a0` (logo).
    FpsPro,
    /// 264-byte feature report 7: `07 0a` + scroll wheel RGB + logo RGB + `a0`.
    Raid,
    /// 65-byte feature reports, id 0: `04 f2 .. [8] 02`, then `81 R G B .. [8] 02`.
    Haste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stream {
    layout: Layout,
    node: Node,
}

/// Interface 1, usage page ff01.
const IFACE_1: Node = Node::Iface(1);

const fn spec(
    name: &'static str,
    vid: u16,
    pid: &'static [u16],
    layout: Layout,
    node: Node,
) -> Spec<Stream> {
    Spec {
        name,
        vid,
        pids: pid,
        family: Stream { layout, node },
    }
}

pub(super) const SURGE: Spec<Stream> = spec(
    "HyperX Pulsefire Surge",
    KINGSTON,
    &[0x16d3],
    Layout::Surge,
    IFACE_1,
);
pub(super) const SURGE_HP: Spec<Stream> = spec(
    "HyperX Pulsefire Surge",
    HP,
    &[0x0490],
    Layout::Surge,
    IFACE_1,
);
pub(super) const FPS_PRO: Spec<Stream> = spec(
    "HyperX Pulsefire FPS Pro",
    KINGSTON,
    &[0x16d7],
    Layout::FpsPro,
    IFACE_1,
);
pub(super) const CORE: Spec<Stream> = spec(
    "HyperX Pulsefire Core",
    KINGSTON,
    &[0x16de],
    Layout::FpsPro,
    IFACE_1,
);
pub(super) const CORE_HP: Spec<Stream> = spec(
    "HyperX Pulsefire Core",
    HP,
    &[0x0d8f],
    Layout::FpsPro,
    IFACE_1,
);
pub(super) const RAID: Spec<Stream> = spec(
    "HyperX Pulsefire Raid",
    KINGSTON,
    &[0x16e4],
    Layout::Raid,
    IFACE_1,
);
/// Linux detector: the generic-desktop mouse collection (page 1, usage 2).
pub(super) const HASTE: Spec<Stream> = spec(
    "HyperX Pulsefire Haste",
    KINGSTON,
    &[0x1727],
    Layout::Haste,
    Node::Usage(0x0001, 0x0002),
);
/// Linux detector: interface 0.
pub(super) const HASTE_HP: Spec<Stream> = spec(
    "HyperX Pulsefire Haste",
    HP,
    &[0x0f8f],
    Layout::Haste,
    Node::Iface(0),
);

#[cfg(test)]
pub(super) const ALL: &[&Spec<Stream>] = &[
    &SURGE, &SURGE_HP, &FPS_PRO, &CORE, &CORE_HP, &RAID, &HASTE, &HASTE_HP,
];

pub(super) fn open(spec: &'static Spec<Stream>, pid: u16, ctx: &DriverCtx) -> Opened {
    direct::open(spec, DeviceKind::Mouse, spec.family.node, pid, ctx)
}

fn frames(layout: Layout, c: [u8; 3]) -> Vec<Vec<u8>> {
    match layout {
        Layout::Surge => {
            let mut p = pad(&[0x07, 0x14, 0x00, 0xa0], 264);
            for (&plane, &v) in [0x08, 0x28, 0x48].iter().zip(&c) {
                p[plane..plane + 32].fill(v);
            }
            p[0x6c..0x6f].copy_from_slice(&c);
            vec![p]
        }
        Layout::FpsPro => {
            let mut p = pad(&[0x07, 0x0a], 264);
            p[2..5].copy_from_slice(&c);
            p[8] = 0xa0;
            vec![p]
        }
        Layout::Raid => {
            let mut p = pad(&[0x07, 0x0a], 264);
            p[2..5].copy_from_slice(&c);
            p[5..8].copy_from_slice(&c);
            p[8] = 0xa0;
            vec![p]
        }
        Layout::Haste => {
            let mut setup = pad(&[0x00, 0x04, 0xf2], 65);
            setup[8] = 0x02;
            let mut color = pad(&[0x00, 0x81, c[0], c[1], c[2]], 65);
            color[8] = 0x02;
            vec![setup, color]
        }
    }
}

impl Frames for Stream {
    fn send(self, dev: &mut HidrawDevice, color: [u8; 3]) -> Result<()> {
        for mut p in frames(self.layout, color) {
            dev.set_feature(&mut p)?;
        }
        Ok(())
    }

    fn keepalive(self) -> Duration {
        match self.layout {
            Layout::Raid => Duration::from_secs(1),
            _ => Duration::from_millis(50),
        }
    }

    /// OpenRGB's Surge controller defines profile writes (`07 03 <profile> 01
    /// 01 01 <brightness>`, `07 07 <profile>`) but never calls them and lists
    /// the mouse as "save ✗".
    fn memory_blocked(self) -> Option<&'static str> {
        (self.layout == Layout::Surge).then_some(
            "single unused source: OpenRGB defines Surge profile writes (07 03) but never sends them",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_match_openrgb_layouts() {
        let c = [0x11, 0x22, 0x33];
        let s = &frames(Layout::Surge, c)[0];
        assert_eq!(s.len(), 264);
        assert_eq!(s[..9], [0x07, 0x14, 0x00, 0xa0, 0, 0, 0, 0, 0x11]);
        assert_eq!(
            (s[0x27], s[0x28], s[0x47], s[0x48], s[0x67]),
            (0x11, 0x22, 0x22, 0x33, 0x33)
        );
        assert_eq!(s[0x68..0x70], [0, 0, 0, 0, 0x11, 0x22, 0x33, 0]);

        let f = &frames(Layout::FpsPro, c)[0];
        assert_eq!(f[..10], [0x07, 0x0a, 0x11, 0x22, 0x33, 0, 0, 0, 0xa0, 0]);
        assert_eq!(f.len(), 264);

        let r = &frames(Layout::Raid, c)[0];
        assert_eq!(
            r[..10],
            [0x07, 0x0a, 0x11, 0x22, 0x33, 0x11, 0x22, 0x33, 0xa0, 0]
        );

        let h = frames(Layout::Haste, c);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0][..9], [0x00, 0x04, 0xf2, 0, 0, 0, 0, 0, 0x02]);
        assert_eq!(h[1][..9], [0x00, 0x81, 0x11, 0x22, 0x33, 0, 0, 0, 0x02]);
        assert!(h.iter().all(|p| p.len() == 65));
    }

    #[test]
    fn keepalive_per_layout() {
        assert_eq!(RAID.family.keepalive(), Duration::from_secs(1));
        assert_eq!(SURGE.family.keepalive(), Duration::from_millis(50));
        assert_eq!(HASTE.family.node, Node::Usage(1, 2));
        assert!(SURGE.family.memory_blocked().is_some());
        assert!(SURGE_HP.family.memory_blocked().is_some());
        assert!(RAID.family.memory_blocked().is_none());
        assert!(super::super::keyboard::ELITE_RGB
            .family
            .memory_blocked()
            .is_some());
        assert!(super::super::keyboard::FPS_RGB
            .family
            .memory_blocked()
            .is_none());
    }
}
