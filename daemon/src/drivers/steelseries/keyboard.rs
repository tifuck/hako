//! Apex keyboards: whole-keyboard color and brightness.
//!
//! Byte layouts from OpenRGB `SteelSeriesApexController.cpp`,
//! `SteelSeriesApexTZoneController.cpp` and `SteelSeriesApex8ZoneController.cpp`,
//! reimplemented:
//! - Per-key boards (Apex 5/7/9/Pro): "direct" feature report
//!   `00 <id> <count> (<key> r g b)*`, id `3a` (gen 1), `40` (gen 2/3 wired) or
//!   `61` (gen 3 wireless). Gen 3 first takes an init feature report `00 4b`.
//!   Direct mode is volatile: the board falls back to its onboard lighting on
//!   power-up, so we re-send on open. Brightness is applied host-side (the
//!   board's `23` brightness only scales onboard effects).
//! - Apex 3: `00 0a 00 <0-100>` then `00 0b 00 (r g b)*10`, 33-byte output reports.
//! - Apex 3 TKL: `00 21 ff (r g b)*8` and `00 23 <0-16>`, 65-byte output reports.
//! - Apex M750 (`SteelSeriesApexMController.cpp`): 513-byte feature reports;
//!   LED control is enabled with `00 00 00 00 01 00 85`, `00 00 00 00 03 01
//!   00 ff`, `00 00 00 00 01 00 85`, then `00 00 00 01 8e 01 03 06 16` +
//!   132 (6 x 22) r g b slots, unused slots `ff 32 00`.
//! - Apex (OG) / 350 (`SteelSeriesOldApexController.cpp`): output report
//!   `00 07 00 (r g b 08)*5` (5 zones, mode 0x08 = direct), 33 bytes.
//!
//! Apex Pro (Gen 3, `1038:1640`) actuation point, from apex-analog
//! (github.com/milescoviello/apex-analog, MIT, `apex_analog.py`): 65-byte
//! output report `00 31 <n> (<hid usage> <press> <release>)*n`, up to 20
//! keys each, on the node whose report descriptor starts `06 c0 ff`. It only
//! changes RAM and was read from firmware 4.16.8 (bcdDevice `0416`), so the
//! control is offered on that firmware only and re-sent on open. Rapid tap,
//! same source, node and gate: `00 32 <n> (<hid usage> <mode>)*n`, up to 30
//! keys each, mode 2 ("the value the keyboard's own RAPID TAP menu sets") or
//! 0 (off); also RAM only.
//!
//! Onboard-profile selection, the Apex 3 save command (OpenRGB
//! `SteelSeriesApexTZoneController::Save`: `00 06 00 08` then `00 09`, the
//! only source, its own docs mark saving unsupported) and the Apex Pro
//! `09 <slot>` flash save (apex-analog only; overwrites one of five configs
//! and the firmware does no range check) are never sent: both are shown as
//! blocked "Save changes to device memory" controls.

use super::{instance, int, pad, request, VID};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{enumerate_hidraw, find_hidraw, HidDeviceInfo, HidrawDevice};
use anyhow::{anyhow, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Gen {
    One,
    Two,
    Three,
}

#[derive(Debug, Clone, Copy)]
pub enum KbProto {
    /// Per-key direct mode; `len` is the feature report size incl. report id.
    /// `fw_gen3`: firmware 1.19.7+ switches the board to gen 3.
    PerKey { len: usize, gen: Gen, fw_gen3: bool },
    /// Apex 3: 10 zones.
    TenZone,
    /// Apex 3 TKL: 8 zones.
    EightZone,
    /// Apex M750: 132 key slots.
    ApexM,
    /// Apex (OG) / 350: 5 zones.
    OldApex,
}

pub struct KeyboardSpec {
    pub interface: u32,
    /// 2.4 GHz dongle PIDs: interface 3, gen 3, direct id `61`.
    pub wireless: &'static [u16],
    pub proto: KbProto,
    /// OmniPoint actuation point (Apex Pro Gen 3, see module docs).
    pub actuation: bool,
}

pub const APEX_LEN: usize = 643;
pub const APEX9_LEN: usize = 513;

/// HID usages lit in direct mode (OpenRGB's key list, as a set).
fn keys() -> impl Iterator<Item = u8> {
    (0x04..=0x64)
        .chain(0x87..=0x8b)
        .chain(0xe0..=0xe7)
        .chain([0xf0, 0xfb])
}

fn direct_id(gen: Gen, wireless: bool) -> u8 {
    match gen {
        Gen::One => 0x3a,
        _ if wireless => 0x61,
        _ => 0x40,
    }
}

/// Direct-mode feature report painting every key `rgb`.
fn per_key_report(len: usize, id: u8, [r, g, b]: [u8; 3]) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    buf[1] = id;
    let n = keys().take((len - 3) / 4).fold(0, |i, key| {
        buf[3 + 4 * i..7 + 4 * i].copy_from_slice(&[key, r, g, b]);
        i + 1
    });
    buf[2] = n as u8;
    buf
}

