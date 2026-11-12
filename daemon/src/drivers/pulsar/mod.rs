//! Pulsar mice: battery and charging state of the wireless ones, and the
//! onboard settings of every model as blocked placeholders.
//!
//! Sources: pulsar-mouse-linux (MIT) `src/pulsar_mouse/drivers/*.py`,
//! `docs/protocol-x2-crazylight.md` <https://github.com/packerlschupfer/pulsar-mouse-linux>,
//! decoded from Pulsar Fusion captures; python-pulsar-mouse-tool (MIT)
//! `pulsar.py` <https://github.com/andrewrabert/python-pulsar-mouse-tool>,
//! the X2 V2 Mini on `3554:f507` / `f508`; rustar `src/main.rs`
//! <https://github.com/GroobleDierne/rustar> (`3554:f509`, writes only);
//! the proposed Linux hid-pulsar patch (GPL, IDs only:
//! `3554:f507/f508/f509` = wired / 1 kHz / 4 kHz); linux-hardware.org device
//! pages for `3710:5402`, `3554:f507`, `f508`, `f509` (USB names).
//!
//! Two protocols:
//! - **Sonix** (`3710:1401..1404`, `3401`, `3409`, `3410`, dongles `5402`,
//!   `5403`, `5404`): 64-byte feature report on interface 3 (no report id).
//!   `[0]` direction (00 request; reply 01 answered, 02 pending, 05 no link
//!   to the mouse), `[1]` category, `[2]` register (bit 7 = read), `[3]`
//!   sub-register, `[6]` profile (0 = active), `[62..64]` LE u16 sum of
//!   `[0..62]`. The host polls GET_REPORT until the reply echoes `[1..3]`
//!   with status 01. Battery: `08 81 01` → `[6]` percent (`ff`: the dongle
//!   has no mouse); charging: `08 83 01` → `[6]` 1 on the cable. Read on the
//!   dongles and on a cabled X3 / X3 LHD (pulsar-mouse-linux
//!   `xlite_v4_wireless.py`, `feinmann8k.py`, `x3.py`).
//! - **Nordic** (`3554:f507`/`f508`/`f509`, and under the Sonix VID the
//!   CrazyLight `5406` / `3414` / `3527` and Feinmann F01 Noctua `5504` /
//!   `7507`): 17-byte report id 8 on interface 1, written as an output report
//!   and answered as an input report among the mouse's own input. `[1]`
//!   command, `[16]` = `0x55 - sum([0..16])`. Battery `08 04` → `[6]`
//!   percent, `[7]` on power (both sources agree). Identity `08 01` with a
//!   nonce at `[6..10]` and `08` at `[5]` → `[2]` status 0, `[10]` cid,
//!   `[11]` mid (pulsar-mouse-linux `nordic.py`, from Fusion's "CheckPsd").
//!   `3554:f507..f509` are only claimed once the mouse answers it.
//!
//! Every setting (DPI stages, report rate, lift-off, debounce, lighting,
//! sleep timer) is stored in the mouse's onboard profiles and no layout has
//! two independent sources, so they are `.persistent().blocked(..)`
//! placeholders and nothing here writes to a mouse. Research:
//! `docs/onboard-writes/pulsar.md`.

use crate::drivers::{DeviceAdapter, Model, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const PULSAR: u16 = 0x3710;
const NORDIC_VID: u16 = 0x3554;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Sonix,
    Nordic,
}

struct Spec {
    protocol: Protocol,
    /// Answers the battery query (a wireless mouse, its dongle or its cable).
    battery: bool,
    max_dpi: f64,
    max_rate: i64,
    /// PIDs other products may share: claimed only after the identity query.
    identify: bool,
}

