//! Razer headsets: Kraken lighting, BlackShark V2 Pro (PlayStation) and
//! BlackShark V2 Pro (2023) battery.
//!
//! Kraken (OpenRazer `driver/razerkraken_driver.{c,h}`, GPL; byte layouts
//! only, reimplemented): a 37-byte output report (id 0x04) sent as
//! SET_REPORT wValue 0x0204 to interface 3:
//!   `[0]` 0x04, `[1]` destination (0x40 = write RAM), `[2]` length,
//!   `[3..5]` address BE, `[5..]` data.
//! The LED mode byte (bits: 0 on/static, 1 one-colour breathing, 2 spectrum,
//! 3 sync) lives at 0x172D ("Kylie": 7.1 V2, Tournament Edition, Ultimate,
//! Kitty V2) or 0x1008 ("Rainie": 7.1, 7.1 Chroma); the static / breathing
//! colour at 0x1741 / 0x15DE. The Kraken 7.1 (classic) has a single-colour
//! LED: on / off only. OpenRazer waits `length * 15` ms after each write.
//! Only RAM is written, so nothing persists; chosen values are re-applied on
//! every connect. No EEPROM write destination is documented (0x20 only reads
//! the EEPROM), so there is no "save to device" here. Reports go out with
//! HIDIOCSOUTPUT, which usbhid turns into exactly that SET_REPORT.
//!
//! BlackShark V2 Pro for PlayStation, dongle 1532:1014 (HeadsetControl
//! `lib/devices/razer_blackshark_v2_pro_ps.hpp` and its test captures,
//! <https://github.com/Sapd/HeadsetControl>, GPL; reimplemented): 64-byte
//! reports with id 0x02 on interface 3, `02 80 LL CRC16(LE) payload`, the
//! CRC-16/XMODEM of the whole frame with the CRC zeroed; each write is
//! acknowledged by input report 0x02 (GET_REPORT, byte 1 = 'O') and answered
//! by an input report `02 LL 'P' 'I' .. type(12) 0x80|seq(13) status(14) data`.
//! The battery voltage (mV) is read from the headset by switching the dongle
//! to "remote" mode and always back to "local" (a runtime mode, not stored).
//!
//! BlackShark V2 Pro (2023), dongle 1532:0555 (OpenRazer PR #2862
//! `driver/razerblackshark_driver.{c,h}`, unmerged, and blackshark-control
//! `blackshark/protocol.py`, which follows it; both GPL, reimplemented):
//! the vendor node (usage page 0xFF00, report id 0x02) takes 64-byte output
//! reports `02 80 LL 00 00 'P' 'A' 08 00 type cmd 00 len payload` (no CRC;
//! LL = 8 + len) and answers with input reports `[12]` cmd, `[13]` 0x01 ack,
//! `[14]` length, data from `[15]`. A query (type 0x03) is wrapped in
//! set_remote_mode frames `02 80 07 00 00 'P' 'A' 0e 00 02 e1 on`: one
//! sacrificial "on" wakes the dozing link, each attempt re-sends "on", the
//! last frame is "off"; 35 ms after each, replies within 150 ms. Battery
//! (0x21) is 0..=100, charging (0x2a) nonzero on the cable. The headset
//! stores sidetone and auto-off (PR #2862: "settings persist in the
//! device"), so those are listed blocked.

use super::protocol::Effect;
use super::VID;
use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, find_hidraw, hidioc, report_ids, usage_pages, HidrawDevice};
use anyhow::{anyhow, bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// The interface OpenRazer and HeadsetControl address on every model here.
const IFACE: u32 = 3;

// ── Kraken lighting ─────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum Gen {
    /// Kraken 7.1: single-colour LED, on / off.
    Classic,
    Rainie,
    Kylie,
}

impl Gen {
    fn led_addr(self) -> u16 {
        match self {
            Gen::Kylie => 0x172d,
            _ => 0x1008,
        }
    }

    fn colour_addr(self) -> u16 {
        match self {
            Gen::Kylie => 0x1741,
            _ => 0x15de,
        }
    }

    fn effects(self) -> &'static [Effect] {
        match self {
            Gen::Classic => &[Effect::Off, Effect::Static],
            _ => &[
                Effect::Off,
                Effect::Static,
                Effect::Spectrum,
                Effect::Breathing,
            ],
        }
    }
}

struct Kraken {
    name: &'static str,
    pids: &'static [u16],
    gen: Gen,
}

/// Names from OpenRazer `razer_attr_read_device_type`.
const KRAKENS: [Kraken; 6] = [
    Kraken {
        name: "Razer Kraken 7.1",
        pids: &[0x0501, 0x0506],
        gen: Gen::Classic,
    },
    Kraken {
        name: "Razer Kraken 7.1 Chroma",
        pids: &[0x0504],
        gen: Gen::Rainie,
    },
    Kraken {
        name: "Razer Kraken 7.1 V2",
        pids: &[0x0510],
        gen: Gen::Kylie,
    },
    Kraken {
        name: "Razer Kraken Tournament Edition",
        pids: &[0x0520],
        gen: Gen::Kylie,
    },
    Kraken {
        name: "Razer Kraken Ultimate",
        pids: &[0x0527],
        gen: Gen::Kylie,
    },
    Kraken {
        name: "Razer Kraken Kitty V2",
        pids: &[0x0560],
        gen: Gen::Kylie,
    },
];

