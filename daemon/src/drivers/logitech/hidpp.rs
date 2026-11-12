//! HID++ protocol layer: packet builders, reply parsers and the one blocking
//! request/reply transport every Logitech adapter here uses.
//!
//! Sources (layouts reimplemented, no code copied):
//! - Logitech cpg-docs `hidpp20/README.rst`, `features/0x0000-IRoot.rst`
//!   (packet layout, 0xff = corded device / receiver, ROOT GetFeature / ping).
//! - Solaar `lib/logitech_receiver/{base,receiver,hidpp10,hidpp10_constants,
//!   hidpp20,hidpp20_constants,settings_templates}.py` (register 0x2B5 pairing
//!   info, device kinds, battery/ADC/DPI/report-rate/LED function ids).
//! - libratbag `src/hidpp20.c` (0x2201 / 0x8060 / 0x8070 / 0x8100 function ids,
//!   0x8070 setZoneEffect layout).
//! - OpenLogi docs `openlogi.org/hidpp/features/` x8070 / x8071 / x1004 / x8300
//!   (persistence byte meaning, unified battery and sidetone layouts).
//! - Solaar `settings_templates.py` `PerKeyLighting` (0x8081 key bitmap fn 0,
//!   range update fn 5, FrameEnd fn 7) and `ADCPower` (0x1F20 v2+ auto-off
//!   fn 1 read / fn 2 write, minutes).

use crate::hid::HidrawDevice;
use anyhow::{bail, Result};
use hako_core::device::DeviceKind;
use std::time::{Duration, Instant};

/// Software id we tag requests with (low nibble of the function byte).
pub const SWID: u8 = 0x0a;
/// Device index for corded devices and the receiver itself.
pub const DIRECT: u8 = 0xff;

pub const FEAT_ROOT: u16 = 0x0000;
pub const FEAT_FEATURE_SET: u16 = 0x0001;
pub const FEAT_DEVICE_NAME: u16 = 0x0005;
pub const FEAT_BATTERY_STATUS: u16 = 0x1000;
pub const FEAT_BATTERY_VOLTAGE: u16 = 0x1001;
pub const FEAT_UNIFIED_BATTERY: u16 = 0x1004;
pub const FEAT_ADC: u16 = 0x1f20;
pub const FEAT_ADJ_DPI: u16 = 0x2201;
pub const FEAT_EXT_DPI: u16 = 0x2202;
pub const FEAT_BRIGHTNESS: u16 = 0x8040;
pub const FEAT_RR: u16 = 0x8060;
pub const FEAT_EXT_RR: u16 = 0x8061;
pub const FEAT_COLOR_LED: u16 = 0x8070;
pub const FEAT_RGB: u16 = 0x8071;
pub const FEAT_PER_KEY: u16 = 0x8081;
pub const FEAT_ONBOARD: u16 = 0x8100;
pub const FEAT_PROFILE_MGMT: u16 = 0x8101;
pub const FEAT_SIDETONE: u16 = 0x8300;
pub const FEAT_EQUALIZER: u16 = 0x8310;

/// HID++ 1.0 receiver register RECEIVER_INFO (long register 0x2B5).
pub const REG_RECEIVER_INFO: u8 = 0xb5;

/// Long request `[0x11, dev, feat, fn<<4 | swid, params..]`, zero padded.
pub fn long_request(dev_idx: u8, feat: u8, fnid: u8, params: &[u8]) -> [u8; 20] {
    let mut pkt = [0u8; 20];
    pkt[0] = 0x11;
    pkt[1] = dev_idx;
    pkt[2] = feat;
    pkt[3] = (fnid << 4) | SWID;
    let n = params.len().min(16);
    pkt[4..4 + n].copy_from_slice(&params[..n]);
    pkt
}

