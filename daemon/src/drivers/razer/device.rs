//! Generic adapter for the models in `models.rs`: extended-matrix lighting,
//! DPI, DPI stages, poll rate, battery, idle time and keyboard game mode, all
//! over hidraw feature reports on the model's report interface (usbhid turns
//! HIDIOC\[SG\]FEATURE into the SET_REPORT / GET_REPORT control transfers
//! OpenRazer sends with wIndex = that interface). Each command goes out with
//! the transaction id the model's `Link` gives its command family.
//!
//! Writes carry the NOSTORE storage byte, so the firmware keeps nothing; the
//! daemon re-applies what the user chose on every (re)connect instead, but
//! reads each value first and writes only what differs (lighting only on
//! models whose stored lighting is what they show; a failed read writes).
//! Values the user never set (DPI, DPI stages, poll rate, idle time) are read
//! from the device.
//!
//! "Save changes to device memory" (`save_to_device`, behind the owner's
//! onboard-memory opt-in) stores lighting and DPI stages with VARSTORE (0x01,
//! onboard profile 1) instead, only inside `set` for a changed value; turning
//! it on stores the current state once. Same commands and storage byte as
//! OpenRazer for these PIDs; byte layouts confirmed by freenapse (MIT,
//! hardware-verified VARSTORE lighting / stages, `src/lighting.rs`,
//! `src/protocol.rs`) and OpenRGB `RazerController.h` (`RAZER_STORAGE_SAVE`).
//! Stages are read, validated and read back around the write; stored
//! brightness is read back. DPI, poll rate, idle time and game mode stay as
//! they are: their storage byte is disputed or absent. Evidence and the
//! per-model exclusions: docs/onboard-writes/razer.md.

use super::models::{spec_for, Battery, Family, Link, Poll, Spec, Wave, CLASSIC_RATES};
use super::protocol::{self as p, Cmd, Effect, MAX_DPI_STAGES, NOSTORE, VARSTORE};
use super::VID;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hako_core::catalog::{
    SettingGroup, SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap,
};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{debug, info};

const STORE: u8 = NOSTORE;
const BATTERY_POLL: Duration = Duration::from_secs(30);
/// Retry period while a wireless device does not answer (asleep / off).
const LINK_PROBE: Duration = Duration::from_secs(5);
const NO_LINK: &str = "no response from the device (asleep or switched off?)";
/// Idle time before power save, minutes (firmware range 60..=900 s).
const IDLE_MIN: (f64, f64) = (1.0, 15.0);
const STAGE_KEYS: [SettingKey; MAX_DPI_STAGES] = [
    SettingKey::DpiStage1,
    SettingKey::DpiStage2,
    SettingKey::DpiStage3,
    SettingKey::DpiStage4,
    SettingKey::DpiStage5,
];
/// Shown until the device reports its own stages.
const DEFAULT_STAGES: [u16; MAX_DPI_STAGES] = [400, 800, 1600, 3200, 6400];
/// Why a model without storable lighting or stages gets no save mode.
const NOTHING_STORABLE: &str =
    "no setting with a confirmed storage byte (DPI byte disputed; poll rate and idle have none)";

/// Where the feature reports go: the hidraw node, or a fake in tests.
trait Feature: Send {
    /// SET_REPORT `buf`, wait, then GET_REPORT into `buf`.
    fn exchange(&mut self, buf: &mut [u8; 91], wait: Duration) -> Result<()>;
    /// The node is gone (unplugged).
    fn gone(&self) -> bool;
}

impl Feature for HidrawDevice {
    fn exchange(&mut self, buf: &mut [u8; 91], wait: Duration) -> Result<()> {
        self.set_feature(buf)?;
        std::thread::sleep(wait);
        *buf = [0u8; 91];
        self.get_feature(buf)
    }

    fn gone(&self) -> bool {
        !self.path.exists()
    }
}

pub fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let spec = spec_for(pid).with_context(|| format!("no Razer model for {pid:04x}"))?;
    let link = spec.link_for(pid);
    let info = find_hidraw(VID, pid, Some(u32::from(link.iface)))
        .with_context(|| format!("hidraw interface {}", link.iface))?;
    let dev = HidrawDevice::open(&info.path)?;
    // Settings live under the canonical (first) PID for wired and wireless.
    let chosen = hako_core::config::load_device_settings(&ctx.paths, VID, spec.pids[0])?;
    let mut settings = chosen.clone();
    defaults(&mut settings, spec);
    let mut a = RazerAdapter {
        spec,
        link,
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: info.path.display().to_string(),
        },
        dev: Some(Box::new(dev)),
        settings,
        chosen,
        battery: None,
        last_error: None,
        paths: ctx.paths.clone(),
        link_up: false,
        last_poll: Instant::now(),
        stage_count: MAX_DPI_STAGES,
    };
    a.connect();
    info!(
        "{} open via {} iface={} link_up={}",
        spec.name,
        info.path.display(),
        link.iface,
        a.link_up
    );
    Ok(vec![Box::new(a)])
}

pub struct RazerAdapter {
    spec: &'static Spec,
    link: Link,
    id: DeviceId,
    dev: Option<Box<dyn Feature>>,
    /// What the UI shows: `chosen` + device readings + defaults.
    settings: SettingsMap,
    /// What the user set (the saved file); re-applied on every connect.
    chosen: SettingsMap,
    /// (percent, charging) from the last successful poll.
    battery: Option<(u8, bool)>,
    last_error: Option<String>,
    paths: ConfigPaths,
    /// The device answers (always true for wired-only models once open).
    link_up: bool,
    last_poll: Instant,
    /// DPI stages the device reported (all five until it answers).
    stage_count: usize,
}

impl RazerAdapter {
    /// Called on open and whenever a wireless link comes back.
    fn connect(&mut self) {
        self.last_poll = Instant::now();
        if self.spec.battery != Battery::None {
            self.link_up = self.read_battery().is_ok();
            if !self.link_up {
                self.last_error = Some(NO_LINK.into());
                return;
            }
        } else {
            self.link_up = true;
        }
        self.last_error = self.apply_all();
    }

