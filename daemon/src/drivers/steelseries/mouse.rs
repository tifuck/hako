//! Rival / Aerox / Prime / Sensei mice.
//!
//! Byte layouts from rivalcfg device profiles (`rivalcfg/devices/*.py`) and
//! value handlers (`rivalcfg/handlers/*.py`), reimplemented. Every command is
//! an output report: report id `0x00` followed by the profile's bytes,
//! zero-padded only where the profile sets `packet_length` (Rival 106 / 110 /
//! 300S: 32 bytes). In 2.4 GHz mode (Aerox / Prime Wireless dongles) the opcode gets
//! bit 6 set and the dongle answers each command with a 64-byte report.
//! The Rival 310 / 500 / 600 / 700 / 710 and Sensei 310 / TEN colors are
//! feature reports (rivalcfg `rgbgradient` / `rgbgradientv2` handlers, one
//! static color per LED, laid out as in rivalcfg's handler tests).
//!
//! DPI stages: mice whose sensitivity command takes a preset list
//! (rivalcfg `multidpi_range`, `multidpi_range_choice[_xy]`, up to 5
//! presets) get the five stage sliders; once one is set the whole list is
//! sent and the DPI slider edits the active stage. Sleep timer: rivalcfg
//! `sleep_timer` (Aerox / Prime Wireless `29`, Rival 650 `2b 01 01 00 00 00`).
//!
//! Values are volatile and re-sent on (re)connect. Every mouse also has a
//! rivalcfg `save_command` (`09`, `11 00`, `59`, ...) that writes the current
//! settings to flash. It is only sent from `set`, after the setter, while the
//! owner-enabled "Save changes to device memory" toggle is on, and only for
//! models where a second source sends the same bytes ([`MouseSave`]):
//! libratbag `src/driver-steelseries.c` `steelseries_write_save` (`09` for
//! protocol 1 / 3 / 4, `59` for protocol 2, PIDs in
//! `data/devices/steelseries-*.device`) or OpenRGB
//! `SteelSeriesMouseController::Save` (`09`, Rival 3). The rest are shown
//! blocked with the reason (`docs/onboard-writes/steelseries.md`).

use super::{instance, int, request, VID};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const BATTERY_POLL: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_millis(200);
/// rivalcfg's default delay between two commands.
const COMMAND_GAP: Duration = Duration::from_millis(50);

/// TrueMove Core codes for 200..=8500 DPI in 100 steps (rivalcfg `dpi/truemove_core.py`).
const TRUEMOVE_CORE: [u8; 84] = [
    0x04, 0x06, 0x08, 0x0b, 0x0d, 0x0f, 0x12, 0x14, 0x16, 0x19, 0x1b, 0x1d, 0x20, 0x22, 0x24, 0x27,
    0x29, 0x2b, 0x2e, 0x30, 0x32, 0x34, 0x37, 0x39, 0x3b, 0x3e, 0x40, 0x42, 0x45, 0x47, 0x49, 0x4c,
    0x4e, 0x50, 0x53, 0x55, 0x57, 0x5a, 0x5c, 0x5e, 0x61, 0x63, 0x65, 0x68, 0x6a, 0x6c, 0x6f, 0x71,
    0x73, 0x76, 0x78, 0x7a, 0x7d, 0x7f, 0x81, 0x84, 0x86, 0x88, 0x8b, 0x8d, 0x8f, 0x92, 0x94, 0x96,
    0x99, 0x9b, 0x9d, 0xa0, 0xa2, 0xa4, 0xa7, 0xa9, 0xab, 0xad, 0xb0, 0xb2, 0xb4, 0xb7, 0xb9, 0xbc,
    0xbe, 0xc0, 0xc3, 0xc5,
];

/// TrueMove Air codes for 100..=18000 DPI in 100 steps (rivalcfg `dpi/truemove_air.py`).
const TRUEMOVE_AIR: [u8; 180] = [
    0x00, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x10, 0x11, 0x12,
    0x13, 0x14, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x23, 0x25, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x32, 0x33, 0x34, 0x35, 0x36, 0x38, 0x39,
    0x3a, 0x3b, 0x3c, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x44, 0x45, 0x46, 0x47, 0x48, 0x4a, 0x4b, 0x4c,
    0x4d, 0x4e, 0x50, 0x51, 0x52, 0x53, 0x54, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5c, 0x5d, 0x5e, 0x5f,
    0x60, 0x62, 0x63, 0x64, 0x65, 0x66, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6e, 0x6f, 0x70, 0x71, 0x72,
    0x74, 0x75, 0x76, 0x77, 0x78, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x80, 0x81, 0x82, 0x83, 0x84, 0x86,
    0x87, 0x88, 0x89, 0x8a, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x92, 0x93, 0x94, 0x95, 0x96, 0x98, 0x99,
    0x9a, 0x9b, 0x9c, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xaa, 0xab, 0xac,
    0xad, 0xae, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd,
    0xbf, 0xc0, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xcf, 0xd0, 0xd1,
    0xd2, 0xd3, 0xd5, 0xd6,
];

#[derive(Debug, Clone, Copy)]
pub enum Sensor {
    TrueMoveCore,
    TrueMoveAir,
    /// PixArt PMW3326 (rivalcfg `dpi/pmw3326.py`): the TrueMove Core codes
    /// for 200..=7200 DPI.
    Pmw3326,
}

impl Sensor {
    /// (codes, lowest DPI); codes step by 100 DPI.
    fn table(self) -> (&'static [u8], u32) {
        match self {
            Self::TrueMoveCore => (&TRUEMOVE_CORE, 200),
            Self::TrueMoveAir => (&TRUEMOVE_AIR, 100),
            Self::Pmw3326 => (&TRUEMOVE_CORE[..71], 200),
        }
    }

    fn code(self, dpi: u32) -> u8 {
        let (codes, min) = self.table();
        let max = min + 100 * (codes.len() as u32 - 1);
        codes[((dpi.clamp(min, max) - min + 50) / 100) as usize]
    }
}

