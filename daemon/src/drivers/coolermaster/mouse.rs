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

/// The MM530's `51 <cmd>` parameter command (`2b` on the MM7xx / MM3xx).
pub(super) const MM530_CMD: u8 = 0x2c;

/// MasterPlus+ levels 1, 3 and 5 (slow, medium, fast).
const BREATHING_SPEED: [u8; 3] = [0x3c, 0x31, 0x26];
const CYCLE_SPEED: [u8; 3] = [0x32, 0x28, 0x1e];
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

/// Speed code of the MM7xx / MM3xx `51 2b` report.
fn master_speed(look: &Look) -> u8 {
    speed(
        look,
        if look.effect == Effect::Cycle {
            CYCLE_SPEED
        } else {
            BREATHING_SPEED
        },
    )
}

/// `51 2b 00 00 mode speed flag ff ff bright R G B 00 00 00 ff..`; the flag
/// byte is 0x20 in breathing only (0xa0 would randomise the colour).
fn params(cmd: u8, look: &Look) -> Report {
    let m = mode(look.effect, 0xfe);
    let flag = if look.effect == Effect::Breathing {
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
        master_speed(look),
        flag,
        0xff,
        0xff,
        brightness(look),
        r,
        g,
        b,
    ]);
    p[17..].fill(0xff);
    p
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

/// MM530 / MM720 / MM730 / MM310 / wired MM712: `41 80` + `51 <cmd>` parameters
/// (unless off), then `41 80` + `51 28` select the mode, as MasterPlus+ does.
pub(super) fn master_packets(cmd: u8, look: &Look) -> Vec<Report> {
    let m = mode(look.effect, 0xfe);
    if cmd == MM530_CMD {
        // MasterPlus+ does not cover the MM530: OpenRGB's order and speed codes.
        let mut out = vec![report(&[0x41, 0x80]), report(&[0x51, 0x28, 0x00, 0x00, m])];
        if look.effect != Effect::Off {
            let mut p = params(cmd, look);
            p[6] = speed(look, BREATHING_SPEED);
            out.push(p);
        }
        return out;
    }
    let mut out = vec![report(&[0x41, 0x80])];
    if look.effect != Effect::Off {
        out.push(params(cmd, look));
        out.push(report(&[0x41, 0x80]));
    }
    out.push(report(&[0x51, 0x28, 0x00, 0x00, m]));
    out
}

/// MM711: `41 80`, `51 2b` parameters (also for off, where OpenRGB sends them
/// zeroed), `41 80`, `51 28` select.
pub(super) fn mm711_packets(look: &Look) -> Vec<Report> {
    let m = mode(look.effect, 0xfe);
    vec![
        report(&[0x41, 0x80]),
        params(0x2b, look),
        report(&[0x41, 0x80]),
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
        assert_eq!(p.len(), 4);
        assert_eq!(p[0][..3], [0x00, 0x41, 0x80]);
        assert_eq!(
            p[1][..17],
            [
                0x00, 0x51, 0x2b, 0x00, 0x00, 0x00, 0x31, 0x00, 0xff, 0xff, 0xff, 0x11, 0x22, 0x33,
                0x00, 0x00, 0x00
            ]
        );
        assert!(p[1][17..].iter().all(|&b| b == 0xff));
        assert_eq!(p[2][..3], [0x00, 0x41, 0x80]);
        assert_eq!(p[3][..6], [0x00, 0x51, 0x28, 0x00, 0x00, 0x00]);

        let mut l = look(Effect::Breathing);
        l.speed = 2;
        l.brightness = 50;
        let p = master_packets(0x2b, &l);
        assert_eq!(p[3][5], 0x01);
        assert_eq!(
            p[1][..14],
            [0, 0x51, 0x2b, 0, 0, 0x01, 0x26, 0x20, 0xff, 0xff, 127, 0x11, 0x22, 0x33]
        );
    }

    #[test]
    fn mm530_keeps_openrgbs_order() {
        let mut l = look(Effect::Breathing);
        l.speed = 2;
        l.brightness = 50;
        let p = master_packets(0x2c, &l);
        assert_eq!(p.len(), 3);
        assert_eq!(p[1][..6], [0, 0x51, 0x28, 0, 0, 0x01]);
        assert_eq!(
            p[2][..14],
            [0, 0x51, 0x2c, 0, 0, 0x01, 0x26, 0x20, 0xff, 0xff, 127, 0x11, 0x22, 0x33]
        );
        assert_eq!(master_packets(0x2c, &look(Effect::Off)).len(), 2);
    }

    #[test]
    fn master_cycle_and_off() {
        let mut l = look(Effect::Cycle);
        l.speed = 2;
        let p = master_packets(0x2b, &l);
        // MasterPlus+'s spectrum speed codes differ from breathing's; spectrum
        // brightness tops out at 0x7f and carries no colour or flag.
        assert_eq!(p[1][5..14], [0x02, 0x1e, 0x00, 0xff, 0xff, 0x7f, 0, 0, 0]);
        let p = master_packets(0x2b, &look(Effect::Off));
        assert_eq!(p.len(), 2);
        assert_eq!(p[1][..6], [0x00, 0x51, 0x28, 0x00, 0x00, 0xfe]);
    }

    #[test]
    fn mm711_layout() {
        let p = mm711_packets(&look(Effect::Breathing));
        assert_eq!(p.len(), 4);
        assert_eq!(p[0][..3], [0, 0x41, 0x80]);
        assert_eq!(
            p[1][..14],
            [0, 0x51, 0x2b, 0, 0, 0x01, 0x31, 0x20, 0xff, 0xff, 0xff, 0x11, 0x22, 0x33]
        );
        assert!(p[1][14..].iter().all(|&b| b == 0xff || b == 0));
        assert_eq!(p[2][..3], [0, 0x41, 0x80]);
        assert_eq!(p[3][..6], [0, 0x51, 0x28, 0, 0, 0x01]);
        let p = mm711_packets(&look(Effect::Off));
        assert_eq!(p[1][5..14], [0xfe, 0, 0, 0xff, 0xff, 0, 0, 0, 0]);
        let p = mm711_packets(&look(Effect::Static));
        assert_eq!(p[1][7], 0x00, "only breathing carries the 0x20 flag");
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
