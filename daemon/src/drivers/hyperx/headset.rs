//! HyperX wireless headsets. Every family is a small request/reply protocol on
//! the dongle's vendor HID collection; builders and parsers here are pure.
//!
//! Byte layouts per family (see `mod.rs` for the projects):
//! - Flight: HeadsetControl `hyperx_cloud_flight.hpp`, HyperHeadset `cloud_flight_wireless.rs`
//! - Alpha: HeadsetControl `hyperx_cloud_alpha_wireless.hpp`, HyperHeadset `cloud_alpha_wireless.rs`
//! - Cloud2Kingston: HeadsetControl `hyperx_cloud_2_wireless_kingston.hpp`,
//!   HyperHeadset `cloud_ii_wireless.rs`
//! - Cloud2Hp: HeadsetControl `hyperx_cloud_2_wireless.hpp`, HyperHeadset `cloud_ii_wireless_dts.rs`;
//!   Cloud Stinger 2 Wireless (03f0:0d93) and its sidetone volume (`0b` get / `23` set,
//!   one byte) also from hsctl `docs/COMMANDS.md` (MIT): <https://github.com/oauthul/hsctl>
//! - Cloud3 / Cloud2Core: HyperHeadset `cloud_iii_wireless.rs` / `cloud_ii_core_wireless.rs`
//! - Cloud3S: HyperHeadset `cloud_iii_s_wireless.rs` (incl. auto-off, voice prompts, EQ)
//! - Alpha2: HeadsetControl `hyperx_cloud_alpha_2_wireless.hpp`
//! - Cloud3Wired: HeadsetControl `hyperx_cloud_3.hpp`
//! - AlphaS: HeadsetControl `hyperx_cloud_alpha_s.hpp`
//!
//! Voice prompts on the Alpha Wireless: HeadsetControl and HyperHeadset agree
//! on `13` (set) / `09` (get). Boom-mic presence (read-only) per HyperHeadset's
//! `get_mic_connected_packet`: Alpha `08`, Cloud2Hp `08`, Cloud2Core `8c` (event `07`).

use super::{exchange, get_input_report, open_node, pad, read_reports, Node, Spec, HP, KINGSTON};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, load_device_settings, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};
use tracing::{debug, info};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// `21 ff 05` battery voltage; `64` power and `65` mute events.
    Flight,
    /// `21 bb <cmd> <value>`, replies echo the command or a status code.
    Alpha,
    /// 62-byte `06 00 02 00 9a .. bb <cmd> <value>`; replies `0b 00 bb <cmd>`.
    Cloud2Kingston,
    /// `06 ff bb <cmd> <value>`; replies echo the header.
    Cloud2Hp,
    /// `66 <cmd> <value>`; replies echo the command or carry a notification id.
    Cloud3,
    /// Cloud III's `66` set plus a signed sidetone level (-5..=5).
    Cloud2Core,
    /// `0c 02 03 <1 = get> 00 <cmd> <value>`; `0d` notifications.
    Cloud3S,
    /// Base station control MCU: `<cmd> <sub> <payload>`, replies `cmd+1`.
    Alpha2,
    /// Wired: feature report `20 86 <on>` (sidetone only).
    Cloud3Wired,
    /// Wired control box: 62-byte `06 00 01 00 9b <value> 00 68 4a 8e <param>`,
    /// the reply fetched with HIDIOCGINPUT (report 6) after 100 ms.
    AlphaS,
}

use Family::*;