/// How a per-preset DPI command encodes the sensitivity.
#[derive(Debug, Clone, Copy)]
pub enum Codes {
    /// rivalcfg `range`: `min..=max` in `step`s to `first`, `first + 1`, ...
    Linear {
        min: u32,
        max: u32,
        step: u32,
        first: u8,
    },
    /// rivalcfg `range_choice` over a sensor table (100 DPI steps).
    Sensor(Sensor),
    /// rivalcfg `choice`: (DPI, code).
    Choice(&'static [(u32, u8)]),
}

impl Codes {
    fn code(self, dpi: u32) -> u8 {
        match self {
            Self::Linear {
                min,
                max,
                step,
                first,
            } => first + ((dpi.clamp(min, max) - min + step / 2) / step) as u8,
            Self::Sensor(sensor) => sensor.code(dpi),
            Self::Choice(choices) => choices
                .iter()
                .min_by_key(|(d, _)| d.abs_diff(dpi))
                .map_or(0, |&(_, code)| code),
        }
    }
}

/// How a mouse takes its sensitivity. We always write one preset and select it.
#[derive(Debug, Clone, Copy)]
pub enum Dpi {
    /// rivalcfg `multidpi_range_choice`: `cmd, count, selected, code`
    /// (code little-endian over `len` bytes).
    Table {
        cmd: &'static [u8],
        sensor: Sensor,
        first: u8,
        len: u8,
    },
    /// rivalcfg `multidpi_range` 50..=18000 to 1..=0x168:
    /// `cmd, count, selected, (code_lo, code_hi)*`; `flags`: the count is a
    /// bit mask (`count_mode: flag`).
    Step50 {
        cmd: &'static [u8],
        first: u8,
        flags: bool,
    },
    /// One command per preset: `head, preset, code, tail`, sent for presets
    /// 1 and 2 so either is active.
    Preset {
        head: &'static [u8],
        tail: &'static [u8],
        codes: Codes,
    },
    /// rivalcfg `multidpi_range_choice_xy` with one preset:
    /// `cmd, 1, first, x, y` ("xyxy", `pad_to` 0), or `cmd, 1, first, x,
    /// 0.., y, 0..` with each axis padded to `pad_to` codes ("xxyy").
    Xy {
        cmd: &'static [u8],
        sensor: Sensor,
        first: u8,
        pad_to: u8,
    },
}

/// rivalcfg per-preset `range` 100..=12000 to 0..=0x77.
pub const RANGE_12000: Codes = Codes::Linear {
    min: 100,
    max: 12000,
    step: 100,
    first: 0,
};

impl Dpi {
    /// One command takes a list of up to 5 presets.
    fn stages(self) -> bool {
        !matches!(self, Self::Preset { .. })
    }

    /// (min, max, step) of a slider; `None` for a fixed list of choices.
    fn range(self) -> Option<(u32, u32, u32)> {
        let sensor = |s: Sensor| {
            let (codes, min) = s.table();
            (min, min + 100 * (codes.len() as u32 - 1), 100)
        };
        match self {
            Self::Table { sensor: s, .. } | Self::Xy { sensor: s, .. } => Some(sensor(s)),
            Self::Step50 { .. } => Some((50, 18000, 50)),
            Self::Preset { codes, .. } => match codes {
                Codes::Linear { min, max, step, .. } => Some((min, max, step)),
                Codes::Sensor(s) => Some(sensor(s)),
                Codes::Choice(_) => None,
            },
        }
    }
}

/// rivalcfg `sleep_timer`: `cmd` + minutes (`min`..=20) times `unit`,
/// little-endian over `len` bytes.
#[derive(Debug, Clone, Copy)]
pub struct Sleep {
    pub cmd: &'static [u8],
    pub min: u8,
    pub unit: u32,
    pub len: usize,
}

/// rivalcfg `rgbgradient` header layouts.
#[derive(Debug, Clone, Copy)]
pub enum Gradient {
    /// `len` bytes: the LED id at each of `led_at`, the duration (2 bytes,
    /// little-endian) at `duration_at`, the repeat flag at `repeat_at`, the
    /// color count at `count_at`.
    V1 {
        len: usize,
        led_at: &'static [usize],
        duration_at: usize,
        repeat_at: usize,
        count_at: usize,
    },
    /// `rgbgradientv2` (Rival 500 / 700 / 710).
    V2,
}

/// Polling-rate command; codes are 1000/500/250/125 Hz = 1/2/3/4, or
/// 0/1/2/3 when `zero_based`.
#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub cmd: &'static [u8],
    pub zero_based: bool,
}

/// LED zones and how each takes a color.
#[derive(Debug, Clone, Copy)]
pub enum Zones {
    None,
    /// Rival 3: `05 00 <zone 1-4> r g b brightness`.
    Rival3,
    /// `21 mask r g b` with 3 zero bytes per lower zone before the color;
    /// one mask byte (`wide` = two, little-endian).
    Mask {
        count: u8,
        wide: bool,
    },
    /// Wireless Aerox: `21 01 zone r g b`.
    Indexed(u8),
    /// One LED: `prefix r g b suffix`.
    Fixed {
        prefix: &'static [u8],
        suffix: &'static [u8],
    },
    /// One command per LED: `prefix r g b` (Rival 300 logo / wheel).
    List(&'static [&'static [u8]]),
    /// One feature report per LED `0..leds`: `cmd` + a one-color gradient.
    Gradient {
        cmd: &'static [u8],
        leds: u8,
        header: &'static Gradient,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brightness {
    None,
    /// Part of the Rival 3 color command.
    Inline,
    /// `23 pct` (0-100).
    Pct,
    /// `23 code` for 0/25/50/75/100 % (Rival 5).
    Steps,
    /// `5f lo hi`, 0-256 (Prime).
    Prime,
    /// `<cmd> 01..04` = off / low / medium / high, one command per LED
    /// (Sensei RAW, Kana v2).
    Level4(&'static [&'static [u8]]),
}

#[derive(Debug, Clone, Copy)]
pub struct Light {
    pub zones: Zones,
    pub brightness: Brightness,
    /// Hardware rainbow effect; any color command ends it.
    pub rainbow: Option<&'static [u8]>,
    /// Sent before the colors to leave the rainbow (or breathing) effect,
    /// when a color alone does not.
    pub steady: &'static [&'static [u8]],
}