/// HID++ 1.0 long-register read sent to the receiver (sub-id 0x83).
/// Short form `[0x10, 0xff, 0x83, reg, sub, 0, 0]`; receivers whose
/// descriptor lacks report 0x10 get the same bytes in a long report.
pub fn register_read(reg: u8, sub: u8, short: bool) -> Vec<u8> {
    let mut pkt = vec![0x10, DIRECT, 0x83, reg, sub, 0, 0];
    if !short {
        pkt[0] = 0x11;
        pkt.resize(20, 0);
    }
    pkt
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    Err(u8),
    Other,
}

/// Classify a frame read after sending `[0x1x, dev, feat, fn_swid, ..]`.
/// A reply echoes feature index + function/software id; error frames carry
/// them at \[3\]/\[4\] behind 0xFF (HID++ 2.0) or 0x8F (HID++ 1.0). Notifications
/// use swid 0 and so never match.
pub fn match_reply(buf: &[u8], dev_idx: u8, feat: u8, fn_swid: u8) -> Reply {
    if buf.len() < 6 || !matches!(buf[0], 0x10 | 0x11) || !(buf[1] == dev_idx || buf[1] == 0xff) {
        return Reply::Other;
    }
    if matches!(buf[2], 0xff | 0x8f) && buf[3] == feat && buf[4] == fn_swid {
        return Reply::Err(buf[5]);
    }
    if buf[2] == feat && buf[3] == fn_swid {
        return Reply::Ok;
    }
    Reply::Other
}

pub fn drain_noise(dev: &mut HidrawDevice) {
    dev.drain(Duration::from_millis(1));
}

/// Write `pkt` and wait (bounded, 250 ms) for the reply echoing
/// `dev_idx`/`feat`/`fn_swid`, skipping input reports and notifications.
pub fn transact(
    dev: &mut HidrawDevice,
    pkt: &[u8],
    dev_idx: u8,
    feat: u8,
    fn_swid: u8,
) -> Result<[u8; 20]> {
    let fnid = fn_swid >> 4;
    drain_noise(dev);
    dev.write(pkt)?;
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut buf = [0u8; 32];
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Ok(n) = dev.read_timeout(&mut buf, remaining.min(Duration::from_millis(80))) {
            if n < 6 {
                continue;
            }
            match match_reply(&buf[..n], dev_idx, feat, fn_swid) {
                Reply::Ok => {
                    let mut out = [0u8; 20];
                    let copy = n.min(20);
                    out[..copy].copy_from_slice(&buf[..copy]);
                    return Ok(out);
                }
                Reply::Err(code) => bail!(
                    "HID++ error feat={feat:#x} fn={fnid} code={code:#x} raw={}",
                    hex(&buf[..n.min(12)])
                ),
                // Mouse input, notification, or someone else's reply.
                Reply::Other => {}
            }
        }
    }
    bail!("HID++ timeout feat={feat:#x} fn={fnid}")
}

/// One HID++ 2.0 call: long request to `feat` index, function `fnid`.
pub fn request(
    dev: &mut HidrawDevice,
    dev_idx: u8,
    feat: u8,
    fnid: u8,
    params: &[u8],
) -> Result<[u8; 20]> {
    let pkt = long_request(dev_idx, feat, fnid, params);
    transact(dev, &pkt, dev_idx, feat, pkt[3])
}

/// Read receiver long register `reg` / `sub`; the reply must echo `sub`.
pub fn read_long_register(
    dev: &mut HidrawDevice,
    reg: u8,
    sub: u8,
    short: bool,
) -> Result<[u8; 20]> {
    let r = transact(dev, &register_read(reg, sub, short), DIRECT, 0x83, reg)?;
    if r[4] != sub {
        bail!(
            "register {reg:#x} reply for sub {:#x}, wanted {sub:#x}",
            r[4]
        );
    }
    Ok(r)
}

/// Minimal hex for error messages without adding a crate.
pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Receiver pairing (HID++ 1.0) ─────────────────────────────────────────────