    /// OpenRazer razer_send_payload: SET, wait, GET, check; retried while the
    /// device is busy / answering something else.
    fn transact(&mut self, txn: u8, (class, cmd, args): Cmd) -> Result<[u8; 90]> {
        let req = p::make_report(txn, class, cmd, &args);
        let wait = Duration::from_millis(self.spec.wait_ms);
        let dev = self.dev.as_mut().context("device not open")?;
        let mut last = anyhow!("no attempt");
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            // Feature buffer = report id 0 + 90-byte report.
            let mut buf = [0u8; 91];
            buf[1..].copy_from_slice(&req);
            if let Err(e) = dev.exchange(&mut buf, wait) {
                // Node gone (unplugged): report disconnected so the hub reopens.
                if dev.gone() {
                    self.dev = None;
                }
                return Err(e);
            }
            let mut resp = [0u8; 90];
            resp.copy_from_slice(&buf[1..]);
            match p::check_response(&req, &resp) {
                Ok(()) => return Ok(resp),
                // Failure / not supported will not change on retry.
                Err(e) if matches!(resp[0], 0x03 | 0x05) => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn send(&mut self, txn: u8, cmd: Cmd) -> Result<()> {
        self.transact(txn, cmd).map(drop)
    }

    fn read_battery(&mut self) -> Result<()> {
        let txn = self.link.txn(Family::Battery);
        let level = p::parse_battery(&self.transact(txn, p::get_battery())?);
        let charging = self.spec.battery == Battery::Rechargeable
            && p::parse_charging(&self.transact(txn, p::get_charging())?);
        self.battery = Some((level, charging));
        Ok(())
    }

    fn supports(&self, key: SettingKey) -> bool {
        match key {
            SettingKey::Dpi => self.spec.dpi_max > 0,
            SettingKey::ReportRate => self.link.poll != Poll::None,
            SettingKey::PmShutdown => self.spec.idle,
            SettingKey::GameMode => self.spec.game_txn.is_some(),
            SettingKey::DpiStageActive => self.spec.stages,
            k if STAGE_KEYS.contains(&k) => self.spec.stages,
            SettingKey::SaveToDevice => save_offer(self.spec).is_ok(),
            k => k.group() == SettingGroup::Lighting && !self.spec.leds.is_empty(),
        }
    }

    /// "Save to device" mode is on (and offered for this model).
    fn saving(&self) -> bool {
        save_offer(self.spec).is_ok()
            && get_setting(&self.settings, SettingKey::SaveToDevice)
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }

    /// Storage byte for a user change of `key`: VARSTORE only in save mode,
    /// for what this model stores.
    fn store_for(&self, key: SettingKey) -> u8 {
        let (lighting, stages) = storable(self.spec);
        let stored = match key {
            k if k == SettingKey::DpiStageActive || STAGE_KEYS.contains(&k) => stages,
            k => k.group() == SettingGroup::Lighting && lighting,
        };
        if stored && self.saving() {
            VARSTORE
        } else {
            NOSTORE
        }
    }

    /// Turning the mode on stores the current state once; turning it off
    /// (also done by the worker when the opt-in is revoked) writes nothing.
    fn set_save(&mut self, on: bool) -> Result<()> {
        if on && !self.saving() {
            let (lighting, stages) = storable(self.spec);
            if lighting {
                self.apply_lighting(VARSTORE, false)?;
            }
            if stages {
                self.write_stages(VARSTORE)?;
            }
        }
        for map in [&mut self.chosen, &mut self.settings] {
            merge_setting(map, SettingKey::SaveToDevice, SettingValue::Bool(on));
        }
        Ok(())
    }

    /// Lighting, then each non-lighting control: the user's value is written
    /// unless the device already reads back the same, an unset one is read
    /// back. Returns the joined errors, if any.
    fn apply_all(&mut self) -> Option<String> {
        let mut errs = Vec::new();
        if !self.spec.leds.is_empty() {
            if let Err(e) = self.apply_lighting(NOSTORE, true) {
                errs.push(format!("lighting: {e}"));
            }
        }
        // Stages first: switching stage moves the DPI, a chosen DPI wins.
        if self.spec.stages {
            if let Err(e) = self.sync_stages() {
                errs.push(format!("dpi stages: {e}"));
            }
        }
        for key in [
            SettingKey::Dpi,
            SettingKey::ReportRate,
            SettingKey::PmShutdown,
            SettingKey::GameMode,
        ] {
            if !self.supports(key) {
                continue;
            }
            let result = match get_setting(&self.chosen, key).cloned() {
                Some(v) if self.holds(key, &v) => Ok(()),
                Some(v) => self.write(key, &v, NOSTORE),
                None => self.read(key),
            };
            if let Err(e) = result {
                errs.push(format!("{}: {e}", key.as_str()));
            }
        }
        (!errs.is_empty()).then(|| errs.join("; "))
    }

    /// Brightness and effect on every LED; stored brightness is read back.
    /// `reapply` (connect) skips each command whose value the device already
    /// holds, on models whose stored lighting (VARSTORE) is what they show.
    fn apply_lighting(&mut self, store: u8, reapply: bool) -> Result<()> {
        let s = &self.settings;
        let mut effect = Effect::from_str(text(s, SettingKey::Effect, "static"));
        if !self.spec.has_effect(effect) {
            effect = default_effect(self.spec);
        }
        let pct = byte(s, SettingKey::Brightness, 80);
        let rgb = [
            byte(s, SettingKey::ColorR, 0),
            byte(s, SettingKey::ColorG, 120),
            byte(s, SettingKey::ColorB, 255),
        ];
        let speed = text(s, SettingKey::Speed, "medium").to_string();
        let direction = text(s, SettingKey::Direction, "left");
        let dir = match self.spec.wave {
            Wave::Std => p::wave_dir(direction, false),
            Wave::Flipped => p::wave_dir(direction, true),
            Wave::ZeroBased => p::wave_dir_zero_based(direction),
        };
        let (br_txn, fx_txn) = (
            self.link.txn(Family::Brightness),
            self.link.txn(Family::Fx(effect)),
        );
        let leds = self.spec.leds;
        let compare = reapply && storable(self.spec).0;
        for &led in leds {
            let br = p::set_brightness(store, led, pct);
            let fx = p::set_effect(store, led, effect, rgb, &speed, dir);
            for (txn, cmd) in [(br_txn, br), (fx_txn, fx)] {
                if !(compare && self.holds_lighting(txn, &cmd)) {
                    self.send(txn, cmd)?;
                }
            }
        }
        if store == VARSTORE {
            verify_brightness(&mut |t, c| self.transact(t, c), br_txn, leds, pct)?;
        }
        Ok(())
    }

    /// The stored lighting already equals what `cmd` sets: the read answer
    /// echoes the LED and every argument byte after it.
    fn holds_lighting(&mut self, txn: u8, (_, id, args): &Cmd) -> bool {
        let get = match id {
            0x04 => p::get_brightness(VARSTORE, args[1]),
            _ => p::get_effect(VARSTORE, args[1]),
        };
        self.transact(txn, get)
            .is_ok_and(|r| r[9] == args[1] && r[10..8 + args.len()] == args[2..])
    }

    /// Read the device's stages into every stage value the user never set,
    /// then write the stages if the user set any of them and the device
    /// holds other ones.
    fn sync_stages(&mut self) -> Result<()> {
        let txn = self.link.txn(Family::DpiStages);
        let chosen = STAGE_KEYS
            .iter()
            .chain([&SettingKey::DpiStageActive])
            .any(|&k| get_setting(&self.chosen, k).is_some());
        let mut held = None;
        match self.transact(txn, p::get_dpi_stages(NOSTORE)) {
            Ok(resp) => {
                let (active, dpis) = p::parse_dpi_stages(&resp);
                if !dpis.is_empty() {
                    self.stage_count = dpis.len();
                    let read = dpis
                        .iter()
                        .zip(STAGE_KEYS)
                        .map(|(&dpi, key)| (key, i64::from(dpi)))
                        .chain([(SettingKey::DpiStageActive, i64::from(active))]);
                    for (key, v) in read {
                        if get_setting(&self.chosen, key).is_none() {
                            merge_setting(&mut self.settings, key, SettingValue::Int(v));
                        }
                    }
                    held = Some((active, dpis));
                }
            }
            Err(e) if !chosen => return Err(e),
            Err(e) => debug!("{} dpi stages: {e}", self.spec.name),
        }
        if chosen && held != Some(self.stage_values()) {
            self.write_stages(NOSTORE)?;
        }
        Ok(())
    }

    /// (active stage, DPI per stage) as `write_stages` sends them.
    fn stage_values(&self) -> (u8, Vec<u16>) {
        let n = self.stage_count;
        let dpis: Vec<u16> = STAGE_KEYS[..n]
            .iter()
            .zip(DEFAULT_STAGES)
            .map(|(&k, d)| {
                get_setting(&self.settings, k)
                    .and_then(|v| v.as_int())
                    .map_or(d.min(self.spec.dpi_max), |v| {
                        v.clamp(100, i64::from(self.spec.dpi_max)) as u16
                    })
            })
            .collect();
        let active = get_setting(&self.settings, SettingKey::DpiStageActive)
            .and_then(|v| v.as_int())
            .map_or(1, |v| v.clamp(1, n as i64) as u8);
        (active, dpis)
    }

    fn write_stages(&mut self, store: u8) -> Result<()> {
        let (active, dpis) = self.stage_values();
        let txn = self.link.txn(Family::DpiStages);
        if store == VARSTORE {
            let dpi_max = self.spec.dpi_max;
            return store_stages(&mut |t, c| self.transact(t, c), txn, dpi_max, active, &dpis);
        }
        self.send(txn, p::set_dpi_stages(store, active, &dpis))
    }

    /// `store` applies to lighting and DPI stages; the rest is always NOSTORE.
    fn write(&mut self, key: SettingKey, value: &SettingValue, store: u8) -> Result<()> {
        let int = value.as_int().context("expected a number");
        match key {
            SettingKey::Dpi => {
                let txn = self.link.txn(Family::Dpi);
                self.send(txn, p::set_dpi(STORE, int? as u16))
            }
            SettingKey::ReportRate => {
                let hz = int? as u32;
                let txn = self.link.txn(Family::Poll);
                match self.link.poll {
                    Poll::Classic => {
                        let cmd = p::set_poll(hz).context("unsupported rate")?;
                        self.send(txn, cmd)
                    }
                    // Razer mice get the HyperPolling rate twice (argument 0x00,
                    // then 0x01), as OpenRazer `razer_attr_write_poll_rate` does.
                    Poll::V2(_, repeat) => {
                        let cmd = p::set_poll2(0x00, hz).context("unsupported rate")?;
                        self.send(txn, cmd)?;
                        if repeat {
                            let txn = self.link.txn(Family::PollRepeat);
                            let cmd = p::set_poll2(0x01, hz).context("unsupported rate")?;
                            self.send(txn, cmd)?;
                        }
                        Ok(())
                    }
                    Poll::None => bail!("no report-rate control"),
                }
            }
            SettingKey::PmShutdown => {
                let txn = self.link.txn(Family::Idle);
                self.send(txn, p::set_idle(int? as u16 * 60))
            }
            k if k == SettingKey::DpiStageActive || STAGE_KEYS.contains(&k) => {
                self.write_stages(store)?;
                // The active stage sets the DPI; show it unless the user chose one.
                if get_setting(&self.chosen, SettingKey::Dpi).is_none() {
                    self.read(SettingKey::Dpi)?;
                }
                Ok(())
            }
            SettingKey::GameMode => {
                let on = value.as_bool().context("expected true/false")?;
                let txn = self.spec.game_txn.context("no game mode")?;
                self.send(txn, p::set_game_mode(STORE, on))
            }
            _ => self.apply_lighting(store, false),
        }
    }

    /// The device's raw value: DPI (X; Y must match), Hz, idle seconds, game
    /// mode 0 / 1. `None` when it has no read for `key`.
    fn device_value(&mut self, key: SettingKey) -> Result<Option<i64>> {
        Ok(match key {
            SettingKey::Dpi => {
                let txn = self.link.txn(Family::Dpi);
                let r = self.transact(txn, p::get_dpi(NOSTORE))?;
                (r[9..11] == r[11..13]).then(|| i64::from(p::parse_dpi(&r)))
            }
            SettingKey::ReportRate => {
                let txn = self.link.txn(Family::PollGet);
                match self.link.poll {
                    Poll::Classic => p::parse_poll(&self.transact(txn, p::get_poll())?),
                    Poll::V2(..) => p::parse_poll2(&self.transact(txn, p::get_poll2())?),
                    Poll::None => None,
                }
                .map(i64::from)
            }
            SettingKey::PmShutdown => {
                let txn = self.link.txn(Family::Idle);
                Some(i64::from(p::parse_idle(
                    &self.transact(txn, p::get_idle())?,
                )))
            }
            SettingKey::GameMode => {
                let txn = self.spec.game_txn.context("no game mode")?;
                // OpenRazer reads it with NOSTORE on 0x1f, VARSTORE on 0xff.
                let store = if txn == 0x1f { NOSTORE } else { VARSTORE };
                Some(i64::from(self.transact(txn, p::get_game_mode(store))?[10]))
            }
            _ => None,
        })
    }

    /// The device reads back what writing `value` would leave; false when it
    /// cannot tell (no read command, read failed).
    fn holds(&mut self, key: SettingKey, value: &SettingValue) -> bool {
        let want = match key {
            SettingKey::PmShutdown => value.as_int().map(|m| (m * 60).clamp(60, 900)),
            SettingKey::GameMode => value.as_bool().map(i64::from),
            _ => value.as_int(),
        };
        match self.device_value(key) {
            Ok(have) => want.is_some() && have == want,
            Err(e) => {
                debug!("{} {}: {e}", self.spec.name, key.as_str());
                false
            }
        }
    }

    /// Reflect the device's current value for a control the user never set.
    fn read(&mut self, key: SettingKey) -> Result<()> {
        if key == SettingKey::GameMode {
            return Ok(());
        }
        let value = match (key, self.device_value(key)?) {
            (SettingKey::Dpi, Some(dpi)) if dpi < 100 => None,
            (SettingKey::PmShutdown, Some(secs)) => {
                Some((secs as f64 / 60.0).round().clamp(IDLE_MIN.0, IDLE_MIN.1) as i64)
            }
            (_, v) => v,
        };
        if let Some(v) = value {
            merge_setting(&mut self.settings, key, SettingValue::Int(v));
        }
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, VID, self.spec.pids[0], &self.chosen)
    }
}

/// (lighting, DPI stages) the save mode stores for this model.
fn storable(spec: &Spec) -> (bool, bool) {
    let lighting = !spec.leds.is_empty() && spec.volatile_lighting.is_none();
    (lighting, spec.stages)
}

/// The save mode, or why this model does not get it.
fn save_offer(spec: &Spec) -> Result<(), &'static str> {
    match storable(spec) {
        (false, false) => Err(spec.volatile_lighting.unwrap_or(NOTHING_STORABLE)),
        _ => Ok(()),
    }
}