pub(super) const CLOUD_II: Spec<Family> = Spec {
    name: "HyperX Cloud II Wireless",
    vid: KINGSTON,
    pids: &[0x1718, 0x0b92],
    family: Cloud2Kingston,
};
/// HyperHeadset lists 16ea/16eb with the Cloud II protocol as Cloud Flight S.
pub(super) const CLOUD_FLIGHT_S: Spec<Family> = Spec {
    name: "HyperX Cloud Flight S",
    vid: KINGSTON,
    pids: &[0x16ea, 0x16eb],
    family: Cloud2Kingston,
};
pub(super) const CLOUD_II_HP: Spec<Family> = Spec {
    name: "HyperX Cloud II Wireless",
    vid: HP,
    pids: &[0x0696, 0x018b],
    family: Cloud2Hp,
};
/// hsctl's only PID; HyperHeadset added it as the Stinger 2 (commit 7e2c301).
pub(super) const CLOUD_STINGER_2: Spec<Family> = Spec {
    name: "HyperX Cloud Stinger 2 Wireless",
    vid: HP,
    pids: &[0x0d93],
    family: Cloud2Hp,
};
pub(super) const CLOUD_III: Spec<Family> = Spec {
    name: "HyperX Cloud III Wireless",
    vid: HP,
    pids: &[0x05b7, 0x0c9d],
    family: Cloud3,
};
pub(super) const CLOUD_II_CORE: Spec<Family> = Spec {
    name: "HyperX Cloud II Core Wireless",
    vid: HP,
    pids: &[0x069f, 0x0995],
    family: Cloud2Core,
};
pub(super) const CLOUD_III_S: Spec<Family> = Spec {
    name: "HyperX Cloud III S Wireless",
    vid: HP,
    pids: &[0x06be, 0x02cc],
    family: Cloud3S,
};
pub(super) const CLOUD_ALPHA: Spec<Family> = Spec {
    name: "HyperX Cloud Alpha Wireless",
    vid: HP,
    pids: &[0x098d, 0x1743, 0x1765],
    family: Alpha,
};
/// 0x08be is the base station's control MCU (0x0abe is its audio device).
pub(super) const CLOUD_ALPHA_2: Spec<Family> = Spec {
    name: "HyperX Cloud Alpha 2 Wireless",
    vid: HP,
    pids: &[0x08be],
    family: Alpha2,
};
pub(super) const CLOUD_FLIGHT: Spec<Family> = Spec {
    name: "HyperX Cloud Flight Wireless",
    vid: KINGSTON,
    pids: &[0x16c4, 0x1723],
    family: Flight,
};
pub(super) const CLOUD_FLIGHT_HP: Spec<Family> = Spec {
    name: "HyperX Cloud Flight Wireless",
    vid: HP,
    pids: &[0x0e90],
    family: Flight,
};
pub(super) const CLOUD_III_WIRED: Spec<Family> = Spec {
    name: "HyperX Cloud III",
    vid: HP,
    pids: &[0x089d, 0x03cc],
    family: Cloud3Wired,
};
pub(super) const CLOUD_ALPHA_S: Spec<Family> = Spec {
    name: "HyperX Cloud Alpha S",
    vid: KINGSTON,
    pids: &[0x16ed],
    family: AlphaS,
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[
    &CLOUD_II,
    &CLOUD_FLIGHT_S,
    &CLOUD_II_HP,
    &CLOUD_STINGER_2,
    &CLOUD_III,
    &CLOUD_II_CORE,
    &CLOUD_III_S,
    &CLOUD_ALPHA,
    &CLOUD_ALPHA_2,
    &CLOUD_FLIGHT,
    &CLOUD_FLIGHT_HP,
    &CLOUD_III_WIRED,
    &CLOUD_ALPHA_S,
];

fn node(f: Family) -> Node {
    match f {
        Flight | Alpha => Node::Report(0x21),
        Cloud2Kingston | Cloud2Hp => Node::Report(0x06),
        Cloud3 | Cloud2Core => Node::Report(0x66),
        Cloud3S => Node::Report(0x0c),
        // HeadsetControl: interface 2, usage page ff13 (interface 0 is the wrong MCU node).
        Alpha2 => Node::Iface(2),
        Cloud3Wired => Node::Report(0x20),
        // Interface 5, usage page ff13.
        AlphaS => Node::Iface(5),
    }
}

// ── Packet builders ─────────────────────────────────────────────────────────

const KINGSTON_HEADER: [u8; 15] = [
    0x06, 0x00, 0x02, 0x00, 0x9a, 0x00, 0x00, 0x68, 0x4a, 0x8e, 0x0a, 0x00, 0x00, 0x00, 0xbb,
];

/// Request `cmd` with one value byte in the family's framing.
fn packet(f: Family, cmd: u8, value: u8) -> Vec<u8> {
    match f {
        Flight => pad(&[0x21, 0xff, cmd], 20),
        Alpha => pad(&[0x21, 0xbb, cmd, value], 31),
        Cloud2Kingston => {
            let mut p = pad(&KINGSTON_HEADER, 62);
            p[15] = cmd;
            p[16] = value;
            p
        }
        Cloud2Hp => pad(&[0x06, 0xff, 0xbb, cmd, value], 52),
        Cloud3 => pad(&[0x66, cmd, value], 62),
        Cloud2Core => pad(&[0x66, cmd, value], 64),
        // Setters clear byte 3 (the "get" flag).
        Cloud3S => pad(&[0x0c, 0x02, 0x03, 0x00, 0x00, cmd, value], 64),
        Alpha2 => pad(&[cmd, value], 64),
        Cloud3Wired => pad(&[0x20, cmd, value], 62),
        AlphaS => {
            let mut p = pad(
                &[0x06, 0x00, 0x01, 0x00, 0x9b, value, 0x00, 0x68, 0x4a, 0x8e],
                62,
            );
            p[10] = cmd;
            p
        }
    }
}

fn cloud3s_get(cmd: u8) -> Vec<u8> {
    pad(&[0x0c, 0x02, 0x03, 0x01, 0x00, cmd], 64)
}

/// Cloud III S setter with a 16-bit big-endian value (`4a` auto-off seconds).
fn cloud3s_set16(cmd: u8, value: u16) -> Vec<u8> {
    let [hi, lo] = value.to_be_bytes();
    pad(&[0x0c, 0x02, 0x03, 0x00, 0x00, cmd, hi, lo], 64)
}

/// Cloud III S EQ band 0..=9 (32 Hz .. 16 kHz): `5f <band> <dB × 100, i16 BE>`.
fn cloud3s_eq(band: u8, db: f64) -> Vec<u8> {
    let [hi, lo] = ((db * 100.0).clamp(-1200.0, 1200.0) as i16).to_be_bytes();
    pad(&[0x0c, 0x02, 0x03, 0x00, 0x00, 0x5f, band, hi, lo], 64)
}

const EQ_BANDS: [(SettingKey, &str); 10] = [
    (SettingKey::EqBand1, "32 Hz"),
    (SettingKey::EqBand2, "64 Hz"),
    (SettingKey::EqBand3, "125 Hz"),
    (SettingKey::EqBand4, "250 Hz"),
    (SettingKey::EqBand5, "500 Hz"),
    (SettingKey::EqBand6, "1 kHz"),
    (SettingKey::EqBand7, "2 kHz"),
    (SettingKey::EqBand8, "4 kHz"),
    (SettingKey::EqBand9, "8 kHz"),
    (SettingKey::EqBand10, "16 kHz"),
];

/// Alpha S status request (parameter 09) and sidetone toggle (parameter 07).
const ALPHA_S_STATUS: u8 = 0x09;
const ALPHA_S_SIDETONE: u8 = 0x07;

/// Status queries sent on every poll, connection status first.
fn queries(f: Family) -> Vec<Vec<u8>> {
    let cmds: &[u8] = match f {
        Flight => &[0x05],
        Alpha => &[0x03, 0x0b, 0x0c, 0x0a, 0x05, 0x06, 0x07, 0x09, 0x08],
        // 0x01 answers connection status (reply id 1) and mute (reply id 8).
        Cloud2Kingston => &[0x01, 0x02, 0x03, 0x1a],
        // No 0x01 here: HyperHeadset notes it resets state (unmutes the mic).
        Cloud2Hp => &[0x02, 0x03, 0x05, 0x06, 0x0b, 0x07, 0x08],
        Cloud3 => &[0x82, 0x89, 0x8a, 0x86, 0x84, 0x85],
        Cloud2Core => &[0x82, 0x89, 0x8a, 0x86, 0x84, 0x88, 0x85, 0x8c],
        Cloud3S => {
            return [0x02, 0x06, 0x48, 0x04, 0x16, 0x14, 0x4b]
                .map(cloud3s_get)
                .to_vec()
        }
        Alpha2 => {
            return vec![
                packet(f, 0x52, 0x01),
                packet(f, 0x50, 0x02),
                packet(f, 0x60, 0x02),
            ]
        }
        Cloud3Wired => &[],
        AlphaS => &[ALPHA_S_STATUS],
    };
    cmds.iter().map(|&c| packet(f, c, 0)).collect()
}

/// Packets for a setting write (Alpha 2 goes through [`alpha2_apply`]).
fn set_packets(f: Family, key: SettingKey, value: &SettingValue) -> Result<Vec<Vec<u8>>> {
    let n = value.as_int().unwrap_or(0).clamp(0, 255) as u8;
    let on = value.as_bool().map(u8::from);
    let p = |cmd, v| packet(f, cmd, v);
    Ok(match (f, key) {
        (Alpha, SettingKey::MicMute) => vec![p(0x15, on.unwrap_or(0))],
        (Alpha, SettingKey::MicSideTone) if n == 0 => vec![p(0x10, 0)],
        (Alpha, SettingKey::MicSideTone) => vec![p(0x10, 1), p(0x11, n)],
        (Alpha, SettingKey::PmShutdown) => vec![p(0x12, n)],
        (Alpha, SettingKey::VoicePrompts) => vec![p(0x13, on.unwrap_or(0))],
        (Cloud2Kingston, SettingKey::MicSideTone) => vec![p(0x19, n)],
        (Cloud2Kingston, SettingKey::PmShutdown) => vec![p(0x18, n)],
        (Cloud2Hp, SettingKey::MicMute) => vec![p(0x20, on.unwrap_or(0))],
        (Cloud2Hp, SettingKey::MicSideTone) => vec![p(0x21, n)],
        (Cloud2Hp, SettingKey::MicSideToneVolume) => vec![p(0x23, n)],
        (Cloud2Hp, SettingKey::PmShutdown) => vec![p(0x22, n)],
        (Cloud3 | Cloud2Core, SettingKey::MicMute) => vec![p(0x03, on.unwrap_or(0))],
        (Cloud3 | Cloud2Core, SettingKey::PmShutdown) => vec![p(0x02, n)],
        (Cloud3, SettingKey::MicSideTone) => vec![p(0x01, n)],
        // Code 0 = off, 1..=11 = level -5..=+5 (signed byte on the wire).
        (Cloud2Core, SettingKey::MicSideTone) if n == 0 => vec![p(0x01, 0)],
        (Cloud2Core, SettingKey::MicSideTone) => {
            vec![
                p(0x01, 1),
                p(0x05, (i8::try_from(n).unwrap_or(6) - 6) as u8),
            ]
        }
        (Cloud3S, SettingKey::MicMute) => vec![p(0x01, on.unwrap_or(0))],
        (Cloud3S, SettingKey::MicSideTone) => vec![p(0x0d, n)],
        (Cloud3S, SettingKey::VoicePrompts) => vec![p(0x0b, on.unwrap_or(0))],
        (Cloud3S, SettingKey::PmShutdown) => vec![cloud3s_set16(0x4a, u16::from(n) * 60)],
        (Cloud3S, k) if EQ_BANDS.iter().any(|(b, _)| *b == k) => {
            let band = EQ_BANDS.iter().position(|(b, _)| *b == k).unwrap_or(0);
            vec![cloud3s_eq(band as u8, value.as_float().unwrap_or(0.0))]
        }
        (Cloud3Wired, SettingKey::MicSideTone) => vec![p(0x86, n)],
        // A toggle: `set` only sends it when the status shows the other state.
        (AlphaS, SettingKey::MicSideTone) => vec![p(ALPHA_S_SIDETONE, 0)],
        _ => bail!("unsupported setting {}", key.as_str()),
    })
}

// ── Reply parsing ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum Event {
    Online(bool),
    Battery(u8),
    Charging(bool),
    Muted(bool),
    SideToneOn(bool),
    SideToneLevel(i64),
    SideToneVolume(u8),
    MicAttached(bool),
    AutoOff(u8),
    EqPreset(u8),
    ChatMix(f64, f64),
    VoicePrompts(bool),
}

