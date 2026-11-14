//! Audeze Maxwell / Maxwell 2 dongles (Airoha based), byte layouts from
//! HeadsetControl `audeze_maxwell_device.hpp`.
//!
//! 62-byte reports: output `06 <len> 80 <fragment>` written to hidraw, input
//! `07 <len> 80 <fragment>` fetched with HIDIOCGINPUT (len 0 = nothing new). The
//! fragments form a stream of messages `05 <type> <len lo> <len hi> <body>`, type
//! 5A request, 5B response, 5D asynchronous result. Bodies used here:
//!
//! ```text
//! read key        01 09 <key> 00          -> 5B 01 09 <key> 00 <status> <value>
//! write key       00 09 <key> 00 <value>  -> 5B 00 09 <key> 00 00
//! read parameter  83 2C <param> 00        -> 5B 83 2C <status> <param> 00 <value>
//! write parameter 82 2C <param> 00 <val>  -> 5B 82 2C <status> <param> 00
//! battery         D6 0C 00                -> 5B D6 0C <status>, 5D D6 0C 00 00 <percent>
//! ```
//!
//! Replies carry no sequence number and are matched on the command and key.
//! Writing the sidetone-enabled parameter toggles it whatever is sent, so it is
//! read first and written only to change state, and never retried.
//!
//! The headset keeps its settings itself, as with its own buttons and app, so
//! they are written only when the user changes one, never when the headset
//! comes online. Then the sidetone is read back (level key, enabled parameter,
//! the reads HeadsetControl uses); voice prompts, noise filter, volume limiter
//! and auto-off have no documented read and show the last value set here.
//!
//! The original Maxwell's custom EQ (`audeze_maxwell.hpp`: stored items
//! `EF00 + slot` / `E42C + slot` via `0D 0A`, after `03 0A`) rewrites the
//! headset's stored EQ slots. Only HeadsetControl documents that layout, so
//! the ten bands are shown blocked (see `docs/onboard-writes/audeze.md`); no
//! source has the Maxwell 2's.