/// Stored DPI stages: read the stored block and check it before overwriting
/// it, write the new block with VARSTORE, then read it back and require
/// exactly what was written.
fn store_stages(
    transact: &mut impl FnMut(u8, Cmd) -> Result<[u8; 90]>,
    txn: u8,
    dpi_max: u16,
    active: u8,
    dpis: &[u16],
) -> Result<()> {
    check_stage_block(&transact(txn, p::get_dpi_stages(VARSTORE))?, dpi_max)?;
    transact(txn, p::set_dpi_stages(VARSTORE, active, dpis))?;
    let back = transact(txn, p::get_dpi_stages(VARSTORE))?;
    check_stage_block(&back, dpi_max)?;
    let (got_active, got) = p::parse_dpi_stages(&back);
    ensure!(
        got_active == active && got == dpis,
        "stored DPI stages read back as {got:?} (active {got_active}), wrote {dpis:?} (active {active})"
    );
    Ok(())
}

/// A stored stage block that may be overwritten: valid checksum, VARSTORE
/// echoed, 1..=5 stages that fit the answer, active stage among them, every
/// DPI within 100..=`dpi_max`.
fn check_stage_block(resp: &[u8; 90], dpi_max: u16) -> Result<()> {
    let (active, dpis) = p::parse_dpi_stages(resp);
    let count = resp[10];
    let valid = p::crc_ok(resp)
        && resp[8] == VARSTORE
        && (1..=MAX_DPI_STAGES as u8).contains(&count)
        && dpis.len() == usize::from(count)
        && (1..=count).contains(&active)
        && dpis.iter().all(|d| (100..=dpi_max).contains(d));
    ensure!(
        valid,
        "stored DPI stages look invalid, not touched: {:02x?}",
        &resp[..usize::from(resp[5]).min(80) + 8]
    );
    Ok(())
}