/// Cloud Flight battery estimate from millivolts (HyperHeadset's threshold
/// table: 5 % steps from 3328 mV to 3970 mV).
const FLIGHT_MV: [u16; 20] = [
    3328, 3584, 3674, 3704, 3732, 3744, 3754, 3764, 3774, 3784, 3794, 3804, 3824, 3840, 3860, 3890,
    3910, 3940, 3960, 3970,
];
/// Above this the Flight reports its charger voltage (HeadsetControl).
const FLIGHT_CHARGING_MV: u16 = 0x100b;

fn flight_percent(mv: u16) -> u8 {
    let i = FLIGHT_MV.partition_point(|&t| t <= mv).saturating_sub(1);
    (i as u8 + 1) * 5
}

fn parse(f: Family, r: &[u8]) -> Vec<Event> {
    use Event::*;
    match f {
        Flight => match r {
            [0x21, 0xff, 0x05, hi, lo, ..] => {
                let mv = u16::from_be_bytes([*hi, *lo]);
                if mv > FLIGHT_CHARGING_MV {
                    vec![Online(true), Charging(true)]
                } else {
                    vec![Online(true), Battery(flight_percent(mv)), Charging(false)]
                }
            }
            [0x64, 0x01 | 0x03, ..] => vec![Online(true)],
            [0x65, m, ..] => vec![Muted(*m == 0x04)],
            _ => vec![],
        },
        Alpha => match r {
            [0x21, 0xbb, code, v, ..] => match code {
                // HeadsetControl: 01 = headset not connected.
                0x03 | 0x24 => vec![Online(*v != 0x01)],
                0x0b | 0x25 => vec![Battery(*v)],
                0x0c | 0x26 => vec![Charging(*v == 0x01)],
                0x0a | 0x23 | 0x15 => vec![Muted(*v == 0x01)],
                0x05 | 0x22 | 0x10 => vec![SideToneOn(*v == 0x01)],
                0x06 | 0x11 => vec![SideToneLevel(i64::from(*v))],
                0x07 | 0x12 => vec![AutoOff(*v)],
                0x09 | 0x13 => vec![VoicePrompts(*v == 0x01)],
                0x08 => vec![MicAttached(*v == 0x01)],
                _ => vec![],
            },
            _ => vec![],
        },
        Cloud2Kingston => match r {
            [0x0b, _, 0xbb, cmd, d4, _, _, d7, ..] => match cmd {
                0x01 => vec![Online(matches!(d4, 0x01 | 0x04))],
                0x02 => vec![Battery(*d7)],
                0x03 => vec![Charging(*d4 == 0x01)],
                0x08 => vec![Muted(*d4 == 0x01)],
                0x19 => vec![SideToneOn(*d4 == 0x01)],
                0x1a => vec![AutoOff(*d4)],
                _ => vec![],
            },
            _ => vec![],
        },
        Cloud2Hp => match r {
            [0x06, 0xff, 0xbb, cmd, d4, _, _, d7, ..] => match cmd {
                0x02 => vec![Battery(*d7)],
                0x03 => vec![Charging(*d4 == 0x01)],
                0x05 | 0x20 => vec![Muted(*d4 == 0x01)],
                0x06 | 0x21 => vec![SideToneOn(*d4 == 0x01)],
                0x07 | 0x22 => vec![AutoOff(*d4)],
                0x08 => vec![MicAttached(*d4 == 0x01)],
                0x0b => vec![SideToneVolume(*d4)],
                _ => vec![],
            },
            _ => vec![],
        },
        Cloud3 | Cloud2Core => {
            let core = f == Cloud2Core;
            match r {
                [0x66, id, a, b, c, ..] => match id {
                    0x82 | 0x0b => vec![Online(*a == 0x01)],
                    // Both state bytes zero = no reading yet.
                    0x89 | 0x0d if *a != 0 || *b != 0 => vec![Battery(*c)],
                    0x8a | 0x0c => vec![Charging(*a == 0x01)],
                    0x86 | 0x0a => vec![Muted(*a == 0x01)],
                    0x03 if core => vec![Muted(*a == 0x01)],
                    0x84 => vec![SideToneOn(*a == 0x01)],
                    0x09 | 0x01 if core => vec![SideToneOn(*a == 0x01)],
                    0x85 => vec![AutoOff(*a)],
                    0x02 if core => vec![AutoOff(*a)],
                    0x8c | 0x07 if core => vec![MicAttached(*a == 0x01)],
                    0x88 | 0x05 if core => {
                        let level = *a as i8;
                        vec![SideToneLevel(if (-5..=5).contains(&level) {
                            i64::from(level)
                        } else {
                            0
                        })]
                    }
                    _ => vec![],
                },
                _ => vec![],
            }
        }
        Cloud3S => match r {
            [0x0c, _, _, _, _, _, 0xff, ..] => vec![],
            [0x0c, _, _, _, _, cmd, v, ..] => match cmd {
                0x02 => vec![Online(*v == 0x02)],
                0x04 => vec![Muted(*v == 0x01)],
                0x06 => vec![Battery(*v)],
                0x14 => vec![VoicePrompts(*v == 0x01)],
                0x16 => vec![SideToneOn(*v == 0x01)],
                0x48 => vec![Charging(*v == 0x01)],
                // Seconds, u16 BE.
                0x4b => match r.get(7) {
                    Some(lo) => {
                        let minutes = u16::from_be_bytes([*v, *lo]) / 60;
                        vec![AutoOff(u8::try_from(minutes).unwrap_or(u8::MAX))]
                    }
                    None => vec![],
                },
                _ => vec![],
            },
            [0x0d, _, _, _, kind, v, ..] => match kind {
                0x01 => vec![Battery(*v)],
                0x03 => vec![Muted(*v == 0x01)],
                0x05 => vec![SideToneOn(*v == 0x01)],
                0x0a => vec![Charging(*v == 0x01)],
                0x0c => vec![Online(*v == 0x01)],
                _ => vec![],
            },
            [0x05, flags, ..] => vec![Muted(flags & 0x02 != 0)],
            _ => vec![],
        },
        Alpha2 => match r {
            [0x53, 0x01, c, ..] => vec![Online(*c != 0)],
            [0x51, 0x02, level, chg, ..] if *level <= 100 => {
                vec![Online(true), Battery(*level), Charging(*chg == 0x01)]
            }
            [0xfb, 0x0a, s, ..] => vec![Online(*s == 0x01)],
            _ => alpha2_block(r).map_or_else(Vec::new, |b| alpha2_events(&b)),
        },
        Cloud3Wired => vec![],
        AlphaS => alpha_s_status(r),
    }
}

