//! Turtle Beach Stealth Pro (Halo) base stations: battery over the Ear Force
//! "DIAG" protocol. Read-only; the sidetone, noise gate and mic settings the
//! same messages could write are shown as blocked placeholders.
//!
//! Source: Turtle Beach Audio Hub V2 3.14.8.91 (official Windows app, read
//! for layouts only; `docs/vendor-re/turtlebeach.md` section 5b). Reports are
//! 64 bytes with no report id (the app prepends id 0 itself) on the 0xFF00
//! collection. Request `[cmd][dst][src][sub][60 payload]`, reply
//! `[cmd][dst][src][status][sub][59 payload]` with status 2 = ok and the
//! request's command and subcommand echoed. The app's settings read is
//! command `a1` sub `20` to the broadcast address 9 from source 6 (USB base
//! on a PC). Reply payload: byte 3 battery percent, 4 charging, 9 mic monitor
//! level, 10 mic monitor on, 15 noise gate on, 16 auto-off minutes.
//! The app waits 1500 ms for a Halo reply. Single source, untested on
//! hardware.

use super::TURTLE_BEACH;
use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, report_ids, usage_pages, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue, SettingsMap};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const SIZE: usize = 64;
const CMD_APP: u8 = 0xa1;
const SUB_GET_SETTINGS: u8 = 0x20;
const ADDR_BROADCAST: u8 = 9;
const ADDR_USB_BASE_PC: u8 = 6;
const STATUS_OK: u8 = 2;
/// Offset of the reply payload.
const PAYLOAD: usize = 5;

const POLL: Duration = Duration::from_secs(15);
const REPLY_WAIT: Duration = Duration::from_millis(1500);
const READS_MAX: usize = 32;

/// Stealth Pro base stations: PlayStation / PC (`2196`), PC mode (`2197`)
/// and Xbox (`2216`).
const NAME: &str = "Turtle Beach Stealth Pro";
const PIDS: &[u16] = &[0x2196, 0x2197, 0x2216];

pub const MODEL: Model = Model {
    vendor_id: TURTLE_BEACH,
    product_ids: PIDS,
    kind: DeviceKind::Headset,
    name: NAME,
    usbfs: false,
    multi: false,
    open,
};

fn get_settings_request() -> [u8; SIZE] {
    let mut r = [0u8; SIZE];
    r[..4].copy_from_slice(&[CMD_APP, ADDR_BROADCAST, ADDR_USB_BASE_PC, SUB_GET_SETTINGS]);
    r
}

#[derive(Debug, PartialEq)]
struct Settings {
    battery: Option<u8>,
    charging: bool,
    side_tone: bool,
    side_tone_level: u8,
    noise_gate: bool,
    auto_off: u8,
}

/// The settings reply to `get_settings_request`; None for any other report.
fn parse_settings(r: &[u8]) -> Option<Settings> {
    if r.len() < PAYLOAD + 17 || r[0] != CMD_APP || r[3] != STATUS_OK || r[4] != SUB_GET_SETTINGS {
        return None;
    }
    let p = &r[PAYLOAD..];
    Some(Settings {
        battery: Some(p[3]).filter(|&b| b <= 100),
        charging: p[4] > 0,
        side_tone: p[10] > 0,
        side_tone_level: p[9],
        noise_gate: p[15] > 0,
        auto_off: p[16],
    })
}

fn controls() -> Vec<SettingMeta> {
    const WHY: &str = "Layout from one vendor app only; whether the headset stores it is unknown";
    vec![
        SettingMeta::toggle(SettingKey::MicSideTone, "Sidetone"),
        SettingMeta::slider(
            SettingKey::MicSideToneVolume,
            "Sidetone level",
            0.0,
            100.0,
            1.0,
        ),
        SettingMeta::toggle(SettingKey::MicNoiseFilter, "Noise gate"),
    ]
    .into_iter()
    .map(|m| m.persistent().blocked(WHY))
    .collect()
}

trait Link: Send {
    fn send(&mut self, report: &[u8]) -> Result<()>;
    /// One input report; Ok(None) on timeout.
    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
}

impl Link for HidrawDevice {
    fn send(&mut self, report: &[u8]) -> Result<()> {
        self.write(report)
    }

    fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let mut buf = [0u8; SIZE];
        match self.read_timeout(&mut buf, timeout) {
            Ok(n) => Ok(Some(buf[..n].to_vec())),
            // A timeout is a plain error, a failed read an I/O one.
            Err(e) if e.downcast_ref::<std::io::Error>().is_some() => Err(e),
            Err(_) => Ok(None),
        }
    }
}

/// One settings read; None = no reply in time. Other reports (the base also
/// pushes events) are skipped.
fn read_settings(link: &mut dyn Link) -> Result<Option<Settings>> {
    // A late reply to an earlier read is not this one's.
    for _ in 0..READS_MAX {
        if link.recv(Duration::ZERO)?.is_none() {
            break;
        }
    }
    link.send(&get_settings_request())?;
    let deadline = Instant::now() + REPLY_WAIT;
    for _ in 0..READS_MAX {
        let left = deadline.saturating_duration_since(Instant::now());
        match link.recv(left)? {
            Some(r) => {
                if let Some(s) = parse_settings(&r) {
                    return Ok(Some(s));
                }
            }
            None => break,
        }
    }
    Ok(None)
}

