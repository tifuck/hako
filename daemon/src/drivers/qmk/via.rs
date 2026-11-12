//! VIA raw HID protocol: 32-byte commands on the QMK raw HID interface, each
//! answered by the same buffer with the values filled in, or with byte 0 set
//! to `id_unhandled` (0xFF). Command ids and value ids from QMK
//! `quantum/via.h` (protocol 0x000B-0x000D: custom value channels) and the
//! QMK 0.14-0.18 `via.h` / `via.c` (protocol 0x0009-0x000A: lighting ids
//! 0x80-0x83; vial-qmk `quantum/via.h` still reports protocol 0x0009 with
//! these ids). Report framing (report id 0, 32 bytes, reply must echo the
//! command bytes) from the-via/app `src/utils/keyboard-api.ts`; value ranges
//! (brightness and speed 0-255 per channel) from the-via/reader
//! `src/common-menus/qmk_*.ts`. Layouts only; no code is copied.
//!
//! `id_custom_save` (`09 <channel>`) makes the firmware store the channel's
//! current state in its own eeconfig block (QMK `via.c`
//! `via_qmk_*_save`); before protocol 0x000B it is `id_lighting_save`
//! (`09`), which stores every compiled-in lighting subsystem (QMK 0.18 /
//! vial-qmk `via.c`). The VIA app sends it after every lighting change
//! (`menusSlice.ts` `commitCustomMenu`), Vial on its Save button
//! (`keyboard_comm.py` `save_rgb`). The host never addresses the EEPROM.

/// VIA payload size (QMK `RAW_EPSIZE`).
pub const REPORT_LEN: usize = 32;

pub const ID_GET_PROTOCOL_VERSION: u8 = 0x01;
/// `id_custom_set_value`; `id_lighting_set_value` before protocol 0x000B.
pub const ID_CUSTOM_SET_VALUE: u8 = 0x07;
/// `id_custom_get_value`; `id_lighting_get_value` before protocol 0x000B.
pub const ID_CUSTOM_GET_VALUE: u8 = 0x08;
/// `id_custom_save`; `id_lighting_save` before protocol 0x000B.
pub const ID_CUSTOM_SAVE: u8 = 0x09;
pub const ID_UNHANDLED: u8 = 0xff;

/// First protocol with per-channel custom values (QMK 0.19).
pub const CHANNELS_SINCE: u16 = 0x000b;
/// Oldest protocol whose lighting ids were checked (QMK 0.14, protocol 9).
pub const LEGACY_SINCE: u16 = 0x0009;

/// A lighting subsystem the firmware exposes over VIA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// `id_qmk_backlight_channel` (1): single-color backlight.
    Backlight,
    /// `id_qmk_rgblight_channel` (2): RGB underglow / strips.
    Rgblight,
    /// `id_qmk_rgb_matrix_channel` (3): per-key RGB.
    RgbMatrix,
    /// `id_qmk_led_matrix_channel` (5, QMK 0.22+): per-key single color.
    LedMatrix,
    /// Protocol 9-10 `id_qmk_rgblight_*` lighting ids 0x80-0x83, served by
    /// rgblight or (QMK 0.18) RGB Matrix; speed is 0-3 there.
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Brightness,
    Effect,
    Speed,
    Color,
}

impl Channel {
    /// Detection order when several channels answer: the richest first.
    pub const MODERN: [Channel; 4] = [
        Channel::RgbMatrix,
        Channel::Rgblight,
        Channel::LedMatrix,
        Channel::Backlight,
    ];

    fn id(self) -> Option<u8> {
        match self {
            Channel::Backlight => Some(1),
            Channel::Rgblight => Some(2),
            Channel::RgbMatrix => Some(3),
            Channel::LedMatrix => Some(5),
            Channel::Legacy => None,
        }
    }

    pub fn has(self, v: Value) -> bool {
        match self {
            Channel::Backlight => matches!(v, Value::Brightness | Value::Effect),
            Channel::LedMatrix => v != Value::Color,
            Channel::Rgblight | Channel::RgbMatrix | Channel::Legacy => true,
        }
    }

    fn value_id(self, v: Value) -> u8 {
        let n = match v {
            Value::Brightness => 1,
            Value::Effect => 2,
            Value::Speed => 3,
            Value::Color => 4,
        };
        match self {
            // id_qmk_rgblight_brightness = 0x80 .. id_qmk_rgblight_color = 0x83
            Channel::Legacy => 0x7f + n,
            _ => n,
        }
    }

    /// Highest effect speed: rgblight (and the legacy RGB Matrix mapping)
    /// only know speeds 0-3; the channel protocol carries a full byte.
    pub fn speed_max(self) -> u8 {
        if self == Channel::Legacy {
            3
        } else {
            255
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Channel::Backlight => "backlight",
            Channel::Rgblight => "RGB light",
            Channel::RgbMatrix => "RGB matrix",
            Channel::LedMatrix => "LED matrix",
            Channel::Legacy => "lighting (VIA protocol 9-10)",
        }
    }
}