const fn kraken_model(k: &Kraken) -> Model {
    Model {
        vendor_id: VID,
        product_ids: k.pids,
        kind: DeviceKind::Headset,
        name: k.name,
        usbfs: false,
        multi: false,
        open: open_kraken,
    }
}

const BLACKSHARK_PID: u16 = 0x1014;
const BLACKSHARK_2023_PID: u16 = 0x0555;

pub const MODELS: &[Model] = &[
    kraken_model(&KRAKENS[0]),
    kraken_model(&KRAKENS[1]),
    kraken_model(&KRAKENS[2]),
    kraken_model(&KRAKENS[3]),
    kraken_model(&KRAKENS[4]),
    kraken_model(&KRAKENS[5]),
    Model {
        vendor_id: VID,
        product_ids: &[BLACKSHARK_PID],
        kind: DeviceKind::Headset,
        name: "Razer BlackShark V2 Pro (PlayStation)",
        usbfs: false,
        multi: false,
        open: open_blackshark,
    },
    Model {
        vendor_id: VID,
        product_ids: &[BLACKSHARK_2023_PID],
        kind: DeviceKind::Headset,
        name: "Razer BlackShark V2 Pro (2023)",
        usbfs: false,
        multi: false,
        open: open_blackshark_2023,
    },
];

const RAM_WRITE: u8 = 0x40;
const LED_ON: u8 = 0x01;
const LED_BREATHING: u8 = 0x02;
const LED_SPECTRUM: u8 = 0x04;
const LED_SYNC: u8 = 0x08;

fn kraken_report(addr: u16, data: &[u8]) -> [u8; 37] {
    let mut r = [0u8; 37];
    let [hi, lo] = addr.to_be_bytes();
    r[..5].copy_from_slice(&[0x04, RAM_WRITE, data.len() as u8, hi, lo]);
    r[5..5 + data.len()].copy_from_slice(data);
    r
}

/// The writes OpenRazer's `matrix_effect_*` handlers send, in order.
fn kraken_writes(gen: Gen, effect: Effect, rgb: [u8; 3]) -> Vec<[u8; 37]> {
    let mode = |bits| kraken_report(gen.led_addr(), &[bits]);
    let colour = kraken_report(gen.colour_addr(), &rgb);
    match effect {
        Effect::Static if gen == Gen::Classic => vec![mode(LED_ON)],
        Effect::Static => vec![colour, mode(LED_ON)],
        Effect::Spectrum => vec![mode(LED_ON | LED_SPECTRUM)],
        Effect::Breathing => vec![colour, mode(LED_ON | LED_BREATHING | LED_SYNC)],
        _ => vec![mode(0x00)],
    }
}

/// HIDIOCSOUTPUT: SET_REPORT(Output) with `buf[0]` as the report id.
fn set_output(file: &File, buf: &mut [u8]) -> Result<()> {
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), hidioc(0x0b, buf.len()), buf.as_mut_ptr()) };
    if ret < 0 {
        bail!("HIDIOCSOUTPUT: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn open_rw(path: &std::path::Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn open_kraken(pid: u16, ctx: &DriverCtx) -> Opened {
    let k = KRAKENS
        .iter()
        .find(|k| k.pids.contains(&pid))
        .with_context(|| format!("no Kraken for {pid:04x}"))?;
    let info = find_hidraw(VID, pid, Some(IFACE)).context("hidraw interface 3")?;
    let mut settings = hako_core::config::load_device_settings(&ctx.paths, VID, k.pids[0])?;
    for (key, v) in [
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::ColorR, SettingValue::Int(0)),
        (SettingKey::ColorG, SettingValue::Int(120)),
        (SettingKey::ColorB, SettingValue::Int(255)),
    ] {
        settings.entry(key.as_str().into()).or_insert(v);
    }
    let mut a = KrakenAdapter {
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: info.path.display().to_string(),
        },
        kraken: k,
        file: Some(open_rw(&info.path)?),
        path: info.path.clone(),
        settings,
        last_error: None,
        paths: ctx.paths.clone(),
    };
    a.last_error = a.apply().err().map(|e| e.to_string());
    info!("{} open via {}", k.name, info.path.display());
    Ok(vec![Box::new(a)])
}