/// Alpha S status reply (the request header echoed, bit 7 of \[5\] set):
/// \[14\] bit 0 sidetone, bit 1 mic muted; \[16\] / \[20\] game / chat level in
/// dB (Q8.8 BE, 0 = full).
fn alpha_s_status(r: &[u8]) -> Vec<Event> {
    if r.len() < 22 || r[0] != 0x06 || r[4] != 0x9b || r[5] & 0x80 == 0 {
        return vec![];
    }
    let db = |at: usize| f32::from(i16::from_be_bytes([r[at], r[at + 1]])) / 256.0;
    // Each side steps from 0 (full) to -8 (silent), with NGENUITY's thresholds.
    let level = |db: f32| {
        let step = [-40.0, -30.0, -20.0, -14.0, -10.0, -6.0, -3.0, -1.0]
            .iter()
            .position(|&limit| db < limit)
            .map_or(0, |i| i as i32 - 8);
        1.0 + f64::from(step) / 8.0
    };
    vec![
        Event::SideToneOn(r[14] & 0x01 != 0),
        Event::Muted(r[14] & 0x02 != 0),
        Event::ChatMix(level(db(16)), level(db(20))),
    ]
}

/// The 12-byte audio settings block of an Alpha 2 `61 02` reply (offset 4):
/// \[4\] sidetone on, \[5\] sidetone 0..=20, \[8\] chat mix 0x7b..=0x85, \[11\] EQ preset 1..=3.
fn alpha2_block(r: &[u8]) -> Option<[u8; 12]> {
    let [0x61, 0x02, _, _, rest @ ..] = r else {
        return None;
    };
    let b: [u8; 12] = rest.get(..12)?.try_into().ok()?;
    (b[4] <= 1 && b[5] <= 20 && (1..=3).contains(&b[11])).then_some(b)
}

