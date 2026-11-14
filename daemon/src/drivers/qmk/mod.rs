//! QMK / VIA keyboards of any vendor, over QMK raw HID.
//!
//! Claimed at runtime (no model table): any unlisted VID:PID with a hidraw
//! node whose report descriptor declares the QMK raw HID application
//! collection, usage page 0xFF60 / usage 0x61 (QMK
//! `tmk_core/protocol/usb_descriptor.c` `RawReport`). Host-side remaps attach
//! because the device is a keyboard.
//!
//! Lighting goes through the VIA protocol (see [`via`]): the protocol version,
//! then which lighting channel the firmware answers (RGB Matrix, RGB light,
//! LED Matrix, backlight; the richest one is driven). Effect numbers depend
//! on which effects the firmware was built with (QMK `rgb_matrix_effects.inc`,
//! `rgblight_modes.h`, `led_matrix_effects.inc` are `#ifdef`-gated), so they
//! are a numeric slider (0 = off) and the firmware's clamped value is read
//! back; only the backlight's breathing toggle is a fixed choice. Every value
//! write is a `_noeeprom` one in QMK `quantum/via.c` except that toggle:
//! `backlight_enable_breathing` / `backlight_disable_breathing` call
//! `eeconfig_update_backlight` (QMK `quantum/backlight/backlight.c`), so it is
//! a persistent control. Chosen volatile values are re-applied on every
//! connect; persistent ones never are.
//!
//! "Save changes to device memory" (`save_to_device`) sends `id_custom_save`
//! for the driven channel (see [`via`]) after each lighting change, and once
//! when switched on, as the VIA app does. VIA cannot read the EEPROM back, so
//! the firmware's echo is the only confirmation; an unhandled save is an
//! error. EEPROM resets, keymap and macro writes are never sent. The kernel's
//! HID battery, if any, is shown.

mod via;

use super::{DeviceAdapter, DriverCtx, Fallback, Opened};
use crate::hid::{descriptor_items, enumerate_hidraw, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tracing::info;
use via::{Channel, Reply, Value};

/// QMK `RAW_USAGE_PAGE` / `RAW_USAGE_ID` as one extended usage.
const RAW_HID_USAGE: u32 = 0xff60_0061;
const REPLY_TIMEOUT: Duration = Duration::from_millis(300);
const BATTERY_POLL: Duration = Duration::from_secs(30);
/// A detection that failed runs again after 2 s, doubling up to a minute.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// Unanswered version probes before the board counts as raw HID without VIA.
const SILENT_TRIES: u32 = 3;

/// Claims a VID:PID whose nodes expose the QMK raw HID interface.
pub fn fallback(nodes: &[&HidDeviceInfo]) -> Option<Fallback> {
    nodes
        .iter()
        .any(|n| is_raw_hid(&n.report_descriptor()))
        .then(|| Fallback {
            kind: DeviceKind::Keyboard,
            name: display_name(nodes),
            open,
        })
}

/// True when the descriptor opens an application collection whose usage is
/// 0xFF60:0x61 (`06 60 ff 09 61 a1 01` in QMK's descriptor).
fn is_raw_hid(desc: &[u8]) -> bool {
    let mut page = 0u32;
    let mut usage = None;
    for (tag, value) in descriptor_items(desc) {
        match tag {
            // Usage Page (global).
            0x04 => page = value,
            // Usage (local); a 4-byte usage already carries its page.
            0x08 => {
                usage = Some(if value > 0xffff {
                    value
                } else {
                    (page << 16) | value
                })
            }
            // Collection: Application (1) with the raw HID usage.
            0xa0 => {
                if value == 1 && usage == Some(RAW_HID_USAGE) {
                    return true;
                }
                usage = None;
            }
            // Input / Output / Feature / End Collection clear local items.
            0x80 | 0x90 | 0xb0 | 0xc0 => usage = None,
            _ => {}
        }
    }
    false
}

fn display_name(nodes: &[&HidDeviceInfo]) -> String {
    let named = nodes.iter().map(|n| n.name.trim()).find(|n| !n.is_empty());
    match (named, nodes.first()) {
        (Some(n), _) => n.to_string(),
        (None, Some(f)) => format!("QMK keyboard {:04x}:{:04x}", f.vendor_id, f.product_id),
        (None, None) => "QMK keyboard".into(),
    }
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let vid = ctx.vendor_id;
    let nodes: Vec<HidDeviceInfo> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .collect();
    let refs: Vec<&HidDeviceInfo> = nodes.iter().collect();
    let node = nodes
        .iter()
        .find(|n| is_raw_hid(&n.report_descriptor()))
        .with_context(|| format!("{vid:04x}:{pid:04x} has no QMK raw HID interface"))?;
    let dev = HidrawDevice::open(&node.path)?;
    info!("QMK raw HID open via {}", dev.path.display());
    let id = DeviceId {
        vendor_id: vid,
        product_id: pid,
        instance: if node.uniq.is_empty() {
            node.path.display().to_string()
        } else {
            node.uniq.clone()
        },
    };
    let chosen = hako_core::config::load_device_settings(&ctx.paths, vid, pid)?;
    let mut kb = QmkKeyboard {
        id,
        name: display_name(&refs),
        link: Some(Box::new(dev)),
        protocol: None,
        channel: None,
        settings: SettingsMap::new(),
        chosen,
        paths: ctx.paths.clone(),
        last_error: None,
        hid_dirs: nodes.iter().map(|n| n.sys.clone()).collect(),
        battery: None,
        last_poll: Instant::now(),
        detect_at: None,
        detect_tries: 0,
    };
    kb.battery = kb.read_battery();
    kb.connect();
    Ok(vec![Box::new(kb)])
}

/// Report transport, so the protocol logic can be tested without hardware.
trait Link: Send {
    fn send(&mut self, report: &[u8]) -> Result<()>;
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
}

impl Link for HidrawDevice {
    fn send(&mut self, report: &[u8]) -> Result<()> {
        self.write(report)
    }

    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.read_timeout(buf, timeout)
    }
}

