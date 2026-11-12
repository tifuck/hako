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
//! - Model O 2 Wireless (PixArt, dongle `822d`, cable `822a`): o2ctl (MIT)
//!   `docs/PROTOCOL.md`, `o2ctl/protocol.py`, `o2ctl/device.py`
//!   <https://github.com/danexpo/o2ctl>. The same interface-1 frames, but
//!   `06 fb <percent> <charging>` carries a charge flag. Over the cable the
//!   mouse sends no frames and is asked instead: feature report 3
//!   `03 08 fa 14`, wait 50 ms, read report 3 back as `03 08 14 <percent>
//!   <charging>` (up to three tries, once a minute). Bluetooth (`822b`) is
//!   left to the generic driver, as the Model I 2's is.
//!
//! DPI, report rate and lighting of the wireless mice are blocked
//! placeholders: every documented path writes onboard profile storage
//! (OpenRGB marks the GMOW modes `MODE_FLAG_AUTOMATIC_SAVE`; glorious-i2
//! writes onboard profiles 1-3), none can be read back, and no write here is
//! confident enough (Model O/D Wireless: mxw alone for DPI, rate, debounce and
//! lift-off, OpenRGB and mxw disagree on the static-color packet, and mxw's
//! README says its author bricked mice while writing it; Model I 2: one
//! write-only source; Model O 2: write-only, o2ctl only). Nothing here writes
//! a wireless setting. Research:
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

/// PixArt wireless mouse: receiver PID first, it is opened when both are
/// present.
macro_rules! pixart {
    ($spec:ident) => {
        Model {
            vendor_id: PIXART,
            product_ids: &[$spec.receiver, $spec.cable],
            kind: DeviceKind::Mouse,
            name: $spec.name,
            usbfs: false,
            multi: false,
            open: |pid, _| PixArtMouse::open(&$spec, pid),
        }
    };
}