/// RECEIVER_INFO sub-register holding pairing info for paired `slot` (1..=6):
/// Unifying/Lightspeed 0x20 + slot - 1, Bolt 0x50 + slot (Solaar receiver.py).
pub fn pairing_sub(bolt: bool, slot: u8) -> u8 {
    if bolt {
        0x50 + slot
    } else {
        0x20 + slot - 1
    }
}

/// (wireless PID, HID++ 1.0 device kind) from a pairing-info register reply.
/// Data starts at \[4\] (sub echo). Unifying: wpid BE at data[3..5], kind in the
/// low nibble of data\[7\]. Bolt: wpid LE at data[2..4], kind in data\[1\].
pub fn parse_pairing(bolt: bool, reply: &[u8; 20]) -> Option<(u16, u8)> {
    let d = &reply[4..];
    let (wpid, kind) = if bolt {
        (u16::from_le_bytes([d[2], d[3]]), d[1] & 0x0f)
    } else {
        (u16::from_be_bytes([d[3], d[4]]), d[7] & 0x0f)
    };
    (wpid != 0).then_some((wpid, kind))
}

/// HID++ 1.0 device kind (pairing register) → Hako kind.
pub fn kind_from_hidpp10(kind: u8) -> DeviceKind {
    match kind {
        0x01 | 0x03 => DeviceKind::Keyboard,
        0x02 | 0x08 | 0x09 => DeviceKind::Mouse,
        0x0d => DeviceKind::Headset,
        _ => DeviceKind::Other,
    }
}

/// HID++ 2.0 DEVICE_NAME fn 2 device type → Hako kind.
pub fn kind_from_hidpp20(kind: u8) -> DeviceKind {
    match kind {
        0x00 | 0x02 => DeviceKind::Keyboard,
        0x03..=0x05 => DeviceKind::Mouse,
        0x08 => DeviceKind::Headset,
        _ => DeviceKind::Other,
    }
}

// ── Battery ──────────────────────────────────────────────────────────────────

/// (percent, charging) decoded from a battery feature reply's params.
pub type Battery = (Option<u8>, bool);

/// BatteryStatus byte shared by 0x1000 and 0x1004: 1 recharging, 2 almost
/// full (still charging), 4 slow recharge.
fn charging_status(status: u8) -> bool {
    matches!(status, 1 | 2 | 4)
}

/// 0x1000 fn 0: [level %, next level %, status]; level 0 = unknown.
pub fn parse_battery_status(p: &[u8]) -> Battery {
    ((p[0] > 0).then_some(p[0].min(100)), charging_status(p[2]))
}

/// 0x1004 fn 1: [state of charge %, level bucket, status]. Devices without a
/// gauge report 0 % and only the bucket (1 critical, 2 low, 4 good, 8 full).
pub fn parse_unified_battery(p: &[u8]) -> Battery {
    let pct = if p[0] > 0 {
        Some(p[0].min(100))
    } else {
        match p[1] {
            8 => Some(100),
            4 => Some(60),
            2 => Some(20),
            1 => Some(5),
            _ => None,
        }
    };
    (pct, charging_status(p[2]))
}

/// 0x1001 fn 0: [mV BE u16, flags]; flags bit 7 = external power / charging.
pub fn parse_battery_voltage(p: &[u8]) -> Battery {
    let mv = u16::from_be_bytes([p[0], p[1]]);
    (percent_from_mv(mv), p[2] & 0x80 != 0)
}

/// 0x1F20 fn 0 (headsets): [mV BE u16, flags]; bit 0 = reading valid (headset
/// on), bit 1 = charging. `None` while the headset is off.
pub fn parse_adc(p: &[u8]) -> Option<Battery> {
    let mv = u16::from_be_bytes([p[0], p[1]]);
    (p[2] & 0x01 != 0).then(|| (percent_from_mv(mv), p[2] & 0x02 != 0))
}