struct KrakenAdapter {
    id: DeviceId,
    kraken: &'static Kraken,
    file: Option<File>,
    path: PathBuf,
    settings: SettingsMap,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl KrakenAdapter {
    fn effect(&self) -> Effect {
        let fx = Effect::from_str(
            get_setting(&self.settings, SettingKey::Effect)
                .and_then(|v| v.as_str())
                .unwrap_or("static"),
        );
        if self.kraken.gen.effects().contains(&fx) {
            fx
        } else {
            Effect::Static
        }
    }

    fn apply(&mut self) -> Result<()> {
        let byte = |k| {
            get_setting(&self.settings, k)
                .and_then(|v| v.as_int())
                .map_or(0, |v| v.clamp(0, 255) as u8)
        };
        let rgb = [
            byte(SettingKey::ColorR),
            byte(SettingKey::ColorG),
            byte(SettingKey::ColorB),
        ];
        let file = self.file.as_ref().context("device not open")?;
        for mut report in kraken_writes(self.kraken.gen, self.effect(), rgb) {
            if let Err(e) = set_output(file, &mut report) {
                if !self.path.exists() {
                    self.file = None;
                }
                return Err(e);
            }
            std::thread::sleep(Duration::from_millis(u64::from(report[2]) * 15));
        }
        Ok(())
    }

    fn controls(&self) -> Vec<SettingMeta> {
        let gen = self.kraken.gen;
        let opts = gen
            .effects()
            .iter()
            .map(|fx| SettingOption::text(fx.as_str(), fx.label()))
            .collect();
        let mut c = vec![SettingMeta::dropdown(SettingKey::Effect, "Effect", opts)];
        if gen != Gen::Classic {
            let hidden = !matches!(self.effect(), Effect::Static | Effect::Breathing);
            for (key, label) in [
                (SettingKey::ColorR, "Red"),
                (SettingKey::ColorG, "Green"),
                (SettingKey::ColorB, "Blue"),
            ] {
                c.push(SettingMeta::color(key, label).hidden(hidden));
            }
        }
        c
    }
}

impl DeviceAdapter for KrakenAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.kraken.name.into(),
            connected: self.file.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if !self.controls().iter().any(|m| m.key == key) {
            bail!("{} has no {} control", self.kraken.name, key.as_str());
        }
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        if let Err(e) = self.apply() {
            self.settings = before;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        hako_core::config::save_device_settings(
            &self.paths,
            VID,
            self.kraken.pids[0],
            &self.settings,
        )
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        self.file = None;
    }
}

// ── BlackShark V2 Pro (PlayStation) battery ─────────────────────────────────

const BS_REPORT_ID: u8 = 0x02;
const BS_FRAME_OUT: u8 = 0x80;
const BS_ACK_OK: u8 = 0x4f;
const TYPE_OTA: u8 = 0x06;
const TYPE_LINK: u8 = 0x0e;
const LINK_STATUS: u8 = 0xe6;
const GET_MODE: u8 = 0xe0;
const SET_MODE: u8 = 0xe1;
const MODE_LOCAL: u8 = 0x00;
const MODE_REMOTE: u8 = 0x01;
const LINK_HEADSET: u8 = 0x08;
const GET_BATTERY: u8 = 0x31;
/// The "read max length" ioctl (0x5A5A1234, 240) Razer's updater sends after
/// every mode switch.
const READ_MAX_LEN: [u8; 13] = [
    0x25, 0x34, 0x12, 0x5a, 0x5a, 0x01, 0x00, 0x00, 0x00, 0xf0, 0x00, 0x00, 0x00,
];
const REPLY_TIMEOUT: Duration = Duration::from_millis(1500);
const IOCTL_TIMEOUT: Duration = Duration::from_millis(2500);
const MODE_SETTLE: Duration = Duration::from_millis(300);
const ATTEMPTS: usize = 3;
const VOLTAGE_RANGE: std::ops::RangeInclusive<u16> = 2500..=4500;
const BATTERY_POLL: Duration = Duration::from_secs(60);
const FAILURE_BACKOFF: Duration = Duration::from_secs(10);
/// Most time one battery poll spends on its query, then on switching the
/// dongle back to local mode (each command's wait is cut to what is left).
const POLL_BUDGET: Duration = Duration::from_secs(5);
const RESTORE_BUDGET: Duration = Duration::from_secs(5);
/// Single-cell Li-ion curve (percent, mV); the headset reports no percentage.
const CALIBRATION: [(u8, u16); 11] = [
    (0, 3300),
    (5, 3500),
    (12, 3600),
    (20, 3650),
    (30, 3700),
    (40, 3750),
    (50, 3800),
    (65, 3900),
    (80, 4000),
    (90, 4100),
    (100, 4180),
];

/// CRC-16/XMODEM (poly 0x1021, init 0, no reflection).
fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |mut crc, &b| {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
        crc
    })
}

/// `"PA" type seq len [len_hi for OTA] body`.
fn bs_payload(ty: u8, seq: u8, body: &[u8]) -> Vec<u8> {
    let [lo, hi] = (body.len() as u16).to_le_bytes();
    let mut p = vec![0x50, 0x41, ty, seq, lo];
    if ty == TYPE_OTA {
        p.push(hi);
    }
    p.extend_from_slice(body);
    p
}

