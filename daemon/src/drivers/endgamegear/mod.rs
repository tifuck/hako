//! Endgame Gear OP1w 4k / XM2w 4k wireless mice, v1 and v2: battery, the
//! stored configuration, and on the XM2w 4k (v1) by cable the sensor block.
//!
//! Sources (MIT, layouts only):
//! - vole `re/PROTOCOL.md`, `src/vole/protocol.h`
//!   <https://github.com/johanneszab/vole>: the vendor tools' binaries plus
//!   44 captures, tested on OP1w 4k and OP1w 4k v2. IDs (§10): every tool
//!   opens the shared dongle `1970` and its mouse's own cabled PID, which cmd
//!   `0x0e` also reports over the dongle.
//! - xm2ctl `PROTOCOL.md` <https://github.com/Mannomatico/xm2ctl>: captures
//!   of the XM2w 4k tool, tested on an XM2w 4k (`1968` / `1970`).
//! - XM2w Control `src-tauri/src/xm2w/mod.rs` <https://github.com/imqstr/xm2w>:
//!   XM2w 4k (`1968`), tested on hardware.
//!
//! Commands are 64-byte feature reports `a1 <cmd> <target> <len>` on the
//! vendor interface (usage page 0xff01, interface 1). The reply comes back on
//! report `a1` with the status at `[1]` and the payload from `[16]`; the
//! 1024-byte configuration block (cmd `0x12`) through the 1041-byte report
//! `a0`. There is no checksum. Over the dongle a `0f` probe precedes every
//! command (the vendor tool does; vole §14: after a failure nothing answers
//! until a probe does); a cabled mouse refuses it (vole §13).
//!
//! Over the dongle the mouse is only asked while it is awake: xm2ctl saw a
//! command to a sleeping XM2w block the receiver until it was replugged. The
//! battery comes from the reports the dongle pushes (`03 b4 <percent>`,
//! `03 b1 01` link up / `03 b1 f0` asleep, vole §4a, xm2ctl), and the
//! configuration is read right after one of them.
//!
//! Every setting lives in a block the mouse stores (`0x14` sensor/CPI, `0x15`
//! polling/power/filters, `0x16` buttons) and is rewritten whole. The one
//! write offered is the `0x14` block on the XM2w 4k (v1) by cable, where all
//! three sources agree on it byte for byte: read the block, change one field,
//! write, read back and compare. Everything else is a blocked placeholder
//! that shows the stored value. Research: `docs/onboard-writes/endgamegear.md`.

use crate::drivers::{DeviceAdapter, Model, Opened};
use crate::hid::{enumerate_hidraw, report_ids, HidrawDevice};
use anyhow::{bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::merge_setting;
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const VID: u16 = 0x3367;
const DONGLE: u16 = 0x1970;
const XM2W_V1: u16 = 0x1968;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gen {
    V1,
    V2,
}

/// vole `kModels`: the PID cmd `0x0e` reports, which is also the cabled PID.
struct Spec {
    pid: u16,
    name: &'static str,
    gen: Gen,
}

const MICE: [Spec; 4] = [
    Spec {
        pid: 0x1972,
        name: "Endgame Gear OP1w 4k",
        gen: Gen::V1,
    },
    Spec {
        pid: 0x1984,
        name: "Endgame Gear OP1w 4k v2",
        gen: Gen::V2,
    },
    Spec {
        pid: XM2W_V1,
        name: "Endgame Gear XM2w 4k",
        gen: Gen::V1,
    },
    Spec {
        pid: 0x1982,
        name: "Endgame Gear XM2w 4k v2",
        gen: Gen::V2,
    },
];

fn spec(pid: u16) -> Option<&'static Spec> {
    MICE.iter().find(|s| s.pid == pid)
}

/// One model: the dongle is shared by all four mice and only cmd `0x0e`
/// tells them apart. Cables first, so a cabled mouse is preferred.
pub const MODELS: &[Model] = &[Model {
    vendor_id: VID,
    product_ids: &[0x1972, 0x1984, XM2W_V1, 0x1982, DONGLE],
    kind: DeviceKind::Mouse,
    name: "Endgame Gear OP1w 4k / XM2w 4k (v1, v2)",
    usbfs: false,
    multi: false,
    open: |pid, _| open(pid),
}];

const REPORT: u8 = 0xa1;
const BULK: u8 = 0xa0;
const BULK_LEN: usize = 1041;
const PAYLOAD: usize = 16;
const BLOB_LEN: usize = 1024;

const CMD_MOUSE_INFO: u8 = 0x0e;
const CMD_PROBE: u8 = 0x0f;
const CMD_CONFIG: u8 = 0x12;
const CMD_SENSOR: u8 = 0x14;
const CMD_BATTERY: u8 = 0xb4;
const TARGET_DONGLE: u8 = 0x01;
const TARGET_MOUSE: u8 = 0x0f;
const SENSOR_LEN: usize = 28;

