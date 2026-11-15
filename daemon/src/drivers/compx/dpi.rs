//! DPI stage records of the v1 protocol and what depends on the sensor.
//!
//! A stage is a 4-byte record `[x_lo, y_lo, ex, crc]` at `0x0c + 4 * i` with
//! `crc = 0x55 - (x_lo + y_lo + ex)`; `ex` carries the high bits of both axes
//! and per-axis scale flags, and how the three bytes map to DPI differs per
//! sensor (ATK HUB 3.2.33, `docs/vendor-re/compx.md` 2.4). Every encoder is
//! checked by decoding its result again, so a value the sensor cannot hold is
//! refused instead of being stored as another one.

use super::tables::{GS, HS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    Paw3395,
    Paw3395Ultra,
    Paw3950,
    Paw3950Dm,
    Paw3950Ultra,
    /// Own 6-byte records at 0x1b00; ATK and VGN disagree on the high bits.
    Paw3955,
    Paw3395Se,
    Paw3311,
    Paw3315,
    Core26k,
}

/// Address of stage `i` (the classic 4-byte layout).
pub const fn stage_addr(i: u8) -> u16 {
    0x000c + 4 * i as u16
}
pub const RECORD: u8 = 4;
/// Stages hako shows (the mice have up to 8).
pub const STAGES: u8 = 5;

const SUM: u8 = 0x55;
const SPLIT: i64 = 10_000;
const DOUBLE_ABOVE: i64 = 30_000;
const BASE: i64 = 10_050;

impl Sensor {
    /// Slider range and step; the encoder refuses what the step grid of the
    /// sensor cannot represent (e.g. odd multiples of 50 above 30000).
    pub fn range(self) -> (i64, i64, i64) {
        match self {
            Sensor::Paw3395 | Sensor::Paw3395Ultra => (100, 30_000, 50),
            Sensor::Paw3950 | Sensor::Paw3950Dm => (50, 36_000, 50),
            Sensor::Paw3950Ultra => (50, 42_000, 50),
            Sensor::Core26k => (50, 26_000, 50),
            Sensor::Paw3395Se => (200, 18_000, 50),
            Sensor::Paw3315 => (200, 16_000, 100),
            Sensor::Paw3311 => (200, 15_000, 100),
            Sensor::Paw3955 => (50, 50_000, 50),
        }
    }

    /// Writes are offered only where the layout is established.
    pub fn writable(self) -> bool {
        self != Sensor::Paw3955
    }

    /// Lift-off codes (1 = 1 mm, 2 = 2 mm, 3 = 0.7 mm): PAW3950 and 3950DM
    /// also have 0.7 mm. `None`: 11-step scale of the PAW3955, not offered.
    pub fn lift_off(self) -> Option<&'static [(i64, &'static str)]> {
        match self {
            Sensor::Paw3955 => None,
            Sensor::Paw3950 | Sensor::Paw3950Dm => Some(&[(3, "0.7 mm"), (1, "1 mm"), (2, "2 mm")]),
            _ => Some(&[(1, "1 mm"), (2, "2 mm")]),
        }
    }

    /// Sensors the vendor's firmware header lists for angle snapping.
    pub fn angle_snapping(self) -> bool {
        matches!(
            self,
            Sensor::Paw3395
                | Sensor::Paw3395Ultra
                | Sensor::Paw3311
                | Sensor::Paw3950
                | Sensor::Paw3950Dm
                | Sensor::Paw3950Ultra
        )
    }
}

pub fn crc(x: u8, y: u8, ex: u8) -> u8 {
    SUM.wrapping_sub(x.wrapping_add(y).wrapping_add(ex))
}

fn hi_x(ex: u8) -> i64 {
    i64::from(ex & 0x0c) << 6
}

fn hi_y(ex: u8) -> i64 {
    i64::from(ex & 0xc0) << 2
}

fn table(sensor: Sensor) -> &'static [(u8, u16, bool)] {
    if sensor == Sensor::Paw3315 {
        GS
    } else {
        HS
    }
}

fn table_value(t: &[(u8, u16, bool)], code: u8, doubled: bool) -> Option<i64> {
    let &(_, dpi, both) = t.iter().find(|e| e.0 == code)?;
    if doubled {
        both.then_some(i64::from(dpi) * 2)
    } else {
        Some(i64::from(dpi))
    }
}

