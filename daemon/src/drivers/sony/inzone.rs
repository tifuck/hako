//! Sony INZONE 2.4 GHz dongles: Sony vendor commands carried in Bluetooth HCI
//! packets on the usage-page 0xFF04 collection (report 0x02, 64 bytes).
//!
//! Command (report id first): `02 <12+n> 01 00 fc <8+n> 96 c3 <dst<<4|src> <event id>
//! <type> <tid lo> <tid hi> <payload n> <checksum>`; events come back as
//! `02 <12+n> 04 ff <9+n> 00 96 c3 ...` in the same shape. The checksum is the low
//! byte of the sum from the key id (`96`, commands) or the reserved `00` (events)
//! through the payload. A GET is answered by RET, a SET by NTFY, both echoing the
//! transaction id; changes made on the headset arrive unsolicited as NTFY or
//! NTFY_ACTIVE. Sources: HeadsetControl `sony_inzone_protocol.hpp` (framing, event
//! ids, payloads), zoneout `device.py` (`_get_report` / `_send_cmd` byte layouts).
//!
//! Value ranges: volume 0-30, sidetone 0-10 and a 0-100 game/chat balance were
//! verified on the H9 II and Buds (zoneout `SPECS.md`, OpenInzone `PROTOCOL.md`).
//! HeadsetControl assumes 0-50 for sidetone and reads the balance as 0-90; the
//! narrower verified ranges are used for every model, the H5 balance follows
//! HeadsetControl (its only source). The balance is set with `22` SET `[value]`,
//! 0 = all game, 100 = all chat (zoneout `SPECS.md` write table, OpenInzone
//! `PROTOCOL.md` worked example); offered on the H9 II and Buds only.
//!
//! The headset keeps its settings itself (Sony's H9 II help guide: kept through
//! a reset until factory initialisation), so they are written only when the
//! user changes one. When the headset comes online each one is asked for with
//! a GET; one that is not answered shows the last value set here, unsent.

use super::open_node;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey as K, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const REPORT_ID: u8 = 0x02;
const LEN: usize = 64;
/// PC (1) to headset (4).
const PC_TO_HEADSET: u8 = 0x41;

const GET: u8 = 0x01;
const SET: u8 = 0x02;
const RET: u8 = 0x10;
const NTFY: u8 = 0x20;
const NTFY_ACTIVE: u8 = 0xa0;

const EID_BATTERY: u8 = 0x04;
const EID_VOLUME: u8 = 0x21;
const EID_BALANCE: u8 = 0x22;
const EID_SIDETONE: u8 = 0x23;
const EID_MIC: u8 = 0x24;
const EID_AMBIENT: u8 = 0x41;
const EID_NC_TOGGLE: u8 = 0x42;
const EID_NC_STARTUP: u8 = 0x43;
const EID_BT_STARTUP: u8 = 0x63;
const EID_AUTO_OFF: u8 = 0x81;
const EID_GUIDANCE: u8 = 0x84;
const EID_MIC_ATTACHED: u8 = 0x8f;

const VOLUME_MAX: u8 = 30;
const SIDETONE_MAX: u8 = 10;
/// Auto power off choices in minutes (HeadsetControl, OpenInzone's H9 table).
const OFF_MINUTES: [u8; 6] = [0, 5, 15, 30, 60, 180];

pub(super) struct Spec {
    pub name: &'static str,
    pub pids: &'static [u16],
    /// Earbuds report left, right and case levels and no charging flag.
    buds: bool,
    /// Balance value at full chat.
    balance_max: u8,
    /// Controls besides the game/chat readout, in display order.
    keys: &'static [K],
}

pub(super) const H9_II: Spec = Spec {
    name: "Sony INZONE H9 II",
    pids: &[0x0fa8],
    buds: false,
    balance_max: 100,
    keys: &[
        K::ChatMixBalance,
        K::Volume,
        K::NoiseCancelling,
        K::NoiseCancellingStartup,
        K::NoiseCancellingButton,
        K::VoicePrompts,
        K::MicSideTone,
        K::MicMute,
        K::MicAttached,
        K::PmShutdown,
        K::BluetoothOnPower,
    ],
};
pub(super) const H5: Spec = Spec {
    name: "Sony INZONE H5",
    pids: &[0x0ebf],
    buds: false,
    balance_max: 90,
    keys: &[K::MicSideTone],
};
/// HeadsetControl reads only the battery; volume, sidetone, ambient sound and
/// the mute flag are from zoneout / OpenInzone, which verified them on Buds.
pub(super) const BUDS: Spec = Spec {
    name: "Sony INZONE Buds",
    pids: &[0x0ec2],
    buds: true,
    balance_max: 100,
    keys: &[
        K::ChatMixBalance,
        K::Volume,
        K::NoiseCancelling,
        K::MicSideTone,
        K::MicMute,
    ],
};

