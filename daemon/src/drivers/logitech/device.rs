//! Generic HID++ 2.0 adapter: one wired device, wireless headset dongle, or
//! device paired to a receiver slot. Everything it offers is discovered at
//! runtime through FEATURE_SET, so one adapter serves every model.
//!
//! Wire behaviour (see `hidpp.rs` for sources):
//! - Read-only until the user changes something: probing, battery and current
//!   DPI / rate / sidetone / brightness reads only.
//! - The first DPI / report-rate / lighting write switches ONBOARD_PROFILES
//!   `0x8100` to host mode (volatile; the onboard profile returns at power-up).
//!   Profiles themselves are never written.
//! - Lighting is written volatile only: 0x8070 persistence 0, 0x8071 flags 0x01.
//!   What the user set (lighting, DPI, report rate) is re-applied whenever
//!   the link comes (back) up; only those values are saved.
//! - A sleeping or switched-off device answers pings with an error; it is
//!   re-pinged every few seconds and re-initialised when it answers.
//! - A receiver slot re-reads its pairing register before every ping. Once
//!   the receiver reports another device (re-paired) or none (unpaired) in
//!   the slot, the adapter lets go of the device and shows as disconnected,
//!   so it never talks to the new device with the old one's feature map; the
//!   receiver's next rescan opens whatever is paired there now.
//! - Keyboards with 0x8081 get the static colour on every key as well (range
//!   update + FrameEnd, Solaar's single-colour path), since per-key paint
//!   overrides the zone colour.
//! - Headsets with 0x1F20 v2+ get the auto-off timer (minutes).
//! - Mice whose 0x2202 reports a settable lift-off distance get it as a
//!   control; it rides the volatile DPI write (fn 6), like DPI itself.
//! - Onboard memory is never written. The store paths that exist (0x8100
//!   profile sectors, the 0x8070 / 0x8071 store flags, the 0x8310 stored
//!   equalizer) are shown as blocked controls with the reason; the research
//!   is in `docs/onboard-writes/logitech.md`.

use super::hidpp::{self, answered, note_miss, MouseEffectKind};
use crate::drivers::DeviceAdapter;
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub const VID: u16 = 0x046d;

const PING_EVERY: Duration = Duration::from_secs(5);
const SYNC_EVERY: Duration = Duration::from_secs(20);
/// How often an incomplete probe is retried while the device answers.
const PROBE_RETRY: Duration = Duration::from_secs(30);
/// DPI slider when the device's list cannot be read.
const DPI_FALLBACK: (u16, u16, u16) = (100, 25600, 50);
/// 0x2202 lift-off byte values (0 = no LOD control); the device rejects 0
/// in a write (OpenLogi x2202 `Lod`, Solaar #3372).
const LOD_CHOICES: [(u8, &str); 3] = [(1, "Low"), (2, "Medium"), (3, "High")];

/// How the adapter reaches the device.
#[derive(Clone, Copy)]
pub enum Link {
    /// Corded device, headset dongle, or Bluetooth: device index 0xff.
    Direct,
    /// Receiver slot 1..=6; pairing info is read the way `bolt` says, in
    /// short reports when the node has them.
    Slot { slot: u8, bolt: bool, short: bool },
}

/// Feature indexes found on the device (feature id in the comment).
#[derive(Default)]
struct Features {
    name: Option<u8>,
    /// (index, feature id) of the preferred battery feature.
    battery: Option<(u8, u16)>,
    /// (index, extended 0x2202?).
    dpi: Option<(u8, bool)>,
    /// (index, extended 0x8061?).
    rate: Option<(u8, bool)>,
    /// (index, 0x8070 or 0x8071).
    led: Option<(u8, u16)>,
    onboard: Option<u8>,
    profile_mgmt: Option<u8>,
    brightness: Option<u8>,
    sidetone: Option<u8>,
    perkey: Option<u8>,
    /// 0x1F20 version 2+ (auto-off timer).
    adc_power: Option<u8>,
    equalizer: Option<u8>,
}

impl Features {
    fn note(&mut self, fid: u16, idx: u8, version: u8) {
        use hidpp::*;
        // Battery preference: unified > status > voltage > ADC.
        let bat_rank = |f: u16| {
            [
                FEAT_UNIFIED_BATTERY,
                FEAT_BATTERY_STATUS,
                FEAT_BATTERY_VOLTAGE,
                FEAT_ADC,
            ]
            .iter()
            .position(|&b| b == f)
        };
        match fid {
            FEAT_DEVICE_NAME => self.name = Some(idx),
            FEAT_UNIFIED_BATTERY | FEAT_BATTERY_STATUS | FEAT_BATTERY_VOLTAGE | FEAT_ADC => {
                if self
                    .battery
                    .is_none_or(|(_, cur)| bat_rank(fid) < bat_rank(cur))
                {
                    self.battery = Some((idx, fid));
                }
            }
            FEAT_EXT_DPI => self.dpi = Some((idx, true)),
            FEAT_ADJ_DPI if self.dpi.is_none() => self.dpi = Some((idx, false)),
            FEAT_EXT_RR => self.rate = Some((idx, true)),
            FEAT_RR if self.rate.is_none() => self.rate = Some((idx, false)),
            FEAT_RGB => self.led = Some((idx, fid)),
            FEAT_COLOR_LED if self.led.is_none() => self.led = Some((idx, fid)),
            FEAT_ONBOARD => self.onboard = Some(idx),
            FEAT_PROFILE_MGMT => self.profile_mgmt = Some(idx),
            FEAT_BRIGHTNESS => self.brightness = Some(idx),
            FEAT_SIDETONE => self.sidetone = Some(idx),
            FEAT_PER_KEY => self.perkey = Some(idx),
            FEAT_EQUALIZER => self.equalizer = Some(idx),
            _ => {}
        }
        if fid == FEAT_ADC && version >= 2 {
            self.adc_power = Some(idx);
        }
    }
}

