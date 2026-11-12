//! Glorious PC Gaming Race mice: battery level of the wireless ones, and the
//! wired Model O / D (`wired.rs`).
//!
//! - Model O / O- / D / D- Wireless (SinoWealth VID 0x258a, cable + dongle
//!   PIDs): mxw (MIT) `src/util/devices.rs`, `src/report/battery.rs`
//!   <https://github.com/korkje/mxw>, cross-checked with glorious-gui (MIT)
//!   `glorious_gui.py` <https://github.com/Ard3ny/glorious-gui> and OpenRGB
//!   (GPL, IDs only) `SinowealthControllerDetect.cpp`. Interface 2, 65-byte
//!   feature report (id 0): request `[3] 02 [4] 02 [6] 83`, reply `[1]` status
//!   (`a1` ok, `a4` asleep, `a0` waking), `[6]` echoes `83`, `[8]` percent.
//!   On the cable the mouse charges ("fully charged" at 100 %).
//! - Model I 2 Wireless (PixArt VID 0x093a): glorious-i2 (MIT) `PROTOCOL.md`,
//!   `battery/mouse-battery-monitor` <https://github.com/Krodity/glorious-i2>
//!   and glorious-i2-battery (GPL, layout only) `README.md`
//!   <https://github.com/tvollstaedt/glorious-i2-battery>. The mouse cannot be
//!   asked; the receiver pushes input report 6 `06 fb <percent> <toggle>` on
//!   interface 1 every 20-40 s while the mouse is used. No frames arrive while
//!   the cable (`821a`) is plugged in, which is the only charging signal.
//!
//! DPI, report rate and lighting of the wireless mice are blocked
//! placeholders: every documented path writes onboard profile storage
//! (OpenRGB marks the GMOW modes `MODE_FLAG_AUTOMATIC_SAVE`; glorious-i2
//! writes onboard profiles 1-3), none can be read back, and no write here is
//! confident enough (Model O/D Wireless: mxw alone for DPI, rate, debounce and
//! lift-off, OpenRGB and mxw disagree on the static-color packet, and mxw's
//! README says its author bricked mice while writing it; Model I 2: one
//! write-only source). Nothing here writes a wireless setting. Research:
//! `docs/onboard-writes/glorious.md`.

mod wired;

use crate::drivers::{DeviceAdapter, Model, Opened};
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const SINOWEALTH: u16 = 0x258a;
const PIXART: u16 = 0x093a;
const I2_RECEIVER: u16 = 0x821d;
const I2_CABLE: u16 = 0x821a;

/// SinoWealth wireless mouse: cable PID first (mxw prefers it when both are
/// present), then the dongle.
macro_rules! sino {
    ($name:literal, $cable:literal, $dongle:literal) => {
        Model {
            vendor_id: SINOWEALTH,
            product_ids: &[$cable, $dongle],
            kind: DeviceKind::Mouse,
            name: $name,
            usbfs: false,
            multi: false,
            open: |pid, _| SinoMouse::open($name, pid, pid == $cable),
        }
    };
}

pub const MODELS: &[Model] = &[
    sino!("Glorious Model O Wireless", 0x2011, 0x2022),
    sino!("Glorious Model D Wireless", 0x2012, 0x2023),
    Model {
        vendor_id: PIXART,
        product_ids: &[I2_RECEIVER, I2_CABLE],
        kind: DeviceKind::Mouse,
        name: "Glorious Model I 2 Wireless",
        usbfs: false,
        multi: false,
        open: |pid, _| I2Mouse::open(pid),
    },
    sino!("Glorious Model O- Wireless", 0x2013, 0x2024),
    sino!("Glorious Model D- Wireless", 0x2014, 0x2025),
    Model {
        vendor_id: SINOWEALTH,
        product_ids: &[wired::MODEL_O],
        kind: DeviceKind::Mouse,
        name: "Glorious Model O / O-",
        usbfs: false,
        multi: false,
        open: |pid, _| wired::open("Glorious Model O / O-", pid),
    },
    Model {
        vendor_id: SINOWEALTH,
        product_ids: &[wired::MODEL_D],
        kind: DeviceKind::Mouse,
        name: "Glorious Model D / D-",
        usbfs: false,
        multi: false,
        open: |pid, _| wired::open("Glorious Model D / D-", pid),
    },
];

const GMOW_BLOCKED: &str = "one source (mxw), whose author bricked mice writing it; \
nothing can be read back";
const I2_BLOCKED: &str = "write-only protocol (nothing can be read back), one source (glorious-i2)";