/// (x, y) DPI of a record; `None` for a bad checksum or a code the sensor
/// does not have.
pub fn decode(sensor: Sensor, rec: &[u8]) -> Option<(i64, i64)> {
    let &[x, y, ex, c] = rec.get(..4)? else {
        return None;
    };
    if c != crc(x, y, ex) {
        return None;
    }
    let (ax, ay) = (i64::from(x) | hi_x(ex), i64::from(y) | hi_y(ex));
    let (dx, dy) = (ex & 1 != 0, ex & 0x10 != 0);
    let pair = |vx: i64, vy: i64| (if dx { vx * 2 } else { vx }, if dy { vy * 2 } else { vy });
    match sensor {
        Sensor::Paw3395 | Sensor::Paw3395Ultra | Sensor::Paw3950 | Sensor::Paw3950Dm => {
            Some(pair((ax + 1) * 50, (ay + 1) * 50))
        }
        Sensor::Paw3950Ultra => {
            let one = |a: i64, fine: bool| if fine { a * 50 + BASE } else { (a + 1) * 10 };
            Some(pair(one(ax, ex & 2 != 0), one(ay, ex & 0x20 != 0)))
        }
        Sensor::Core26k => {
            let one = |a: i64, wide: bool| (a + 1) * if wide { 100 } else { 50 };
            Some(pair(one(ax, ex & 2 != 0), one(ay, ex & 0x20 != 0)))
        }
        Sensor::Paw3395Se | Sensor::Paw3311 | Sensor::Paw3315 => {
            let t = table(sensor);
            Some((
                table_value(t, x, ex & 2 != 0)?,
                table_value(t, y, ex & 0x20 != 0)?,
            ))
        }
        Sensor::Paw3955 => None,
    }
}

fn record(hx: i64, hy: i64, flags: u8) -> Option<[u8; 4]> {
    if !(0..1024).contains(&hx) || !(0..1024).contains(&hy) {
        return None;
    }
    let ex = flags | ((hx >> 8) as u8 & 3) << 2 | ((hy >> 8) as u8 & 3) << 6;
    let (xl, yl) = (hx as u8, hy as u8);
    Some([xl, yl, ex, crc(xl, yl, ex)])
}

fn code_for(t: &[(u8, u16, bool)], dpi: i64) -> Option<(u8, bool)> {
    let mut r = (dpi + 25) / 50 * 50;
    if r > SPLIT && r % 100 != 0 {
        r += 50;
    }
    t.iter()
        .find(|e| i64::from(e.1) == r)
        .map(|e| (e.0, false))
        .or_else(|| {
            t.iter()
                .find(|e| e.2 && i64::from(e.1) * 2 == r)
                .map(|e| (e.0, true))
        })
}

