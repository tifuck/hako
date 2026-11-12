//! Legacy "NXP" protocol (K70 RGB MK.2, K95 Platinum, M65 RGB Elite, ...).
//!
//! 64-byte unnumbered reports on the vendor collection (usage page 0xFFC2,
//! interface 1). `07` writes get no reply, so this side is write-only.
//! Sources: ckb-next `src/daemon/nxp_proto.h`, `led_keyboard.c`,
//! `led_mouse.c`, `dpi.c`, `device_keyboard.c`, `structures.h` (lift range);
//! corsair-protocol docs
//! (github.com/ckb-next/corsair-protocol, host-to-device/07/*); OpenRGB
//! `Controllers/CorsairPeripheralController`.

use super::Backend;
use crate::hid::HidrawDevice;
use anyhow::Result;
use std::time::Duration;

pub const USAGE_PAGE: u16 = 0xffc2;
pub const IFACE: u32 = 1;

const CMD_SET: u8 = 0x07;
const CMD_WRITE_BULK: u8 = 0x7f;
const FIELD_SPECIAL: u8 = 0x04;
const FIELD_LIGHTING: u8 = 0x05;
const FIELD_MOUSE: u8 = 0x13;
const FIELD_M_COLOR: u8 = 0x22;
const FIELD_KB_ZNCLR: u8 = 0x25;
const FIELD_KB_COLOR: u8 = 0x28;
const MODE_HARDWARE: u8 = 0x01;
const MODE_SOFTWARE: u8 = 0x02;
const MOUSE_DPI: u8 = 0x02;
const MOUSE_LIFT: u8 = 0x03;
const MOUSE_SNAP: u8 = 0x04;
const MOUSE_DPIPROF: u8 = 0xd0;

/// ckb-next LIFT_MIN / LIFT_MAX.
pub const LIFT_MIN: u8 = 1;
pub const LIFT_MAX: u8 = 5;

/// DPI stage the host drives; stage 0 is the sniper stage in ckb-next.
const DPI_STAGE: u8 = 1;

pub type Packet = [u8; 64];

fn packet(head: &[u8]) -> Packet {
    let mut p = [0u8; 64];
    p[..head.len()].copy_from_slice(head);
    p
}

/// `07 04 0X`: special functions (M-keys, DPI/sniper buttons) to
/// hardware (1) or host (2). Hardware also restores the onboard lighting.
pub fn special(software: bool) -> Packet {
    let mode = if software {
        MODE_SOFTWARE
    } else {
        MODE_HARDWARE
    };
    packet(&[CMD_SET, FIELD_SPECIAL, mode])
}

/// `07 05 02 00 0X`: software lighting; X = 3 on keyboards, 1 on mice.
pub fn software_lighting(keyboard: bool) -> Packet {
    packet(&[
        CMD_SET,
        FIELD_LIGHTING,
        MODE_SOFTWARE,
        0,
        if keyboard { 3 } else { 1 },
    ])
}

/// 24-bit keyboard frame: per channel three `7f` stream packets (60+60+48
/// keys) then `07 28 <channel> 03 <1 more / 2 last>`.
pub fn keyboard_color(rgb: [u8; 3]) -> Vec<Packet> {
    let mut out = Vec::with_capacity(12);
    for (ch, &v) in rgb.iter().enumerate() {
        for (n, len) in [(1u8, 60u8), (2, 60), (3, 48)] {
            let mut p = packet(&[CMD_WRITE_BULK, n, len, 0]);
            p[4..4 + usize::from(len)].fill(v);
            out.push(p);
        }
        let last = if ch == 2 { 2 } else { 1 };
        out.push(packet(&[CMD_SET, FIELD_KB_COLOR, ch as u8 + 1, 3, last]));
    }
    out
}

/// K55 RGB: three zones, `07 25 00 00` then RGB triplets.
pub fn k55_color(rgb: [u8; 3]) -> Packet {
    let mut p = packet(&[CMD_SET, FIELD_KB_ZNCLR, 0, 0]);
    for zone in 0..3 {
        p[4 + zone * 3..7 + zone * 3].copy_from_slice(&rgb);
    }
    p
}

/// `07 22 <count> 01` then `<zone> R G B` per zone.
pub fn mouse_color(zones: &[u8], rgb: [u8; 3]) -> Packet {
    let mut p = packet(&[CMD_SET, FIELD_M_COLOR, zones.len() as u8, 0x01]);
    for (i, &z) in zones.iter().enumerate() {
        p[4 + i * 4] = z;
        p[5 + i * 4..8 + i * 4].copy_from_slice(&rgb);
    }
    p
}

/// DPI for the host stage, then select it. Byte 3 = 0 targets the live
/// settings; ckb-next only sets it to 1 when saving to the onboard profile.
pub fn dpi(dpi: u16) -> [Packet; 2] {
    let [lo, hi] = dpi.to_le_bytes();
    [
        packet(&[
            CMD_SET,
            FIELD_MOUSE,
            MOUSE_DPIPROF | DPI_STAGE,
            0,
            0,
            lo,
            hi,
            lo,
            hi,
        ]),
        packet(&[CMD_SET, FIELD_MOUSE, MOUSE_DPI, 0, DPI_STAGE]),
    ]
}

/// Live lift-off height (1..=5); byte 3 = 1 would save it to the profile.
pub fn lift(level: u8) -> Packet {
    packet(&[
        CMD_SET,
        FIELD_MOUSE,
        MOUSE_LIFT,
        0,
        level.clamp(LIFT_MIN, LIFT_MAX),
    ])
}

