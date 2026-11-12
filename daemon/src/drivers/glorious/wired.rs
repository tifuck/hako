//! Glorious Model O / O- (`258a:0036`) and Model D / D- (`258a:0033`), wired
//! SinoWealth mice.
//!
//! Layouts from libratbag `src/driver-sinowealth.c` and
//! `data/devices/glorious-model-{o,d}.device` (MIT), gloriousctl
//! `gloriousctl.c` (EUPL, layouts only) <https://github.com/enkore/gloriousctl>,
//! magnificent `src/model_o_minus.rs`
//! <https://gitlab.com/derpmalicious/magnificent> and OpenRGB
//! `SinowealthController.cpp` (GPL, layouts only). Commands are 6-byte
//! feature reports with id 5 (`05 <cmd> ..`), answered on the same report;
//! the configuration block is read with `05 11` and feature report 4
//! (520 bytes).
//!
//! The one onboard write offered is the click debounce time, `05 1a <ms/2>`:
//! all four sources agree on it, it touches nothing else, and it can be read
//! back. It is only offered on the firmware libratbag lists for the model.
//! DPI stages, report rate, lighting and lift-off live in the configuration
//! block; rewriting that block is reported to break mice (OpenRGB issues
//! #1223, #2083 and its fix 96e198ad), so those are blocked placeholders that
//! show the stored values. Research: `docs/onboard-writes/glorious.md`.

use crate::drivers::{DeviceAdapter, Opened};
use crate::hid::{enumerate_hidraw, report_ids, HidrawDevice};
use anyhow::{bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::merge_setting;
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

use super::SINOWEALTH;

pub(super) const MODEL_O: u16 = 0x0036;
pub(super) const MODEL_D: u16 = 0x0033;

/// Firmware libratbag's device file names for each PID.
fn known_firmware(pid: u16) -> &'static str {
    match pid {
        MODEL_O => "V103",
        _ => "V102",
    }
}

const CMD_FIRMWARE: u8 = 0x01;
const CMD_CONFIG: u8 = 0x11;
const CMD_DEBOUNCE: u8 = 0x1a;
const CONFIG_LEN: usize = 520;
/// libratbag `SINOWEALTH_DEBOUNCE_MIN/MAX`, the Glorious software's range.
const DEBOUNCE: (i64, i64) = (4, 16);
const REFRESH: Duration = Duration::from_secs(30);
const SETTLE: Duration = Duration::from_millis(50);

const CONFIG_BLOCKED: &str = "rewriting the configuration block broke Model O/D mice \
(OpenRGB #1223, #2083, fix 96e198ad)";
const LOD_BLOCKED: &str = "sources disagree on the offset (libratbag 129, gloriousctl and \
magnificent 130)";

/// Feature-report access, so the write sequence can run against a fake.
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

fn command(cmd: u8, arg: u8) -> [u8; 6] {
    [0x05, cmd, arg, 0, 0, 0]
}

/// Sends a read command and returns the 6-byte answer (`05 <cmd> ..`).
fn query(dev: &impl Feature, cmd: u8) -> Result<[u8; 6]> {
    dev.set(&mut command(cmd, 0))?;
    let mut buf = [0u8; 6];
    buf[0] = 0x05;
    dev.get(&mut buf)?;
    ensure!(
        buf[1] == cmd,
        "asked for command {cmd:#04x}, got an answer to {:#04x}",
        buf[1]
    );
    Ok(buf)
}

fn firmware(dev: &impl Feature) -> Result<String> {
    let b = query(dev, CMD_FIRMWARE)?;
    Ok(String::from_utf8_lossy(&b[2..6]).into_owned())
}

/// Debounce time in ms; only the values the protocol can carry (2..16 even).
fn read_debounce(dev: &impl Feature) -> Result<i64> {
    let b = query(dev, CMD_DEBOUNCE)?;
    ensure!(
        (1..=8).contains(&b[2]),
        "unexpected debounce value {:#04x}",
        b[2]
    );
    Ok(i64::from(b[2]) * 2)
}