#[cfg(test)]
pub(super) const ALL: &[&Spec] = &[&H9_II, &H5, &BUDS];

// ── Packets ─────────────────────────────────────────────────────────────────

fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b))
}

/// A command to the headset; `payload` is at most 50 bytes.
fn command(eid: u8, etype: u8, tid: u16, payload: &[u8]) -> [u8; LEN] {
    let n = payload.len() as u8;
    let [lo, hi] = tid.to_le_bytes();
    let mut b = [0u8; LEN];
    b[..13].copy_from_slice(&[
        REPORT_ID,
        12 + n,
        0x01,
        0x00,
        0xfc,
        8 + n,
        0x96,
        0xc3,
        PC_TO_HEADSET,
        eid,
        etype,
        lo,
        hi,
    ]);
    let end = 13 + payload.len();
    b[13..end].copy_from_slice(payload);
    b[end] = checksum(&b[6..end]);
    b
}

#[derive(Debug, PartialEq)]
struct Event {
    eid: u8,
    etype: u8,
    tid: u16,
    payload: Vec<u8>,
}

/// A well-formed Sony vendor event addressed to the PC, checksum verified.
fn parse(r: &[u8]) -> Option<Event> {
    let len = usize::from(*r.get(1)?);
    if r[0] != REPORT_ID || !(12..=LEN - 2).contains(&len) || r.len() < len + 2 {
        return None;
    }
    let shell = r[2] == 0x04 && r[3] == 0xff && r[5] == 0x00 && r[6..8] == [0x96, 0xc3];
    if !shell || r[8] >> 4 != 0x1 || checksum(&r[5..=len]) != r[len + 1] {
        return None;
    }
    Some(Event {
        eid: r[9],
        etype: r[10],
        tid: u16::from_le_bytes([r[11], r[12]]),
        payload: r[13..=len].to_vec(),
    })
}

#[derive(Debug, PartialEq)]
enum Reading {
    /// Headset level (None = headset offline) and charging flag.
    Battery(Option<u8>, bool),
    /// Earbuds: left, right and case levels; None = not reporting.
    Buds([Option<u8>; 3]),
    /// The 4-byte ambient sound block (mode, level, percent, voice focus).
    Ambient([u8; 4]),
    Setting(K, SettingValue),
}

/// What an event payload says, limited to what `spec` exposes.
fn decode(spec: &Spec, eid: u8, p: &[u8]) -> Vec<Reading> {
    use Reading::*;
    let int = |k, v: u8| Setting(k, SettingValue::Int(i64::from(v)));
    let flag = |k, v: bool| Setting(k, SettingValue::Bool(v));
    let level = |v: u8| (v <= 100).then_some(v);
    let out = match (eid, p) {
        (EID_BATTERY, [_, l, _, r, _, c, ..]) if spec.buds => {
            vec![Buds([l, r, c].map(|v| level(*v)))]
        }
        (EID_BATTERY, [_, 0xff, ..]) if !spec.buds => vec![Battery(None, false)],
        (EID_BATTERY, [charger, pct, ..]) if !spec.buds && *pct <= 100 => {
            vec![Battery(Some(*pct), *charger != 0)]
        }
        (EID_BALANCE, [v, ..]) if *v <= spec.balance_max => {
            let chat = f64::from(*v) / f64::from(spec.balance_max);
            vec![
                Setting(K::ChatMixGame, SettingValue::Float(1.0 - chat)),
                Setting(K::ChatMixChat, SettingValue::Float(chat)),
                int(K::ChatMixBalance, *v),
            ]
        }
        (EID_VOLUME, [_, v, ..]) if *v <= VOLUME_MAX => vec![int(K::Volume, *v)],
        (EID_SIDETONE, [v, ..]) if *v <= SIDETONE_MAX => vec![int(K::MicSideTone, *v)],
        (EID_MIC, [m @ (0 | 1), ..]) => vec![flag(K::MicMute, *m == 1)],
        (EID_AMBIENT, [mode, a, b, c, ..]) => {
            let mut v = vec![Ambient([*mode, *a, *b, *c])];
            if *mode <= 2 {
                v.push(int(K::NoiseCancelling, *mode));
            }
            v
        }
        // A "removed" flag: 1 with the boom mic unplugged.
        (EID_MIC_ATTACHED, [r @ (0 | 1), ..]) => vec![flag(K::MicAttached, *r == 0)],
        (EID_AUTO_OFF, [m, ..]) if OFF_MINUTES.contains(m) => vec![int(K::PmShutdown, *m)],
        (EID_GUIDANCE, [b @ (0 | 1), ..]) => vec![flag(K::VoicePrompts, *b == 1)],
        (EID_BT_STARTUP, [b @ (0 | 1), ..]) => vec![flag(K::BluetoothOnPower, *b == 1)],
        (EID_NC_STARTUP, [m @ 0..=3, ..]) => vec![int(K::NoiseCancellingStartup, *m)],
        (EID_NC_TOGGLE, [a @ (0 | 1), b @ (0 | 1), c @ (0 | 1), ..]) if a | b | c != 0 => {
            vec![int(K::NoiseCancellingButton, a | b << 1 | c << 2)]
        }
        _ => vec![],
    };
    out.into_iter()
        .filter(|r| match r {
            Setting(K::ChatMixGame | K::ChatMixChat, _) => true,
            Setting(k, _) => spec.keys.contains(k),
            _ => true,
        })
        .collect()
}

