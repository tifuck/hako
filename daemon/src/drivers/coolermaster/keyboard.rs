//! MasterKeys / CK / SK / MK keyboard lighting packets.
//!
//! V1 (MasterKeys Pro L / S): OpenRGB `CMKeyboardV1Controller.cpp`, matching
//! libcmmk. Static and off use manual mode (`41 02`, then eight `c0 02`
//! reports of 16 RGB slots fill all 128 key slots); breathing and wave use
//! effect mode (`41 01`, `51 28` select, `51 2c` parameters). The protocol has
//! no brightness byte, so brightness scales the colour. Speeds: OpenRGB's
//! slowest / default / fastest for breathing; for wave its range ends and the
//! midpoint (its default 0x29 lies outside its own range; libcmmk: any value
//! 0x00-0x50 animates). Color cycle is left out: OpenRGB's speeds for it
//! (0x68-0x96) contradict libcmmk's range.
//!
//! V2 (CK / SK / MK): OpenRGB `CMKeyboardV2Controller::SetMode`: `41 80` +
//! `52`, a `56 81` effect-class report, one `56 83` effect report covering
//! all 0xc1 LEDs, then `41 80` + `51 28 .. ff` (off: `51 28 .. 10` + `41 80`;
//! the all-zero report OpenRGB sends there carries no command and is skipped).
//! Speed index 1-5 (slow / medium / fast = 1 / 3 / 5) picks from OpenRGB's
//! per-effect speed tables.

use super::{report, Dir, Effect, Look, Report};

pub(super) const V1_EFFECTS: &[Effect] =
    &[Effect::Static, Effect::Breathing, Effect::Wave, Effect::Off];

pub(super) const V2_EFFECTS: &[Effect] = &[
    Effect::Static,
    Effect::Breathing,
    Effect::Cycle,
    Effect::Wave,
    Effect::Circle,
    Effect::Off,
];

const V1_BREATHE: [u8; 3] = [0x46, 0x36, 0x27];
const V1_WAVE: [u8; 3] = [0x48, 0x39, 0x2a];

fn scale(rgb: [u8; 3], pct: u8) -> [u8; 3] {
    rgb.map(|c| (u16::from(c) * u16::from(pct.min(100)) / 100) as u8)
}

pub(super) fn v1_packets(look: &Look) -> Vec<Report> {
    let rgb = scale(look.rgb, look.brightness);
    match look.effect {
        Effect::Breathing => v1_effect(0x01, V1_BREATHE[look.speed], 0x00, rgb),
        Effect::Wave => {
            let dir = match look.dir {
                Dir::Left => 0x00,
                Dir::Right => 0x04,
                Dir::Up => 0x06,
                Dir::Down => 0x02,
            };
            v1_effect(0x04, V1_WAVE[look.speed], dir, rgb)
        }
        Effect::Off => v1_fill([0; 3]),
        _ => v1_fill(rgb),
    }
}

/// `51 2c 00 00 id p1 p2 ff ff ff R G B 00 00 00` + 48 × `ff`.
fn v1_effect(id: u8, p1: u8, p2: u8, [r, g, b]: [u8; 3]) -> Vec<Report> {
    let mut p = report(&[
        0x51, 0x2c, 0x00, 0x00, id, p1, p2, 0xff, 0xff, 0xff, r, g, b, 0x00, 0x00, 0x00,
    ]);
    p[17..].fill(0xff);
    vec![
        report(&[0x41, 0x01]),
        report(&[0x51, 0x28, 0x00, 0x00, id]),
        p,
    ]
}

fn v1_fill(rgb: [u8; 3]) -> Vec<Report> {
    let mut out = vec![report(&[0x41, 0x02])];
    for i in 0..8u8 {
        let mut p = report(&[0xc0, 0x02, i * 2, 0x00]);
        for j in 0..16 {
            p[5 + 3 * j..8 + 3 * j].copy_from_slice(&rgb);
        }
        out.push(p);
    }
    out
}