/// Why each researched onboard-memory write is not offered (sources and
/// issue links in `docs/onboard-writes/logitech.md`).
pub(super) const BLOCK_PROFILES: &str =
    "onboard profile (0x8100) writes: libratbag and Solaar disagree on \
     which profile is active (libratbag #1875), and libratbag rewrites whole sectors (#1898)";
pub(super) const BLOCK_ZONE_STORE: &str =
    "0x8070 store flag (persistence 1): libratbag writes an effect id where \
     Solaar, OpenRGB and OpenLogi write an effect index, and no capture confirms the stored result";
pub(super) const BLOCK_RGB_STORE: &str =
    "0x8071 store flag: OpenLogi (bit 1 = EEPROM) and OpenRGB (0x01 = persist) disagree";
pub(super) const BLOCK_EQ_STORE: &str =
    "stored equalizer (0x8310 mode 2) does not change the sound \
     and mode 1 is rejected (Solaar #2546, #3128)";

/// Blocked placeholders for researched onboard-memory writes: a
/// save-to-device mode and/or a stored equalizer, each with its reason.
pub(super) fn memory_placeholders(save: Option<&str>, eq: Option<&str>) -> Vec<SettingMeta> {
    let save = save.map(|why| {
        SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory")
            .persistent()
            .blocked(why)
    });
    let eq = eq.map(|why| {
        SettingMeta::dropdown(
            SettingKey::EqPreset,
            "Equalizer (saved on device)",
            Vec::new(),
        )
        .persistent()
        .blocked(why)
    });
    save.into_iter().chain(eq).collect()
}

/// Auto-off choices (minutes, 0 = never): the values HeadsetControl's G535
/// support accepts, all inside Solaar's 0..=255 range for 0x1F20.
const AUTO_OFF_MIN: [u8; 7] = [0, 1, 2, 5, 10, 15, 30];

/// One LED zone (0x8070) or cluster (0x8071): effect id → slot index.
struct LedZone {
    index: u8,
    slots: HashMap<u16, u8>,
}

pub struct HidppAdapter {
    id: DeviceId,
    kind: DeviceKind,
    name: String,
    /// Name came from the model table; DEVICE_NAME must not replace it.
    fixed_name: bool,
    dev: Option<HidrawDevice>,
    index: u8,
    /// Receiver slots: (bolt, short) for the pairing-register check.
    pairing: Option<(bool, bool)>,
    paths: ConfigPaths,
    /// What the UI shows: `chosen` + values read from the device.
    settings: SettingsMap,
    /// What the user set (the saved file).
    chosen: SettingsMap,
    last_error: Option<String>,
    online: bool,
    probed: bool,
    host_mode: bool,
    led_claimed: bool,
    feat: Features,
    battery: Option<BatteryState>,
    dpis: Vec<u16>,
    /// Raw 0x2202 lift-off byte last read back or written; DPI writes resend it.
    lod: Option<u8>,
    /// 0x2202 reports a settable lift-off distance.
    lod_settable: bool,
    rates: Vec<u32>,
    zones: Vec<LedZone>,
    brightness: Option<(u16, u16)>,
    /// First and last 0x8081 key id.
    perkey: Option<(u8, u8)>,
    last_ping: Instant,
    last_sync: Instant,
    last_probe: Instant,
}

impl HidppAdapter {
    /// `name` is `Some` for models with a fixed table name; receiver slots
    /// and unlisted devices pass `None` and take name and kind from
    /// DEVICE_NAME once the device answers.
    pub fn new(
        paths: ConfigPaths,
        dev: HidrawDevice,
        link: Link,
        product_id: u16,
        kind: DeviceKind,
        name: Option<&str>,
    ) -> Result<Self> {
        let chosen = hako_core::config::load_device_settings(&paths, VID, product_id)?;
        let (index, pairing) = match link {
            Link::Direct => (hidpp::DIRECT, None),
            Link::Slot { slot, bolt, short } => (slot, Some((bolt, short))),
        };
        let mut a = Self {
            id: DeviceId {
                vendor_id: VID,
                product_id,
                instance: format!("hidpp:{index}"),
            },
            kind,
            name: name.map_or_else(|| fallback_name(kind, product_id), str::to_string),
            fixed_name: name.is_some(),
            dev: Some(dev),
            index,
            pairing,
            paths,
            settings: chosen.clone(),
            chosen,
            last_error: None,
            online: false,
            probed: false,
            host_mode: false,
            led_claimed: false,
            feat: Features::default(),
            battery: None,
            dpis: Vec::new(),
            lod: None,
            lod_settable: false,
            rates: Vec::new(),
            zones: Vec::new(),
            brightness: None,
            perkey: None,
            last_ping: Instant::now(),
            last_sync: Instant::now(),
            last_probe: Instant::now(),
        };
        if a.ping() {
            a.link_up();
        }
        info!(
            "{} open index={} online={} features: dpi={:?} rate={:?} led={:?} battery={:?}",
            a.name, index, a.online, a.feat.dpi, a.feat.rate, a.feat.led, a.feat.battery
        );
        Ok(a)
    }