/// Command bytes up to the value data: `[cmd, channel, value]`, or
/// `[cmd, value]` for the legacy lighting ids.
fn header(cmd: u8, ch: Channel, v: Value) -> Vec<u8> {
    let mut h = vec![cmd];
    h.extend(ch.id());
    h.push(ch.value_id(v));
    h
}

pub fn get_value(ch: Channel, v: Value) -> Vec<u8> {
    header(ID_CUSTOM_GET_VALUE, ch, v)
}

pub fn set_value(ch: Channel, v: Value, data: &[u8]) -> Vec<u8> {
    let mut c = header(ID_CUSTOM_SET_VALUE, ch, v);
    c.extend_from_slice(data);
    c
}

/// Store the channel's current state in the keyboard's EEPROM: `[09, channel]`,
/// or `[09]` (all lighting) for the legacy ids. Reply header = command length.
pub fn save(ch: Channel) -> Vec<u8> {
    let mut c = vec![ID_CUSTOM_SAVE];
    c.extend(ch.id());
    c
}

pub fn protocol_version() -> Vec<u8> {
    vec![ID_GET_PROTOCOL_VERSION]
}

/// hidraw output report: report id 0, then the command zero-padded to 32 bytes.
pub fn report(cmd: &[u8]) -> [u8; REPORT_LEN + 1] {
    let mut r = [0u8; REPORT_LEN + 1];
    let n = cmd.len().min(REPORT_LEN);
    r[1..1 + n].copy_from_slice(&cmd[..n]);
    r
}

/// How a received report relates to the command `sent`.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    /// Echoed command; the bytes after the command's header.
    Data(&'a [u8]),
    /// The firmware does not handle this command / channel / value.
    Unhandled,
    /// Someone else's reply or input traffic.
    Other,
}

/// Match a reply against the command it answers. `header_len` bytes of the
/// command (command, channel, value id) must come back unchanged.
pub fn parse_reply<'a>(sent: &[u8], header_len: usize, got: &'a [u8]) -> Reply<'a> {
    let n = header_len.min(sent.len());
    if got.len() < n.max(1) || got[1..n] != sent[1..n] {
        return Reply::Other;
    }
    if got[0] == sent[0] {
        Reply::Data(&got[n..])
    } else if got[0] == ID_UNHANDLED {
        Reply::Unhandled
    } else {
        Reply::Other
    }
}

/// Header length of a value command on `ch`.
pub fn header_len(ch: Channel) -> usize {
    if ch == Channel::Legacy {
        2
    } else {
        3
    }
}

/// Big-endian protocol version from an `id_get_protocol_version` reply.
pub fn parse_version(data: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*data.first()?, *data.get(1)?]))
}

/// 0-100 % to the protocol's 0-255 brightness (the firmware scales it).
pub fn pct_to_byte(pct: i64) -> u8 {
    ((pct.clamp(0, 100) * 255 + 50) / 100) as u8
}

pub fn byte_to_pct(b: u8) -> i64 {
    (i64::from(b) * 100 + 127) / 255
}

/// RGB to QMK hue / saturation (both 0-255, hue 256 steps per turn). Value
/// is the separate brightness control.
pub fn rgb_to_hs([r, g, b]: [u8; 3]) -> [u8; 2] {
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    if max == 0.0 || delta == 0.0 {
        return [0, 0];
    }
    let sector = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    let hue = (sector / 6.0 * 256.0).round() as u32 % 256;
    [hue as u8, (delta / max * 255.0).round() as u8]
}