fn bs_frame(payload: &[u8]) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[..3].copy_from_slice(&[BS_REPORT_ID, BS_FRAME_OUT, payload.len() as u8]);
    f[5..5 + payload.len()].copy_from_slice(payload);
    let [lo, hi] = crc16(&f).to_le_bytes();
    f[3] = lo;
    f[4] = hi;
    f
}

/// Bytes from the status byte on, if `report` answers command `ty` / `seq`.
fn bs_reply(report: &[u8], ty: u8, seq: u8) -> Option<&[u8]> {
    let end = report.len().min(2 + usize::from(*report.get(1)?));
    (end > 14
        && report[0] == BS_REPORT_ID
        && report[2..4] == [0x50, 0x49]
        && report[12] == ty
        && report[13] == 0x80 | seq)
        .then(|| &report[14..end])
}

/// Link-layer value: the reply echoes a "PI" frame, id at `[11]`, value `[12]`.
fn bs_link_value(data: &[u8], id: u8) -> Option<u8> {
    (data.len() >= 13 && data[0] == 0 && data[1..3] == [0x50, 0x49] && data[11] == id)
        .then(|| data[12])
}

fn bs_voltage(data: &[u8]) -> Option<u16> {
    (data.len() >= 3 && data[0] == 0)
        .then(|| u16::from_le_bytes([data[1], data[2]]))
        .filter(|mv| VOLTAGE_RANGE.contains(mv))
}

/// A command's reply wait, cut to what is left before `deadline`; `None`
/// once it has passed.
fn wait_within(timeout: Duration, deadline: Option<Instant>, now: Instant) -> Option<Duration> {
    let left = deadline.map_or(timeout, |d| timeout.min(d.saturating_duration_since(now)));
    (!left.is_zero()).then_some(left)
}

/// Time between battery polls: doubled from `FAILURE_BACKOFF` per failed poll
/// in a row, up to the normal period.
fn poll_every(failures: u32) -> Duration {
    if failures == 0 {
        return BATTERY_POLL;
    }
    FAILURE_BACKOFF
        .saturating_mul(1 << (failures - 1).min(8))
        .min(BATTERY_POLL)
}

fn voltage_to_percent(mv: u16) -> u8 {
    let (first, last) = (CALIBRATION[0], CALIBRATION[CALIBRATION.len() - 1]);
    if mv <= first.1 {
        return first.0;
    }
    if mv >= last.1 {
        return last.0;
    }
    CALIBRATION
        .windows(2)
        .find(|w| mv <= w[1].1)
        .map_or(last.0, |w| {
            let ((p0, v0), (p1, v1)) = (w[0], w[1]);
            (u32::from(p0) + u32::from(mv - v0) * u32::from(p1 - p0) / u32::from(v1 - v0)) as u8
        })
}

fn open_blackshark(pid: u16, _ctx: &DriverCtx) -> Opened {
    let info = find_hidraw(VID, pid, Some(IFACE)).context("hidraw interface 3")?;
    let a = BlackSharkAdapter {
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: info.path.display().to_string(),
        },
        dev: Some(HidrawDevice::open(&info.path)?),
        ctl: open_rw(&info.path)?,
        seq: 0,
        linked: false,
        percent: None,
        last_poll: None,
        failures: 0,
        deadline: None,
        last_error: None,
    };
    info!("Razer BlackShark V2 Pro open via {}", info.path.display());
    Ok(vec![Box::new(a)])
}

struct BlackSharkAdapter {
    id: DeviceId,
    dev: Option<HidrawDevice>,
    /// Second handle for HIDIOCGINPUT (the acknowledgement).
    ctl: File,
    seq: u8,
    linked: bool,
    percent: Option<u8>,
    last_poll: Option<Instant>,
    /// Failed polls in a row (backs the next poll off).
    failures: u32,
    /// Every command's reply wait ends by then (a poll's time budget).
    deadline: Option<Instant>,
    last_error: Option<String>,
}

impl BlackSharkAdapter {
    fn command(&mut self, ty: u8, body: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        let timeout = wait_within(timeout, self.deadline, Instant::now())
            .ok_or_else(|| anyhow!("battery poll ran out of time"))?;
        self.seq = self.seq % 127 + 1;
        let seq = self.seq;
        let dev = self.dev.as_mut().context("device not open")?;
        let frame = bs_frame(&bs_payload(ty, seq, body));
        if let Err(e) = dev.write(&frame) {
            if !dev.path.exists() {
                self.dev = None;
            }
            return Err(e);
        }
        // HIDIOCGINPUT (0x0a): input report 0x02 over the control pipe.
        let mut ack = [0u8; 64];
        ack[0] = BS_REPORT_ID;
        let ret = unsafe {
            libc::ioctl(
                self.ctl.as_raw_fd(),
                hidioc(0x0a, ack.len()),
                ack.as_mut_ptr(),
            )
        };
        if ret < 0 {
            bail!("HIDIOCGINPUT: {}", std::io::Error::last_os_error());
        }
        if ack[1] != BS_ACK_OK {
            bail!("dongle rejected command {ty:#04x}");
        }
        let start = Instant::now();
        let mut buf = [0u8; 64];
        loop {
            let left = timeout
                .checked_sub(start.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| anyhow!("no reply from the dongle to command {ty:#04x}"))?;
            let n = dev.read_timeout(&mut buf, left)?;
            if let Some(data) = bs_reply(&buf[..n], ty, seq) {
                return Ok(data.to_vec());
            }
        }
    }

