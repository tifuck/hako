//! Clutch GM11 (`0db0:0d11`) lighting commands.
//!
//! MSI Center's `Class_Mouse_GM11` / Mystic Light `Class_GM11` (static
//! analysis, see `docs/vendor-re/msi.md`) and OpenRGB
//! `MSIClutchGM11Controller` agree: interface 2, feature report id 0 whose
//! payload is 8 bytes, the last one `0xff - sum(bytes 1..7)`; every command is
//! answered by a feature read of the same size. After the handshake
//! `03 03 0f f2 40`:
//! - `05 <style> 00 <brightness>` then `05 <style> 08 <param>` set a style
//!   (0 off, 1 wave, 2 steady, 3 breathing, 4 radar, 5 horizon, 6 ripple,
//!   7 reactive); the param is the colour index (steady), `speed * 16 +
//!   colour` (breathing, reactive) or `speed * 16 + direction` (the others);
//! - `10 00 18` plus a 24-byte output report (eight RGB palette entries; the
//!   seven fixed colours, then the custom one) sets colour 7, used for any RGB;
//! - reads: `8d` (style in byte 2), `85 <style>` (brightness byte 3, param
//!   byte 4).

use super::Effect;

pub(super) const EFFECTS: &[Effect] = &[
    Effect::new("static", "Static", 2, true, false),
    Effect::new("breathing", "Breathing", 3, true, true),
    Effect::new("wave", "Wave", 1, false, true),
    Effect::new("radar", "Radar", 4, false, true),
    Effect::new("horizon", "Horizon", 5, false, true),
    Effect::new("ripple", "Ripple", 6, false, true),
    Effect::new("reactive", "Reactive", 7, true, true),
    Effect::new("off", "Off", 0, false, false),
];

/// Feature report buffer: report id 0, seven payload bytes, checksum.
pub(super) type Cmd = [u8; 9];

pub(super) fn cmd(payload: &[u8]) -> Cmd {
    let mut c = [0u8; 9];
    c[1..=payload.len()].copy_from_slice(payload);
    let sum = c[1..8].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    c[8] = 0xff - sum;
    c
}

pub(super) fn handshake() -> Cmd {
    cmd(&[0x03, 0x03, 0x0f, 0xf2, 0x40])
}

/// The custom colour index (the eighth palette entry).
const CUSTOM: u8 = 7;

/// Output report of the palette: id 0, 24 colour bytes, zero fill.
pub(super) fn palette_report(rgb: [u8; 3]) -> [u8; 33] {
    let mut r = [0u8; 33];
    for (i, p) in super::gk30::PALETTE.iter().enumerate() {
        r[1 + 3 * i..4 + 3 * i].copy_from_slice(p);
    }
    r[22..25].copy_from_slice(&rgb);
    r
}

pub(super) fn palette_cmd() -> Cmd {
    cmd(&[0x10, 0x00, 0x18])
}

/// The two reports of a style; `speed` 1..=3.
pub(super) fn style(e: &Effect, brightness: u8, speed: u8) -> Vec<Cmd> {
    if e.style == 0 {
        return vec![cmd(&[0x05, 0x00])];
    }
    let param = match (e.color, e.speed) {
        (true, false) => CUSTOM,
        (true, true) => speed * 16 + CUSTOM,
        _ => speed * 16,
    };
    vec![
        cmd(&[0x05, e.style, 0x00, brightness]),
        cmd(&[0x05, e.style, 0x08, param]),
    ]
}

pub(super) const READ_STYLE: [u8; 2] = [0x8d, 0];

pub(super) fn read_params(style: u8) -> Cmd {
    cmd(&[0x85, style])
}

/// What `85 <style>` reports: brightness, speed (high nibble) and colour
/// (low nibble, for the styles that have one).
pub(super) fn parse_params(r: &[u8]) -> (u8, u8, u8) {
    (r[3], r[4] >> 4, r[4] & 0x0f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effect(name: &str) -> &'static Effect {
        EFFECTS.iter().find(|e| e.id == name).unwrap()
    }

    #[test]
    fn checksummed_commands() {
        // OpenRGB's handshake and MSI's: last byte is 0xff - sum.
        assert_eq!(handshake(), [0, 3, 3, 0x0f, 0xf2, 0x40, 0, 0, 0xb8]);
        assert_eq!(cmd(&[0x05, 0x02])[8], 0xff - 0x07);
        assert_eq!(cmd(&[0x10, 0x00, 0x18])[..4], [0, 0x10, 0x00, 0x18]);
    }

    #[test]
    fn styles() {
        let p = style(effect("static"), 200, 2);
        assert_eq!(p[0][1..8], [0x05, 0x02, 0x00, 200, 0, 0, 0]);
        assert_eq!(p[1][1..5], [0x05, 0x02, 0x08, 7]);
        let p = style(effect("breathing"), 10, 3);
        assert_eq!(p[1][1..5], [0x05, 0x03, 0x08, 0x37]);
        let p = style(effect("wave"), 10, 1);
        assert_eq!(p[1][1..5], [0x05, 0x01, 0x08, 0x10]);
        assert_eq!(style(effect("off"), 10, 1), vec![cmd(&[0x05, 0x00])]);
    }

    #[test]
    fn palette_has_custom_colour_last() {
        let r = palette_report([1, 2, 3]);
        assert_eq!(r[1..4], [255, 0, 0]);
        assert_eq!(r[19..22], [255, 255, 255]);
        assert_eq!(r[22..25], [1, 2, 3]);
        assert!(r[25..].iter().all(|&b| b == 0));
    }

    #[test]
    fn params_decode() {
        assert_eq!(parse_params(&[0, 0x85, 3, 150, 0x27]), (150, 2, 7));
    }
}