/// Live angle snapping; ckb-next sends a trailing 0x05 with it.
pub fn snap(on: bool) -> Packet {
    packet(&[CMD_SET, FIELD_MOUSE, MOUSE_SNAP, 0, u8::from(on), 0x05])
}

#[derive(Clone, Copy, Debug)]
pub enum Leds {
    Keyboard,
    K55,
    Mouse(&'static [u8]),
}

pub struct Nxp {
    dev: HidrawDevice,
    leds: Leds,
    software: Option<bool>,
}

impl Nxp {
    pub fn open(dev: HidrawDevice, leds: Leds) -> Self {
        Self {
            dev,
            leds,
            software: None,
        }
    }

    fn send(&mut self, p: &Packet) -> Result<()> {
        let mut buf = [0u8; 65];
        buf[1..].copy_from_slice(p);
        self.dev.write(&buf)?;
        // Writes are unacknowledged; pace them like ckb-next's NXP delay.
        std::thread::sleep(Duration::from_millis(3));
        self.dev.drain(Duration::ZERO);
        Ok(())
    }
}

impl Backend for Nxp {
    fn software(&mut self, on: bool) -> Result<()> {
        if self.software == Some(on) {
            return Ok(());
        }
        if on {
            self.send(&special(true))?;
            std::thread::sleep(Duration::from_millis(30));
            self.send(&software_lighting(!matches!(self.leds, Leds::Mouse(_))))?;
        } else {
            // ckb-next sends it twice to restore the hardware profile.
            self.send(&special(false))?;
            std::thread::sleep(Duration::from_millis(30));
            self.send(&special(false))?;
        }
        std::thread::sleep(Duration::from_millis(30));
        self.software = Some(on);
        Ok(())
    }

    fn color(&mut self, rgb: [u8; 3]) -> Result<()> {
        match self.leds {
            Leds::Keyboard => keyboard_color(rgb).iter().try_for_each(|p| self.send(p)),
            Leds::K55 => self.send(&k55_color(rgb)),
            Leds::Mouse(zones) => self.send(&mouse_color(zones, rgb)),
        }
    }

    fn dpi(&mut self, value: u16) -> Result<()> {
        dpi(value).iter().try_for_each(|p| self.send(p))
    }

    fn lift(&mut self, level: u8) -> Result<()> {
        self.send(&lift(level))
    }

    fn angle_snap(&mut self, on: bool) -> Result<()> {
        self.send(&snap(on))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_packets() {
        assert_eq!(special(true)[..4], [0x07, 0x04, 0x02, 0x00]);
        assert_eq!(special(false)[..4], [0x07, 0x04, 0x01, 0x00]);
        assert_eq!(
            software_lighting(true)[..6],
            [0x07, 0x05, 0x02, 0x00, 0x03, 0x00]
        );
        assert_eq!(
            software_lighting(false)[..6],
            [0x07, 0x05, 0x02, 0x00, 0x01, 0x00]
        );
    }

    #[test]
    fn keyboard_frame_layout() {
        let f = keyboard_color([0x11, 0x22, 0x33]);
        assert_eq!(f.len(), 12);
        assert_eq!(f[0][..5], [0x7f, 0x01, 0x3c, 0x00, 0x11]);
        assert_eq!(f[0][63], 0x11);
        assert_eq!(f[2][..4], [0x7f, 0x03, 0x30, 0x00]);
        // Third stream packet carries 48 keys, the rest stays zero.
        assert_eq!(f[2][51], 0x11);
        assert_eq!(f[2][52], 0x00);
        assert_eq!(f[3][..6], [0x07, 0x28, 0x01, 0x03, 0x01, 0x00]);
        assert_eq!(f[4][4], 0x22);
        assert_eq!(f[7][..5], [0x07, 0x28, 0x02, 0x03, 0x01]);
        assert_eq!(f[8][4], 0x33);
        assert_eq!(f[11][..5], [0x07, 0x28, 0x03, 0x03, 0x02]);
    }

    #[test]
    fn zone_packets() {
        assert_eq!(
            k55_color([1, 2, 3])[..14],
            [0x07, 0x25, 0x00, 0x00, 1, 2, 3, 1, 2, 3, 1, 2, 3, 0]
        );
        assert_eq!(
            mouse_color(&[1, 2, 6], [0xff, 0x80, 0x00])[..17],
            [
                0x07, 0x22, 0x03, 0x01, 1, 0xff, 0x80, 0x00, 2, 0xff, 0x80, 0x00, 6, 0xff, 0x80,
                0x00, 0
            ]
        );
    }

    #[test]
    fn dpi_packets() {
        let [stage, select] = dpi(1600);
        assert_eq!(
            stage[..10],
            [0x07, 0x13, 0xd1, 0x00, 0x00, 0x40, 0x06, 0x40, 0x06, 0x00]
        );
        assert_eq!(select[..6], [0x07, 0x13, 0x02, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn lift_and_snap_packets() {
        assert_eq!(lift(3)[..6], [0x07, 0x13, 0x03, 0x00, 0x03, 0x00]);
        assert_eq!(lift(0)[4], LIFT_MIN);
        assert_eq!(lift(9)[4], LIFT_MAX);
        assert_eq!(snap(true)[..7], [0x07, 0x13, 0x04, 0x00, 0x01, 0x05, 0x00]);
        assert_eq!(snap(false)[..6], [0x07, 0x13, 0x04, 0x00, 0x00, 0x05]);
    }
}