/// Speed level 1..=5 as a table index.
const V2_LEVEL: [usize; 3] = [0, 2, 4];
const BREATHE: ([u8; 5], [u8; 5]) = (
    [0x08, 0x0a, 0x0c, 0x07, 0x09],
    [0x01, 0x02, 0x04, 0x04, 0x09],
);
const CYCLE: [u8; 5] = [0x10, 0x0c, 0x08, 0x04, 0x00];
const WAVE: ([u8; 5], [u8; 5]) = (
    [0x17, 0x0d, 0x07, 0x09, 0x08],
    [0x01, 0x04, 0x06, 0x0c, 0x11],
);
const CIRCLE_LEFT: ([u8; 5], [u8; 5]) = (
    [0xff, 0xfe, 0xfd, 0xfc, 0xfc],
    [0x04, 0x08, 0x08, 0x0c, 0x00],
);
const CIRCLE_RIGHT: ([u8; 5], [u8; 5]) = (
    [0x00, 0x01, 0x02, 0x03, 0x04],
    [0x0c, 0x08, 0x08, 0x04, 0x00],
);

pub(super) fn v2_packets(look: &Look) -> Vec<Report> {
    let br = (u16::from(look.brightness.min(100)) * 255 / 100) as u8;
    let i = V2_LEVEL[look.speed];
    let [r, g, b] = look.rgb;
    let class = |tag: u8| {
        report(&[
            0x56, 0x81, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, tag, tag, tag,
            tag,
        ])
    };
    let effect = |body: &[u8]| {
        let mut v = vec![0x56, 0x83, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
        v.extend_from_slice(body);
        report(&v)
    };
    let mut out = vec![report(&[0x41, 0x80]), report(&[0x52])];
    match look.effect {
        Effect::Off => {
            out.push(report(&[0x51, 0x28, 0x00, 0x00, 0x10]));
            out.push(report(&[0x41, 0x80]));
            return out;
        }
        Effect::Static => {
            out.push(class(0x88));
            out.push(effect(&[
                0x01, 0x00, 0xc1, 0x00, 0x00, 0x00, 0x00, r, g, b, br, 0x00, 0x00, 0x00, 0x00,
            ]));
        }
        Effect::Breathing => {
            let (s1, s2) = (BREATHE.0[i], BREATHE.1[i]);
            out.push(class(0x88));
            out.push(effect(&[
                0x30, 0x00, 0xc1, s1, 0x00, 0x00, 0x00, r, g, b, br, 0x01, 0x00, s2, 0x00,
            ]));
        }
        Effect::Cycle => {
            out.push(class(0x88));
            out.push(effect(&[
                0x31, 0x00, 0xc1, CYCLE[i], 0x00, 0x00, 0x00, 0x40, 0x00, 0xff, br, 0x00, 0x00,
                0x03, 0x00,
            ]));
        }
        Effect::Wave => {
            let (s1, s2) = (WAVE.0[i], WAVE.1[i]);
            let dir = match look.dir {
                Dir::Left => 0x04,
                Dir::Right => 0x00,
                Dir::Up => 0x06,
                Dir::Down => 0x02,
            };
            out.push(class(0x99));
            out.push(effect(&[
                0x32, 0x00, 0xc1, s1, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, br, 0x00, dir, s2, 0x00,
                0x00, 0x04, 0x08,
            ]));
        }
        Effect::Circle => {
            // Speed and direction share the two speed bytes.
            let t = if look.dir == Dir::Left {
                CIRCLE_LEFT
            } else {
                CIRCLE_RIGHT
            };
            let (s1, s2) = (t.0[i], t.1[i]);
            out.push(class(0x88));
            out.push(effect(&[
                0x34, 0x00, 0xc1, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, br, 0x00, 0x00, s1,
                0x00, 0x00, s2, 0x00, 0x04, 0xa0, 0x00, 0x30, 0x00,
            ]));
        }
    }
    out.push(report(&[0x41, 0x80]));
    out.push(report(&[0x51, 0x28, 0x00, 0x00, 0xff]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(effect: Effect) -> Look {
        Look {
            effect,
            brightness: 100,
            speed: 1,
            dir: Dir::Left,
            rgb: [0x11, 0x22, 0x33],
        }
    }

    #[test]
    fn v1_static_fills_all_slots() {
        let p = v1_packets(&look(Effect::Static));
        assert_eq!(p.len(), 9);
        assert_eq!(p[0][..3], [0, 0x41, 0x02]);
        assert_eq!(p[1][..8], [0, 0xc0, 0x02, 0x00, 0x00, 0x11, 0x22, 0x33]);
        assert_eq!(p[8][..5], [0, 0xc0, 0x02, 0x0e, 0x00]);
        // Slot 15 of each report ends at byte 52.
        assert_eq!(p[8][50..53], [0x11, 0x22, 0x33]);
        assert!(p[8][53..].iter().all(|&b| b == 0));
        let mut l = look(Effect::Static);
        l.brightness = 50;
        assert_eq!(v1_packets(&l)[1][5..8], [0x08, 0x11, 0x19]);
        assert_eq!(v1_packets(&look(Effect::Off))[3][5..8], [0, 0, 0]);
    }

    #[test]
    fn v1_effects() {
        let p = v1_packets(&look(Effect::Breathing));
        assert_eq!(p[0][..3], [0, 0x41, 0x01]);
        assert_eq!(p[1][..6], [0, 0x51, 0x28, 0, 0, 0x01]);
        assert_eq!(
            p[2][..17],
            [0, 0x51, 0x2c, 0, 0, 0x01, 0x36, 0x00, 0xff, 0xff, 0xff, 0x11, 0x22, 0x33, 0, 0, 0]
        );
        assert!(p[2][17..].iter().all(|&b| b == 0xff));
        let mut l = look(Effect::Wave);
        l.dir = Dir::Up;
        l.speed = 2;
        let p = v1_packets(&l);
        assert_eq!(p[2][5..9], [0x04, 0x2a, 0x06, 0xff]);
    }

    #[test]
    fn v2_static() {
        let p = v2_packets(&look(Effect::Static));
        assert_eq!(p.len(), 6);
        assert_eq!(p[0][..3], [0, 0x41, 0x80]);
        assert_eq!(p[1][..3], [0, 0x52, 0x00]);
        assert_eq!(
            p[2][..17],
            [0, 0x56, 0x81, 0, 0, 0x01, 0, 0, 0, 0x01, 0, 0, 0, 0x88, 0x88, 0x88, 0x88]
        );
        assert_eq!(
            p[3][..25],
            [
                0, 0x56, 0x83, 0, 0, 0x01, 0, 0, 0, 0x00, 0x01, 0x00, 0xc1, 0, 0, 0, 0, 0x11, 0x22,
                0x33, 0xff, 0, 0, 0, 0
            ]
        );
        assert_eq!(p[4][..3], [0, 0x41, 0x80]);
        assert_eq!(p[5][..6], [0, 0x51, 0x28, 0, 0, 0xff]);
    }

    #[test]
    fn v2_speed_tables_and_direction() {
        let mut l = look(Effect::Breathing);
        l.speed = 0;
        let p = v2_packets(&l);
        assert_eq!(
            p[3][10..25],
            [0x30, 0, 0xc1, 0x08, 0, 0, 0, 0x11, 0x22, 0x33, 0xff, 0x01, 0, 0x01, 0]
        );

        let p = v2_packets(&look(Effect::Cycle));
        assert_eq!(
            p[3][10..25],
            [0x31, 0, 0xc1, 0x08, 0, 0, 0, 0x40, 0, 0xff, 0xff, 0, 0, 0x03, 0]
        );

        let mut l = look(Effect::Wave);
        l.speed = 2;
        l.dir = Dir::Down;
        let p = v2_packets(&l);
        assert_eq!(p[2][13..17], [0x99; 4]);
        assert_eq!(
            p[3][10..28],
            [
                0x32, 0, 0xc1, 0x08, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0x02, 0x11, 0, 0, 0x04,
                0x08
            ]
        );

        let mut l = look(Effect::Circle);
        l.speed = 0;
        let p = v2_packets(&l);
        assert_eq!(
            p[3][10..33],
            [
                0x34, 0, 0xc1, 0x04, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0xff, 0, 0, 0x04, 0, 0x04,
                0xa0, 0, 0x30, 0
            ]
        );
        l.dir = Dir::Right;
        let p = v2_packets(&l);
        assert_eq!((p[3][23], p[3][26]), (0x00, 0x0c));
    }

    #[test]
    fn v2_off() {
        let p = v2_packets(&look(Effect::Off));
        assert_eq!(p.len(), 4);
        assert_eq!(p[2][..6], [0, 0x51, 0x28, 0, 0, 0x10]);
        assert_eq!(p[3][..3], [0, 0x41, 0x80]);
    }
}