/// Record for (x, y) DPI, or `None` if the sensor cannot hold exactly that.
pub fn encode(sensor: Sensor, x: i64, y: i64) -> Option<[u8; 4]> {
    let (lo, hi, _) = sensor.range();
    if !(lo..=hi).contains(&x) || !(lo..=hi).contains(&y) {
        return None;
    }
    let rec = match sensor {
        Sensor::Paw3395 | Sensor::Paw3395Ultra | Sensor::Paw3950 | Sensor::Paw3950Dm => {
            let step = |v: i64| if v <= DOUBLE_ABOVE { 50 } else { 100 };
            let flags = u8::from(x > DOUBLE_ABOVE) | u8::from(y > DOUBLE_ABOVE) << 4;
            record(x / step(x) - 1, y / step(y) - 1, flags)
        }
        Sensor::Paw3950Ultra => {
            let raw = |v: i64| {
                if v > SPLIT {
                    (if v <= DOUBLE_ABOVE { v } else { v / 2 } - BASE) / 50
                } else {
                    v / 10 - 1
                }
            };
            let flag = |v: i64, fine: u8, dbl: u8| {
                if v > SPLIT {
                    fine | if v > DOUBLE_ABOVE { dbl } else { 0 }
                } else {
                    0
                }
            };
            record(raw(x), raw(y), flag(x, 2, 1) | flag(y, 0x20, 0x10))
        }
        Sensor::Core26k => record(x / 50 - 1, y / 50 - 1, 0),
        Sensor::Paw3395Se | Sensor::Paw3311 | Sensor::Paw3315 => {
            let t = table(sensor);
            let (cx, dx) = code_for(t, x)?;
            let (cy, dy) = code_for(t, y)?;
            let ex = u8::from(dx) << 1 | u8::from(dy) << 5;
            Some([cx, cy, ex, crc(cx, cy, ex)])
        }
        Sensor::Paw3955 => None,
    }?;
    (decode(sensor, &rec) == Some((x, y))).then_some(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_layout_matches_the_vendor_codec() {
        // 800 DPI: raw 15, no flags; crc = 0x55 - (15 + 15).
        assert_eq!(
            encode(Sensor::Paw3395, 800, 800),
            Some([15, 15, 0, 0x55 - 30])
        );
        // 26000: raw 519 = 0x207, high bits 2 (x) and 2 (y).
        let r = encode(Sensor::Paw3395Ultra, 26_000, 26_000).unwrap();
        assert_eq!(r[..3], [7, 7, 2 << 2 | 2 << 6]);
        assert_eq!(decode(Sensor::Paw3395Ultra, &r), Some((26_000, 26_000)));
        // Above 30000 the 100 step and the x2 flags are used (PAW3950).
        let r = encode(Sensor::Paw3950, 36_000, 800).unwrap();
        assert_eq!(r[2] & 0x11, 1);
        assert_eq!(decode(Sensor::Paw3950, &r), Some((36_000, 800)));
        assert_eq!(encode(Sensor::Paw3950, 30_050, 800), None, "odd step");
        assert_eq!(encode(Sensor::Paw3395, 30_050, 800), None, "out of range");
    }

    #[test]
    fn paw3950_ultra_uses_10_dpi_steps_below_10000() {
        // 800 DPI -> raw 79; 12000 -> (12000 - 10050) / 50 = 39 with bit 1.
        assert_eq!(
            encode(Sensor::Paw3950Ultra, 800, 12_000),
            Some([79, 39, 0x20, crc(79, 39, 0x20)])
        );
        let r = encode(Sensor::Paw3950Ultra, 42_000, 42_000).unwrap();
        assert_eq!(r[2] & 0x33, 0x33);
        assert_eq!(decode(Sensor::Paw3950Ultra, &r), Some((42_000, 42_000)));
        assert_eq!(encode(Sensor::Paw3950Ultra, 12_025, 800), None);
    }

    #[test]
    fn core26k_and_table_sensors_round_trip() {
        let r = encode(Sensor::Core26k, 1600, 1600).unwrap();
        assert_eq!(r[..3], [31, 31, 0]);
        for s in [Sensor::Paw3311, Sensor::Paw3315, Sensor::Paw3395Se] {
            for v in [400, 800, 1600, 3200, 9600, 12_000] {
                let r = encode(s, v, v).unwrap_or_else(|| panic!("{s:?} {v}"));
                assert_eq!(decode(s, &r), Some((v, v)), "{s:?} {v}");
            }
        }
        // The table skips codes (7 is not a DPI code): decoding it fails.
        let bad = [7, 7, 0, crc(7, 7, 0)];
        assert_eq!(decode(Sensor::Paw3311, &bad), None);
    }

    #[test]
    fn bad_checksums_and_the_3955_are_refused() {
        let mut r = encode(Sensor::Paw3395, 800, 800).unwrap();
        r[3] ^= 1;
        assert_eq!(decode(Sensor::Paw3395, &r), None);
        assert_eq!(encode(Sensor::Paw3955, 800, 800), None);
        assert!(!Sensor::Paw3955.writable());
        assert_eq!(decode(Sensor::Paw3395, &[1, 2]), None);
    }

    #[test]
    fn lift_off_sets_follow_the_sensor() {
        assert_eq!(Sensor::Paw3950.lift_off().unwrap().len(), 3);
        assert_eq!(Sensor::Paw3950Ultra.lift_off().unwrap().len(), 2);
        assert!(Sensor::Paw3955.lift_off().is_none());
    }
}