/// Blocked placeholders for the onboard settings of a wireless mouse.
/// `max_dpi`: Model O/D Wireless sensor 19000, Model I 2 26000 (glorious-i2).
fn wireless_controls(reason: &str, max_dpi: f64) -> Vec<SettingMeta> {
    use SettingKey::*;
    let mut c = Vec::new();
    for (i, key) in [DpiStage1, DpiStage2, DpiStage3, DpiStage4, DpiStage5]
        .into_iter()
        .enumerate()
    {
        let label = format!("DPI stage {}", i + 1);
        c.push(SettingMeta::slider(key, &label, 100.0, max_dpi, 50.0).unit("dpi"));
    }
    c.push(SettingMeta::dropdown(
        DpiStageActive,
        "Active DPI stage",
        (1..=5)
            .map(|i| SettingOption::int(i, &i.to_string()))
            .collect(),
    ));
    c.push(SettingMeta::dropdown(
        ReportRate,
        "Report rate",
        [125, 250, 500, 1000]
            .map(|r| SettingOption::int(r, &format!("{r} Hz")))
            .to_vec(),
    ));
    c.push(SettingMeta::slider(Debounce, "Click debounce", 0.0, 16.0, 2.0).unit("ms"));
    c.push(
        SettingMeta::dropdown(
            LiftOffDistance,
            "Lift-off distance",
            vec![SettingOption::int(1, "1 mm"), SettingOption::int(2, "2 mm")],
        )
        .unit("mm"),
    );
    c.push(SettingMeta::slider(Brightness, "LED brightness", 0.0, 100.0, 1.0).unit("%"));
    c.into_iter()
        .map(|m| m.persistent().blocked(reason))
        .collect()
}

/// Open interface `iface`; returns the device and a stable instance key.
fn open_node(vid: u16, pid: u16, iface: u32) -> Result<(HidrawDevice, String)> {
    let info = enumerate_hidraw()?
        .into_iter()
        .find(|d| d.vendor_id == vid && d.product_id == pid && d.interface == Some(iface))
        .with_context(|| format!("no hidraw node for {vid:04x}:{pid:04x} interface {iface}"))?;
    let dev = HidrawDevice::open(&info.path)?;
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq
    };
    Ok((dev, instance))
}

fn mouse_battery() -> BatteryState {
    BatteryState {
        label: "Mouse".into(),
        percent: None,
        charging: false,
        present: false,
    }
}

fn snapshot(
    id: &DeviceId,
    name: &str,
    connected: bool,
    battery: &BatteryState,
    last_error: &Option<String>,
    blocked: (&str, f64),
) -> DeviceSnapshot {
    DeviceSnapshot {
        id: id.clone(),
        kind: DeviceKind::Mouse,
        name: name.into(),
        connected,
        batteries: vec![battery.clone()],
        settings: Default::default(),
        controls: wireless_controls(blocked.0, blocked.1),
        last_error: last_error.clone(),
    }
}

// ── SinoWealth Model O / D Wireless ────────────────────────────────────────

const POLL: Duration = Duration::from_secs(30);

fn battery_request() -> [u8; 65] {
    let mut b = [0u8; 65];
    b[3] = 0x02;
    b[4] = 0x02;
    b[6] = 0x83;
    b
}

#[derive(Debug, PartialEq, Eq)]
enum Reading {
    Level(u8),
    Asleep,
}

/// mxw reads a 0 % level as 1 %; unknown status bytes are ignored.
fn parse_battery(r: &[u8]) -> Option<Reading> {
    if r.get(6) != Some(&0x83) {
        return None;
    }
    match r.get(1)? {
        0xa1 => Some(Reading::Level((*r.get(8)?).clamp(1, 100))),
        0xa4 | 0xa0 => Some(Reading::Asleep),
        _ => None,
    }
}

struct SinoMouse {
    name: &'static str,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    wired: bool,
    battery: BatteryState,
    last_poll: Option<Instant>,
    failures: u8,
    last_error: Option<String>,
}

impl SinoMouse {
    fn open(name: &'static str, pid: u16, wired: bool) -> Opened {
        let (dev, instance) = open_node(SINOWEALTH, pid, 2)?;
        info!("{name} open via {}", dev.path.display());
        Ok(vec![Box::new(Self {
            name,
            id: DeviceId {
                vendor_id: SINOWEALTH,
                product_id: pid,
                instance,
            },
            dev: Some(dev),
            wired,
            battery: mouse_battery(),
            last_poll: None,
            failures: 0,
            last_error: None,
        })])
    }