use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{enumerate_hidraw, usage_pages, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey as K, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const SIZE: usize = 62;
const REPORT_OUT: u8 = 0x06;
const REPORT_IN: u8 = 0x07;
const ROUTE_HEADSET: u8 = 0x80;
/// Fragment bytes per report (after id, length and route).
const FRAGMENT_MAX: usize = SIZE - 3;

const MARKER: u8 = 0x05;
const REQUEST: u8 = 0x5a;
const RESPONSE: u8 = 0x5b;
const RESULT: u8 = 0x5d;
const MESSAGE_MAX: usize = 1024;

const WRITE_KEY: [u8; 2] = [0x00, 0x09];
const READ_KEY: [u8; 2] = [0x01, 0x09];
const WRITE_PARAM: [u8; 2] = [0x82, 0x2c];
const READ_PARAM: [u8; 2] = [0x83, 0x2c];
const BATTERY: [u8; 2] = [0xd6, 0x0c];

const KEY_EQ_PRESET: u8 = 0x00;
const KEY_VOICE_PROMPTS: u8 = 0x20;
const KEY_MIC_MUTE: u8 = 0x22;
const KEY_NOISE_FILTER: u8 = 0x24;
const KEY_VOLUME_LIMITER: u8 = 0x28;
const KEY_SIDETONE: u8 = 0x2c;
const KEY_SIDETONE_MAX: u8 = 0x2d;

const PARAM_AUTO_OFF: u8 = 0x01;
const PARAM_SIDETONE_ON: u8 = 0x07;
const PARAM_CHATMIX: u8 = 0x0b;

/// Used when the headset does not report its own maximum.
const SIDETONE_MAX_DEFAULT: u8 = 31;
const VOICE_PROMPTS_MAX: u8 = 15;
const LIMITER_ON: u8 = 0x88;
const LIMITER_OFF: u8 = 0x8e;
/// Auto power off choices in minutes; 0 = never.
const OFF_MINUTES: [u16; 11] = [0, 5, 10, 15, 30, 45, 60, 90, 120, 240, 360];
/// Custom EQ band centres and gain range (HeadsetControl `audeze_maxwell.hpp`).
const EQ_BANDS: [(K, &str); 10] = [
    (K::EqBand1, "32 Hz"),
    (K::EqBand2, "64 Hz"),
    (K::EqBand3, "125 Hz"),
    (K::EqBand4, "250 Hz"),
    (K::EqBand5, "500 Hz"),
    (K::EqBand6, "1 kHz"),
    (K::EqBand7, "2 kHz"),
    (K::EqBand8, "4 kHz"),
    (K::EqBand9, "8 kHz"),
    (K::EqBand10, "16 kHz"),
];
const EQ_GAIN_MAX: f64 = 12.0;
const EQ_BLOCKED: &str = "Writes the headset's stored custom EQ slot; only one source \
(HeadsetControl, Oct 2026) documents the layout";
/// Headset preset numbers 1-10 (HeadsetControl's 0-9 plus one).
const EQ_PRESETS: [&str; 10] = [
    "Audeze",
    "Treble Boost",
    "Bass Boost",
    "Immersive",
    "Competition",
    "Footsteps",
    "EQ1",
    "EQ2",
    "EQ3",
    "EQ4",
];

pub(super) struct Spec {
    pub name: &'static str,
    pub pids: &'static [u16],
    /// Interface of the 0xFF13 control collection (HeadsetControl).
    iface: u32,
    volume_limiter: bool,
    /// Custom EQ storage documented (blocked placeholder).
    custom_eq: bool,
}

pub(super) const MAXWELL: Spec = Spec {
    name: "Audeze Maxwell",
    // PlayStation/PC, Xbox dongle.
    pids: &[0x4b19, 0x4b18],
    iface: 0,
    volume_limiter: true,
    custom_eq: true,
};
pub(super) const MAXWELL_2: Spec = Spec {
    name: "Audeze Maxwell 2",
    pids: &[0x4b29, 0x4b28],
    iface: 5,
    volume_limiter: false,
    custom_eq: false,
};

// ── Messages ────────────────────────────────────────────────────────────────

/// One output report carrying a request. Every body sent here is a few bytes,
/// well under one report's fragment.
fn request_report(body: &[u8]) -> [u8; SIZE] {
    let mut msg = vec![MARKER, REQUEST];
    msg.extend((body.len() as u16).to_le_bytes());
    msg.extend_from_slice(body);
    let mut r = [0u8; SIZE];
    r[..3].copy_from_slice(&[REPORT_OUT, msg.len() as u8, ROUTE_HEADSET]);
    r[3..3 + msg.len()].copy_from_slice(&msg);
    r
}

/// The valid fragment of an input report; empty if it carries nothing new.
fn fragment(r: &[u8]) -> &[u8] {
    let Some(&len) = r.get(1) else { return &[] };
    let len = usize::from(len);
    if len > FRAGMENT_MAX || 3 + len > r.len() {
        return &[];
    }
    &r[3..3 + len]
}

/// Take the next complete message off the front of `stream`, dropping bytes
/// that cannot start one.
fn take_message(stream: &mut Vec<u8>) -> Option<Vec<u8>> {
    while let Some(&first) = stream.first() {
        if first != MARKER {
            stream.remove(0);
            continue;
        }
        let len = u16::from_le_bytes([*stream.get(2)?, *stream.get(3)?]);
        let size = 4 + usize::from(len);
        if size > MESSAGE_MAX {
            stream.remove(0);
            continue;
        }
        if stream.len() < size {
            return None;
        }
        return Some(stream.drain(..size).collect());
    }
    None
}

fn is(m: &[u8], kind: u8, cmd: [u8; 2]) -> bool {
    m.len() >= 6 && m[1] == kind && m[4..6] == cmd
}

/// `id` (a key or parameter, 16-bit LE) at `offset`.
fn has_id(m: &[u8], offset: usize, id: u8) -> bool {
    m.get(offset..offset + 2) == Some(&[id, 0x00])
}

fn read_key_body(key: u8) -> Vec<u8> {
    vec![READ_KEY[0], READ_KEY[1], key, 0x00]
}

fn write_key_body(key: u8, value: u8) -> Vec<u8> {
    vec![WRITE_KEY[0], WRITE_KEY[1], key, 0x00, value]
}

fn read_param_body(param: u8) -> Vec<u8> {
    vec![READ_PARAM[0], READ_PARAM[1], param, 0x00]
}

fn write_param_body(param: u8, value: &[u8]) -> Vec<u8> {
    let mut b = vec![WRITE_PARAM[0], WRITE_PARAM[1], param, 0x00];
    b.extend_from_slice(value);
    b
}

/// Value of a successful key read.
fn key_value(m: &[u8], key: u8) -> Option<u8> {
    (is(m, RESPONSE, READ_KEY) && has_id(m, 6, key) && m.get(8) == Some(&0))
        .then(|| m.get(9))?
        .copied()
}

/// Value of a successful parameter read.
fn param_value(m: &[u8], param: u8) -> Option<u8> {
    (is(m, RESPONSE, READ_PARAM) && has_id(m, 7, param) && m.get(6) == Some(&0))
        .then(|| m.get(9))?
        .copied()
}

/// Battery percent from the asynchronous result.
fn battery_percent(m: &[u8]) -> Option<u8> {
    is(m, RESULT, BATTERY)
        .then(|| m.get(8))?
        .map(|&p| p.min(100))
}

/// Auto power off: enable flag and seconds (LE), sent twice.
fn auto_off_value(minutes: u16) -> [u8; 8] {
    let en = u8::from(minutes > 0);
    let [lo, hi] = minutes.saturating_mul(60).to_le_bytes();
    [en, 0, lo, hi, en, 0, lo, hi]
}

/// Raw chat mix 0 (all chat) ..10 (centre).. 20 (all game) as game / chat levels.
fn chatmix(raw: u8) -> (f64, f64) {
    let raw = raw.min(20);
    (
        f64::from(raw.min(10)) / 10.0,
        f64::from((20 - raw).min(10)) / 10.0,
    )
}

#[derive(Debug, PartialEq)]
enum Write {
    Key(u8, u8),
    Param(u8, Vec<u8>),
    /// 0 = off, else switch on and set the level.
    Sidetone(u8),
}

fn plan(spec: &Spec, key: K, value: &SettingValue) -> Result<Write> {
    let n = value.as_int().unwrap_or(0).clamp(0, 0xffff);
    let byte = n.min(255) as u8;
    Ok(match key {
        K::EqPreset => Write::Key(KEY_EQ_PRESET, byte),
        K::VoicePrompts => Write::Key(KEY_VOICE_PROMPTS, byte),
        K::MicNoiseFilter => Write::Key(KEY_NOISE_FILTER, byte),
        K::VolumeLimiter if spec.volume_limiter => Write::Key(
            KEY_VOLUME_LIMITER,
            if value.as_bool() == Some(true) {
                LIMITER_ON
            } else {
                LIMITER_OFF
            },
        ),
        K::PmShutdown => Write::Param(PARAM_AUTO_OFF, auto_off_value(n as u16).to_vec()),
        K::MicSideTone => Write::Sidetone(byte),
        _ => bail!("unsupported setting {}", key.as_str()),
    })
}

fn controls(spec: &Spec, sidetone_max: u8) -> Vec<SettingMeta> {
    let mut c = vec![
        SettingMeta::readonly(K::ChatMixGame, "Game", 0.0, 1.0),
        SettingMeta::readonly(K::ChatMixChat, "Chat", 0.0, 1.0),
        SettingMeta::dropdown(
            K::EqPreset,
            "EQ preset",
            EQ_PRESETS
                .iter()
                .zip(1..)
                .map(|(l, v)| SettingOption::int(v, l))
                .collect(),
        ),
    ];
    if spec.custom_eq {
        c.extend(EQ_BANDS.iter().map(|&(k, label)| {
            SettingMeta::slider(k, label, -EQ_GAIN_MAX, EQ_GAIN_MAX, 1.0)
                .unit("dB")
                .persistent()
                .blocked(EQ_BLOCKED)
        }));
    }
    if spec.volume_limiter {
        c.push(SettingMeta::toggle(K::VolumeLimiter, "Volume limiter"));
    }
    c.extend([
        SettingMeta::slider(
            K::VoicePrompts,
            "Voice prompt level",
            0.0,
            f64::from(VOICE_PROMPTS_MAX),
            1.0,
        ),
        SettingMeta::slider(
            K::MicSideTone,
            "Sidetone",
            0.0,
            f64::from(sidetone_max),
            1.0,
        ),
        SettingMeta::dropdown(
            K::MicNoiseFilter,
            "Noise filter",
            vec![
                SettingOption::int(0, "Off"),
                SettingOption::int(1, "Low"),
                SettingOption::int(2, "High"),
            ],
        ),
        SettingMeta::readonly(K::MicMute, "Muted", 0.0, 1.0),
        SettingMeta::dropdown(
            K::PmShutdown,
            "Auto-off",
            OFF_MINUTES
                .iter()
                .map(|&m| match m {
                    0 => SettingOption::int(0, "Never"),
                    m => SettingOption::int(i64::from(m), &format!("After {m} min")),
                })
                .collect(),
        ),
    ]);
    c
}

// ── Adapter ─────────────────────────────────────────────────────────────────

const POLL: Duration = Duration::from_secs(10);
/// Audeze's app waits 50 ms before each input report read, 25 reads at most.
const READ_GAP: Duration = Duration::from_millis(50);
const REPLY_WAIT: Duration = Duration::from_millis(1250);
/// Reads are repeated on a timeout; writes never are.
const READ_ATTEMPTS: u32 = 2;

/// The 0xFF13 collection's node, on the documented interface if several have one.
fn find_node(spec: &Spec, pid: u16) -> Result<HidDeviceInfo> {
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == super::AUDEZE && d.product_id == pid)
        .filter(|d| usage_pages(&d.report_descriptor()).contains(&0xff13))
        .collect();
    nodes.sort_by_key(|d| d.interface != Some(spec.iface));
    nodes
        .into_iter()
        .next()
        .with_context(|| format!("no control node for {:04x}:{pid:04x}", super::AUDEZE))
}