/// The write command for `ms` (libratbag `sinowealth_set_debounce_time`).
fn debounce_command(ms: i64) -> Result<[u8; 6]> {
    let (min, max) = DEBOUNCE;
    ensure!(
        (min..=max).contains(&ms) && ms % 2 == 0,
        "debounce must be an even {min}..={max} ms"
    );
    Ok(command(CMD_DEBOUNCE, (ms / 2) as u8))
}

/// Read, write if different, then read back and compare.
fn write_debounce(dev: &impl Feature, ms: i64, settle: Duration) -> Result<()> {
    let mut cmd = debounce_command(ms)?;
    let old = read_debounce(dev).context("not writing: the current debounce is unreadable")?;
    if old == ms {
        return Ok(());
    }
    dev.set(&mut cmd)?;
    std::thread::sleep(settle);
    let back = read_debounce(dev)?;
    ensure!(
        back == ms,
        "read-back debounce is {back} ms, not {ms} ms; check the mouse in Glorious Core"
    );
    Ok(())
}

/// What the configuration block holds, for display only.
#[derive(Debug, PartialEq, Eq)]
struct Config {
    /// DPI of slots 1..=5 (`None` when disabled).
    stages: [Option<i64>; 5],
    /// Active slot 1..=8.
    active: Option<i64>,
    rate: i64,
    effect: u8,
}

/// Parses a configuration block (libratbag `sinowealth_config_report`).
fn parse_config(b: &[u8]) -> Option<Config> {
    if b.len() < 131 || b[0] != 0x04 || b[1] != CMD_CONFIG {
        return None;
    }
    let rate = [125, 250, 500, 1000].get(usize::from(b[10] & 0x0f).checked_sub(1)?)?;
    // XY-independent stages use two bytes each; not shown.
    if b[10] & 0x80 != 0 {
        return None;
    }
    let sensor_offset = match b[9] {
        0x06 | 0x0e => 1, // PMW3360 / PMW3327: (raw + 1) × 100
        0x0f => 0,        // PMW3389: raw × 100
        _ => return None,
    };
    let (count, active) = (b[11] & 0x0f, b[11] >> 4);
    let enabled = !b[12];
    if count == 0 || count > 8 || enabled.count_ones() != u32::from(count) || active > count {
        return None;
    }
    let mut stages = [None; 5];
    for (i, s) in stages.iter_mut().enumerate() {
        if enabled & (1 << i) != 0 {
            *s = Some((i64::from(b[13 + i]) + sensor_offset) * 100);
        }
    }
    // `active` counts enabled slots only, from 1.
    let slot = (0..8)
        .filter(|i| enabled & (1 << i) != 0)
        .nth(usize::from(active).checked_sub(1)?)
        .map(|i| i + 1);
    Some(Config {
        stages,
        active: slot,
        rate: *rate,
        effect: b[53],
    })
}

/// Effect codes (libratbag `sinowealth_rgb_effect`, gloriousctl names).
const EFFECTS: &[(u8, &str, &str)] = &[
    (0x00, "off", "Off"),
    (0x01, "glorious", "Glorious"),
    (0x02, "static", "Single color"),
    (0x03, "breathing7", "Seven-color breathing"),
    (0x04, "tail", "Tail"),
    (0x05, "breathing", "RGB breathing"),
    (0x07, "rave", "Rave"),
    (0x09, "wave", "Wave"),
    (0x0a, "breathing1", "Single color breathing"),
];

const STAGES: [SettingKey; 5] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];

