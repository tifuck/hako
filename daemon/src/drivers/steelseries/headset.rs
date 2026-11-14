//! Arctis / Arctis Nova headsets other than the Nova Pro Wireless.
//!
//! Byte layouts from HeadsetControl (`lib/devices/steelseries_arctis_*.hpp`,
//! `protocols/steelseries_protocol.hpp`), reimplemented:
//! - Legacy (Arctis 1/7/9/Pro Wireless): 31-byte output reports; the first
//!   byte is the report id (`0x06` for Arctis 1/7).
//! - Nova (Arctis 7+, Nova 3P/5/7, GameBuds): report id `0x00`, 64-byte output
//!   reports, status = `00 b0`, answered by a report starting with `0xb0`.
//! - Nova 3 (wired): 64-byte feature reports with report id `0x06`.
//! - Nova Pro (wired GameDAC): 64-byte output reports `06 37` mic level
//!   (1-10) and `06 39` sidetone (0-3) on interface 4, from
//!   Linux-Arctis-Manager `base_arctis_nova_pro.yaml` / `nova_pro_wired.yaml`
//!   only (HeadsetControl has no wired Nova Pro); the hardware-verified
//!   wireless sibling (`nova_pro.rs`) sends the same two commands.
//!
//! Cross-checked against Linux-Arctis-Manager `daemon/device-configs/*.yaml`
//! (github.com/elegos/Linux-Arctis-Manager); where the two disagree the
//! feature is left out (see `models.rs`). Arctis 7+ mic level (`00 37`, 1-7)
//! and mute LED (`00 ae`, 0-3) come from Linux-Arctis-Manager
//! `base_arctis_7_plus.yaml` only, same bytes as the Nova 7 setters.
//!
//! Settings apply live and are re-sent whenever the headset (re)connects.
//! HeadsetControl follows most setters with a "save state" (`06 09`, `00 09`,
//! `00 35 01`, `90 aa`, ...), Linux-Arctis-Manager sends the same
//! `save_to_flash` on shutdown: it stores the headset's current settings in
//! flash. It is only sent from `set` while the owner-enabled "Save changes to
//! device memory" toggle is on, and only where the sources agree byte for
//! byte for that model ([`Save`], `docs/onboard-writes/steelseries.md`):
//! - Arctis 7 / 1 Wireless: `06 09`, 31-byte output report (HeadsetControl
//!   `protocols/steelseries_protocol.hpp` `saveState`; Linux-Arctis-Manager
//!   `base_arctis_7.yaml` / `base_arctis_1_wireless.yaml` `save_to_flash`).
//! - Arctis 7+: `00 09`, 64 bytes (HeadsetControl `steelseries_arctis_7_plus.hpp`
//!   `saveStateArctis7Plus`; Linux-Arctis-Manager `base_arctis_7_plus.yaml`).
//! - Arctis Nova 7 Gen 2 (`227e`): `00 09`, after which the firmware ignores
//!   commands for 1.5 s (HeadsetControl `steelseries_arctis_nova_7.hpp`;
//!   Linux-Arctis-Manager `base_arctis_nova_7_gen2.yaml`). Its `00 20` audio
//!   settings (`[1]` mic, `[2]` sidetone, `[3]` limiter) and `00 a0` device
//!   settings (`[1]` auto-off) frames are read before the save (an invalid
//!   frame aborts it) and after it (a different value is an error); same
//!   offsets in both sources and the proposed kernel `hid-steelseries` patch
//!   (lore: 20260227235042.410062-1-srimanachanta@gmail.com).
//!
//! Elsewhere the control is shown blocked with the reason.
//!
//! ChatMix: the Arctis 7 / Pro / 9 present separate Game and Chat playback
//! devices and mix them in the headset, so their dial is shown read-only. The
//! Arctis 7+ and Nova 5 / 7 present one playback device and only report the
//! dial position (awth13/Linux-Arctis-7-Plus-ChatMix README,
//! 0xGingi/nova7wireless-chatmix-linux, Arctis Sound Manager's ChatMix notes):
//! for those the dial drives the host-side Hako_Game / Hako_Chat sinks.

use super::{instance, pad, request, VID};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{anyhow, bail, Context, Result};
use hako_audio::AudioService;
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const POLL: Duration = Duration::from_secs(5);
/// Dial poll period when the dial drives the host mix.
const MIX_POLL: Duration = Duration::from_millis(450);
const REPLY_TIMEOUT: Duration = Duration::from_millis(200);

/// Wire protocol of a headset family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Arctis 7 / Pro (2019) / Pro GameDAC.
    Arctis7,
    /// Arctis 1 Wireless / 7X / 7P: Arctis 7 commands plus a status byte.
    Arctis1,
    Arctis9,
    ProWireless,
    /// Arctis 7+: battery in 0-4 steps.
    Arctis7Plus,
    /// Arctis Nova 7 / 7X / 7P.
    Nova7,
    /// Arctis Nova 5 / 5X and Nova 3P / 3X Wireless.
    Nova5,
    /// Arctis Nova 3 (wired, feature reports).
    Nova3,
    /// Arctis GameBuds: battery of each bud.
    GameBuds,
    /// Arctis Nova Pro wired GameDAC (no battery, nothing to poll).
    NovaProWired,
}

/// What the ChatMix dial does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatMix {
    None,
    /// The headset mixes its own Game / Chat outputs; shown read-only.
    Dial,
    /// The dial only reports: it drives the host mix, routed to the first
    /// PipeWire sink matching these regexes (most specific first).
    Host(&'static [&'static str]),
}

/// How the equalizer frame is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqLayout {
    /// `<report> 33 (base + 2 * dB)...`: one byte per band, 0.5 dB steps.
    Graphic { report: u8, feature: bool, base: u8 },
    /// Nova 5: `00 33` + per band `freq(LE) filter (20 + 2 * dB) q(LE)`.
    Nova5,
    /// Nova 3P: `00 33` + per band `freq(LE) 01 (dB * 10 as i8) q(LE)`.
    Nova3P,
}

pub struct EqSpec {
    pub layout: EqLayout,
    pub bands: usize,
    pub max_db: f64,
    pub step: f64,
    /// Band centre frequencies when the source names them.
    pub freqs: &'static [u16],
    /// (name, gain per band in dB).
    pub presets: &'static [(&'static str, &'static [f64])],
}