/// QMK hue / saturation at full value to RGB, for showing the device's color.
pub fn hs_to_rgb([h, s]: [u8; 2]) -> [u8; 3] {
    let sector = f64::from(h) / 256.0 * 6.0;
    let c = f64::from(s) / 255.0;
    let x = c * (1.0 - ((sector % 2.0) - 1.0).abs());
    let (r, g, b) = match sector as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = 1.0 - c;
    [r, g, b].map(|v| ((v + m) * 255.0).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_commands_match_via_h() {
        // id_custom_get_value, id_qmk_rgb_matrix_channel, id_qmk_rgb_matrix_effect
        assert_eq!(
            get_value(Channel::RgbMatrix, Value::Effect),
            [0x08, 0x03, 0x02]
        );
        // id_custom_set_value, id_qmk_rgblight_channel, id_qmk_rgblight_color, hue, sat
        assert_eq!(
            set_value(Channel::Rgblight, Value::Color, &[0x55, 0xff]),
            [0x07, 0x02, 0x04, 0x55, 0xff]
        );
        assert_eq!(
            set_value(Channel::Backlight, Value::Brightness, &[0x80]),
            [0x07, 0x01, 0x01, 0x80]
        );
        assert_eq!(
            set_value(Channel::LedMatrix, Value::Speed, &[0x10]),
            [0x07, 0x05, 0x03, 0x10]
        );
        // Pre-0x000B: id_lighting_get_value, id_qmk_rgblight_effect = 0x81.
        assert_eq!(get_value(Channel::Legacy, Value::Effect), [0x08, 0x81]);
        assert_eq!(
            set_value(Channel::Legacy, Value::Color, &[1, 2]),
            [0x07, 0x83, 1, 2]
        );
        assert_eq!(protocol_version(), [0x01]);
        // id_custom_save per channel; id_lighting_save alone before 0x000B.
        assert_eq!(save(Channel::Backlight), [0x09, 0x01]);
        assert_eq!(save(Channel::Rgblight), [0x09, 0x02]);
        assert_eq!(save(Channel::RgbMatrix), [0x09, 0x03]);
        assert_eq!(save(Channel::LedMatrix), [0x09, 0x05]);
        assert_eq!(save(Channel::Legacy), [0x09]);
        assert_eq!(
            report(&save(Channel::RgbMatrix))[..4],
            [0x00, 0x09, 0x03, 0x00]
        );
        assert!(!Channel::Backlight.has(Value::Speed));
        assert!(!Channel::LedMatrix.has(Value::Color));
        assert_eq!(Channel::Legacy.speed_max(), 3);
    }

    #[test]
    fn report_is_id_zero_plus_32_bytes() {
        let r = report(&[0x08, 0x03, 0x01]);
        assert_eq!(r.len(), 33);
        assert_eq!(r[..5], [0x00, 0x08, 0x03, 0x01, 0x00]);
        assert!(r[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn replies_are_matched_to_their_command() {
        let sent = get_value(Channel::RgbMatrix, Value::Color);
        let mut got = [0u8; 32];
        got[..5].copy_from_slice(&[0x08, 0x03, 0x04, 0x2a, 0xc8]);
        assert_eq!(
            parse_reply(&sent, 3, &got),
            Reply::Data(&got[3..]),
            "hue at data[0], sat at data[1]"
        );
        got[0] = ID_UNHANDLED;
        assert_eq!(parse_reply(&sent, 3, &got), Reply::Unhandled);
        // Another channel's reply, or a keymap reply, is not ours.
        got[..3].copy_from_slice(&[0x08, 0x02, 0x04]);
        assert_eq!(parse_reply(&sent, 3, &got), Reply::Other);
        got[..3].copy_from_slice(&[0x04, 0x03, 0x04]);
        assert_eq!(parse_reply(&sent, 3, &got), Reply::Other);
        assert_eq!(parse_reply(&sent, 3, &[]), Reply::Other);

        // id_get_protocol_version -> [0x01, hi, lo]
        let v = protocol_version();
        let mut got = [0u8; 32];
        got[..3].copy_from_slice(&[0x01, 0x00, 0x0c]);
        let Reply::Data(d) = parse_reply(&v, 1, &got) else {
            panic!("version reply");
        };
        assert_eq!(parse_version(d), Some(0x000c));
    }

    #[test]
    fn brightness_and_color_conversions() {
        assert_eq!(pct_to_byte(100), 255);
        assert_eq!(pct_to_byte(0), 0);
        assert_eq!(pct_to_byte(50), 128);
        for p in 0..=100 {
            assert_eq!(byte_to_pct(pct_to_byte(p)), p);
        }
        assert_eq!(rgb_to_hs([255, 0, 0]), [0, 255]);
        assert_eq!(rgb_to_hs([0, 255, 0]), [85, 255]);
        assert_eq!(rgb_to_hs([0, 0, 255]), [171, 255]);
        assert_eq!(rgb_to_hs([255, 255, 255]), [0, 0]);
        assert_eq!(rgb_to_hs([0, 0, 0]), [0, 0]);
        assert_eq!(rgb_to_hs([255, 0, 128]), [235, 255]);
        assert_eq!(hs_to_rgb([0, 255]), [255, 0, 0]);
        assert_eq!(hs_to_rgb([0, 0]), [255, 255, 255]);
        assert_eq!(hs_to_rgb([128, 255]), [0, 255, 255]);
        for rgb in [[255, 0, 0], [0, 0, 255], [255, 255, 0], [255, 128, 0]] {
            let back = hs_to_rgb(rgb_to_hs(rgb));
            assert!(
                back.iter().zip(rgb).all(|(&a, b)| a.abs_diff(b) <= 4),
                "{rgb:?} -> {back:?}"
            );
        }
    }
}