const OK: u8 = 0x01;
/// Not answered yet: `03` busy (vole), `08` reply pending (xm2ctl).
const PENDING: [u8; 2] = [0x03, 0x08];

/// 1 ms in production; the sleeps are multiples of it (vole §2: 150 ms per
/// command, 360 ms for the block, +200 ms per busy round).
const PACE: Duration = Duration::from_millis(1);
const CABLE_REFRESH: Duration = Duration::from_secs(60);
/// Over the dongle, at most this often, and only right after a report.
const DONGLE_REFRESH: Duration = Duration::from_secs(300);

const ONE_SOURCE: &str =
    "one source (vole) for this model; every change rewrites a settings block in the mouse";
const RECEIVER: &str = "connect the cable to change it: over the receiver a command to a \
sleeping mouse can block the receiver until it is replugged (xm2ctl)";
const NOT_IDENTIFIED: &str = "the mouse has not identified itself yet (move it)";
const ACTIVE_V1: &str = "the v1 firmware ignores a stage change from software (vole); use the \
CPI button";
const POWER_BLOCK: &str = "not implemented: this block (cmd 0x15) also rewrites every click \
filter";

/// Feature-report access, so the protocol can run against a fake.
trait Feature {
    fn get(&self, buf: &mut [u8]) -> Result<()>;
    fn set(&self, buf: &mut [u8]) -> Result<()>;
}

impl Feature for HidrawDevice {
    fn get(&self, buf: &mut [u8]) -> Result<()> {
        self.get_feature(buf)
    }
    fn set(&self, buf: &mut [u8]) -> Result<()> {
        self.set_feature(buf)
    }
}

fn request(cmd: u8, target: u8, len: u8) -> [u8; 64] {
    let mut b = [0u8; 64];
    b[..4].copy_from_slice(&[REPORT, cmd, target, len]);
    b
}

/// Sends `req` and reads the reply (report `a0` for the configuration
/// block): the status and the whole reply. Pending replies are re-read.
fn transact(dev: &impl Feature, req: &mut [u8; 64], pace: Duration) -> Result<(u8, Vec<u8>)> {
    let bulk = req[1] == CMD_CONFIG;
    dev.set(req)?;
    std::thread::sleep(pace * if bulk { 360 } else { 150 });
    let mut buf = vec![0u8; if bulk { BULK_LEN } else { 64 }];
    for round in 1..=4 {
        buf.fill(0);
        buf[0] = if bulk { BULK } else { REPORT };
        dev.get(&mut buf)?;
        // Both reports answer with `a1` in byte 0 (vole §2).
        ensure!(buf[0] == REPORT, "unexpected reply report {:#04x}", buf[0]);
        if !PENDING.contains(&buf[1]) || round == 4 {
            break;
        }
        std::thread::sleep(pace * 200 * round);
    }
    Ok((buf[1], buf))
}

fn probe(dev: &impl Feature, pace: Duration) -> Result<()> {
    for target in [TARGET_DONGLE, TARGET_MOUSE] {
        if transact(dev, &mut request(CMD_PROBE, target, 0), pace)?.0 == OK {
            return Ok(());
        }
    }
    bail!("the dongle does not answer")
}

/// Runs a read command; returns its payload.
fn query(dev: &impl Feature, cmd: u8, cabled: bool, pace: Duration) -> Result<Vec<u8>> {
    if !cabled {
        probe(dev, pace)?;
    }
    let (status, reply) = transact(dev, &mut request(cmd, 0, 0), pace)?;
    ensure!(
        status == OK,
        "command {cmd:#04x} answered status {status:#04x}"
    );
    Ok(reply[PAYLOAD..].to_vec())
}

fn read_blob(dev: &impl Feature, cabled: bool, pace: Duration) -> Result<Vec<u8>> {
    let mut b = query(dev, CMD_CONFIG, cabled, pace)?;
    b.truncate(BLOB_LEN);
    Ok(b)
}

/// cmd `0x0e`: VID, mouse PID, bootloader PID, firmware (hex digits), LE.
fn parse_mouse_info(p: &[u8]) -> Option<(u16, String)> {
    let p = p.get(..8)?;
    if u16::from_le_bytes([p[0], p[1]]) != VID {
        return None;
    }
    let pid = u16::from_le_bytes([p[2], p[3]]);
    Some((pid, format!("{:x}.{:02x}", p[6], p[7])))
}

fn battery(percent: Option<u8>, charging: bool) -> BatteryState {
    BatteryState {
        label: "Mouse".into(),
        percent,
        charging,
        present: percent.is_some() || charging,
    }
}