pub(super) fn open(spec: &'static Spec, pid: u16, ctx: &DriverCtx) -> Opened {
    let info = find_node(spec, pid)?;
    let dongle = Dongle::open(&info)?;
    info!("{} open via {}", spec.name, info.path.display());
    let mut a = MaxwellAdapter {
        spec,
        id: DeviceId {
            vendor_id: super::AUDEZE,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        link: Some(Box::new(dongle)),
        settings: hako_core::config::load_device_settings(&ctx.paths, super::AUDEZE, spec.pids[0])?,
        battery: BatteryState {
            label: "Headset".into(),
            percent: None,
            charging: false,
            present: false,
        },
        sidetone_max: SIDETONE_MAX_DEFAULT,
        last_poll: None,
        failures: 0,
        last_error: None,
        paths: ctx.paths.clone(),
    };
    if let Err(e) = a.poll() {
        warn!("{} status: {e}", spec.name);
        a.last_error = Some(e.to_string());
    }
    Ok(vec![Box::new(a)])
}

/// HIDIOCGINPUT; `buf[0]` is the report id. Returns the length read.
fn get_input_report(file: &File, buf: &mut [u8]) -> Result<usize> {
    let req = crate::hid::hidioc(0x0a, buf.len());
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), req, buf.as_mut_ptr()) };
    if ret < 0 {
        bail!("HIDIOCGINPUT: {}", std::io::Error::last_os_error());
    }
    Ok((ret as usize).min(buf.len()))
}

