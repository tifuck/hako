//! Sony INZONE Mouse-A (MSE-G500): wired `054c:0fae`, 2.4 GHz dongle `054c:0faf`.
//!
//! Byte layouts from INZONE Hub 1.0.20.0 (static analysis, `docs/vendor-re/sony.md`;
//! `MouseCommunication`, `MouseParam`, `KeyboardMousePacket*`, `UsbCommunication`).
//! 65-byte reports, id 0, on interface 2's vendor collection (usage page
//! 0xFF00 wired, 0xFF90 dongle): `[00][cmd][index][profile<<4 | packet][len][data]`.
//! cmd 0xA0 = GET (reply echoes cmd, index and field with the data), 0x20 =
//! SET (reply `[len 2][EC AC]` ok or `[len 3][EC FA][error]`). Indices:
//!
//! ```text
//! 00 device info (GET, 18 B: pid, vid, three fw versions, current profile at 16)
//! 03 dongle: RF status (GET, CC = mouse connected, DD = not)
//! 04 battery level (GET, 0-3)      09 charging (GET, 0/1)
//! 05 profile function (GET, 9 B: rate, angle snap, lift-off, dpi u16, motion sync, brightness, r, g, b)
//! 06 SET angle snap [0/1]   07 SET dpi [u16 LE]   10 SET report rate [0-4]
//! ```
//!
//! DPI is sent as `(dpi - 50) / 50`; rates 0-4 are 500 / 1000 / 2000 / 4000 / 8000 Hz.
//! Hub keeps profiles in the mouse and saves them with a separate command
//! (index 0x50), which is never sent here: changes are live-only for the
//! session and the mouse's stored profile is untouched (the SET commands are
//! documented as separate from the save; whether a SET alone persists is not
//! proven, so this is MEDIUM confidence). Nothing here writes firmware, button
//! maps or profile names.

use super::open_node_where;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey as K, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const GET: u8 = 0xa0;
const SET: u8 = 0x20;
const IDX_INFO: u8 = 0x00;
const IDX_RF: u8 = 0x03;
const IDX_BATTERY: u8 = 0x04;
const IDX_FUNCTION: u8 = 0x05;
const IDX_SNAP: u8 = 0x06;
const IDX_DPI: u8 = 0x07;
const IDX_CHARGE: u8 = 0x09;
const IDX_RATE: u8 = 0x10;

const RF_CONNECTED: u8 = 0xcc;
const STATUS_OK: [u8; 2] = [0xec, 0xac];
const STATUS_NG: [u8; 2] = [0xec, 0xfa];

const WIRED: u16 = 0x0fae;
const DONGLE: u16 = 0x0faf;
const RATES: [i64; 5] = [500, 1000, 2000, 4000, 8000];
const DPI_MIN: f64 = 100.0;
const DPI_MAX: f64 = 30000.0;
const DPI_STEP: f64 = 50.0;

const POLL: Duration = Duration::from_secs(10);
const REPLY_WAIT: Duration = Duration::from_millis(1000);

pub(super) struct Spec {
    pub name: &'static str,
    pub pids: &'static [u16],
}

pub(super) const MOUSE_A: Spec = Spec {
    name: "Sony INZONE Mouse-A",
    pids: &[WIRED, DONGLE],
};

/// One output report (leading report id 0).
fn packet(cmd: u8, index: u8, profile: u8, data: &[u8]) -> [u8; 65] {
    let mut r = [0u8; 65];
    r[1..5].copy_from_slice(&[cmd, index, profile << 4, data.len() as u8]);
    r[5..5 + data.len()].copy_from_slice(data);
    r
}

/// Data of a reply to `cmd`/`index` (input reports carry no report id).
fn reply_data(r: &[u8], cmd: u8, index: u8) -> Option<&[u8]> {
    let len = usize::from(*r.get(3)?);
    (r.len() >= 4 + len && r[0] == cmd && r[1] == index).then(|| &r[4..4 + len])
}