    fn call(&mut self, feat: u8, fnid: u8, params: &[u8]) -> Result<[u8; 20]> {
        let dev = self.dev.as_mut().context("device closed")?;
        hidpp::request(dev, self.index, feat, fnid, params)
    }

    /// ROOT GetProtocolVersion; an offline device gets an error from its receiver.
    fn ping(&mut self) -> bool {
        self.last_ping = Instant::now();
        if !self.still_paired() {
            warn!(
                "{}: slot {} no longer holds this device; releasing it",
                self.name, self.index
            );
            self.dev = None;
            self.online = false;
            self.last_error = Some(format!(
                "no longer paired to receiver slot {} (unpaired or replaced)",
                self.index
            ));
            return false;
        }
        self.call(0x00, 0x01, &[0x00, 0x00, 0x88]).is_ok()
    }

    /// Receiver slots: false once the pairing register names another device
    /// or none. A failed read (receiver busy) proves nothing, so it passes.
    fn still_paired(&mut self) -> bool {
        let (Some((bolt, short)), Some(dev)) = (self.pairing, self.dev.as_mut()) else {
            return true;
        };
        let sub = hidpp::pairing_sub(bolt, self.index);
        match hidpp::read_long_register(dev, hidpp::REG_RECEIVER_INFO, sub, short) {
            Ok(r) => {
                hidpp::parse_pairing(bolt, &r).map(|(wpid, _)| wpid) == Some(self.id.product_id)
            }
            Err(_) => true,
        }
    }

    fn link_up(&mut self) {
        self.online = true;
        self.host_mode = false;
        self.led_claimed = false;
        self.lod = None;
        if !self.probed {
            self.last_probe = Instant::now();
            match self.probe() {
                Ok(()) => {
                    self.probed = true;
                    self.last_error = None;
                }
                Err(e) => {
                    warn!("{}: hidpp probe: {e}", self.name);
                    self.last_error = Some(e.to_string());
                }
            }
        }
        if self.probed {
            // Saved values reach the device below: check them against the
            // controls the probe found first.
            let controls = self.controls();
            crate::drivers::sanitize_saved(&mut self.chosen, &controls);
            crate::drivers::sanitize_saved(&mut self.settings, &controls);
        }
        self.sync();
        // Host-mode DPI / rate are volatile: put back what the user chose.
        let restore = [
            (SettingKey::LiftOffDistance, self.lod_settable),
            (SettingKey::Dpi, self.feat.dpi.is_some()),
            (SettingKey::ReportRate, self.feat.rate.is_some()),
        ];
        for (key, supported) in restore {
            let Some(v) = get_setting(&self.chosen, key).and_then(|v| v.as_int()) else {
                continue;
            };
            if !supported {
                continue;
            }
            let out_of_range = || anyhow::anyhow!("saved value {v} is out of range");
            let written = match key {
                SettingKey::Dpi => u16::try_from(v)
                    .map_err(|_| out_of_range())
                    .and_then(|dpi| self.set_dpi(dpi)),
                SettingKey::LiftOffDistance => u8::try_from(v)
                    .map_err(|_| out_of_range())
                    .and_then(|lod| self.set_lod(lod)),
                _ => u32::try_from(v)
                    .map_err(|_| out_of_range())
                    .and_then(|hz| self.set_rate(hz))
                    .map(|()| {
                        merge_setting(&mut self.settings, key, SettingValue::Int(v));
                    }),
            };
            if let Err(e) = written {
                warn!("{}: restore {}: {e}", self.name, key.as_str());
                self.last_error = Some(e.to_string());
            }
        }
        // Firmware falls back to its onboard effect after power-up.
        if self.feat.led.is_some() && get_setting(&self.settings, SettingKey::Effect).is_some() {
            if let Err(e) = self.apply_lighting() {
                warn!("{}: lighting restore: {e}", self.name);
                self.last_error = Some(e.to_string());
            }
        }
    }

    fn link_down(&mut self) {
        if self.online {
            info!("{} went offline", self.name);
        }
        self.online = false;
        if let Some(b) = &mut self.battery {
            b.percent = None;
            b.charging = false;
        }
    }

    /// FEATURE_SET walk, then the capability reads each feature needs. What
    /// was found is kept, but any read that got no answer fails the probe,
    /// so it is retried.
    fn probe(&mut self) -> Result<()> {
        let r = self.call(0x00, 0x00, &hidpp::FEAT_FEATURE_SET.to_be_bytes())?;
        let fs = r[4];
        if fs == 0 {
            bail!("no FEATURE_SET");
        }
        // Count excludes ROOT (index 0), so valid indices are 0..=count.
        let count = self.call(fs, 0x00, &[])?[4];
        let mut feat = Features::default();
        let mut missed = Vec::new();
        for i in 1..=count.min(63) {
            // fn 1 → feature id BE, flags, version.
            if let Some(r) = answered(self.call(fs, 0x01, &[i]), "feature table", &mut missed) {
                feat.note(u16::from_be_bytes([r[4], r[5]]), i, r[7]);
            }
        }
        self.feat = feat;
        self.read_name(&mut missed);
        self.read_caps(&mut missed);
        if !missed.is_empty() {
            bail!("incomplete probe ({})", missed.join("; "));
        }
        Ok(())
    }