    fn link_value(&mut self, id: u8) -> Result<u8> {
        let data = self.command(TYPE_LINK, &[id], REPLY_TIMEOUT)?;
        bs_link_value(&data, id)
            .with_context(|| format!("unexpected reply to link query {id:#04x}"))
    }

    fn is_mode(&mut self, mode: u8) -> bool {
        self.link_value(GET_MODE).is_ok_and(|m| m == mode)
    }

    fn set_mode(&mut self, mode: u8) -> bool {
        let ok = self
            .command(TYPE_LINK, &[SET_MODE, mode], REPLY_TIMEOUT)
            .is_ok_and(|r| r.first() == Some(&0));
        if !ok {
            return false;
        }
        std::thread::sleep(MODE_SETTLE);
        self.command(TYPE_OTA, &READ_MAX_LEN, IOCTL_TIMEOUT).is_ok()
    }

    fn request_voltage(&mut self) -> Result<u16> {
        let mut last = anyhow!("no battery reply from the headset");
        for _ in 0..ATTEMPTS {
            match self.command(TYPE_OTA, &[GET_BATTERY], REPLY_TIMEOUT) {
                Ok(data) => match bs_voltage(&data) {
                    Some(mv) => return Ok(mv),
                    None => last = anyhow!("headset rejected the battery request"),
                },
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Battery voltage, or `None` when no headset is linked to the dongle;
    /// bounded by `POLL_BUDGET` + `RESTORE_BUDGET` (a dongle that acks but
    /// never answers would otherwise hold the worker for ~25 s).
    fn read_voltage(&mut self) -> Result<Option<u16>> {
        self.deadline = Some(Instant::now() + POLL_BUDGET);
        let result = self.read_voltage_within();
        self.deadline = None;
        result
    }

    fn read_voltage_within(&mut self) -> Result<Option<u16>> {
        if self.link_value(LINK_STATUS)? & LINK_HEADSET == 0 {
            return Ok(None);
        }
        let mode = self.link_value(GET_MODE)?;
        if mode != MODE_LOCAL && mode != MODE_REMOTE {
            bail!("dongle is in unknown mode {mode:#04x}");
        }
        let voltage =
            if mode == MODE_REMOTE || (self.set_mode(MODE_REMOTE) && self.is_mode(MODE_REMOTE)) {
                self.request_voltage()
            } else {
                Err(anyhow!("dongle did not enter remote mode"))
            };
        // Local is the dongle's normal mode: restore it whatever happened.
        self.deadline = Some(Instant::now() + RESTORE_BUDGET);
        let mut local = self.is_mode(MODE_LOCAL);
        for _ in 0..ATTEMPTS {
            if local {
                break;
            }
            self.set_mode(MODE_LOCAL);
            local = self.is_mode(MODE_LOCAL);
        }
        if !local {
            bail!("could not switch the dongle back to local mode");
        }
        voltage.map(Some)
    }
}

impl DeviceAdapter for BlackSharkAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: "Razer BlackShark V2 Pro (PlayStation)".into(),
            connected: self.dev.is_some(),
            batteries: vec![BatteryState {
                label: "Battery".into(),
                percent: self.percent,
                charging: false,
                present: self.linked,
            }],
            settings: SettingsMap::new(),
            controls: vec![],
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("BlackShark V2 Pro has no {} control", key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        let every = poll_every(self.failures);
        if self.dev.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < every) {
            return Ok(());
        }
        let result = self.read_voltage();
        // Counted from the poll's end, so a slow poll is not run back to back.
        self.last_poll = Some(Instant::now());
        self.failures = if result.is_ok() {
            0
        } else {
            self.failures.saturating_add(1)
        };
        match result {
            Ok(mv) => {
                self.linked = mv.is_some();
                self.percent = mv.map(voltage_to_percent);
                self.last_error = None;
            }
            Err(e) => {
                debug!("BlackShark battery: {e}");
                self.last_error = Some(e.to_string());
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

// ── BlackShark V2 Pro (2023) battery ───────────────────────────────────────

const BS23_USAGE_PAGE: u16 = 0xff00;
const BS23_QUERY: u8 = 0x03;
const BS23_BATTERY: u8 = 0x21;
const BS23_CHARGING: u8 = 0x2a;
const BS23_SETTLE: Duration = Duration::from_millis(35);
/// Per-attempt reply wait, below the ~0.3 s after which the link dozes.
const BS23_REPLY: Duration = Duration::from_millis(150);
const BS23_STORED: &str =
    "the headset stores it; layout from one unmerged OpenRazer PR (#2862) and a tool based on it";

/// A query (type 0x03) without payload.
fn bs23_query(cmd: u8) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[..3].copy_from_slice(&[BS_REPORT_ID, BS_FRAME_OUT, 0x08]);
    f[5..8].copy_from_slice(&[0x50, 0x41, 0x08]);
    f[9..11].copy_from_slice(&[BS23_QUERY, cmd]);
    f
}

/// set_remote_mode (type 0x02, cmd 0xe1, the flag byte carries on / off).
fn bs23_remote(on: bool) -> [u8; 64] {
    let mut f = [0u8; 64];
    f[..3].copy_from_slice(&[BS_REPORT_ID, BS_FRAME_OUT, 0x07]);
    f[5..8].copy_from_slice(&[0x50, 0x41, 0x0e]);
    f[9..12].copy_from_slice(&[0x02, SET_MODE, u8::from(on)]);
    f
}

/// Payload of an acknowledged reply to `cmd`, cut to the length it reports.
fn bs23_reply(report: &[u8], cmd: u8) -> Option<&[u8]> {
    (report.len() > 15 && report[0] == BS_REPORT_ID && report[12] == cmd && report[13] == 0x01)
        .then(|| &report[15..report.len().min(15 + usize::from(report[14]))])
}

fn open_blackshark_2023(pid: u16, _ctx: &DriverCtx) -> Opened {
    let info = enumerate_hidraw()?
        .into_iter()
        .find(|d| {
            let desc = d.report_descriptor();
            d.vendor_id == VID
                && d.product_id == pid
                && usage_pages(&desc).contains(&BS23_USAGE_PAGE)
                && report_ids(&desc).contains(&BS_REPORT_ID)
        })
        .context("no vendor control node (usage page ff00, report id 2)")?;
    let a = BlackShark2023Adapter {
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: info.path.display().to_string(),
        },
        dev: Some(HidrawDevice::open(&info.path)?),
        state: None,
        last_poll: None,
        last_error: None,
    };
    info!(
        "Razer BlackShark V2 Pro (2023) open via {}",
        info.path.display()
    );
    Ok(vec![Box::new(a)])
}

struct BlackShark2023Adapter {
    id: DeviceId,
    dev: Option<HidrawDevice>,
    /// (percent, charging) while the headset answers.
    state: Option<(u8, bool)>,
    last_poll: Option<Instant>,
    last_error: Option<String>,
}

impl BlackShark2023Adapter {
    fn send(&mut self, frame: &[u8; 64]) -> Result<()> {
        let dev = self.dev.as_mut().context("device not open")?;
        if let Err(e) = dev.write(frame) {
            if !dev.path.exists() {
                self.dev = None;
            }
            return Err(e);
        }
        Ok(())
    }

    fn remote(&mut self, on: bool) -> Result<()> {
        self.send(&bs23_remote(on))?;
        std::thread::sleep(BS23_SETTLE);
        Ok(())
    }

    /// First payload byte of the answer, `None` if the headset does not
    /// answer (off or out of range: the dongle enumerates either way).
    fn query(&mut self, cmd: u8) -> Result<Option<u8>> {
        self.dev
            .as_mut()
            .context("device not open")?
            .drain(Duration::ZERO);
        let answer = self.ask(cmd);
        // Remote mode stays until it is switched off: always switch it off.
        let off = self.remote(false);
        let answer = answer?;
        off?;
        Ok(answer)
    }

    fn ask(&mut self, cmd: u8) -> Result<Option<u8>> {
        self.remote(true)?;
        for _ in 0..ATTEMPTS {
            self.remote(true)?;
            self.send(&bs23_query(cmd))?;
            if let Some(answer) = self.await_reply(cmd) {
                return Ok(Some(answer));
            }
        }
        Ok(None)
    }

    fn await_reply(&mut self, cmd: u8) -> Option<u8> {
        let dev = self.dev.as_mut()?;
        let start = Instant::now();
        let mut buf = [0u8; 64];
        loop {
            let left = BS23_REPLY
                .checked_sub(start.elapsed())
                .filter(|d| !d.is_zero())?;
            let n = dev.read_timeout(&mut buf, left).ok()?;
            if let Some(data) = bs23_reply(&buf[..n], cmd) {
                return data.first().copied();
            }
        }
    }

    fn read_state(&mut self) -> Result<Option<(u8, bool)>> {
        let Some(level) = self.query(BS23_BATTERY)? else {
            return Ok(None);
        };
        let charging = self.query(BS23_CHARGING)?.is_some_and(|c| c != 0);
        Ok(Some((level.min(100), charging)))
    }
}

impl DeviceAdapter for BlackShark2023Adapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        let stored = |meta: SettingMeta| meta.persistent().blocked(BS23_STORED);
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: "Razer BlackShark V2 Pro (2023)".into(),
            connected: self.dev.is_some(),
            batteries: vec![BatteryState {
                label: "Battery".into(),
                percent: self.state.map(|(pct, _)| pct),
                charging: self.state.is_some_and(|(_, c)| c),
                present: self.state.is_some(),
            }],
            settings: SettingsMap::new(),
            controls: vec![
                stored(SettingMeta::slider(
                    SettingKey::MicSideTone,
                    "Mic monitoring",
                    0.0,
                    10.0,
                    1.0,
                )),
                stored(
                    SettingMeta::slider(SettingKey::PmShutdown, "Auto-off", 15.0, 60.0, 1.0)
                        .unit("min"),
                ),
            ],
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!(
            "BlackShark V2 Pro (2023): {} is not available",
            key.as_str()
        )
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < BATTERY_POLL) {
            return Ok(());
        }
        self.last_poll = Some(Instant::now());
        match self.read_state() {
            Ok(state) => {
                self.state = state;
                self.last_error = None;
            }
            Err(e) => {
                debug!("BlackShark V2 Pro (2023) battery: {e}");
                self.state = None;
                self.last_error = Some(e.to_string());
            }
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

    /// A battery poll's command waits end at its deadline, and failed polls
    /// back off instead of repeating every 10 s.
    #[test]
    fn blackshark_poll_is_bounded_and_backs_off() {
        let now = Instant::now();
        assert_eq!(wait_within(REPLY_TIMEOUT, None, now), Some(REPLY_TIMEOUT));
        let soon = now + Duration::from_millis(200);
        assert_eq!(
            wait_within(REPLY_TIMEOUT, Some(soon), now),
            Some(Duration::from_millis(200))
        );
        assert_eq!(wait_within(REPLY_TIMEOUT, Some(now), soon), None);
        let waits: Vec<_> = (0..6).map(|f| poll_every(f).as_secs()).collect();
        assert_eq!(waits, [60, 10, 20, 40, 60, 60]);
        assert_eq!(poll_every(u32::MAX), BATTERY_POLL);
    }

    #[test]
    fn kraken_reports_match_openrazer_layout() {
        // Kylie static: colour to 0x1741 (3 bytes), then mode 0x01 at 0x172D.
        let w = kraken_writes(Gen::Kylie, Effect::Static, [0xff, 0x00, 0x80]);
        assert_eq!(w.len(), 2);
        assert_eq!(
            &w[0][..8],
            &[0x04, 0x40, 0x03, 0x17, 0x41, 0xff, 0x00, 0x80]
        );
        assert!(w[0][8..].iter().all(|&b| b == 0));
        assert_eq!(&w[1][..6], &[0x04, 0x40, 0x01, 0x17, 0x2d, 0x01]);
        // Spectrum = on | spectrum cycling; none = 0.
        assert_eq!(
            kraken_writes(Gen::Kylie, Effect::Spectrum, [0; 3])[0][5],
            0x05
        );
        assert_eq!(kraken_writes(Gen::Kylie, Effect::Off, [0; 3])[0][5], 0x00);
        // Single-colour breathing = on | breathing | sync, Rainie addresses.
        let w = kraken_writes(Gen::Rainie, Effect::Breathing, [1, 2, 3]);
        assert_eq!(&w[0][..8], &[0x04, 0x40, 0x03, 0x15, 0xde, 1, 2, 3]);
        assert_eq!(&w[1][..6], &[0x04, 0x40, 0x01, 0x10, 0x08, 0x0b]);
        // Kraken 7.1 takes no colour for static.
        let w = kraken_writes(Gen::Classic, Effect::Static, [1, 2, 3]);
        assert_eq!(w.len(), 1);
        assert_eq!(&w[0][..6], &[0x04, 0x40, 0x01, 0x10, 0x08, 0x01]);
    }

    #[test]
    fn model_table() {
        let mut pids: Vec<u16> = MODELS
            .iter()
            .flat_map(|m| m.product_ids.iter().copied())
            .collect();
        let n = pids.len();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), n, "PID listed twice");
        assert!(MODELS
            .iter()
            .all(|m| m.kind == DeviceKind::Headset && m.vendor_id == VID));
        for k in &KRAKENS {
            assert!(k.gen.effects().contains(&Effect::Static), "{}", k.name);
        }
    }