const SONIX_WIRED: Spec = Spec {
    protocol: Protocol::Sonix,
    battery: false,
    max_dpi: 26000.0,
    max_rate: 1000,
    identify: false,
};
/// Fusion2 line (XS-1 sensor): Xlite v4 on its cable.
const FUSION2_WIRED: Spec = Spec {
    max_dpi: 32000.0,
    ..SONIX_WIRED
};
/// X3 / X3 LHD on the cable and the 1K dongle: answer the power category.
const FUSION2: Spec = Spec {
    battery: true,
    ..FUSION2_WIRED
};
const FUSION2_8K: Spec = Spec {
    max_rate: 8000,
    ..FUSION2
};
const CRAZYLIGHT: Spec = Spec {
    protocol: Protocol::Nordic,
    battery: true,
    max_dpi: 32000.0,
    max_rate: 8000,
    identify: false,
};
const CRAZYLIGHT_WIRED: Spec = Spec {
    max_rate: 1000,
    ..CRAZYLIGHT
};
const NOCTUA: Spec = Spec {
    max_dpi: 42000.0,
    ..CRAZYLIGHT
};
const NORDIC: Spec = Spec {
    max_dpi: 26000.0,
    max_rate: 1000,
    identify: true,
    ..CRAZYLIGHT
};

macro_rules! pulsar {
    ($name:literal, $vid:expr, [$($pid:literal),+], $spec:expr) => {
        Model {
            vendor_id: $vid,
            product_ids: &[$($pid),+],
            kind: DeviceKind::Mouse,
            name: $name,
            usbfs: false,
            multi: false,
            open: |pid, _| open($name, $vid, pid, &$spec),
        }
    };
}

pub const MODELS: &[Model] = &[
    pulsar!("Pulsar Xlite Wired", PULSAR, [0x1401], SONIX_WIRED),
    pulsar!("Pulsar X2 Wired", PULSAR, [0x1402], SONIX_WIRED),
    pulsar!("Pulsar X2H Wired Medium", PULSAR, [0x1403], SONIX_WIRED),
    pulsar!("Pulsar X2A Medium Wired", PULSAR, [0x1404], SONIX_WIRED),
    pulsar!("Pulsar Xlite v4", PULSAR, [0x3401], FUSION2_WIRED),
    pulsar!("Pulsar X3", PULSAR, [0x3409], FUSION2),
    pulsar!("Pulsar X3 LHD", PULSAR, [0x3410], FUSION2),
    pulsar!("Pulsar 1K Wireless Dongle", PULSAR, [0x5402], FUSION2),
    pulsar!("Pulsar 8K Wireless Dongle", PULSAR, [0x5403], FUSION2_8K),
    pulsar!("Pulsar Feinmann 8K / F01", PULSAR, [0x5404], FUSION2_8K),
    pulsar!(
        "Pulsar CrazyLight Wireless Dongle",
        PULSAR,
        [0x5406],
        CRAZYLIGHT
    ),
    pulsar!(
        "Pulsar X2 CrazyLight Wired",
        PULSAR,
        [0x3414],
        CRAZYLIGHT_WIRED
    ),
    pulsar!(
        "Pulsar X3 LHD CrazyLight Medium Wired",
        PULSAR,
        [0x3527],
        CRAZYLIGHT_WIRED
    ),
    pulsar!(
        "Pulsar Feinmann F01 Noctua Edition",
        PULSAR,
        [0x5504, 0x7507],
        NOCTUA
    ),
    pulsar!(
        "Pulsar Wireless (Nordic)",
        NORDIC_VID,
        [0xf507, 0xf508, 0xf509],
        NORDIC
    ),
];

