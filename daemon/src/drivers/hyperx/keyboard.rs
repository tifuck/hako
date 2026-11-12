//! HyperX keyboards: one host-driven colour across the whole board.
//!
//! Layouts from OpenRGB `Controllers/HyperXKeyboardController/*`:
//! `HyperXAlloyOriginsController`, `HyperXAlloyOrigins60and65Controller`,
//! `HyperXAlloyOriginsCoreController`, `HyperXAlloyElite2Controller`,
//! `HyperXAlloyEliteController`, `HyperXAlloyFPSController`,
//! `HyperXOrigins2_65Controller` and `HyperXEve1800Controller` (the `*Detect.cpp`
//! files give the IDs and the Linux interfaces). Only their "direct" mode is
//! used: the firmware drops it shortly after the last frame and falls back to
//! its onboard profile, so `direct.rs` keeps re-sending it. The Alloy Elite's
//! other OpenRGB modes write the keyboard's profile: a blocked placeholder.

use super::direct::{self, Frames};
use super::{pad, Node, Spec, HP, KINGSTON};
use crate::drivers::{DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::Result;
use hako_core::device::DeviceKind;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// Feature reports (interface 0 on Linux): init `04 f2 .. [9] = init`,
    /// then `packets` reports of 16 × `81 R G B`.
    Origins {
        packets: u8,
        /// Byte 9 of the init report (the Elite 2 leaves it 0).
        init: u8,
        /// Slots with no LED, kept dark.
        skip: &'static [usize],
        keepalive_ms: u64,
    },
    /// Output reports on interface 2: `a2 <seq> 00 <len>` + chunks of a
    /// 380-byte G/R/B buffer.
    OriginsCore,
    /// 264-byte feature report 7 on interface 2, one per colour channel:
    /// `07 16 <channel 1-3> a0` + the channel's value at each key offset; the
    /// Alloy Elite adds channel 4 for its media keys and light bar.
    Elite { extended: bool },
    /// 65-byte output report 0x44: `44 01 04 00`, then `packets` ×
    /// `44 02 <seq> 00` + up to 20 RGB triplets.
    Lamp {
        iface: u32,
        leds: usize,
        packets: u8,
    },
}

const ORIGINS_SKIP: &[usize] = &[
    23, 29, 41, 47, 59, 70, 71, 87, 88, 93, 99, 100, 102, 108, 113, 114, 120, 123, 124,
];
const ELITE_2_SKIP: &[usize] = &[
    23, 29, 41, 47, 70, 71, 76, 77, 87, 88, 93, 99, 100, 102, 108, 113,
];
const FULL: Family = Family::Origins {
    packets: 9,
    init: 9,
    skip: ORIGINS_SKIP,
    keepalive_ms: 1000,
};
const COMPACT: Family = Family::Origins {
    packets: 5,
    init: 5,
    skip: &[],
    keepalive_ms: 1000,
};
/// OpenRGB's Elite 2 keepalive repeats the frame once a second.
const ELITE_2: Family = Family::Origins {
    packets: 9,
    init: 0,
    skip: ELITE_2_SKIP,
    keepalive_ms: 1000,
};