/// Read the stored brightness of every LED back.
fn verify_brightness(
    transact: &mut impl FnMut(u8, Cmd) -> Result<[u8; 90]>,
    txn: u8,
    leds: &[u8],
    pct: u8,
) -> Result<()> {
    let want = p::brightness_wire(pct);
    for &led in leds {
        let got = transact(txn, p::get_brightness(VARSTORE, led))?[10];
        ensure!(
            got == want,
            "stored brightness of LED {led:#04x} reads {got}, wrote {want}"
        );
    }
    Ok(())
}

fn text<'a>(s: &'a SettingsMap, key: SettingKey, default: &'a str) -> &'a str {
    get_setting(s, key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
}

fn byte(s: &SettingsMap, key: SettingKey, default: u8) -> u8 {
    get_setting(s, key)
        .and_then(|v| v.as_int())
        .map_or(default, |v| v.clamp(0, 255) as u8)
}

fn default_effect(spec: &Spec) -> Effect {
    if spec.has_effect(Effect::Static) {
        Effect::Static
    } else {
        Effect::ALL
            .into_iter()
            .find(|&fx| spec.has_effect(fx))
            .unwrap_or(Effect::Static)
    }
}

fn defaults(s: &mut SettingsMap, spec: &Spec) {
    let mut put = |k: SettingKey, v| {
        s.entry(k.as_str().into()).or_insert(v);
    };
    if !spec.leds.is_empty() {
        put(SettingKey::Brightness, SettingValue::Int(80));
        put(SettingKey::ColorR, SettingValue::Int(0));
        put(SettingKey::ColorG, SettingValue::Int(120));
        put(SettingKey::ColorB, SettingValue::Int(255));
        let fx = default_effect(spec).as_str().into();
        put(SettingKey::Effect, SettingValue::Text(fx));
        put(SettingKey::Speed, SettingValue::Text("medium".into()));
        put(SettingKey::Direction, SettingValue::Text("left".into()));
    }
    if spec.game_txn.is_some() {
        put(SettingKey::GameMode, SettingValue::Bool(false));
    }
    if spec.stages {
        for (key, dpi) in STAGE_KEYS.into_iter().zip(DEFAULT_STAGES) {
            put(key, SettingValue::Int(i64::from(dpi.min(spec.dpi_max))));
        }
        put(SettingKey::DpiStageActive, SettingValue::Int(1));
    }
    if save_offer(spec).is_ok() {
        put(SettingKey::SaveToDevice, SettingValue::Bool(false));
    }
}

fn controls(spec: &Spec, link: &Link, settings: &SettingsMap, stages: usize) -> Vec<SettingMeta> {
    use SettingKey as K;
    let mut c = Vec::new();
    let dpi_max = f64::from(spec.dpi_max);
    if spec.dpi_max > 0 {
        c.push(SettingMeta::slider(K::Dpi, "DPI", 100.0, dpi_max, 50.0).unit("dpi"));
    }
    if spec.stages {
        for (i, &key) in STAGE_KEYS[..stages].iter().enumerate() {
            let label = format!("DPI stage {}", i + 1);
            c.push(SettingMeta::slider(key, &label, 100.0, dpi_max, 50.0).unit("dpi"));
        }
        c.push(SettingMeta::slider(
            K::DpiStageActive,
            "Active DPI stage",
            1.0,
            stages as f64,
            1.0,
        ));
    }
    let rates = match link.poll {
        Poll::None => &[][..],
        Poll::Classic => CLASSIC_RATES,
        Poll::V2(r, _) => r,
    };
    if !rates.is_empty() {
        c.push(SettingMeta::dropdown(
            K::ReportRate,
            "Report rate",
            rates
                .iter()
                .map(|&hz| SettingOption::int(i64::from(hz), &format!("{hz} Hz")))
                .collect(),
        ));
    }
    if spec.game_txn.is_some() {
        c.push(SettingMeta::toggle(K::GameMode, "Game mode"));
    }
    if !spec.leds.is_empty() {
        let effect = Effect::from_str(text(settings, K::Effect, "static"));
        let effect_opts = Effect::ALL
            .into_iter()
            .filter(|&fx| spec.has_effect(fx))
            .map(|fx| SettingOption::text(fx.as_str(), fx.label()))
            .collect();
        let speed_opts = vec![
            SettingOption::text("slow", "Slow"),
            SettingOption::text("medium", "Medium"),
            SettingOption::text("fast", "Fast"),
        ];
        let dir_opts = vec![
            SettingOption::text("left", "Left"),
            SettingOption::text("right", "Right"),
        ];
        // Same order as the Huntsman V2 TKL: Effect, Brightness, Direction, Speed, Color.
        c.push(SettingMeta::dropdown(K::Effect, "Effect", effect_opts));
        c.push(SettingMeta::slider(K::Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"));
        if spec.has_effect(Effect::Wave) {
            c.push(
                SettingMeta::dropdown(K::Direction, "Direction", dir_opts)
                    .hidden(effect != Effect::Wave),
            );
        }
        if spec.has_effect(Effect::Reactive) || spec.has_effect(Effect::Starlight) {
            c.push(
                SettingMeta::dropdown(K::Speed, "Speed", speed_opts).hidden(!effect.has_speed()),
            );
        }
        for (key, label) in [
            (K::ColorR, "Red"),
            (K::ColorG, "Green"),
            (K::ColorB, "Blue"),
        ] {
            c.push(SettingMeta::color(key, label).hidden(!effect.has_color()));
        }
    }
    if spec.idle {
        c.push(
            SettingMeta::slider(K::PmShutdown, "Auto-off", IDLE_MIN.0, IDLE_MIN.1, 1.0).unit("min"),
        );
    }
    let save = SettingMeta::toggle(K::SaveToDevice, "Save changes to device memory").persistent();
    c.push(match save_offer(spec) {
        Ok(()) => save,
        Err(why) => save.blocked(why),
    });
    c
}

impl DeviceAdapter for RazerAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        let batteries = match self.spec.battery {
            Battery::None => vec![],
            _ => vec![BatteryState {
                label: "Battery".into(),
                percent: self.battery.map(|(pct, _)| pct),
                charging: self.battery.is_some_and(|(_, c)| c),
                present: true,
            }],
        };
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.spec.kind,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries,
            settings: self.settings.clone(),
            controls: controls(self.spec, &self.link, &self.settings, self.stage_count),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if !self.supports(key) {
            bail!("{} has no {} control", self.spec.name, key.as_str());
        }
        let before = (self.chosen.clone(), self.settings.clone());
        let result = if key == SettingKey::SaveToDevice {
            let on = value.as_bool().context("expected true/false")?;
            self.set_save(on)
        } else {
            // In save mode the device already holds an unchanged value.
            let store = if get_setting(&self.settings, key) == Some(&value) {
                NOSTORE
            } else {
                self.store_for(key)
            };
            merge_setting(&mut self.chosen, key, value.clone());
            merge_setting(&mut self.settings, key, value.clone());
            self.write(key, &value, store)
        };
        if let Err(e) = result {
            // The device kept its old value; so do the UI and the file.
            (self.chosen, self.settings) = before;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        self.persist()
    }

    fn tick(&mut self) -> Result<()> {
        if self.spec.battery == Battery::None || self.dev.is_none() {
            return Ok(());
        }
        let every = if self.link_up {
            BATTERY_POLL
        } else {
            LINK_PROBE
        };
        if self.last_poll.elapsed() < every {
            return Ok(());
        }
        if self.link_up {
            self.last_poll = Instant::now();
            if let Err(e) = self.read_battery() {
                debug!("{} battery: {e}", self.spec.name);
                self.link_up = false;
                self.battery = None;
                self.last_error = Some(NO_LINK.into());
            }
        } else {
            // Link back: restore volatile state the device lost while off.
            self.connect();
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
    use crate::drivers::razer::models::SPECS;

    #[test]
    fn controls_follow_spec_and_defaults_validate() {
        for spec in SPECS {
            for &pid in spec.pids {
                let link = spec.link_for(pid);
                let mut s = SettingsMap::new();
                defaults(&mut s, spec);
                let c = controls(spec, &link, &s, MAX_DPI_STAGES);
                assert!(
                    !c.is_empty() || spec.battery != Battery::None,
                    "{}",
                    spec.name
                );
                // Every default the adapter ships must pass its own control's validation.
                for meta in &c {
                    if let Some(v) = get_setting(&s, meta.key) {
                        assert!(
                            meta.validate(v).is_ok(),
                            "{} {}",
                            spec.name,
                            meta.key.as_str()
                        );
                    }
                }
                let has = |k| c.iter().any(|m| m.key == k);
                assert_eq!(has(SettingKey::Dpi), spec.dpi_max > 0, "{}", spec.name);
                assert_eq!(
                    has(SettingKey::Effect),
                    !spec.leds.is_empty(),
                    "{}",
                    spec.name
                );
                assert_eq!(
                    has(SettingKey::GameMode),
                    spec.game_txn.is_some(),
                    "{}",
                    spec.name
                );
                assert_eq!(has(SettingKey::PmShutdown), spec.idle, "{}", spec.name);
                assert_eq!(has(SettingKey::DpiStage1), spec.stages, "{}", spec.name);
                assert_eq!(
                    has(SettingKey::DpiStageActive),
                    spec.stages,
                    "{}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn stage_controls_follow_reported_count() {
        let da = spec_for(0x00b7).unwrap();
        let mut s = SettingsMap::new();
        defaults(&mut s, da);
        let c = controls(da, &da.link, &s, 3);
        let stage_keys = c.iter().filter(|m| STAGE_KEYS.contains(&m.key)).count();
        assert_eq!(stage_keys, 3);
        let active = c
            .iter()
            .find(|m| m.key == SettingKey::DpiStageActive)
            .unwrap();
        assert_eq!((active.min, active.max), (Some(1.0), Some(3.0)));
        assert!(active.validate(&SettingValue::Int(4)).is_err());
        // Defaults never exceed the model's DPI ceiling.
        let mini = spec_for(0x008c).unwrap();
        let mut s = SettingsMap::new();
        defaults(&mut s, mini);
        assert_eq!(
            get_setting(&s, SettingKey::DpiStage5).and_then(|v| v.as_int()),
            Some(6400)
        );
    }

    #[test]
    fn effect_dropdown_only_offers_supported_effects() {
        let v3 = spec_for(0x0099).unwrap();
        let mut s = SettingsMap::new();
        defaults(&mut s, v3);
        let c = controls(v3, &v3.link, &s, MAX_DPI_STAGES);
        let fx = c.iter().find(|m| m.key == SettingKey::Effect).unwrap();
        let offered: Vec<_> = fx.options.iter().filter_map(|o| o.value.as_str()).collect();
        assert_eq!(offered, ["static", "spectrum", "wave"]);
        // No reactive / starlight → no speed control at all.
        assert!(!c.iter().any(|m| m.key == SettingKey::Speed));
    }

    #[test]
    fn hyperpolling_rates_per_link() {
        let viper = spec_for(0x00c1).unwrap();
        let rates = |pid| {
            let c = controls(
                viper,
                &viper.link_for(pid),
                &SettingsMap::new(),
                MAX_DPI_STAGES,
            );
            c.into_iter()
                .find(|m| m.key == SettingKey::ReportRate)
                .unwrap()
                .options
                .len()
        };
        assert_eq!(rates(0x00c0), 3);
        assert_eq!(rates(0x00c1), 6);
    }

    /// A device that keeps what it is set to and answers like the firmware:
    /// status ok, a set echoed, a get (command | 0x80) with the arguments of
    /// the last matching set, whatever its storage byte.
    #[derive(Default)]
    struct Fake {
        sent: Vec<[u8; 90]>,
        stages: Vec<u8>,
        /// Last set arguments per (class, set command, LED for class 0x0f).
        held: std::collections::HashMap<(u8, u8, u8), Vec<u8>>,
        /// Acknowledge stage writes without keeping them.
        drop_writes: bool,
        bad_crc: bool,
    }

    impl Fake {
        /// 800 / 1600 / 3200 DPI, second stage active.
        fn new() -> Self {
            let (_, _, stages) = p::set_dpi_stages(VARSTORE, 2, &[800, 1600, 3200]);
            Self {
                stages,
                ..Self::default()
            }
        }

        fn answer(&mut self, req: &[u8; 90]) -> [u8; 90] {
            self.sent.push(*req);
            let (class, cmd) = (req[6], req[7]);
            let args = &req[8..8 + usize::from(req[5])];
            let led = if class == 0x0f { args[1] } else { 0 };
            let out = match (class, cmd) {
                (0x04, 0x86) => [&args[..1], &self.stages[1..]].concat(),
                (0x04, 0x06) if !self.drop_writes => {
                    self.stages = args.to_vec();
                    args.to_vec()
                }
                (_, get) if get & 0x80 != 0 => self
                    .held
                    .get(&(class, get & 0x7f, led))
                    .cloned()
                    .unwrap_or_else(|| args.to_vec()),
                _ => {
                    self.held.insert((class, cmd, led), args.to_vec());
                    args.to_vec()
                }
            };
            let mut resp = p::make_report(req[1], class, cmd, &out);
            resp[0] = 0x02;
            if self.bad_crc {
                resp[88] ^= 0xff;
            }
            resp
        }

        fn link(&mut self) -> impl FnMut(u8, Cmd) -> Result<[u8; 90]> + '_ {
            |txn, (class, cmd, args)| Ok(self.answer(&p::make_report(txn, class, cmd, &args)))
        }
    }

    struct Shared(std::sync::Arc<std::sync::Mutex<Fake>>);

    impl Feature for Shared {
        fn exchange(&mut self, buf: &mut [u8; 91], _wait: Duration) -> Result<()> {
            let req: [u8; 90] = buf[1..].try_into().unwrap();
            buf[1..].copy_from_slice(&self.0.lock().unwrap().answer(&req));
            Ok(())
        }

        fn gone(&self) -> bool {
            false
        }
    }

    /// (class, cmd) of every set (no read) sent, then forget them all.
    fn writes(fake: &std::sync::Mutex<Fake>) -> Vec<(u8, u8)> {
        let sent = std::mem::take(&mut fake.lock().unwrap().sent);
        sent.iter()
            .filter(|r| r[7] & 0x80 == 0)
            .map(|r| (r[6], r[7]))
            .collect()
    }

    /// (class, cmd, storage byte) of each frame that carries one, then forget them.
    fn stores(fake: &std::sync::Mutex<Fake>) -> Vec<(u8, u8, u8)> {
        let sent = std::mem::take(&mut fake.lock().unwrap().sent);
        sent.iter()
            .filter(|r| {
                matches!(
                    (r[6], r[7]),
                    (0x0f, 0x02 | 0x04 | 0x84) | (0x04, 0x05 | 0x06 | 0x85 | 0x86)
                )
            })
            .map(|r| (r[6], r[7], r[8]))
            .collect()
    }

    fn fake_adapter(
        pid: u16,
        fake: &std::sync::Arc<std::sync::Mutex<Fake>>,
        root: &std::path::Path,
    ) -> RazerAdapter {
        let spec = spec_for(pid).unwrap();
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.to_path_buf(),
        };
        paths.ensure().unwrap();
        let mut settings = SettingsMap::new();
        defaults(&mut settings, spec);
        RazerAdapter {
            spec,
            link: spec.link_for(pid),
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: "fake".into(),
            },
            dev: Some(Box::new(Shared(fake.clone()))),
            settings,
            chosen: SettingsMap::new(),
            battery: None,
            last_error: None,
            paths,
            link_up: false,
            last_poll: Instant::now(),
            stage_count: MAX_DPI_STAGES,
        }
    }

    #[test]
    fn varstore_frames_are_byte_identical() {
        // Brightness 80 % on the logo, stored, transaction 0x3f:
        // 00 3f 0000 00 03 0f 04 01 04 cc .. crc c1
        let r = p::make_report(0x3f, 0x0f, 0x04, &p::set_brightness(VARSTORE, 0x04, 80).2);
        assert_eq!(
            &r[..11],
            &[0, 0x3f, 0, 0, 0, 0x03, 0x0f, 0x04, 0x01, 0x04, 0xcc]
        );
        assert_eq!(r[88], 0xc1);
        // Stored brightness read: 0f 84 01 04 00.
        assert_eq!(
            p::get_brightness(VARSTORE, 0x04),
            (0x0f, 0x84, vec![0x01, 0x04, 0x00])
        );
        // Static red on the logo, stored (razerchromacommon.c / freenapse vector).
        assert_eq!(
            p::set_effect(VARSTORE, 0x04, Effect::Static, [0xff, 0, 0], "medium", 1),
            (
                0x0f,
                0x02,
                vec![0x01, 0x04, 0x01, 0x00, 0x00, 0x01, 0xff, 0x00, 0x00]
            )
        );
        // Stages 800 / 1800 / 3600, second active, stored: 04 06 size 26.
        let (class, cmd, args) = p::set_dpi_stages(VARSTORE, 2, &[800, 1800, 3600]);
        assert_eq!((class, cmd, args.len()), (0x04, 0x06, 0x26));
        assert_eq!(
            &args[..24],
            &[
                0x01, 0x02, 0x03, 0x00, 0x03, 0x20, 0x03, 0x20, 0x00, 0x00, 0x01, 0x07, 0x08, 0x07,
                0x08, 0x00, 0x00, 0x02, 0x0e, 0x10, 0x0e, 0x10, 0x00, 0x00
            ]
        );
        assert_eq!(p::get_dpi_stages(VARSTORE).2[0], 0x01);
    }

    #[test]
    fn stage_store_reads_checks_writes_and_reads_back() {
        let mut fake = Fake::new();
        store_stages(&mut fake.link(), 0xff, 20000, 1, &[400, 900, 2000]).unwrap();
        let frames: Vec<_> = fake.sent.iter().map(|r| (r[1], r[6], r[7], r[8])).collect();
        assert_eq!(
            frames,
            [
                (0xff, 0x04, 0x86, 0x01),
                (0xff, 0x04, 0x06, 0x01),
                (0xff, 0x04, 0x86, 0x01)
            ]
        );
        assert_eq!(&fake.stages[..5], &[0x01, 0x01, 0x03, 0x00, 0x01]);
    }

    #[test]
    fn invalid_stored_stages_abort_before_any_write() {
        let bad: [fn(&mut Fake); 4] = [
            |f| f.bad_crc = true,
            |f| f.stages[2] = 0,           // no stages
            |f| f.stages[1] = 4,           // active stage past the count
            |f| f.stages[4..6].fill(0xff), // 65535 DPI
        ];
        for corrupt in bad {
            let mut fake = Fake::new();
            corrupt(&mut fake);
            assert!(store_stages(&mut fake.link(), 0x1f, 30000, 1, &[400, 800]).is_err());
            assert_eq!(fake.sent.len(), 1, "only the read went out");
        }
    }

    #[test]
    fn stage_read_back_mismatch_is_an_error() {
        let mut fake = Fake {
            drop_writes: true,
            ..Fake::new()
        };
        let err = store_stages(&mut fake.link(), 0x1f, 30000, 1, &[400, 800])
            .unwrap_err()
            .to_string();
        assert!(err.contains("read back"), "{err}");
    }

    #[test]
    fn stored_brightness_is_read_back() {
        let mut fake = Fake::new();
        let wire = p::brightness_wire(50);
        fake.held.insert((0x0f, 0x04, 0x04), vec![0x01, 0x04, wire]);
        assert!(verify_brightness(&mut fake.link(), 0x3f, &[0x04], 50).is_ok());
        assert!(verify_brightness(&mut fake.link(), 0x3f, &[0x04], 60).is_err());
    }

    /// Save mode stores only user changes: connect re-applies NOSTORE, turning
    /// it on stores once, unchanged values and other controls stay NOSTORE,
    /// turning it off sends nothing.
    #[test]
    fn save_mode_stores_only_user_changes() {
        let root = std::env::temp_dir().join(format!("hako-razer-save-{}", std::process::id()));
        let fake = std::sync::Arc::new(std::sync::Mutex::new(Fake::new()));
        // DeathAdder V2 Mini: logo LED on 0x3f, stages on 0xff, wired.
        let mut a = fake_adapter(0x008c, &fake, &root);
        let set_nostore = |fake: &std::sync::Mutex<Fake>| {
            stores(fake)
                .iter()
                .all(|&(_, cmd, store)| cmd & 0x80 != 0 || store == NOSTORE)
        };
        a.connect();
        assert_eq!(a.last_error, None);
        assert!(set_nostore(&fake));

        a.set(SettingKey::SaveToDevice, SettingValue::Bool(true))
            .unwrap();
        let stored = [(0x04, 0x86, 1), (0x04, 0x06, 1), (0x04, 0x86, 1)];
        let lit = [(0x0f, 0x04, 1), (0x0f, 0x02, 1), (0x0f, 0x84, 1)];
        assert_eq!(stores(&fake), [&lit[..], &stored[..]].concat());
        let file = hako_core::config::load_device_settings(&a.paths, VID, 0x008c).unwrap();
        assert_eq!(
            get_setting(&file, SettingKey::SaveToDevice),
            Some(&SettingValue::Bool(true))
        );

        a.set(SettingKey::Brightness, SettingValue::Int(40))
            .unwrap();
        assert_eq!(stores(&fake), lit);
        a.set(SettingKey::Brightness, SettingValue::Int(40))
            .unwrap();
        assert_eq!(stores(&fake), [(0x0f, 0x04, 0), (0x0f, 0x02, 0)]);
        a.set(SettingKey::Dpi, SettingValue::Int(800)).unwrap();
        assert_eq!(stores(&fake), [(0x04, 0x05, 0)]);
        a.set(SettingKey::DpiStage1, SettingValue::Int(900))
            .unwrap();
        assert_eq!(stores(&fake), stored);
        assert_eq!(fake.lock().unwrap().stages[3..6], [0x00, 0x03, 0x84]);

        // A reconnect finds every value in place and writes nothing.
        a.connect();
        assert!(set_nostore(&fake));
        a.connect();
        assert_eq!(writes(&fake), []);

        a.set(SettingKey::SaveToDevice, SettingValue::Bool(false))
            .unwrap();
        assert!(fake.lock().unwrap().sent.is_empty());
        a.set(SettingKey::Brightness, SettingValue::Int(41))
            .unwrap();
        assert_eq!(stores(&fake), [(0x0f, 0x04, 0), (0x0f, 0x02, 0)]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_mode_offer_per_model() {
        let save = |pid| {
            let spec = spec_for(pid).unwrap();
            let mut s = SettingsMap::new();
            defaults(&mut s, spec);
            let c = controls(spec, &spec.link, &s, MAX_DPI_STAGES);
            let m = c
                .into_iter()
                .find(|m| m.key == SettingKey::SaveToDevice)
                .unwrap();
            assert!(m.persistent);
            (m.blocked, storable(spec))
        };
        assert_eq!(save(0x0287), (None, (true, false)), "BlackWidow V4");
        assert_eq!(save(0x00c1), (None, (false, true)), "Viper V3 Pro");
        assert_eq!(save(0x0099), (None, (false, true)), "Basilisk V3");
        let (why, _) = save(0x02a7);
        assert!(why.unwrap().contains("#2935"), "Huntsman V3 Pro TKL");
        let (why, _) = save(0x004f);
        assert_eq!(why.as_deref(), Some(NOTHING_STORABLE), "DeathAdder 2000");
    }

    /// Set every control the model has, as a user would.
    fn choose_everything(a: &mut RazerAdapter) {
        use SettingValue::{Bool, Int};
        for (key, value) in [
            (SettingKey::Dpi, Int(1600)),
            (SettingKey::ReportRate, Int(500)),
            (SettingKey::PmShutdown, Int(5)),
            (SettingKey::DpiStage1, Int(900)),
            (SettingKey::Brightness, Int(40)),
            (SettingKey::GameMode, Bool(true)),
        ] {
            if a.supports(key) {
                a.set(key, value).unwrap();
            }
        }
    }

    /// Reconnect re-apply reads first: equal values send only reads, one
    /// changed value on the device gets exactly its one write.
    #[test]
    fn reconnect_writes_only_what_differs() {
        let root = std::env::temp_dir().join(format!("hako-razer-re-{}", std::process::id()));
        // DeathAdder V2 Pro (DPI, classic poll, idle, stages, logo LED) and
        // BlackWidow V4 (HyperPolling, game mode, backlight).
        type Change = fn(&mut Fake);
        let cases: [(u16, Change, (u8, u8)); 7] = [
            (
                0x007c,
                |f| f.held.get_mut(&(0x04, 0x05, 0)).unwrap()[2] ^= 1,
                (0x04, 0x05),
            ),
            (
                0x007c,
                |f| f.held.get_mut(&(0x00, 0x05, 0)).unwrap()[0] = 0x08,
                (0x00, 0x05),
            ),
            (
                0x007c,
                |f| f.held.get_mut(&(0x07, 0x03, 0)).unwrap()[1] ^= 1,
                (0x07, 0x03),
            ),
            (0x007c, |f| f.stages[5] ^= 1, (0x04, 0x06)),
            (
                0x007c,
                |f| f.held.get_mut(&(0x0f, 0x04, 0x04)).unwrap()[2] ^= 1,
                (0x0f, 0x04),
            ),
            (
                0x0287,
                |f| f.held.get_mut(&(0x03, 0x00, 0)).unwrap()[2] = 0,
                (0x03, 0x00),
            ),
            (
                0x0287,
                |f| f.held.get_mut(&(0x0f, 0x02, 0x05)).unwrap()[6] ^= 1,
                (0x0f, 0x02),
            ),
        ];
        for (pid, change, write) in cases {
            let fake = std::sync::Arc::new(std::sync::Mutex::new(Fake::new()));
            let mut a = fake_adapter(pid, &fake, &root);
            choose_everything(&mut a);
            writes(&fake);
            a.connect();
            assert_eq!(a.last_error, None);
            assert_eq!(writes(&fake), [], "{pid:04x}: equal values, reads only");
            change(&mut fake.lock().unwrap());
            a.connect();
            assert_eq!(writes(&fake), [write], "{pid:04x}");
        }
        // The game-mode read OpenRazer sends for an 0xff keyboard.
        let fake = std::sync::Arc::new(std::sync::Mutex::new(Fake::new()));
        let mut a = fake_adapter(0x0287, &fake, &root);
        a.chosen
            .insert("game_mode".into(), SettingValue::Bool(false));
        a.connect();
        let sent = std::mem::take(&mut fake.lock().unwrap().sent);
        let read = sent.iter().find(|r| (r[6], r[7]) == (0x03, 0x80)).unwrap();
        assert_eq!((read[1], &read[8..11]), (0xff, &[0x01, 0x08, 0x00][..]));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Lighting that is not stored on the model is re-sent as before.
    #[test]
    fn unstorable_lighting_is_reapplied_without_reads() {
        let root = std::env::temp_dir().join(format!("hako-razer-v3re-{}", std::process::id()));
        let fake = std::sync::Arc::new(std::sync::Mutex::new(Fake::new()));
        let mut a = fake_adapter(0x0099, &fake, &root);
        a.connect();
        writes(&fake);
        a.connect();
        let sent = std::mem::take(&mut fake.lock().unwrap().sent);
        let lighting: Vec<_> = sent.iter().filter(|r| r[6] == 0x0f).map(|r| r[7]).collect();
        assert_eq!(lighting, [0x04, 0x02]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Basilisk V3 in save mode: stages are stored, lighting never is.
    #[test]
    fn volatile_lighting_stays_nostore_in_save_mode() {
        let root = std::env::temp_dir().join(format!("hako-razer-bv3-{}", std::process::id()));
        let fake = std::sync::Arc::new(std::sync::Mutex::new(Fake::new()));
        let mut a = fake_adapter(0x0099, &fake, &root);
        a.connect();
        stores(&fake);
        a.set(SettingKey::SaveToDevice, SettingValue::Bool(true))
            .unwrap();
        assert_eq!(
            stores(&fake),
            [(0x04, 0x86, 1), (0x04, 0x06, 1), (0x04, 0x86, 1)]
        );
        a.set(SettingKey::Brightness, SettingValue::Int(10))
            .unwrap();
        assert_eq!(stores(&fake), [(0x0f, 0x04, 0), (0x0f, 0x02, 0)]);
        let _ = std::fs::remove_dir_all(root);
    }
}