/// Model names by mid for cid 6, from Fusion's Cfg.ini as listed in
/// pulsar-mouse-linux `nordic.py` (`MODELS_BY_MID`).
const NORDIC_MIDS: &[(u8, &str)] = &[
    (0x01, "Pulsar Xlite Wireless"),
    (0x02, "Pulsar Xlite Mini Wireless"),
    (0x03, "Pulsar X2 Mini Wireless"),
    (0x04, "Pulsar X2 Wireless"),
    (0x05, "Pulsar X2 Mini Bruce Lee"),
    (0x06, "Pulsar X2 Bruce Lee"),
    (0x07, "Pulsar Xlite V3"),
    (0x08, "Pulsar X2A Wireless"),
    (0x09, "Pulsar X2A Mini Wireless"),
    (0x0a, "Pulsar X2V2 Wireless"),
    (0x0b, "Pulsar X2V2 Mini Wireless"),
    (0x0c, "Pulsar X2H Wireless"),
    (0x0d, "Pulsar X2H Mini Wireless"),
    (0x0e, "Pulsar Xlite V3 Mini"),
    (0x0f, "Pulsar Xlite V3 Large"),
    (0x10, "Pulsar Xlite V3 Custom Optical"),
    (0x11, "Pulsar Xlite V3 Custom Mech"),
    (0x12, "Pulsar X2 Custom Optical"),
    (0x13, "Pulsar X2 Custom Mech"),
    (0x14, "Pulsar X2 Custom Optical"),
    (0x15, "Pulsar X2 Custom Mech"),
];

fn mid_name(cid: u8, mid: u8) -> Option<&'static str> {
    if cid != 6 {
        return None;
    }
    NORDIC_MIDS.iter().find(|(m, _)| *m == mid).map(|(_, n)| *n)
}

// ── Sonix ──────────────────────────────────────────────────────────────────

const SONIX_IFACE: u32 = 3;
const SONIX_POLL: Duration = Duration::from_millis(15);
const SONIX_REPLY: Duration = Duration::from_secs(1);
const STATUS_ANSWERED: u8 = 0x01;
const STATUS_NO_LINK: u8 = 0x05;

/// A request with profile 0 and no payload.
fn sonix_packet(cat: u8, reg: u8, sub: u8) -> [u8; 64] {
    let mut p = [0u8; 64];
    p[1] = cat;
    p[2] = reg;
    p[3] = sub;
    let sum = p[..62]
        .iter()
        .fold(0u16, |a, &b| a.wrapping_add(u16::from(b)));
    p[62..].copy_from_slice(&sum.to_le_bytes());
    p
}

/// The status byte of `r` if it answers `req` (same category and register).
fn sonix_status(r: &[u8], req: &[u8; 64]) -> Option<u8> {
    (r.len() >= 64 && r[1..3] == req[1..3]).then_some(r[0])
}

/// The answer to a read, or `None` when the dongle has no link to the mouse.
fn sonix_read(dev: &HidrawDevice, cat: u8, reg: u8, sub: u8) -> Result<Option<[u8; 64]>> {
    let req = sonix_packet(cat, reg, sub);
    let mut buf = [0u8; 65];
    buf[1..].copy_from_slice(&req);
    dev.set_feature(&mut buf)?;
    let deadline = Instant::now() + SONIX_REPLY;
    // ponytail: no resend of a request the dongle lost; the next poll retries.
    loop {
        std::thread::sleep(SONIX_POLL);
        let mut buf = [0u8; 65];
        dev.get_feature(&mut buf)?;
        match sonix_status(&buf[1..], &req) {
            Some(STATUS_ANSWERED) => return Ok(buf[1..].try_into().ok()),
            Some(STATUS_NO_LINK) => return Ok(None),
            _ if Instant::now() >= deadline => {
                bail!("no answer to {cat:#04x}/{reg:#04x}/{sub:#04x}")
            }
            _ => {}
        }
    }
}

/// Battery percent from the `08 81 01` answer; `ff` means no mouse.
fn sonix_percent(r: &[u8; 64]) -> Option<u8> {
    (r[6] <= 100).then_some(r[6])
}

fn sonix_battery(dev: &HidrawDevice) -> Result<Option<(u8, bool)>> {
    let Some(pct) = sonix_read(dev, 0x08, 0x81, 0x01)?.and_then(|r| sonix_percent(&r)) else {
        return Ok(None);
    };
    let charging = sonix_read(dev, 0x08, 0x83, 0x01)?.is_some_and(|r| r[6] != 0);
    Ok(Some((pct, charging)))
}

// ── Nordic ─────────────────────────────────────────────────────────────────