/// Rough single-cell Li-ion curve (resting voltage → charge), interpolated.
/// ponytail: one generic curve; per-model curves if readings prove off.
pub fn percent_from_mv(mv: u16) -> Option<u8> {
    const CURVE: [(u16, u8); 9] = [
        (4200, 100),
        (4100, 90),
        (4000, 78),
        (3900, 62),
        (3800, 45),
        (3750, 33),
        (3700, 20),
        (3600, 6),
        (3500, 0),
    ];
    if mv == 0 {
        return None;
    }
    if mv >= CURVE[0].0 {
        return Some(100);
    }
    for w in CURVE.windows(2) {
        let ((hi_mv, hi_p), (lo_mv, lo_p)) = (w[0], w[1]);
        if mv >= lo_mv {
            let span = u32::from(hi_mv - lo_mv);
            let p = u32::from(lo_p) + u32::from(hi_p - lo_p) * u32::from(mv - lo_mv) / span;
            return Some(p as u8);
        }
    }
    Some(0)
}

// ── DPI / report rate ────────────────────────────────────────────────────────

/// Expand a DPI list (0x2201 fn 1 after the sensor byte, 0x2202 fn 2 after the
/// 3-byte echo): big-endian u16 values, 0 terminates; a value whose top three
/// bits are set is a step `v & 0x1fff` from the previous value up to the next.
pub fn parse_dpi_list(bytes: &[u8]) -> Vec<u16> {
    let vals: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .take_while(|&v| v != 0)
        .collect();
    let mut out: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < vals.len() {
        let v = vals[i];
        if v >> 13 == 0b111 {
            let step = v & 0x1fff;
            match (out.last().copied(), vals.get(i + 1)) {
                (Some(from), Some(&to)) if step > 0 && to > from => {
                    out.extend(
                        (u32::from(from) + u32::from(step)..=u32::from(to))
                            .step_by(step as usize)
                            .map(|d| d as u16),
                    );
                }
                _ => return out,
            }
            i += 2;
        } else {
            out.push(v);
            i += 1;
        }
    }
    out
}

/// `(min, max, smallest step)` when a slider suits `dpis`: one evenly spaced
/// run, or too many values for a dropdown (writes snap to the list).
pub fn dpi_slider(dpis: &[u16]) -> Option<(u16, u16, u16)> {
    let (&min, &max) = (dpis.first()?, dpis.last()?);
    let gaps = || dpis.windows(2).map(|w| w[1].saturating_sub(w[0]));
    let step = gaps().min().unwrap_or(1).max(1);
    (dpis.len() > 16 || gaps().all(|g| g == step)).then_some((min, max, step))
}

/// Extended report-rate index → Hz (Solaar ExtendedReportRate).
pub const EXT_RR_HZ: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

/// Rates whose bit is set in the 0x8061 fn 0x10 flags (bit i = EXT_RR_HZ\[i\]).
pub fn ext_rr_supported(flags: u16) -> Vec<u32> {
    EXT_RR_HZ
        .iter()
        .enumerate()
        .filter(|(i, _)| flags & (1 << i) != 0)
        .map(|(_, &hz)| hz)
        .collect()
}

/// 0x8060 fn 0 flags: bit i set => an (i+1) ms period is supported.
pub fn rr_supported(flags: u8) -> Vec<u32> {
    [(8u8, 125u32), (4, 250), (2, 500), (1, 1000)]
        .into_iter()
        .filter(|(ms, _)| flags & (1 << (ms - 1)) != 0)
        .map(|(_, hz)| hz)
        .collect()
}

// ── Lighting (0x8070 / 0x8071) ───────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MouseEffectKind {
    Off,
    Static,
    Pulse,
    Cycle,
    Breathe,
}