/// Report transport, so the protocol logic can be tested without hardware.
trait Link: Send {
    fn send(&mut self, report: &[u8]) -> Result<()>;
    /// One input report (`buf[0]` = report id); returns the length read.
    fn input(&mut self, buf: &mut [u8]) -> Result<usize>;
}

struct Dongle {
    dev: HidrawDevice,
    /// Second handle for HIDIOCGINPUT (replies are fetched, not pushed).
    input: File,
}

impl Dongle {
    fn open(info: &HidDeviceInfo) -> Result<Self> {
        Ok(Self {
            dev: HidrawDevice::open(&info.path)?,
            input: OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&info.path)?,
        })
    }
}

impl Link for Dongle {
    fn send(&mut self, report: &[u8]) -> Result<()> {
        self.dev.write(report)
    }

    fn input(&mut self, buf: &mut [u8]) -> Result<usize> {
        get_input_report(&self.input, buf)
    }
}

struct MaxwellAdapter {
    spec: &'static Spec,
    id: DeviceId,
    link: Option<Box<dyn Link>>,
    settings: SettingsMap,
    battery: BatteryState,
    sidetone_max: u8,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl MaxwellAdapter {
    /// Send `body` and return the first message `accept` takes.
    fn request(
        &mut self,
        body: &[u8],
        attempts: u32,
        accept: impl Fn(&[u8]) -> bool,
    ) -> Result<Option<Vec<u8>>> {
        let report = request_report(body);
        let link = self.link.as_deref_mut().context("not connected")?;
        for _ in 0..attempts {
            link.send(&report)?;
            let deadline = Instant::now() + REPLY_WAIT;
            let mut stream = Vec::new();
            loop {
                while let Some(m) = take_message(&mut stream) {
                    if accept(&m) {
                        return Ok(Some(m));
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(READ_GAP);
                let mut buf = [0u8; SIZE];
                buf[0] = REPORT_IN;
                let n = link.input(&mut buf)?;
                stream.extend_from_slice(fragment(&buf[..n]));
            }
        }
        Ok(None)
    }

    fn read_key(&mut self, key: u8) -> Result<Option<u8>> {
        let accept = |m: &[u8]| is(m, RESPONSE, READ_KEY) && has_id(m, 6, key);
        Ok(self
            .request(&read_key_body(key), READ_ATTEMPTS, accept)?
            .and_then(|m| key_value(&m, key)))
    }

    fn read_param(&mut self, param: u8) -> Result<Option<u8>> {
        let accept = |m: &[u8]| is(m, RESPONSE, READ_PARAM) && has_id(m, 7, param);
        Ok(self
            .request(&read_param_body(param), READ_ATTEMPTS, accept)?
            .and_then(|m| param_value(&m, param)))
    }

    fn write_key(&mut self, key: u8, value: u8) -> Result<()> {
        let accept = |m: &[u8]| is(m, RESPONSE, WRITE_KEY) && has_id(m, 6, key);
        if self
            .request(&write_key_body(key, value), 1, accept)?
            .is_none()
        {
            bail!("headset did not acknowledge key {key:#04x}");
        }
        Ok(())
    }

    fn write_param(&mut self, param: u8, value: &[u8]) -> Result<()> {
        let accept = |m: &[u8]| is(m, RESPONSE, WRITE_PARAM) && has_id(m, 7, param);
        match self.request(&write_param_body(param, value), 1, accept)? {
            Some(m) if m.get(6) == Some(&0) => Ok(()),
            Some(_) => bail!("headset rejected parameter {param:#04x}"),
            None => bail!("headset did not acknowledge parameter {param:#04x}"),
        }
    }

    fn write(&mut self, key: K, value: &SettingValue) -> Result<()> {
        match plan(self.spec, key, value)? {
            Write::Key(k, v) => self.write_key(k, v),
            Write::Param(p, v) => self.write_param(p, &v),
            Write::Sidetone(level) => {
                // Switch on first, then the level: writing the level first
                // switches an original Maxwell on and the toggle off again.
                let on = level > 0;
                let state = self
                    .read_param(PARAM_SIDETONE_ON)?
                    .context("no sidetone state from the headset")?;
                if (state != 0) != on {
                    self.write_param(PARAM_SIDETONE_ON, &[u8::from(on)])?;
                }
                if on {
                    self.write_key(KEY_SIDETONE, level)?;
                }
                Ok(())
            }
        }
    }

    /// None = no answer (the headset is off) or the dongle refused.
    fn read_battery(&mut self) -> Result<Option<u8>> {
        let accept = |m: &[u8]| {
            battery_percent(m).is_some()
                || (is(m, RESPONSE, BATTERY) && m.get(6).is_some_and(|&s| s != 0))
        };
        Ok(self
            .request(&[BATTERY[0], BATTERY[1], 0x00], READ_ATTEMPTS, accept)?
            .and_then(|m| battery_percent(&m)))
    }

    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        let was_online = self.battery.present;
        let Some(pct) = self.read_battery()? else {
            (self.battery.percent, self.battery.present) = (None, false);
            return Ok(());
        };
        self.battery.percent = Some(pct);
        if !was_online {
            // Present only once this worked, so a failed read is retried.
            self.read_sidetone()?;
        }
        self.battery.present = true;
        if let Some(m) = self.read_key(KEY_MIC_MUTE)? {
            // 00 while muted, FF otherwise.
            merge_setting(&mut self.settings, K::MicMute, SettingValue::Bool(m == 0));
        }
        if let Some(raw) = self.read_param(PARAM_CHATMIX)? {
            let (game, chat) = chatmix(raw);
            merge_setting(
                &mut self.settings,
                K::ChatMixGame,
                SettingValue::Float(game),
            );
            merge_setting(
                &mut self.settings,
                K::ChatMixChat,
                SettingValue::Float(chat),
            );
        }
        if let Some(p) = self
            .read_key(KEY_EQ_PRESET)?
            .filter(|p| (1..=10).contains(p))
        {
            merge_setting(
                &mut self.settings,
                K::EqPreset,
                SettingValue::Int(i64::from(p)),
            );
        }
        debug!("{} battery {:?}", self.spec.name, self.battery);
        Ok(())
    }

    /// The headset's own sidetone: 0 while switched off, else its level.
    fn read_sidetone(&mut self) -> Result<()> {
        let max = self
            .read_key(KEY_SIDETONE_MAX)?
            .filter(|&m| m > 0)
            .unwrap_or(SIDETONE_MAX_DEFAULT);
        let level = self.read_key(KEY_SIDETONE)?;
        let on = self.read_param(PARAM_SIDETONE_ON)?;
        self.sidetone_max = max.max(level.unwrap_or(0));
        if let (Some(level), Some(on)) = (level, on) {
            let shown = if on == 0 { 0 } else { level };
            merge_setting(
                &mut self.settings,
                K::MicSideTone,
                SettingValue::Int(i64::from(shown)),
            );
        }
        Ok(())
    }
}

impl DeviceAdapter for MaxwellAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.link.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: controls(self.spec, self.sidetone_max),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: K, value: SettingValue) -> Result<()> {
        self.write(key, &value)?;
        merge_setting(&mut self.settings, key, value);
        hako_core::config::save_device_settings(
            &self.paths,
            super::AUDEZE,
            self.spec.pids[0],
            &self.settings,
        )
        .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn tick(&mut self) -> Result<()> {
        if self.link.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
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
                    self.link = None;
                }
                Err(e)
            }
        }
    }

    fn shutdown(&mut self) {
        self.link = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reply message as the dongle frames it.
    fn msg(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut m = vec![MARKER, kind];
        m.extend((body.len() as u16).to_le_bytes());
        m.extend_from_slice(body);
        m
    }

    #[test]
    fn requests_match_headsetcontrol_framing() {
        let r = request_report(&read_key_body(KEY_MIC_MUTE));
        assert_eq!(r.len(), 62);
        assert_eq!(
            r[..11],
            [0x06, 8, 0x80, 0x05, 0x5a, 4, 0, 0x01, 0x09, 0x22, 0x00]
        );
        assert!(r[11..].iter().all(|&b| b == 0));
        assert_eq!(
            request_report(&write_key_body(KEY_VOLUME_LIMITER, LIMITER_ON))[..12],
            [0x06, 9, 0x80, 0x05, 0x5a, 5, 0, 0x00, 0x09, 0x28, 0x00, 0x88]
        );
        assert_eq!(read_param_body(PARAM_CHATMIX), [0x83, 0x2c, 0x0b, 0x00]);
        // 30 min = 1800 s = 0x0708, enable flag and seconds twice.
        assert_eq!(
            write_param_body(PARAM_AUTO_OFF, &auto_off_value(30)),
            [0x82, 0x2c, 0x01, 0x00, 1, 0, 0x08, 0x07, 1, 0, 0x08, 0x07]
        );
        assert_eq!(auto_off_value(0), [0; 8]);
        // 6 h = 21600 s = 0x5460.
        assert_eq!(auto_off_value(360)[2..4], [0x60, 0x54]);
    }

    #[test]
    fn replies_are_reassembled_and_matched() {
        // 05 5B 06 00 01 09 22 00 <status 00> <value 00 = muted>, split across two reports.
        let reply = msg(RESPONSE, &[0x01, 0x09, 0x22, 0x00, 0x00, 0x00]);
        let mut a = [0u8; SIZE];
        a[..3].copy_from_slice(&[REPORT_IN, 5, 0x80]);
        a[3] = 0xaa; // junk before the marker is dropped
        a[4..8].copy_from_slice(&reply[..4]);
        let mut b = [0u8; SIZE];
        b[..3].copy_from_slice(&[REPORT_IN, (reply.len() - 4) as u8, 0x80]);
        b[3..3 + reply.len() - 4].copy_from_slice(&reply[4..]);
        let mut stream = fragment(&a).to_vec();
        assert_eq!(take_message(&mut stream), None);
        stream.extend_from_slice(fragment(&b));
        let m = take_message(&mut stream).unwrap();
        assert_eq!(m, reply);
        assert_eq!(key_value(&m, KEY_MIC_MUTE), Some(0));
        assert_eq!(key_value(&m, KEY_EQ_PRESET), None);
        // Length 0 = nothing new; an oversized length is ignored.
        assert!(fragment(&[REPORT_IN, 0, 0x80, 0x05, 0x5b]).is_empty());
        assert!(fragment(&[REPORT_IN, 60, 0x80]).is_empty());

        let rejected = msg(RESPONSE, &[0x01, 0x09, 0x22, 0x00, 0x01, 0xff]);
        assert_eq!(key_value(&rejected, KEY_MIC_MUTE), None);
        let p = msg(RESPONSE, &[0x83, 0x2c, 0x00, 0x0b, 0x00, 14]);
        assert_eq!(param_value(&p, PARAM_CHATMIX), Some(14));
        assert_eq!(param_value(&p, PARAM_AUTO_OFF), None);
        assert_eq!(
            battery_percent(&msg(RESULT, &[0xd6, 0x0c, 0, 0, 64])),
            Some(64)
        );
        assert_eq!(battery_percent(&msg(RESPONSE, &[0xd6, 0x0c, 0])), None);
    }

    #[test]
    fn chatmix_follows_headsetcontrol_split() {
        assert_eq!(chatmix(0), (0.0, 1.0));
        assert_eq!(chatmix(10), (1.0, 1.0));
        assert_eq!(chatmix(15), (1.0, 0.5));
        assert_eq!(chatmix(20), (1.0, 0.0));
        assert_eq!(chatmix(200), (1.0, 0.0));
    }

    #[test]
    fn writes_per_setting() {
        let int = SettingValue::Int;
        assert_eq!(
            plan(&MAXWELL, K::EqPreset, &int(7)).unwrap(),
            Write::Key(0x00, 7)
        );
        assert_eq!(
            plan(&MAXWELL, K::MicNoiseFilter, &int(2)).unwrap(),
            Write::Key(0x24, 2)
        );
        assert_eq!(
            plan(&MAXWELL, K::VolumeLimiter, &SettingValue::Bool(false)).unwrap(),
            Write::Key(0x28, 0x8e)
        );
        assert!(plan(&MAXWELL_2, K::VolumeLimiter, &SettingValue::Bool(true)).is_err());
        assert_eq!(
            plan(&MAXWELL_2, K::MicSideTone, &int(0)).unwrap(),
            Write::Sidetone(0)
        );
        assert!(plan(&MAXWELL, K::Volume, &int(1)).is_err());
    }

    #[test]
    fn every_control_validates_and_plans() {
        for spec in [&MAXWELL, &MAXWELL_2] {
            for meta in controls(spec, SIDETONE_MAX_DEFAULT)
                .iter()
                .filter(|m| !m.read_only && m.blocked.is_none())
            {
                let values: Vec<SettingValue> = match meta.kind {
                    hako_core::catalog::ControlKind::Toggle => {
                        vec![SettingValue::Bool(false), SettingValue::Bool(true)]
                    }
                    hako_core::catalog::ControlKind::Dropdown => {
                        meta.options.iter().map(|o| o.value.clone()).collect()
                    }
                    _ => vec![
                        SettingValue::Int(meta.min.unwrap() as i64),
                        SettingValue::Int(meta.max.unwrap() as i64),
                    ],
                };
                for v in values {
                    assert!(meta.validate(&v).is_ok(), "{} {:?}", spec.name, meta.key);
                    assert!(
                        plan(spec, meta.key, &v).is_ok(),
                        "{} {:?}",
                        spec.name,
                        meta.key
                    );
                }
            }
        }
        assert!(MAXWELL.pids.iter().all(|p| !MAXWELL_2.pids.contains(p)));
    }

    #[test]
    fn custom_eq_is_a_blocked_memory_write() {
        let eq = |spec| {
            controls(spec, SIDETONE_MAX_DEFAULT)
                .into_iter()
                .filter(|m| m.persistent)
                .collect::<Vec<_>>()
        };
        let bands = eq(&MAXWELL);
        assert_eq!(bands.len(), 10);
        assert!(bands
            .iter()
            .all(|m| m.blocked.is_some() && m.min == Some(-12.0) && m.max == Some(12.0)));
        assert_eq!(bands[0].key, K::EqBand1);
        assert!(eq(&MAXWELL_2).is_empty());
        // Never planned as a write.
        assert!(plan(&MAXWELL, K::EqBand1, &SettingValue::Int(3)).is_err());
    }

    use hako_core::config::get_setting;

    type Sent = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

    /// Dongle double answering like the headset: keys and parameters from its
    /// maps; writes are recorded as request bodies and stored.
    struct FakeHeadset {
        keys: std::collections::HashMap<u8, u8>,
        params: std::collections::HashMap<u8, u8>,
        sent: Sent,
        replies: std::collections::VecDeque<[u8; SIZE]>,
        /// The next read of this key fails (a dropped link).
        fail_key: Option<u8>,
    }

    impl Link for FakeHeadset {
        fn send(&mut self, r: &[u8]) -> Result<()> {
            let body = fragment(r)[4..].to_vec();
            if (body[0], body[1]) == (0x01, 0x09) && self.fail_key == Some(body[2]) {
                self.fail_key = None;
                anyhow::bail!("link dropped");
            }
            self.sent.lock().unwrap().push(body.clone());
            let (kind, reply) = match (body[0], body[1]) {
                (0x01, 0x09) => {
                    let v = self.keys.get(&body[2]).copied().unwrap_or(0);
                    (RESPONSE, vec![0x01, 0x09, body[2], 0, 0, v])
                }
                (0x00, 0x09) => {
                    self.keys.insert(body[2], body[4]);
                    (RESPONSE, vec![0x00, 0x09, body[2], 0, 0])
                }
                (0x83, 0x2c) => {
                    let v = self.params.get(&body[2]).copied().unwrap_or(0);
                    (RESPONSE, vec![0x83, 0x2c, 0, body[2], 0, v])
                }
                (0x82, 0x2c) => {
                    if body[2] == PARAM_SIDETONE_ON {
                        // Toggles whatever is sent.
                        let on = self.params.entry(body[2]).or_default();
                        *on = u8::from(*on == 0);
                    }
                    (RESPONSE, vec![0x82, 0x2c, 0, body[2], 0])
                }
                _ => (RESULT, vec![0xd6, 0x0c, 0, 0, 80]),
            };
            let m = msg(kind, &reply);
            let mut rep = [0u8; SIZE];
            rep[..3].copy_from_slice(&[REPORT_IN, m.len() as u8, ROUTE_HEADSET]);
            rep[3..3 + m.len()].copy_from_slice(&m);
            self.replies.push_back(rep);
            Ok(())
        }

        fn input(&mut self, buf: &mut [u8]) -> Result<usize> {
            match self.replies.pop_front() {
                Some(r) => buf.copy_from_slice(&r),
                None => buf[1..].fill(0),
            }
            Ok(buf.len())
        }
    }

    fn headset(spec: &'static Spec, saved: SettingsMap, sidetone_on: u8) -> (MaxwellAdapter, Sent) {
        let dir = std::env::temp_dir().join(format!("hako-maxwell-{}", std::process::id()));
        let sent = Sent::default();
        let fake = FakeHeadset {
            keys: [
                (KEY_SIDETONE, 9),
                (KEY_SIDETONE_MAX, 31),
                (KEY_EQ_PRESET, 2),
            ]
            .into(),
            params: [(PARAM_SIDETONE_ON, sidetone_on), (PARAM_CHATMIX, 10)].into(),
            sent: sent.clone(),
            replies: Default::default(),
            fail_key: None,
        };
        let a = MaxwellAdapter {
            spec,
            id: DeviceId {
                vendor_id: super::super::AUDEZE,
                product_id: spec.pids[0],
                instance: "test".into(),
            },
            link: Some(Box::new(fake)),
            settings: saved,
            battery: BatteryState {
                label: "Headset".into(),
                percent: None,
                charging: false,
                present: false,
            },
            sidetone_max: SIDETONE_MAX_DEFAULT,
            last_poll: None,
            failures: 0,
            last_error: None,
            paths: ConfigPaths {
                root: dir.clone(),
                config_toml: dir.join("config.toml"),
                devices_dir: dir.join("devices"),
                profiles_dir: dir.join("profiles"),
                remaps_dir: dir.join("remaps"),
            },
        };
        (a, sent)
    }

    fn writes(sent: &Sent) -> Vec<Vec<u8>> {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|b| b[..2] == WRITE_KEY || b[..2] == WRITE_PARAM)
            .cloned()
            .collect()
    }

    #[test]
    fn coming_online_reads_back_and_never_writes() {
        let mut saved = SettingsMap::new();
        for (k, v) in [
            (K::MicSideTone, SettingValue::Int(20)),
            (K::VoicePrompts, SettingValue::Int(3)),
            (K::MicNoiseFilter, SettingValue::Int(2)),
            (K::VolumeLimiter, SettingValue::Bool(true)),
            (K::PmShutdown, SettingValue::Int(30)),
        ] {
            merge_setting(&mut saved, k, v);
        }
        for spec in [&MAXWELL, &MAXWELL_2] {
            let (mut a, sent) = headset(spec, saved.clone(), 1);
            a.poll().unwrap();
            assert!(a.battery.present);
            a.poll().unwrap();
            assert!(writes(&sent).is_empty(), "{:02x?}", writes(&sent));
            let got = |k| get_setting(&a.settings, k).cloned();
            // Sidetone read back from the headset...
            assert_eq!(got(K::MicSideTone), Some(SettingValue::Int(9)));
            // ...the rest have no read: the user's last value, not re-sent.
            assert_eq!(got(K::VoicePrompts), Some(SettingValue::Int(3)));
            assert_eq!(got(K::MicNoiseFilter), Some(SettingValue::Int(2)));
            assert_eq!(got(K::PmShutdown), Some(SettingValue::Int(30)));
            assert_eq!(got(K::EqPreset), Some(SettingValue::Int(2)));
        }
        // Sidetone switched off on the headset reads as 0.
        let (mut a, sent) = headset(&MAXWELL, saved, 0);
        a.poll().unwrap();
        assert!(writes(&sent).is_empty());
        assert_eq!(
            get_setting(&a.settings, K::MicSideTone),
            Some(&SettingValue::Int(0))
        );
    }

    #[test]
    fn a_failed_sidetone_read_is_retried() {
        let (mut a, sent) = headset(&MAXWELL, SettingsMap::new(), 1);
        let fake = FakeHeadset {
            keys: [(KEY_SIDETONE, 9), (KEY_SIDETONE_MAX, 31)].into(),
            params: [(PARAM_SIDETONE_ON, 1)].into(),
            sent,
            replies: Default::default(),
            fail_key: Some(KEY_SIDETONE_MAX),
        };
        a.link = Some(Box::new(fake));
        assert!(a.poll().is_err());
        assert!(!a.battery.present);
        assert_eq!(get_setting(&a.settings, K::MicSideTone), None);
        // The next poll reads it again, now successfully.
        a.poll().unwrap();
        assert!(a.battery.present);
        assert_eq!(a.sidetone_max, 31);
        assert_eq!(
            get_setting(&a.settings, K::MicSideTone),
            Some(&SettingValue::Int(9))
        );
    }

    #[test]
    fn user_changes_are_still_written() {
        let (mut a, sent) = headset(&MAXWELL, SettingsMap::new(), 0);
        a.poll().unwrap();
        a.set(K::MicNoiseFilter, SettingValue::Int(1)).unwrap();
        a.set(K::MicSideTone, SettingValue::Int(5)).unwrap();
        assert_eq!(
            writes(&sent),
            [
                vec![0x00, 0x09, KEY_NOISE_FILTER, 0x00, 1],
                vec![0x82, 0x2c, PARAM_SIDETONE_ON, 0x00, 1],
                vec![0x00, 0x09, KEY_SIDETONE, 0x00, 5],
            ]
        );
        let _ = std::fs::remove_dir_all(&a.paths.root);
    }

    /// Read-only probe of a plugged-in dongle (battery, mute, chat mix, preset).
    #[test]
    #[ignore]
    fn hw_poll_status() {
        let paths = ConfigPaths::xdg().unwrap();
        let ctx_paths = paths.clone();
        for spec in [&MAXWELL, &MAXWELL_2] {
            for &pid in spec.pids {
                let Ok(info) = find_node(spec, pid) else {
                    continue;
                };
                let mut a = MaxwellAdapter {
                    spec,
                    id: DeviceId {
                        vendor_id: super::super::AUDEZE,
                        product_id: pid,
                        instance: String::new(),
                    },
                    link: Some(Box::new(Dongle::open(&info).unwrap())),
                    settings: SettingsMap::new(),
                    battery: BatteryState {
                        label: "Headset".into(),
                        percent: None,
                        charging: false,
                        present: true,
                    },
                    sidetone_max: SIDETONE_MAX_DEFAULT,
                    last_poll: None,
                    failures: 0,
                    last_error: None,
                    paths: ctx_paths.clone(),
                };
                a.poll().unwrap();
                eprintln!("{} {:?} {:?}", spec.name, a.battery, a.settings);
            }
        }
    }
}