const NORDIC_IFACE: u32 = 1;
const NORDIC_REPORT: u8 = 0x08;
const CMD_IDENTITY: u8 = 0x01;
const CMD_POWER: u8 = 0x04;
const NORDIC_REPLY: Duration = Duration::from_secs(1);
/// The nonce Fusion sent in the capture quoted in pulsar-mouse-linux
/// `nordic.py`; the answer does not depend on it.
const NONCE: [u8; 4] = [0x5f, 0x5f, 0x32, 0x5d];

fn nordic_checksum(b: &[u8]) -> u8 {
    b.iter().fold(0x55u8, |a, &x| a.wrapping_sub(x))
}

/// `body` is bytes 2..16.
fn nordic_packet(cmd: u8, body: [u8; 14]) -> [u8; 17] {
    let mut p = [0u8; 17];
    p[0] = NORDIC_REPORT;
    p[1] = cmd;
    p[2..16].copy_from_slice(&body);
    p[16] = nordic_checksum(&p[..16]);
    p
}

fn identity_request() -> [u8; 17] {
    let mut body = [0u8; 14];
    body[3] = 0x08;
    body[4..8].copy_from_slice(&NONCE);
    nordic_packet(CMD_IDENTITY, body)
}

/// Whether `r` is a well-formed answer to `cmd`. Mouse input reports share
/// the node and have other report ids.
fn nordic_reply(r: &[u8], cmd: u8) -> bool {
    r.len() == 17 && r[0] == NORDIC_REPORT && r[1] == cmd && r[16] == nordic_checksum(&r[..16])
}

/// (cid, mid); `None` when the mouse refuses (status) or says nothing.
fn parse_identity(r: &[u8; 17]) -> Option<(u8, u8)> {
    (r[2] == 0 && r[10] != 0 && r[11] != 0).then_some((r[10], r[11]))
}

/// (percent, on power).
fn parse_power(r: &[u8; 17]) -> Option<(u8, bool)> {
    (r[6] <= 100).then_some((r[6], r[7] != 0))
}

/// Sends `req` and waits for its answer; `None` if none comes (a dongle
/// whose mouse is asleep).
fn nordic_command(dev: &mut HidrawDevice, req: &[u8; 17]) -> Result<Option<[u8; 17]>> {
    // Input reports queue up between polls; a full queue would drop the reply.
    dev.drain(Duration::ZERO);
    dev.write(req)?;
    let deadline = Instant::now() + NORDIC_REPLY;
    let mut buf = [0u8; 64];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(n) = dev.read_timeout(&mut buf, left) else {
            break;
        };
        if nordic_reply(&buf[..n], req[1]) {
            return Ok(buf[..17].try_into().ok());
        }
    }
    Ok(None)
}

fn nordic_battery(dev: &mut HidrawDevice) -> Result<Option<(u8, bool)>> {
    let req = nordic_packet(CMD_POWER, [0; 14]);
    Ok(nordic_command(dev, &req)?.and_then(|r| parse_power(&r)))
}

// ── Controls ───────────────────────────────────────────────────────────────

const SONIX_BLOCKED: &str = "stored in the mouse's onboard profiles; single source \
(pulsar-mouse-linux)";
const NORDIC_BLOCKED: &str = "stored in the mouse's onboard profile memory; \
only small tools agree on the layout, each for other PIDs and not shown to be independent \
(python-pulsar-mouse-tool, rustar; pulsar-mouse-linux builds on the first)";