impl MouseEffectKind {
    pub const ALL: [(Self, &'static str, &'static str); 5] = [
        (Self::Off, "off", "Off"),
        (Self::Static, "static", "Static"),
        (Self::Pulse, "pulse", "Pulse"),
        (Self::Cycle, "cycle", "Cycle"),
        (Self::Breathe, "breathe", "Breathe"),
    ];

    pub fn from_str(s: &str) -> Self {
        match s {
            "off" => Self::Off,
            "pulse" => Self::Pulse,
            "cycle" => Self::Cycle,
            "breathe" => Self::Breathe,
            _ => Self::Static,
        }
    }
    /// The 0x8070/0x8071 effect id this maps to (used to find the slot index).
    pub fn effect_id(self) -> u16 {
        match self {
            Self::Off => 0x00,
            Self::Static => 0x01,
            Self::Pulse => 0x02,
            Self::Cycle => 0x03,
            Self::Breathe => 0x0A,
        }
    }
    pub fn has_speed(self) -> bool {
        matches!(self, Self::Pulse | Self::Breathe)
    }
    pub fn has_color(self) -> bool {
        self.has_speed() || self == Self::Static
    }
}

/// 1-byte pulse speed (lower = faster).
fn mouse_pulse_speed(speed: &str) -> u8 {
    match speed {
        "slow" => 0x08,
        "fast" => 0x02,
        _ => 0x04,
    }
}

/// Period in ms for breathe/cycle (shorter = faster).
fn mouse_period_ms(speed: &str) -> u16 {
    match speed {
        "slow" => 5000,
        "fast" => 1000,
        _ => 3000,
    }
}

/// Build the 13-byte SetRgbClusterEffect payload (fn 0x10), as Solaar's
/// RGBEffectSetting: zone, effect slot, 10 param bytes (LEDEffects offsets),
/// suffix 0x01. On 0x8071 the suffix is the persistence flags (bit 0 = RAM,
/// bit 1 = EEPROM), so 0x01 is volatile; 0x8070 setZoneEffect uses the same
/// first 12 bytes with byte 12 = persistence (0 = RAM only), see
/// [`zone_effect_payload`].
pub fn mouse_effect_payload(
    cluster: u8,
    slot: u8,
    effect: MouseEffectKind,
    r: u8,
    g: u8,
    b: u8,
    speed: &str,
) -> [u8; 13] {
    let mut p = [0u8; 13];
    p[0] = cluster;
    p[1] = slot;
    // p[2..12] are the 10 param bytes (descriptor bytes 1-10).
    match effect {
        MouseEffectKind::Off => {}
        MouseEffectKind::Static => {
            p[2] = r;
            p[3] = g;
            p[4] = b;
            // p[5] = ramp = 0 (default)
        }
        MouseEffectKind::Pulse => {
            p[2] = r;
            p[3] = g;
            p[4] = b;
            p[5] = mouse_pulse_speed(speed);
        }
        MouseEffectKind::Cycle => {
            let period = mouse_period_ms(speed).to_be_bytes();
            p[7] = period[0]; // descriptor byte6
            p[8] = period[1]; // descriptor byte7
            p[9] = 100; // intensity (descriptor byte8)
        }
        MouseEffectKind::Breathe => {
            p[2] = r;
            p[3] = g;
            p[4] = b;
            let period = mouse_period_ms(speed).to_be_bytes();
            p[5] = period[0]; // descriptor byte4
            p[6] = period[1]; // descriptor byte5
                              // p[7] = form = 0 (default)
            p[8] = 100; // intensity (descriptor byte7)
        }
    }
    p[12] = 0x01;
    p
}

/// First and last key id set in a 0x8081 key bitmap (fn 0 pages 0..=2, each
/// reply's params after the 2-byte echo, concatenated; bit i = key id i,
/// ids 1..=254).
pub fn perkey_range(bitmap: &[u8]) -> Option<(u8, u8)> {
    let mut keys = (1..255u8).filter(|&i| {
        bitmap
            .get(usize::from(i / 8))
            .is_some_and(|b| b >> (i % 8) & 1 == 1)
    });
    let first = keys.next()?;
    Some((first, keys.next_back().unwrap_or(first)))
}