impl Light {
    pub const NONE: Light = Light {
        zones: Zones::None,
        brightness: Brightness::None,
        rainbow: None,
        steady: &[],
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Battery {
    /// `92` -> \[1\]: bit 7 charging, `((v & 0x7f) - 1) * 5` percent.
    Aerox,
    /// `aa 01` -> \[0\] percent, \[2\] charging.
    Rival,
}

pub struct MouseSpec {
    /// USB interface (rivalcfg "endpoint").
    pub interface: u32,
    /// PIDs of the 2.4 GHz dongle (opcode | 0x40, readback).
    pub wireless: &'static [u16],
    pub dpi: Dpi,
    pub rate: Option<Rate>,
    pub light: Light,
    pub battery: Option<Battery>,
    pub sleep: Option<Sleep>,
    /// rivalcfg `packet_length`: zero-pad every payload to this (0 = none).
    pub packet_len: usize,
    pub save: MouseSave,
}

/// The model's flash save command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseSave {
    /// rivalcfg `save_command` payload, sent like any other command.
    Command(&'static [u8]),
    /// Known, but not confirmed well enough: the reason.
    Blocked(&'static str),
}

// ── Protocol: payloads (without the report id) ──────────────────────────────

/// The sensitivity command for presets `dpis` (1 to 5), `active` selected
/// (0-based). Per-preset mice take `dpis[active]` only.
fn dpi_payloads(dpi: Dpi, dpis: &[u32], active: usize) -> Vec<Vec<u8>> {
    let (n, sel) = (dpis.len() as u8, active as u8);
    match dpi {
        Dpi::Table {
            cmd,
            sensor,
            first,
            len,
        } => {
            let mut p = [cmd, &[n, first + sel]].concat();
            for &d in dpis {
                let code = u32::from(sensor.code(d));
                p.extend_from_slice(&code.to_le_bytes()[..usize::from(len)]);
            }
            vec![p]
        }
        Dpi::Step50 { cmd, first, flags } => {
            let count = if flags { (1u8 << n) - 1 } else { n };
            let mut p = [cmd, &[count, first + sel]].concat();
            for &d in dpis {
                let code = ((d.clamp(50, 18000) + 25) / 50).clamp(1, 0x168) as u16;
                p.extend_from_slice(&code.to_le_bytes());
            }
            vec![p]
        }
        Dpi::Preset { head, tail, codes } => (1..=2u8)
            .map(|preset| [head, &[preset, codes.code(dpis[active])], tail].concat())
            .collect(),
        Dpi::Xy {
            cmd,
            sensor,
            first,
            pad_to,
        } => {
            let codes: Vec<u8> = dpis.iter().map(|&d| sensor.code(d)).collect();
            let mut p = [cmd, &[n, first + sel]].concat();
            if pad_to == 0 {
                // "xyxy"
                p.extend(codes.iter().flat_map(|&c| [c, c]));
            } else {
                // "xxyy", each axis padded to `pad_to` presets.
                for _ in 0..2 {
                    let start = p.len();
                    p.extend_from_slice(&codes);
                    p.resize(start + usize::from(pad_to), 0);
                }
            }
            vec![p]
        }
    }
}

fn sleep_payload(sleep: Sleep, minutes: u8) -> Vec<u8> {
    let v = u32::from(minutes.clamp(sleep.min, 20)) * sleep.unit;
    [sleep.cmd, &v.to_le_bytes()[..sleep.len]].concat()
}

/// rivalcfg's default gradient duration (ms), also sent for one color.
const GRADIENT_MS: u16 = 1000;

/// A one-color `rgbgradient` / `rgbgradientv2` frame for LED `led`.
fn gradient_payload(cmd: &[u8], header: &Gradient, led: u8, [r, g, b]: [u8; 3]) -> Vec<u8> {
    let mut p = cmd.to_vec();
    match *header {
        Gradient::V1 {
            len,
            led_at,
            duration_at,
            repeat_at,
            count_at,
        } => {
            let mut h = vec![0u8; len];
            for &at in led_at {
                h[at] = led;
            }
            h[duration_at..duration_at + 2].copy_from_slice(&GRADIENT_MS.to_le_bytes());
            // A single color repeats; one color stop at position 0.
            h[repeat_at] = 0x01;
            h[count_at] = 0x01;
            p.extend(h);
            p.extend_from_slice(&[r, g, b, r, g, b, 0x00]);
        }
        Gradient::V2 => {
            let mut h = vec![led, 0x1d, 0x01, 0x02, 0x31, 0x51, 0xff, 0xc8, 0x00];
            h.resize(139, 0);
            // Start color as (low nibble << 4, high nibble) pairs.
            for c in [r, g, b] {
                h.extend_from_slice(&[(c & 0x0f) << 4, c >> 4]);
            }
            // Suffix, focal point 1500 / 650, color stops - 1 = 0, duration.
            h.extend_from_slice(&[0xff, 0x00, 0xdc, 0x05, 0x8a, 0x02]);
            h.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
            h.extend_from_slice(&GRADIENT_MS.to_le_bytes());
            p.extend(h);
        }
    }
    p
}

fn rate_payload(rate: Rate, hz: u32) -> Vec<u8> {
    let code = match hz {
        125 => 4,
        250 => 3,
        500 => 2,
        _ => 1,
    } - u8::from(rate.zero_based);
    [rate.cmd, &[code]].concat()
}

fn color_payloads(zones: Zones, [r, g, b]: [u8; 3], brightness: u8) -> Vec<Vec<u8>> {
    match zones {
        Zones::None => vec![],
        Zones::Rival3 => (1..=4)
            .map(|zone| vec![0x05, 0x00, zone, r, g, b, brightness])
            .collect(),
        Zones::Mask { count, wide } => (0..count)
            .map(|i| {
                let mask = (1u16 << i).to_le_bytes();
                let mut p = vec![0x21];
                p.extend_from_slice(&mask[..if wide { 2 } else { 1 }]);
                p.resize(p.len() + 3 * usize::from(i), 0);
                p.extend_from_slice(&[r, g, b]);
                p
            })
            .collect(),
        Zones::Indexed(count) => (0..count).map(|i| vec![0x21, 0x01, i, r, g, b]).collect(),
        Zones::Fixed { prefix, suffix } => vec![[prefix, &[r, g, b], suffix].concat()],
        Zones::List(prefixes) => prefixes
            .iter()
            .map(|prefix| [prefix, &[r, g, b][..]].concat())
            .collect(),
        Zones::Gradient { cmd, leds, header } => (0..leds)
            .map(|led| gradient_payload(cmd, header, led, [r, g, b]))
            .collect(),
    }
}

fn brightness_payloads(kind: Brightness, pct: u8) -> Vec<Vec<u8>> {
    let pct = pct.min(100);
    match kind {
        Brightness::None | Brightness::Inline => vec![],
        Brightness::Pct => vec![vec![0x23, pct]],
        Brightness::Steps => vec![vec![
            0x23,
            match pct {
                0 => 0x00,
                1..=25 => 0x0c,
                26..=50 => 0x19,
                51..=75 => 0x32,
                _ => 0x64,
            },
        ]],
        Brightness::Prime => {
            let [lo, hi] = (u16::from(pct) * 256 / 100).to_le_bytes();
            vec![vec![0x5f, lo, hi]]
        }
        Brightness::Level4(cmds) => {
            let level = match pct {
                0 => 0x01,
                1..=33 => 0x02,
                34..=66 => 0x03,
                _ => 0x04,
            };
            cmds.iter()
                .map(|cmd| [cmd, &[level][..]].concat())
                .collect()
        }
    }
}

fn battery_request(kind: Battery) -> Vec<u8> {
    match kind {
        Battery::Aerox => vec![0x92],
        Battery::Rival => vec![0xaa, 0x01],
    }
}

/// (percent, charging); `None` for an implausible reply (mouse asleep/off).
fn parse_battery(kind: Battery, r: &[u8]) -> Option<(u8, bool)> {
    let (level, charging) = match kind {
        Battery::Aerox => {
            let v = *r.get(1)?;
            ((i16::from(v & 0x7f) - 1) * 5, v & 0x80 != 0)
        }
        Battery::Rival => (i16::from(*r.first()?), *r.get(2)? != 0),
    };
    (0..=100)
        .contains(&level)
        .then_some((level as u8, charging))
}

/// Report id 0 + payload zero-padded to `len`; the dongle wants bit 6 set
/// on the opcode.
fn frame(payload: &[u8], wireless: bool, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len().max(len) + 1);
    v.push(0x00);
    v.extend_from_slice(payload);
    v.resize(v.len().max(len + 1), 0);
    if wireless {
        v[1] |= 0x40;
    }
    v
}

const LIGHT_KEYS: [SettingKey; 5] = [
    SettingKey::Effect,
    SettingKey::Brightness,
    SettingKey::ColorR,
    SettingKey::ColorG,
    SettingKey::ColorB,
];

const STAGE_KEYS: [SettingKey; 5] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];
/// Stages the first stage change starts from.
const DEFAULT_STAGES: [u32; 5] = [400, 800, 1600, 3200, 6400];

fn is_dpi_key(key: SettingKey) -> bool {
    key == SettingKey::Dpi || key == SettingKey::DpiStageActive || STAGE_KEYS.contains(&key)
}

/// (DPI per stage, active stage 0-based) once any stage is stored, or with
/// `force`; unset stages take the defaults.
fn stage_list(dpi: Dpi, s: &SettingsMap, force: bool) -> Option<(Vec<u32>, usize)> {
    let (min, max, _) = dpi.range().filter(|_| dpi.stages())?;
    let stored = STAGE_KEYS
        .iter()
        .chain([&SettingKey::DpiStageActive])
        .any(|&k| get_setting(s, k).is_some());
    if !(stored || force) {
        return None;
    }
    let dpis = STAGE_KEYS
        .iter()
        .zip(DEFAULT_STAGES)
        .map(|(&k, d)| int(s, k).map_or(d, |v| v.max(0) as u32).clamp(min, max))
        .collect();
    let active = int(s, SettingKey::DpiStageActive).map_or(0, |v| v.clamp(1, 5) as usize - 1);
    Some((dpis, active))
}

/// Once a stage is set the mouse takes the whole list: store it in full,
/// with the DPI slider editing (and showing) the active stage. A new list
/// starts its active stage at the current DPI.
fn link_stages(dpi: Dpi, s: &mut SettingsMap, key: SettingKey) {
    use SettingKey as K;
    let staged = is_dpi_key(key) && key != K::Dpi;
    let Some((mut dpis, active)) = stage_list(dpi, s, staged).filter(|_| is_dpi_key(key)) else {
        return;
    };
    if key == K::Dpi || get_setting(s, STAGE_KEYS[active]).is_none() {
        if let Some(v) = int(s, K::Dpi) {
            dpis[active] = v.max(0) as u32;
        }
    }
    for (&k, &v) in STAGE_KEYS.iter().zip(&dpis) {
        merge_setting(s, k, SettingValue::Int(v.into()));
    }
    merge_setting(s, K::DpiStageActive, SettingValue::Int(active as i64 + 1));
    merge_setting(s, K::Dpi, SettingValue::Int(dpis[active].into()));
}

// ── Controls ────────────────────────────────────────────────────────────────

fn rainbow_on(spec: &MouseSpec, settings: &SettingsMap) -> bool {
    spec.light.rainbow.is_some()
        && get_setting(settings, SettingKey::Effect).and_then(|v| v.as_str()) == Some("rainbow")
}

pub(super) fn controls(spec: &MouseSpec, settings: &SettingsMap) -> Vec<SettingMeta> {
    let mut c = setting_controls(spec, settings);
    let save =
        SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory").persistent();
    c.push(match spec.save {
        MouseSave::Command(_) => save,
        MouseSave::Blocked(reason) => save.blocked(reason),
    });
    c
}

fn setting_controls(spec: &MouseSpec, settings: &SettingsMap) -> Vec<SettingMeta> {
    use SettingKey as K;
    let dpi = match (spec.dpi, spec.dpi.range()) {
        (_, Some((min, max, step))) => {
            SettingMeta::slider(K::Dpi, "DPI", min.into(), max.into(), step.into()).unit("dpi")
        }
        (
            Dpi::Preset {
                codes: Codes::Choice(choices),
                ..
            },
            None,
        ) => SettingMeta::dropdown(
            K::Dpi,
            "DPI",
            choices
                .iter()
                .map(|&(dpi, _)| SettingOption::int(dpi.into(), &format!("{dpi} DPI")))
                .collect(),
        ),
        _ => unreachable!("only choice tables have no range"),
    };
    let mut c = vec![dpi];
    if let (true, Some((min, max, step))) = (spec.dpi.stages(), spec.dpi.range()) {
        for (i, &key) in STAGE_KEYS.iter().enumerate() {
            let label = format!("DPI stage {}", i + 1);
            c.push(
                SettingMeta::slider(key, &label, min.into(), max.into(), step.into()).unit("dpi"),
            );
        }
        c.push(SettingMeta::dropdown(
            K::DpiStageActive,
            "Active DPI stage",
            (1..=5)
                .map(|i| SettingOption::int(i, &i.to_string()))
                .collect(),
        ));
    }
    if let Some(sleep) = spec.sleep {
        c.push(
            SettingMeta::slider(K::PmShutdown, "Sleep after", sleep.min.into(), 20.0, 1.0)
                .unit("min"),
        );
    }
    if spec.rate.is_some() {
        let options = [125, 250, 500, 1000]
            .map(|hz| SettingOption::int(hz, &format!("{hz} Hz")))
            .to_vec();
        c.push(SettingMeta::dropdown(
            K::ReportRate,
            "Polling rate",
            options,
        ));
    }
    let light = spec.light;
    let colors = !matches!(light.zones, Zones::None);
    if !colors && light.brightness == Brightness::None {
        return c;
    }
    let rainbow = rainbow_on(spec, settings);
    if light.rainbow.is_some() {
        c.push(SettingMeta::dropdown(
            K::Effect,
            "Effect",
            vec![
                SettingOption::text("static", "Static"),
                SettingOption::text("rainbow", "Rainbow"),
            ],
        ));
    }
    match light.brightness {
        Brightness::None => {}
        Brightness::Steps => c.push(SettingMeta::dropdown(
            K::Brightness,
            "Brightness",
            [0, 25, 50, 75, 100]
                .map(|p| SettingOption::int(p, &format!("{p}%")))
                .to_vec(),
        )),
        Brightness::Level4(_) => c.push(SettingMeta::dropdown(
            K::Brightness,
            "Brightness",
            vec![
                SettingOption::int(0, "Off"),
                SettingOption::int(33, "Low"),
                SettingOption::int(66, "Medium"),
                SettingOption::int(100, "High"),
            ],
        )),
        _ => c.push(SettingMeta::slider(K::Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%")),
    }
    if colors {
        for (key, label) in [
            (K::ColorR, "Red"),
            (K::ColorG, "Green"),
            (K::ColorB, "Blue"),
        ] {
            c.push(SettingMeta::color(key, label).hidden(rainbow));
        }
    }
    c
}

// ── Adapter ─────────────────────────────────────────────────────────────────

pub(super) fn open(
    pid: u16,
    name: &'static str,
    spec: &'static MouseSpec,
    ctx: &DriverCtx,
) -> Opened {
    Ok(vec![Box::new(MouseAdapter::try_open(
        pid,
        name,
        spec,
        ctx.paths.clone(),
    )?)])
}

pub struct MouseAdapter {
    id: DeviceId,
    name: &'static str,
    spec: &'static MouseSpec,
    wireless: bool,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    battery: BatteryState,
    last_error: Option<String>,
    last_battery: Instant,
    failures: u8,
    paths: ConfigPaths,
}

impl MouseAdapter {
    fn try_open(
        pid: u16,
        name: &'static str,
        spec: &'static MouseSpec,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let info = find_hidraw(VID, pid, Some(spec.interface))?;
        let dev = HidrawDevice::open(&info.path)?;
        let settings = hako_core::config::load_device_settings(&paths, VID, pid)?;
        let mut a = Self {
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: instance(&info),
            },
            name,
            spec,
            wireless: spec.wireless.contains(&pid),
            dev: Some(dev),
            settings,
            battery: BatteryState {
                label: "Mouse".into(),
                percent: None,
                charging: false,
                present: false,
            },
            last_error: None,
            last_battery: Instant::now(),
            failures: 0,
            paths,
        };
        // Only what the user set before: never overwrite onboard defaults.
        let stored = |k: &SettingKey| get_setting(&a.settings, *k).is_some();
        let mut restore: Vec<_> = [
            SettingKey::Dpi,
            SettingKey::ReportRate,
            SettingKey::PmShutdown,
        ]
        .into_iter()
        .filter(stored)
        .collect();
        if LIGHT_KEYS.iter().any(stored) {
            restore.push(SettingKey::Effect);
        }
        for key in restore {
            let payloads = a.payloads(&a.settings, key);
            if let Err(e) = a.write_all(&payloads, a.feature(key)) {
                warn!("{name} restore {}: {e}", key.as_str());
                a.last_error = Some(e.to_string());
            }
        }
        a.poll_battery();
        info!(
            "SteelSeries {name} open {} wireless={}",
            info.path.display(),
            a.wireless
        );
        Ok(a)
    }

    /// Payloads that put `key` (from `s`) on the device. Lighting keys
    /// resend the whole lighting state.
    fn payloads(&self, s: &SettingsMap, key: SettingKey) -> Vec<Vec<u8>> {
        use SettingKey as K;
        let spec = self.spec;
        match key {
            k if is_dpi_key(k) => match stage_list(spec.dpi, s, false) {
                Some((dpis, active)) => dpi_payloads(spec.dpi, &dpis, active),
                None => int(s, K::Dpi).map_or(vec![], |v| dpi_payloads(spec.dpi, &[v as u32], 0)),
            },
            K::ReportRate => match (spec.rate, int(s, K::ReportRate)) {
                (Some(rate), Some(hz)) => vec![rate_payload(rate, hz as u32)],
                _ => vec![],
            },
            K::PmShutdown => match (spec.sleep, int(s, K::PmShutdown)) {
                (Some(sleep), Some(min)) => vec![sleep_payload(sleep, min.clamp(0, 20) as u8)],
                _ => vec![],
            },
            k if LIGHT_KEYS.contains(&k) => {
                let light = spec.light;
                let pct = int(s, K::Brightness).map(|v| v.clamp(0, 100) as u8);
                let mut out: Vec<Vec<u8>> = pct
                    .map(|p| brightness_payloads(light.brightness, p))
                    .unwrap_or_default();
                if rainbow_on(spec, s) {
                    out.extend(light.rainbow.map(<[u8]>::to_vec));
                } else if !matches!(light.zones, Zones::None) {
                    out.extend(light.steady.iter().map(|p| p.to_vec()));
                    let ch = |k| int(s, k).unwrap_or(0).clamp(0, 255) as u8;
                    out.extend(color_payloads(
                        light.zones,
                        [ch(K::ColorR), ch(K::ColorG), ch(K::ColorB)],
                        pct.unwrap_or(100),
                    ));
                }
                out
            }
            _ => vec![],
        }
    }

    /// `key` goes out as feature reports (gradient colors).
    fn feature(&self, key: SettingKey) -> bool {
        LIGHT_KEYS.contains(&key) && matches!(self.spec.light.zones, Zones::Gradient { .. })
    }

    fn write_all(&mut self, payloads: &[Vec<u8>], feature: bool) -> Result<()> {
        let (wireless, len) = (self.wireless, self.spec.packet_len);
        let dev = self.dev.as_mut().context("no device")?;
        for p in payloads {
            if feature {
                dev.set_feature(&mut frame(p, false, len))?;
            } else if wireless {
                // The dongle answers every command; take the answer off the queue.
                request(dev, &frame(p, true, len), |_| true, REPLY_TIMEOUT)?;
            } else {
                dev.write(&frame(p, false, len))?;
            }
            std::thread::sleep(COMMAND_GAP);
        }
        Ok(())
    }

    fn poll_battery(&mut self) {
        self.last_battery = Instant::now();
        let Some(kind) = self.spec.battery else {
            return;
        };
        let wireless = self.wireless;
        let Some(dev) = self.dev.as_mut() else {
            return;
        };
        let req = frame(&battery_request(kind), wireless, self.spec.packet_len);
        match request(
            dev,
            &req,
            |r| parse_battery(kind, r).is_some(),
            REPLY_TIMEOUT,
        ) {
            Ok(reply) => {
                self.failures = 0;
                let b = reply.and_then(|r| parse_battery(kind, &r));
                self.battery.percent = b.map(|(p, _)| p);
                self.battery.charging = b.is_some_and(|(_, c)| c);
                self.battery.present = b.is_some();
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                self.last_error = Some(e.to_string());
                if self.failures >= 3 {
                    self.dev = None;
                }
            }
        }
    }
}

impl DeviceAdapter for MouseAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: if self.spec.battery.is_some() {
                vec![self.battery.clone()]
            } else {
                vec![]
            },
            settings: self.settings.clone(),
            controls: controls(self.spec, &self.settings),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let mut proposed = self.settings.clone();
        merge_setting(&mut proposed, key, value.clone());
        let save = match self.spec.save {
            MouseSave::Command(cmd) => Some(cmd.to_vec()),
            MouseSave::Blocked(_) => None,
        };
        if key == SettingKey::SaveToDevice {
            // Switching on stores the current state once; off writes nothing.
            if value.as_bool() == Some(true) {
                let cmd = save.clone().context("this mouse has no save command")?;
                self.write_all(&[cmd], false)?;
            }
        } else {
            link_stages(self.spec.dpi, &mut proposed, key);
            let payloads = self.payloads(&proposed, key);
            if payloads.is_empty() {
                bail!("unsupported setting {}", key.as_str());
            }
            self.write_all(&payloads, self.feature(key))?;
        }
        hako_core::config::save_device_settings(&self.paths, VID, self.id.product_id, &proposed)?;
        self.settings = proposed;
        self.last_error = None;
        let saving = get_setting(&self.settings, SettingKey::SaveToDevice)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if key != SettingKey::SaveToDevice && saving {
            if let Some(cmd) = save {
                self.write_all(&[cmd], false)?;
            }
        }
        Ok(())
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_some() && self.last_battery.elapsed() >= BATTERY_POLL {
            self.poll_battery();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE: Dpi = Dpi::Table {
        cmd: &[0x0b, 0x00],
        sensor: Sensor::TrueMoveCore,
        first: 1,
        len: 1,
    };

    #[test]
    fn dpi_payloads_match_rivalcfg() {
        // Rival 3 `--sensitivity 800`: 0B 00 01 01 12.
        assert_eq!(
            dpi_payloads(CORE, &[800], 0),
            vec![vec![0x0b, 0x00, 0x01, 0x01, 0x12]]
        );
        // Rounded to the nearest 100 and clamped to the sensor range.
        assert_eq!(
            dpi_payloads(CORE, &[1649], 0),
            vec![vec![0x0b, 0x00, 0x01, 0x01, 0x24]]
        );
        assert_eq!(dpi_payloads(CORE, &[50], 0)[0][4], 0x04);
        assert_eq!(dpi_payloads(CORE, &[9000], 0)[0][4], 0xc5);
        // Aerox 3 Wireless 18000: 2D 01 00 D6; Rival 3 Wireless 1600 (2-byte code).
        let air = |cmd, first, len| Dpi::Table {
            cmd,
            sensor: Sensor::TrueMoveAir,
            first,
            len,
        };
        assert_eq!(
            dpi_payloads(air(&[0x2d], 0, 1), &[18000], 0),
            vec![vec![0x2d, 0x01, 0x00, 0xd6]]
        );
        assert_eq!(
            dpi_payloads(air(&[0x20], 1, 2), &[1600], 0),
            vec![vec![0x20, 0x01, 0x01, 0x12, 0x00]]
        );
        // Prime 18000 = 0x168 little-endian; Sensei TEN 800 = 16.
        let prime = Dpi::Step50 {
            cmd: &[0x61],
            first: 0,
            flags: false,
        };
        let sensei_ten = Dpi::Step50 {
            cmd: &[0x55, 0x00],
            first: 1,
            flags: true,
        };
        assert_eq!(
            dpi_payloads(prime, &[18000], 0),
            vec![vec![0x61, 0x01, 0x00, 0x68, 0x01]]
        );
        assert_eq!(
            dpi_payloads(sensei_ten, &[800], 0),
            vec![vec![0x55, 0x00, 0x01, 0x01, 0x10, 0x00]]
        );
        // Rival 600: 03 00 <preset> <(dpi-100)/100> 00 42, presets 1 and 2.
        let rival600 = Dpi::Preset {
            head: &[0x03, 0x00],
            tail: &[0x00, 0x42],
            codes: RANGE_12000,
        };
        assert_eq!(
            dpi_payloads(rival600, &[12000], 0),
            vec![
                vec![0x03, 0x00, 0x01, 0x77, 0x00, 0x42],
                vec![0x03, 0x00, 0x02, 0x77, 0x00, 0x42]
            ]
        );
        let rival650 = Dpi::Preset {
            head: &[0x15],
            tail: &[],
            codes: RANGE_12000,
        };
        assert_eq!(dpi_payloads(rival650, &[100], 0)[0], vec![0x15, 0x01, 0x00]);
    }

    #[test]
    fn stage_lists_match_rivalcfg() {
        // Aerox 3 Wireless spec capture `--sensitivity 200,400,800,1600,18000`.
        let air = Dpi::Table {
            cmd: &[0x2d],
            sensor: Sensor::TrueMoveAir,
            first: 0,
            len: 1,
        };
        assert_eq!(
            dpi_payloads(air, &[200, 400, 800, 1600, 18000], 0),
            vec![vec![0x2d, 0x05, 0x00, 0x02, 0x04, 0x09, 0x12, 0xd6]]
        );
        assert_eq!(
            dpi_payloads(air, &[200, 400], 1)[0][..3],
            [0x2d, 0x02, 0x01]
        );
        // multidpi_range "flag" count: 2 presets = 0b11; "number" = 2.
        let step = |flags| Dpi::Step50 {
            cmd: &[0x55, 0x00],
            first: 1,
            flags,
        };
        assert_eq!(
            dpi_payloads(step(true), &[50, 100], 1),
            vec![vec![0x55, 0x00, 0b11, 0x02, 0x01, 0x00, 0x02, 0x00]]
        );
        assert_eq!(dpi_payloads(step(false), &[50, 100], 0)[0][2], 2);
        assert_eq!(dpi_payloads(step(true), &[50; 5], 0)[0][2], 0b1_1111);
        // xxyy pads each axis to 5 presets; xyxy interleaves.
        let xy = |pad_to| Dpi::Xy {
            cmd: &[0x2c],
            sensor: Sensor::TrueMoveAir,
            first: 1,
            pad_to,
        };
        assert_eq!(
            dpi_payloads(xy(5), &[100, 200], 1),
            vec![vec![0x2c, 2, 2, 0x00, 0x02, 0, 0, 0, 0x00, 0x02, 0, 0, 0]]
        );
        assert_eq!(
            dpi_payloads(xy(0), &[100, 200], 0),
            vec![vec![0x2c, 2, 1, 0x00, 0x00, 0x02, 0x02]]
        );
    }

    #[test]
    fn stages_and_dpi_stay_linked() {
        use SettingKey as K;
        let air = Dpi::Table {
            cmd: &[0x2d],
            sensor: Sensor::TrueMoveAir,
            first: 0,
            len: 1,
        };
        // No stage set: the DPI slider sends one preset.
        let mut s = SettingsMap::new();
        merge_setting(&mut s, K::Dpi, SettingValue::Int(800));
        link_stages(air, &mut s, K::Dpi);
        assert!(stage_list(air, &s, false).is_none());
        // Setting a stage stores the whole list and shows the active stage.
        merge_setting(&mut s, K::DpiStage2, SettingValue::Int(1200));
        link_stages(air, &mut s, K::DpiStage2);
        assert_eq!(
            stage_list(air, &s, false),
            Some((vec![800, 1200, 1600, 3200, 6400], 0))
        );
        assert_eq!(int(&s, K::Dpi), Some(800));
        merge_setting(&mut s, K::DpiStageActive, SettingValue::Int(2));
        link_stages(air, &mut s, K::DpiStageActive);
        assert_eq!(int(&s, K::Dpi), Some(1200));
        // The DPI slider then edits the active stage.
        merge_setting(&mut s, K::Dpi, SettingValue::Int(2000));
        link_stages(air, &mut s, K::Dpi);
        assert_eq!(int(&s, K::DpiStage2), Some(2000));
        // Per-preset mice have no stages.
        let preset = Dpi::Preset {
            head: &[0x03],
            tail: &[],
            codes: RANGE_12000,
        };
        assert!(stage_list(preset, &s, true).is_none());
    }

    #[test]
    fn sleep_and_gradient_payloads_match_rivalcfg() {
        // Aerox 3 Wireless spec capture: 29 + ms (3 bytes LE).
        let aerox = Sleep {
            cmd: &[0x29],
            min: 0,
            unit: 60_000,
            len: 3,
        };
        assert_eq!(sleep_payload(aerox, 0), vec![0x29, 0x00, 0x00, 0x00]);
        assert_eq!(sleep_payload(aerox, 5), vec![0x29, 0xe0, 0x93, 0x04]);
        assert_eq!(sleep_payload(aerox, 20), vec![0x29, 0x80, 0x4f, 0x12]);
        // Rival 650 spec capture: 2b 01 01 00 00 00 + seconds (2 bytes LE).
        let rival650 = Sleep {
            cmd: &[0x2b, 0x01, 0x01, 0x00, 0x00, 0x00],
            min: 1,
            unit: 60,
            len: 2,
        };
        assert_eq!(
            sleep_payload(rival650, 5),
            vec![0x2b, 0x01, 0x01, 0x00, 0x00, 0x00, 0x2c, 0x01]
        );
        assert_eq!(sleep_payload(rival650, 0)[6..], [0x3c, 0x00]);

        // rivalcfg test_rgbgradient.py: LED 2, #ff2200.
        const V1: Gradient = Gradient::V1 {
            len: 26,
            led_at: &[0],
            duration_at: 1,
            repeat_at: 17,
            count_at: 25,
        };
        let mut want = vec![0x5b, 0x00, 0x02, 0xe8, 0x03];
        want.resize(2 + 17, 0);
        want.push(0x01);
        want.resize(2 + 25, 0);
        want.extend_from_slice(&[0x01, 0xff, 0x22, 0x00, 0xff, 0x22, 0x00, 0x00]);
        assert_eq!(
            gradient_payload(&[0x5b, 0x00], &V1, 2, [0xff, 0x22, 0x00]),
            want
        );
        // Rival 600 header: LED id twice (0 and 5), duration at 6.
        const RIVAL600: Gradient = Gradient::V1 {
            len: 28,
            led_at: &[0, 5],
            duration_at: 6,
            repeat_at: 22,
            count_at: 27,
        };
        let p = gradient_payload(&[0x05, 0x00], &RIVAL600, 7, [1, 2, 3]);
        assert_eq!(p.len(), 2 + 28 + 7);
        assert_eq!(
            (p[2], p[7], &p[8..10], p[24], p[29]),
            (7, 7, &[0xe8, 0x03][..], 1, 1)
        );
        // rivalcfg test_rgbgradientv2.py: LED 2, #ff2200, 161 bytes.
        let p = gradient_payload(&[0x05, 0x00], &Gradient::V2, 2, [0xff, 0x22, 0x00]);
        assert_eq!(p.len(), 2 + 161);
        let p = &p[2..];
        assert_eq!(
            &p[..9],
            &[0x02, 0x1d, 0x01, 0x02, 0x31, 0x51, 0xff, 0xc8, 0x00]
        );
        assert!(p[9..139].iter().all(|&b| b == 0));
        assert_eq!(
            &p[139..],
            &[
                0xf0, 0x0f, 0x20, 0x02, 0x00, 0x00, 0xff, 0x00, 0xdc, 0x05, 0x8a, 0x02, 0x00, 0x00,
                0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0xe8, 0x03
            ]
        );
        // One report per LED.
        let zones = Zones::Gradient {
            cmd: &[0x05, 0x00],
            leds: 8,
            header: &RIVAL600,
        };
        let c = color_payloads(zones, [1, 2, 3], 0);
        assert_eq!(c.len(), 8);
        assert_eq!(c[3][2], 3);
    }

    #[test]
    fn rate_color_and_brightness_payloads() {
        let one = Rate {
            cmd: &[0x04, 0x00],
            zero_based: false,
        };
        let zero = Rate {
            cmd: &[0x2b],
            zero_based: true,
        };
        assert_eq!(rate_payload(one, 1000), vec![0x04, 0x00, 0x01]);
        assert_eq!(rate_payload(one, 125), vec![0x04, 0x00, 0x04]);
        assert_eq!(rate_payload(zero, 1000), vec![0x2b, 0x00]);
        assert_eq!(rate_payload(zero, 250), vec![0x2b, 0x02]);

        let rgb = [0x11, 0x22, 0x33];
        let rival3 = color_payloads(Zones::Rival3, rgb, 0x64);
        assert_eq!(rival3.len(), 4);
        assert_eq!(rival3[0], vec![0x05, 0x00, 0x01, 0x11, 0x22, 0x33, 0x64]);
        assert_eq!(rival3[3][2], 0x04);
        // Aerox 3: 21 01 rgb / 21 02 000000 rgb / 21 04 000000000000 rgb.
        let aerox = color_payloads(
            Zones::Mask {
                count: 3,
                wide: false,
            },
            rgb,
            0,
        );
        assert_eq!(aerox[0], vec![0x21, 0x01, 0x11, 0x22, 0x33]);
        assert_eq!(aerox[1], vec![0x21, 0x02, 0, 0, 0, 0x11, 0x22, 0x33]);
        assert_eq!(
            aerox[2],
            vec![0x21, 0x04, 0, 0, 0, 0, 0, 0, 0x11, 0x22, 0x33]
        );
        // Rival 5 logo: flags 00 02, 9 padded zones.
        let rival5 = color_payloads(
            Zones::Mask {
                count: 10,
                wide: true,
            },
            rgb,
            0,
        );
        assert_eq!(rival5.len(), 10);
        assert_eq!(&rival5[9][..3], &[0x21, 0x00, 0x02]);
        assert_eq!(rival5[9].len(), 3 + 27 + 3);
        assert_eq!(rival5[8][..3], [0x21, 0x00, 0x01]);
        // Aerox 3 Wireless: 21 01 <zone> rgb.
        assert_eq!(
            color_payloads(Zones::Indexed(3), rgb, 0)[2],
            vec![0x21, 0x01, 0x02, 0x11, 0x22, 0x33]
        );
        // Prime: 62 01 rgb + 15 zeros + ff.
        let prime = color_payloads(
            Zones::Fixed {
                prefix: &[0x62, 0x01],
                suffix: &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff],
            },
            rgb,
            0,
        );
        assert_eq!(prime[0].len(), 21);
        assert_eq!(&prime[0][..5], &[0x62, 0x01, 0x11, 0x22, 0x33]);
        assert_eq!(prime[0][20], 0xff);

        assert_eq!(
            brightness_payloads(Brightness::Pct, 40),
            vec![vec![0x23, 40]]
        );
        assert_eq!(
            brightness_payloads(Brightness::Steps, 75),
            vec![vec![0x23, 0x32]]
        );
        assert_eq!(
            brightness_payloads(Brightness::Steps, 25),
            vec![vec![0x23, 0x0c]]
        );
        assert_eq!(
            brightness_payloads(Brightness::Prime, 100),
            vec![vec![0x5f, 0x00, 0x01]]
        );
        assert!(brightness_payloads(Brightness::Inline, 50).is_empty());
    }

    #[test]
    fn older_mice_payloads_match_rivalcfg() {
        let preset = |codes| Dpi::Preset {
            head: &[0x03],
            tail: &[],
            codes,
        };
        // Rival 110 (pmw3326.py): 800 -> 0x12, 500 -> 0x0b, 5100 -> 0x76, 7200 -> 0xa7.
        let pmw = preset(Codes::Sensor(Sensor::Pmw3326));
        assert_eq!(
            dpi_payloads(pmw, &[800], 0),
            vec![vec![0x03, 0x01, 0x12], vec![0x03, 0x02, 0x12]]
        );
        for (dpi, code) in [
            (500, 0x0b),
            (2400, 0x37),
            (5100, 0x76),
            (7200, 0xa7),
            (9000, 0xa7),
        ] {
            assert_eq!(dpi_payloads(pmw, &[dpi], 0)[0][2], code, "{dpi}");
        }
        // Rival 300: 50..=6500 / 50 -> 0x01..=0x82; Sensei RAW 90..=5670 / 90 -> 0x01..=0x3f.
        let linear = |min, max, step| {
            preset(Codes::Linear {
                min,
                max,
                step,
                first: 1,
            })
        };
        assert_eq!(
            dpi_payloads(linear(50, 6500, 50), &[800], 0)[0],
            vec![0x03, 0x01, 0x10]
        );
        assert_eq!(
            dpi_payloads(linear(50, 6500, 50), &[6500], 0)[1],
            vec![0x03, 0x02, 0x82]
        );
        assert_eq!(dpi_payloads(linear(90, 5670, 90), &[1620], 0)[0][2], 0x12);
        assert_eq!(dpi_payloads(linear(90, 5670, 90), &[5670], 0)[0][2], 0x3f);
        // Rival 100 choices: 1000 -> 0x06, 4000 -> 0x01.
        let choice = preset(Codes::Choice(&[(1000, 0x06), (1250, 0x05), (4000, 0x01)]));
        assert_eq!(dpi_payloads(choice, &[1000], 0)[0], vec![0x03, 0x01, 0x06]);
        assert_eq!(dpi_payloads(choice, &[4000], 0)[1], vec![0x03, 0x02, 0x01]);
        // Rival 3 Gen 2 (xyxy): 34 01 00 x y; Rival 3 Wireless Gen 2 (xxyy, 5 presets).
        let xy = |cmd, sensor, first, pad_to| Dpi::Xy {
            cmd,
            sensor,
            first,
            pad_to,
        };
        assert_eq!(
            dpi_payloads(xy(&[0x34], Sensor::TrueMoveCore, 0, 0), &[800], 0),
            vec![vec![0x34, 0x01, 0x00, 0x12, 0x12]]
        );
        assert_eq!(
            dpi_payloads(xy(&[0x2c], Sensor::TrueMoveAir, 1, 5), &[1600], 0),
            vec![vec![0x2c, 0x01, 0x01, 0x12, 0, 0, 0, 0, 0x12, 0, 0, 0, 0]]
        );
        // Rival 300 logo + wheel colors; Kana v2 two LEDs at "medium".
        let rgb = [1, 2, 3];
        assert_eq!(
            color_payloads(Zones::List(&[&[0x08, 0x01], &[0x08, 0x02]]), rgb, 0),
            vec![vec![0x08, 0x01, 1, 2, 3], vec![0x08, 0x02, 1, 2, 3]]
        );
        let kana = Brightness::Level4(&[&[0x05, 0x01], &[0x05, 0x02]]);
        assert_eq!(
            brightness_payloads(kana, 66),
            vec![vec![0x05, 0x01, 0x03], vec![0x05, 0x02, 0x03]]
        );
        assert_eq!(brightness_payloads(kana, 0)[0], vec![0x05, 0x01, 0x01]);
        // Rival 110: every report padded to 32 bytes after the report id.
        let f = frame(&[0x03, 0x01, 0x12], false, 32);
        assert_eq!((f.len(), &f[..4]), (33, &[0x00, 0x03, 0x01, 0x12][..]));
    }

    #[test]
    fn controls_match_dpi_kind_and_lighting() {
        let spec = MouseSpec {
            interface: 0,
            wireless: &[],
            dpi: Dpi::Preset {
                head: &[0x03],
                tail: &[],
                codes: Codes::Choice(&[(400, 4), (800, 3)]),
            },
            rate: None,
            light: Light {
                zones: Zones::None,
                brightness: Brightness::Level4(&[&[0x05, 0x01]]),
                rainbow: None,
                steady: &[],
            },
            battery: None,
            sleep: None,
            packet_len: 0,
            save: MouseSave::Blocked("x"),
        };
        let c = controls(&spec, &SettingsMap::new());
        let keys: Vec<_> = c.iter().map(|m| m.key).collect();
        assert_eq!(
            keys,
            vec![
                SettingKey::Dpi,
                SettingKey::Brightness,
                SettingKey::SaveToDevice
            ]
        );
        assert!(c[0].validate(&SettingValue::Int(800)).is_ok());
        assert!(c[0].validate(&SettingValue::Int(600)).is_err());
    }

    #[test]
    fn wireless_frame_and_battery() {
        assert_eq!(frame(&[0x92], false, 0), vec![0x00, 0x92]);
        assert_eq!(frame(&[0x92], true, 0), vec![0x00, 0xd2]);
        assert_eq!(
            frame(&[0x21, 0x01, 0x00, 1, 2, 3], true, 0)[..3],
            [0x00, 0x61, 0x01]
        );
        // Aerox: 0x80 | level; ((v & 0x7f) - 1) * 5.
        assert_eq!(
            parse_battery(Battery::Aerox, &[0x92, 0x80 | 21]),
            Some((100, true))
        );
        assert_eq!(
            parse_battery(Battery::Aerox, &[0x92, 11]),
            Some((50, false))
        );
        assert_eq!(parse_battery(Battery::Aerox, &[0x92, 0]), None);
        assert_eq!(parse_battery(Battery::Rival, &[64, 0, 1]), Some((64, true)));
        assert_eq!(parse_battery(Battery::Rival, &[200, 0, 0]), None);
    }

    #[test]
    fn save_frames_match_rivalcfg_and_libratbag() {
        // rivalcfg save_command / libratbag STEELSERIES_ID_SAVE_SHORT (09),
        // STEELSERIES_ID_SAVE (59), report id 0 first.
        assert_eq!(frame(&[0x09, 0x00], false, 0), vec![0x00, 0x09, 0x00]);
        assert_eq!(frame(&[0x59, 0x00], false, 0), vec![0x00, 0x59, 0x00]);
        assert_eq!(frame(&[0x09], false, 0), vec![0x00, 0x09]);
        let spec = |save| MouseSpec {
            interface: 0,
            wireless: &[],
            dpi: Dpi::Step50 {
                cmd: &[0x61],
                first: 0,
                flags: false,
            },
            rate: None,
            light: Light::NONE,
            battery: None,
            sleep: None,
            packet_len: 0,
            save,
        };
        let save = |s: &MouseSpec| {
            controls(s, &SettingsMap::new())
                .into_iter()
                .find(|m| m.key == SettingKey::SaveToDevice)
                .unwrap()
        };
        let on = save(&spec(MouseSave::Command(&[0x09, 0x00])));
        assert!(on.persistent && on.blocked.is_none());
        assert!(on.validate(&SettingValue::Bool(true)).is_ok());
        let off = save(&spec(MouseSave::Blocked("why")));
        assert!(off.persistent && off.blocked.as_deref() == Some("why"));
    }
}