    fn padded(prefix: &[u8]) -> [u8; 64] {
        let mut f = [0u8; 64];
        f[..prefix.len()].copy_from_slice(prefix);
        f
    }

    /// Frames from HeadsetControl's captured session (dongle firmware 1.0.8).
    #[test]
    fn blackshark_frames_match_capture() {
        let tx = |ty, seq, body: &[u8]| bs_frame(&bs_payload(ty, seq, body));
        assert_eq!(
            tx(TYPE_LINK, 1, &[LINK_STATUS]),
            padded(&[0x02, 0x80, 0x06, 0x1a, 0x40, 0x50, 0x41, 0x0e, 0x01, 0x01, 0xe6])
        );
        assert_eq!(
            tx(TYPE_LINK, 3, &[SET_MODE, MODE_REMOTE]),
            padded(&[0x02, 0x80, 0x07, 0xbd, 0xbd, 0x50, 0x41, 0x0e, 0x03, 0x02, 0xe1, 0x01])
        );
        assert_eq!(
            tx(TYPE_OTA, 4, &READ_MAX_LEN),
            padded(&[
                0x02, 0x80, 0x13, 0xbd, 0xdb, 0x50, 0x41, 0x06, 0x04, 0x0d, 0x00, 0x25, 0x34, 0x12,
                0x5a, 0x5a, 0x01, 0x00, 0x00, 0x00, 0xf0, 0x00, 0x00, 0x00
            ])
        );
        assert_eq!(
            tx(TYPE_OTA, 6, &[GET_BATTERY]),
            padded(&[0x02, 0x80, 0x07, 0xf6, 0x5d, 0x50, 0x41, 0x06, 0x06, 0x01, 0x00, 0x31])
        );
        assert_eq!(
            tx(TYPE_LINK, 8, &[SET_MODE, MODE_LOCAL]),
            padded(&[0x02, 0x80, 0x07, 0x72, 0xde, 0x50, 0x41, 0x0e, 0x08, 0x02, 0xe1, 0x00])
        );
    }