/// Every onboard setting the sources document, as blocked placeholders.
fn controls(spec: &Spec) -> Vec<SettingMeta> {
    use SettingKey::*;
    let (reason, sleep) = match spec.protocol {
        Protocol::Sonix => (SONIX_BLOCKED, (30.0, 900.0, 30.0)),
        Protocol::Nordic => (NORDIC_BLOCKED, (10.0, 600.0, 10.0)),
    };
    let mut c = Vec::new();
    for (i, key) in [DpiStage1, DpiStage2, DpiStage3, DpiStage4, DpiStage5]
        .into_iter()
        .enumerate()
    {
        let label = format!("DPI stage {}", i + 1);
        c.push(SettingMeta::slider(key, &label, 50.0, spec.max_dpi, 10.0).unit("dpi"));
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
        [125, 250, 500, 1000, 2000, 4000, 8000]
            .into_iter()
            .filter(|&r| r <= spec.max_rate)
            .map(|r| SettingOption::int(r, &format!("{r} Hz")))
            .collect(),
    ));
    c.push(
        SettingMeta::dropdown(
            LiftOffDistance,
            "Lift-off distance",
            vec![SettingOption::int(1, "1 mm"), SettingOption::int(2, "2 mm")],
        )
        .unit("mm"),
    );
    c.push(SettingMeta::slider(Debounce, "Click debounce", 0.0, 20.0, 1.0).unit("ms"));
    c.push(SettingMeta::toggle(AngleSnapping, "Angle snapping"));
    c.push(SettingMeta::dropdown(
        Effect,
        "LED effect",
        [
            ("off", "Off"),
            ("steady", "Steady"),
            ("breathe", "Breathing"),
        ]
        .map(|(v, l)| SettingOption::text(v, l))
        .to_vec(),
    ));
    c.push(SettingMeta::slider(
        Brightness,
        "LED brightness",
        0.0,
        255.0,
        1.0,
    ));
    if spec.battery {
        let (min, max, step) = sleep;
        c.push(SettingMeta::slider(PmShutdown, "Sleep timer", min, max, step).unit("s"));
    }
    c.into_iter()
        .map(|m| m.persistent().blocked(reason))
        .collect()
}

// ── Adapter ────────────────────────────────────────────────────────────────

const POLL: Duration = Duration::from_secs(30);

fn open(name: &'static str, vid: u16, pid: u16, spec: &'static Spec) -> Opened {
    let iface = match spec.protocol {
        Protocol::Sonix => SONIX_IFACE,
        Protocol::Nordic => NORDIC_IFACE,
    };
    let info = find_hidraw(vid, pid, Some(iface))
        .with_context(|| format!("no hidraw node for {vid:04x}:{pid:04x} interface {iface}"))?;
    let mut dev = if spec.battery || spec.identify {
        Some(HidrawDevice::open(&info.path)?)
    } else {
        None
    };
    let mut shown = name.to_string();
    if let (true, Some(d)) = (spec.identify, dev.as_mut()) {
        let ids = nordic_command(d, &identity_request())
            .ok()
            .flatten()
            .and_then(|r| parse_identity(&r));
        let Some((cid, mid)) = ids else {
            info!("{vid:04x}:{pid:04x} did not answer the Pulsar identity query; not claimed");
            return Ok(vec![]);
        };
        if let Some(n) = mid_name(cid, mid) {
            shown = n.into();
        }
    }
    info!("{shown} open via {}", info.path.display());
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq
    };
    Ok(vec![Box::new(Mouse {
        name: shown,
        id: DeviceId {
            vendor_id: vid,
            product_id: pid,
            instance,
        },
        spec,
        dev,
        battery: BatteryState {
            label: "Mouse".into(),
            percent: None,
            charging: false,
            present: false,
        },
        last_poll: None,
        last_error: None,
    })])
}

struct Mouse {
    name: String,
    id: DeviceId,
    spec: &'static Spec,
    /// Open only for models that answer the battery query.
    dev: Option<HidrawDevice>,
    battery: BatteryState,
    last_poll: Option<Instant>,
    last_error: Option<String>,
}

impl Mouse {
    fn poll(&mut self) -> Result<()> {
        let dev = self.dev.as_mut().context("not connected")?;
        let reading = match self.spec.protocol {
            Protocol::Sonix => sonix_battery(dev)?,
            Protocol::Nordic => nordic_battery(dev)?,
        };
        self.battery.present = reading.is_some();
        if let Some((pct, charging)) = reading {
            self.battery.percent = Some(pct);
            self.battery.charging = charging;
        }
        Ok(())
    }
}