pub(super) const ORIGINS: Spec<Family> = Spec {
    name: "HyperX Alloy Origins",
    vid: KINGSTON,
    pids: &[0x16e5],
    family: FULL,
};
pub(super) const ORIGINS_HP: Spec<Family> = Spec {
    name: "HyperX Alloy Origins",
    vid: HP,
    pids: &[0x0591],
    family: FULL,
};
pub(super) const ORIGINS_CORE: Spec<Family> = Spec {
    name: "HyperX Alloy Origins Core",
    vid: KINGSTON,
    pids: &[0x16e6],
    family: Family::OriginsCore,
};
pub(super) const ORIGINS_CORE_HP: Spec<Family> = Spec {
    name: "HyperX Alloy Origins Core",
    vid: HP,
    pids: &[0x098f],
    family: Family::OriginsCore,
};
pub(super) const ORIGINS_60: Spec<Family> = Spec {
    name: "HyperX Alloy Origins 60",
    vid: KINGSTON,
    pids: &[0x1734],
    family: COMPACT,
};
pub(super) const ORIGINS_60_HP: Spec<Family> = Spec {
    name: "HyperX Alloy Origins 60",
    vid: HP,
    pids: &[0x0c8e],
    family: COMPACT,
};
pub(super) const ORIGINS_65_HP: Spec<Family> = Spec {
    name: "HyperX Alloy Origins 65",
    vid: HP,
    pids: &[0x038f],
    family: COMPACT,
};
pub(super) const ELITE_2_KB: Spec<Family> = Spec {
    name: "HyperX Alloy Elite 2",
    vid: KINGSTON,
    pids: &[0x1711],
    family: ELITE_2,
};
pub(super) const ELITE_2_HP: Spec<Family> = Spec {
    name: "HyperX Alloy Elite 2",
    vid: HP,
    pids: &[0x058f],
    family: ELITE_2,
};
pub(super) const ELITE_RGB: Spec<Family> = Spec {
    name: "HyperX Alloy Elite RGB",
    vid: KINGSTON,
    pids: &[0x16be],
    family: Family::Elite { extended: true },
};
pub(super) const FPS_RGB: Spec<Family> = Spec {
    name: "HyperX Alloy FPS RGB",
    vid: KINGSTON,
    pids: &[0x16dc],
    family: Family::Elite { extended: false },
};
pub(super) const ORIGINS_2_65: Spec<Family> = Spec {
    name: "HyperX Alloy Origins 2 65",
    vid: HP,
    pids: &[0x0cc2],
    family: Family::Lamp {
        iface: 3,
        leds: 74,
        packets: 4,
    },
};
pub(super) const EVE_1800: Spec<Family> = Spec {
    name: "HyperX Eve 1800",
    vid: HP,
    pids: &[0x08c2],
    family: Family::Lamp {
        iface: 2,
        leds: 10,
        packets: 1,
    },
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[
    &ORIGINS,
    &ORIGINS_HP,
    &ORIGINS_CORE,
    &ORIGINS_CORE_HP,
    &ORIGINS_60,
    &ORIGINS_60_HP,
    &ORIGINS_65_HP,
    &ELITE_2_KB,
    &ELITE_2_HP,
    &ELITE_RGB,
    &FPS_RGB,
    &ORIGINS_2_65,
    &EVE_1800,
];

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, ctx: &DriverCtx) -> Opened {
    let iface = match spec.family {
        Family::Origins { .. } => 0,
        Family::OriginsCore | Family::Elite { .. } => 2,
        Family::Lamp { iface, .. } => iface,
    };
    direct::open(spec, DeviceKind::Keyboard, Node::Iface(iface), pid, ctx)
}

impl Frames for Family {
    fn send(self, dev: &mut HidrawDevice, color: [u8; 3]) -> Result<()> {
        match self {
            Family::Origins {
                packets,
                init,
                skip,
                ..
            } => {
                for mut p in origins_frames(packets, init, skip, color) {
                    dev.set_feature(&mut p)?;
                }
            }
            Family::OriginsCore => {
                for p in core_frames(color) {
                    dev.write(&p)?;
                }
            }
            Family::Elite { extended } => {
                // OpenRGB waits 5 ms (Elite) / 10 ms (FPS) between channels.
                let gap = Duration::from_millis(if extended { 5 } else { 10 });
                for (i, mut p) in elite_frames(extended, color).into_iter().enumerate() {
                    if i > 0 {
                        std::thread::sleep(gap);
                    }
                    dev.set_feature(&mut p)?;
                }
            }
            Family::Lamp { leds, packets, .. } => {
                for p in lamp_frames(leds, packets, color) {
                    dev.write(&p)?;
                }
            }
        }
        Ok(())
    }

    fn keepalive(self) -> Duration {
        match self {
            Family::Origins { keepalive_ms, .. } => Duration::from_millis(keepalive_ms),
            Family::OriginsCore => Duration::from_secs(1),
            Family::Elite { .. } | Family::Lamp { .. } => Duration::from_millis(50),
        }
    }

    /// OpenRGB `HyperXAlloyEliteController` writes the Alloy Elite's profile
    /// effects (`07 02 <profile> ..`, `07 01 <profile>`); nothing else
    /// documents them and OpenRGB lists the board as "save ✗".
    fn memory_blocked(self) -> Option<&'static str> {
        matches!(self, Family::Elite { extended: true }).then_some(
            "single source: only OpenRGB writes the onboard profile effects (07 02), \
             unverified by any second project",
        )
    }
}

/// Init report plus `packets` color reports (65 bytes, report id 0).
fn origins_frames(packets: u8, init: u8, skip: &[usize], color: [u8; 3]) -> Vec<[u8; 65]> {
    let mut first = [0u8; 65];
    first[1..3].copy_from_slice(&[0x04, 0xf2]);
    first[9] = init;
    let mut out = vec![first];
    for k in 0..usize::from(packets) {
        let mut p = [0u8; 65];
        for j in 0..16 {
            let c = if skip.contains(&(k * 16 + j)) {
                [0; 3]
            } else {
                color
            };
            p[1 + 4 * j] = 0x81;
            p[2 + 4 * j..5 + 4 * j].copy_from_slice(&c);
        }
        out.push(p);
    }
    out
}