/// Every control: debounce (offered when `debounce_block` is `None`) and the
/// blocked configuration-block placeholders.
fn controls(debounce_block: Option<&str>) -> Vec<SettingMeta> {
    use SettingKey::*;
    let (min, max) = DEBOUNCE;
    let debounce = SettingMeta::slider(Debounce, "Click debounce", min as f64, max as f64, 2.0)
        .unit("ms")
        .persistent();
    let mut c = vec![match debounce_block {
        Some(reason) => debounce.blocked(reason),
        None => debounce,
    }];
    let blocked = |m: SettingMeta| m.persistent().blocked(CONFIG_BLOCKED);
    for (i, key) in STAGES.into_iter().enumerate() {
        let label = format!("DPI stage {}", i + 1);
        c.push(blocked(
            SettingMeta::slider(key, &label, 100.0, 12000.0, 100.0).unit("dpi"),
        ));
    }
    c.push(blocked(SettingMeta::dropdown(
        DpiStageActive,
        "Active DPI stage",
        (1..=8)
            .map(|i| SettingOption::int(i, &i.to_string()))
            .collect(),
    )));
    c.push(blocked(SettingMeta::dropdown(
        ReportRate,
        "Report rate",
        [125, 250, 500, 1000]
            .map(|r| SettingOption::int(r, &format!("{r} Hz")))
            .to_vec(),
    )));
    c.push(
        SettingMeta::dropdown(
            LiftOffDistance,
            "Lift-off distance",
            vec![SettingOption::int(2, "2 mm"), SettingOption::int(3, "3 mm")],
        )
        .unit("mm")
        .persistent()
        .blocked(LOD_BLOCKED),
    );
    c.push(blocked(SettingMeta::dropdown(
        Effect,
        "Effect",
        EFFECTS
            .iter()
            .map(|(_, v, l)| SettingOption::text(v, l))
            .collect(),
    )));
    c.push(blocked(
        SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 25.0).unit("%"),
    ));
    for (key, label) in [(ColorR, "Red"), (ColorG, "Green"), (ColorB, "Blue")] {
        c.push(blocked(SettingMeta::color(key, label)));
    }
    c
}

/// The hidraw node carrying the command (5) and config (4) reports
/// (libratbag `sinowealth_test_hidraw`), else interface 1 (gloriousctl).
fn open_node(pid: u16) -> Result<(HidrawDevice, String)> {
    let nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == SINOWEALTH && d.product_id == pid)
        .collect();
    let info = nodes
        .iter()
        .find(|d| {
            let ids = report_ids(&d.report_descriptor());
            ids.contains(&0x04) && ids.contains(&0x05)
        })
        .or_else(|| nodes.iter().find(|d| d.interface == Some(1)))
        .with_context(|| format!("no configuration node for {SINOWEALTH:04x}:{pid:04x}"))?;
    let dev = HidrawDevice::open(&info.path)?;
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq.clone()
    };
    Ok((dev, instance))
}

pub(super) fn open(name: &'static str, pid: u16) -> Opened {
    let (dev, instance) = open_node(pid)?;
    info!("{name} open via {}", dev.path.display());
    let firmware = firmware(&dev).ok();
    let mut m = Mouse {
        name,
        id: DeviceId {
            vendor_id: SINOWEALTH,
            product_id: pid,
            instance,
        },
        dev: Some(dev),
        firmware,
        debounce_ok: false,
        state: SettingsMap::new(),
        last_read: None,
        last_error: None,
    };
    m.refresh();
    Ok(vec![Box::new(m)])
}

struct Mouse {
    name: &'static str,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    firmware: Option<String>,
    /// The debounce time read back successfully.
    debounce_ok: bool,
    state: SettingsMap,
    last_read: Option<Instant>,
    last_error: Option<String>,
}

impl Mouse {
    fn refresh(&mut self) {
        self.last_read = Some(Instant::now());
        let Some(dev) = self.dev.as_ref() else {
            return;
        };
        self.state.clear();
        let mut errors = Vec::new();
        match read_debounce(dev) {
            Ok(ms) => {
                self.debounce_ok = true;
                merge_setting(&mut self.state, SettingKey::Debounce, SettingValue::Int(ms));
            }
            Err(e) => {
                self.debounce_ok = false;
                errors.push(format!("debounce: {e}"));
            }
        }
        match read_config(dev) {
            Ok(c) => show_config(&mut self.state, &c),
            Err(e) => errors.push(format!("configuration: {e}")),
        }
        self.last_error = (!errors.is_empty()).then(|| errors.join("; "));
    }