/// Fixed band frequencies of the Nova 5 / 3P frames.
const NOVA_FREQS: &[u16] = &[32, 64, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
/// Q 1.414 in thousandths, little-endian.
const Q_DEFAULT: [u8; 2] = [0x86, 0x05];

/// Nova 5 / 3P presets (HeadsetControl `PRESET_FLAT/BASS/FOCUS/SMILEY`).
const NOVA_PRESETS: &[(&str, &[f64])] = &[
    ("Flat", &[0.0; 10]),
    (
        "Bass",
        &[3.5, 5.5, 4.0, 1.0, -1.5, -1.5, -1.0, -1.0, -1.0, -1.0],
    ),
    (
        "Focus",
        &[-5.0, -3.5, -1.0, -3.5, -2.5, 4.0, 6.0, -3.5, 0.0, 0.0],
    ),
    (
        "Smiley",
        &[3.0, 3.5, 1.5, -1.5, -4.0, -4.0, -2.5, 1.5, 3.0, 4.0],
    ),
];

pub const EQ_ARCTIS7_PLUS: EqSpec = EqSpec {
    layout: EqLayout::Graphic {
        report: 0x00,
        feature: false,
        base: 0x18,
    },
    bands: 10,
    max_db: 12.0,
    step: 0.5,
    freqs: &[],
    presets: &[
        ("Flat", &[0.0; 10]),
        (
            "Bass Boost",
            &[3.5, 4.0, 1.0, -1.5, -1.5, -1.0, -1.0, -1.0, -1.0, 5.5],
        ),
        (
            "Smiley",
            &[3.0, 1.5, -1.5, -4.0, -4.0, -2.5, 1.5, 3.0, 4.0, 3.5],
        ),
        (
            "Focus",
            &[-5.0, -1.0, -3.5, -2.5, 4.0, 6.0, 3.5, -3.5, 0.0, -3.5],
        ),
    ],
};

pub const EQ_NOVA5: EqSpec = EqSpec {
    layout: EqLayout::Nova5,
    bands: 10,
    max_db: 10.0,
    step: 0.5,
    freqs: NOVA_FREQS,
    presets: NOVA_PRESETS,
};

pub const EQ_NOVA3P: EqSpec = EqSpec {
    layout: EqLayout::Nova3P,
    bands: 10,
    max_db: 12.0,
    step: 0.1,
    freqs: NOVA_FREQS,
    presets: NOVA_PRESETS,
};

pub const EQ_NOVA3: EqSpec = EqSpec {
    layout: EqLayout::Graphic {
        report: 0x06,
        feature: true,
        base: 0x14,
    },
    bands: 6,
    max_db: 6.0,
    step: 0.5,
    freqs: &[],
    presets: &[
        ("Flat", &[0.0; 6]),
        ("Bass", &[4.0, 2.5, -1.5, 0.0, 0.0, 0.0]),
        ("Smiley", &[3.0, 1.5, -2.5, -1.0, 1.5, 3.0]),
        ("Focus", &[-4.0, -3.5, -1.5, 2.0, 4.0, 0.0]),
    ],
};

const EQ_KEYS: [SettingKey; 10] = [
    SettingKey::EqBand1,
    SettingKey::EqBand2,
    SettingKey::EqBand3,
    SettingKey::EqBand4,
    SettingKey::EqBand5,
    SettingKey::EqBand6,
    SettingKey::EqBand7,
    SettingKey::EqBand8,
    SettingKey::EqBand9,
    SettingKey::EqBand10,
];

/// How a model stores its current settings in flash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Save {
    /// No save command known.
    None,
    /// Known, but the sources do not support it well enough: the reason.
    Blocked(&'static str),
    /// `06 09`, 31-byte output report (Arctis 7 / 1 Wireless).
    Legacy,
    /// `00 09`, 64-byte output report (Arctis 7+).
    Nova,
    /// `00 09`, then 1.5 s busy; settings frames read back (Nova 7 Gen 2).
    NovaGen2,
}

impl Save {
    fn packet(self) -> Option<Packet> {
        match self {
            Save::Legacy => Some(Packet::Output(pad(&[0x06, 0x09], 31))),
            Save::Nova | Save::NovaGen2 => Some(Packet::Output(pad(&[0x00, 0x09], 64))),
            Save::None | Save::Blocked(_) => None,
        }
    }
}

/// The Nova 7 Gen 2 firmware ignores commands this long after a save
/// (Linux-Arctis-Manager `base_arctis_nova_7_gen2.yaml`).
const GEN2_SAVE_BUSY: Duration = Duration::from_millis(1500);

pub struct HeadsetSpec {
    /// USB interface of the vendor HID collection.
    pub interface: u32,
    pub family: Family,
    pub battery: bool,
    pub chatmix: ChatMix,
    /// Highest sidetone level (0 = off).
    pub sidetone: Option<u8>,
    /// Highest mic level.
    pub mic_volume: Option<u8>,
    /// Mute LED brightness in four steps (off/low/medium/high).
    pub mic_led: bool,
    /// Auto-off choices in minutes (0 = never); empty = not supported.
    pub auto_off: &'static [u8],
    pub eq: Option<&'static EqSpec>,
    pub volume_limiter: bool,
    /// Game audio during Bluetooth calls: unchanged / -12 dB / muted. Also
    /// lists the (blocked) Bluetooth-on-power-up setting.
    pub bt_call: bool,
    pub save: Save,
}

/// Nova 7 PIDs that report battery in 0-4 steps (HeadsetControl
/// `DISCRETE_BATTERY_PIDS`); later firmware reports a percentage.
const NOVA7_DISCRETE: &[u16] = &[0x2202, 0x2206, 0x220a, 0x223a, 0x227a, 0x22a4];
/// HeadsetControl reads a percentage here, Linux-Arctis-Manager 0-4 steps:
/// the level is not shown.
const NOVA7_BATTERY_UNKNOWN: &[u16] = &[0x2258];

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Packet {
    Output(Vec<u8>),
    Feature(Vec<u8>),
}

/// The packet setting `key` to device level `v`, if the family has one.
fn encode(family: Family, key: SettingKey, v: u8) -> Option<Packet> {
    use Family::*;
    use SettingKey::*;
    let legacy = |cmd: &[u8]| Some(Packet::Output(pad(cmd, 31)));
    let nova = |cmd: u8, v: u8| Some(Packet::Output(pad(&[0x00, cmd, v], 64)));
    match (family, key) {
        (Arctis7 | Arctis1, MicSideTone) if v == 0 => legacy(&[0x06, 0x35]),
        (Arctis7 | Arctis1, MicSideTone) => legacy(&[0x06, 0x35, 0x01, 0x00, v]),
        (Arctis7, PmShutdown) => legacy(&[0x06, 0x51, v]),
        (Arctis1, PmShutdown) => legacy(&[0x06, 0x53, v]),
        // 0xc0 = off .. 0xfd = loudest.
        (Arctis9, MicSideTone) => legacy(&[0x06, 0x00, 0xc0u8.saturating_add(v).min(0xfd)]),
        // Seconds, big-endian.
        (Arctis9, PmShutdown) => {
            let [hi, lo] = (u16::from(v) * 60).to_be_bytes();
            legacy(&[0x04, 0x00, hi, lo])
        }
        (ProWireless, MicSideTone) => legacy(&[0x39, 0xaa, v]),
        // 10-minute steps.
        (ProWireless, PmShutdown) => legacy(&[0x3c, 0xaa, v / 10]),
        (Arctis7Plus | Nova7 | Nova5, MicSideTone) => nova(0x39, v),
        (Arctis7Plus | Nova7 | Nova5, PmShutdown) => nova(0xa3, v),
        (Arctis7Plus | Nova7 | Nova5, MicVolume) => nova(0x37, v),
        (Arctis7Plus | Nova7, MicLedBrightness) => nova(0xae, v),
        (NovaProWired, MicVolume | MicSideTone) => {
            let cmd = if key == MicVolume { 0x37 } else { 0x39 };
            Some(Packet::Output(pad(&[0x06, cmd, v], 64)))
        }
        // Nova 5 steps are off / 0x01 / 0x04 / 0x0a.
        (Nova5, MicLedBrightness) => nova(0xae, [0x00, 0x01, 0x04, 0x0a][usize::from(v.min(3))]),
        (Nova7, VolumeLimiter) => nova(0x3a, v.min(1)),
        (Nova5, VolumeLimiter) => nova(0x27, v.min(1)),
        (Nova7, BluetoothCallVolume) => nova(0xb3, v.min(2)),
        (Nova3, MicSideTone | MicVolume | MicLedBrightness) => {
            let cmd = match key {
                MicSideTone => 0x39,
                MicVolume => 0x37,
                _ => 0xae,
            };
            Some(Packet::Feature(pad(&[0x06, cmd, v], 64)))
        }
        _ => None,
    }
}

/// The equalizer frame for `gains` (dB, one per band, already in range).
fn eq_packet(eq: &EqSpec, gains: &[f64]) -> Packet {
    let half_db = |base: u8, db: f64| (f64::from(base) + 2.0 * db).round().clamp(0.0, 255.0) as u8;
    match eq.layout {
        EqLayout::Graphic {
            report,
            feature,
            base,
        } => {
            let mut b = vec![report, 0x33];
            b.extend(gains.iter().map(|&g| half_db(base, g)));
            let b = pad(&b, 64);
            if feature {
                Packet::Feature(b)
            } else {
                Packet::Output(b)
            }
        }
        EqLayout::Nova5 | EqLayout::Nova3P => {
            let mut b = vec![0x00, 0x33];
            for (i, (&g, freq)) in gains.iter().zip(NOVA_FREQS).enumerate() {
                let (filter, gain) = if eq.layout == EqLayout::Nova5 {
                    let raw = half_db(20, g);
                    // Low shelf on the first band, high shelf on the last,
                    // peaking otherwise (and for flat bands).
                    let filter = match i {
                        _ if raw == 20 => 0x01,
                        0 => 0x04,
                        9 => 0x05,
                        _ => 0x01,
                    };
                    (filter, raw)
                } else {
                    (0x01, (g * 10.0).round() as i8 as u8)
                };
                b.extend_from_slice(&freq.to_le_bytes());
                b.extend_from_slice(&[filter, gain]);
                b.extend_from_slice(&Q_DEFAULT);
            }
            Packet::Output(pad(&b, 64))
        }
    }
}

/// Stored band gains, 0 dB where unset.
fn eq_gains(eq: &EqSpec, s: &SettingsMap) -> Vec<f64> {
    EQ_KEYS[..eq.bands]
        .iter()
        .map(|&k| {
            get_setting(s, k)
                .and_then(|v| v.as_float())
                .unwrap_or(0.0)
                .clamp(-eq.max_db, eq.max_db)
        })
        .collect()
}

#[derive(Debug, Default, PartialEq)]
struct Status {
    online: bool,
    percent: Option<u8>,
    charging: bool,
    /// (game, chat) dial levels, 0.0-1.0.
    chatmix: Option<(f64, f64)>,
    /// GameBuds: (left, right) battery of each bud out of the case.
    buds: Option<[Option<u8>; 2]>,
}

/// HeadsetControl's integer `map(x, in_min, in_max, 0, 100)`, clamped.
fn percent(x: u8, min: u8, max: u8) -> u8 {
    let x = x.clamp(min, max);
    (u16::from(x - min) * 100 / u16::from(max - min)) as u8
}

fn mix(game: u8, chat: u8, max: u8) -> Option<(f64, f64)> {
    let level = |v: u8| f64::from(v.min(max)) / f64::from(max);
    Some((level(game), level(chat)))
}

/// First status request of a family (`None`: nothing to poll).
fn status_request(family: Family) -> Option<Vec<u8>> {
    match family {
        Family::Arctis7 => Some(vec![0x06, 0x18]),
        Family::Arctis1 => Some(vec![0x06, 0x12]),
        Family::Arctis9 => Some(vec![0x00, 0x20]),
        Family::ProWireless => Some(pad(&[0x41, 0xaa], 31)),
        Family::Arctis7Plus | Family::Nova7 | Family::Nova5 | Family::GameBuds => {
            Some(vec![0x00, 0xb0])
        }
        Family::Nova3 | Family::NovaProWired => None,
    }
}

/// Parse the reply to [`status_request`]; `None` = not that reply.
fn parse_status(family: Family, discrete: bool, r: &[u8]) -> Option<Status> {
    let st = match family {
        // [2] battery percent; no status byte.
        Family::Arctis7 if r.len() >= 3 => Status {
            online: true,
            percent: Some(r[2].min(100)),
            ..Status::default()
        },
        // [2] 0x01 = offline, [3] battery percent.
        Family::Arctis1 if r.len() >= 4 => Status {
            online: r[2] != 0x01,
            percent: Some(r[3].min(100)),
            ..Status::default()
        },
        // [3] battery 0x64-0x9a, [4] 0x01 charging / 0xff offline, [9]/[10] dial 0-19.
        Family::Arctis9 if r.len() >= 11 => Status {
            online: r[4] != 0xff,
            percent: Some(percent(r[3], 0x64, 0x9a)),
            charging: r[4] == 0x01,
            chatmix: mix(r[9], r[10], 19),
            buds: None,
        },
        // [0] 0x02 = offline, 0x04 = online (battery is a second request).
        Family::ProWireless if !r.is_empty() => Status {
            online: r[0] != 0x02,
            ..Status::default()
        },
        // [1] 0x01 offline, [2] battery 0-4, [3] 0x01 charging, [4]/[5] dial 0-100.
        Family::Arctis7Plus if r.len() >= 6 && r[0] == 0xb0 => Status {
            online: r[1] != 0x01,
            percent: Some(percent(r[2], 0, 4)),
            charging: r[3] == 0x01,
            chatmix: mix(r[4], r[5], 100),
            buds: None,
        },
        // [3] 0x00 offline / 0x01-0x02 charging, [2] battery, [4]/[5] dial 0-100.
        Family::Nova7 if r.len() >= 6 && r[0] == 0xb0 => Status {
            online: r[3] != 0x00,
            percent: Some(if discrete {
                percent(r[2], 0, 4)
            } else {
                r[2].min(100)
            }),
            charging: matches!(r[3], 0x01 | 0x02),
            chatmix: mix(r[4], r[5], 100),
            buds: None,
        },
        // [1] 0x02 offline, [3] battery percent, [4] 0x01 charging, [5]/[6] dial 0-100.
        Family::Nova5 if r.len() >= 7 && r[0] == 0xb0 => Status {
            online: r[1] != 0x02,
            percent: Some(r[3].min(100)),
            charging: r[4] == 0x01,
            chatmix: mix(r[5], r[6], 100),
            buds: None,
        },
        // [3]/[4] bud status (0x03 = out of the case), [5]/[6] bud battery %.
        Family::GameBuds if r.len() >= 7 && r[0] == 0xb0 => {
            let bud = |status: u8, level: u8| (status == 0x03).then_some(level.min(100));
            let buds = [bud(r[3], r[5]), bud(r[4], r[6])];
            Status {
                online: buds.iter().any(Option::is_some),
                percent: buds.iter().flatten().min().copied(),
                buds: Some(buds),
                ..Status::default()
            }
        }
        _ => return None,
    };
    Some(Status {
        percent: st.percent.filter(|_| st.online),
        ..st
    })
}

/// Arctis 7 ChatMix reply to `06 24`: \[2\] game, \[3\] chat, 191 (muted) to
/// 255 (full); 0 also means full.
fn parse_arctis7_chatmix(r: &[u8]) -> Option<(f64, f64)> {
    let level = |v: u8| match v {
        0 => 1.0,
        v => f64::from(percent(v, 191, 255)) / 100.0,
    };
    (r.len() >= 4).then(|| (level(r[2]), level(r[3])))
}

/// What the save sequence needs from the device (a fake in tests).
pub(super) trait Link {
    fn send(&mut self, packet: &Packet) -> Result<()>;
    /// Write `req`; the first reply starting with `id`, if any.
    fn ask(&mut self, req: &[u8], id: u8) -> Result<Option<Vec<u8>>>;
}

impl Link for HidrawDevice {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        match packet {
            Packet::Output(b) => self.write(b),
            Packet::Feature(b) => self.set_feature(&mut b.clone()),
        }
    }

    fn ask(&mut self, req: &[u8], id: u8) -> Result<Option<Vec<u8>>> {
        request(self, req, |r| r.first() == Some(&id), REPLY_TIMEOUT)
    }
}