fn alpha2_events(b: &[u8; 12]) -> Vec<Event> {
    // -5 (all game) .. 0 (balanced) .. +5 (all chat).
    let step = (i32::from(b[8]) - 0x80).clamp(-5, 5);
    let game = if step > 0 {
        1.0 - step as f64 / 5.0
    } else {
        1.0
    };
    let chat = if step < 0 {
        1.0 + step as f64 / 5.0
    } else {
        1.0
    };
    vec![
        Event::SideToneOn(b[4] != 0),
        Event::SideToneLevel(i64::from(b[5])),
        Event::EqPreset(b[11]),
        Event::ChatMix(game, chat),
    ]
}

/// Change one field of the block; the base station only takes the whole block.
fn alpha2_apply(b: &mut [u8; 12], key: SettingKey, value: &SettingValue) -> Result<()> {
    let n = value.as_int().unwrap_or(0).clamp(0, 255) as u8;
    match key {
        SettingKey::MicSideTone if n == 0 => b[4] = 0,
        SettingKey::MicSideTone => (b[4], b[5]) = (1, n),
        SettingKey::EqPreset => b[11] = n,
        _ => bail!("unsupported setting {}", key.as_str()),
    }
    Ok(())
}

fn alpha2_write(b: &[u8; 12]) -> Vec<u8> {
    let mut p = pad(&[0x60, 0x01], 64);
    p[2..14].copy_from_slice(b);
    p
}