    fn poll(&mut self) -> Result<()> {
        let dev = self.dev.as_ref().context("not connected")?;
        dev.set_feature(&mut battery_request())?;
        std::thread::sleep(Duration::from_millis(50));
        let mut reply = [0u8; 65];
        dev.get_feature(&mut reply)?;
        match parse_battery(&reply) {
            Some(Reading::Level(pct)) => {
                self.battery.present = true;
                self.battery.percent = Some(pct);
                self.battery.charging = self.wired && pct < 100;
            }
            Some(Reading::Asleep) => self.battery.present = false,
            None => {}
        }
        Ok(())
    }
}

impl DeviceAdapter for SinoMouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        snapshot(
            &self.id,
            self.name,
            self.dev.is_some(),
            &self.battery,
            &self.last_error,
            (GMOW_BLOCKED, 19000.0),
        )
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("{} has no setting {}", self.name, key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return Ok(());
        }
        self.last_poll = Some(Instant::now());
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

// ── PixArt Model I 2 Wireless ──────────────────────────────────────────────

/// Frames are pushed between mouse reports on the same node, so each tick
/// keeps reading for most of its interval.
const I2_TICK: Duration = Duration::from_millis(200);
const I2_READ: Duration = Duration::from_millis(180);
const CABLE_CHECK: Duration = Duration::from_secs(2);

/// `06 fb <percent> <toggle>`; the all-zero frame that follows and the `f9`
/// (button) / `f6` (profile) frames are not levels. The toggle is not a
/// charge flag.
fn parse_frame(r: &[u8]) -> Option<u8> {
    match r {
        [0x06, 0xfb, pct @ 1..=100, _, ..] => Some(*pct),
        _ => None,
    }
}

fn cable_present() -> bool {
    enumerate_hidraw().is_ok_and(|nodes| {
        nodes
            .iter()
            .any(|d| d.vendor_id == PIXART && d.product_id == I2_CABLE)
    })
}

struct I2Mouse {
    id: DeviceId,
    /// Receiver node; `None` on the cable, which sends no battery frames.
    dev: Option<HidrawDevice>,
    connected: bool,
    battery: BatteryState,
    cable_checked: Option<Instant>,
    last_error: Option<String>,
}

impl I2Mouse {
    const NAME: &'static str = "Glorious Model I 2 Wireless";

    fn open(pid: u16) -> Opened {
        let (dev, instance) = if pid == I2_RECEIVER {
            let (dev, instance) = open_node(PIXART, pid, 1)?;
            info!("{} open via {}", Self::NAME, dev.path.display());
            (Some(dev), instance)
        } else {
            let info = crate::hid::find_hidraw(PIXART, pid, Some(1))?;
            (None, info.path.display().to_string())
        };
        let mut battery = mouse_battery();
        battery.charging = pid == I2_CABLE;
        battery.present = pid == I2_CABLE;
        Ok(vec![Box::new(Self {
            id: DeviceId {
                vendor_id: PIXART,
                product_id: pid,
                instance,
            },
            dev,
            connected: true,
            battery,
            cable_checked: None,
            last_error: None,
        })])
    }
}