fn ten_zone_reports(pct: u8, rgb: [u8; 3]) -> [Vec<u8>; 2] {
    let mut colors = vec![0x00, 0x0b, 0x00];
    colors.extend(rgb.repeat(10));
    [pad(&[0x00, 0x0a, 0x00, pct.min(100)], 33), pad(&colors, 33)]
}

fn eight_zone_reports(pct: u8, rgb: [u8; 3]) -> [Vec<u8>; 2] {
    let mut colors = vec![0x00, 0x21, 0xff];
    colors.extend(rgb.repeat(8));
    let level = (u16::from(pct.min(100)) * 16 + 50) / 100;
    [pad(&colors, 65), pad(&[0x00, 0x23, level as u8], 65)]
}

const APEX_M_LEN: usize = 513;
/// Apex M750 slots with no key (OpenRGB's `NA` entries in its 6 x 22 map).
const APEX_M_EMPTY: [usize; 28] = [
    3, 5, 6, 7, 8, 13, 14, 18, 20, 33, 35, 36, 37, 39, 56, 58, 59, 60, 61, 65, 79, 101, 115, 120,
    128, 129, 130, 131,
];

fn apex_m_init() -> [Vec<u8>; 3] {
    let enable = pad(&[0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x85], APEX_M_LEN);
    let direct = pad(
        &[0x00, 0x00, 0x00, 0x00, 0x03, 0x01, 0x00, 0xff],
        APEX_M_LEN,
    );
    [enable.clone(), direct, enable]
}

fn apex_m_report(rgb: [u8; 3]) -> Vec<u8> {
    let mut b = pad(
        &[0x00, 0x00, 0x00, 0x01, 0x8e, 0x01, 0x03, 0x06, 0x16],
        APEX_M_LEN,
    );
    for i in 0..132 {
        let slot = if APEX_M_EMPTY.contains(&i) {
            [0xff, 0x32, 0x00]
        } else {
            rgb
        };
        b[9 + 3 * i..12 + 3 * i].copy_from_slice(&slot);
    }
    b
}

fn old_apex_report([r, g, b]: [u8; 3]) -> Vec<u8> {
    let mut v = vec![0x00, 0x07, 0x00];
    for _ in 0..5 {
        v.extend_from_slice(&[r, g, b, 0x08]);
    }
    pad(&v, 33)
}