/// Nova 7 Gen 2 read-back of `key`: (request, reply id, offset, highest value).
fn gen2_readback(key: SettingKey) -> Option<([u8; 2], u8, usize, u8)> {
    match key {
        SettingKey::MicVolume => Some(([0x00, 0x20], 0x20, 1, 7)),
        SettingKey::MicSideTone => Some(([0x00, 0x20], 0x20, 2, 3)),
        SettingKey::VolumeLimiter => Some(([0x00, 0x20], 0x20, 3, 1)),
        SettingKey::PmShutdown => Some(([0x00, 0xa0], 0xa0, 1, 0xff)),
        _ => None,
    }
}

/// Read `key` back from a Nova 7 Gen 2 settings frame; a missing, short or
/// out-of-range frame is an error.
fn read_gen2(link: &mut dyn Link, key: SettingKey) -> Result<Option<u8>> {
    let Some((req, id, at, max)) = gen2_readback(key) else {
        return Ok(None);
    };
    let r = link
        .ask(&pad(&req, 64), id)?
        .with_context(|| format!("no {id:02x} settings reply"))?;
    match r.get(at) {
        Some(&v) if r[0] == id && v <= max => Ok(Some(v)),
        _ => bail!(
            "invalid {id:02x} settings reply {:02x?}",
            &r[..r.len().min(8)]
        ),
    }
}