/// By cable the level is not a state of charge until it reaches 100: the
/// vendor tool shows "Charging" below that (vole §13).
fn cable_battery(raw: u8) -> BatteryState {
    if raw >= 100 {
        battery(Some(100), false)
    } else {
        battery(None, true)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Battery(u8),
    Link(bool),
}

/// Reports the dongle pushes on the configuration node (report id 3).
fn parse_event(r: &[u8]) -> Option<Event> {
    match r {
        [0x03, 0xb4, pct, ..] => Some(Event::Battery((*pct).min(100))),
        [0x03, 0xb1, 0x01, ..] => Some(Event::Link(true)),
        [0x03, 0xb1, 0xf0 | 0xf1, ..] => Some(Event::Link(false)),
        _ => None,
    }
}

// Configuration block offsets (vole `blob::`, xm2ctl / XM2w Control at +16).
const B_DEEP_SLEEP: usize = 0x03;
const B_POLLING: usize = 0x05;
const B_LOD: usize = 0x09;
const B_SNAPPING: usize = 0x0a;
const B_ACTIVE: usize = 0x0d;
const B_COUNT: usize = 0x0e;
const B_CPI: usize = 0x23;
const B_GLASS: usize = 0x6f;

/// Polling byte → Hz; `80` is 1000 Hz with wireless power saving (v2).
fn polling_hz(b: u8) -> Option<i64> {
    Some(match b {
        0x02 => 4000,
        0x04 => 2000,
        0x08 | 0x80 => 1000,
        0x40 => 125,
        _ => return None,
    })
}

#[derive(Debug, PartialEq)]
struct Config {
    /// X CPI of the four stages.
    stages: [i64; 4],
    active: i64,
    rate: i64,
    lod: SettingValue,
    snapping: bool,
    /// Minutes, 0 = off.
    deep_sleep: i64,
}

fn stage_x(b: &[u8], i: usize) -> i64 {
    let o = B_CPI + 5 * i;
    i64::from(u16::from_le_bytes([b[o + 1], b[o + 2]]))
}

/// Checks and decodes a configuration block; `None` if anything is off.
fn parse_config(b: &[u8], gen: Gen) -> Option<Config> {
    if b.len() < BLOB_LEN {
        return None;
    }
    let count = b[B_COUNT];
    if !(1..=4).contains(&count) || b[B_ACTIVE] > 3 || b[B_SNAPPING] > 1 {
        return None;
    }
    let stages = [0, 1, 2, 3].map(|i| stage_x(b, i));
    if stages.contains(&0) {
        return None;
    }
    // v1: whole millimetres. v2: 0.7 + 0.1 × byte, or whole millimetres in
    // glass mode (vole §4, §12).
    let lod = match (gen, b[B_LOD], b[B_GLASS]) {
        (Gen::V1, mm @ 1..=2, _) | (Gen::V2, mm @ 1..=2, 1) => SettingValue::Int(mm.into()),
        (Gen::V2, n @ 0..=10, 0) => SettingValue::Float(f64::from(n + 7) / 10.0),
        _ => return None,
    };
    let sleep = b[B_DEEP_SLEEP];
    Some(Config {
        stages,
        active: i64::from(b[B_ACTIVE]) + 1,
        rate: polling_hz(b[B_POLLING])?,
        lod,
        snapping: b[B_SNAPPING] == 1,
        deep_sleep: if sleep & 0x80 != 0 { 0 } else { sleep.into() },
    })
}

const STAGES: [SettingKey; 4] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
];

fn show_config(c: &Config) -> SettingsMap {
    let mut s = SettingsMap::new();
    for (key, dpi) in STAGES.into_iter().zip(c.stages) {
        merge_setting(&mut s, key, SettingValue::Int(dpi));
    }
    merge_setting(
        &mut s,
        SettingKey::DpiStageActive,
        SettingValue::Int(c.active),
    );
    merge_setting(&mut s, SettingKey::ReportRate, SettingValue::Int(c.rate));
    merge_setting(&mut s, SettingKey::LiftOffDistance, c.lod.clone());
    merge_setting(
        &mut s,
        SettingKey::AngleSnapping,
        SettingValue::Bool(c.snapping),
    );
    merge_setting(
        &mut s,
        SettingKey::PmShutdown,
        SettingValue::Int(c.deep_sleep),
    );
    s
}

/// The v1 cmd `0x14` payload for a block: `+0` blob 07, `+1` LED on
/// lift-off, `+2` lift-off, `+3` angle snapping, `+4` ripple control, `+5`
/// 0 (angle tuning, never written on v1), `+6` stage count, `+7` active
/// stage, then the four 5-byte CPI records (vole §4, xm2ctl "Write blocks",
/// XM2w Control `write_sensor`).
fn v1_sensor_payload(b: &[u8]) -> [u8; SENSOR_LEN] {
    let mut p = [0u8; SENSOR_LEN];
    p[..5].copy_from_slice(&b[0x07..0x0c]);
    p[6] = b[B_COUNT];
    p[7] = b[B_ACTIVE];
    p[8..].copy_from_slice(&b[B_CPI..B_CPI + 20]);
    p
}

/// v1 CPI limits of the vendor tool (vole `kModels`, xm2ctl).
const V1_CPI: (i64, i64, i64) = (50, 26000, 50);
const V2_CPI: (i64, i64, i64) = (10, 30000, 10);