/// No reply in time (unlike an I/O error, the keyboard may just be busy, or
/// have no VIA).
#[derive(Debug)]
struct NoReply(u8);

impl std::fmt::Display for NoReply {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "no VIA reply to command {:#04x}", self.0)
    }
}

impl std::error::Error for NoReply {}

/// Send one VIA command and wait for its echo. `None` = unhandled.
fn call(link: &mut dyn Link, cmd: &[u8], header_len: usize) -> Result<Option<Vec<u8>>> {
    let mut buf = [0u8; 64];
    // Stale replies (another VIA client shares the node) are not ours.
    for _ in 0..64 {
        if link.recv(&mut buf, Duration::ZERO).is_err() {
            break;
        }
    }
    link.send(&via::report(cmd))?;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(NoReply(cmd[0]).into());
        }
        let n = match link.recv(&mut buf, left) {
            Ok(n) => n,
            // `read_timeout` fails with an I/O error, or a plain one on timeout.
            Err(e) if e.downcast_ref::<std::io::Error>().is_none() => {
                return Err(NoReply(cmd[0]).into())
            }
            Err(e) => return Err(e),
        };
        match via::parse_reply(cmd, header_len, &buf[..n]) {
            Reply::Data(d) => return Ok(Some(d.to_vec())),
            Reply::Unhandled => return Ok(None),
            Reply::Other => {}
        }
    }
}

const COLOR_KEYS: [SettingKey; 3] = [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB];