/// LED index i of 94 lands at segment (i/12)*48 + sector ((i/6)&1)*8 + i%6,
/// with G at +0, R at +16 and B at +32; sent in 60-byte chunks.
fn core_frames([r, g, b]: [u8; 3]) -> Vec<Vec<u8>> {
    let mut buf = [0u8; 380];
    for i in 0..94 {
        let pos = (i / 12) * 48 + ((i / 6) & 1) * 8 + i % 6;
        buf[pos] = g;
        buf[pos + 16] = r;
        buf[pos + 32] = b;
    }
    buf.chunks(60)
        .enumerate()
        .map(|(seq, chunk)| {
            let mut p = vec![0x00, 0xa2, seq as u8, 0x00, chunk.len() as u8];
            p.extend_from_slice(chunk);
            p
        })
        .collect()
}

/// Offsets of the 106 main keys in an Elite / FPS channel report.
const ELITE_KEYS: [usize; 106] = [
    0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
    0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x20, 0x21, 0x22, 0x23, 0x24, 0x26, 0x27, 0x28, 0x29,
    0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x37, 0x38, 0x39, 0x3a, 0x3b,
    0x3c, 0x3e, 0x3f, 0x41, 0x44, 0x45, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x51, 0x54,
    0x55, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5e, 0x5f, 0x61, 0x64, 0x65, 0x68, 0x69, 0x6a, 0x6b, 0x6c,
    0x6e, 0x6f, 0x74, 0x75, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x81, 0x84, 0x85, 0x88,
    0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x91, 0x94, 0x95,
];
/// Alloy Elite channel 4: R, G and B offsets of its 22 extra LEDs.
const ELITE_EXT: [[usize; 22]; 3] = [
    [
        0x08, 0x48, 0x88, 0x09, 0x89, 0x0a, 0x8a, 0x0b, 0x8b, 0x0c, 0x8c, 0x0d, 0x8d, 0x0e, 0x8f,
        0x8e, 0x0f, 0x4f, 0x92, 0x13, 0x93, 0x12,
    ],
    [
        0x29, 0x28, 0x78, 0x19, 0x79, 0x1a, 0x7a, 0x1b, 0x7b, 0x1c, 0x7c, 0x1d, 0x7d, 0x1e, 0x6e,
        0x7e, 0x1f, 0x6f, 0x82, 0x23, 0x83, 0x22,
    ],
    [
        0x39, 0x38, 0x68, 0x3a, 0x69, 0x2a, 0x6a, 0x2b, 0x6b, 0x2c, 0x6c, 0x2d, 0x6d, 0x2e, 0x5e,
        0x5d, 0x2f, 0x5f, 0x72, 0x33, 0x73, 0x32,
    ],
];

fn elite_frames(extended: bool, color: [u8; 3]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = (0..3)
        .map(|ch| {
            let mut p = pad(&[0x07, 0x16, ch as u8 + 1, 0xa0], 264);
            for &k in &ELITE_KEYS {
                p[k] = color[ch];
            }
            p
        })
        .collect();
    if extended {
        let mut p = pad(&[0x07, 0x16, 0x04, 0xa0], 264);
        for (offsets, &c) in ELITE_EXT.iter().zip(&color) {
            for &k in offsets {
                p[k] = c;
            }
        }
        out.push(p);
    }
    out
}