    /// Why debounce writes are not offered, if they are not.
    fn debounce_block(&self) -> Option<String> {
        let want = known_firmware(self.id.product_id);
        match &self.firmware {
            None => Some("firmware version unreadable".into()),
            Some(fw) if fw != want => Some(format!(
                "firmware {fw} is not the one libratbag supports ({want})"
            )),
            Some(_) if !self.debounce_ok => Some("current value unreadable".into()),
            Some(_) => None,
        }
    }
}

fn read_config(dev: &impl Feature) -> Result<Config> {
    dev.set(&mut command(CMD_CONFIG, 0))?;
    let mut buf = vec![0u8; CONFIG_LEN];
    buf[0] = 0x04;
    dev.get(&mut buf)?;
    parse_config(&buf).context("unexpected configuration block")
}

fn show_config(s: &mut SettingsMap, c: &Config) {
    for (key, dpi) in STAGES.into_iter().zip(c.stages) {
        if let Some(dpi) = dpi {
            merge_setting(s, key, SettingValue::Int(dpi));
        }
    }
    if let Some(a) = c.active {
        merge_setting(s, SettingKey::DpiStageActive, SettingValue::Int(a));
    }
    merge_setting(s, SettingKey::ReportRate, SettingValue::Int(c.rate));
    if let Some((_, v, _)) = EFFECTS.iter().find(|(code, _, _)| *code == c.effect) {
        merge_setting(s, SettingKey::Effect, SettingValue::Text((*v).into()));
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
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.state.clone(),
            controls: controls(self.debounce_block().as_deref()),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if key != SettingKey::Debounce {
            bail!(
                "{} is stored in the configuration block; not written",
                key.as_str()
            );
        }
        if let Some(reason) = self.debounce_block() {
            bail!("debounce is not available: {reason}");
        }
        let ms = value.as_int().context("expected a number")?;
        let dev = self.dev.as_ref().context("not connected")?;
        let result = write_debounce(dev, ms, SETTLE);
        self.refresh();
        if let Err(e) = &result {
            self.last_error = Some(e.to_string());
        }
        result
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_some() && self.last_read.is_none_or(|t| t.elapsed() >= REFRESH) {
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
    use std::cell::RefCell;

    #[test]
    fn debounce_bytes_match_sources() {
        // libratbag, gloriousctl and magnificent: `05 1a <ms / 2>`.
        assert_eq!(debounce_command(4).unwrap(), [0x05, 0x1a, 0x02, 0, 0, 0]);
        assert_eq!(debounce_command(16).unwrap(), [0x05, 0x1a, 0x08, 0, 0, 0]);
        assert!(debounce_command(2).is_err());
        assert!(debounce_command(5).is_err());
        assert!(debounce_command(18).is_err());
        assert_eq!(command(CMD_CONFIG, 0), [0x05, 0x11, 0, 0, 0, 0]);
        assert_eq!(command(CMD_FIRMWARE, 0), [0x05, 0x01, 0, 0, 0, 0]);
    }

    /// magnificent's Model O- default block, as far as it sets bytes.
    fn model_o_block() -> Vec<u8> {
        let mut b = vec![0u8; CONFIG_LEN];
        b[..13].copy_from_slice(&[
            0x04, 0x11, 0x00, 0x7b, 0x00, 0x00, 0x00, 0x00, 0x64, 0x06, 0x04, 0x11, 0xfb,
        ]);
        b[13..19].fill(0x0f);
        b[53] = 0x01;
        b
    }

    #[test]
    fn parses_configuration_block() {
        let b = model_o_block();
        // One stage enabled (mask fb: slot 2), active 1st enabled = slot 3,
        // PMW3360 (0x0f + 1) × 100 = 1600 dpi, 1000 Hz, Glorious effect.
        assert_eq!(
            parse_config(&b),
            Some(Config {
                stages: [None, None, Some(1600), None, None],
                active: Some(3),
                rate: 1000,
                effect: 0x01,
            })
        );
        let mut s = SettingsMap::new();
        show_config(&mut s, &parse_config(&b).unwrap());
        assert_eq!(s.get("dpi_stage_3"), Some(&SettingValue::Int(1600)));
        assert_eq!(
            s.get("effect"),
            Some(&SettingValue::Text("glorious".into()))
        );
        // Count disagreeing with the mask, bad rate, wrong command: rejected.
        let mut bad = b.clone();
        bad[11] = 0x12;
        assert_eq!(parse_config(&bad), None);
        let mut bad = b.clone();
        bad[10] = 0x05;
        assert_eq!(parse_config(&bad), None);
        let mut bad = b;
        bad[1] = 0x12;
        assert_eq!(parse_config(&bad), None);
    }

    /// A mouse answering command reports; `stuck` ignores debounce writes.
    struct Fake {
        debounce: RefCell<u8>,
        last: RefCell<u8>,
        writes: RefCell<Vec<[u8; 6]>>,
        stuck: bool,
        garbage: bool,
    }

    impl Fake {
        fn new(raw: u8) -> Self {
            Fake {
                debounce: RefCell::new(raw),
                last: RefCell::new(0),
                writes: RefCell::new(vec![]),
                stuck: false,
                garbage: false,
            }
        }
    }

    impl Feature for Fake {
        fn get(&self, buf: &mut [u8]) -> Result<()> {
            let cmd = *self.last.borrow();
            buf[1] = cmd;
            if cmd == CMD_DEBOUNCE {
                buf[2] = if self.garbage {
                    0xff
                } else {
                    *self.debounce.borrow()
                };
            }
            Ok(())
        }
        fn set(&self, buf: &mut [u8]) -> Result<()> {
            assert_eq!(buf.len(), 6);
            *self.last.borrow_mut() = buf[1];
            if buf[1] == CMD_DEBOUNCE && buf[2] != 0 {
                self.writes.borrow_mut().push(buf.try_into().unwrap());
                if !self.stuck {
                    *self.debounce.borrow_mut() = buf[2];
                }
            }
            Ok(())
        }
    }

    #[test]
    fn debounce_write_reads_writes_and_verifies() {
        let dev = Fake::new(4);
        assert_eq!(read_debounce(&dev).unwrap(), 8);
        write_debounce(&dev, 12, Duration::ZERO).unwrap();
        assert_eq!(*dev.writes.borrow(), [[0x05, 0x1a, 0x06, 0, 0, 0]]);
        assert_eq!(read_debounce(&dev).unwrap(), 12);
        // Same value: nothing written.
        write_debounce(&dev, 12, Duration::ZERO).unwrap();
        assert_eq!(dev.writes.borrow().len(), 1);
    }

    #[test]
    fn debounce_write_aborts_on_unreadable_value_and_reports_mismatch() {
        let mut dev = Fake::new(4);
        dev.garbage = true;
        assert!(write_debounce(&dev, 12, Duration::ZERO).is_err());
        assert!(dev.writes.borrow().is_empty());
        let mut dev = Fake::new(4);
        dev.stuck = true;
        let e = write_debounce(&dev, 12, Duration::ZERO).unwrap_err();
        assert!(e.to_string().contains("read-back"), "{e}");
    }

    #[test]
    fn controls_offer_only_debounce() {
        let c = controls(None);
        for m in &c {
            assert!(m.persistent, "{:?}", m.key);
            assert_eq!(
                m.blocked.is_none(),
                m.key == SettingKey::Debounce,
                "{:?}",
                m.key
            );
        }
        let d = &c[0];
        assert!(d.validate(&SettingValue::Int(4)).is_ok());
        assert!(d.validate(&SettingValue::Int(18)).is_err());
        assert!(controls(Some("x")).iter().all(|m| m.blocked.is_some()));
        assert_eq!(known_firmware(MODEL_O), "V103");
        assert_eq!(known_firmware(MODEL_D), "V102");
    }
}