    fn read_name(&mut self, missed: &mut Vec<String>) {
        let Some(idx) = self.feat.name else { return };
        if !self.fixed_name {
            if let Some(r) = answered(self.call(idx, 0x00, &[]), "name length", missed) {
                let len = usize::from(r[4]).min(64);
                let mut raw = Vec::new();
                while raw.len() < len {
                    match self.call(idx, 0x01, &[raw.len() as u8]) {
                        Ok(r) => raw.extend_from_slice(&r[4..4 + 16.min(len - raw.len())]),
                        Err(e) => {
                            note_miss(e, "name", missed);
                            break;
                        }
                    }
                }
                let name = String::from_utf8_lossy(&raw)
                    .trim_end_matches('\0')
                    .trim()
                    .to_string();
                if !name.is_empty() {
                    self.name = format!("Logitech {name}");
                }
            }
        }
        // Pairing kinds can be coarse (0 = unknown) and an unlisted device's
        // kind is a descriptor guess; the device knows.
        if !self.fixed_name || self.kind == DeviceKind::Other {
            if let Some(r) = answered(self.call(idx, 0x02, &[]), "device type", missed) {
                let kind = hidpp::kind_from_hidpp20(r[4]);
                if kind != DeviceKind::Other {
                    self.kind = kind;
                }
            }
        }
    }

    fn read_caps(&mut self, missed: &mut Vec<String>) {
        if let Some((idx, true)) = self.feat.dpi {
            // fn 1 GetSensorCapabilities(sensor 0) → flags at [6], bit 1 = LOD.
            self.lod_settable = answered(self.call(idx, 0x01, &[0x00]), "sensor", missed)
                .is_some_and(|r| hidpp::ext_dpi_has_lod(r[6]));
        }
        if let Some((idx, ext)) = self.feat.dpi {
            self.dpis = if ext {
                // fn 2 GetSensorDpiRanges(sensor 0, X, page) → 3-byte echo + list.
                let mut bytes = Vec::new();
                for page in 0..4u8 {
                    match self.call(idx, 0x02, &[0x00, 0x00, page]) {
                        Ok(r) => bytes.extend_from_slice(&r[7..20]),
                        // A refused page is past the end of the list.
                        Err(e) => {
                            note_miss(e, "DPI list", missed);
                            break;
                        }
                    }
                    if bytes.as_chunks::<2>().0.contains(&[0, 0]) {
                        break;
                    }
                }
                hidpp::parse_dpi_list(&bytes)
            } else {
                // fn 1 GetSensorDpiList(sensor 0) → sensor echo + list.
                answered(self.call(idx, 0x01, &[0x00]), "DPI list", missed)
                    .map(|r| hidpp::parse_dpi_list(&r[5..20]))
                    .unwrap_or_default()
            };
        }
        if let Some((idx, ext)) = self.feat.rate {
            self.rates = if ext {
                answered(self.call(idx, 0x01, &[]), "report rates", missed)
                    .map(|r| hidpp::ext_rr_supported(u16::from_be_bytes([r[4], r[5]])))
                    .unwrap_or_default()
            } else {
                answered(self.call(idx, 0x00, &[]), "report rates", missed)
                    .map(|r| hidpp::rr_supported(r[4]))
                    .unwrap_or_default()
            };
        }
        if let Some((idx, fid)) = self.feat.led {
            self.zones = self.read_zones(idx, fid, missed).unwrap_or_else(|e| {
                warn!("{}: LED zones: {e}", self.name);
                note_miss(e, "LED zones", missed);
                Vec::new()
            });
        }
        if let (Some(idx), DeviceKind::Keyboard) = (self.feat.perkey, self.kind) {
            // fn 0 [0, page] → 2-byte echo + 14 bitmap bytes, pages 0..=2.
            let mut bitmap = Vec::new();
            for page in 0..3u8 {
                match self.call(idx, 0x00, &[0x00, page]) {
                    Ok(r) => bitmap.extend_from_slice(&r[6..20]),
                    Err(e) => {
                        note_miss(e, "key map", missed);
                        break;
                    }
                }
            }
            self.perkey = hidpp::perkey_range(&bitmap);
        }
        if let Some(idx) = self.feat.brightness {
            // fn 0 GetInfo: max BE u16, steps, caps, min BE u16.
            if let Some(r) = answered(self.call(idx, 0x00, &[]), "brightness range", missed) {
                let max = u16::from_be_bytes([r[4], r[5]]);
                let min = u16::from_be_bytes([r[8], r[9]]);
                self.brightness = (max > min).then_some((min, max));
            }
        }
        if let Some((_, fid)) = self.feat.battery {
            self.battery = Some(BatteryState {
                label: if fid == hidpp::FEAT_ADC {
                    "Headset"
                } else {
                    "Battery"
                }
                .into(),
                percent: None,
                charging: false,
                present: true,
            });
        }
    }