fn lamp_frames(leds: usize, packets: u8, color: [u8; 3]) -> Vec<Vec<u8>> {
    let mut out = vec![pad(&[0x44, 0x01, 0x04, 0x00], 65)];
    for seq in 0..usize::from(packets) {
        let mut p = pad(&[0x44, 0x02, seq as u8, 0x00], 65);
        // Past the last LED OpenRGB still sends the packet, with one black LED.
        for i in 0..leds.saturating_sub(seq * 20).min(20) {
            p[4 + 3 * i..7 + 3 * i].copy_from_slice(&color);
        }
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_frames_layout() {
        let f = origins_frames(9, 9, ORIGINS_SKIP, [1, 2, 3]);
        assert_eq!(f.len(), 10);
        assert_eq!(f[0][..10], [0, 0x04, 0xf2, 0, 0, 0, 0, 0, 0, 0x09]);
        assert_eq!(f[1][..9], [0, 0x81, 1, 2, 3, 0x81, 1, 2, 3]);
        // Slot 23 = packet 1, entry 7: dark.
        assert_eq!(f[2][1 + 4 * 7..5 + 4 * 7], [0x81, 0, 0, 0]);
        assert_eq!(f[2][1 + 4 * 6..5 + 4 * 6], [0x81, 1, 2, 3]);
        let f = origins_frames(5, 5, &[], [9, 9, 9]);
        assert_eq!(f.len(), 6);
        assert_eq!(f[0][9], 0x05);
        assert!(f.iter().all(|p| p.len() == 65));
    }

    #[test]
    fn elite_2_frames_layout() {
        // 128 LEDs + 16 gaps fill exactly 9 × 16 slots; the init has no count.
        assert_eq!(9 * 16, 128 + ELITE_2_SKIP.len());
        let f = origins_frames(9, 0, ELITE_2_SKIP, [1, 2, 3]);
        assert_eq!(f.len(), 10);
        assert_eq!(f[0][..10], [0, 0x04, 0xf2, 0, 0, 0, 0, 0, 0, 0]);
        // Slot 76 = packet 4, entry 12: dark; slot 75 lit.
        assert_eq!(f[5][1 + 4 * 12..5 + 4 * 12], [0x81, 0, 0, 0]);
        assert_eq!(f[5][1 + 4 * 11..5 + 4 * 11], [0x81, 1, 2, 3]);
        assert_eq!(f[9][61..65], [0x81, 1, 2, 3]);
    }

    #[test]
    fn core_frames_layout() {
        let f = core_frames([0x11, 0x22, 0x33]);
        // 380 bytes = 6 full chunks + 20.
        assert_eq!(f.len(), 7);
        assert_eq!(f[0][..5], [0x00, 0xa2, 0x00, 0x00, 60]);
        assert_eq!(f[6][..5], [0x00, 0xa2, 0x06, 0x00, 20]);
        assert_eq!(f[6].len(), 25);
        // LED 0: G at 0, R at 16, B at 32.
        assert_eq!((f[0][5], f[0][5 + 16], f[0][5 + 32]), (0x22, 0x11, 0x33));
        // Bytes 6/7 of each sector are padding.
        assert_eq!(f[0][5 + 6], 0);
        // LED 93 -> 336 + 8 + 3 = 347: B at 379, the last byte.
        assert_eq!(f[6][5 + 19], 0x33);
    }

    #[test]
    fn elite_frames_layout() {
        let f = elite_frames(true, [0x11, 0x22, 0x33]);
        assert_eq!(f.len(), 4);
        assert!(f.iter().all(|p| p.len() == 264));
        assert_eq!(f[0][..9], [0x07, 0x16, 0x01, 0xa0, 0, 0, 0, 0, 0x11]);
        assert_eq!(f[1][..4], [0x07, 0x16, 0x02, 0xa0]);
        assert_eq!((f[1][0x95], f[2][0x95]), (0x22, 0x33));
        // 0x1f and 0x25 are gaps between keys.
        assert_eq!((f[0][0x1f], f[0][0x25], f[0][0x96]), (0, 0, 0));
        assert_eq!(f[0].iter().filter(|&&b| b == 0x11).count(), 106);
        assert_eq!(f[3][..4], [0x07, 0x16, 0x04, 0xa0]);
        assert_eq!((f[3][0x08], f[3][0x29], f[3][0x39]), (0x11, 0x22, 0x33));
        assert_eq!(f[3].iter().filter(|&&b| b == 0x33).count(), 22);
        let f = elite_frames(false, [1, 2, 3]);
        assert_eq!(f.len(), 3);
    }

    #[test]
    fn lamp_frames_layout() {
        let f = lamp_frames(74, 4, [1, 2, 3]);
        assert_eq!(f.len(), 5);
        assert!(f.iter().all(|p| p.len() == 65));
        assert_eq!(f[0][..5], [0x44, 0x01, 0x04, 0x00, 0]);
        assert_eq!(f[1][..10], [0x44, 0x02, 0x00, 0x00, 1, 2, 3, 1, 2, 3]);
        assert_eq!(f[4][..4], [0x44, 0x02, 0x03, 0x00]);
        // Last packet: LEDs 60..74 = 14 triplets.
        assert_eq!(f[4][4 + 3 * 13..7 + 3 * 13], [1, 2, 3]);
        assert_eq!(f[4][4 + 3 * 14], 0);
        let f = lamp_frames(10, 1, [9, 8, 7]);
        assert_eq!(f.len(), 2);
        assert_eq!(f[1][31..34], [9, 8, 7]);
        assert_eq!(f[1][34], 0);
    }

    #[test]
    fn keepalives_and_nodes() {
        assert_eq!(FULL.keepalive(), Duration::from_secs(1));
        assert_eq!(ELITE_RGB.family.keepalive(), Duration::from_millis(50));
        assert_eq!(EVE_1800.family.keepalive(), Duration::from_millis(50));
        assert!(!ALL.is_empty());
    }
}