fn find_node(pid: u16) -> Result<HidDeviceInfo> {
    enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == TURTLE_BEACH && d.product_id == pid)
        .find(|d| {
            let desc = d.report_descriptor();
            usage_pages(&desc).contains(&0xff00) && report_ids(&desc).is_empty()
        })
        .with_context(|| format!("no DIAG node for {TURTLE_BEACH:04x}:{pid:04x}"))
}

fn offline() -> BatteryState {
    BatteryState {
        label: "Headset".into(),
        percent: None,
        charging: false,
        present: false,
    }
}

fn open(pid: u16, _ctx: &DriverCtx) -> Opened {
    let info = find_node(pid)?;
    let dev = HidrawDevice::open(&info.path)?;
    info!("{NAME} open via {}", info.path.display());
    let mut a = HaloAdapter {
        id: DeviceId {
            vendor_id: TURTLE_BEACH,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        link: Some(Box::new(dev)),
        battery: offline(),
        settings: SettingsMap::new(),
        last_poll: None,
        failures: 0,
        last_error: None,
    };
    if let Err(e) = a.poll() {
        warn!("{NAME} status: {e}");
        a.last_error = Some(e.to_string());
    }
    Ok(vec![Box::new(a)])
}

struct HaloAdapter {
    id: DeviceId,
    link: Option<Box<dyn Link>>,
    battery: BatteryState,
    settings: SettingsMap,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
}

impl HaloAdapter {
    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        let link = self.link.as_deref_mut().context("not connected")?;
        let Some(s) = read_settings(link)? else {
            self.battery = offline();
            bail!("the base station did not answer the settings read");
        };
        debug!("{NAME} {s:?}");
        self.battery = BatteryState {
            percent: s.battery,
            charging: s.charging,
            present: s.battery.is_some(),
            ..offline()
        };
        self.settings
            .insert("mic_side_tone".into(), SettingValue::Bool(s.side_tone));
        self.settings.insert(
            "mic_side_tone_volume".into(),
            SettingValue::Int(i64::from(s.side_tone_level.min(100))),
        );
        self.settings
            .insert("mic_noise_filter".into(), SettingValue::Bool(s.noise_gate));
        Ok(())
    }
}

impl DeviceAdapter for HaloAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: NAME.into(),
            connected: self.link.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("unsupported setting {}", key.as_str())
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
    use std::collections::VecDeque;

    fn reply(battery: u8, charging: u8) -> Vec<u8> {
        let mut r = vec![0u8; SIZE];
        r[..5].copy_from_slice(&[CMD_APP, ADDR_USB_BASE_PC, ADDR_BROADCAST, STATUS_OK, 0x20]);
        let p = &mut r[PAYLOAD..];
        p[3] = battery;
        p[4] = charging;
        p[9] = 40;
        p[10] = 1;
        p[15] = 1;
        p[16] = 15;
        r
    }

    /// Queues the next batch of `answers` on every request.
    struct Fake {
        sent: Vec<Vec<u8>>,
        queue: VecDeque<Vec<u8>>,
        answers: VecDeque<Vec<Vec<u8>>>,
    }

    impl Link for Fake {
        fn send(&mut self, report: &[u8]) -> Result<()> {
            self.sent.push(report.to_vec());
            self.queue
                .extend(self.answers.pop_front().unwrap_or_default());
            Ok(())
        }

        fn recv(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
            Ok(self.queue.pop_front())
        }
    }

    #[test]
    fn request_and_reply_layout() {
        let r = get_settings_request();
        assert_eq!(r.len(), 64);
        assert_eq!(r[..5], [0xa1, 9, 6, 0x20, 0]);
        assert_eq!(
            parse_settings(&reply(87, 1)),
            Some(Settings {
                battery: Some(87),
                charging: true,
                side_tone: true,
                side_tone_level: 40,
                noise_gate: true,
                auto_off: 15,
            })
        );
        // A bad status, another subcommand or command, or an impossible level.
        let mut bad = reply(50, 0);
        bad[3] = 1;
        assert_eq!(parse_settings(&bad), None);
        let mut other = reply(50, 0);
        other[4] = 0x44;
        assert_eq!(parse_settings(&other), None);
        assert_eq!(parse_settings(&reply(200, 0)).unwrap().battery, None);
        assert_eq!(parse_settings(&reply(50, 0)[..10]), None);
    }

    #[test]
    fn events_are_skipped_and_state_is_kept() {
        let mut event = vec![0u8; SIZE];
        event[0] = 0xe6;
        // A late answer to an earlier read is queued before the request.
        let fake = Fake {
            sent: vec![],
            queue: VecDeque::from([reply(10, 0)]),
            answers: VecDeque::from([vec![event, reply(64, 0)]]),
        };
        let mut a = HaloAdapter {
            id: DeviceId {
                vendor_id: TURTLE_BEACH,
                product_id: PIDS[0],
                instance: "test".into(),
            },
            link: Some(Box::new(fake)),
            battery: offline(),
            settings: SettingsMap::new(),
            last_poll: None,
            failures: 0,
            last_error: None,
        };
        a.poll().unwrap();
        assert_eq!(a.battery.percent, Some(64));
        assert!(a.battery.present && !a.battery.charging);
        assert_eq!(a.settings["mic_side_tone_volume"], SettingValue::Int(40));
        let snap = a.snapshot();
        assert!(snap.controls.iter().all(|c| c.blocked.is_some()));
        assert!(a
            .set(SettingKey::MicSideTone, SettingValue::Bool(false))
            .is_err());
        // No reply: an error and no battery.
        assert!(a.poll().is_err());
        assert!(!a.battery.present);
    }
}