    /// Zones (0x8070 fn 1/2) or clusters (0x8071 fn 0) and their effect slots.
    fn read_zones(&mut self, idx: u8, fid: u16, missed: &mut Vec<String>) -> Result<Vec<LedZone>> {
        let rgb = fid == hidpp::FEAT_RGB;
        let count = if rgb {
            self.call(idx, 0x00, &[0xff, 0xff, 0x00])?[6]
        } else {
            self.call(idx, 0x00, &[])?[4]
        };
        let mut zones = Vec::new();
        for z in 0..count.min(8) {
            let effects = if rgb {
                self.call(idx, 0x00, &[z, 0xff, 0x00])?[8]
            } else {
                self.call(idx, 0x01, &[z])?[7]
            };
            let mut slots = HashMap::new();
            for slot in 0..effects.min(16) {
                let info = if rgb {
                    self.call(idx, 0x00, &[z, slot, 0x00])
                } else {
                    self.call(idx, 0x02, &[z, slot])
                };
                if let Some(r) = answered(info, "LED effect", missed) {
                    slots.insert(u16::from_be_bytes([r[6], r[7]]), slot);
                }
            }
            zones.push(LedZone { index: z, slots });
        }
        Ok(zones)
    }

    /// Read current values. Headsets error on ADC reads while switched off.
    fn sync(&mut self) {
        self.last_sync = Instant::now();
        if let Some((idx, fid)) = self.feat.battery {
            let fnid = if fid == hidpp::FEAT_UNIFIED_BATTERY {
                0x01
            } else {
                0x00
            };
            match self.call(idx, fnid, &[]) {
                Ok(r) => {
                    let p = &r[4..];
                    let reading = match fid {
                        hidpp::FEAT_UNIFIED_BATTERY => Some(hidpp::parse_unified_battery(p)),
                        hidpp::FEAT_BATTERY_STATUS => Some(hidpp::parse_battery_status(p)),
                        hidpp::FEAT_BATTERY_VOLTAGE => Some(hidpp::parse_battery_voltage(p)),
                        _ => hidpp::parse_adc(p),
                    };
                    if let Some(b) = &mut self.battery {
                        (b.percent, b.charging) = reading.unwrap_or((None, false));
                    }
                }
                Err(_) => {
                    if let Some(b) = &mut self.battery {
                        (b.percent, b.charging) = (None, false);
                    }
                }
            }
        }
        if let Some((idx, ext)) = self.feat.dpi {
            // 0x2202 fn 5 → sensor, X BE, default X, Y, default Y, LOD at [13];
            // 0x2201 fn 2 → sensor, DPI BE.
            if let Ok(r) = self.call(idx, if ext { 0x05 } else { 0x02 }, &[0x00]) {
                let dpi = u16::from_be_bytes([r[5], r[6]]);
                if dpi >= 50 {
                    merge_setting(
                        &mut self.settings,
                        SettingKey::Dpi,
                        SettingValue::Int(dpi.into()),
                    );
                }
                if ext {
                    self.lod = Some(r[13]);
                    if self.lod_settable && LOD_CHOICES.iter().any(|&(v, _)| v == r[13]) {
                        merge_setting(
                            &mut self.settings,
                            SettingKey::LiftOffDistance,
                            SettingValue::Int(r[13].into()),
                        );
                    }
                }
            }
        }
        if let Some((idx, ext)) = self.feat.rate {
            // 0x8061 fn 2 → rate index; 0x8060 fn 1 → period in ms.
            let hz = if ext {
                self.call(idx, 0x02, &[])
                    .ok()
                    .and_then(|r| hidpp::EXT_RR_HZ.get(usize::from(r[4])).copied())
            } else {
                self.call(idx, 0x01, &[0x00])
                    .ok()
                    .map(|r| 1000 / u32::from(r[4].max(1)))
            };
            if let Some(hz) = hz {
                merge_setting(
                    &mut self.settings,
                    SettingKey::ReportRate,
                    SettingValue::Int(hz.into()),
                );
            }
        }
        if let Some(idx) = self.feat.brightness {
            if let Ok(r) = self.call(idx, 0x01, &[]) {
                let v = u16::from_be_bytes([r[4], r[5]]);
                merge_setting(
                    &mut self.settings,
                    SettingKey::Brightness,
                    SettingValue::Int(v.into()),
                );
            }
        }
        if let Some(idx) = self.feat.sidetone {
            if let Ok(r) = self.call(idx, 0x00, &[]) {
                merge_setting(
                    &mut self.settings,
                    SettingKey::MicSideTone,
                    SettingValue::Int(r[4].min(100).into()),
                );
            }
        }
        if let Some(idx) = self.feat.adc_power {
            if let Ok(r) = self.call(idx, 0x01, &[]) {
                merge_setting(
                    &mut self.settings,
                    SettingKey::PmShutdown,
                    SettingValue::Int(r[4].into()),
                );
            }
        }
    }

    /// SET_ONBOARD_MODE (fn 1) host = 2, so host DPI/rate/LED writes stick.
    fn ensure_host_mode(&mut self) -> Result<()> {
        if let (Some(ob), false) = (self.feat.onboard, self.host_mode) {
            self.call(ob, 0x01, &[0x02])?;
            self.host_mode = true;
        }
        Ok(())
    }