/// Store the headset's current settings in flash. `check` is the setting
/// just written and its device value: on the Nova 7 Gen 2 its settings frame
/// is read and validated first (abort on a bad frame) and compared after the
/// save. `busy` is how long the firmware ignores commands after a save.
fn store(
    link: &mut dyn Link,
    save: Save,
    check: Option<(SettingKey, u8)>,
    busy: Duration,
) -> Result<()> {
    let Some(packet) = save.packet() else {
        bail!("this headset has no save command");
    };
    let check = check.filter(|_| save == Save::NovaGen2);
    std::thread::sleep(Duration::from_millis(10));
    if let Some((key, _)) = check {
        read_gen2(link, key)?;
    }
    link.send(&packet)?;
    if save != Save::NovaGen2 {
        return Ok(());
    }
    std::thread::sleep(busy);
    if let Some((key, want)) = check {
        if let Some(got) = read_gen2(link, key)? {
            if got != want {
                bail!("{} reads back {got} after saving, not {want}", key.as_str());
            }
        }
    }
    Ok(())
}

fn sidetone_options() -> Vec<SettingOption> {
    vec![
        SettingOption::int(0, "Off"),
        SettingOption::int(1, "Low"),
        SettingOption::int(2, "Medium"),
        SettingOption::int(3, "High"),
    ]
}

/// The "Custom" choice after the presets: the stored band gains.
fn custom_preset(eq: &EqSpec) -> i64 {
    eq.presets.len() as i64
}

pub(super) fn controls(spec: &HeadsetSpec) -> Vec<SettingMeta> {
    use SettingKey::*;
    let mut c = Vec::new();
    if spec.chatmix != ChatMix::None {
        c.push(SettingMeta::readonly(ChatMixGame, "Game", 0.0, 1.0));
        c.push(SettingMeta::readonly(ChatMixChat, "Chat", 0.0, 1.0));
    }
    if let Some(eq) = spec.eq {
        let mut options: Vec<_> = eq
            .presets
            .iter()
            .enumerate()
            .map(|(i, (name, _))| SettingOption::int(i as i64, name))
            .collect();
        options.push(SettingOption::int(custom_preset(eq), "Custom"));
        c.push(SettingMeta::dropdown(EqPreset, "EQ preset", options));
        for (i, &key) in EQ_KEYS[..eq.bands].iter().enumerate() {
            let label = match eq.freqs.get(i) {
                Some(&f) if f >= 1000 => format!("{} kHz", f / 1000),
                Some(f) => format!("{f} Hz"),
                None => format!("Band {}", i + 1),
            };
            c.push(SettingMeta::slider(key, &label, -eq.max_db, eq.max_db, eq.step).unit("dB"));
        }
    }
    if spec.volume_limiter {
        c.push(SettingMeta::toggle(VolumeLimiter, "Volume limiter"));
    }
    if spec.bt_call {
        c.push(SettingMeta::dropdown(
            BluetoothCallVolume,
            "Game audio in Bluetooth calls",
            vec![
                SettingOption::int(0, "Unchanged"),
                SettingOption::int(1, "Lowered 12 dB"),
                SettingOption::int(2, "Muted"),
            ],
        ));
    }
    if !spec.auto_off.is_empty() {
        let options = spec
            .auto_off
            .iter()
            .map(|&m| match m {
                0 => SettingOption::int(0, "Never"),
                m => SettingOption::int(m.into(), &format!("After {m} min")),
            })
            .collect();
        c.push(SettingMeta::dropdown(PmShutdown, "Auto-off", options));
    }
    if let Some(max) = spec.mic_volume {
        // Linux-Arctis-Manager starts these at 1.
        let min = matches!(spec.family, Family::Arctis7Plus | Family::NovaProWired);
        c.push(SettingMeta::slider(
            MicVolume,
            "Mic level",
            f64::from(u8::from(min)),
            max.into(),
            1.0,
        ));
    }
    match spec.sidetone {
        Some(3) => c.push(SettingMeta::dropdown(
            MicSideTone,
            "Sidetone",
            sidetone_options(),
        )),
        Some(max) => c.push(SettingMeta::slider(
            MicSideTone,
            "Sidetone",
            0.0,
            max.into(),
            1.0,
        )),
        None => {}
    }
    if spec.mic_led {
        c.push(SettingMeta::dropdown(
            MicLedBrightness,
            "Mute LED",
            sidetone_options(),
        ));
    }
    if spec.bt_call {
        // HeadsetControl follows `00 b2` with `06 09`, Linux-Arctis-Manager
        // and the kernel patch with `00 09`.
        c.push(
            SettingMeta::toggle(BluetoothOnPower, "Bluetooth on at power-up")
                .persistent()
                .blocked("sources disagree on the save command (06 09 vs 00 09)"),
        );
    }
    let save = SettingMeta::toggle(SaveToDevice, "Save changes to device memory").persistent();
    match spec.save {
        Save::None => {}
        Save::Blocked(reason) => c.push(save.blocked(reason)),
        _ => c.push(save),
    }
    c
}

/// Packets that put `key` from `s` on the device. EQ keys may rewrite `s`:
/// a preset fills in its band gains, a band switches the preset to "Custom".
pub(super) fn packets(
    spec: &HeadsetSpec,
    s: &mut SettingsMap,
    key: SettingKey,
) -> Result<Vec<Packet>> {
    let Some(value) = get_setting(s, key).cloned() else {
        bail!("{} has no value", key.as_str());
    };
    if let Some(eq) = spec.eq {
        let band = EQ_KEYS[..eq.bands].contains(&key);
        if key == SettingKey::EqPreset || band {
            if band {
                merge_setting(
                    s,
                    SettingKey::EqPreset,
                    SettingValue::Int(custom_preset(eq)),
                );
            } else if let Some((_, gains)) = value
                .as_int()
                .and_then(|p| eq.presets.get(usize::try_from(p).ok()?))
            {
                for (&k, &g) in EQ_KEYS.iter().zip(gains.iter()) {
                    merge_setting(s, k, SettingValue::Float(g));
                }
            }
            return Ok(vec![eq_packet(eq, &eq_gains(eq, s))]);
        }
    }
    let v = match value {
        SettingValue::Bool(b) => u8::from(b),
        v => match v.as_int() {
            Some(n) => n.clamp(0, 255) as u8,
            None => bail!("{} expects a number", key.as_str()),
        },
    };
    match encode(spec.family, key, v) {
        Some(p) => Ok(vec![p]),
        None => bail!("unsupported setting {}", key.as_str()),
    }
}