    /// Frames as PR #2862 `blackshark_v2pro_write_header` / `blackshark_set_remote`
    /// and blackshark-control `_frame` / `_remote` build them.
    #[test]
    fn blackshark_2023_frames_and_replies() {
        assert_eq!(
            bs23_query(BS23_BATTERY),
            padded(&[0x02, 0x80, 0x08, 0, 0, 0x50, 0x41, 0x08, 0, 0x03, 0x21])
        );
        assert_eq!(
            bs23_remote(true),
            padded(&[0x02, 0x80, 0x07, 0, 0, 0x50, 0x41, 0x0e, 0, 0x02, 0xe1, 0x01])
        );
        assert_eq!(bs23_remote(false)[11], 0x00);

        let mut r = padded(&[0x02]);
        r[12..16].copy_from_slice(&[BS23_BATTERY, 0x01, 0x01, 0x4b]);
        assert_eq!(bs23_reply(&r, BS23_BATTERY), Some(&[0x4b][..]));
        assert_eq!(bs23_reply(&r, BS23_CHARGING), None, "other command");
        r[13] = 0x00;
        assert_eq!(bs23_reply(&r, BS23_BATTERY), None, "not acknowledged");
        r[13] = 0x01;
        r[14] = 0xff;
        assert_eq!(bs23_reply(&r, BS23_BATTERY).map(<[u8]>::len), Some(64 - 15));
        assert_eq!(bs23_reply(&r[..15], BS23_BATTERY), None, "too short");

        // Headset-stored settings are listed, never offered.
        let a = BlackShark2023Adapter {
            id: DeviceId {
                vendor_id: VID,
                product_id: BLACKSHARK_2023_PID,
                instance: "test".into(),
            },
            dev: None,
            state: None,
            last_poll: None,
            last_error: None,
        };
        let snap = a.snapshot();
        assert!(!snap.controls.is_empty());
        for m in &snap.controls {
            assert!(m.persistent && m.blocked.is_some(), "{}", m.key.as_str());
            assert!(m.validate(&SettingValue::Int(5)).is_err());
        }
        assert!(!snap.batteries[0].present);
    }