/// Event id and payload of a setting write. Noise cancelling rewrites the
/// whole ambient block, so the current one (`ambient`) is required.
fn set_payload(key: K, value: &SettingValue, ambient: Option<[u8; 4]>) -> Result<(u8, Vec<u8>)> {
    let n = value.as_int().unwrap_or(0).clamp(0, 255) as u8;
    let b = u8::from(value.as_bool().unwrap_or(false));
    Ok(match key {
        // [mute, value, percent]; 0xFF = percent not given (as INZONE Hub sends).
        K::Volume => (EID_VOLUME, vec![0x00, n, 0xff]),
        K::MicSideTone => (EID_SIDETONE, vec![n, 0xff]),
        K::ChatMixBalance => (EID_BALANCE, vec![n]),
        K::NoiseCancelling => {
            let [_, level, pct, focus] = ambient.context("ambient sound state unknown")?;
            (EID_AMBIENT, vec![n, level, pct, focus])
        }
        K::NoiseCancellingStartup => (EID_NC_STARTUP, vec![n]),
        // Bit 0 = off, 1 = noise cancelling, 2 = ambient sound in the button cycle.
        K::NoiseCancellingButton => (EID_NC_TOGGLE, vec![n & 1, n >> 1 & 1, n >> 2 & 1]),
        K::VoicePrompts => (EID_GUIDANCE, vec![b]),
        K::BluetoothOnPower => (EID_BT_STARTUP, vec![b]),
        // The H9 II takes the minutes twice (HeadsetControl `h9ii_format`).
        K::PmShutdown => (EID_AUTO_OFF, vec![n, n]),
        _ => bail!("unsupported setting {}", key.as_str()),
    })
}

/// Settings the headset keeps, asked for once each time it comes online.
fn stored_ids(spec: &Spec) -> Vec<u8> {
    spec.keys
        .iter()
        .filter_map(|k| match k {
            K::NoiseCancellingStartup => Some(EID_NC_STARTUP),
            K::NoiseCancellingButton => Some(EID_NC_TOGGLE),
            K::VoicePrompts => Some(EID_GUIDANCE),
            K::PmShutdown => Some(EID_AUTO_OFF),
            K::BluetoothOnPower => Some(EID_BT_STARTUP),
            _ => None,
        })
        .collect()
}

/// GETs sent on every poll after the battery.
fn poll_ids(spec: &Spec) -> Vec<u8> {
    let mut ids = vec![EID_BALANCE];
    ids.extend(spec.keys.iter().filter_map(|k| match k {
        K::Volume => Some(EID_VOLUME),
        K::MicSideTone => Some(EID_SIDETONE),
        K::NoiseCancelling => Some(EID_AMBIENT),
        K::MicMute => Some(EID_MIC),
        K::MicAttached => Some(EID_MIC_ATTACHED),
        _ => None,
    }));
    ids
}

