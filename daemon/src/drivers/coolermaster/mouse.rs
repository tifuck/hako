//! MasterMouse lighting packets.
//!
//! OpenRGB `CMMMController` (MM530 / MM720 / MM730), `CMMM711Controller` and
//! `CMMM712Controller` + `MM712protocol.txt`. All three take the firmware
//! effect modes (static, breathing, spectrum cycle, off) with one colour,
//! a 0-255 brightness byte (0-0x7f in cycle) and a per-family speed byte; the
//! effects light every LED of the mouse. Slow / medium / fast are OpenRGB's
//! slowest, normal and fastest values; static sends the normal one, as
//! OpenRGB does. The direct (per-LED) modes are not used.

use super::{report, Effect, Look, Report};

pub(super) const EFFECTS: &[Effect] = &[
    Effect::Static,
    Effect::Breathing,
    Effect::Cycle,
    Effect::Off,
];

const MASTER_SPEED: [u8; 3] = [0x3c, 0x31, 0x26];
const MM711_SPEED: [u8; 3] = [0x5f, 0x38, 0x20];
const MM712_SPEED: [u8; 3] = [0x00, 0x02, 0x04];

/// Firmware mode: static 0, breathing 1, cycle 2, else `off` (MM7xx `fe`, MM712 3).
fn mode(effect: Effect, off: u8) -> u8 {
    match effect {
        Effect::Static => 0,
        Effect::Breathing => 1,
        Effect::Cycle => 2,
        _ => off,
    }
}

/// OpenRGB's off mode carries zero speed, brightness and colour.
fn speed(look: &Look, table: [u8; 3]) -> u8 {
    match look.effect {
        Effect::Off => 0,
        Effect::Static => table[1],
        _ => table[look.speed],
    }
}

fn brightness(look: &Look) -> u8 {
    let max = match look.effect {
        Effect::Off => 0,
        Effect::Cycle => 0x7f,
        _ => 0xff,
    };
    (u32::from(look.brightness.min(100)) * max / 100) as u8
}

fn color(look: &Look) -> [u8; 3] {
    match look.effect {
        Effect::Static | Effect::Breathing => look.rgb,
        _ => [0; 3],
    }
}

/// MM530 / MM720 / MM730: `41 80` init, `51 28` select mode, then (unless
/// off) `51 <cmd> 00 00 mode speed x ff ff bright R G B 00 00 00 ff..`.
pub(super) fn master_packets(cmd: u8, look: &Look) -> Vec<Report> {
    let m = mode(look.effect, 0xfe);
    let mut out = vec![report(&[0x41, 0x80]), report(&[0x51, 0x28, 0x00, 0x00, m])];
    if look.effect != Effect::Off {
        let x = if look.effect == Effect::Breathing {
            0x20
        } else {
            0x00
        };
        let [r, g, b] = color(look);
        let mut p = report(&[
            0x51,
            cmd,
            0x00,
            0x00,
            m,
            speed(look, MASTER_SPEED),
            x,
            0xff,
            0xff,
            brightness(look),
            r,
            g,
            b,
        ]);
        p[17..].fill(0xff);
        out.push(p);
    }
    out
}

/// MM711: `51 2b 00 00 mode speed 20 ff ff bright R G B`, then `51 28` select.
pub(super) fn mm711_packets(look: &Look) -> Vec<Report> {
    let m = mode(look.effect, 0xfe);
    let [r, g, b] = color(look);
    vec![
        report(&[
            0x51,
            0x2b,
            0x00,
            0x00,
            m,
            speed(look, MM711_SPEED),
            0x20,
            0xff,
            0xff,
            brightness(look),
            r,
            g,
            b,
        ]),
        report(&[0x51, 0x28, 0x00, 0x00, m]),
    ]
}

/// MM712 (interface 3): `4c 81 04 mode ff bright speed R G B ff` stores the
/// mode's look, `4c 81 08 mode` switches to it.
pub(super) fn mm712_packets(look: &Look) -> Vec<Report> {
    let m = mode(look.effect, 0x03);
    let [r, g, b] = color(look);
    vec![
        report(&[
            0x4c,
            0x81,
            0x04,
            m,
            0xff,
            brightness(look),
            speed(look, MM712_SPEED),
            r,
            g,
            b,
            0xff,
        ]),
        report(&[0x4c, 0x81, 0x08, m]),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::Dir;
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
    fn master_static_and_breathing() {
        let p = master_packets(0x2b, &look(Effect::Static));
        assert_eq!(p.len(), 3);
        assert_eq!(p[0][..3], [0x00, 0x41, 0x80]);
        assert_eq!(p[1][..6], [0x00, 0x51, 0x28, 0x00, 0x00, 0x00]);
        assert_eq!(
            p[2][..17],
            [
                0x00, 0x51, 0x2b, 0x00, 0x00, 0x00, 0x31, 0x00, 0xff, 0xff, 0xff, 0x11, 0x22, 0x33,
                0x00, 0x00, 0x00
            ]
        );
        assert!(p[2][17..].iter().all(|&b| b == 0xff));

        let mut l = look(Effect::Breathing);
        l.speed = 2;
        l.brightness = 50;
        let p = master_packets(0x2c, &l);
        assert_eq!(p[1][5], 0x01);
        assert_eq!(
            p[2][..14],
            [0, 0x51, 0x2c, 0, 0, 0x01, 0x26, 0x20, 0xff, 0xff, 127, 0x11, 0x22, 0x33]
        );
    }

    #[test]
    fn master_cycle_and_off() {
        let p = master_packets(0x2b, &look(Effect::Cycle));
        // Spectrum brightness tops out at 0x7f and carries no colour.
        assert_eq!(p[2][5..14], [0x02, 0x31, 0x00, 0xff, 0xff, 0x7f, 0, 0, 0]);
        let p = master_packets(0x2b, &look(Effect::Off));
        assert_eq!(p.len(), 2);
        assert_eq!(p[1][..6], [0x00, 0x51, 0x28, 0x00, 0x00, 0xfe]);
    }

    #[test]
    fn mm711_layout() {
        let p = mm711_packets(&look(Effect::Breathing));
        assert_eq!(
            p[0][..14],
            [0, 0x51, 0x2b, 0, 0, 0x01, 0x38, 0x20, 0xff, 0xff, 0xff, 0x11, 0x22, 0x33]
        );
        assert!(p[0][14..].iter().all(|&b| b == 0));
        assert_eq!(p[1][..6], [0, 0x51, 0x28, 0, 0, 0x01]);
        let p = mm711_packets(&look(Effect::Off));
        assert_eq!(p[0][5..14], [0xfe, 0, 0x20, 0xff, 0xff, 0, 0, 0, 0]);
    }

    #[test]
    fn mm712_layout() {
        let mut l = look(Effect::Breathing);
        l.speed = 0;
        let p = mm712_packets(&l);
        assert_eq!(
            p[0][..12],
            [0, 0x4c, 0x81, 0x04, 0x01, 0xff, 0xff, 0x00, 0x11, 0x22, 0x33, 0xff]
        );
        assert_eq!(p[1][..5], [0, 0x4c, 0x81, 0x08, 0x01]);
        let p = mm712_packets(&look(Effect::Off));
        assert_eq!(
            p[0][..12],
            [0, 0x4c, 0x81, 0x04, 0x03, 0xff, 0, 0, 0, 0, 0, 0xff]
        );
        assert_eq!(mm712_packets(&look(Effect::Static))[0][7], 0x02);
    }
}