/// The packets that re-send every stored setting, in control order (one EQ
/// frame: the stored gains, else the stored preset's), and why each stored
/// setting that cannot be encoded was left out.
fn stored_packets(spec: &HeadsetSpec, settings: &SettingsMap) -> (Vec<Vec<Packet>>, Vec<String>) {
    let (mut all, mut skipped) = (Vec::new(), Vec::new());
    let mut eq_sent = false;
    for meta in controls(spec) {
        if meta.read_only || meta.persistent || get_setting(settings, meta.key).is_none() {
            continue;
        }
        let eq = meta.key == SettingKey::EqPreset || EQ_KEYS.contains(&meta.key);
        if eq && std::mem::replace(&mut eq_sent, true) {
            continue;
        }
        let key = if eq {
            EQ_KEYS
                .into_iter()
                .find(|&k| get_setting(settings, k).is_some())
                .unwrap_or(SettingKey::EqPreset)
        } else {
            meta.key
        };
        match packets(spec, &mut settings.clone(), key) {
            Ok(p) => all.push(p),
            Err(e) => {
                warn!("stored {} not re-applied: {e:#}", key.as_str());
                skipped.push(format!("{}: {e:#}", key.as_str()));
            }
        }
    }
    (all, skipped)
}

/// `apply_stored`'s skipped settings as an error, if any.
fn not_reapplied(skipped: Vec<String>) -> Result<()> {
    if skipped.is_empty() {
        return Ok(());
    }
    bail!("not re-applied: {}", skipped.join("; "))
}

pub(super) fn open(
    pid: u16,
    name: &'static str,
    spec: &'static HeadsetSpec,
    ctx: &DriverCtx,
) -> Opened {
    Ok(vec![Box::new(HeadsetAdapter::try_open(
        pid,
        name,
        spec,
        ctx.paths.clone(),
        ctx.audio.clone(),
    )?)])
}

pub struct HeadsetAdapter {
    id: DeviceId,
    name: &'static str,
    spec: &'static HeadsetSpec,
    discrete: bool,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    batteries: Vec<BatteryState>,
    online: bool,
    last_error: Option<String>,
    last_poll: Instant,
    last_chatmix_try: Instant,
    failures: u8,
    paths: ConfigPaths,
    audio: Arc<AudioService>,
}

fn battery(label: &str) -> BatteryState {
    BatteryState {
        label: label.into(),
        percent: None,
        charging: false,
        present: false,
    }
}