/// Actuation depths the Apex Pro firmware offers, in tenths of a mm
/// (apex-analog `LEVEL_TENTHS`).
const ACTUATION_TENTHS: [u8; 20] = [
    1, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 37, 40,
];
/// Press / release thresholds those levels pick from (`LEVEL_THRESHOLDS`).
const ACTUATION_THRESHOLDS: [u8; 40] = [
    5, 5, 4, 5, 6, 8, 10, 12, 14, 16, 18, 21, 23, 26, 28, 31, 34, 38, 42, 46, 50, 55, 60, 65, 71,
    77, 84, 91, 99, 108, 118, 128, 139, 151, 165, 180, 201, 206, 210, 215,
];
/// HID usages of the sensor keys every layout has (apex-analog `CHANNELS`
/// without LGUI, which this command ignores, and the ISO / JIS-only keys).
const ACTUATION_KEYS: [u8; 60] = [
    0x35, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2d, 0x2e, 0x2b, 0x14, 0x1a,
    0x08, 0x15, 0x17, 0x1c, 0x18, 0x0c, 0x12, 0x13, 0x2f, 0x30, 0x31, 0x39, 0x04, 0x16, 0x07, 0x09,
    0x0a, 0x0b, 0x0d, 0x0e, 0x0f, 0x33, 0x34, 0x28, 0xe1, 0x1d, 0x1b, 0x06, 0x19, 0x05, 0x11, 0x10,
    0x36, 0x37, 0x38, 0xe5, 0xe0, 0xe2, 0x2c, 0xe6, 0xe7, 0xf0, 0xe4, 0x2a,
];
/// The only firmware the actuation command was read from.
const ACTUATION_FIRMWARE: &str = "0416";
/// apex-analog `RAPID_TAP_MODE`.
const RAPID_TAP_MODE: u8 = 2;

/// (press, release) thresholds of the level nearest `tenths` mm.
fn actuation_points(tenths: u8) -> (u8, u8) {
    let level = (0..ACTUATION_TENTHS.len())
        .min_by_key(|&i| ACTUATION_TENTHS[i].abs_diff(tenths))
        .unwrap_or(0);
    let t = &ACTUATION_THRESHOLDS;
    match level {
        0 => (t[1], t[1]),
        19 => (t[39], t[38]),
        l => (t[2 * l + 2], t[2 * l + 1]),
    }
}

/// Reports setting every key in [`ACTUATION_KEYS`] to `tenths` mm.
fn actuation_reports(tenths: u8) -> Vec<Vec<u8>> {
    let (press, release) = actuation_points(tenths);
    ACTUATION_KEYS
        .chunks(20)
        .map(|keys| {
            let mut v = vec![0x00, 0x31, keys.len() as u8];
            for &hid in keys {
                v.extend_from_slice(&[hid, press, release]);
            }
            pad(&v, 65)
        })
        .collect()
}

/// Reports turning rapid tap on or off for every key in [`ACTUATION_KEYS`].
fn rapid_tap_reports(on: bool) -> Vec<Vec<u8>> {
    let mode = if on { RAPID_TAP_MODE } else { 0 };
    ACTUATION_KEYS
        .chunks(30)
        .map(|keys| {
            let mut v = vec![0x00, 0x32, keys.len() as u8];
            for &hid in keys {
                v.extend_from_slice(&[hid, mode]);
            }
            pad(&v, 65)
        })
        .collect()
}

/// The Apex Pro's `06 c0 ff` node, if it runs the firmware the actuation
/// command was read from. Sysfs only.
fn actuation_node(pid: u16) -> Option<HidDeviceInfo> {
    let node = enumerate_hidraw().ok()?.into_iter().find(|d| {
        d.vendor_id == VID
            && d.product_id == pid
            && d.report_descriptor().starts_with(&[0x06, 0xc0, 0xff])
    })?;
    let firmware = node
        .sys
        .ancestors()
        .find_map(|p| std::fs::read_to_string(p.join("bcdDevice")).ok())?;
    (firmware.trim() == ACTUATION_FIRMWARE).then_some(node)
}

fn scale([r, g, b]: [u8; 3], pct: u8) -> [u8; 3] {
    let s = |c: u8| (u16::from(c) * u16::from(pct.min(100)) / 100) as u8;
    [s(r), s(g), s(b)]
}

/// `major.minor.patch` from the firmware reply (ASCII, maybe prefixed).
fn parse_version(reply: &[u8]) -> Option<(u32, u32, u32)> {
    let text = String::from_utf8_lossy(reply);
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut it = word.split('.').map(|n| n.parse().ok());
            Some((it.next()??, it.next()??, it.next()??))
        })
}