fn controls(spec: &Spec) -> Vec<SettingMeta> {
    let opts = |o: &[(i64, &str)]| o.iter().map(|&(v, l)| SettingOption::int(v, l)).collect();
    let mut c = vec![
        SettingMeta::readonly(K::ChatMixGame, "Game", 0.0, 1.0),
        SettingMeta::readonly(K::ChatMixChat, "Chat", 0.0, 1.0),
    ];
    for &k in spec.keys {
        c.push(match k {
            // INZONE Hub moves it in steps of 10 (OpenInzone).
            K::ChatMixBalance => {
                SettingMeta::slider(k, "Game / chat balance", 0.0, 100.0, 10.0).unit("% chat")
            }
            K::Volume => SettingMeta::slider(k, "Volume", 0.0, f64::from(VOLUME_MAX), 1.0),
            K::MicSideTone => SettingMeta::slider(k, "Sidetone", 0.0, f64::from(SIDETONE_MAX), 1.0),
            K::NoiseCancelling => SettingMeta::dropdown(
                k,
                "Noise control",
                opts(&[(0, "Off"), (1, "Noise cancelling"), (2, "Ambient sound")]),
            ),
            K::NoiseCancellingStartup => SettingMeta::dropdown(
                k,
                "Noise control at power on",
                opts(&[
                    (0, "Off"),
                    (1, "Noise cancelling"),
                    (2, "Ambient sound"),
                    (3, "Last used"),
                ]),
            ),
            K::NoiseCancellingButton => SettingMeta::dropdown(
                k,
                "Noise control button",
                opts(&[
                    (3, "Off / NC"),
                    (5, "Off / Ambient"),
                    (6, "NC / Ambient"),
                    (7, "Off / NC / Ambient"),
                ]),
            ),
            K::VoicePrompts => SettingMeta::toggle(k, "Voice guidance"),
            K::BluetoothOnPower => SettingMeta::toggle(k, "Bluetooth at power on"),
            K::MicMute => SettingMeta::readonly(k, "Muted", 0.0, 1.0),
            K::MicAttached => SettingMeta::readonly(k, "Boom mic attached", 0.0, 1.0),
            K::PmShutdown => SettingMeta::dropdown(
                k,
                "Auto-off",
                OFF_MINUTES
                    .iter()
                    .map(|&m| match m {
                        0 => SettingOption::int(0, "Never"),
                        m => SettingOption::int(i64::from(m), &format!("After {m} min")),
                    })
                    .collect(),
            ),
            _ => continue,
        });
    }
    c
}

// ── Adapter ─────────────────────────────────────────────────────────────────

const POLL: Duration = Duration::from_secs(30);
/// Buds answer in 225-300 ms (zoneout), the H9 II in under 60 ms.
const REPLY_WAIT: Duration = Duration::from_millis(600);