/// A SET reply: Ok, or the error code.
fn set_result(data: &[u8]) -> Result<()> {
    match data {
        [a, b, ..] if [*a, *b] == STATUS_OK => Ok(()),
        [a, b, code, ..] if [*a, *b] == STATUS_NG => {
            bail!("mouse rejected the write (error {code})")
        }
        _ => bail!("unexpected write reply {data:02x?}"),
    }
}

fn dpi_bytes(dpi: u16) -> [u8; 2] {
    (dpi.saturating_sub(50) / 50).to_le_bytes()
}

/// Settings in a profile-function block.
#[derive(Debug, PartialEq)]
struct Function {
    rate: i64,
    snap: bool,
    dpi: i64,
}

fn parse_function(d: &[u8]) -> Option<Function> {
    let [rate, snap, _lod, dl, dh, ..] = *d else {
        return None;
    };
    Some(Function {
        rate: *RATES.get(usize::from(rate))?,
        snap: snap != 0,
        dpi: i64::from(u16::from_le_bytes([dl, dh])) * 50 + 50,
    })
}

/// The four battery steps have no percentage; shown as the middle of each.
fn battery_percent(level: u8) -> Option<u8> {
    [12, 37, 62, 87].get(usize::from(level)).copied()
}

fn controls() -> Vec<SettingMeta> {
    vec![
        SettingMeta::slider(K::Dpi, "DPI", DPI_MIN, DPI_MAX, DPI_STEP),
        SettingMeta::dropdown(
            K::ReportRate,
            "Report rate",
            RATES
                .iter()
                .map(|&r| SettingOption::int(r, &format!("{r} Hz")))
                .collect(),
        ),
        SettingMeta::toggle(K::AngleSnapping, "Sensor snap"),
    ]
}

/// Report transport, so the protocol logic can be tested without hardware.
trait Link: Send {
    fn write(&mut self, report: &[u8]) -> Result<()>;
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
    fn drain(&mut self);
}

impl Link for HidrawDevice {
    fn write(&mut self, report: &[u8]) -> Result<()> {
        HidrawDevice::write(self, report)
    }

    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        HidrawDevice::read_timeout(self, buf, timeout)
    }

    fn drain(&mut self) {
        HidrawDevice::drain(self, Duration::ZERO);
    }
}

struct MouseAdapter {
    spec: &'static Spec,
    id: DeviceId,
    dongle: bool,
    dev: Option<Box<dyn Link>>,
    settings: SettingsMap,
    battery: BatteryState,
    profile: u8,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
    paths: ConfigPaths,
}