    fn set_dpi(&mut self, want: u16) -> Result<()> {
        let (idx, ext) = self.feat.dpi.context("no DPI feature")?;
        // Snap to a value the sensor advertised.
        let dpi = self
            .dpis
            .iter()
            .copied()
            .min_by_key(|d| d.abs_diff(want))
            .unwrap_or(want);
        self.ensure_host_mode()?;
        let [hi, lo] = dpi.to_be_bytes();
        if ext {
            // fn 6: sensor, X BE, Y BE, LOD (kept).
            let lod = hidpp::ext_dpi_lod(self.lod, || self.call(idx, 0x05, &[0x00]))?;
            self.lod = Some(lod);
            self.call(idx, 0x06, &[0x00, hi, lo, hi, lo, lod])?;
        } else {
            // fn 3: sensor, DPI BE.
            self.call(idx, 0x03, &[0x00, hi, lo])?;
        }
        merge_setting(
            &mut self.settings,
            SettingKey::Dpi,
            SettingValue::Int(dpi.into()),
        );
        Ok(())
    }

    /// 0x2202 has no LOD-only write: re-send the current DPI with the new LOD.
    fn set_lod(&mut self, lod: u8) -> Result<()> {
        let dpi = get_setting(&self.settings, SettingKey::Dpi)
            .and_then(|v| v.as_int())
            .context("current DPI unknown")? as u16;
        let old = self.lod.replace(lod);
        self.set_dpi(dpi).inspect_err(|_| self.lod = old)?;
        merge_setting(
            &mut self.settings,
            SettingKey::LiftOffDistance,
            SettingValue::Int(lod.into()),
        );
        Ok(())
    }

    fn set_rate(&mut self, hz: u32) -> Result<()> {
        let (idx, ext) = self.feat.rate.context("no report-rate feature")?;
        self.ensure_host_mode()?;
        if ext {
            let i = hidpp::EXT_RR_HZ
                .iter()
                .position(|&r| r == hz)
                .context("unsupported report rate")?;
            self.call(idx, 0x03, &[i as u8])?;
        } else {
            // fn 2: period in ms.
            self.call(idx, 0x02, &[(1000 / hz.max(1)).clamp(1, 8) as u8])?;
        }
        Ok(())
    }

    fn apply_lighting(&mut self) -> Result<()> {
        let (idx, fid) = self.feat.led.context("no lighting feature")?;
        let effect = self.effect();
        let int =
            |s: &SettingsMap, k, d| get_setting(s, k).and_then(|v| v.as_int()).unwrap_or(d) as u8;
        let (r, g, b) = (
            int(&self.settings, SettingKey::ColorR, 0),
            int(&self.settings, SettingKey::ColorG, 200),
            int(&self.settings, SettingKey::ColorB, 80),
        );
        let speed = get_setting(&self.settings, SettingKey::Speed)
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
            .to_string();
        let targets: Vec<(u8, u8)> = self
            .zones
            .iter()
            .filter_map(|z| z.slots.get(&effect.effect_id()).map(|&s| (z.index, s)))
            .collect();
        let perkey = self
            .perkey
            .filter(|_| effect == MouseEffectKind::Static)
            .zip(self.feat.perkey);
        if targets.is_empty() && perkey.is_none() {
            return Ok(());
        }
        self.ensure_host_mode()?;
        let rgb = fid == hidpp::FEAT_RGB;
        if !self.led_claimed {
            if rgb {
                // Take LED power management from the firmware (PROFILE_MANAGEMENT
                // fn 6 [05], else host mode above), then SetSWControl fn 5
                // [write, mode 3, flags 4] — the PRO 2's proven claim.
                if let Some(pm) = self.feat.profile_mgmt {
                    self.call(pm, 0x06, &[0x05])?;
                }
                self.call(idx, 0x05, &[0x01, 0x03, 0x04])?;
            } else {
                // 0x8070 SetSWControl fn 8 [software]; absent on version 0.
                let _ = self.call(idx, 0x08, &[0x01, 0x00]);
            }
            self.led_claimed = true;
        }
        for (zone, slot) in targets {
            if rgb {
                let p = hidpp::mouse_effect_payload(zone, slot, effect, r, g, b, &speed);
                self.call(idx, 0x01, &p)?;
            } else {
                let p = hidpp::zone_effect_payload(zone, slot, effect, r, g, b, &speed);
                self.call(idx, 0x03, &p)?;
            }
        }
        if let Some(((first, last), pk)) = perkey {
            self.call(pk, 0x05, &hidpp::perkey_range_payload(first, last, r, g, b))?;
            self.call(pk, 0x07, &[0x00])?;
        }
        Ok(())
    }