/// Applies one user change to a v1 block copy.
fn change(b: &mut [u8], key: SettingKey, value: &SettingValue) -> Result<()> {
    match key {
        SettingKey::AngleSnapping => {
            b[B_SNAPPING] = u8::from(value.as_bool().context("expected on/off")?);
        }
        SettingKey::LiftOffDistance => {
            let mm = value.as_int().context("expected a number")?;
            ensure!((1..=2).contains(&mm), "lift-off is 1 or 2 mm");
            b[B_LOD] = mm as u8;
        }
        _ => {
            let i = STAGES
                .iter()
                .position(|k| *k == key)
                .with_context(|| format!("{} is not written", key.as_str()))?;
            let dpi = value.as_int().context("expected a number")?;
            let (min, max, step) = V1_CPI;
            ensure!(
                (min..=max).contains(&dpi) && dpi % step == 0,
                "CPI must be {min}..={max} in steps of {step}"
            );
            // One value for both axes: xySplit 0, X = Y (vole §4).
            let o = B_CPI + 5 * i;
            let le = (dpi as u16).to_le_bytes();
            b[o..o + 5].copy_from_slice(&[0, le[0], le[1], le[0], le[1]]);
        }
    }
    Ok(())
}

/// Read the block, check it, change one field, write cmd `0x14`, read back.
fn write_sensor(
    dev: &impl Feature,
    key: SettingKey,
    value: &SettingValue,
    pace: Duration,
) -> Result<()> {
    let mut blob = read_blob(dev, true, pace)?;
    parse_config(&blob, Gen::V1).context("not writing: unexpected configuration block")?;
    let old = v1_sensor_payload(&blob);
    change(&mut blob, key, value)?;
    let new = v1_sensor_payload(&blob);
    if new == old {
        return Ok(());
    }
    parse_config(&blob, Gen::V1).context("refusing to write an invalid block")?;
    let mut req = request(CMD_SENSOR, TARGET_MOUSE, SENSOR_LEN as u8);
    req[PAYLOAD..PAYLOAD + SENSOR_LEN].copy_from_slice(&new);
    let (status, _) = transact(dev, &mut req, pace)?;
    ensure!(
        status == OK,
        "the mouse refused the write (status {status:#04x})"
    );
    let back = read_blob(dev, true, pace)?;
    ensure!(
        v1_sensor_payload(&back) == new,
        "read-back differs from what was written; check the mouse in the vendor tool"
    );
    Ok(())
}

/// What one read returns.
struct Status {
    spec: &'static Spec,
    firmware: Option<String>,
    battery: Option<BatteryState>,
    config: Config,
}

fn read_status(dev: &impl Feature, usb_pid: u16, pace: Duration) -> Result<Status> {
    let cabled = usb_pid != DONGLE;
    let (spec, firmware) = if cabled {
        (spec(usb_pid).context("not a known mouse")?, None)
    } else {
        let info = query(dev, CMD_MOUSE_INFO, false, pace)?;
        let (pid, fw) = parse_mouse_info(&info).context("unexpected mouse info")?;
        let s = spec(pid).with_context(|| format!("unknown mouse {VID:04x}:{pid:04x}"))?;
        (s, Some(fw))
    };
    let battery = if cabled {
        Some(cable_battery(query(dev, CMD_BATTERY, true, pace)?[0]))
    } else {
        None
    };
    let blob = read_blob(dev, cabled, pace)?;
    let config = parse_config(&blob, spec.gen).context("unexpected configuration block")?;
    Ok(Status {
        spec,
        firmware,
        battery,
        config,
    })
}

/// The configuration node: reports `a1` and `a0`, else interface 1.
fn open_node(pid: u16) -> Result<(HidrawDevice, String)> {
    let nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == VID && d.product_id == pid)
        .collect();
    let info = nodes
        .iter()
        .find(|d| {
            let ids = report_ids(&d.report_descriptor());
            ids.contains(&REPORT) && ids.contains(&BULK)
        })
        .or_else(|| nodes.iter().find(|d| d.interface == Some(1)))
        .with_context(|| format!("no configuration node for {VID:04x}:{pid:04x}"))?;
    let dev = HidrawDevice::open(&info.path)?;
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq.clone()
    };
    Ok((dev, instance))
}

fn open(pid: u16) -> Opened {
    let (dev, instance) = open_node(pid)?;
    info!("Endgame Gear {pid:04x} open via {}", dev.path.display());
    let mut m = Mouse {
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance,
        },
        dev: Some(dev),
        spec: spec(pid),
        battery: battery(None, false),
        state: SettingsMap::new(),
        last_read: None,
        last_error: None,
    };
    if pid != DONGLE {
        m.refresh();
    }
    Ok(vec![Box::new(m)])
}