/// The MCU acks every command with `ff 01 .. <cmd at 14> <sub at 15>`, `ff 02` rejects.
fn alpha2_ack(r: &[u8], cmd: u8, sub: u8) -> Option<bool> {
    match r {
        [0xff, status @ (0x01 | 0x02), ..] if r.get(14..16) == Some(&[cmd, sub]) => {
            Some(*status == 0x01)
        }
        _ => None,
    }
}

/// The `mic_side_tone` value the family's control shows, from the on flag and level.
fn sidetone_value(f: Family, on: Option<bool>, level: Option<i64>) -> Option<i64> {
    match f {
        Alpha | Alpha2 => match on? {
            false => Some(0),
            true => level,
        },
        Cloud2Core => match on? {
            false => Some(0),
            true => level.map(|l| l + 6),
        },
        _ => on.map(i64::from),
    }
}

// ── Controls ────────────────────────────────────────────────────────────────

fn has_battery(f: Family) -> bool {
    !matches!(f, Cloud3Wired | AlphaS)
}

fn minutes(values: &[i64]) -> Vec<SettingOption> {
    values
        .iter()
        .map(|&m| match m {
            0 => SettingOption::int(0, "Never"),
            m => SettingOption::int(m, &format!("After {m} min")),
        })
        .collect()
}

fn on_off() -> Vec<SettingOption> {
    vec![SettingOption::int(0, "Off"), SettingOption::int(1, "On")]
}

fn controls(f: Family) -> Vec<SettingMeta> {
    use SettingKey::*;
    let mut c = Vec::new();
    match f {
        AlphaS => {
            c.push(SettingMeta::readonly(ChatMixGame, "Game", 0.0, 1.0));
            c.push(SettingMeta::readonly(ChatMixChat, "Chat", 0.0, 1.0));
        }
        Cloud3S => {
            for (key, label) in EQ_BANDS {
                c.push(SettingMeta::slider(key, label, -12.0, 12.0, 0.5).unit("dB"));
            }
        }
        Alpha2 => {
            c.push(SettingMeta::readonly(ChatMixGame, "Game", 0.0, 1.0));
            c.push(SettingMeta::readonly(ChatMixChat, "Chat", 0.0, 1.0));
            c.push(SettingMeta::dropdown(
                EqPreset,
                "EQ preset",
                (1..=3)
                    .map(|p| SettingOption::int(p, &format!("Preset {p}")))
                    .collect(),
            ));
        }
        // Range per HeadsetControl; 0 turns sidetone off.
        Alpha => c.push(SettingMeta::slider(
            MicSideTone,
            "Sidetone",
            0.0,
            128.0,
            1.0,
        )),
        _ => {}
    }
    if matches!(f, Alpha | Cloud3S) {
        c.push(SettingMeta::toggle(VoicePrompts, "Voice prompts"));
    }
    match f {
        // HyperHeadset: off, 10, 20 or 30 minutes.
        Cloud3S => c.push(SettingMeta::dropdown(
            PmShutdown,
            "Auto-off",
            minutes(&[0, 10, 20, 30]),
        )),
        Alpha | Cloud2Hp | Cloud3 | Cloud2Core => c.push(SettingMeta::dropdown(
            PmShutdown,
            "Auto-off",
            minutes(&[0, 5, 10, 15, 20, 30, 40, 60]),
        )),
        // HeadsetControl caps the Kingston Cloud II at 30 minutes.
        Cloud2Kingston => c.push(SettingMeta::dropdown(
            PmShutdown,
            "Auto-off",
            minutes(&[0, 5, 10, 15, 20, 30]),
        )),
        _ => {}
    }
    match f {
        Alpha | Cloud2Hp | Cloud3 | Cloud2Core | Cloud3S => {
            c.push(SettingMeta::toggle(MicMute, "Mute"))
        }
        // State is reported, but no setter is documented.
        Flight | Cloud2Kingston | AlphaS => {
            c.push(SettingMeta::readonly(MicMute, "Muted", 0.0, 1.0))
        }
        _ => {}
    }
    if matches!(f, Alpha | Cloud2Hp | Cloud2Core) {
        c.push(SettingMeta::readonly(
            MicAttached,
            "Boom mic attached",
            0.0,
            1.0,
        ));
    }
    match f {
        Alpha2 => c.push(SettingMeta::slider(MicSideTone, "Sidetone", 0.0, 20.0, 1.0)),
        Cloud2Core => {
            let mut opts = vec![SettingOption::int(0, "Off")];
            opts.extend((-5..=5).map(|l: i64| SettingOption::int(l + 6, &format!("{l:+}"))));
            c.push(SettingMeta::dropdown(MicSideTone, "Sidetone", opts));
        }
        Cloud2Kingston | Cloud2Hp | Cloud3 | Cloud3S | Cloud3Wired | AlphaS => {
            c.push(SettingMeta::dropdown(MicSideTone, "Sidetone", on_off()))
        }
        Alpha | Flight => {}
    }
    // A byte per hsctl; HyperHeadset offers 0..=250.
    if f == Cloud2Hp {
        c.push(SettingMeta::slider(
            MicSideToneVolume,
            "Sidetone volume",
            0.0,
            255.0,
            1.0,
        ));
    }
    c
}