/// 0x8081 fn 5 range update: every key from `first` to `last` gets one
/// colour; it shows after FrameEnd (fn 7 `[0x00]`).
pub fn perkey_range_payload(first: u8, last: u8, r: u8, g: u8, b: u8) -> [u8; 5] {
    [first, last, r, g, b]
}

/// 0x8070 setZoneEffect (fn 3): same zone/slot/params, persistence 0 (RAM only).
pub fn zone_effect_payload(
    zone: u8,
    slot: u8,
    effect: MouseEffectKind,
    r: u8,
    g: u8,
    b: u8,
    speed: &str,
) -> [u8; 13] {
    let mut p = mouse_effect_payload(zone, slot, effect, r, g, b, speed);
    p[12] = 0x00;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_request_layout() {
        // ROOT GetFeature(0x2201) on device 1: 11 01 00 0a 22 01 00 ..
        let p = long_request(1, 0x00, 0x00, &[0x22, 0x01, 0x00]);
        assert_eq!(&p[..7], &[0x11, 0x01, 0x00, 0x0a, 0x22, 0x01, 0x00]);
        assert!(p[7..].iter().all(|&b| b == 0));
        // Params past 16 bytes are cut.
        let p = long_request(0xff, 0x0b, 0x01, &[0xaa; 20]);
        assert_eq!(&p[..4], &[0x11, 0xff, 0x0b, 0x1a]);
        assert_eq!(&p[4..], &[0xaa; 16]);
    }

    #[test]
    fn register_read_layout() {
        // Unifying pairing info for slot 1: 10 ff 83 b5 20 00 00
        assert_eq!(
            register_read(REG_RECEIVER_INFO, pairing_sub(false, 1), true),
            vec![0x10, 0xff, 0x83, 0xb5, 0x20, 0, 0]
        );
        // Bolt slot 2 uses 0x52; long form keeps the bytes, padded to 20.
        let long = register_read(REG_RECEIVER_INFO, pairing_sub(true, 2), false);
        assert_eq!(&long[..5], &[0x11, 0xff, 0x83, 0xb5, 0x52]);
        assert_eq!(long.len(), 20);
    }

    #[test]
    fn pairing_info_parsing() {
        // Unifying: data = [sub, dest, interval, wpid hi, wpid lo, _, _, kind]
        let mut r = [0u8; 20];
        r[..12].copy_from_slice(&[
            0x11, 0xff, 0x83, 0xb5, 0x20, 0x00, 0x08, 0x40, 0x82, 0, 0, 0x02,
        ]);
        assert_eq!(parse_pairing(false, &r), Some((0x4082, 0x02)));
        // Bolt: data = [sub, kind, wpid lo, wpid hi, serial..]
        let mut b = [0u8; 20];
        b[..8].copy_from_slice(&[0x11, 0xff, 0x83, 0xb5, 0x51, 0x01, 0x8a, 0x40]);
        assert_eq!(parse_pairing(true, &b), Some((0x408a, 0x01)));
        // Empty slot.
        assert_eq!(parse_pairing(false, &[0u8; 20]), None);
        assert_eq!(kind_from_hidpp10(0x02), DeviceKind::Mouse);
        assert_eq!(kind_from_hidpp10(0x01), DeviceKind::Keyboard);
        assert_eq!(kind_from_hidpp20(0x08), DeviceKind::Headset);
        assert_eq!(kind_from_hidpp20(0x03), DeviceKind::Mouse);
    }

    #[test]
    fn battery_parsing() {
        assert_eq!(parse_battery_status(&[50, 20, 0]), (Some(50), false));
        assert_eq!(parse_battery_status(&[0, 0, 1]), (None, true));
        assert_eq!(parse_unified_battery(&[87, 4, 1]), (Some(87), true));
        // No gauge: level bucket only.
        assert_eq!(parse_unified_battery(&[0, 2, 0]), (Some(20), false));
        // 0x0F3C = 3900 mV, charging.
        assert_eq!(parse_battery_voltage(&[0x0f, 0x3c, 0x80]), (Some(62), true));
        assert_eq!(parse_adc(&[0x10, 0x68, 0x03]), Some((Some(100), true)));
        assert_eq!(parse_adc(&[0x00, 0x00, 0x00]), None);
        assert_eq!(percent_from_mv(3850), Some(53));
        assert_eq!(percent_from_mv(3400), Some(0));
        assert_eq!(percent_from_mv(0), None);
    }

    #[test]
    fn dpi_lists() {
        // 100, step 50, 4000 -> a slider.
        let v = parse_dpi_list(&[0x00, 0x64, 0xe0, 0x32, 0x0f, 0xa0, 0x00, 0x00]);
        assert_eq!(v.len(), 79);
        assert_eq!(dpi_slider(&v), Some((100, 4000, 50)));
        // Discrete list 400/800/1600 -> no slider.
        let v = parse_dpi_list(&[0x01, 0x90, 0x03, 0x20, 0x06, 0x40, 0, 0, 0x12, 0x34]);
        assert_eq!(v, vec![400, 800, 1600]);
        assert_eq!(dpi_slider(&v), None);
        // Mixed runs (100..1000 by 50, then ..2000 by 100) still get a slider.
        let v = parse_dpi_list(&[0x00, 0x64, 0xe0, 0x32, 0x03, 0xe8, 0xe0, 0x64, 0x07, 0xd0]);
        assert_eq!(v.len(), 29);
        assert_eq!(dpi_slider(&v), Some((100, 2000, 50)));
        // A step with nothing before it is malformed: stop.
        assert!(parse_dpi_list(&[0xe0, 0x32, 0x0f, 0xa0]).is_empty());
    }

    #[test]
    fn report_rate_flags_are_big_endian() {
        // Reply bytes 00 7f -> u16 0x007f -> all seven rates.
        assert_eq!(
            ext_rr_supported(u16::from_be_bytes([0x00, 0x7f])),
            vec![125, 250, 500, 1000, 2000, 4000, 8000]
        );
        // 00 0f -> 125..1000 only (little-endian read would give 0x0f00 -> none).
        assert_eq!(
            ext_rr_supported(u16::from_be_bytes([0x00, 0x0f])),
            vec![125, 250, 500, 1000]
        );
        // 0x8060: bits 0,1,3,7 -> 1, 2, 4, 8 ms.
        assert_eq!(rr_supported(0x8b), vec![125, 250, 500, 1000]);
        assert_eq!(rr_supported(0x01), vec![1000]);
    }

    #[test]
    fn reply_matching() {
        let fs = (0x05 << 4) | SWID;
        // Reply to our fn 0x50 on feature 0x0a.
        assert_eq!(
            match_reply(&[0x11, 1, 0x0a, fs, 1, 3], 1, 0x0a, fs),
            Reply::Ok
        );
        // Same feature, swid 0 = unsolicited notification.
        assert_eq!(
            match_reply(&[0x11, 1, 0x0a, 0x00, 1, 3], 1, 0x0a, fs),
            Reply::Other
        );
        // HID++ 2.0 error for our request.
        assert_eq!(
            match_reply(&[0x11, 1, 0xff, 0x0a, fs, 0x05], 1, 0x0a, fs),
            Reply::Err(0x05)
        );
        // HID++ 1.0 error (mouse offline), same shape as seen on hw for a
        // ROOT fn1 request tagged swid 0xb: 10 01 8f 00 1b 09 00.
        assert_eq!(
            match_reply(&[0x10, 1, 0x8f, 0x00, 0x1a, 0x09, 0x00], 1, 0x00, 0x1a),
            Reply::Err(0x09)
        );
        // Someone else's error (other feature) is not ours.
        assert_eq!(
            match_reply(&[0x11, 1, 0xff, 0x0b, fs, 0x05], 1, 0x0a, fs),
            Reply::Other
        );
        // Receiver register reply / error (sub-id 0x83, address 0xb5).
        assert_eq!(
            match_reply(&[0x11, 0xff, 0x83, 0xb5, 0x20, 0], 0xff, 0x83, 0xb5),
            Reply::Ok
        );
        assert_eq!(
            match_reply(&[0x10, 0xff, 0x8f, 0x83, 0xb5, 0x03, 0], 0xff, 0x83, 0xb5),
            Reply::Err(0x03)
        );
    }

    #[test]
    fn perkey_bitmap_and_range() {
        // Keys 4..=7 on page 0 (byte 0 bits 4-7) and 0x70 (byte 14 = page 1).
        let mut bm = [0u8; 42];
        bm[0] = 0xf0;
        bm[14] = 0x01;
        assert_eq!(perkey_range(&bm), Some((4, 0x70)));
        // Bit 0 (id 0) is not a key; one key gives first == last.
        assert_eq!(perkey_range(&[0x01, 0x02]), Some((9, 9)));
        assert_eq!(perkey_range(&[0x01]), None);
        assert_eq!(
            perkey_range_payload(4, 0x70, 0xff, 0x10, 0x00),
            [0x04, 0x70, 0xff, 0x10, 0x00]
        );
        // Range update and FrameEnd on feature index 0x0d, device 0xff.
        assert_eq!(
            &long_request(0xff, 0x0d, 0x05, &[4, 0x70, 1, 2, 3])[..9],
            &[0x11, 0xff, 0x0d, 0x5a, 4, 0x70, 1, 2, 3]
        );
        assert_eq!(
            &long_request(0xff, 0x0d, 0x07, &[0])[..5],
            &[0x11, 0xff, 0x0d, 0x7a, 0]
        );
    }

    #[test]
    fn mouse_effect_payload_layouts() {
        // Static red, slot 1, cluster 0: [0,1, R,G,B, ramp=0, 0..., 0x01]
        assert_eq!(
            mouse_effect_payload(0, 1, MouseEffectKind::Static, 0xFF, 0, 0, "medium"),
            [0, 1, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        // Off, slot 0: no params, suffix still 0x01
        assert_eq!(
            mouse_effect_payload(0, 0, MouseEffectKind::Off, 0, 0, 0, "medium"),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        // Pulse green fast: speed byte at param[3]=0x02
        assert_eq!(
            mouse_effect_payload(0, 2, MouseEffectKind::Pulse, 0, 0xFF, 0, "fast"),
            [0, 2, 0, 0xFF, 0, 0x02, 0, 0, 0, 0, 0, 0, 1]
        );
        // Breathe blue medium: color + period be16 (3000=0x0BB8) at param[3..5] + intensity param[6]=100
        assert_eq!(
            mouse_effect_payload(0, 3, MouseEffectKind::Breathe, 0, 0, 0xFF, "medium"),
            [0, 3, 0, 0, 0xFF, 0x0B, 0xB8, 0, 100, 0, 0, 0, 1]
        );
        // Cycle medium: period be16 (3000=0x0BB8) at param[5..7], intensity param[7]=100
        assert_eq!(
            mouse_effect_payload(0, 4, MouseEffectKind::Cycle, 0, 0, 0, "medium"),
            [0, 4, 0, 0, 0, 0, 0, 0x0B, 0xB8, 100, 0, 0, 1]
        );
        // 0x8070 zone effect: identical params, persistence byte 0 (RAM only).
        assert_eq!(
            zone_effect_payload(2, 1, MouseEffectKind::Static, 1, 2, 3, "medium"),
            [2, 1, 1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }
}