pub const MODELS: &[Model] = &[
    sino!("Glorious Model O Wireless", 0x2011, 0x2022),
    sino!("Glorious Model D Wireless", 0x2012, 0x2023),
    pixart!(I2),
    pixart!(O2),
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

// ── PixArt Model I 2 / O 2 Wireless ────────────────────────────────────────

struct PixArt {
    name: &'static str,
    receiver: u16,
    cable: u16,
    blocked: &'static str,
    /// Battery frame byte 3 is a charge flag (o2ctl); on the I 2 it toggles.
    charge_flag: bool,
    /// The cable answers the `08` battery query (o2ctl).
    cable_query: bool,
}

const I2: PixArt = PixArt {
    name: "Glorious Model I 2 Wireless",
    receiver: 0x821d,
    cable: 0x821a,
    blocked: "write-only protocol (nothing can be read back), one source (glorious-i2)",
    charge_flag: false,
    cable_query: false,
};

const O2: PixArt = PixArt {
    name: "Glorious Model O 2 Wireless",
    receiver: 0x822d,
    cable: 0x822a,
    blocked: "write-only protocol (nothing can be read back), one source (o2ctl)",
    charge_flag: true,
    cable_query: true,
};

/// Frames are pushed between mouse reports on the same node, so each tick
/// keeps reading for most of its interval.
const I2_TICK: Duration = Duration::from_millis(200);
const I2_READ: Duration = Duration::from_millis(180);
const CABLE_CHECK: Duration = Duration::from_secs(2);
/// o2ctl asks the cable once a minute.
const CABLE_POLL: Duration = Duration::from_secs(60);

/// `06 fb <percent> <flag>`; the all-zero frame that follows and the `f9`
/// (button) / `f6` (profile) frames are not levels.
fn parse_frame(r: &[u8]) -> Option<(u8, bool)> {
    match r {
        [0x06, 0xfb, pct @ 1..=100, flag, ..] => Some((*pct, *flag != 0)),
        _ => None,
    }
}

/// Model O 2 cable battery query (o2ctl `battery_query_packet`, cable route `fa`).
fn cable_query() -> [u8; 64] {
    let mut b = [0u8; 64];
    b[..4].copy_from_slice(&[0x03, 0x08, 0xfa, 0x14]);
    b
}

/// Feature report 3 after a query, report id first: `03 08 14 <percent>
/// <charging>`. o2ctl shows 0 % as 1 %; `ff` = no answer yet.
fn parse_query_reply(r: &[u8]) -> Option<(u8, bool)> {
    match r {
        [0x03, 0x08, 0x14, pct @ 0..=100, charging, ..] => Some(((*pct).max(1), *charging != 0)),
        _ => None,
    }
}

fn query_cable_battery(dev: &HidrawDevice) -> Result<Option<(u8, bool)>> {
    for _ in 0..3 {
        dev.set_feature(&mut cable_query())?;
        std::thread::sleep(Duration::from_millis(50));
        let mut reply = [0u8; 64];
        reply[0] = 0x03;
        dev.get_feature(&mut reply)?;
        if let Some(r) = parse_query_reply(&reply) {
            return Ok(Some(r));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(None)
}

fn cable_present(cable: u16) -> bool {
    enumerate_hidraw().is_ok_and(|nodes| {
        nodes
            .iter()
            .any(|d| d.vendor_id == PIXART && d.product_id == cable)
    })
}

struct PixArtMouse {
    spec: &'static PixArt,
    id: DeviceId,
    /// Opened on the cable only when it answers battery queries.
    wired: bool,
    /// Receiver node, or the cable node (`wired`); `None` on a cable that
    /// sends no battery frames and cannot be asked.
    dev: Option<HidrawDevice>,
    connected: bool,
    battery: BatteryState,
    /// The last frame's charge flag (`spec.charge_flag` only).
    flag_charging: bool,
    /// Last cable presence check, or last query when `wired`.
    cable_checked: Option<Instant>,
    last_error: Option<String>,
}

impl PixArtMouse {
    fn open(spec: &'static PixArt, pid: u16) -> Opened {
        let wired = pid == spec.cable;
        let (dev, instance) = if !wired || spec.cable_query {
            let (dev, instance) = open_node(PIXART, pid, 1)?;
            info!("{} open via {}", spec.name, dev.path.display());
            (Some(dev), instance)
        } else {
            let info = crate::hid::find_hidraw(PIXART, pid, Some(1))?;
            (None, info.path.display().to_string())
        };
        let mut battery = mouse_battery();
        battery.charging = wired;
        battery.present = wired;
        Ok(vec![Box::new(Self {
            spec,
            id: DeviceId {
                vendor_id: PIXART,
                product_id: pid,
                instance,
            },
            wired,
            dev,
            connected: true,
            battery,
            flag_charging: false,
            cable_checked: None,
            last_error: None,
        })])
    }

    fn poll_cable(&mut self) -> Result<()> {
        let Some(dev) = self.dev.as_ref() else {
            return Ok(());
        };
        if self.cable_checked.is_some_and(|t| t.elapsed() < CABLE_POLL) {
            return Ok(());
        }
        self.cable_checked = Some(Instant::now());
        match query_cable_battery(dev) {
            Ok(reading) => {
                if let Some((pct, charging)) = reading {
                    self.battery.percent = Some(pct);
                    self.battery.charging = charging;
                }
                self.last_error = None;
                Ok(())
            }
            Err(e) => {
                self.last_error = Some(e.to_string());
                Err(e)
            }
        }
    }
}

impl DeviceAdapter for PixArtMouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        snapshot(
            &self.id,
            self.spec.name,
            self.connected,
            &self.battery,
            &self.last_error,
            (self.spec.blocked, 26000.0),
        )
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!("{} has no setting {}", self.spec.name, key.as_str())
    }

    fn tick(&mut self) -> Result<()> {
        if self.wired {
            return self.poll_cable();
        }
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        let start = Instant::now();
        let mut buf = [0u8; 64];
        while let Some(left) = I2_READ.checked_sub(start.elapsed()) {
            let Ok(n) = dev.read_timeout(&mut buf, left) else {
                break;
            };
            if let Some((pct, flag)) = parse_frame(&buf[..n]) {
                self.battery.percent = Some(pct);
                self.battery.present = true;
                self.flag_charging = self.spec.charge_flag && flag;
            }
        }
        if self
            .cable_checked
            .is_none_or(|t| t.elapsed() >= CABLE_CHECK)
        {
            self.cable_checked = Some(Instant::now());
            self.battery.charging = cable_present(self.spec.cable) || self.flag_charging;
            self.battery.present |= self.battery.charging;
        }
        Ok(())
    }

    fn tick_interval(&self) -> Duration {
        if self.dev.is_some() && !self.wired {
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
        assert_eq!(
            parse_frame(&[0x06, 0xfb, 0x32, 0x00, 0, 0, 0, 0]),
            Some((50, false))
        );
        assert_eq!(
            parse_frame(&[0x06, 0xfb, 0x32, 0x01, 0, 0, 0, 0]),
            Some((50, true))
        );
        // o2ctl PROTOCOL.md: `06 fb 2f 00` = 47 % through the O 2 dongle.
        assert_eq!(parse_frame(&[0x06, 0xfb, 0x2f, 0x00]), Some((47, false)));
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
    fn o2_cable_battery_query_matches_o2ctl() {
        let q = cable_query();
        assert_eq!(q[..4], [0x03, 0x08, 0xfa, 0x14]);
        assert!(q[4..].iter().all(|&b| b == 0));
        // o2ctl PROTOCOL.md, verified over the cable: `08 14 64 01` = 100 %, charging.
        let reply = |b: &[u8]| {
            let mut r = [0u8; 64];
            r[0] = 0x03;
            r[1..=b.len()].copy_from_slice(b);
            r
        };
        assert_eq!(
            parse_query_reply(&reply(&[0x08, 0x14, 0x64, 0x01])),
            Some((100, true))
        );
        assert_eq!(
            parse_query_reply(&reply(&[0x08, 0x14, 0x40, 0x00])),
            Some((64, false))
        );
        assert_eq!(
            parse_query_reply(&reply(&[0x08, 0x14, 0x00, 0x00])),
            Some((1, false))
        );
        // No answer yet, or a reply to another command.
        assert_eq!(parse_query_reply(&reply(&[0x08, 0x14, 0xff, 0x00])), None);
        assert_eq!(parse_query_reply(&reply(&[0x08, 0x06, 0x40, 0x00])), None);
        assert_eq!(parse_query_reply(&[0u8; 64]), None);
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
                (PIXART, &[0x822d, 0x822a][..]),
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