    #[test]
    fn blackshark_replies_from_capture() {
        let link = [
            0x02, 0x19, 0x50, 0x49, 0x01, 0xc0, 0x99, 0x5c, 0x56, 0x02, 0x03, 0x00, 0x0e, 0x81,
            0x00, 0x50, 0x49, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0xe6, 0x1b, 0xce,
        ];
        let data = bs_reply(&link, TYPE_LINK, 1).unwrap();
        assert_eq!(bs_link_value(data, LINK_STATUS), Some(0x1b));
        assert_eq!(bs_link_value(data, GET_MODE), None);
        // Wrong sequence number or command type: not ours.
        assert!(bs_reply(&link, TYPE_LINK, 2).is_none());
        assert!(bs_reply(&link, TYPE_OTA, 1).is_none());

        let battery = [
            0x02, 0x11, 0x50, 0x49, 0x01, 0xe4, 0xbd, 0xcd, 0xd3, 0x01, 0x07, 0x00, 0x06, 0x86,
            0x00, 0x6c, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0xe0, 0x01, 0xce,
        ];
        let data = bs_reply(&battery, TYPE_OTA, 6).unwrap();
        // Byte 1 counts the valid bytes: the stale tail is cut off.
        assert_eq!(data.len(), 5);
        assert_eq!(bs_voltage(data), Some(3948));
        assert_eq!(voltage_to_percent(3948), 72);
        assert_eq!(voltage_to_percent(3800), 50);
        assert_eq!(voltage_to_percent(3000), 0);
        assert_eq!(voltage_to_percent(4300), 100);
        assert_eq!(bs_voltage(&[0x00, 0x10, 0x00]), None, "implausible voltage");
    }
}