impl HeadsetAdapter {
    fn try_open(
        pid: u16,
        name: &'static str,
        spec: &'static HeadsetSpec,
        paths: ConfigPaths,
        audio: Arc<AudioService>,
    ) -> Result<Self> {
        let info = find_hidraw(VID, pid, Some(spec.interface))?;
        let dev = HidrawDevice::open(&info.path)?;
        let mut settings = hako_core::config::load_device_settings(&paths, VID, pid)?;
        crate::drivers::sanitize_saved(&mut settings, &controls(spec));
        let batteries = match spec.family {
            Family::GameBuds => vec![battery("Left"), battery("Right")],
            _ if spec.battery => vec![battery("Headset")],
            _ => vec![],
        };
        let mut a = Self {
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: instance(&info),
            },
            name,
            spec,
            discrete: NOVA7_DISCRETE.contains(&pid),
            dev: Some(dev),
            settings,
            batteries,
            online: false,
            last_error: None,
            last_poll: Instant::now(),
            last_chatmix_try: Instant::now(),
            failures: 0,
            paths,
            audio,
        };
        if let ChatMix::Host(sinks) = spec.chatmix {
            // A (re)connected headset ends a kill-switch reset.
            a.audio.resume_chatmix();
            if let Err(e) = a.audio.ensure_chatmix(sinks) {
                warn!("{name} chatmix: {e}");
            }
        }
        if let Err(e) = a.poll() {
            warn!("{name} status: {e}");
            a.last_error = Some(e.to_string());
        }
        info!(
            "SteelSeries {name} open {} online={}",
            info.path.display(),
            a.online
        );
        Ok(a)
    }

    fn send(&mut self, packets: Vec<Packet>) -> Result<()> {
        let dev = self.dev.as_mut().context("no device")?;
        for packet in packets {
            match packet {
                Packet::Output(b) => dev.write(&b)?,
                Packet::Feature(mut b) => dev.set_feature(&mut b)?,
            }
        }
        Ok(())
    }

    /// Re-send every stored setting (they are volatile on the headset).
    /// Never saves: memory writes only happen for a user change in `set`.
    /// A failed send aborts (the caller retries it all); a setting that
    /// cannot be encoded is skipped and returned.
    fn apply_stored(&mut self) -> Result<Vec<String>> {
        let (all, skipped) = stored_packets(self.spec, &self.settings);
        for p in all {
            self.send(p)?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(skipped)
    }

    fn poll(&mut self) -> Result<()> {
        self.last_poll = Instant::now();
        let family = self.spec.family;
        let Some(req) = status_request(family) else {
            // Wired, nothing to poll: always reachable.
            if !self.online {
                let skipped = self.apply_stored()?;
                self.online = true;
                return not_reapplied(skipped);
            }
            return Ok(());
        };
        let discrete = self.discrete;
        let dev = self.dev.as_mut().context("no device")?;
        let mut st = request(
            dev,
            &req,
            |r| parse_status(family, discrete, r).is_some(),
            REPLY_TIMEOUT,
        )?
        .and_then(|r| parse_status(family, discrete, &r))
        .unwrap_or_default();
        if family == Family::Arctis7 && self.spec.chatmix != ChatMix::None {
            st.chatmix = request(dev, &[0x06, 0x24], |r| r.len() >= 4, REPLY_TIMEOUT)?
                .and_then(|r| parse_arctis7_chatmix(&r));
        }
        if family == Family::ProWireless && st.online {
            // [0] battery 0-4.
            st.percent = request(
                dev,
                &pad(&[0x40, 0xaa], 31),
                |r| !r.is_empty(),
                REPLY_TIMEOUT,
            )?
            .map(|r| percent(r[0], 0, 4));
        }
        if NOVA7_BATTERY_UNKNOWN.contains(&self.id.product_id) {
            st.percent = None;
        }
        if let Some((game, chat)) = st.chatmix.filter(|_| self.spec.chatmix != ChatMix::None) {
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
            if matches!(self.spec.chatmix, ChatMix::Host(_)) {
                if let Err(e) = self.audio.set_chatmix_volumes(game as f32, chat as f32) {
                    warn!("chatmix volumes: {e}");
                }
            }
        }
        match (st.buds, self.batteries.as_mut_slice()) {
            (Some(buds), [left, right]) => {
                for (b, level) in [left, right].into_iter().zip(buds) {
                    b.percent = level;
                    b.present = level.is_some();
                }
            }
            (_, [b]) => {
                b.percent = st.percent;
                b.charging = st.charging;
                b.present = st.online;
            }
            _ => {}
        }
        // Online only once the stored settings went out: a failed re-apply is
        // tried again on the next poll.
        let skipped = if st.online && !self.online {
            self.apply_stored()?
        } else {
            Vec::new()
        };
        self.online = st.online;
        not_reapplied(skipped)
    }

    fn persist(&self, settings: &SettingsMap) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, VID, self.id.product_id, settings)
    }

    /// "Save changes to device memory" is on (and this model can save).
    fn saving(&self) -> bool {
        self.spec.save.packet().is_some()
            && get_setting(&self.settings, SettingKey::SaveToDevice)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }

    fn store(&mut self, check: Option<(SettingKey, u8)>) -> Result<()> {
        let dev = self.dev.as_mut().context("no device")?;
        store(dev, self.spec.save, check, GEN2_SAVE_BUSY)
    }

    fn host_mix(&self) -> Option<&'static [&'static str]> {
        match self.spec.chatmix {
            ChatMix::Host(sinks) => Some(sinks),
            _ => None,
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
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: self.batteries.clone(),
            settings: self.settings.clone(),
            controls: controls(self.spec),
            last_error: self.last_error.clone().or_else(|| {
                self.host_mix()
                    .and_then(|_| self.audio.last_error())
                    .filter(|_| !self.audio.chatmix_active())
            }),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let mut proposed = self.settings.clone();
        merge_setting(&mut proposed, key, value.clone());
        if key == SettingKey::SaveToDevice {
            // Switching on stores the current state once; off writes nothing.
            if value.as_bool() == Some(true) {
                self.store(None)?;
            }
            self.settings = proposed;
            return self
                .persist(&self.settings)
                .map_err(|e| anyhow!("applied, but not saved: {e:#}"));
        }
        let p = packets(self.spec, &mut proposed, key)?;
        self.send(p)?;
        // The headset has the value: keep it even if the file cannot be written.
        self.settings = proposed;
        let file = self
            .persist(&self.settings)
            .map_err(|e| anyhow!("applied, but not saved: {e:#}"));
        let stored = if self.saving() {
            let device_value = match value {
                SettingValue::Bool(b) => Some(u8::from(b)),
                v => v.as_int().and_then(|n| u8::try_from(n).ok()),
            };
            self.store(device_value.map(|v| (key, v)))
        } else {
            Ok(())
        };
        file.and(stored)
    }

    fn tick(&mut self) -> Result<()> {
        if let Some(sinks) = self.host_mix() {
            if self.last_chatmix_try.elapsed() >= POLL && !self.audio.chatmix_active() {
                self.last_chatmix_try = Instant::now();
                let _ = self.audio.ensure_chatmix(sinks);
            }
        }
        let period = if self.host_mix().is_some() {
            MIX_POLL
        } else {
            POLL
        };
        if self.dev.is_none() || self.last_poll.elapsed() < period {
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

    fn tick_interval(&self) -> Duration {
        if self.host_mix().is_some() {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(1)
        }
    }

    fn shutdown(&mut self) {
        // ChatMix sinks are torn down by the daemon-level audio reset on stop.
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SettingKey::*;

    fn bytes(p: Option<Packet>) -> (bool, Vec<u8>) {
        match p.expect("packet") {
            Packet::Output(b) => (false, b),
            Packet::Feature(b) => (true, b),
        }
    }

    const NOVA7_SPEC: HeadsetSpec = HeadsetSpec {
        interface: 3,
        family: Family::Nova7,
        battery: true,
        chatmix: ChatMix::Host(&["x"]),
        sidetone: Some(3),
        mic_volume: Some(7),
        mic_led: true,
        auto_off: &[0, 15],
        eq: None,
        volume_limiter: true,
        bt_call: true,
        save: Save::NovaGen2,
    };

    /// A stored value that cannot be encoded is skipped and reported; the
    /// other stored settings are still re-sent.
    #[test]
    fn reapply_skips_a_bad_stored_value() {
        let mut s = SettingsMap::new();
        s.insert("mic_volume".into(), SettingValue::Text("loud".into()));
        s.insert("mic_side_tone".into(), SettingValue::Int(2));
        s.insert("volume_limiter".into(), SettingValue::Bool(true));
        let (all, skipped) = stored_packets(&NOVA7_SPEC, &s);
        assert_eq!(all.len(), 2);
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].starts_with("mic_volume"), "{skipped:?}");
        assert!(not_reapplied(skipped).is_err());
        assert!(not_reapplied(Vec::new()).is_ok());
    }

    #[test]
    fn legacy_packets_match_headsetcontrol() {
        let (feature, b) = bytes(encode(Family::Arctis7, MicSideTone, 0x12));
        assert!(!feature);
        assert_eq!(b.len(), 31);
        assert_eq!(&b[..6], &[0x06, 0x35, 0x01, 0x00, 0x12, 0x00]);
        let (_, b) = bytes(encode(Family::Arctis1, MicSideTone, 0));
        assert_eq!(&b[..3], &[0x06, 0x35, 0x00]);
        assert_eq!(
            &bytes(encode(Family::Arctis7, PmShutdown, 30)).1[..3],
            &[0x06, 0x51, 30]
        );
        assert_eq!(
            &bytes(encode(Family::Arctis1, PmShutdown, 90)).1[..3],
            &[0x06, 0x53, 90]
        );
        // Arctis 9: sidetone 0xc0-0xfd, auto-off in seconds big-endian.
        assert_eq!(
            &bytes(encode(Family::Arctis9, MicSideTone, 0)).1[..3],
            &[0x06, 0x00, 0xc0]
        );
        assert_eq!(
            &bytes(encode(Family::Arctis9, MicSideTone, 61)).1[..3],
            &[0x06, 0x00, 0xfd]
        );
        assert_eq!(
            &bytes(encode(Family::Arctis9, PmShutdown, 10)).1[..4],
            &[0x04, 0x00, 0x02, 0x58]
        );
        assert_eq!(
            &bytes(encode(Family::ProWireless, MicSideTone, 9)).1[..3],
            &[0x39, 0xaa, 9]
        );
        assert_eq!(
            &bytes(encode(Family::ProWireless, PmShutdown, 30)).1[..3],
            &[0x3c, 0xaa, 3]
        );
        assert!(encode(Family::Arctis7, MicVolume, 1).is_none());
    }

    #[test]
    fn nova_packets_match_headsetcontrol() {
        let (feature, b) = bytes(encode(Family::Nova7, MicSideTone, 2));
        assert!(!feature);
        assert_eq!(b.len(), 64);
        assert_eq!(&b[..4], &[0x00, 0x39, 0x02, 0x00]);
        assert_eq!(
            &bytes(encode(Family::Nova7, MicVolume, 7)).1[..3],
            &[0x00, 0x37, 7]
        );
        assert_eq!(
            &bytes(encode(Family::Nova7, MicLedBrightness, 3)).1[..3],
            &[0x00, 0xae, 3]
        );
        assert_eq!(
            &bytes(encode(Family::Arctis7Plus, PmShutdown, 15)).1[..3],
            &[0x00, 0xa3, 15]
        );
        assert_eq!(
            &bytes(encode(Family::Nova5, MicLedBrightness, 2)).1[..3],
            &[0x00, 0xae, 0x04]
        );
        assert_eq!(
            &bytes(encode(Family::Nova5, MicLedBrightness, 3)).1[..3],
            &[0x00, 0xae, 0x0a]
        );
        let (feature, b) = bytes(encode(Family::Nova3, MicVolume, 10));
        assert!(feature);
        assert_eq!(b.len(), 64);
        assert_eq!(&b[..3], &[0x06, 0x37, 10]);
        // Arctis 7+ (Linux-Arctis-Manager): 00 37 mic, 00 ae mute LED.
        assert_eq!(
            &bytes(encode(Family::Arctis7Plus, MicVolume, 7)).1[..3],
            &[0x00, 0x37, 7]
        );
        assert_eq!(
            &bytes(encode(Family::Arctis7Plus, MicLedBrightness, 2)).1[..3],
            &[0x00, 0xae, 2]
        );
        assert!(encode(Family::Arctis7Plus, VolumeLimiter, 1).is_none());
        // Wired Nova Pro: 06 37 / 06 39, 64-byte output reports.
        let (feature, b) = bytes(encode(Family::NovaProWired, MicVolume, 10));
        assert!(!feature);
        assert_eq!((b.len(), &b[..4]), (64, &[0x06, 0x37, 10, 0x00][..]));
        assert_eq!(
            &bytes(encode(Family::NovaProWired, MicSideTone, 3)).1[..3],
            &[0x06, 0x39, 3]
        );
        assert!(encode(Family::NovaProWired, PmShutdown, 5).is_none());
        // Volume limiter: 0x3a on the Nova 7, 0x27 on the Nova 5.
        assert_eq!(
            &bytes(encode(Family::Nova7, VolumeLimiter, 1)).1[..3],
            &[0x00, 0x3a, 0x01]
        );
        assert_eq!(
            &bytes(encode(Family::Nova5, VolumeLimiter, 0)).1[..3],
            &[0x00, 0x27, 0x00]
        );
        assert_eq!(
            &bytes(encode(Family::Nova7, BluetoothCallVolume, 2)).1[..3],
            &[0x00, 0xb3, 0x02]
        );
        assert!(encode(Family::Nova5, BluetoothCallVolume, 1).is_none());
    }

    #[test]
    fn eq_frames_match_headsetcontrol() {
        // Arctis 7+ presets: HeadsetControl sends these bytes verbatim.
        let raw: [&[u8]; 4] = [
            &[0x18; 10],
            &[0x1f, 0x20, 0x1a, 0x15, 0x15, 0x16, 0x16, 0x16, 0x16, 0x23],
            &[0x1e, 0x1b, 0x15, 0x10, 0x10, 0x13, 0x1b, 0x1e, 0x20, 0x1f],
            &[0x0e, 0x16, 0x11, 0x13, 0x20, 0x24, 0x1f, 0x11, 0x18, 0x11],
        ];
        for ((_, gains), raw) in EQ_ARCTIS7_PLUS.presets.iter().zip(raw) {
            let (feature, b) = bytes(Some(eq_packet(&EQ_ARCTIS7_PLUS, gains)));
            assert!(!feature);
            assert_eq!((b.len(), &b[..2]), (64, &[0x00, 0x33][..]));
            assert_eq!(&b[2..12], raw);
            assert_eq!(b[12], 0);
        }
        // Nova 3: feature report 06 33, base 0x14.
        let raw: [&[u8]; 4] = [
            &[0x14; 6],
            &[0x1c, 0x19, 0x11, 0x14, 0x14, 0x14],
            &[0x1a, 0x17, 0x0f, 0x12, 0x17, 0x1a],
            &[0x0c, 0x0d, 0x11, 0x18, 0x1c, 0x14],
        ];
        for ((_, gains), raw) in EQ_NOVA3.presets.iter().zip(raw) {
            let (feature, b) = bytes(Some(eq_packet(&EQ_NOVA3, gains)));
            assert!(feature);
            assert_eq!(&b[..2], &[0x06, 0x33]);
            assert_eq!(&b[2..8], raw);
        }
        // Nova 5 bass: 6-byte bands, filter 04 / 05 on non-flat edge bands.
        let (_, b) = bytes(Some(eq_packet(&EQ_NOVA5, NOVA_PRESETS[1].1)));
        assert_eq!(&b[..8], &[0x00, 0x33, 0x20, 0x00, 0x04, 27, 0x86, 0x05]);
        assert_eq!(&b[8..14], &[0x40, 0x00, 0x01, 31, 0x86, 0x05]);
        assert_eq!(&b[56..62], &[0x80, 0x3e, 0x05, 18, 0x86, 0x05]);
        let (_, b) = bytes(Some(eq_packet(&EQ_NOVA5, &[0.0; 10])));
        assert_eq!(&b[2..8], &[0x20, 0x00, 0x01, 20, 0x86, 0x05]);
        assert_eq!(b[58], 0x01);
        // Nova 3P: tenths of a dB, two's complement.
        let (_, b) = bytes(Some(eq_packet(&EQ_NOVA3P, NOVA_PRESETS[1].1)));
        assert_eq!(&b[2..8], &[0x20, 0x00, 0x01, 35, 0x86, 0x05]);
        assert_eq!(b[2 + 6 * 4 + 3], 0xf1); // -1.5 dB
        let (_, b) = bytes(Some(eq_packet(&EQ_NOVA3P, &[-12.0; 10])));
        assert_eq!(b[5], 0x88);
    }

    #[test]
    fn eq_presets_fill_bands_and_bands_switch_to_custom() {
        let spec = HeadsetSpec {
            family: Family::Nova5,
            eq: Some(&EQ_NOVA5),
            ..NOVA7_SPEC
        };
        let mut s = SettingsMap::new();
        merge_setting(&mut s, EqPreset, SettingValue::Int(2));
        let p = packets(&spec, &mut s, EqPreset).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(
            get_setting(&s, EqBand7).and_then(|v| v.as_float()),
            Some(6.0)
        );
        merge_setting(&mut s, EqBand1, SettingValue::Float(1.5));
        let Packet::Output(b) = &packets(&spec, &mut s, EqBand1).unwrap()[0] else {
            panic!("output report");
        };
        assert_eq!(b[5], 23);
        assert_eq!(get_setting(&s, EqPreset), Some(&SettingValue::Int(4)));
        // Toggles go out as 0/1.
        merge_setting(&mut s, VolumeLimiter, SettingValue::Bool(true));
        assert_eq!(
            packets(&spec, &mut s, VolumeLimiter).unwrap(),
            vec![Packet::Output(pad(&[0x00, 0x27, 0x01], 64))]
        );
    }

    #[test]
    fn status_replies_decode() {
        // Nova 7, discrete battery 3/4, discharging, dial 100/40.
        let mut r = [0u8; 8];
        r[..6].copy_from_slice(&[0xb0, 0x00, 0x03, 0x03, 100, 40]);
        let st = parse_status(Family::Nova7, true, &r).unwrap();
        assert_eq!(
            st,
            Status {
                online: true,
                percent: Some(75),
                charging: false,
                chatmix: Some((1.0, 0.4)),
                buds: None,
            }
        );
        r[2] = 64;
        assert_eq!(
            parse_status(Family::Nova7, false, &r).unwrap().percent,
            Some(64)
        );
        r[3] = 0x00; // offline
        let st = parse_status(Family::Nova7, false, &r).unwrap();
        assert!(!st.online && st.percent.is_none());
        r[0] = 0x07; // not a status reply
        assert!(parse_status(Family::Nova7, false, &r).is_none());

        // Nova 5: [1] 0x02 offline, [3] percent, [4] charging.
        let r = [0xb0, 0x00, 0x00, 80, 0x01, 50, 100];
        let st = parse_status(Family::Nova5, false, &r).unwrap();
        assert_eq!(
            (st.percent, st.charging, st.chatmix),
            (Some(80), true, Some((0.5, 1.0)))
        );
        // Arctis 7+: battery 0-4.
        let r = [0xb0, 0x00, 0x02, 0x01, 0, 0];
        assert_eq!(
            parse_status(Family::Arctis7Plus, false, &r)
                .unwrap()
                .percent,
            Some(50)
        );
        // Arctis 1: [2] 0x01 offline.
        assert!(
            !parse_status(Family::Arctis1, false, &[0x06, 0x12, 0x01, 50])
                .unwrap()
                .online
        );
        // Arctis 9: 0x64-0x9a battery, 0-19 dial.
        let mut r = [0u8; 12];
        r[3] = 0x9a;
        r[4] = 0x01;
        r[9] = 19;
        let st = parse_status(Family::Arctis9, false, &r).unwrap();
        assert_eq!((st.percent, st.charging), (Some(100), true));
        assert_eq!(st.chatmix, Some((1.0, 0.0)));
        // Arctis 7 ChatMix: 0 and 255 are full, 191 muted.
        assert_eq!(
            parse_arctis7_chatmix(&[0x06, 0x24, 0, 191]),
            Some((1.0, 0.0))
        );
        assert_eq!(
            parse_arctis7_chatmix(&[0x06, 0x24, 255, 223]),
            Some((1.0, 0.5))
        );
        // GameBuds: left out of the case at 80 %, right docked.
        let st = parse_status(Family::GameBuds, false, &[0xb0, 1, 1, 0x03, 0x02, 80, 0]).unwrap();
        assert_eq!((st.online, st.buds), (true, Some([Some(80), None])));
        let st = parse_status(Family::GameBuds, false, &[0xb0, 1, 1, 0x03, 0x03, 80, 60]).unwrap();
        assert_eq!(st.percent, Some(60));
        let st = parse_status(Family::GameBuds, false, &[0xb0, 1, 1, 0x02, 0x02, 0, 0]).unwrap();
        assert!(!st.online && st.percent.is_none());
    }

    /// Records packets; answers `ask` from a script.
    #[derive(Default)]
    struct FakeLink {
        sent: Vec<Packet>,
        asked: Vec<Vec<u8>>,
        replies: Vec<Option<Vec<u8>>>,
    }

    impl Link for FakeLink {
        fn send(&mut self, packet: &Packet) -> Result<()> {
            self.sent.push(match packet {
                Packet::Output(b) => Packet::Output(b.clone()),
                Packet::Feature(b) => Packet::Feature(b.clone()),
            });
            Ok(())
        }
        fn ask(&mut self, req: &[u8], id: u8) -> Result<Option<Vec<u8>>> {
            self.asked.push(req.to_vec());
            Ok(self.replies.remove(0).filter(|r| r[0] == id))
        }
    }

    fn frame(bytes: &[u8]) -> Option<Vec<u8>> {
        Some(pad(bytes, 64))
    }

    #[test]
    fn save_packets_match_the_sources() {
        // HeadsetControl saveState / Linux-Arctis-Manager save_to_flash.
        let mut legacy = vec![0x06, 0x09];
        legacy.resize(31, 0);
        assert_eq!(Save::Legacy.packet(), Some(Packet::Output(legacy)));
        let mut nova = vec![0x00, 0x09];
        nova.resize(64, 0);
        assert_eq!(Save::Nova.packet(), Some(Packet::Output(nova.clone())));
        assert_eq!(Save::NovaGen2.packet(), Some(Packet::Output(nova.clone())));
        assert_eq!(Save::Blocked("x").packet(), None);
        assert_eq!(Save::None.packet(), None);

        // Arctis 7+: one save, nothing read.
        let mut link = FakeLink::default();
        store(
            &mut link,
            Save::Nova,
            Some((MicSideTone, 2)),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(link.sent, vec![Packet::Output(nova)]);
        assert!(link.asked.is_empty());
        // No save command: nothing is sent.
        let mut link = FakeLink::default();
        assert!(store(&mut link, Save::Blocked("x"), None, Duration::ZERO).is_err());
        assert!(link.sent.is_empty());
    }

    #[test]
    fn gen2_save_reads_validates_and_verifies() {
        let save = Save::NovaGen2.packet().unwrap();
        let sidetone = Some((MicSideTone, 2));
        // Sidetone 2: valid frame before, same value after.
        let mut link = FakeLink {
            replies: vec![
                frame(&[0x20, 0x05, 0x02, 0x00]),
                frame(&[0x20, 0x05, 0x02, 0x00]),
            ],
            ..FakeLink::default()
        };
        store(&mut link, Save::NovaGen2, sidetone, Duration::ZERO).unwrap();
        assert_eq!(link.sent, vec![save.clone()]);
        assert_eq!(link.asked, vec![pad(&[0x00, 0x20], 64); 2]);

        // Auto-off reads the 0xa0 frame.
        let mut link = FakeLink {
            replies: vec![frame(&[0xa0, 30, 0x02]), frame(&[0xa0, 30, 0x02])],
            ..FakeLink::default()
        };
        store(
            &mut link,
            Save::NovaGen2,
            Some((PmShutdown, 30)),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(link.asked[0], pad(&[0x00, 0xa0], 64));

        // An invalid frame (sidetone 7 > 3), no reply or a short one aborts
        // before the save.
        for reply in [
            frame(&[0x20, 0x05, 0x07, 0x00]),
            None,
            Some(vec![0x20, 0x05]),
        ] {
            let mut link = FakeLink {
                replies: vec![reply],
                ..FakeLink::default()
            };
            assert!(store(&mut link, Save::NovaGen2, sidetone, Duration::ZERO).is_err());
            assert!(link.sent.is_empty());
        }

        // A different value after the save is an error.
        let mut link = FakeLink {
            replies: vec![
                frame(&[0x20, 0x05, 0x02, 0x00]),
                frame(&[0x20, 0x05, 0x01, 0x00]),
            ],
            ..FakeLink::default()
        };
        let e = store(&mut link, Save::NovaGen2, sidetone, Duration::ZERO).unwrap_err();
        assert!(e.to_string().contains("reads back 1"), "{e}");
        assert_eq!(link.sent, vec![save.clone()]);

        // Keys without a read-back (mute LED) and switching the mode on
        // just save.
        for check in [Some((MicLedBrightness, 2)), None] {
            let mut link = FakeLink::default();
            store(&mut link, Save::NovaGen2, check, Duration::ZERO).unwrap();
            assert_eq!(link.sent, vec![save.clone()]);
            assert!(link.asked.is_empty());
        }
    }

    #[test]
    fn controls_follow_the_spec() {
        let spec = HeadsetSpec {
            eq: Some(&EQ_NOVA3P),
            ..NOVA7_SPEC
        };
        let keys: Vec<_> = controls(&spec).into_iter().map(|c| c.key).collect();
        let mut want = vec![ChatMixGame, ChatMixChat, EqPreset];
        want.extend(EQ_KEYS);
        want.extend([
            VolumeLimiter,
            BluetoothCallVolume,
            PmShutdown,
            MicVolume,
            MicSideTone,
            MicLedBrightness,
            BluetoothOnPower,
            SaveToDevice,
        ]);
        assert_eq!(keys, want);
        let c = controls(&spec);
        let bt = c.iter().find(|m| m.key == BluetoothOnPower).unwrap();
        assert!(bt.persistent && bt.blocked.is_some());
        let save = c.iter().find(|m| m.key == SaveToDevice).unwrap();
        assert!(save.persistent && save.blocked.is_none());
        for meta in controls(&NOVA7_SPEC)
            .into_iter()
            .filter(|c| !c.read_only && !c.persistent)
        {
            assert!(encode(spec.family, meta.key, 1).is_some(), "{:?}", meta.key);
        }
        // Band sliders take the source's half-dB / tenth-dB steps.
        let c = controls(&spec);
        let band = c.iter().find(|m| m.key == EqBand10).unwrap();
        assert_eq!(
            (band.min, band.max, band.label.as_str()),
            (Some(-12.0), Some(12.0), "16 kHz")
        );
        assert!(band.validate(&SettingValue::Float(-3.7)).is_ok());
        let preset = c.iter().find(|m| m.key == EqPreset).unwrap();
        assert_eq!(preset.options.len(), 5);
    }
}