// ── Adapter ─────────────────────────────────────────────────────────────────

/// The model's settings file. The Stinger 2 used to be listed under the HP
/// Cloud II Wireless, so its old settings sit in that model's file.
fn load_settings(spec: &Spec<Family>, paths: &ConfigPaths) -> Result<SettingsMap> {
    let settings = load_device_settings(paths, spec.vid, spec.pids[0])?;
    if settings.is_empty() && spec.pids == CLOUD_STINGER_2.pids {
        return load_device_settings(paths, HP, CLOUD_II_HP.pids[0])
            .context("the Stinger 2's old settings (HP Cloud II Wireless file)");
    }
    Ok(settings)
}

const POLL: Duration = Duration::from_secs(30);
const REPLY_WAIT: Duration = Duration::from_millis(300);
/// HyperHeadset waits 50 ms between packets; bursts upset the Cloud III S dongle.
const GAP: Duration = Duration::from_millis(50);

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, ctx: &DriverCtx) -> Opened {
    Ok(vec![Box::new(HeadsetAdapter::open(
        spec,
        pid,
        ctx.paths.clone(),
    )?)])
}

struct HeadsetAdapter {
    spec: &'static Spec<Family>,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    /// Second handle for the HIDIOCGINPUT read the Kingston Cloud II wants
    /// before every write (both sources do it and ignore failures).
    input: Option<File>,
    settings: SettingsMap,
    battery: BatteryState,
    sidetone: (Option<bool>, Option<i64>),
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl HeadsetAdapter {
    fn open(spec: &'static Spec<Family>, pid: u16, paths: ConfigPaths) -> Result<Self> {
        let f = spec.family;
        let (dev, instance) = open_node(spec.vid, pid, node(f))?;
        let input = matches!(f, Cloud2Kingston | AlphaS)
            .then(|| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&dev.path)
                    .ok()
            })
            .flatten();
        info!("{} open via {}", spec.name, dev.path.display());
        Ok(Self {
            spec,
            id: DeviceId {
                vendor_id: spec.vid,
                product_id: pid,
                instance,
            },
            dev: Some(dev),
            input,
            settings: load_settings(spec, &paths)?,
            battery: BatteryState {
                label: "Headset".into(),
                percent: None,
                charging: false,
                present: false,
            },
            sidetone: (None, None),
            last_poll: None,
            failures: 0,
            last_error: None,
            paths,
        })
    }

    fn family(&self) -> Family {
        self.spec.family
    }

    fn prepare(&self) {
        if let Some(file) = &self.input {
            let mut buf = [0u8; 64];
            buf[0] = 0x06;
            let _ = get_input_report(file, &mut buf);
        }
    }

    /// Send one packet and apply whatever answers within `wait`.
    fn send(&mut self, packet: &[u8], wait: Duration) -> Result<Vec<Vec<u8>>> {
        let f = self.family();
        self.prepare();
        let dev = self.dev.as_mut().context("not connected")?;
        let reports = if f == Cloud3Wired {
            dev.set_feature(&mut packet.to_vec())?;
            vec![]
        } else if f == AlphaS {
            dev.write(packet)?;
            std::thread::sleep(Duration::from_millis(100));
            let mut reply = [0u8; 62];
            reply[0] = 0x06;
            let file = self.input.as_ref().context("no input-report handle")?;
            get_input_report(file, &mut reply)?;
            // Only a status reply carries the status layout.
            if packet[10] == ALPHA_S_STATUS {
                vec![reply.to_vec()]
            } else {
                vec![]
            }
        } else {
            exchange(dev, packet, wait, |r| !parse(f, r).is_empty())?
        };
        for r in &reports {
            self.handle(r);
        }
        Ok(reports)
    }

    fn handle(&mut self, report: &[u8]) {
        for e in parse(self.family(), report) {
            self.apply(e);
        }
    }

    fn apply(&mut self, e: Event) {
        let b = &mut self.battery;
        match e {
            Event::Online(on) => {
                b.present = on;
                if !on {
                    b.percent = None;
                    b.charging = false;
                }
            }
            Event::Battery(p) => (b.present, b.percent) = (true, Some(p.min(100))),
            Event::Charging(c) => b.charging = c,
            Event::Muted(m) => merge_setting(
                &mut self.settings,
                SettingKey::MicMute,
                SettingValue::Bool(m),
            ),
            Event::SideToneOn(on) => self.sidetone.0 = Some(on),
            Event::SideToneLevel(l) => self.sidetone.1 = Some(l),
            Event::SideToneVolume(v) => merge_setting(
                &mut self.settings,
                SettingKey::MicSideToneVolume,
                SettingValue::Int(i64::from(v)),
            ),
            Event::MicAttached(on) => merge_setting(
                &mut self.settings,
                SettingKey::MicAttached,
                SettingValue::Bool(on),
            ),
            Event::AutoOff(m) => merge_setting(
                &mut self.settings,
                SettingKey::PmShutdown,
                SettingValue::Int(i64::from(m)),
            ),
            Event::EqPreset(p) => merge_setting(
                &mut self.settings,
                SettingKey::EqPreset,
                SettingValue::Int(i64::from(p)),
            ),
            Event::VoicePrompts(on) => merge_setting(
                &mut self.settings,
                SettingKey::VoicePrompts,
                SettingValue::Bool(on),
            ),
            Event::ChatMix(game, chat) => {
                merge_setting(
                    &mut self.settings,
                    SettingKey::ChatMixGame,
                    SettingValue::Float(game),
                );
                merge_setting(
                    &mut self.settings,
                    SettingKey::ChatMixChat,
                    SettingValue::Float(chat),
                );
            }
        }
        if let Some(v) = sidetone_value(self.family(), self.sidetone.0, self.sidetone.1) {
            merge_setting(
                &mut self.settings,
                SettingKey::MicSideTone,
                SettingValue::Int(v),
            );
        }
    }

    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        let mut any = false;
        for q in queries(self.family()) {
            let answered = !self.send(&q, REPLY_WAIT)?.is_empty();
            any |= answered;
            // No point asking an absent headset for the rest.
            if answered && !self.battery.present {
                break;
            }
            std::thread::sleep(GAP);
        }
        if !any && has_battery(self.family()) {
            // Silent dongle: the headset is off or out of range.
            self.apply(Event::Online(false));
        }
        debug!("{} battery {:?}", self.spec.name, self.battery);
        Ok(())
    }

    fn alpha2_set(&mut self, key: SettingKey, value: &SettingValue) -> Result<()> {
        // NGENUITY polls the same node, so replies are picked out of other traffic.
        let dev = self.dev.as_mut().context("not connected")?;
        let reports = exchange(
            dev,
            &packet(Alpha2, 0x60, 0x02),
            Duration::from_secs(1),
            |r| alpha2_block(r).is_some(),
        )?;
        let mut block = reports
            .iter()
            .find_map(|r| alpha2_block(r))
            .context("no audio settings from the base station")?;
        alpha2_apply(&mut block, key, value)?;
        let acks = exchange(dev, &alpha2_write(&block), Duration::from_secs(1), |r| {
            alpha2_ack(r, 0x60, 0x01).is_some()
        })?;
        for r in reports.iter().chain(&acks) {
            self.handle(r);
        }
        match acks.iter().find_map(|r| alpha2_ack(r, 0x60, 0x01)) {
            Some(true) => Ok(()),
            Some(false) => bail!("base station rejected the setting"),
            None => bail!("no acknowledgement from the base station"),
        }
    }
}