/// Controls for the driven channel; speed and color hide while it is off.
fn controls(ch: Channel, settings: &SettingsMap) -> Vec<SettingMeta> {
    let off = int(settings, SettingKey::Effect) == Some(0);
    let mut c = vec![if ch == Channel::Backlight {
        SettingMeta::dropdown(
            SettingKey::Effect,
            "Effect",
            vec![
                SettingOption::int(0, "Steady"),
                SettingOption::int(1, "Breathing"),
            ],
        )
        .persistent()
    } else {
        SettingMeta::slider(SettingKey::Effect, "Effect (0 = off)", 0.0, 255.0, 1.0)
    }];
    c.push(SettingMeta::slider(SettingKey::Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"));
    if ch.has(Value::Speed) {
        let max = f64::from(ch.speed_max());
        c.push(SettingMeta::slider(SettingKey::Speed, "Effect speed", 0.0, max, 1.0).hidden(off));
    }
    if ch.has(Value::Color) {
        for (key, label) in COLOR_KEYS.into_iter().zip(["Red", "Green", "Blue"]) {
            c.push(SettingMeta::color(key, label).hidden(off));
        }
    }
    c.push(
        SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory").persistent(),
    );
    c
}

fn int(settings: &SettingsMap, key: SettingKey) -> Option<i64> {
    get_setting(settings, key).and_then(SettingValue::as_int)
}

fn rgb(settings: &SettingsMap) -> [u8; 3] {
    COLOR_KEYS.map(|k| int(settings, k).unwrap_or(255).clamp(0, 255) as u8)
}

pub struct QmkKeyboard {
    id: DeviceId,
    name: String,
    link: Option<Box<dyn Link>>,
    /// VIA protocol version; `None` = no VIA on the raw HID interface.
    protocol: Option<u16>,
    channel: Option<Channel>,
    /// What the UI shows: the device's state with the user's choices on top.
    settings: SettingsMap,
    /// What the user picked; persisted and re-applied on connect.
    chosen: SettingsMap,
    paths: ConfigPaths,
    last_error: Option<String>,
    hid_dirs: Vec<PathBuf>,
    battery: Option<BatteryState>,
    last_poll: Instant,
    /// When `detect` runs again; `None` once it succeeded or gave up.
    detect_at: Option<Instant>,
    detect_tries: u32,
}

impl QmkKeyboard {
    fn call(&mut self, cmd: &[u8], header_len: usize) -> Result<Option<Vec<u8>>> {
        let link = self.link.as_deref_mut().context("not connected")?;
        call(link, cmd, header_len)
    }

    fn get(&mut self, ch: Channel, v: Value) -> Result<Vec<u8>> {
        self.call(&via::get_value(ch, v), via::header_len(ch))?
            .with_context(|| format!("firmware does not report {} {v:?}", ch.label()))
    }

    fn put(&mut self, ch: Channel, v: Value, data: &[u8]) -> Result<()> {
        match self.call(&via::set_value(ch, v, data), via::header_len(ch))? {
            Some(_) => Ok(()),
            None => bail!("firmware rejected {} {v:?}", ch.label()),
        }
    }

    /// Run `detect`; a failure is shown and retried from `tick`.
    fn connect(&mut self) {
        let result = self.detect();
        self.detect_tries = self.detect_tries.saturating_add(1);
        match result {
            Ok(()) => {
                self.detect_at = None;
                self.last_error = None;
            }
            // QMK's default `raw_hid_receive` ignores every report, so raw HID
            // without VIA never answers: remaps only, once a few probes (in
            // case the keyboard was busy) went unanswered.
            Err(e) if self.protocol.is_none() && e.is::<NoReply>() => {
                if self.detect_tries >= SILENT_TRIES {
                    info!("{}: raw HID without VIA; remaps only", self.name);
                    self.detect_at = None;
                } else {
                    self.retry_later();
                }
            }
            Err(e) => {
                self.last_error = Some(format!("{e:#}"));
                self.retry_later();
            }
        }
    }

    fn retry_later(&mut self) {
        let doublings = self.detect_tries.saturating_sub(1).min(5);
        let wait = (RETRY_MIN * (1 << doublings)).min(RETRY_MAX);
        self.detect_at = Some(Instant::now() + wait);
    }

    /// Protocol version, lighting channel, current state, then the user's values.
    fn detect(&mut self) -> Result<()> {
        let Some(reply) = self.call(&via::protocol_version(), 1)? else {
            info!("{}: raw HID without VIA; remaps only", self.name);
            return Ok(());
        };
        let version = via::parse_version(&reply).context("short VIA version reply")?;
        self.protocol = Some(version);
        let ch = if version >= via::CHANNELS_SINCE {
            let mut found = None;
            for ch in Channel::MODERN {
                if self
                    .call(&via::get_value(ch, Value::Brightness), via::header_len(ch))?
                    .is_some()
                {
                    found = Some(ch);
                    break;
                }
            }
            found
        } else if version >= via::LEGACY_SINCE {
            // Unknown lighting ids leave the buffer as sent (zero); rgblight
            // and RGB Matrix modes are clamped to >= 1. A backlight-only board
            // cannot be told from a board without VIA lighting: not driven.
            self.call(&via::get_value(Channel::Legacy, Value::Effect), 2)?
                .filter(|d| d.first().is_some_and(|&m| m >= 1))
                .map(|_| Channel::Legacy)
        } else {
            None
        };
        info!(
            "{}: VIA protocol {version:#06x}, lighting {}",
            self.name,
            ch.map_or("none", Channel::label)
        );
        self.channel = ch;
        if let Some(ch) = ch {
            self.read_state(ch)?;
            self.reapply(ch)?;
        }
        Ok(())
    }

    fn read_state(&mut self, ch: Channel) -> Result<()> {
        let b = self.get(ch, Value::Brightness)?;
        let mut s = SettingsMap::new();
        let byte = |d: &[u8]| i64::from(d.first().copied().unwrap_or(0));
        merge_setting(
            &mut s,
            SettingKey::Brightness,
            SettingValue::Int(via::byte_to_pct(b.first().copied().unwrap_or(0))),
        );
        let effect = byte(&self.get(ch, Value::Effect)?);
        // Backlight effect is 0/1; anything else would not be a choice.
        let effect = if ch == Channel::Backlight {
            effect.min(1)
        } else {
            effect
        };
        merge_setting(&mut s, SettingKey::Effect, SettingValue::Int(effect));
        if ch.has(Value::Speed) {
            let speed = byte(&self.get(ch, Value::Speed)?).min(i64::from(ch.speed_max()));
            merge_setting(&mut s, SettingKey::Speed, SettingValue::Int(speed));
        }
        if ch.has(Value::Color) {
            let hs = self.get(ch, Value::Color)?;
            let color = via::hs_to_rgb([
                hs.first().copied().unwrap_or(0),
                hs.get(1).copied().unwrap_or(0),
            ]);
            for (k, c) in COLOR_KEYS.into_iter().zip(color) {
                merge_setting(&mut s, k, SettingValue::Int(c.into()));
            }
        }
        merge_setting(
            &mut s,
            SettingKey::SaveToDevice,
            SettingValue::Bool(self.saving()),
        );
        self.settings = s;
        Ok(())
    }

    fn saving(&self) -> bool {
        get_setting(&self.chosen, SettingKey::SaveToDevice).and_then(SettingValue::as_bool)
            == Some(true)
    }

    /// `id_custom_save`: the firmware stores the channel's state in EEPROM.
    fn save(&mut self, ch: Channel) -> Result<()> {
        let cmd = via::save(ch);
        match self.call(&cmd, cmd.len())? {
            Some(_) => Ok(()),
            None => bail!("firmware does not save {} to its memory", ch.label()),
        }
    }

    /// Send the user's saved values the current controls accept; never one
    /// that writes the keyboard's memory.
    fn reapply(&mut self, ch: Channel) -> Result<()> {
        let metas = controls(ch, &self.settings);
        let mut pending = Vec::new();
        for meta in metas.iter().filter(|m| !m.persistent) {
            let Some(v) = get_setting(&self.chosen, meta.key) else {
                continue;
            };
            if meta.validate(v).is_ok() {
                merge_setting(&mut self.settings, meta.key, v.clone());
                // One write carries the whole color.
                let key = if COLOR_KEYS.contains(&meta.key) {
                    SettingKey::ColorR
                } else {
                    meta.key
                };
                if !pending.contains(&key) {
                    pending.push(key);
                }
            }
        }
        // `controls` lists effect first, so it is enabled before the rest.
        for key in pending {
            self.write(ch, key)?;
            if key == SettingKey::Effect {
                self.read_effect(ch)?;
            }
        }
        Ok(())
    }

    /// The firmware clamps the effect to the ones it was built with; show
    /// what it runs. Legacy firmware reports the mode even when off.
    fn read_effect(&mut self, ch: Channel) -> Result<()> {
        if ch != Channel::Legacy {
            let actual = self.get(ch, Value::Effect)?;
            let actual = i64::from(actual.first().copied().unwrap_or(0));
            merge_setting(
                &mut self.settings,
                SettingKey::Effect,
                SettingValue::Int(actual),
            );
        }
        Ok(())
    }

    /// Send the current value of `key` (any color key sends the whole color).
    fn write(&mut self, ch: Channel, key: SettingKey) -> Result<()> {
        let s = &self.settings;
        match key {
            SettingKey::Brightness => {
                let b = via::pct_to_byte(int(s, key).unwrap_or(100));
                self.put(ch, Value::Brightness, &[b])
            }
            SettingKey::Speed => {
                let v = int(s, key).unwrap_or(0).clamp(0, i64::from(ch.speed_max())) as u8;
                self.put(ch, Value::Speed, &[v])
            }
            SettingKey::Effect => {
                let v = int(s, key).unwrap_or(1).clamp(0, 255) as u8;
                self.put(ch, Value::Effect, &[v])
            }
            SettingKey::ColorR | SettingKey::ColorG | SettingKey::ColorB => {
                let hs = via::rgb_to_hs(rgb(s));
                self.put(ch, Value::Color, &hs)
            }
            _ => bail!("{} has no {} control", self.name, key.as_str()),
        }
    }

    /// The kernel's HID battery (`power_supply/*` under any of the nodes).
    fn read_battery(&self) -> Option<BatteryState> {
        self.hid_dirs.iter().find_map(|d| {
            let ps = std::fs::read_dir(d.join("power_supply"))
                .ok()?
                .flatten()
                .next()?;
            let read = |f: &str| std::fs::read_to_string(ps.path().join(f)).ok();
            let percent = read("capacity").and_then(|s| s.trim().parse::<u8>().ok());
            let status = read("status").unwrap_or_default();
            Some(BatteryState {
                label: "Battery".into(),
                percent: percent.map(|p| p.min(100)),
                charging: status.trim() == "Charging",
                present: true,
            })
        })
    }
}

impl DeviceAdapter for QmkKeyboard {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Keyboard,
            name: self.name.clone(),
            connected: self.link.is_some(),
            batteries: self.battery.iter().cloned().collect(),
            settings: self.settings.clone(),
            controls: self
                .channel
                .map(|ch| controls(ch, &self.settings))
                .unwrap_or_default(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if key == SettingKey::SaveToDevice {
            let on = value.as_bool().unwrap_or(false);
            // Switching on stores the current state once; off writes nothing.
            if let (true, Some(ch)) = (on, self.channel) {
                if let Err(e) = self.save(ch) {
                    self.last_error = Some(e.to_string());
                    return Err(e);
                }
            }
            merge_setting(&mut self.settings, key, value.clone());
            merge_setting(&mut self.chosen, key, value);
            return hako_core::config::save_device_settings(
                &self.paths,
                self.id.vendor_id,
                self.id.product_id,
                &self.chosen,
            )
            .context("applied, but not saved");
        }
        let ch = self
            .channel
            .with_context(|| match (self.protocol, self.detect_at) {
                (_, Some(_)) => format!("{} has not answered VIA yet; retrying", self.name),
                (Some(v), None) => {
                    format!("{} has no VIA lighting (protocol {v:#06x})", self.name)
                }
                (None, None) => format!("{} does not speak VIA", self.name),
            })?;
        let previous = self.settings.clone();
        merge_setting(&mut self.settings, key, value.clone());
        if let Err(e) = self.write(ch, key) {
            self.settings = previous;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        merge_setting(&mut self.chosen, key, value);
        if COLOR_KEYS.contains(&key) {
            // The firmware holds one hue/sat; keep the trio together.
            for k in COLOR_KEYS {
                if let Some(v) = get_setting(&self.settings, k) {
                    merge_setting(&mut self.chosen, k, v.clone());
                }
            }
        }
        // The keyboard runs the new value from here on, whatever fails next.
        let mut problems = Vec::new();
        if key == SettingKey::Effect {
            if let Err(e) = self.read_effect(ch) {
                problems.push(format!("applied, but not read back: {e:#}"));
            }
        }
        if let Err(e) = hako_core::config::save_device_settings(
            &self.paths,
            self.id.vendor_id,
            self.id.product_id,
            &self.chosen,
        ) {
            problems.push(format!("applied, but not saved: {e:#}"));
        }
        if self.saving() {
            if let Err(e) = self.save(ch) {
                problems.push(format!(
                    "applied, but not saved to the keyboard's memory: {e:#}"
                ));
            }
        }
        self.last_error = (!problems.is_empty()).then(|| problems.join("; "));
        match &self.last_error {
            Some(e) => bail!("{e}"),
            None => Ok(()),
        }
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if self.last_poll.elapsed() >= BATTERY_POLL {
            self.last_poll = Instant::now();
            self.battery = self.read_battery();
        }
        if self.detect_at.is_some_and(|t| Instant::now() >= t) {
            self.connect();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.link = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    /// QMK's raw HID report descriptor (`RawReport`, RAW_EPSIZE 32).
    const QMK_RAW: [u8; 34] = [
        0x06, 0x60, 0xff, 0x09, 0x61, 0xa1, 0x01, 0x09, 0x62, 0x15, 0x00, 0x26, 0xff, 0x00, 0x95,
        0x20, 0x75, 0x08, 0x81, 0x02, 0x09, 0x63, 0x15, 0x00, 0x26, 0xff, 0x00, 0x95, 0x20, 0x75,
        0x08, 0x91, 0x02, 0xc0,
    ];

    #[test]
    fn probe_recognises_only_the_raw_hid_collection() {
        assert!(is_raw_hid(&QMK_RAW));
        // Extended 4-byte usage form.
        let ext = [0x0b, 0x61, 0x00, 0x60, 0xff, 0xa1, 0x01, 0xc0];
        assert!(is_raw_hid(&ext));
        // Boot keyboard.
        assert!(!is_raw_hid(&[
            0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0xc0
        ]));
        // Same page, other usage (e.g. a vendor console).
        assert!(!is_raw_hid(&[
            0x06, 0x60, 0xff, 0x09, 0x74, 0xa1, 0x01, 0xc0
        ]));
        // Usage 0x61 on another vendor page.
        assert!(!is_raw_hid(&[
            0x06, 0x00, 0xff, 0x09, 0x61, 0xa1, 0x01, 0xc0
        ]));
        // 0x61 only as a usage inside a collection, not the application.
        assert!(!is_raw_hid(&[
            0x06, 0x60, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x61, 0x81, 0x02, 0xc0
        ]));
        // Logical (not application) collection.
        assert!(!is_raw_hid(&[
            0x06, 0x60, 0xff, 0x09, 0x61, 0xa1, 0x02, 0xc0
        ]));
        assert!(!is_raw_hid(&[]));
    }

    #[test]
    fn probe_claims_a_keyboard_by_its_name() {
        let dir = std::env::temp_dir().join(format!("hako-qmk-probe-{}", std::process::id()));
        let node = |sub: &str, desc: &[u8], name: &str| {
            let sys = dir.join(sub);
            std::fs::create_dir_all(&sys).unwrap();
            std::fs::write(sys.join("report_descriptor"), desc).unwrap();
            HidDeviceInfo {
                path: PathBuf::from(format!("/dev/{sub}")),
                vendor_id: 0x3434,
                product_id: 0x0123,
                interface: Some(1),
                uniq: String::new(),
                bus: crate::hid::BUS_USB,
                name: name.into(),
                sys,
            }
        };
        let kbd = node(
            "hidraw0",
            &[0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0xc0],
            " Keychron Q1 ",
        );
        let raw = node("hidraw1", &QMK_RAW, "Keychron Q1");
        let fb = fallback(&[&kbd, &raw]).expect("raw HID node claims the device");
        assert_eq!(fb.kind, DeviceKind::Keyboard);
        assert_eq!(fb.name, "Keychron Q1");
        assert!(fallback(&[&kbd]).is_none());
        let unnamed = node("hidraw2", &QMK_RAW, "");
        assert_eq!(
            fallback(&[&unnamed]).unwrap().name,
            "QMK keyboard 3434:0123"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Firmware double answering like QMK `via.c` for the channels it has.
    struct Fake {
        version: Option<u16>,
        /// Modern channel ids handled, or `None` for legacy firmware.
        channels: Option<Vec<u8>>,
        values: HashMap<Vec<u8>, Vec<u8>>,
        /// Highest effect the firmware was built with.
        effect_max: u8,
        /// Answers `id_custom_save` (a keyboard override may not).
        saves: bool,
        /// Commands (by index in `sent`) whose reply is lost.
        dropped: Vec<usize>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        replies: VecDeque<[u8; 32]>,
    }

    impl Fake {
        fn new(
            version: Option<u16>,
            channels: Option<Vec<u8>>,
        ) -> (Self, Arc<Mutex<Vec<Vec<u8>>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let mut values = HashMap::new();
            for ch in [1u8, 2, 3, 5] {
                values.insert(vec![ch, 1], vec![0xff]);
                values.insert(vec![ch, 2], vec![if ch == 1 { 0 } else { 4 }]);
                values.insert(vec![ch, 3], vec![0x80]);
                values.insert(vec![ch, 4], vec![0, 255]);
            }
            for (id, v) in [
                (0x80u8, vec![0xff]),
                (0x81, vec![1]),
                (0x82, vec![2]),
                (0x83, vec![85, 255]),
            ] {
                values.insert(vec![id], v);
            }
            let fake = Fake {
                version,
                channels,
                values,
                effect_max: 40,
                saves: true,
                dropped: vec![],
                sent: sent.clone(),
                replies: VecDeque::new(),
            };
            (fake, sent)
        }
    }

    impl Link for Fake {
        fn send(&mut self, report: &[u8]) -> Result<()> {
            assert_eq!(report.len(), 33);
            assert_eq!(report[0], 0, "report id");
            let mut b = [0u8; 32];
            b.copy_from_slice(&report[1..]);
            let n = {
                let mut sent = self.sent.lock().unwrap();
                sent.push(b.to_vec());
                sent.len() - 1
            };
            match (b[0], &self.channels) {
                (0x01, _) => match self.version {
                    Some(v) => b[1..3].copy_from_slice(&v.to_be_bytes()),
                    None => b[0] = 0xff,
                },
                (0x07 | 0x08, Some(chs)) if chs.contains(&b[1]) => {
                    let key = vec![b[1], b[2]];
                    if b[0] == 0x08 {
                        let v = self.values.get(&key).cloned().unwrap_or_default();
                        b[3..3 + v.len()].copy_from_slice(&v);
                    } else {
                        let mut v = b[3..5].to_vec();
                        if b[2] == 2 && b[1] != 1 && v[0] > self.effect_max {
                            v[0] = self.effect_max;
                        }
                        self.values.insert(key, v);
                    }
                }
                (0x07 | 0x08, None) => {
                    if b[0] == 0x08 {
                        let v = self.values.get(&vec![b[1]]).cloned().unwrap_or_default();
                        b[2..2 + v.len()].copy_from_slice(&v);
                    }
                }
                // id_custom_save echoes [09, channel]; id_lighting_save [09].
                (0x09, Some(chs)) if self.saves && chs.contains(&b[1]) => {}
                (0x09, None) if self.saves => {}
                _ => b[0] = 0xff,
            }
            if !self.dropped.contains(&n) {
                self.replies.push_back(b);
            }
            Ok(())
        }

        fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
            let r = self.replies.pop_front().context("timeout")?;
            buf[..32].copy_from_slice(&r);
            Ok(32)
        }
    }

    fn keyboard(fake: Fake, chosen: SettingsMap) -> QmkKeyboard {
        // One dir per keyboard: tests run in parallel and some remove theirs.
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hako-qmk-test-{}-{n}", std::process::id()));
        QmkKeyboard {
            id: DeviceId {
                vendor_id: 0xfeed,
                product_id: 0x0001,
                instance: "test".into(),
            },
            name: "Test".into(),
            link: Some(Box::new(fake)),
            protocol: None,
            channel: None,
            settings: SettingsMap::new(),
            chosen,
            paths: ConfigPaths {
                root: dir.clone(),
                config_toml: dir.join("config.toml"),
                devices_dir: dir.join("devices"),
                profiles_dir: dir.join("profiles"),
                remaps_dir: dir.join("remaps"),
            },
            last_error: None,
            hid_dirs: vec![],
            battery: None,
            last_poll: Instant::now(),
            detect_at: None,
            detect_tries: 0,
        }
    }

    /// Nothing that writes EEPROM / flash, keymaps or macros.
    fn assert_volatile(sent: &[Vec<u8>]) {
        for cmd in sent {
            assert!(
                matches!(cmd[0], 0x01 | 0x07 | 0x08),
                "non-volatile or keymap command sent: {cmd:02x?}"
            );
        }
    }

    fn assert_defaults_validate(kb: &QmkKeyboard) {
        let snap = kb.snapshot();
        assert!(!snap.controls.is_empty());
        for c in &snap.controls {
            let v = snap
                .settings
                .get(c.key.as_str())
                .expect("value for every control");
            c.validate(v).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    #[test]
    fn detects_rgb_matrix_first_and_reads_its_state() {
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![2, 3]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        assert_eq!(kb.last_error, None);
        assert_eq!(kb.protocol, Some(0x000c));
        assert_eq!(kb.channel, Some(Channel::RgbMatrix));
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent[0][..2], [0x01, 0x00]);
        // First probe is RGB Matrix brightness: 08 03 01.
        assert_eq!(sent[1][..3], [0x08, 0x03, 0x01]);
        assert_volatile(&sent);
        assert_eq!(int(&kb.settings, SettingKey::Brightness), Some(100));
        assert_eq!(int(&kb.settings, SettingKey::Effect), Some(4));
        assert_eq!(int(&kb.settings, SettingKey::Speed), Some(128));
        assert_eq!(rgb(&kb.settings), [255, 0, 0]);
        assert_defaults_validate(&kb);
    }

    #[test]
    fn every_channel_has_valid_defaults() {
        for (chs, want) in [
            (vec![2], Channel::Rgblight),
            (vec![5], Channel::LedMatrix),
            (vec![1], Channel::Backlight),
            (vec![1, 2], Channel::Rgblight),
        ] {
            let (fake, sent) = Fake::new(Some(0x000d), Some(chs));
            let mut kb = keyboard(fake, SettingsMap::new());
            kb.connect();
            assert_eq!(kb.channel, Some(want));
            assert_defaults_validate(&kb);
            assert_volatile(&sent.lock().unwrap());
        }
        let backlight = controls(Channel::Backlight, &SettingsMap::new());
        assert!(backlight
            .iter()
            .all(|c| !matches!(c.key, SettingKey::Speed | SettingKey::ColorR)));
        assert_eq!(
            backlight[0].options,
            vec![
                SettingOption::int(0, "Steady"),
                SettingOption::int(1, "Breathing")
            ]
        );
    }

    #[test]
    fn no_via_or_no_lighting_means_no_controls() {
        let (fake, _) = Fake::new(None, Some(vec![3]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        assert_eq!((kb.protocol, kb.channel), (None, None));
        assert!(kb.snapshot().controls.is_empty());
        assert!(kb.snapshot().connected);

        let (fake, _) = Fake::new(Some(0x000c), Some(vec![]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        assert_eq!(kb.channel, None);
        assert_eq!(kb.last_error, None);
    }

    #[test]
    fn legacy_firmware_uses_lighting_ids() {
        let (fake, sent) = Fake::new(Some(0x000a), None);
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        assert_eq!(kb.channel, Some(Channel::Legacy));
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent[1][..2], [0x08, 0x81]);
        assert_eq!(int(&kb.settings, SettingKey::Speed), Some(2));
        assert_eq!(rgb(&kb.settings), via::hs_to_rgb([85, 255]));
        assert_defaults_validate(&kb);
        let speed = kb
            .snapshot()
            .controls
            .into_iter()
            .find(|c| c.key == SettingKey::Speed)
            .unwrap();
        assert_eq!(speed.max, Some(3.0));
    }

    #[test]
    fn saved_values_are_reapplied_on_connect() {
        let mut chosen = SettingsMap::new();
        merge_setting(&mut chosen, SettingKey::Effect, SettingValue::Int(7));
        merge_setting(&mut chosen, SettingKey::Brightness, SettingValue::Int(50));
        merge_setting(&mut chosen, SettingKey::ColorB, SettingValue::Int(255));
        merge_setting(&mut chosen, SettingKey::ColorR, SettingValue::Int(0));
        merge_setting(&mut chosen, SettingKey::ColorG, SettingValue::Int(0));
        // Out of range for this channel: dropped, not sent.
        merge_setting(&mut chosen, SettingKey::Speed, SettingValue::Int(999));
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        let mut kb = keyboard(fake, chosen);
        kb.connect();
        assert_eq!(kb.last_error, None);
        let sets: Vec<Vec<u8>> = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x07)
            .map(|c| c[..5].to_vec())
            .collect();
        assert_eq!(
            sets,
            vec![
                vec![0x07, 0x03, 0x02, 7, 0],
                vec![0x07, 0x03, 0x01, 128, 0],
                vec![0x07, 0x03, 0x04, 171, 255],
            ]
        );
        assert_eq!(int(&kb.settings, SettingKey::Speed), Some(128));
        assert_eq!(rgb(&kb.settings), [0, 0, 255]);
    }

    #[test]
    fn effect_is_read_back_after_the_firmware_clamps_it() {
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        kb.set(SettingKey::Effect, SettingValue::Int(200)).unwrap();
        assert_eq!(int(&kb.settings, SettingKey::Effect), Some(40));
        kb.set(SettingKey::Effect, SettingValue::Int(0)).unwrap();
        let snap = kb.snapshot();
        assert!(snap
            .controls
            .iter()
            .filter(|c| c.key == SettingKey::Speed || COLOR_KEYS.contains(&c.key))
            .all(|c| c.hidden));
        assert!(snap
            .controls
            .iter()
            .filter(|c| c.hidden)
            .all(|c| !matches!(c.key, SettingKey::Effect | SettingKey::SaveToDevice)));
        kb.set(SettingKey::ColorG, SettingValue::Int(0)).unwrap();
        assert_eq!(int(&kb.chosen, SettingKey::ColorR), Some(255));
        assert_volatile(&sent.lock().unwrap());
        let _ = std::fs::remove_dir_all(&kb.paths.root);
    }

    fn saves(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> Vec<Vec<u8>> {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x09)
            .map(|c| c[..3].to_vec())
            .collect()
    }

    #[test]
    fn save_mode_stores_each_change_only_while_on() {
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        let save = kb
            .snapshot()
            .controls
            .into_iter()
            .find(|c| c.key == SettingKey::SaveToDevice)
            .unwrap();
        assert!(save.persistent && save.blocked.is_none());
        assert_eq!(
            get_setting(&kb.settings, SettingKey::SaveToDevice),
            Some(&SettingValue::Bool(false))
        );

        kb.set(SettingKey::Brightness, SettingValue::Int(50))
            .unwrap();
        assert!(saves(&sent).is_empty(), "volatile while off");

        // Switching on stores the current state once: 09 03 (RGB Matrix).
        kb.set(SettingKey::SaveToDevice, SettingValue::Bool(true))
            .unwrap();
        assert_eq!(saves(&sent), [vec![0x09, 0x03, 0x00]]);
        sent.lock().unwrap().clear();

        // Each change: the volatile set, then the save.
        kb.set(SettingKey::Brightness, SettingValue::Int(100))
            .unwrap();
        let tail: Vec<Vec<u8>> = sent
            .lock()
            .unwrap()
            .iter()
            .map(|c| c[..4].to_vec())
            .collect();
        assert_eq!(
            tail,
            [vec![0x07, 0x03, 0x01, 0xff], vec![0x09, 0x03, 0x00, 0x00]]
        );

        // Off: no device write at all, and later changes stay volatile.
        sent.lock().unwrap().clear();
        kb.set(SettingKey::SaveToDevice, SettingValue::Bool(false))
            .unwrap();
        assert!(sent.lock().unwrap().is_empty());
        kb.set(SettingKey::Speed, SettingValue::Int(9)).unwrap();
        assert!(saves(&sent).is_empty());
    }

    #[test]
    fn save_mode_never_writes_on_connect() {
        let mut chosen = SettingsMap::new();
        merge_setting(
            &mut chosen,
            SettingKey::SaveToDevice,
            SettingValue::Bool(true),
        );
        merge_setting(&mut chosen, SettingKey::Brightness, SettingValue::Int(50));
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![2]));
        let mut kb = keyboard(fake, chosen);
        kb.connect();
        assert_eq!(kb.last_error, None);
        assert_eq!(
            get_setting(&kb.settings, SettingKey::SaveToDevice),
            Some(&SettingValue::Bool(true))
        );
        assert_volatile(&sent.lock().unwrap());
        // The re-applied brightness was sent, volatile.
        assert!(sent
            .lock()
            .unwrap()
            .iter()
            .any(|c| c[..4] == [0x07, 0x02, 0x01, 128]));
        // Rgblight channel id 2 on the next user change.
        kb.set(SettingKey::Effect, SettingValue::Int(3)).unwrap();
        assert_eq!(saves(&sent), [vec![0x09, 0x02, 0x00]]);
    }

    #[test]
    fn legacy_save_is_the_bare_lighting_save() {
        let (fake, sent) = Fake::new(Some(0x0009), None);
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        kb.set(SettingKey::SaveToDevice, SettingValue::Bool(true))
            .unwrap();
        assert_eq!(saves(&sent), [vec![0x09, 0x00, 0x00]]);
    }

    #[test]
    fn unanswered_save_is_an_error_and_stays_off() {
        let (mut fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        fake.saves = false;
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        assert!(kb
            .set(SettingKey::SaveToDevice, SettingValue::Bool(true))
            .is_err());
        assert_eq!(
            get_setting(&kb.settings, SettingKey::SaveToDevice),
            Some(&SettingValue::Bool(false))
        );
        assert_eq!(saves(&sent).len(), 1);

        // Saved mode on from an older firmware: the change applies, the save errors.
        merge_setting(
            &mut kb.chosen,
            SettingKey::SaveToDevice,
            SettingValue::Bool(true),
        );
        let err = kb
            .set(SettingKey::Brightness, SettingValue::Int(10))
            .unwrap_err();
        assert!(format!("{err:#}").contains("not saved"), "{err:#}");
        assert_eq!(int(&kb.settings, SettingKey::Brightness), Some(10));
    }

    #[test]
    fn backlight_breathing_is_persistent_and_never_reapplied() {
        // via.c backlight effect -> backlight_enable_breathing -> eeconfig_update_backlight.
        let effect = &controls(Channel::Backlight, &SettingsMap::new())[0];
        assert_eq!(effect.key, SettingKey::Effect);
        assert!(effect.persistent);
        assert!(!controls(Channel::RgbMatrix, &SettingsMap::new())[0].persistent);

        let mut chosen = SettingsMap::new();
        merge_setting(&mut chosen, SettingKey::Effect, SettingValue::Int(1));
        merge_setting(&mut chosen, SettingKey::Brightness, SettingValue::Int(50));
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![1]));
        let mut kb = keyboard(fake, chosen);
        kb.connect();
        let sets: Vec<Vec<u8>> = sent
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x07)
            .map(|c| c[..3].to_vec())
            .collect();
        assert_eq!(sets, [vec![0x07, 0x01, 0x01]], "brightness only");
        assert_eq!(
            int(&kb.settings, SettingKey::Effect),
            Some(0),
            "device's own value"
        );
    }

    #[test]
    fn silent_raw_hid_is_no_via_after_a_few_tries() {
        let (mut fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        fake.dropped = (0..10).collect();
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        for _ in 0..10 {
            assert_eq!(kb.last_error, None, "no reply is not an error");
            if kb.detect_at.is_none() {
                break;
            }
            kb.detect_at = Some(Instant::now());
            kb.tick().unwrap();
        }
        assert_eq!(kb.detect_at, None);
        assert_eq!(sent.lock().unwrap().len(), SILENT_TRIES as usize);
        assert!(kb.snapshot().controls.is_empty());
        let e = kb.set(SettingKey::Brightness, SettingValue::Int(5));
        assert!(e.unwrap_err().to_string().contains("does not speak VIA"));
    }

    #[test]
    fn a_lost_reply_during_detection_is_retried() {
        let mut chosen = SettingsMap::new();
        merge_setting(&mut chosen, SettingKey::Brightness, SettingValue::Int(50));
        let (mut fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        // Version, channel probe, then the state read's brightness: lost.
        fake.dropped = vec![2];
        let mut kb = keyboard(fake, chosen);
        kb.connect();
        assert!(kb.last_error.as_deref().unwrap().contains("no VIA reply"));
        let at = kb.detect_at.expect("retry scheduled");
        assert!(at > Instant::now());
        // Not due yet: nothing sent.
        let n = sent.lock().unwrap().len();
        kb.tick().unwrap();
        assert_eq!(sent.lock().unwrap().len(), n);
        kb.detect_at = Some(Instant::now());
        kb.tick().unwrap();
        assert_eq!((kb.last_error.clone(), kb.detect_at), (None, None));
        assert!(sent.lock().unwrap()[n..]
            .iter()
            .any(|c| c[..4] == [0x07, 0x03, 0x01, 128]));
        assert_eq!(int(&kb.settings, SettingKey::Brightness), Some(50));
    }

    #[test]
    fn effect_written_but_not_read_back_is_kept() {
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        assert_eq!(int(&kb.settings, SettingKey::Effect), Some(4));
        let n = sent.lock().unwrap().len();
        // The put (n) is answered, its read-back (n + 1) is not.
        let mut fake = Fake::new(Some(0x000c), Some(vec![3])).0;
        fake.dropped = vec![n + 1];
        fake.sent = sent.clone();
        kb.link = Some(Box::new(fake));
        let e = kb
            .set(SettingKey::Effect, SettingValue::Int(7))
            .unwrap_err();
        assert!(e.to_string().contains("not read back"), "{e}");
        assert_eq!(int(&kb.settings, SettingKey::Effect), Some(7));
        assert_eq!(int(&kb.chosen, SettingKey::Effect), Some(7));
        assert!(kb.last_error.is_some());
        let _ = std::fs::remove_dir_all(&kb.paths.root);
    }

    #[test]
    fn unsaved_file_still_saves_to_the_keyboard() {
        let mut chosen = SettingsMap::new();
        merge_setting(
            &mut chosen,
            SettingKey::SaveToDevice,
            SettingValue::Bool(true),
        );
        let (fake, sent) = Fake::new(Some(0x000c), Some(vec![3]));
        let mut kb = keyboard(fake, chosen);
        kb.connect();
        // The settings folder cannot be created: a file is in the way.
        std::fs::create_dir_all(&kb.paths.root).unwrap();
        let blocker = kb.paths.root.join("blocker");
        std::fs::write(&blocker, "").unwrap();
        kb.paths.devices_dir = blocker.join("devices");
        let e = kb
            .set(SettingKey::Brightness, SettingValue::Int(10))
            .unwrap_err();
        assert!(e.to_string().contains("applied, but not saved"), "{e}");
        assert_eq!(int(&kb.settings, SettingKey::Brightness), Some(10));
        assert_eq!(int(&kb.chosen, SettingKey::Brightness), Some(10));
        assert_eq!(saves(&sent), [vec![0x09, 0x03, 0x00]]);
        let _ = std::fs::remove_dir_all(&kb.paths.root);
    }

    #[test]
    fn leaving_save_mode_without_a_channel_writes_nothing() {
        let (fake, sent) = Fake::new(None, Some(vec![3]));
        let mut kb = keyboard(fake, SettingsMap::new());
        kb.connect();
        sent.lock().unwrap().clear();
        kb.set(SettingKey::SaveToDevice, SettingValue::Bool(false))
            .unwrap();
        assert!(sent.lock().unwrap().is_empty());
    }
}