    fn effect(&self) -> MouseEffectKind {
        MouseEffectKind::from_str(
            get_setting(&self.settings, SettingKey::Effect)
                .and_then(|v| v.as_str())
                .unwrap_or("static"),
        )
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        let mut c = Vec::new();
        if self.feat.dpi.is_some() {
            c.push(match hidpp::dpi_slider(&self.dpis) {
                Some((min, max, step)) => dpi_meta(min, max, step),
                None if !self.dpis.is_empty() => SettingMeta::dropdown(
                    Dpi,
                    "DPI",
                    self.dpis
                        .iter()
                        .map(|&d| SettingOption::int(d.into(), &d.to_string()))
                        .collect(),
                ),
                None => dpi_meta(DPI_FALLBACK.0, DPI_FALLBACK.1, DPI_FALLBACK.2),
            });
        }
        if self.lod_settable {
            c.push(lod_meta());
        }
        if self.feat.rate.is_some() && !self.rates.is_empty() {
            c.push(SettingMeta::dropdown(
                ReportRate,
                "Report rate",
                self.rates
                    .iter()
                    .map(|&hz| SettingOption::int(hz.into(), &format!("{hz} Hz")))
                    .collect(),
            ));
        }
        // Effects every zone/cluster 0 can do.
        if let Some(first) = self.zones.first() {
            let opts: Vec<SettingOption> = MouseEffectKind::ALL
                .iter()
                .filter(|(k, _, _)| first.slots.contains_key(&k.effect_id()))
                .map(|(_, v, l)| SettingOption::text(v, l))
                .collect();
            if !opts.is_empty() {
                let effect = self.effect();
                c.push(SettingMeta::dropdown(Effect, "Effect", opts));
                c.push(
                    SettingMeta::dropdown(
                        Speed,
                        "Speed",
                        vec![
                            SettingOption::text("slow", "Slow"),
                            SettingOption::text("medium", "Medium"),
                            SettingOption::text("fast", "Fast"),
                        ],
                    )
                    .hidden(!effect.has_speed()),
                );
                for (key, label) in [(ColorR, "Red"), (ColorG, "Green"), (ColorB, "Blue")] {
                    c.push(SettingMeta::color(key, label).hidden(!effect.has_color()));
                }
            }
        }
        if let Some((min, max)) = self.brightness {
            c.push(SettingMeta::slider(
                Brightness,
                "Brightness",
                min.into(),
                max.into(),
                1.0,
            ));
        }
        if self.feat.sidetone.is_some() {
            c.push(SettingMeta::slider(
                MicSideTone,
                "Sidetone",
                0.0,
                100.0,
                1.0,
            ));
        }
        if self.feat.adc_power.is_some() {
            c.push(SettingMeta::dropdown(
                PmShutdown,
                "Auto-off",
                AUTO_OFF_MIN
                    .iter()
                    .map(|&m| match m {
                        0 => SettingOption::int(0, "Never"),
                        m => SettingOption::int(m.into(), &format!("{m} min")),
                    })
                    .collect(),
            ));
        }
        // DPI / rate / lighting would be stored in the active onboard profile
        // when the device has one, else through the lighting store flag.
        let save = match (self.feat.onboard, self.feat.led) {
            (Some(_), _) => Some(BLOCK_PROFILES),
            (None, Some((_, hidpp::FEAT_RGB))) => Some(BLOCK_RGB_STORE),
            (None, Some(_)) => Some(BLOCK_ZONE_STORE),
            (None, None) => None,
        };
        c.extend(memory_placeholders(
            save,
            self.feat.equalizer.map(|_| BLOCK_EQ_STORE),
        ));
        c
    }

    fn persist(&self) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, VID, self.id.product_id, &self.chosen)
    }
}

fn dpi_meta(min: u16, max: u16, step: u16) -> SettingMeta {
    SettingMeta::slider(SettingKey::Dpi, "DPI", min.into(), max.into(), step.into()).unit("dpi")
}

fn lod_meta() -> SettingMeta {
    let options = LOD_CHOICES
        .iter()
        .map(|&(v, label)| SettingOption::int(v.into(), label))
        .collect();
    SettingMeta::dropdown(SettingKey::LiftOffDistance, "Lift-off distance", options)
}

fn fallback_name(kind: DeviceKind, pid: u16) -> String {
    let what = match kind {
        DeviceKind::Keyboard => "keyboard",
        DeviceKind::Mouse => "mouse",
        DeviceKind::Headset => "headset",
        DeviceKind::Other => "device",
    };
    format!("Logitech {what} {pid:04x}")
}