impl DeviceAdapter for HeadsetAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: if has_battery(self.family()) {
                vec![self.battery.clone()]
            } else {
                vec![]
            },
            settings: self.settings.clone(),
            // Boom-mic presence only once the headset reports it (the
            // Stinger 2's mic is fixed and may never answer).
            controls: controls(self.family())
                .into_iter()
                .filter(|c| {
                    c.key != SettingKey::MicAttached
                        || get_setting(&self.settings, SettingKey::MicAttached).is_some()
                })
                .collect(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let f = self.family();
        if has_battery(f) {
            if self.last_poll.is_none() {
                // Not asked yet (just opened): find out whether it is on.
                self.poll()?;
            }
            if !self.battery.present {
                bail!("{} is off or out of range", self.spec.name);
            }
        }
        if f == Alpha2 {
            self.alpha2_set(key, &value)?;
        } else if f == AlphaS && key == SettingKey::MicSideTone {
            self.send(&packet(AlphaS, ALPHA_S_STATUS, 0), REPLY_WAIT)?;
            let want = value.as_int().unwrap_or(0) != 0;
            match self.sidetone.0 {
                Some(on) if on == want => {}
                Some(_) => {
                    for p in set_packets(f, key, &value)? {
                        self.send(&p, REPLY_WAIT)?;
                    }
                }
                None => bail!("no status reply from the control box"),
            }
        } else {
            let old = get_setting(&self.settings, key).cloned();
            for (i, p) in set_packets(f, key, &value)?.iter().enumerate() {
                if let Err(e) = self.send(p, Duration::from_millis(100)) {
                    // A packet before this one landed: put the old value back.
                    if let (true, Some(old)) = (i > 0, &old) {
                        for p in set_packets(f, key, old).unwrap_or_default() {
                            let _ = self.send(&p, Duration::from_millis(100));
                            std::thread::sleep(GAP);
                        }
                    }
                    return Err(e);
                }
                std::thread::sleep(GAP);
            }
        }
        if key == SettingKey::MicSideTone {
            // Stale on/level reports must not override the new value.
            self.sidetone = (None, None);
        }
        merge_setting(&mut self.settings, key, value);
        hako_core::config::save_device_settings(
            &self.paths,
            self.spec.vid,
            self.spec.pids[0],
            &self.settings,
        )
        .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn tick(&mut self) -> Result<()> {
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        // Mute button, link and battery notifications arrive unsolicited.
        for r in read_reports(dev, Duration::ZERO, |_| false) {
            self.handle(&r);
        }
        if self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return Ok(());
        }
        match self.poll() {
            Ok(()) => {
                self.failures = 0;
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(e.to_string());
                if self.failures >= 3 {
                    self.dev = None;
                }
                Err(e)
            }
        }
    }

    fn shutdown(&mut self) {
        self.dev = None;
        self.input = None;
    }
}

#[cfg(test)]
mod tests;