/// Lighting keys and the values unset ones get (Razer's and Corsair's default
/// colour): every lighting write sends the colour.
const LIGHT_DEFAULTS: [(SettingKey, i64); 4] = [
    (SettingKey::Brightness, 100),
    (SettingKey::ColorR, 0),
    (SettingKey::ColorG, 120),
    (SettingKey::ColorB, 255),
];

/// Fill unset lighting keys in `s` from [`LIGHT_DEFAULTS`].
fn light_defaults(s: &mut SettingsMap) {
    for (k, v) in LIGHT_DEFAULTS {
        if get_setting(s, k).is_none() {
            merge_setting(s, k, SettingValue::Int(v));
        }
    }
}

/// Why the model's flash save is not offered, if it has a known one.
pub(super) fn save_blocked(spec: &KeyboardSpec) -> Option<&'static str> {
    if spec.actuation {
        Some("only apex-analog sends 09 <slot>; it overwrites a saved config unchecked")
    } else if matches!(spec.proto, KbProto::TenZone) {
        Some("only OpenRGB sends the Apex 3 save, and it marks saving unsupported")
    } else {
        None
    }
}

pub(super) fn controls(actuation: bool, save_blocked: Option<&str>) -> Vec<SettingMeta> {
    let mut c = vec![
        SettingMeta::slider(SettingKey::Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"),
        SettingMeta::color(SettingKey::ColorR, "Red"),
        SettingMeta::color(SettingKey::ColorG, "Green"),
        SettingMeta::color(SettingKey::ColorB, "Blue"),
    ];
    if actuation {
        c.push(SettingMeta::dropdown(
            SettingKey::ActuationPoint,
            "Actuation point",
            ACTUATION_TENTHS
                .iter()
                .map(|&t| SettingOption::int(t.into(), &format!("{}.{} mm", t / 10, t % 10)))
                .collect(),
        ));
        c.push(SettingMeta::toggle(SettingKey::RapidTap, "Rapid tap"));
    }
    if let Some(reason) = save_blocked {
        c.push(
            SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory")
                .persistent()
                .blocked(reason),
        );
    }
    c
}

pub(super) fn open(
    pid: u16,
    name: &'static str,
    spec: &'static KeyboardSpec,
    ctx: &DriverCtx,
) -> Opened {
    Ok(vec![Box::new(KeyboardAdapter::try_open(
        pid,
        name,
        spec,
        ctx.paths.clone(),
    )?)])
}

pub struct KeyboardAdapter {
    id: DeviceId,
    name: &'static str,
    spec: &'static KeyboardSpec,
    wireless: bool,
    gen: Gen,
    initialized: bool,
    dev: Option<HidrawDevice>,
    /// The Apex Pro's actuation node, on supported firmware.
    actuation: Option<HidrawDevice>,
    settings: SettingsMap,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl KeyboardAdapter {
    fn try_open(
        pid: u16,
        name: &'static str,
        spec: &'static KeyboardSpec,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let wireless = spec.wireless.contains(&pid);
        let info = find_hidraw(VID, pid, Some(if wireless { 3 } else { spec.interface }))?;
        let mut dev = HidrawDevice::open(&info.path)?;
        let gen = match spec.proto {
            _ if wireless => Gen::Three,
            KbProto::PerKey {
                gen, fw_gen3: true, ..
            } => {
                let v = request(
                    &mut dev,
                    &pad(&[0x00, 0x90], 65),
                    |_| true,
                    Duration::from_millis(100),
                )?
                .and_then(|r| parse_version(r.strip_prefix(&[0x90]).unwrap_or(&r)));
                if v >= Some((1, 19, 7)) {
                    Gen::Three
                } else {
                    gen
                }
            }
            KbProto::PerKey { gen, .. } => gen,
            _ => Gen::One,
        };
        let mut settings = hako_core::config::load_device_settings(&paths, VID, pid)?;
        let actuation = spec
            .actuation
            .then(|| actuation_node(pid))
            .flatten()
            .and_then(|n| HidrawDevice::open(&n.path).ok());
        crate::drivers::sanitize_saved(
            &mut settings,
            &controls(actuation.is_some(), save_blocked(spec)),
        );
        let mut a = Self {
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: instance(&info),
            },
            name,
            spec,
            wireless,
            gen,
            initialized: false,
            dev: Some(dev),
            actuation,
            settings,
            last_error: None,
            paths,
        };
        // Leave onboard lighting and actuation alone until the user sets them.
        let mut restore = Vec::new();
        if LIGHT_DEFAULTS
            .iter()
            .any(|&(k, _)| get_setting(&a.settings, k).is_some())
        {
            light_defaults(&mut a.settings);
            restore.push(SettingKey::Brightness);
        }
        let s = a.settings.clone();
        for key in [SettingKey::ActuationPoint, SettingKey::RapidTap] {
            if a.actuation.is_some() && get_setting(&s, key).is_some() {
                restore.push(key);
            }
        }
        for key in restore {
            if let Err(e) = a.apply(&s, key) {
                warn!("{name} restore {}: {e}", key.as_str());
                a.last_error = Some(e.to_string());
            }
        }
        info!(
            "SteelSeries {name} open {} gen={:?}",
            info.path.display(),
            a.gen
        );
        Ok(a)
    }

    /// Put `key` from `s` on the board; lighting keys resend all lighting.
    fn apply(&mut self, s: &SettingsMap, key: SettingKey) -> Result<()> {
        if key == SettingKey::ActuationPoint || key == SettingKey::RapidTap {
            let reports = if key == SettingKey::RapidTap {
                let on = get_setting(s, key)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                rapid_tap_reports(on)
            } else {
                actuation_reports(int(s, key).unwrap_or(18).clamp(1, 40) as u8)
            };
            let dev = self.actuation.as_mut().context("actuation not supported")?;
            for r in reports {
                dev.write(&r)?;
                // A command arriving straight after another can replace it.
                std::thread::sleep(Duration::from_millis(5));
            }
            return Ok(());
        }
        let ch = |k| int(s, k).unwrap_or(0).clamp(0, 255) as u8;
        let rgb = [
            ch(SettingKey::ColorR),
            ch(SettingKey::ColorG),
            ch(SettingKey::ColorB),
        ];
        let pct = int(s, SettingKey::Brightness).unwrap_or(100).clamp(0, 100) as u8;
        let (gen, wireless, init) = (self.gen, self.wireless, !self.initialized);
        let dev = self.dev.as_mut().context("no device")?;
        match self.spec.proto {
            KbProto::PerKey { len, .. } => {
                if gen == Gen::Three && init {
                    dev.set_feature(&mut pad(&[0x00, 0x4b], 65))?;
                }
                dev.set_feature(&mut per_key_report(
                    len,
                    direct_id(gen, wireless),
                    scale(rgb, pct),
                ))?;
            }
            KbProto::TenZone => ten_zone_reports(pct, rgb)
                .iter()
                .try_for_each(|r| dev.write(r))?,
            KbProto::EightZone => eight_zone_reports(pct, rgb)
                .iter()
                .try_for_each(|r| dev.write(r))?,
            KbProto::ApexM => {
                if init {
                    for mut r in apex_m_init() {
                        dev.set_feature(&mut r)?;
                    }
                }
                dev.set_feature(&mut apex_m_report(scale(rgb, pct)))?;
            }
            KbProto::OldApex => dev.write(&old_apex_report(scale(rgb, pct)))?,
        }
        self.initialized = true;
        Ok(())
    }
}

impl DeviceAdapter for KeyboardAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Keyboard,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: controls(self.actuation.is_some(), save_blocked(self.spec)),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let mut proposed = self.settings.clone();
        merge_setting(&mut proposed, key, value);
        if LIGHT_DEFAULTS.iter().any(|&(k, _)| k == key) {
            light_defaults(&mut proposed);
        }
        if let Err(e) = self.apply(&proposed, key) {
            // Actuation (3 reports), rapid tap (2) and zone lighting (2) may
            // stop midway: send the old value back when there is one.
            if get_setting(&self.settings, key).is_some() {
                let old = self.settings.clone();
                if let Err(re) = self.apply(&old, key) {
                    return Err(anyhow!(
                        "{e:#}; restoring the previous value also failed ({re:#}), the keyboard may hold part of the change"
                    ));
                }
            }
            return Err(e);
        }
        // The board has the value: keep it even if the file cannot be written.
        self.settings = proposed;
        self.last_error = None;
        hako_core::config::save_device_settings(
            &self.paths,
            VID,
            self.id.product_id,
            &self.settings,
        )
        .map_err(|e| anyhow!("applied, but not saved: {e:#}"))
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_key_report_layout() {
        assert_eq!(keys().count(), 112);
        let r = per_key_report(APEX_LEN, direct_id(Gen::One, false), [1, 2, 3]);
        assert_eq!(r.len(), 643);
        assert_eq!(&r[..11], &[0x00, 0x3a, 112, 0x04, 1, 2, 3, 0x05, 1, 2, 3]);
        assert_eq!(&r[3 + 4 * 111..3 + 4 * 112], &[0xfb, 1, 2, 3]);
        assert!(r[3 + 4 * 112..].iter().all(|&b| b == 0));
        // Apex 9: 513 bytes hold all 112 keys; gen 2/3 ids.
        let r = per_key_report(APEX9_LEN, direct_id(Gen::Two, false), [9, 9, 9]);
        assert_eq!((r.len(), r[1], r[2]), (513, 0x40, 112));
        assert_eq!(direct_id(Gen::Three, true), 0x61);
        assert_eq!(scale([200, 100, 0], 50), [100, 50, 0]);
    }

    #[test]
    fn zone_reports() {
        let [br, col] = ten_zone_reports(80, [1, 2, 3]);
        assert_eq!((br.len(), &br[..4]), (33, &[0x00, 0x0a, 0x00, 80][..]));
        assert_eq!(col.len(), 33);
        assert_eq!(&col[..6], &[0x00, 0x0b, 0x00, 1, 2, 3]);
        assert_eq!(&col[30..33], &[1, 2, 3]);
        let [col, br] = eight_zone_reports(100, [4, 5, 6]);
        assert_eq!(
            (col.len(), &col[..6]),
            (65, &[0x00, 0x21, 0xff, 4, 5, 6][..])
        );
        assert_eq!(&col[24..27], &[4, 5, 6]);
        assert_eq!(col[27], 0);
        assert_eq!(&br[..3], &[0x00, 0x23, 16]);
        assert_eq!(eight_zone_reports(50, [0; 3])[1][2], 8);
    }

    #[test]
    fn apex_m750_and_old_apex_reports() {
        let init = apex_m_init();
        assert!(init.iter().all(|r| r.len() == 513));
        assert_eq!(&init[0][..7], &[0, 0, 0, 0, 0x01, 0x00, 0x85]);
        assert_eq!(&init[1][..8], &[0, 0, 0, 0, 0x03, 0x01, 0x00, 0xff]);
        assert_eq!(init[2], init[0]);
        let r = apex_m_report([1, 2, 3]);
        assert_eq!(r.len(), 513);
        assert_eq!(&r[..9], &[0, 0, 0, 0x01, 0x8e, 0x01, 0x03, 0x06, 0x16]);
        // Slot 0 = LCTRL, slot 3 is empty, slot 131 (last) is empty.
        assert_eq!(&r[9..15], &[1, 2, 3, 1, 2, 3]);
        assert_eq!(&r[18..21], &[0xff, 0x32, 0x00]);
        assert_eq!(&r[9 + 3 * 131..9 + 3 * 132], &[0xff, 0x32, 0x00]);
        assert_eq!(&r[9 + 3 * 132..], &[0; 108][..]);
        let r = old_apex_report([9, 8, 7]);
        assert_eq!(r.len(), 33);
        assert_eq!(&r[..7], &[0x00, 0x07, 0x00, 9, 8, 7, 0x08]);
        assert_eq!(&r[19..23], &[9, 8, 7, 0x08]);
        assert!(r[23..].iter().all(|&b| b == 0));
    }

    #[test]
    fn actuation_reports_match_apex_analog() {
        // actuation_level(): 0.1 mm = (5, 5), 4.0 mm = (215, 210), 1.8 mm = level 9.
        assert_eq!(actuation_points(1), (5, 5));
        assert_eq!(actuation_points(40), (215, 210));
        assert_eq!(
            actuation_points(18),
            (ACTUATION_THRESHOLDS[20], ACTUATION_THRESHOLDS[19])
        );
        assert_eq!(actuation_points(18), (50, 46));
        assert_eq!(actuation_points(37), (210, 206));
        let r = actuation_reports(18);
        assert_eq!(r.len(), 3);
        assert!(r
            .iter()
            .all(|r| r.len() == 65 && r[..2] == [0x00, 0x31] && r[2] == 20));
        assert_eq!(&r[0][3..6], &[0x35, 50, 46]);
        assert!(!ACTUATION_KEYS.contains(&0xe3), "LGUI is not settable");
        let c = controls(true, None);
        let a = c
            .iter()
            .find(|m| m.key == SettingKey::ActuationPoint)
            .unwrap();
        assert_eq!(
            (a.options.len(), a.options[0].label.as_str()),
            (20, "0.1 mm")
        );
        assert!(a.validate(&SettingValue::Int(37)).is_ok());
        assert!(a.validate(&SettingValue::Int(36)).is_err());
        assert!(c.iter().any(|m| m.key == SettingKey::RapidTap));
        assert_eq!(controls(false, None).len(), 4);
        // apex-analog set_mode: 32 <n> (<hid> 2)*, 30 keys per report.
        let r = rapid_tap_reports(true);
        assert_eq!(r.len(), 2);
        assert!(r
            .iter()
            .all(|r| r.len() == 65 && r[..3] == [0x00, 0x32, 30]));
        assert_eq!(&r[0][3..7], &[0x35, 2, 0x1e, 2]);
        assert!(r[0][3 + 60..].iter().all(|&b| b == 0));
        assert_eq!(&rapid_tap_reports(false)[1][3..5], &[ACTUATION_KEYS[30], 0]);
        // Apex 3 / Apex Pro 3 saves are listed, blocked.
        let apex_pro3 = KeyboardSpec {
            interface: 1,
            wireless: &[],
            proto: KbProto::TenZone,
            actuation: true,
        };
        let c = controls(false, save_blocked(&apex_pro3));
        let save = c.last().unwrap();
        assert_eq!(save.key, SettingKey::SaveToDevice);
        assert!(save.persistent && save.blocked.is_some());
        assert!(save.validate(&SettingValue::Bool(true)).is_err());
    }

    #[test]
    fn brightness_alone_keeps_a_colour() {
        let mut s = SettingsMap::new();
        merge_setting(&mut s, SettingKey::Brightness, SettingValue::Int(40));
        merge_setting(&mut s, SettingKey::ColorB, SettingValue::Int(7));
        light_defaults(&mut s);
        let get = |k| int(&s, k);
        assert_eq!(get(SettingKey::Brightness), Some(40));
        assert_eq!(
            [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB].map(get),
            [Some(0), Some(120), Some(7)]
        );
    }

    #[test]
    fn firmware_version_parses() {
        assert_eq!(parse_version(b"1.19.7\0\0"), Some((1, 19, 7)));
        assert_eq!(parse_version(b"KBD: 1.20.0 LED 0.1.2"), Some((1, 20, 0)));
        assert_eq!(parse_version(b"\0\0"), None);
        assert!(Some((1, 19, 6)) < Some((1, 19, 7)) && None < Some((1, 19, 7)));
    }
}