impl DeviceAdapter for HidppAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            connected: self.dev.is_some(),
            batteries: self.battery.iter().cloned().collect(),
            settings: self.settings.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if !self.online {
            bail!("{} is asleep or switched off", self.name);
        }
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value.clone());
        let n = value.as_int().unwrap_or(0);
        let result = match key {
            SettingKey::Dpi => self.set_dpi(n as u16),
            SettingKey::LiftOffDistance => self.set_lod(n as u8),
            SettingKey::ReportRate => self.set_rate(n as u32),
            SettingKey::Effect
            | SettingKey::Speed
            | SettingKey::ColorR
            | SettingKey::ColorG
            | SettingKey::ColorB => {
                // Any lighting write pins an effect, so it is restored on reconnect.
                if get_setting(&self.settings, SettingKey::Effect).is_none() {
                    merge_setting(
                        &mut self.settings,
                        SettingKey::Effect,
                        SettingValue::Text("static".into()),
                    );
                }
                self.apply_lighting()
            }
            SettingKey::Brightness => {
                let idx = self.feat.brightness.context("no brightness feature")?;
                self.call(idx, 0x02, &(n as u16).to_be_bytes()).map(|_| ())
            }
            SettingKey::MicSideTone => {
                let idx = self.feat.sidetone.context("no sidetone feature")?;
                self.call(idx, 0x01, &[n as u8]).map(|_| ())
            }
            SettingKey::PmShutdown => {
                let idx = self.feat.adc_power.context("no auto-off feature")?;
                self.call(idx, 0x02, &[n as u8]).map(|_| ())
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            // The device kept its old value; so do the UI and the file.
            let lighting = matches!(
                key,
                SettingKey::Effect
                    | SettingKey::Speed
                    | SettingKey::ColorR
                    | SettingKey::ColorG
                    | SettingKey::ColorB
            );
            // Several writes (zones, per-key): some may have landed.
            let several = self.zones.len() > 1 || self.perkey.is_some();
            let lit = get_setting(&before, SettingKey::Effect).is_some();
            self.settings = before;
            self.last_error = Some(e.to_string());
            if lighting && several && lit {
                // Zones written before the failure get the old lighting back.
                if let Err(e) = self.apply_lighting() {
                    warn!("{}: lighting rollback: {e}", self.name);
                }
            }
            return Err(e);
        }
        self.last_error = None;
        // Save what was applied (DPI snapped to the sensor list; a lighting
        // write also pins the effect).
        for k in [key, SettingKey::Effect] {
            if let Some(v) = get_setting(&self.settings, k).cloned() {
                if k == key || k.group() == key.group() {
                    merge_setting(&mut self.chosen, k, v);
                }
            }
        }
        self.persist()
            .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings, &mut a.chosen])
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() {
            return Ok(());
        }
        if !self.online {
            if self.last_ping.elapsed() >= PING_EVERY && self.ping() {
                info!("{} link up; initialising", self.name);
                self.link_up();
            }
            return Ok(());
        }
        if !self.probed && self.last_probe.elapsed() >= PROBE_RETRY {
            info!("{}: retrying the incomplete probe", self.name);
            self.link_up();
            return Ok(());
        }
        if self.last_sync.elapsed() >= SYNC_EVERY {
            if self.ping() {
                self.sync();
            } else {
                self.link_down();
            }
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

    #[test]
    fn battery_feature_preference() {
        let mut f = Features::default();
        f.note(hidpp::FEAT_ADC, 7, 1);
        f.note(hidpp::FEAT_BATTERY_VOLTAGE, 5, 0);
        assert_eq!(f.battery, Some((5, hidpp::FEAT_BATTERY_VOLTAGE)));
        f.note(hidpp::FEAT_UNIFIED_BATTERY, 9, 0);
        f.note(hidpp::FEAT_BATTERY_STATUS, 3, 0);
        assert_eq!(f.battery, Some((9, hidpp::FEAT_UNIFIED_BATTERY)));
        // Extended DPI / rate and 0x8071 win regardless of table order.
        f.note(hidpp::FEAT_EXT_DPI, 10, 0);
        f.note(hidpp::FEAT_ADJ_DPI, 11, 0);
        f.note(hidpp::FEAT_RGB, 12, 0);
        f.note(hidpp::FEAT_COLOR_LED, 13, 0);
        f.note(hidpp::FEAT_PER_KEY, 14, 0);
        assert_eq!(f.dpi, Some((10, true)));
        assert_eq!(f.led, Some((12, hidpp::FEAT_RGB)));
        assert_eq!(f.perkey, Some(14));
        // Auto-off needs 0x1F20 version 2 (Solaar ADCPower min_version).
        assert_eq!(f.adc_power, None);
        f.note(hidpp::FEAT_ADC, 8, 4);
        assert_eq!(f.adc_power, Some(8));
    }

    #[test]
    fn onboard_writes_are_blocked_placeholders() {
        let all = memory_placeholders(Some(BLOCK_PROFILES), Some(BLOCK_EQ_STORE));
        assert_eq!(
            all.iter().map(|c| c.key).collect::<Vec<_>>(),
            [SettingKey::SaveToDevice, SettingKey::EqPreset]
        );
        for c in &all {
            assert!(c.persistent && c.blocked.is_some(), "{:?}", c.key);
            // The worker validates before `set`: every write is refused.
            assert!(c.validate(&SettingValue::Bool(true)).is_err());
        }
        assert!(memory_placeholders(None, None).is_empty());
        let mut f = Features::default();
        f.note(hidpp::FEAT_EQUALIZER, 6, 1);
        assert_eq!(f.equalizer, Some(6));
    }

    #[test]
    fn lift_off_choices_are_the_wire_values() {
        let meta = lod_meta();
        // The PRO X 2 reads back 3 (High); 0 means no LOD and is refused.
        assert!(meta.validate(&SettingValue::Int(3)).is_ok());
        assert!(meta.validate(&SettingValue::Int(0)).is_err());
    }

    #[test]
    fn auto_off_choices_validate() {
        let meta = SettingMeta::dropdown(
            SettingKey::PmShutdown,
            "Auto-off",
            AUTO_OFF_MIN
                .iter()
                .map(|&m| SettingOption::int(m.into(), "x"))
                .collect(),
        );
        assert!(meta.validate(&SettingValue::Int(30)).is_ok());
        assert!(meta.validate(&SettingValue::Int(3)).is_err());
    }

    #[test]
    fn fallback_names() {
        assert_eq!(
            fallback_name(DeviceKind::Mouse, 0x4082),
            "Logitech mouse 4082"
        );
        assert_eq!(
            fallback_name(DeviceKind::Other, 0x405f),
            "Logitech device 405f"
        );
    }
}