pub(super) fn open(spec: &'static Spec, pid: u16, ctx: &DriverCtx) -> Opened {
    let (dev, instance) = open_node(pid, |d| crate::hid::usage_pages(d).contains(&0xff04))?;
    info!("{} open via {}", spec.name, dev.path.display());
    let labels: &[&str] = if spec.buds {
        &["Left", "Right", "Case"]
    } else {
        &["Headset"]
    };
    let mut a = InzoneAdapter {
        spec,
        id: DeviceId {
            vendor_id: super::SONY,
            product_id: pid,
            instance,
        },
        dev: Some(Box::new(dev)),
        tid: 1,
        settings: hako_core::config::load_device_settings(&ctx.paths, super::SONY, spec.pids[0])?,
        batteries: labels
            .iter()
            .map(|l| BatteryState {
                label: (*l).into(),
                percent: None,
                charging: false,
                present: false,
            })
            .collect(),
        ambient: None,
        online: false,
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

/// Report transport, so the protocol logic can be tested without hardware.
trait Link: Send {
    fn write(&mut self, report: &[u8]) -> Result<()>;
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
}

impl Link for HidrawDevice {
    fn write(&mut self, report: &[u8]) -> Result<()> {
        HidrawDevice::write(self, report)
    }

    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        HidrawDevice::read_timeout(self, buf, timeout)
    }
}

struct InzoneAdapter {
    spec: &'static Spec,
    id: DeviceId,
    dev: Option<Box<dyn Link>>,
    tid: u16,
    settings: SettingsMap,
    batteries: Vec<BatteryState>,
    ambient: Option<[u8; 4]>,
    online: bool,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl InzoneAdapter {
    /// Send a command and wait for its reply; every event read meanwhile is
    /// applied. `None` = no reply in time.
    fn request(&mut self, eid: u8, etype: u8, payload: &[u8]) -> Result<Option<Event>> {
        // TID 1 is what the dongle's own pushes carry; 0 and 1 are skipped.
        self.tid = self.tid.wrapping_add(1).max(2);
        let tid = self.tid;
        let want = if etype == SET { NTFY } else { RET };
        let dev = self.dev.as_mut().context("not connected")?;
        dev.write(&command(eid, etype, tid, payload))?;
        let deadline = Instant::now() + REPLY_WAIT;
        let mut buf = [0u8; LEN];
        for _ in 0..64 {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(dev) = self.dev.as_mut() else { break };
            let n = match dev.read_timeout(&mut buf, left) {
                Ok(n) => n,
                // A timeout is a plain error, a failed read an I/O one.
                Err(e) if e.downcast_ref::<std::io::Error>().is_some() => return Err(e),
                Err(_) => break,
            };
            let Some(ev) = parse(&buf[..n]) else { continue };
            self.apply(&ev);
            if ev.eid == eid && ev.tid == tid && (ev.etype == want || ev.etype == NTFY_ACTIVE) {
                return Ok(Some(ev));
            }
        }
        Ok(None)
    }

    fn apply(&mut self, ev: &Event) {
        for r in decode(self.spec, ev.eid, &ev.payload) {
            match r {
                Reading::Battery(pct, charging) => {
                    self.online = pct.is_some();
                    let b = &mut self.batteries[0];
                    (b.percent, b.charging, b.present) = (pct, charging, pct.is_some());
                }
                Reading::Buds(levels) => {
                    self.online = levels[..2].iter().any(Option::is_some);
                    for (b, l) in self.batteries.iter_mut().zip(levels) {
                        (b.percent, b.present) = (l, l.is_some());
                    }
                }
                Reading::Ambient(block) => self.ambient = Some(block),
                Reading::Setting(k, v) => merge_setting(&mut self.settings, k, v),
            }
        }
    }

    fn set_offline(&mut self) {
        self.online = false;
        for b in &mut self.batteries {
            (b.percent, b.charging, b.present) = (None, false, false);
        }
    }

    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        let was_online = self.online;
        if self.request(EID_BATTERY, GET, &[])?.is_none() {
            self.set_offline();
        }
        if !self.online {
            return Ok(());
        }
        if !was_online {
            for eid in stored_ids(self.spec) {
                self.request(eid, GET, &[])?;
            }
        }
        for eid in poll_ids(self.spec) {
            self.request(eid, GET, &[])?;
        }
        debug!("{} batteries {:?}", self.spec.name, self.batteries);
        Ok(())
    }

    fn write(&mut self, key: K, value: &SettingValue) -> Result<()> {
        if key == K::NoiseCancelling {
            // Fresh block: the level and voice focus may have changed on the headset.
            self.ambient = None;
            self.request(EID_AMBIENT, GET, &[])?;
        }
        let (eid, payload) = set_payload(key, value, self.ambient)?;
        if self.request(eid, SET, &payload)?.is_none() {
            bail!("headset did not acknowledge {}", key.as_str());
        }
        Ok(())
    }
}

impl DeviceAdapter for InzoneAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: self.batteries.clone(),
            settings: self.settings.clone(),
            controls: controls(self.spec),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: K, value: SettingValue) -> Result<()> {
        self.write(key, &value)?;
        merge_setting(&mut self.settings, key, value);
        hako_core::config::save_device_settings(
            &self.paths,
            super::SONY,
            self.spec.pids[0],
            &self.settings,
        )
        .context("applied, but not saved")
    }

    fn tick(&mut self) -> Result<()> {
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        // Changes made on the headset arrive as unsolicited events.
        let mut events = Vec::new();
        let mut buf = [0u8; LEN];
        for _ in 0..64 {
            let Ok(n) = dev.read_timeout(&mut buf, Duration::ZERO) else {
                break;
            };
            events.extend(parse(&buf[..n]));
        }
        for ev in &events {
            self.apply(ev);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An event as the dongle frames it, checksum included.
    fn event(eid: u8, etype: u8, tid: u16, payload: &[u8]) -> Vec<u8> {
        let n = payload.len() as u8;
        let [lo, hi] = tid.to_le_bytes();
        let mut r = vec![
            REPORT_ID,
            12 + n,
            0x04,
            0xff,
            9 + n,
            0x00,
            0x96,
            0xc3,
            0x14,
            eid,
            etype,
            lo,
            hi,
        ];
        r.extend_from_slice(payload);
        r.push(checksum(&r[5..]));
        r.resize(LEN, 0);
        r
    }

    #[test]
    fn get_matches_zoneout_read_request() {
        // zoneout `_get_report`: tid 1, checksum = event id + 0x9c.
        let c = command(EID_BATTERY, GET, 1, &[]);
        assert_eq!(
            c[..14],
            [0x02, 0x0c, 0x01, 0x00, 0xfc, 0x08, 0x96, 0xc3, 0x41, 0x04, 0x01, 0x01, 0x00, 0xa0]
        );
        assert!(c[14..].iter().all(|&b| b == 0));
    }

    #[test]
    fn sets_match_zoneout_write_table() {
        // zoneout WRITE_MAP: (type, value offset, checksum offset, constant, spacers).
        let (eid, p) = set_payload(K::Volume, &SettingValue::Int(20), None).unwrap();
        let c = command(eid, SET, 7, &p);
        assert_eq!(
            c[..17],
            [
                0x02, 0x0f, 0x01, 0x00, 0xfc, 0x0b, 0x96, 0xc3, 0x41, 0x21, 0x02, 7, 0, 0, 20,
                0xff, 0xd7
            ]
        );
        // Checksum = sequence + value + 0xbc.
        assert_eq!(c[16], (7u8 + 20).wrapping_add(0xbc));
        assert!(c[17..].iter().all(|&b| b == 0));
        let (eid, p) = set_payload(K::MicSideTone, &SettingValue::Int(10), None).unwrap();
        let c = command(eid, SET, 3, &p);
        assert_eq!((c[1], c[9], c[13], c[14]), (0x0e, 0x23, 10, 0xff));
        assert_eq!(c[15], (3u8 + 10).wrapping_add(0xbe));
        // OpenInzone worked example: balance 30 with transaction id 0x65.
        let (eid, p) = set_payload(K::ChatMixBalance, &SettingValue::Int(30), None).unwrap();
        let c = command(eid, SET, 0x65, &p);
        assert_eq!(
            c[..15],
            [
                0x02, 0x0d, 0x01, 0x00, 0xfc, 0x09, 0x96, 0xc3, 0x41, 0x22, 0x02, 0x65, 0x00, 0x1e,
                0x41
            ]
        );
        // zoneout: checksum = sequence + value + 0xbe.
        assert_eq!(c[14], (0x65u8 + 30).wrapping_add(0xbe));
        let (eid, p) = set_payload(K::VoicePrompts, &SettingValue::Bool(true), None).unwrap();
        let c = command(eid, SET, 2, &p);
        assert_eq!(
            (c[1], c[9], c[13], c[14]),
            (0x0d, 0x84, 1, (2u8 + 1).wrapping_add(0x20))
        );
        let (eid, p) = set_payload(K::NoiseCancellingStartup, &SettingValue::Int(3), None).unwrap();
        assert_eq!(command(eid, SET, 2, &p)[14], (2u8 + 3).wrapping_add(0xdf));
        let (eid, p) = set_payload(K::BluetoothOnPower, &SettingValue::Bool(false), None).unwrap();
        assert_eq!(command(eid, SET, 9, &p)[14], 9u8.wrapping_add(0xff));
        // Noise control keeps the level / voice focus read before (zoneout nc_mode: 14 = 0x14, 15 = 0xff).
        let amb = Some([0x02, 0x14, 0xff, 0x00]);
        let (eid, p) = set_payload(K::NoiseCancelling, &SettingValue::Int(1), amb).unwrap();
        let c = command(eid, SET, 4, &p);
        assert_eq!(
            c[13..18],
            [1, 0x14, 0xff, 0x00, (4u8 + 1).wrapping_add(0xf0)]
        );
        assert!(set_payload(K::NoiseCancelling, &SettingValue::Int(1), None).is_err());
    }

    #[test]
    fn headsetcontrol_only_payloads() {
        let int = SettingValue::Int;
        assert_eq!(
            set_payload(K::PmShutdown, &int(30), None).unwrap(),
            (0x81, vec![30, 30])
        );
        assert_eq!(
            set_payload(K::NoiseCancellingButton, &int(5), None).unwrap(),
            (0x42, vec![1, 0, 1])
        );
        assert!(set_payload(K::Effect, &int(1), None).is_err());
    }

    #[test]
    fn events_parse_and_reject_bad_frames() {
        let r = event(EID_BATTERY, RET, 2, &[0x00, 0x50]);
        // Hand-computed: 00+96+c3+14+04+10+02+00+00+50 = 0x1d3.
        assert_eq!(r[15], 0xd3);
        assert_eq!(
            parse(&r),
            Some(Event {
                eid: 0x04,
                etype: RET,
                tid: 2,
                payload: vec![0x00, 0x50]
            })
        );
        let mut bad = r.clone();
        bad[15] ^= 1;
        assert_eq!(parse(&bad), None);
        let mut to_headset = r.clone();
        to_headset[8] = 0x41;
        assert_eq!(parse(&to_headset), None);
        assert_eq!(parse(&command(EID_BATTERY, GET, 2, &[])), None);
        assert!(parse(&event(EID_BATTERY, NTFY_ACTIVE, 1, &[]))
            .unwrap()
            .payload
            .is_empty());
    }

    #[test]
    fn decode_follows_sources() {
        use Reading::*;
        assert_eq!(
            decode(&H9_II, EID_BATTERY, &[1, 80]),
            vec![Battery(Some(80), true)]
        );
        assert_eq!(
            decode(&H9_II, EID_BATTERY, &[0, 0xff]),
            vec![Battery(None, false)]
        );
        assert!(decode(&H9_II, EID_BATTERY, &[0, 101]).is_empty());
        // OpenInzone: left first, case last; 0xff = docked.
        assert_eq!(
            decode(&BUDS, EID_BATTERY, &[0x00, 0x2f, 0x00, 0xff, 0xff, 0x22]),
            vec![Buds([Some(0x2f), None, Some(0x22)])]
        );
        let mix = |spec, v| decode(spec, EID_BALANCE, &[v]);
        assert_eq!(
            mix(&H9_II, 25),
            vec![
                Setting(K::ChatMixGame, SettingValue::Float(0.75)),
                Setting(K::ChatMixChat, SettingValue::Float(0.25)),
                Setting(K::ChatMixBalance, SettingValue::Int(25)),
            ]
        );
        // The H5 balance is shown, not offered.
        assert_eq!(mix(&H5, 45).len(), 2);
        assert_eq!(
            mix(&H5, 90)[1],
            Setting(K::ChatMixChat, SettingValue::Float(1.0))
        );
        assert!(mix(&H5, 100).is_empty());
        assert_eq!(
            decode(&H9_II, EID_VOLUME, &[0, 16, 0xff]),
            vec![Setting(K::Volume, SettingValue::Int(16))]
        );
        // H5 has no volume control.
        assert!(decode(&H5, EID_VOLUME, &[0, 16, 0xff]).is_empty());
        assert_eq!(
            decode(&H9_II, EID_AMBIENT, &[1, 0x14, 0xff, 0]),
            vec![
                Ambient([1, 0x14, 0xff, 0]),
                Setting(K::NoiseCancelling, SettingValue::Int(1))
            ]
        );
        assert_eq!(
            decode(&H9_II, EID_MIC_ATTACHED, &[1]),
            vec![Setting(K::MicAttached, SettingValue::Bool(false))]
        );
        assert_eq!(
            decode(&H9_II, EID_NC_TOGGLE, &[0, 1, 1]),
            vec![Setting(K::NoiseCancellingButton, SettingValue::Int(6))]
        );
        assert!(decode(&H9_II, EID_AUTO_OFF, &[7]).is_empty());
    }

    #[test]
    fn every_control_validates_and_encodes() {
        for spec in ALL {
            assert!(!spec.pids.is_empty());
            for meta in controls(spec).iter().filter(|m| !m.read_only) {
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
                    let (eid, p) = set_payload(meta.key, &v, Some([0, 0x14, 0xff, 0])).unwrap();
                    // Every written value reads back as itself.
                    let back = decode(spec, eid, &p);
                    assert!(
                        back.contains(&Reading::Setting(meta.key, v.clone())),
                        "{} {:?} {v:?} -> {back:?}",
                        spec.name,
                        meta.key
                    );
                }
            }
        }
    }

    type Sent = std::sync::Arc<std::sync::Mutex<Vec<(u8, u8, Vec<u8>)>>>;

    /// Dongle double: answers GETs from `values` (others time out), stores SETs.
    struct FakeDongle {
        values: std::collections::HashMap<u8, Vec<u8>>,
        sent: Sent,
        replies: std::collections::VecDeque<Vec<u8>>,
    }

    impl Link for FakeDongle {
        fn write(&mut self, c: &[u8]) -> Result<()> {
            let n = usize::from(c[1]) - 12;
            let (eid, etype) = (c[9], c[10]);
            let tid = u16::from_le_bytes([c[11], c[12]]);
            let payload = c[13..13 + n].to_vec();
            self.sent
                .lock()
                .unwrap()
                .push((eid, etype, payload.clone()));
            if etype == SET {
                self.values.insert(eid, payload.clone());
                self.replies.push_back(event(eid, NTFY, tid, &payload));
            } else if let Some(v) = self.values.get(&eid) {
                self.replies.push_back(event(eid, RET, tid, v));
            }
            Ok(())
        }

        fn read_timeout(&mut self, buf: &mut [u8], _: Duration) -> Result<usize> {
            let r = self.replies.pop_front().context("timeout")?;
            buf[..r.len()].copy_from_slice(&r);
            Ok(r.len())
        }
    }

    #[test]
    fn coming_online_reads_back_and_never_writes() {
        let dir = std::env::temp_dir().join(format!("hako-inzone-{}", std::process::id()));
        let paths = ConfigPaths {
            root: dir.clone(),
            config_toml: dir.join("config.toml"),
            devices_dir: dir.join("devices"),
            profiles_dir: dir.join("profiles"),
            remaps_dir: dir.join("remaps"),
        };
        // What the user set earlier, from the device settings file.
        let mut saved = SettingsMap::new();
        for (k, v) in [
            (K::MicSideTone, SettingValue::Int(2)),
            (K::VoicePrompts, SettingValue::Bool(true)),
            (K::PmShutdown, SettingValue::Int(30)),
            (K::BluetoothOnPower, SettingValue::Bool(true)),
            (K::NoiseCancellingButton, SettingValue::Int(7)),
        ] {
            merge_setting(&mut saved, k, v);
        }
        // The headset's own state; it does not answer 0x42 or 0x63.
        let values = [
            (EID_BATTERY, vec![0, 80]),
            (EID_BALANCE, vec![50]),
            (EID_VOLUME, vec![0, 12, 0xff]),
            (EID_SIDETONE, vec![7, 0xff]),
            (EID_AMBIENT, vec![1, 0x14, 0xff, 0]),
            (EID_MIC, vec![0, 0xff, 0xff]),
            (EID_MIC_ATTACHED, vec![0]),
            (EID_NC_STARTUP, vec![3]),
            (EID_GUIDANCE, vec![0]),
            (EID_AUTO_OFF, vec![5]),
        ]
        .into();
        let sent = Sent::default();
        let mut a = InzoneAdapter {
            spec: &H9_II,
            id: DeviceId {
                vendor_id: super::super::SONY,
                product_id: 0x0fa8,
                instance: "test".into(),
            },
            dev: Some(Box::new(FakeDongle {
                values,
                sent: sent.clone(),
                replies: Default::default(),
            })),
            tid: 1,
            settings: saved,
            batteries: vec![BatteryState {
                label: "Headset".into(),
                percent: None,
                charging: false,
                present: false,
            }],
            ambient: None,
            online: false,
            last_poll: None,
            failures: 0,
            last_error: None,
            paths,
        };
        a.poll().unwrap();
        assert!(a.online);
        a.poll().unwrap();
        let sets = |s: &Sent| s.lock().unwrap().iter().filter(|c| c.1 == SET).count();
        assert_eq!(sets(&sent), 0, "{:02x?}", sent.lock().unwrap());
        let got = |k| hako_core::config::get_setting(&a.settings, k).cloned();
        // Read back from the headset...
        assert_eq!(got(K::MicSideTone), Some(SettingValue::Int(7)));
        assert_eq!(got(K::VoicePrompts), Some(SettingValue::Bool(false)));
        assert_eq!(got(K::PmShutdown), Some(SettingValue::Int(5)));
        assert_eq!(got(K::NoiseCancellingStartup), Some(SettingValue::Int(3)));
        // ...or, unanswered, the user's last value, not re-sent.
        assert_eq!(got(K::BluetoothOnPower), Some(SettingValue::Bool(true)));
        assert_eq!(got(K::NoiseCancellingButton), Some(SettingValue::Int(7)));
        // Stored settings are asked for only when the headset comes online.
        let guidance_gets = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == EID_GUIDANCE)
            .count();
        assert_eq!(guidance_gets, 1);

        // A user change is still written.
        a.set(K::VoicePrompts, SettingValue::Bool(true)).unwrap();
        assert_eq!(
            sent.lock().unwrap().last(),
            Some(&(EID_GUIDANCE, SET, vec![1]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_read_is_an_error_not_offline() {
        struct Gone;
        impl Link for Gone {
            fn write(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn read_timeout(&mut self, _: &mut [u8], _: Duration) -> Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::EIO).into())
            }
        }
        let dir = std::env::temp_dir().join(format!("hako-inzone-gone-{}", std::process::id()));
        let mut a = InzoneAdapter {
            spec: &H9_II,
            id: DeviceId {
                vendor_id: super::super::SONY,
                product_id: 0x0fa8,
                instance: "test".into(),
            },
            dev: Some(Box::new(Gone)),
            tid: 1,
            settings: SettingsMap::new(),
            batteries: vec![BatteryState {
                label: "Headset".into(),
                percent: Some(50),
                charging: false,
                present: true,
            }],
            ambient: None,
            online: true,
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
        assert!(a.poll().is_err());
        assert!(a.online, "not taken for out of range");
        assert!(a.request(EID_BATTERY, GET, &[]).is_err());
    }

    /// Read-only probe of a plugged-in dongle (GETs only).
    #[test]
    #[ignore]
    fn hw_poll_status() {
        let ctx_paths = ConfigPaths::xdg().unwrap();
        for spec in ALL {
            let Ok((dev, instance)) = open_node(spec.pids[0], |d| {
                crate::hid::usage_pages(d).contains(&0xff04)
            }) else {
                continue;
            };
            let mut a = InzoneAdapter {
                spec,
                id: DeviceId {
                    vendor_id: super::super::SONY,
                    product_id: spec.pids[0],
                    instance,
                },
                dev: Some(Box::new(dev)),
                tid: 1,
                settings: SettingsMap::new(),
                batteries: vec![
                    BatteryState {
                        label: "Headset".into(),
                        percent: None,
                        charging: false,
                        present: false
                    };
                    3
                ],
                ambient: None,
                online: true,
                last_poll: None,
                failures: 0,
                last_error: None,
                paths: ctx_paths.clone(),
            };
            a.poll().unwrap();
            eprintln!("{} {:?} {:?}", spec.name, a.batteries, a.settings);
        }
    }
}