impl DeviceAdapter for I2Mouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        snapshot(
            &self.id,
            Self::NAME,
            self.connected,
            &self.battery,
            &self.last_error,
            (I2_BLOCKED, 26000.0),
        )
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("{} has no setting {}", Self::NAME, key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        let start = Instant::now();
        let mut buf = [0u8; 64];
        while let Some(left) = I2_READ.checked_sub(start.elapsed()) {
            let Ok(n) = dev.read_timeout(&mut buf, left) else {
                break;
            };
            if let Some(pct) = parse_frame(&buf[..n]) {
                self.battery.percent = Some(pct);
                self.battery.present = true;
            }
        }
        if self
            .cable_checked
            .is_none_or(|t| t.elapsed() >= CABLE_CHECK)
        {
            self.cable_checked = Some(Instant::now());
            self.battery.charging = cable_present();
            self.battery.present |= self.battery.charging;
        }
        Ok(())
    }

    fn tick_interval(&self) -> Duration {
        if self.dev.is_some() {
            I2_TICK
        } else {
            Duration::from_secs(1)
        }
    }

    fn shutdown(&mut self) {
        self.dev = None;
        self.connected = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_query_matches_mxw() {
        let r = battery_request();
        assert_eq!(r.len(), 65);
        assert_eq!(r[..7], [0x00, 0x00, 0x00, 0x02, 0x02, 0x00, 0x83]);
        assert!(r[7..].iter().all(|&b| b == 0));
    }

    #[test]
    fn battery_reply_parsing() {
        let reply = |status: u8, echo: u8, pct: u8| {
            let mut r = [0u8; 65];
            r[1] = status;
            r[6] = echo;
            r[8] = pct;
            r
        };
        assert_eq!(
            parse_battery(&reply(0xa1, 0x83, 57)),
            Some(Reading::Level(57))
        );
        assert_eq!(
            parse_battery(&reply(0xa1, 0x83, 0)),
            Some(Reading::Level(1))
        );
        assert_eq!(
            parse_battery(&reply(0xa1, 0x83, 100)),
            Some(Reading::Level(100))
        );
        assert_eq!(parse_battery(&reply(0xa4, 0x83, 0)), Some(Reading::Asleep));
        assert_eq!(parse_battery(&reply(0xa0, 0x83, 0)), Some(Reading::Asleep));
        // Unknown status (a2/a3) or a reply to another command.
        assert_eq!(parse_battery(&reply(0xa2, 0x83, 50)), None);
        assert_eq!(parse_battery(&reply(0xa1, 0x81, 50)), None);
        assert_eq!(parse_battery(&[0xa1]), None);
    }

    #[test]
    fn i2_battery_frames() {
        // Captured pair from glorious-i2's monitor notes.
        assert_eq!(parse_frame(&[0x06, 0xfb, 0x32, 0x00, 0, 0, 0, 0]), Some(50));
        assert_eq!(parse_frame(&[0x06, 0xfb, 0x32, 0x01, 0, 0, 0, 0]), Some(50));
        assert_eq!(parse_frame(&[0x06, 0, 0, 0, 0, 0, 0, 0]), None);
        assert_eq!(parse_frame(&[0x06, 0xf9, 0x80, 0x01]), None);
        assert_eq!(parse_frame(&[0x06, 0xf6, 0x02, 0x00]), None);
        assert_eq!(parse_frame(&[0x06, 0xfb, 0x00, 0x00]), None);
        assert_eq!(parse_frame(&[0x06, 0xfb, 0x65, 0x00]), None);
        assert_eq!(parse_frame(&[0x06, 0xfb, 0x32]), None);
        // Mouse input reports share the node.
        assert_eq!(parse_frame(&[0x01, 0xfb, 0x32, 0x00]), None);
    }

    #[test]
    fn model_table_matches_sources() {
        let ids: Vec<(u16, &[u16])> = MODELS
            .iter()
            .map(|m| (m.vendor_id, m.product_ids))
            .collect();
        assert_eq!(
            ids,
            [
                (SINOWEALTH, &[0x2011, 0x2022][..]),
                (SINOWEALTH, &[0x2012, 0x2023][..]),
                (PIXART, &[0x821d, 0x821a][..]),
                (SINOWEALTH, &[0x2013, 0x2024][..]),
                (SINOWEALTH, &[0x2014, 0x2025][..]),
                (SINOWEALTH, &[0x0036][..]),
                (SINOWEALTH, &[0x0033][..]),
            ]
        );
        let mut seen = std::collections::HashSet::new();
        for m in MODELS {
            assert_eq!(m.kind, DeviceKind::Mouse);
            assert!(!m.usbfs && !m.multi);
            assert!(m.name.starts_with("Glorious "));
            for &pid in m.product_ids {
                assert!(seen.insert((m.vendor_id, pid)), "{pid:04x} twice");
            }
        }
    }

    #[test]
    fn wireless_settings_are_blocked_placeholders() {
        let id = DeviceId {
            vendor_id: SINOWEALTH,
            product_id: 0x2022,
            instance: "t".into(),
        };
        let s = snapshot(
            &id,
            "x",
            true,
            &mouse_battery(),
            &None,
            (GMOW_BLOCKED, 19000.0),
        );
        assert!(s.settings.is_empty());
        assert_eq!(s.controls.len(), 10);
        for c in &s.controls {
            assert!(c.persistent && c.blocked.is_some(), "{:?}", c.key);
            assert!(c.validate(&SettingValue::Int(2)).is_err());
        }
        assert_eq!(s.batteries.len(), 1);
    }

    /// Read-only: one battery query on a plugged-in Model O/D Wireless.
    #[test]
    #[ignore]
    fn hw_sinowealth_battery() {
        for m in MODELS.iter().filter(|m| m.vendor_id == SINOWEALTH) {
            for &pid in m.product_ids {
                let Ok(mut a) = SinoMouse::open(m.name, pid, pid == m.product_ids[0]) else {
                    continue;
                };
                a[0].tick().unwrap();
                eprintln!("{}: {:?}", m.name, a[0].snapshot().batteries);
                return;
            }
        }
        panic!("no Glorious SinoWealth wireless mouse found");
    }
}