pub(super) fn open(spec: &'static Spec, pid: u16, ctx: &DriverCtx) -> Opened {
    let page = if pid == DONGLE { 0xff90 } else { 0xff00 };
    let (dev, instance) = open_node_where(pid, |i, d| {
        i == Some(2) && crate::hid::usage_pages(d).contains(&page)
    })?;
    info!("{} open via {}", spec.name, dev.path.display());
    let mut a = MouseAdapter {
        spec,
        id: DeviceId {
            vendor_id: super::SONY,
            product_id: pid,
            instance,
        },
        dongle: pid == DONGLE,
        dev: Some(Box::new(dev)),
        settings: hako_core::config::load_device_settings(&ctx.paths, super::SONY, spec.pids[0])?,
        battery: BatteryState {
            label: "Mouse".into(),
            percent: None,
            charging: false,
            present: false,
        },
        profile: 1,
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

impl MouseAdapter {
    /// Send one command and return the data of its reply, if one arrives.
    fn request(&mut self, cmd: u8, index: u8, profile: u8, data: &[u8]) -> Result<Option<Vec<u8>>> {
        let dev = self.dev.as_mut().context("not connected")?;
        dev.drain();
        dev.write(&packet(cmd, index, profile, data))?;
        let deadline = Instant::now() + REPLY_WAIT;
        let mut buf = [0u8; 64];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let n = match dev.read_timeout(&mut buf, left) {
                Ok(n) => n,
                Err(e) if e.downcast_ref::<std::io::Error>().is_some() => return Err(e),
                Err(_) => return Ok(None),
            };
            if let Some(d) = reply_data(&buf[..n], cmd, index) {
                return Ok(Some(d.to_vec()));
            }
        }
    }

    fn poll(&mut self) -> Result<()> {
        self.last_poll = Some(Instant::now());
        if self.dongle {
            let rf = self.request(GET, IDX_RF, 0, &[])?;
            if rf.as_deref().and_then(|d| d.first()) != Some(&RF_CONNECTED) {
                self.battery = BatteryState {
                    percent: None,
                    charging: false,
                    present: false,
                    ..self.battery.clone()
                };
                return Ok(());
            }
        }
        if let Some(d) = self.request(GET, IDX_INFO, 0, &[])? {
            if let Some(&p @ 1..=4) = d.get(16) {
                self.profile = p;
            }
        }
        let level = self.request(GET, IDX_BATTERY, 0, &[])?;
        let Some(level) = level.as_deref().and_then(|d| d.first().copied()) else {
            bail!("no battery reply");
        };
        self.battery.percent = battery_percent(level);
        self.battery.present = self.battery.percent.is_some();
        if let Some(c) = self.request(GET, IDX_CHARGE, 0, &[])? {
            self.battery.charging = c.first() == Some(&1);
        }
        if let Some(f) = self
            .request(GET, IDX_FUNCTION, self.profile, &[])?
            .as_deref()
            .and_then(parse_function)
        {
            merge_setting(&mut self.settings, K::Dpi, SettingValue::Int(f.dpi));
            merge_setting(&mut self.settings, K::ReportRate, SettingValue::Int(f.rate));
            merge_setting(
                &mut self.settings,
                K::AngleSnapping,
                SettingValue::Bool(f.snap),
            );
        }
        debug!("{} battery {:?}", self.spec.name, self.battery);
        Ok(())
    }

    fn write(&mut self, key: K, value: &SettingValue) -> Result<()> {
        let n = value.as_int().unwrap_or(0).max(0);
        let (index, data): (u8, Vec<u8>) = match key {
            K::Dpi => (
                IDX_DPI,
                dpi_bytes(n.min(i64::from(u16::MAX)) as u16).to_vec(),
            ),
            K::ReportRate => (
                IDX_RATE,
                vec![RATES
                    .iter()
                    .position(|&r| r == n)
                    .context("unsupported report rate")? as u8],
            ),
            K::AngleSnapping => (IDX_SNAP, vec![u8::from(value.as_bool() == Some(true))]),
            _ => bail!("unsupported setting {}", key.as_str()),
        };
        let reply = self
            .request(SET, index, self.profile, &data)?
            .context("mouse did not acknowledge the write")?;
        set_result(&reply)
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
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: controls(),
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
        if self.dev.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
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

    #[test]
    fn packets_match_hub_layout() {
        let p = packet(SET, IDX_DPI, 2, &dpi_bytes(800));
        // (800 - 50) / 50 = 15
        assert_eq!(p[..7], [0, 0x20, 0x07, 0x20, 2, 15, 0]);
        assert_eq!(dpi_bytes(100), [1, 0]);
        assert_eq!(dpi_bytes(30000), [(599u16 & 0xff) as u8, 2]);
        let g = packet(GET, IDX_FUNCTION, 1, &[]);
        assert_eq!(g[..5], [0, 0xa0, 0x05, 0x10, 0]);
        assert_eq!(g.len(), 65);
    }

    #[test]
    fn replies_and_function_block() {
        let r = [0xa0, 0x05, 0x10, 9, 3, 1, 0, 15, 0, 1, 80, 255, 0, 0];
        let d = reply_data(&r, GET, IDX_FUNCTION).unwrap();
        assert_eq!(
            parse_function(d),
            Some(Function {
                rate: 4000,
                snap: true,
                dpi: 800
            })
        );
        assert!(reply_data(&r, GET, IDX_BATTERY).is_none());
        assert!(parse_function(&[9, 0, 0, 0, 0]).is_none());
        assert!(set_result(&[0xec, 0xac]).is_ok());
        assert!(set_result(&[0xec, 0xfa, 4])
            .unwrap_err()
            .to_string()
            .contains("error 4"));
        assert_eq!(battery_percent(0), Some(12));
        assert_eq!(battery_percent(4), None);
    }

    /// Mouse double answering GETs from stored blocks and acknowledging SETs.
    struct Fake {
        sent: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        replies: std::collections::VecDeque<Vec<u8>>,
        rf: u8,
    }

    impl Link for Fake {
        fn write(&mut self, r: &[u8]) -> Result<()> {
            self.sent.lock().unwrap().push(r.to_vec());
            let (cmd, index, field) = (r[1], r[2], r[3]);
            let data: Vec<u8> = match (cmd, index) {
                (GET, IDX_RF) => vec![self.rf],
                (GET, IDX_INFO) => {
                    let mut d = vec![0; 18];
                    d[16] = 2;
                    d
                }
                (GET, IDX_BATTERY) => vec![2],
                (GET, IDX_CHARGE) => vec![1],
                (GET, IDX_FUNCTION) => vec![1, 0, 1, 15, 0, 0, 80, 255, 0, 0],
                (SET, _) => vec![0xec, 0xac],
                _ => return Ok(()),
            };
            let mut reply = vec![cmd, index, field, data.len() as u8];
            reply.extend(data);
            self.replies.push_back(reply);
            Ok(())
        }

        fn read_timeout(&mut self, buf: &mut [u8], _: Duration) -> Result<usize> {
            let r = self.replies.pop_front().context("timeout")?;
            buf[..r.len()].copy_from_slice(&r);
            Ok(r.len())
        }

        fn drain(&mut self) {
            self.replies.clear();
        }
    }

    fn adapter(
        dongle: bool,
        rf: u8,
    ) -> (MouseAdapter, std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
        let dir = std::env::temp_dir().join(format!("hako-sony-mouse-{}", std::process::id()));
        let sent = Default::default();
        let a = MouseAdapter {
            spec: &MOUSE_A,
            id: DeviceId {
                vendor_id: super::super::SONY,
                product_id: if dongle { DONGLE } else { WIRED },
                instance: "test".into(),
            },
            dongle,
            dev: Some(Box::new(Fake {
                sent: std::sync::Arc::clone(&sent),
                replies: Default::default(),
                rf,
            })),
            settings: SettingsMap::new(),
            battery: BatteryState {
                label: "Mouse".into(),
                percent: None,
                charging: false,
                present: false,
            },
            profile: 1,
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

    #[test]
    fn poll_reads_state_and_writes_use_the_current_profile() {
        let (mut a, sent) = adapter(true, RF_CONNECTED);
        a.poll().unwrap();
        assert_eq!(a.profile, 2);
        assert_eq!(a.battery.percent, Some(62));
        assert!(a.battery.charging);
        let got = |a: &MouseAdapter, k| hako_core::config::get_setting(&a.settings, k).cloned();
        assert_eq!(got(&a, K::Dpi), Some(SettingValue::Int(800)));
        assert_eq!(got(&a, K::ReportRate), Some(SettingValue::Int(1000)));
        assert_eq!(got(&a, K::AngleSnapping), Some(SettingValue::Bool(false)));
        assert!(sent.lock().unwrap().iter().all(|r| r[1] == GET));
        a.write(K::ReportRate, &SettingValue::Int(4000)).unwrap();
        assert_eq!(
            sent.lock().unwrap().last().unwrap()[1..6],
            [SET, IDX_RATE, 0x20, 1, 3]
        );
        assert!(a.write(K::ReportRate, &SettingValue::Int(750)).is_err());
        assert!(a.write(K::Volume, &SettingValue::Int(1)).is_err());
    }

    #[test]
    fn dongle_without_mouse_is_offline_and_reads_nothing_else() {
        let (mut a, sent) = adapter(true, 0xdd);
        a.poll().unwrap();
        assert!(!a.battery.present);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn every_control_validates_and_encodes() {
        let (mut a, _) = adapter(false, 0);
        for meta in controls() {
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
                assert!(meta.validate(&v).is_ok(), "{:?}", meta.key);
                a.write(meta.key, &v).unwrap();
            }
        }
    }
}
