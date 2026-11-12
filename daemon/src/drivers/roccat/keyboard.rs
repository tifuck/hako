//! ROCCAT / Turtle Beach Vulcan, Pyro and Magma keyboards (direct mode) and
//! the Horde AIMO.
//!
//! Layouts from OpenRGB `RoccatVulcanKeyboardController.{h,cpp}`,
//! `RoccatVulcanKeyboardLayouts.h`, `RoccatControllerDetect.cpp` and
//! `RoccatHordeAimoController`. Vulcan-style boards have a control node
//! (interface 1: info, direct on/off and ready reports) and an LED node
//! (interface 3, 4 on the Vulcan II TKL Pro) taking 65-byte `a1` color
//! frames. The mode report (0x0D / 0x11) is the onboard-profile write and is
//! never sent; direct on/off (`15 00 <on>` / `0e 05 <on> 00 00`) is volatile,
//! so "onboard" restores the keyboard's own lighting. Every LED slot of the
//! model's layout gets the one color.
//!
//! Saving lighting to the keyboard is that mode report, known from OpenRGB
//! alone (`SendMode`, per-model packet lengths), so it is a blocked
//! placeholder. The Horde AIMO has no documented save (OpenRGB `@save :x:`).

use super::{
    checksum, open_iface, save_blocked, wait_ready, Adapter, Lights, Spec, ONBOARD_STATIC,
    OPENRGB_ONLY, ROCCAT, STICKY_STATIC, TURTLE_BEACH,
};
use crate::drivers::{DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::Result;
use hako_core::catalog::SettingMeta;
use hako_core::device::DeviceKind;
use std::time::Duration;

/// Where each LED's R, G and B sit in the color stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    /// LED i: column i / `col`, row i % `col`; R at column * 3 * col + row,
    /// G `col` later, B `2 * col` later. `col = 1` is plain R G B per LED.
    Columns { col: usize, leds: usize },
    /// Plain R G B per slot, only for the listed slots (Vulcan II Mini).
    Slots(&'static [u8]),
    /// All R, then all G, then all B, `leds` each (Vulcan II TKL Pro).
    Planar { leds: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Vulcan {
    /// Newer protocol: info report 0x09 (9 bytes), direct `0e 05`, `a1 <n>`
    /// on every frame packet. Older: info 0x0F (8 bytes), direct `15 00`,
    /// header on the first packet only.
    v2: bool,
    /// Ready report 0x04 length.
    ready_len: usize,
    /// Byte count announced in the first frame packet.
    frame_len: u16,
    layout: Layout,
    led_iface: u32,
    /// Always little-endian frame length, whatever the firmware.
    le_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    Vulcan(Vulcan),
    /// Feature report `18 18 ff ff` + 6 × RGB + checksum (interface 1).
    Horde,
}

const fn v1(leds: usize) -> Family {
    Family::Vulcan(Vulcan {
        v2: false,
        ready_len: 3,
        frame_len: 436,
        layout: Layout::Columns { col: 12, leds },
        led_iface: 3,
        le_only: false,
    })
}

const fn v2(ready_len: usize, frame_len: u16, layout: Layout) -> Family {
    Family::Vulcan(Vulcan {
        v2: true,
        ready_len,
        frame_len,
        layout,
        led_iface: 3,
        le_only: false,
    })
}

/// LED slot counts: highest LED id of the model's OpenRGB layouts + 1.
const AIMO_120_LEDS: usize = 132;
const PYRO_LEDS: usize = 118;
const VULCAN_II_LEDS: usize = 122;
const VULCAN_II_MAX_LEDS: usize = 174;
const TKL_LEDS: usize = 96;
/// Hardware slots the Vulcan II Mini's 69 LEDs map to.
const MINI_SLOTS: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 19, 20, 21, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 42, 43, 45, 46, 48, 49, 50, 51, 52, 53, 54,
    55, 56, 57, 58, 59, 60, 61, 62, 64, 65, 66, 68, 70, 71, 72, 73, 74, 75, 76,
];

pub(super) const VULCAN_120_AIMO: Spec<Family> = Spec {
    name: "ROCCAT Vulcan 120-Series AIMO",
    vid: ROCCAT,
    pids: &[0x3098],
    family: v1(AIMO_120_LEDS),
};
pub(super) const VULCAN_100_AIMO: Spec<Family> = Spec {
    name: "ROCCAT Vulcan 100 AIMO",
    vid: ROCCAT,
    pids: &[0x307a],
    family: v1(AIMO_120_LEDS),
};
pub(super) const VULCAN_TKL: Spec<Family> = Spec {
    name: "ROCCAT Vulcan TKL",
    vid: ROCCAT,
    pids: &[0x2fee],
    family: v1(TKL_LEDS),
};
pub(super) const TB_VULCAN_II_TKL: Spec<Family> = Spec {
    name: "Turtle Beach Vulcan II TKL",
    vid: TURTLE_BEACH,
    pids: &[0x5023],
    family: v1(TKL_LEDS),
};
pub(super) const VULCAN_PRO: Spec<Family> = Spec {
    name: "ROCCAT Vulcan Pro",
    vid: ROCCAT,
    pids: &[0x30f7],
    family: v2(
        4,
        384,
        Layout::Columns {
            col: 12,
            leds: PYRO_LEDS,
        },
    ),
};
pub(super) const VULCAN_TKL_PRO: Spec<Family> = Spec {
    name: "ROCCAT Vulcan TKL Pro",
    vid: ROCCAT,
    pids: &[0x311a],
    family: v2(
        4,
        384,
        Layout::Columns {
            col: 12,
            leds: TKL_LEDS,
        },
    ),
};
pub(super) const PYRO: Spec<Family> = Spec {
    name: "ROCCAT Pyro",
    vid: ROCCAT,
    pids: &[0x314c],
    family: v2(
        4,
        378,
        Layout::Columns {
            col: 1,
            leds: PYRO_LEDS,
        },
    ),
};
pub(super) const MAGMA: Spec<Family> = Spec {
    name: "ROCCAT Magma",
    vid: ROCCAT,
    pids: &[0x3124],
    family: v2(4, 64, Layout::Columns { col: 5, leds: 5 }),
};
pub(super) const MAGMA_MINI: Spec<Family> = Spec {
    name: "ROCCAT Magma Mini",
    vid: ROCCAT,
    pids: &[0x69a0],
    family: v2(4, 64, Layout::Columns { col: 5, leds: 5 }),
};
pub(super) const VULCAN_II: Spec<Family> = Spec {
    name: "ROCCAT Vulcan II",
    vid: ROCCAT,
    pids: &[0x2f4e],
    family: v2(
        3,
        396,
        Layout::Columns {
            col: 1,
            leds: VULCAN_II_LEDS,
        },
    ),
};
pub(super) const TB_VULCAN_II: Spec<Family> = Spec {
    name: "Turtle Beach Vulcan II",
    vid: TURTLE_BEACH,
    pids: &[0x501b],
    family: v2(
        3,
        396,
        Layout::Columns {
            col: 1,
            leds: VULCAN_II_LEDS,
        },
    ),
};
pub(super) const VULCAN_II_MAX: Spec<Family> = Spec {
    name: "ROCCAT Vulcan II Max",
    vid: ROCCAT,
    pids: &[0x2ee2],
    family: v2(
        3,
        567,
        Layout::Columns {
            col: 1,
            leds: VULCAN_II_MAX_LEDS,
        },
    ),
};
pub(super) const VULCAN_II_MINI: Spec<Family> = Spec {
    name: "ROCCAT Vulcan II Mini",
    vid: ROCCAT,
    pids: &[0x2f09],
    family: Family::Vulcan(Vulcan {
        v2: true,
        ready_len: 3,
        frame_len: 345,
        layout: Layout::Slots(MINI_SLOTS),
        led_iface: 3,
        le_only: true,
    }),
};
pub(super) const TB_VULCAN_II_TKL_PRO: Spec<Family> = Spec {
    name: "Turtle Beach Vulcan II TKL Pro",
    vid: TURTLE_BEACH,
    pids: &[0x5001],
    family: Family::Vulcan(Vulcan {
        v2: true,
        ready_len: 3,
        frame_len: 320,
        layout: Layout::Planar { leds: 91 },
        led_iface: 4,
        le_only: false,
    }),
};
pub(super) const HORDE_AIMO: Spec<Family> = Spec {
    name: "ROCCAT Horde AIMO",
    vid: ROCCAT,
    pids: &[0x303e],
    family: Family::Horde,
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[
    &VULCAN_120_AIMO,
    &VULCAN_100_AIMO,
    &VULCAN_TKL,
    &TB_VULCAN_II_TKL,
    &VULCAN_PRO,
    &VULCAN_TKL_PRO,
    &PYRO,
    &MAGMA,
    &MAGMA_MINI,
    &VULCAN_II,
    &TB_VULCAN_II,
    &VULCAN_II_MAX,
    &VULCAN_II_MINI,
    &TB_VULCAN_II_TKL_PRO,
    &HORDE_AIMO,
];

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, ctx: &DriverCtx) -> Opened {
    let (ctrl, instance) = open_iface(spec.vid, pid, 1)?;
    let lights: Box<dyn Lights> = match spec.family {
        Family::Vulcan(v) => {
            let (led, _) = open_iface(spec.vid, pid, v.led_iface)?;
            let firmware = firmware(&ctrl, v.v2);
            Box::new(Keyboard {
                ctrl,
                led,
                v,
                big_endian: big_endian(v, firmware),
                direct: false,
            })
        }
        Family::Horde => Box::new(Horde {
            dev: ctrl,
            init: false,
        }),
    };
    Adapter::open(spec, DeviceKind::Keyboard, pid, instance, lights, ctx)
}

/// Firmware version byte (103 = v1.03) from the info report; 0 if unreadable.
fn firmware(ctrl: &HidrawDevice, v2: bool) -> u8 {
    let mut buf = if v2 { vec![0u8; 9] } else { vec![0u8; 8] };
    buf[0] = if v2 { 0x09 } else { 0x0f };
    match ctrl.get_feature(&mut buf) {
        Ok(()) => buf[2],
        Err(_) => 0,
    }
}

/// Firmware 1.16 and later expect the frame length big-endian. The test is
/// the source's (major >= 1 and minor >= 16), kept verbatim.
fn big_endian(v: Vulcan, firmware: u8) -> bool {
    !v.le_only && firmware / 100 >= 1 && firmware % 100 >= 16
}

fn direct(v2: bool, on: bool) -> Vec<u8> {
    if v2 {
        vec![0x0e, 0x05, u8::from(on), 0x00, 0x00]
    } else {
        vec![0x15, 0x00, u8::from(on)]
    }
}

/// R, G, B positions of every LED in the color stream.
fn positions(layout: Layout) -> Vec<[usize; 3]> {
    match layout {
        Layout::Columns { col, leds } => (0..leds)
            .map(|i| {
                let base = (i / col) * 3 * col + i % col;
                [base, base + col, base + 2 * col]
            })
            .collect(),
        Layout::Slots(slots) => slots
            .iter()
            .map(|&s| {
                let s = 3 * usize::from(s);
                [s, s + 1, s + 2]
            })
            .collect(),
        Layout::Planar { leds } => (0..leds).map(|i| [i, i + leds, i + 2 * leds]).collect(),
    }
}

/// The 65-byte output reports (report id 0) carrying one color on every LED.
fn frames(v: Vulcan, big_endian: bool, color: [u8; 3]) -> Vec<[u8; 65]> {
    let len = usize::from(v.frame_len);
    let mut bufs = vec![[0u8; 65]; len.div_ceil(64)];
    for (i, b) in bufs.iter_mut().enumerate() {
        if v.v2 || i == 0 {
            b[1] = 0xa1;
            b[2] = i as u8 + 1;
        }
    }
    let header = if len > 255 { 4 } else { 3 };
    if header == 3 {
        bufs[0][3] = len as u8;
    } else if big_endian {
        bufs[0][3..5].copy_from_slice(&v.frame_len.to_be_bytes());
    } else {
        bufs[0][3..5].copy_from_slice(&v.frame_len.to_le_bytes());
    }
    let data = 64 - header;
    for pos in positions(v.layout) {
        for (&p, c) in pos.iter().zip(color) {
            let (packet, at) = if v.v2 {
                (p / data, p % data + header + 1)
            } else {
                ((p + header) / 64, (p + header) % 64 + 1)
            };
            if let Some(b) = bufs.get_mut(packet) {
                b[at] = c;
            }
        }
    }
    bufs
}

struct Keyboard {
    ctrl: HidrawDevice,
    led: HidrawDevice,
    v: Vulcan,
    big_endian: bool,
    direct: bool,
}

impl Keyboard {
    fn set_direct(&mut self, on: bool) -> Result<()> {
        self.ctrl.set_feature(&mut direct(self.v.v2, on))?;
        wait_ready(
            &self.ctrl,
            0x04,
            self.v.ready_len,
            Duration::from_millis(25),
            40,
        );
        self.direct = on;
        Ok(())
    }
}

impl Lights for Keyboard {
    fn effects(&self) -> &'static [(&'static str, &'static str)] {
        ONBOARD_STATIC
    }

    fn apply(&mut self, effect: &str, color: [u8; 3]) -> Result<()> {
        if effect != "static" {
            if self.direct {
                self.set_direct(false)?;
            }
            return Ok(());
        }
        if !self.direct {
            self.set_direct(true)?;
        }
        for f in frames(self.v, self.big_endian, color) {
            self.led.write(&f)?;
        }
        // The LED node acknowledges each frame; drop the replies.
        self.led.drain(Duration::ZERO);
        let _ = self
            .led
            .read_timeout(&mut [0u8; 65], Duration::from_millis(20));
        Ok(())
    }

    fn extra_controls(&self) -> Vec<SettingMeta> {
        vec![save_blocked(OPENRGB_ONLY)]
    }

    fn release(&mut self) {
        if self.direct {
            let _ = self.set_direct(false);
        }
    }
}

/// Horde AIMO: init `13 08 01`, then six RGB zones.
fn horde_frame(color: [u8; 3]) -> [u8; 24] {
    let mut p = [0u8; 24];
    p[..4].copy_from_slice(&[0x18, 0x18, 0xff, 0xff]);
    for zone in p[4..22].chunks_exact_mut(3) {
        zone.copy_from_slice(&color);
    }
    checksum(&mut p);
    p
}

const HORDE_INIT: [u8; 8] = [0x13, 0x08, 0x01, 0, 0, 0, 0, 0];

struct Horde {
    dev: HidrawDevice,
    /// The init report was sent (once, as OpenRGB does).
    init: bool,
}

impl Lights for Horde {
    /// No documented way back to the onboard effect.
    fn effects(&self) -> &'static [(&'static str, &'static str)] {
        STICKY_STATIC
    }

    fn apply(&mut self, effect: &str, color: [u8; 3]) -> Result<()> {
        if effect == "static" {
            if !self.init {
                let mut init = HORDE_INIT;
                self.dev.set_feature(&mut init)?;
                self.init = true;
            }
            self.dev.set_feature(&mut horde_frame(color))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vulcan(spec: &Spec<Family>) -> Vulcan {
        match spec.family {
            Family::Vulcan(v) => v,
            Family::Horde => unreachable!(),
        }
    }

    #[test]
    fn direct_and_endianness() {
        assert_eq!(direct(false, true), [0x15, 0x00, 0x01]);
        assert_eq!(direct(true, false), [0x0e, 0x05, 0x00, 0x00, 0x00]);
        let v = vulcan(&VULCAN_120_AIMO);
        assert!(!big_endian(v, 115));
        assert!(big_endian(v, 116));
        assert!(!big_endian(v, 205), "source test is minor >= 16");
        assert!(!big_endian(vulcan(&VULCAN_II_MINI), 120));
    }

    #[test]
    fn v1_frames() {
        let f = frames(vulcan(&VULCAN_120_AIMO), false, [1, 2, 3]);
        assert_eq!(f.len(), 7);
        // 436 = 0x01b4, little-endian before firmware 1.16.
        assert_eq!(f[0][..5], [0, 0xa1, 0x01, 0xb4, 0x01]);
        // Only the first packet carries a header: packet 1 continues the
        // stream at position 60 = LED 12's B (column 1, + 24).
        assert_eq!(f[1][..3], [0, 3, 3]);
        // LED 0: R at 4, G at 16, B at 28 (+1 for the report id).
        assert_eq!((f[0][5], f[0][17], f[0][29]), (1, 2, 3));
        // LED 12 starts column 1: R at 36 + 4.
        assert_eq!(f[0][41], 1);
        let f = frames(vulcan(&VULCAN_120_AIMO), true, [1, 2, 3]);
        assert_eq!(f[0][3..5], [0x01, 0xb4]);
    }

    #[test]
    fn v2_frames() {
        let f = frames(vulcan(&PYRO), false, [1, 2, 3]);
        assert_eq!(f.len(), 6);
        // 378 = 0x017a.
        assert_eq!(f[0][..8], [0, 0xa1, 0x01, 0x7a, 0x01, 1, 2, 3]);
        // 60 data bytes per packet: LED 20 opens packet 1.
        assert_eq!(f[1][..8], [0, 0xa1, 0x02, 0, 0, 1, 2, 3]);
        // Magma: 64 bytes, 3-byte header, 5-row column.
        let f = frames(vulcan(&MAGMA), false, [1, 2, 3]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0][..4], [0, 0xa1, 0x01, 64]);
        assert_eq!(f[0][4..19], [1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3]);
        assert_eq!(f[0][19], 0);
    }

    #[test]
    fn mini_and_planar_frames() {
        let mini = vulcan(&VULCAN_II_MINI);
        let f = frames(mini, big_endian(mini, 120), [1, 2, 3]);
        // Mini is never big-endian: 345 = 0x0159.
        assert_eq!(f[0][3..5], [0x59, 0x01]);
        // Slot 17 is unused, slot 18 follows 16.
        assert_eq!(f[0][5 + 3 * 16..5 + 3 * 17], [1, 2, 3]);
        assert_eq!(f[0][5 + 3 * 17..5 + 3 * 18], [0, 0, 0]);
        assert_eq!(f[0][5 + 3 * 18], 1);
        let f = frames(vulcan(&TB_VULCAN_II_TKL_PRO), false, [1, 2, 3]);
        assert_eq!(f.len(), 5);
        // R block 0..91, G 91..182, B 182..273; 60 data bytes per packet.
        assert_eq!((f[0][5], f[1][5 + 30], f[1][5 + 31]), (1, 1, 2));
        assert_eq!((f[3][5 + 1], f[3][5 + 2]), (2, 3));
        assert_eq!((f[4][5 + 32], f[4][5 + 33]), (3, 0));
    }

    #[test]
    fn every_layout_fits_its_frame() {
        for spec in ALL {
            let Family::Vulcan(v) = spec.family else {
                continue;
            };
            let header = if v.frame_len > 255 { 4 } else { 3 };
            let last = positions(v.layout).iter().flatten().copied().max().unwrap();
            assert!(last < usize::from(v.frame_len) - header, "{}", spec.name);
        }
    }

    #[test]
    fn horde_packets() {
        let f = horde_frame([1, 2, 3]);
        assert_eq!(f[..7], [0x18, 0x18, 0xff, 0xff, 1, 2, 3]);
        assert_eq!(f[19..22], [1, 2, 3]);
        // 0x18 + 0x18 + 0xff + 0xff + 6 * 6 = 0x0252.
        assert_eq!(f[22..], [0x52, 0x02]);
        assert_eq!(HORDE_INIT[..3], [0x13, 0x08, 0x01]);
    }
}