impl DeviceAdapter for Mouse {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: self.name.clone(),
            connected: true,
            batteries: if self.spec.battery {
                vec![self.battery.clone()]
            } else {
                vec![]
            },
            settings: Default::default(),
            controls: controls(self.spec),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, _value: SettingValue) -> Result<()> {
        bail!(
            "{} is stored in the mouse's onboard profile; not written",
            key.as_str()
        )
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() || self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return Ok(());
        }
        self.last_poll = Some(Instant::now());
        let result = self.poll();
        self.last_error = result.as_ref().err().map(|e| e.to_string());
        if result.is_err() {
            self.battery.present = false;
        }
        result
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sonix_packets_and_checksum() {
        // Battery read: cat 08, reg 81, sub 01, profile 0.
        let p = sonix_packet(0x08, 0x81, 0x01);
        assert_eq!(p[..7], [0x00, 0x08, 0x81, 0x01, 0x00, 0x00, 0x00]);
        assert!(p[7..62].iter().all(|&b| b == 0));
        assert_eq!(p[62..], [0x8a, 0x00]);
        // Charging read; the sum is little-endian u16 and carries.
        assert_eq!(sonix_packet(0x08, 0x83, 0x01)[62..], [0x8c, 0x00]);
        assert_eq!(sonix_packet(0xff, 0xff, 0xff)[62..], [0xfd, 0x02]);
    }

    #[test]
    fn sonix_reply_status_and_battery() {
        let req = sonix_packet(0x08, 0x81, 0x01);
        let reply = |status: u8, reg: u8, pct: u8| {
            let mut r = req;
            r[0] = status;
            r[2] = reg;
            r[6] = pct;
            r
        };
        assert_eq!(sonix_status(&reply(0x01, 0x81, 87), &req), Some(0x01));
        assert_eq!(sonix_status(&reply(0x02, 0x81, 0), &req), Some(0x02));
        assert_eq!(sonix_status(&reply(0x05, 0x81, 0), &req), Some(0x05));
        // An answer to the charging read is not ours.
        assert_eq!(sonix_status(&reply(0x01, 0x83, 1), &req), None);
        assert_eq!(sonix_status(&[0x01, 0x08, 0x81], &req), None);
        assert_eq!(sonix_percent(&reply(0x01, 0x81, 87)), Some(87));
        assert_eq!(sonix_percent(&reply(0x01, 0x81, 100)), Some(100));
        assert_eq!(sonix_percent(&reply(0x01, 0x81, 0xff)), None);
    }

    #[test]
    fn nordic_checksum_matches_capture() {
        // python-pulsar-mouse-tool: the unsolicited 0x0a report
        // 08:0a:00:00:00:0a:04:00:00:00:00:00:00:00:00:00:35.
        let mut r = [0u8; 17];
        r[..7].copy_from_slice(&[0x08, 0x0a, 0x00, 0x00, 0x00, 0x0a, 0x04]);
        assert_eq!(nordic_checksum(&r[..16]), 0x35);
        r[16] = 0x35;
        assert!(nordic_reply(&r, 0x0a));
        assert!(!nordic_reply(&r, CMD_POWER));
        r[16] = 0x36;
        assert!(!nordic_reply(&r, 0x0a));
        assert!(!nordic_reply(&r[..16], 0x0a));
    }

    #[test]
    fn nordic_power() {
        let req = nordic_packet(CMD_POWER, [0; 14]);
        let mut want = [0u8; 17];
        want[0] = 0x08;
        want[1] = 0x04;
        want[16] = 0x49;
        assert_eq!(req, want);
        // 90 % (the CrazyLight capture's 0x5a), on the cable, 4201 mV.
        let mut body = [0u8; 14];
        body[4..8].copy_from_slice(&[0x5a, 0x01, 0x10, 0x69]);
        let r = nordic_packet(CMD_POWER, body);
        assert!(nordic_reply(&r, CMD_POWER));
        assert_eq!(parse_power(&r), Some((90, true)));
        body[4] = 0xff;
        assert_eq!(parse_power(&nordic_packet(CMD_POWER, body)), None);
    }

    #[test]
    fn nordic_identity_matches_capture() {
        // > 08 01 00 00 00 08 5f 5f 32 5d ...
        let req = identity_request();
        assert_eq!(
            req[..10],
            [0x08, 0x01, 0x00, 0x00, 0x00, 0x08, 0x5f, 0x5f, 0x32, 0x5d]
        );
        assert!(req[10..16].iter().all(|&b| b == 0));
        assert_eq!(req[16], nordic_checksum(&req[..16]));
        // < 08 01 00 00 00 08 c4 f7 f3 d3 06 07 02 ...  (an Xlite V3)
        let mut body = [0u8; 14];
        body[3..11].copy_from_slice(&[0x08, 0xc4, 0xf7, 0xf3, 0xd3, 0x06, 0x07, 0x02]);
        let r = nordic_packet(CMD_IDENTITY, body);
        assert_eq!(parse_identity(&r), Some((6, 7)));
        assert_eq!(mid_name(6, 7), Some("Pulsar Xlite V3"));
        // CrazyLight answers cid 0x57; its mids mean other models.
        assert_eq!(mid_name(0x57, 3), None);
        assert_eq!(mid_name(6, 0x40), None);
        // A refusal (status 1, as to a zero nonce) or no ids: not claimed.
        let mut refused = r;
        refused[2] = 0x01;
        assert_eq!(parse_identity(&refused), None);
        body[9] = 0;
        assert_eq!(parse_identity(&nordic_packet(CMD_IDENTITY, body)), None);
    }

    #[test]
    fn model_table_matches_sources() {
        let ids: Vec<(u16, u16)> = MODELS
            .iter()
            .flat_map(|m| m.product_ids.iter().map(move |&p| (m.vendor_id, p)))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            assert!(seen.insert(id), "{id:04x?} twice");
        }
        // Lamzu / Compx PIDs on the shared VID and the Areson IDs are not ours.
        for p in [0xf50d, 0xf50f, 0xf510] {
            assert!(!ids.contains(&(NORDIC_VID, p)));
        }
        assert!(MODELS.iter().all(|m| m.vendor_id != 0x25a7));
        for m in MODELS {
            assert_eq!(m.kind, DeviceKind::Mouse);
            assert!(!m.usbfs && !m.multi);
            assert!(m.name.starts_with("Pulsar "));
        }
        assert_eq!(ids.len(), 18);
    }

    #[test]
    fn every_setting_is_a_blocked_onboard_placeholder() {
        for spec in [&SONIX_WIRED, &FUSION2_8K, &CRAZYLIGHT_WIRED, &NORDIC] {
            let c = controls(spec);
            for m in &c {
                assert!(m.persistent && m.blocked.is_some(), "{:?}", m.key);
                assert!(m.validate(&SettingValue::Int(1)).is_err());
            }
            assert_eq!(
                c.iter().any(|m| m.key == SettingKey::PmShutdown),
                spec.battery
            );
            let rates = &c.iter().find(|m| m.key == SettingKey::ReportRate).unwrap();
            let top = rates.options.last().unwrap().value.clone();
            assert_eq!(top, SettingValue::Int(spec.max_rate));
        }
    }

    /// Read-only: one battery query on whichever Pulsar is plugged in.
    #[test]
    #[ignore]
    fn hw_battery() {
        let ctx = crate::drivers::DriverCtx {
            paths: hako_core::config::ConfigPaths::xdg().unwrap(),
            audio: std::sync::Arc::new(hako_audio::AudioService::new()),
            open: Vec::new(),
            vendor_id: PULSAR,
        };
        for m in MODELS {
            for &pid in m.product_ids {
                let Ok(mut a) = (m.open)(pid, &ctx) else {
                    continue;
                };
                if let Some(a) = a.first_mut() {
                    let r = a.tick();
                    eprintln!("{}: {r:?} {:?}", m.name, a.snapshot().batteries);
                    return;
                }
            }
        }
        panic!("no Pulsar mouse found");
    }
}
