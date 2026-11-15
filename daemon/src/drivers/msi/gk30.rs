//! Vigor GK30 (`0db0:0b30`) lighting reports.
//!
//! MSI Center's Mystic Light module (`Class_GK30`, static analysis, see
//! `docs/vendor-re/msi.md`) and OpenRGB `MSIVigorGK30Controller` agree on the
//! 8-byte feature report with id 7 on interface 1:
//! `07 ff <style> <colour> <direction> <speed> <brightness> 00`. MSI's code
//! adds the read-back (`07 fd`, then a feature read of report 7: style,
//! colour, direction, speed, brightness in bytes 1..5) and `07 fa` (factory
//! defaults, never sent here). The colour is a palette index: red, yellow,
//! green, cyan, blue, magenta, white. MSI's own read path takes one off the
//! stored breathing colour, so the breathing write adds one.

use super::Effect;

pub(super) const EFFECTS: &[Effect] = &[
    Effect::new("static", "Static", 0x10, true, false),
    Effect::new("breathing", "Breathing", 0x20, true, true),
    Effect::new("rainbow", "Rainbow", 0x30, false, true),
    Effect::new("meteor", "Meteor", 0x40, false, true),
    Effect::new("ripple", "Ripple", 0x50, false, true),
    Effect::new("dimming", "Dimming", 0x60, false, true),
    Effect::new("off", "Off", 0x00, false, false),
];

pub(super) const PALETTE: [[u8; 3]; 7] = [
    [255, 0, 0],
    [255, 255, 0],
    [0, 255, 0],
    [0, 255, 255],
    [0, 0, 255],
    [255, 0, 255],
    [255, 255, 255],
];

/// Brightness and speed levels run 1..=3.
pub(super) const LEVELS: (u8, u8) = (1, 3);

/// The palette entry closest to `rgb`.
pub(super) fn nearest(rgb: [u8; 3]) -> u8 {
    let dist = |p: &[u8; 3]| -> i32 {
        p.iter()
            .zip(rgb)
            .map(|(&a, b)| (i32::from(a) - i32::from(b)).pow(2))
            .sum()
    };
    PALETTE
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| dist(p))
        .map_or(0, |(i, _)| i as u8)
}

pub(super) fn write(e: &Effect, colour: u8, speed: u8, brightness: u8) -> [u8; 8] {
    if e.style == 0 {
        // MSI's "off": colour byte ff, nothing else.
        return [0x07, 0xff, 0x00, 0xff, 0, 0, 0, 0];
    }
    let colour = if e.style == 0x20 { colour + 1 } else { colour };
    let speed = if e.speed { speed } else { 0 };
    [0x07, 0xff, e.style, colour, 0, speed, brightness, 0]
}

pub(super) const READ_REQUEST: [u8; 8] = [0x07, 0xfd, 0, 0, 0, 0, 0, 0];

/// What `07 fd` reports.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct State {
    pub style: u8,
    /// Palette index (breathing corrected).
    pub colour: u8,
    pub speed: u8,
    pub brightness: u8,
}

pub(super) fn parse(r: &[u8; 8]) -> State {
    let colour = if r[1] == 0x20 {
        r[2].saturating_sub(1)
    } else {
        r[2]
    };
    State {
        style: r[1],
        colour,
        speed: r[4],
        brightness: r[5],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(name: &str) -> &'static Effect {
        EFFECTS.iter().find(|e| e.id == name).unwrap()
    }

    #[test]
    fn write_layouts() {
        assert_eq!(
            write(effect("static"), 4, 2, 3),
            [0x07, 0xff, 0x10, 4, 0, 0, 3, 0]
        );
        assert_eq!(
            write(effect("breathing"), 4, 2, 3),
            [0x07, 0xff, 0x20, 5, 0, 2, 3, 0]
        );
        assert_eq!(
            write(effect("ripple"), 4, 2, 3),
            [0x07, 0xff, 0x50, 4, 0, 2, 3, 0]
        );
        assert_eq!(
            write(effect("off"), 4, 2, 3),
            [0x07, 0xff, 0, 0xff, 0, 0, 0, 0]
        );
    }

    #[test]
    fn read_back_layout() {
        assert_eq!(READ_REQUEST[..2], [0x07, 0xfd]);
        let s = parse(&[0x07, 0x10, 6, 0, 0, 3, 0, 0]);
        assert_eq!(
            s,
            State {
                style: 0x10,
                colour: 6,
                speed: 0,
                brightness: 3
            }
        );
        // Breathing stores the palette index plus one.
        assert_eq!(parse(&[0x07, 0x20, 5, 0, 2, 1, 0, 0]).colour, 4);
        assert_eq!(parse(&[0x07, 0x20, 0, 0, 2, 1, 0, 0]).colour, 0);
    }

    #[test]
    fn colours_snap_to_the_palette() {
        assert_eq!(nearest([255, 0, 0]), 0);
        assert_eq!(nearest([250, 240, 10]), 1);
        assert_eq!(nearest([0, 0, 200]), 4);
        assert_eq!(PALETTE[nearest([255, 255, 255]) as usize], [255, 255, 255]);
    }
}