struct Mouse {
    id: DeviceId,
    dev: Option<HidrawDevice>,
    /// Known from the cable PID, or once the mouse answered cmd `0x0e`.
    spec: Option<&'static Spec>,
    battery: BatteryState,
    state: SettingsMap,
    last_read: Option<Instant>,
    last_error: Option<String>,
}

impl Mouse {
    fn cabled(&self) -> bool {
        self.id.product_id != DONGLE
    }

    fn refresh(&mut self) {
        self.last_read = Some(Instant::now());
        let Some(dev) = self.dev.as_ref() else {
            return;
        };
        match read_status(dev, self.id.product_id, PACE) {
            Ok(s) => {
                if let Some(fw) = s.firmware {
                    info!("{} firmware {fw}", s.spec.name);
                }
                self.spec = Some(s.spec);
                if let Some(b) = s.battery {
                    self.battery = b;
                }
                self.state = show_config(&s.config);
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    /// Why the sensor block (`0x14`) is not written, if it is not.
    fn sensor_block(&self) -> Option<&'static str> {
        match self.spec {
            Some(s) if s.pid == XM2W_V1 && self.id.product_id == XM2W_V1 => None,
            Some(s) if s.pid == XM2W_V1 => Some(RECEIVER),
            Some(_) => Some(ONE_SOURCE),
            None => Some(NOT_IDENTIFIED),
        }
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        let gen = self.spec.map(|s| s.gen);
        let other = match self.spec {
            None => NOT_IDENTIFIED,
            Some(s) if s.pid == XM2W_V1 => POWER_BLOCK,
            Some(_) => ONE_SOURCE,
        };
        let sensor = |m: SettingMeta| {
            let m = m.persistent();
            match self.sensor_block() {
                Some(reason) => m.blocked(reason),
                None => m,
            }
        };
        let (min, max, step) = if gen == Some(Gen::V2) { V2_CPI } else { V1_CPI };
        let mut c: Vec<_> = STAGES
            .into_iter()
            .enumerate()
            .map(|(i, key)| {
                let label = format!("DPI stage {}", i + 1);
                sensor(
                    SettingMeta::slider(key, &label, min as f64, max as f64, step as f64)
                        .unit("dpi"),
                )
            })
            .collect();
        c.push(
            SettingMeta::dropdown(
                DpiStageActive,
                "Active DPI stage",
                (1..=4)
                    .map(|i| SettingOption::int(i, &i.to_string()))
                    .collect(),
            )
            .persistent()
            .blocked(if gen == Some(Gen::V1) {
                ACTIVE_V1
            } else {
                other
            }),
        );
        c.push(
            SettingMeta::dropdown(
                ReportRate,
                "Report rate",
                [125, 1000, 2000, 4000]
                    .map(|r| SettingOption::int(r, &format!("{r} Hz")))
                    .to_vec(),
            )
            .persistent()
            .blocked(other),
        );
        c.push(match gen {
            Some(Gen::V2) => {
                SettingMeta::slider(LiftOffDistance, "Lift-off distance", 0.7, 2.0, 0.1)
                    .unit("mm")
                    .persistent()
                    .blocked(ONE_SOURCE)
            }
            _ => sensor(
                SettingMeta::dropdown(
                    LiftOffDistance,
                    "Lift-off distance",
                    vec![SettingOption::int(1, "1 mm"), SettingOption::int(2, "2 mm")],
                )
                .unit("mm"),
            ),
        });
        c.push(sensor(SettingMeta::toggle(AngleSnapping, "Angle snapping")));
        c.push(
            SettingMeta::slider(PmShutdown, "Deep sleep (0 = off)", 0.0, 120.0, 1.0)
                .unit("min")
                .persistent()
                .blocked(other),
        );
        c
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
            name: self.spec.map_or(MODELS[0].name, |s| s.name).into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.state.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if let Some(reason) = self.sensor_block() {
            bail!("{} is not available: {reason}", key.as_str());
        }
        let dev = self.dev.as_ref().context("not connected")?;
        let result = write_sensor(dev, key, &value, PACE);
        self.refresh();
        if let Err(e) = &result {
            self.last_error = Some(e.to_string());
        }
        result
    }

    fn tick(&mut self) -> Result<()> {
        if self.cabled() {
            if self.dev.is_some() && self.last_read.is_none_or(|t| t.elapsed() >= CABLE_REFRESH) {
                self.refresh();
            }
            return Ok(());
        }
        let Some(dev) = self.dev.as_mut() else {
            return Ok(());
        };
        let mut awake = false;
        let mut buf = [0u8; 64];
        for _ in 0..64 {
            let Ok(n) = dev.read_timeout(&mut buf, Duration::ZERO) else {
                break;
            };
            match parse_event(&buf[..n]) {
                Some(Event::Battery(pct)) => {
                    self.battery = battery(Some(pct), false);
                    awake = true;
                }
                Some(Event::Link(up)) => {
                    awake |= up;
                    self.battery.present = up && self.battery.percent.is_some();
                }
                None => {}
            }
        }
        if awake && self.last_read.is_none_or(|t| t.elapsed() >= DONGLE_REFRESH) {
            self.refresh();
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
    use std::cell::{Cell, RefCell};

    #[test]
    fn model_table_matches_vole() {
        let pids: Vec<u16> = MICE.iter().map(|s| s.pid).collect();
        assert_eq!(pids, [0x1972, 0x1984, 0x1968, 0x1982]);
        assert_eq!(MODELS[0].product_ids[..4], pids[..]);
        assert_eq!(MODELS[0].product_ids[4], 0x1970);
        assert_eq!(MODELS.len(), 1);
    }

    #[test]
    fn request_headers_match_sources() {
        // vole §10: the header bytes every vendor tool uses.
        assert_eq!(request(CMD_MOUSE_INFO, 0, 0)[..4], [0xa1, 0x0e, 0, 0]);
        assert_eq!(
            request(CMD_PROBE, TARGET_DONGLE, 0)[..4],
            [0xa1, 0x0f, 0x01, 0]
        );
        assert_eq!(
            request(CMD_PROBE, TARGET_MOUSE, 0)[..4],
            [0xa1, 0x0f, 0x0f, 0]
        );
        assert_eq!(request(CMD_CONFIG, 0, 0)[..4], [0xa1, 0x12, 0, 0]);
        assert_eq!(request(CMD_BATTERY, 0, 0)[..4], [0xa1, 0xb4, 0, 0]);
        let w = request(CMD_SENSOR, TARGET_MOUSE, SENSOR_LEN as u8);
        assert_eq!(w[..4], [0xa1, 0x14, 0x0f, 0x1c]);
        assert!(w[4..].iter().all(|&b| b == 0));
        assert_eq!(w.len(), 64);
    }

    #[test]
    fn mouse_info_and_events() {
        // vole §12, captured on hardware.
        let v1 = [0x67, 0x33, 0x72, 0x19, 0x71, 0x19, 0x01, 0x08];
        let v2 = [0x67, 0x33, 0x84, 0x19, 0x83, 0x19, 0x01, 0x07];
        assert_eq!(parse_mouse_info(&v1), Some((0x1972, "1.08".into())));
        assert_eq!(parse_mouse_info(&v2), Some((0x1984, "1.07".into())));
        // Hex digits: 1.10 is 01 10 (vole §13, xm2ctl).
        let fw110 = [0x67, 0x33, 0x68, 0x19, 0x67, 0x19, 0x01, 0x10];
        assert_eq!(parse_mouse_info(&fw110), Some((0x1968, "1.10".into())));
        assert_eq!(parse_mouse_info(&[0; 8]), None);
        assert_eq!(parse_mouse_info(&v1[..7]), None);
        // vole §4a.
        let ev = |b: &[u8]| parse_event(b);
        assert_eq!(
            ev(&[3, 0xb4, 0x41, 0xe8, 0x0f, 0, 0, 0]),
            Some(Event::Battery(65))
        );
        assert_eq!(ev(&[3, 0xb1, 0x01, 0, 0, 0, 0, 0]), Some(Event::Link(true)));
        assert_eq!(
            ev(&[3, 0xb1, 0xf0, 0x0a, 0, 0, 0, 0]),
            Some(Event::Link(false))
        );
        assert_eq!(ev(&[3, 0x31, 0x14, 0x10, 0, 0, 0, 0]), None);
        assert_eq!(ev(&[2, 0, 0, 0, 4, 0, 0, 0]), None);
        assert_eq!(cable_battery(55), battery(None, true));
        assert_eq!(cable_battery(100), battery(Some(100), false));
    }

    /// A block with vole's factory defaults: 400/800/1600/3200, 4 stages,
    /// stage 1 active, LED on lift-off, lift-off byte 3.
    fn default_blob() -> Vec<u8> {
        let mut b = vec![0u8; BLOB_LEN];
        b[B_DEEP_SLEEP] = 0x03;
        b[B_POLLING] = 0x02;
        b[0x08] = 1;
        b[B_LOD] = 3;
        b[B_COUNT] = 4;
        b[B_CPI..B_CPI + 20].copy_from_slice(&[
            0x00, 0x90, 0x01, 0x90, 0x01, 0x00, 0x20, 0x03, 0x20, 0x03, 0x00, 0x40, 0x06, 0x40,
            0x06, 0x00, 0x80, 0x0c, 0x80, 0x0c,
        ]);
        b
    }

    #[test]
    fn sensor_payload_matches_vole_defaults() {
        // vole §4, cmd 0x14 factory defaults.
        assert_eq!(
            v1_sensor_payload(&default_blob()),
            [
                0x00, 0x01, 0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x90, 0x01, 0x90, 0x01, 0x00,
                0x20, 0x03, 0x20, 0x03, 0x00, 0x40, 0x06, 0x40, 0x06, 0x00, 0x80, 0x0c, 0x80, 0x0c
            ]
        );
    }

    #[test]
    fn parses_configuration_block() {
        let b = default_blob();
        let c = parse_config(&b, Gen::V2).unwrap();
        assert_eq!(c.stages, [400, 800, 1600, 3200]);
        assert_eq!((c.active, c.rate, c.deep_sleep), (1, 4000, 3));
        assert_eq!(c.lod, SettingValue::Float(1.0));
        assert!(!c.snapping);
        let s = show_config(&c);
        assert_eq!(s.get("dpi_stage_3"), Some(&SettingValue::Int(1600)));
        assert_eq!(s.get("report_rate"), Some(&SettingValue::Int(4000)));
        // v1 reads the byte as millimetres; 3 is not a v1 value.
        assert_eq!(parse_config(&b, Gen::V1), None);
        let mut v1 = b.clone();
        v1[B_LOD] = 2;
        v1[B_DEEP_SLEEP] = 0x81;
        let c = parse_config(&v1, Gen::V1).unwrap();
        assert_eq!((c.lod, c.deep_sleep), (SettingValue::Int(2), 0));
        // v2 glass mode switches to millimetres.
        let mut glass = v1.clone();
        glass[B_GLASS] = 1;
        assert_eq!(
            parse_config(&glass, Gen::V2).unwrap().lod,
            SettingValue::Int(2)
        );
        for (off, val) in [(B_COUNT, 5), (B_COUNT, 0), (B_ACTIVE, 4), (B_POLLING, 0x10)] {
            let mut bad = b.clone();
            bad[off] = val;
            assert_eq!(parse_config(&bad, Gen::V2), None, "{off:#x}={val:#x}");
        }
        let mut bad = b;
        bad[B_CPI + 1..B_CPI + 3].fill(0);
        assert_eq!(parse_config(&bad, Gen::V2), None);
    }

    /// A v1 mouse: answers commands, stores `0x14` writes into its block.
    struct Fake {
        cabled: bool,
        blob: RefCell<Vec<u8>>,
        last: Cell<u8>,
        sent: RefCell<Vec<[u8; 4]>>,
        busy: Cell<u8>,
        stuck: bool,
    }

    impl Fake {
        fn new(cabled: bool) -> Self {
            let mut blob = default_blob();
            blob[B_LOD] = 1;
            Fake {
                cabled,
                blob: RefCell::new(blob),
                last: Cell::new(0),
                sent: RefCell::new(vec![]),
                busy: Cell::new(0),
                stuck: false,
            }
        }
    }

    impl Feature for Fake {
        fn set(&self, buf: &mut [u8]) -> Result<()> {
            assert_eq!((buf.len(), buf[0]), (64, REPORT));
            self.sent.borrow_mut().push(buf[..4].try_into().unwrap());
            self.last.set(buf[1]);
            if buf[1] == CMD_SENSOR && !self.stuck {
                let p = &buf[PAYLOAD..PAYLOAD + SENSOR_LEN];
                let mut b = self.blob.borrow_mut();
                b[0x07..0x0c].copy_from_slice(&p[..5]);
                b[B_COUNT] = p[6];
                b[B_ACTIVE] = p[7];
                b[B_CPI..B_CPI + 20].copy_from_slice(&p[8..]);
            }
            Ok(())
        }
        fn get(&self, buf: &mut [u8]) -> Result<()> {
            let cmd = self.last.get();
            assert_eq!(buf.len() == BULK_LEN, cmd == CMD_CONFIG);
            assert_eq!(buf[0], if cmd == CMD_CONFIG { BULK } else { REPORT });
            buf[0] = REPORT;
            buf[1] = match cmd {
                CMD_PROBE if self.cabled => 0x07,
                _ if self.busy.get() > 0 => {
                    self.busy.set(self.busy.get() - 1);
                    0x03
                }
                _ => OK,
            };
            match cmd {
                CMD_MOUSE_INFO => {
                    buf[16..24].copy_from_slice(&[0x67, 0x33, 0x68, 0x19, 0x67, 0x19, 0x01, 0x10])
                }
                CMD_BATTERY => buf[16] = 0x41,
                CMD_CONFIG => buf[16..16 + BLOB_LEN].copy_from_slice(&self.blob.borrow()),
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn reads_over_dongle_and_cable() {
        let dev = Fake::new(false);
        dev.busy.set(2);
        let s = read_status(&dev, DONGLE, Duration::ZERO).unwrap();
        assert_eq!(s.spec.pid, XM2W_V1);
        assert_eq!(s.firmware.as_deref(), Some("1.10"));
        assert!(s.battery.is_none());
        assert_eq!(s.config.stages, [400, 800, 1600, 3200]);
        // Every command over the dongle follows a probe.
        let cmds: Vec<u8> = dev.sent.borrow().iter().map(|r| r[1]).collect();
        assert_eq!(cmds, [0x0f, 0x0e, 0x0f, 0x12]);

        let dev = Fake::new(true);
        let s = read_status(&dev, XM2W_V1, Duration::ZERO).unwrap();
        assert_eq!(s.battery, Some(battery(None, true)));
        assert!(dev.sent.borrow().iter().all(|r| r[1] != CMD_PROBE));
    }

    #[test]
    fn sensor_write_reads_changes_writes_and_verifies() {
        let dev = Fake::new(true);
        write_sensor(
            &dev,
            SettingKey::DpiStage1,
            &SettingValue::Int(500),
            Duration::ZERO,
        )
        .unwrap();
        // vole §13: stage 1, 400 → 500 is `90 01` → `f4 01`.
        assert_eq!(
            dev.blob.borrow()[0x23..0x28],
            [0x00, 0xf4, 0x01, 0xf4, 0x01]
        );
        let writes: Vec<[u8; 4]> = dev
            .sent
            .borrow()
            .iter()
            .copied()
            .filter(|r| r[1] == CMD_SENSOR)
            .collect();
        assert_eq!(writes, [[0xa1, 0x14, 0x0f, 0x1c]]);
        // Everything else in the block is what was read.
        let mut want = default_blob();
        want[B_LOD] = 1;
        want[0x23..0x28].copy_from_slice(&[0x00, 0xf4, 0x01, 0xf4, 0x01]);
        assert_eq!(*dev.blob.borrow(), want);

        write_sensor(
            &dev,
            SettingKey::LiftOffDistance,
            &SettingValue::Int(2),
            Duration::ZERO,
        )
        .unwrap();
        write_sensor(
            &dev,
            SettingKey::AngleSnapping,
            &SettingValue::Bool(true),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(dev.blob.borrow()[B_LOD..=B_SNAPPING], [2, 1]);
        // Same value: nothing written.
        let n = dev.sent.borrow().len();
        write_sensor(
            &dev,
            SettingKey::LiftOffDistance,
            &SettingValue::Int(2),
            Duration::ZERO,
        )
        .unwrap();
        assert!(dev.sent.borrow()[n..].iter().all(|r| r[1] != CMD_SENSOR));
    }

    #[test]
    fn sensor_write_refuses_bad_input_and_reports_mismatch() {
        let dev = Fake::new(true);
        for (key, v) in [
            (SettingKey::DpiStage1, 25),
            (SettingKey::DpiStage1, 425),
            (SettingKey::DpiStage1, 26050),
            (SettingKey::LiftOffDistance, 3),
            (SettingKey::ReportRate, 1000),
        ] {
            assert!(write_sensor(&dev, key, &SettingValue::Int(v), Duration::ZERO).is_err());
        }
        // An unexpected block is never written over.
        dev.blob.borrow_mut()[B_COUNT] = 7;
        assert!(write_sensor(
            &dev,
            SettingKey::DpiStage2,
            &SettingValue::Int(900),
            Duration::ZERO
        )
        .is_err());
        assert!(dev.sent.borrow().iter().all(|r| r[1] != CMD_SENSOR));
        let mut dev = Fake::new(true);
        dev.stuck = true;
        let e = write_sensor(
            &dev,
            SettingKey::DpiStage2,
            &SettingValue::Int(900),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(e.to_string().contains("read-back"), "{e}");
    }

    fn mouse(usb_pid: u16, reported: Option<u16>) -> Mouse {
        Mouse {
            id: DeviceId {
                vendor_id: VID,
                product_id: usb_pid,
                instance: "t".into(),
            },
            dev: None,
            spec: reported.and_then(spec),
            battery: battery(None, false),
            state: SettingsMap::new(),
            last_read: None,
            last_error: None,
        }
    }

    #[test]
    fn only_the_xm2w_v1_sensor_block_by_cable_is_offered() {
        let offered = |m: &Mouse| -> Vec<SettingKey> {
            m.controls()
                .iter()
                .inspect(|c| assert!(c.persistent, "{:?}", c.key))
                .filter(|c| c.blocked.is_none())
                .map(|c| c.key)
                .collect()
        };
        use SettingKey::*;
        let m = mouse(XM2W_V1, Some(XM2W_V1));
        assert_eq!(
            offered(&m),
            [
                DpiStage1,
                DpiStage2,
                DpiStage3,
                DpiStage4,
                LiftOffDistance,
                AngleSnapping
            ]
        );
        for m in [
            mouse(DONGLE, Some(XM2W_V1)),
            mouse(DONGLE, None),
            mouse(0x1972, Some(0x1972)),
            mouse(0x1984, Some(0x1984)),
            mouse(DONGLE, Some(0x1982)),
        ] {
            assert!(offered(&m).is_empty(), "{:04x}", m.id.product_id);
            let mut m = m;
            assert!(m.set(DpiStage1, SettingValue::Int(800)).is_err());
        }
        let v2 = mouse(0x1984, Some(0x1984)).controls();
        let lod = v2.iter().find(|c| c.key == LiftOffDistance).unwrap();
        assert_eq!((lod.min, lod.max), (Some(0.7), Some(2.0)));
    }
}
