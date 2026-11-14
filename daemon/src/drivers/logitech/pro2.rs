//! PRO 2 LIGHTSPEED — HID++ 2.0 over receiver `046d:c543` or device node `046d:40a8`.
//!
//! Protocol notes (Solaar / libratbag concepts, reimplemented):
//! - Prefer the kernel `logitech-hidpp-device` node when present (`046d:40a8`).
//! - Else open the receiver interface that declares report IDs `0x10`/`0x11`.
//! - Feature discovery via ROOT + FEATURE_SET; host mode via ONBOARD_PROFILES `0x8100`.
//! - DPI: EXTENDED_ADJUSTABLE_DPI `0x2202` (read fn 0x50 / write fn 0x60).
//! - Report rate: EXTENDED_ADJUSTABLE_REPORT_RATE `0x8061` (caps 0x10 / read 0x20 / write 0x30).
//! - RGB: RGB_EFFECTS `0x8071` after SW control claim (fn 0x50), as Solaar's RGBControl.
//! - The mouse may be asleep/off when the daemon starts: features are re-probed
//!   and state re-applied whenever the link comes (back) up.
//! - The model is `multi`: besides the PRO 2 (opened exactly as before), the
//!   other slots of its `c543` receiver get the generic HID++ adapter, and
//!   rescans add devices paired later. A rescan never reopens or touches the
//!   PRO 2 while its adapter is open; it only reads the receiver's pairing
//!   registers.
//! - Onboard memory is never written: its profiles use format 0x06 / 0x07,
//!   which neither libratbag nor Solaar supports, so "save to device" is a
//!   blocked placeholder (no extra request on the wire). Research:
//!   `docs/onboard-writes/logitech.md`.

use super::hidpp::{
    self, answered, ext_rr_supported, mouse_effect_payload, rr_supported, MouseEffectKind,
    EXT_RR_HZ, FEAT_ADJ_DPI, FEAT_EXT_DPI, FEAT_EXT_RR, FEAT_FEATURE_SET, FEAT_ONBOARD,
    FEAT_PROFILE_MGMT, FEAT_RGB, FEAT_ROOT, FEAT_RR,
};
use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const RX_VID: u16 = 0x046d;
pub(super) const RX_PID: u16 = 0xc543;
const MOUSE_PID: u16 = 0x40a8;

const BLOCK_SAVE: &str = "PRO 2 onboard profiles use format 0x06/0x07 (firmware-dependent), which \
     libratbag and Solaar do not support (libratbag PR #1896 closed unmerged)";

pub const MODEL: Model = Model {
    vendor_id: RX_VID,
    // Mouse PID first: it names remaps/046d_40a8.toml (the existing file).
    product_ids: &[MOUSE_PID, RX_PID],
    kind: DeviceKind::Mouse,
    name: "Logitech PRO 2 LIGHTSPEED",
    usbfs: false,
    multi: true,
    open,
};

/// The PRO 2 (unless already open) plus every other device paired to the
/// `c543` receiver. Fails only when the PRO 2 fails and nothing else is there.
fn open(_pid: u16, ctx: &DriverCtx) -> Opened {
    let is_pro2 = |id: &DeviceId| id.product_id == MOUSE_PID || id.product_id == RX_PID;
    let held = ctx.open.iter().find(|id| is_pro2(id)).cloned();
    let mut out: Vec<Box<dyn DeviceAdapter>> = Vec::new();
    let mut pro2_err = None;
    // The receiver slot the PRO 2 adapter talks to when it uses the receiver
    // node itself (no kernel child node): never give it a second adapter.
    let mut pro2_slot = held
        .as_ref()
        .filter(|id| id.product_id == RX_PID)
        .and_then(|id| id.instance.strip_prefix("hidpp:")?.parse::<u8>().ok());
    if held.is_none() {
        match LogitechMouseAdapter::try_open(ctx.paths.clone()) {
            Ok(a) => {
                if a.id.product_id == RX_PID {
                    pro2_slot = Some(a.device_index);
                }
                out.push(Box::new(a));
            }
            Err(e) => pro2_err = Some(e),
        }
    }
    let others_ctx = DriverCtx {
        open: ctx.open.iter().filter(|id| !is_pro2(id)).cloned().collect(),
        ..ctx.clone()
    };
    let skip = |slot: u8, wpid: u16| wpid == MOUSE_PID || Some(slot) == pro2_slot;
    match super::receiver_slots(RX_PID, &others_ctx, skip) {
        Ok(others) => out.extend(others),
        Err(e) if out.is_empty() && pro2_err.is_none() => return Err(e),
        Err(e) => warn!("PRO 2 receiver: other slots: {e}"),
    }
    match pro2_err {
        Some(e) if out.is_empty() && held.is_none() => Err(e),
        Some(e) => {
            warn!("PRO 2: {e}");
            Ok(out)
        }
        None => Ok(out),
    }
}

pub struct LogitechMouseAdapter {
    id: DeviceId,
    dev: Option<HidrawDevice>,
    device_index: u8,
    settings: SettingsMap,
    batteries: Vec<BatteryState>,
    last_error: Option<String>,
    last_bat: Instant,
    last_sync: Instant,
    paths: ConfigPaths,
    has_rgb: bool,
    /// Feature index table: feature_id → index on device.
    feat: FeatureMap,
    /// Raw 0x2202 lift-off byte last read back; DPI writes resend it.
    lod: Option<u8>,
    rgb_cluster: u8,
    /// Effect-id → cluster slot index, enumerated from the device at open.
    rgb_slots: std::collections::HashMap<u16, u8>,
    /// Report rates (Hz) the device advertises.
    rr_hz: Vec<u32>,
    /// Link state from the kernel hidpp battery node (`online`).
    online: bool,
    last_probe: Instant,
    /// `device_index` answered a ping (false: a guess, rediscovered on link-up).
    index_known: bool,
    /// Some feature lookup got no answer: probed again while online.
    probe_incomplete: bool,
    /// ONBOARD_PROFILES is in host mode since the link came up.
    host_mode: bool,
    /// Why the last read of DPI / report rate failed, until one succeeds.
    sync_error: Option<String>,
}

#[derive(Default, Clone)]
struct FeatureMap {
    dpi: Option<u8>,
    dpi_is_ext: bool,
    report_rate: Option<u8>,
    rr_is_ext: bool,
    rgb: Option<u8>,
    onboard: Option<u8>,
    profile_mgmt: Option<u8>,
}

impl LogitechMouseAdapter {
    pub fn try_open(paths: ConfigPaths) -> Result<Self> {
        let (info, dev, found) = open_hidpp_path()?;
        // Unanswered (mouse asleep): 1 until a ping finds the real index.
        let device_index = found.unwrap_or(1);
        let id = DeviceId {
            vendor_id: info.vendor_id,
            product_id: info.product_id,
            instance: format!("hidpp:{device_index}"),
        };
        let mut settings = hako_core::config::load_device_settings(&paths, RX_VID, RX_PID)?;
        // Also try mouse PID config stem if present.
        if settings.is_empty() {
            settings = hako_core::config::load_device_settings(&paths, RX_VID, MOUSE_PID)?;
        }
        defaults(&mut settings);
        let mut a = Self {
            id,
            dev: Some(dev),
            device_index,
            settings,
            batteries: vec![BatteryState {
                label: "Mouse".into(),
                percent: None,
                charging: false,
                present: true,
            }],
            last_error: None,
            last_bat: Instant::now(),
            last_sync: Instant::now() - Duration::from_secs(60),
            paths,
            has_rgb: false,
            feat: FeatureMap::default(),
            lod: None,
            rgb_cluster: 0,
            rgb_slots: std::collections::HashMap::new(),
            rr_hz: Vec::new(),
            online: false,
            last_probe: Instant::now(),
            index_known: found.is_some(),
            probe_incomplete: true,
            host_mode: false,
            sync_error: None,
        };
        migrate_mouse_effect(&mut a.settings);
        a.online = read_sysfs_battery().map(|b| b.online).unwrap_or(true);
        a.update_battery();
        if a.online {
            a.init_link();
        }
        info!(
            "Logitech PRO 2 open {} index={} online={} dpi_feat={:?} rr_feat={:?} rgb={}",
            info.path.display(),
            device_index,
            a.online,
            a.feat.dpi,
            a.feat.report_rate,
            a.has_rgb
        );
        Ok(a)
    }

    /// (Re)discover features and push state after the mouse link comes up.
    /// A sleeping/off mouse answers nothing, so this also runs on reconnect.
    fn init_link(&mut self) {
        self.last_probe = Instant::now();
        // Power-up brings the onboard profile back.
        self.host_mode = false;
        self.lod = None;
        if !self.index_known {
            match self.dev.as_mut().context("no dev").and_then(discover_index) {
                Ok(idx) => {
                    self.device_index = idx;
                    self.index_known = true;
                }
                Err(e) => {
                    warn!("hidpp index: {e}");
                    self.last_error = Some(format!("the mouse does not answer: {e}"));
                    return;
                }
            }
        }
        if self.probe_incomplete {
            match self.probe_features() {
                Ok(()) => {
                    self.probe_incomplete = false;
                    self.last_error = None;
                }
                Err(e) => {
                    warn!("hidpp probe: {e}");
                    self.last_error = Some(e.to_string());
                }
            }
        }
        if self.has_rgb && self.rgb_slots.is_empty() {
            if let Err(e) = self.probe_rgb_cluster() {
                warn!("rgb 0x8071 probe: {e}");
                self.last_error = Some(e.to_string());
            }
        }
        // Onboard mode stays until the user writes DPI / rate / lighting:
        // those calls take host mode themselves.
        // Read true DPI/rate from hardware. Do not push saved config —
        // that overwrote live DPI and made the UI lie.
        self.sync();
        // Re-apply saved lighting (firmware falls back to its onboard effect
        // after power-up); DPI/rate stay hardware-sourced above.
        if self.has_rgb {
            if let Err(e) = self.apply_lighting() {
                warn!("logitech lighting restore: {e}");
                self.last_error = Some(e.to_string());
            }
        }
    }

    fn update_battery(&mut self) {
        match read_sysfs_battery() {
            Some(b) if b.online => {
                self.batteries[0].percent = Some(b.percent);
                self.batteries[0].charging = b.charging;
            }
            _ => {
                self.batteries[0].percent = None;
                self.batteries[0].charging = false;
            }
        }
        self.last_bat = Instant::now();
    }

    fn hidpp_long(&mut self, feature_idx: u8, fnid: u8, params: &[u8]) -> Result<[u8; 20]> {
        let dev = self.dev.as_mut().context("no dev")?;
        hidpp::request(dev, self.device_index, feature_idx, fnid, params)
    }

    fn root_get_feature(&mut self, feature_id: u16) -> Result<Option<(u8, u8)>> {
        let hi = (feature_id >> 8) as u8;
        let lo = (feature_id & 0xff) as u8;
        let resp = self.hidpp_long(0x00, 0x00, &[hi, lo, 0x00])?;
        Ok(parse_get_feature(&resp, feature_id))
    }

    /// Index of `feature_id`; a lookup without an answer is noted in `missed`.
    fn lookup(&mut self, feature_id: u16, missed: &mut Vec<String>) -> Option<u8> {
        answered(self.root_get_feature(feature_id), "feature lookup", missed)
            .flatten()
            .map(|(idx, _)| idx)
    }

    /// What is found is kept; any lookup that got no answer fails the probe,
    /// so it is run again while the mouse is online.
    fn probe_features(&mut self) -> Result<()> {
        let mut missed = Vec::new();
        // Ping / protocol
        let _ = self.hidpp_long(0x00, 0x01, &[0x00, 0x00, 0x88]);

        // Prefer FEATURE_SET enumeration when available.
        if let Some(fs_idx) = self.lookup(FEAT_FEATURE_SET, &mut missed) {
            let count_r = self.hidpp_long(fs_idx, 0x00, &[]);
            if let Some(count_r) = answered(count_r, "feature count", &mut missed) {
                // Count excludes ROOT (index 0), so valid indices are 0..=count.
                let count = count_r[4] as usize;
                for i in 0..=count.min(63) {
                    let r = self.hidpp_long(fs_idx, 0x01, &[i as u8]);
                    if let Some(r) = answered(r, "feature table", &mut missed) {
                        let fid = u16::from(r[4]) << 8 | u16::from(r[5]);
                        let fidx = i as u8;
                        self.note_feature(fid, fidx);
                    }
                }
            }
        }

        // Explicit lookups for anything missed.
        if self.feat.dpi.is_none() {
            if let Some(idx) = self.lookup(FEAT_EXT_DPI, &mut missed) {
                self.feat.dpi = Some(idx);
                self.feat.dpi_is_ext = true;
            } else if let Some(idx) = self.lookup(FEAT_ADJ_DPI, &mut missed) {
                self.feat.dpi = Some(idx);
                self.feat.dpi_is_ext = false;
            }
        }
        if self.feat.report_rate.is_none() {
            if let Some(idx) = self.lookup(FEAT_EXT_RR, &mut missed) {
                self.feat.report_rate = Some(idx);
                self.feat.rr_is_ext = true;
            } else if let Some(idx) = self.lookup(FEAT_RR, &mut missed) {
                self.feat.report_rate = Some(idx);
                self.feat.rr_is_ext = false;
            }
        }
        if self.feat.rgb.is_none() {
            if let Some(idx) = self.lookup(FEAT_RGB, &mut missed) {
                self.feat.rgb = Some(idx);
            }
        }
        if self.feat.onboard.is_none() {
            if let Some(idx) = self.lookup(FEAT_ONBOARD, &mut missed) {
                self.feat.onboard = Some(idx);
            }
        }
        self.has_rgb = self.feat.rgb.is_some();
        self.probe_report_rates(&mut missed);
        if !missed.is_empty() {
            bail!("incomplete probe ({})", missed.join("; "));
        }
        Ok(())
    }

    fn probe_report_rates(&mut self, missed: &mut Vec<String>) {
        let Some(idx) = self.feat.report_rate else {
            return;
        };
        self.rr_hz = if self.feat.rr_is_ext {
            // fn 0x10: rate flags, big-endian u16 (Solaar ExtendedReportRate).
            answered(self.hidpp_long(idx, 0x01, &[]), "report rates", missed)
                .map(|r| ext_rr_supported(u16::from_be_bytes([r[4], r[5]])))
                .unwrap_or_default()
        } else {
            // fn 0x00: bit i set => (i+1) ms period is supported (Solaar ReportRate).
            answered(self.hidpp_long(idx, 0x00, &[]), "report rates", missed)
                .map(|r| rr_supported(r[4]))
                .unwrap_or_default()
        };
    }

    fn note_feature(&mut self, fid: u16, fidx: u8) {
        match fid {
            FEAT_EXT_DPI => {
                self.feat.dpi = Some(fidx);
                self.feat.dpi_is_ext = true;
            }
            FEAT_ADJ_DPI if self.feat.dpi.is_none() => {
                self.feat.dpi = Some(fidx);
                self.feat.dpi_is_ext = false;
            }
            FEAT_EXT_RR => {
                self.feat.report_rate = Some(fidx);
                self.feat.rr_is_ext = true;
            }
            FEAT_RR if self.feat.report_rate.is_none() => {
                self.feat.report_rate = Some(fidx);
                self.feat.rr_is_ext = false;
            }
            FEAT_RGB => self.feat.rgb = Some(fidx),
            FEAT_ONBOARD => self.feat.onboard = Some(fidx),
            FEAT_PROFILE_MGMT => self.feat.profile_mgmt = Some(fidx),
            _ => {}
        }
    }

    /// Without host mode the onboard profile overrides what is written, so
    /// a failed switch fails the write.
    fn ensure_host_mode(&mut self) -> Result<()> {
        let (Some(ob), false) = (self.feat.onboard, self.host_mode) else {
            return Ok(());
        };
        // SET_ONBOARD_MODE 0x10, HOST_MODE = 0x02
        self.hidpp_long(ob, 0x01, &[0x02])
            .context("switching to host mode")?;
        self.host_mode = true;
        Ok(())
    }

    /// [`Self::sync_from_device`], its failure kept in `sync_error`.
    fn sync(&mut self) {
        match self.sync_from_device() {
            Ok(()) => self.sync_error = None,
            Err(e) => {
                warn!("mouse sync: {e}");
                self.sync_error = Some(e.to_string());
            }
        }
    }

    fn sync_from_device(&mut self) -> Result<()> {
        let mut failed = Vec::new();
        let mut read = |r: Result<[u8; 20]>, what: &str| {
            r.map_err(|e| failed.push(format!("{what}: {e}"))).ok()
        };
        if let Some(idx) = self.feat.dpi {
            if self.feat.dpi_is_ext {
                // read fn 0x50
                if let Some(r) = read(self.hidpp_long(idx, 0x05, &[0x00]), "DPI") {
                    // params: sensor, dpi_x BE, dpi_y BE, …, lod
                    let dpi = u16::from(r[5]) << 8 | u16::from(r[6]);
                    if dpi >= 50 {
                        merge_setting(
                            &mut self.settings,
                            SettingKey::Dpi,
                            SettingValue::Int(dpi as i64),
                        );
                    }
                    // Solaar ExtendedAdjustableDpi read: sensor, x, default x,
                    // y, default y, lod (0 low / 1 medium / 2 high) at param[9].
                    self.lod = Some(r[13]);
                }
            } else {
                // classic 0x2201 get dpi fn 0x20
                if let Some(r) = read(self.hidpp_long(idx, 0x02, &[0x00]), "DPI") {
                    let dpi = u16::from(r[5]) << 8 | u16::from(r[6]);
                    if dpi >= 50 {
                        merge_setting(
                            &mut self.settings,
                            SettingKey::Dpi,
                            SettingValue::Int(dpi as i64),
                        );
                    }
                }
            }
        }

        if let Some(idx) = self.feat.report_rate {
            if self.feat.rr_is_ext {
                if let Some(r) = read(self.hidpp_long(idx, 0x02, &[]), "report rate") {
                    let rate_idx = r[4] as usize;
                    if rate_idx < EXT_RR_HZ.len() {
                        merge_setting(
                            &mut self.settings,
                            SettingKey::ReportRate,
                            SettingValue::Int(EXT_RR_HZ[rate_idx] as i64),
                        );
                    }
                }
            } else if let Some(r) = read(self.hidpp_long(idx, 0x01, &[0x00]), "report rate") {
                // classic: value is ms period
                let ms = r[4].max(1);
                let hz = 1000u32 / u32::from(ms);
                merge_setting(
                    &mut self.settings,
                    SettingKey::ReportRate,
                    SettingValue::Int(hz as i64),
                );
            }
        }

        self.update_battery();
        self.last_sync = Instant::now();
        if !failed.is_empty() {
            bail!("reading the mouse failed ({})", failed.join("; "));
        }
        Ok(())
    }

    fn set_dpi(&mut self, dpi: u16) -> Result<()> {
        let idx = self.feat.dpi.context("no DPI feature")?;
        // Host mode so the host DPI sticks.
        self.ensure_host_mode()?;
        let dpi = dpi.clamp(100, 32000);
        if self.feat.dpi_is_ext {
            let hi = (dpi >> 8) as u8;
            let lo = (dpi & 0xff) as u8;
            // write fn 0x60: sensor, x BE, y BE, lod (keep the device's LOD).
            // Rejections surface as HID++ error frames from hidpp_long.
            let lod = hidpp::ext_dpi_lod(self.lod, || self.hidpp_long(idx, 0x05, &[0x00]))?;
            self.lod = Some(lod);
            self.hidpp_long(idx, 0x06, &[0x00, hi, lo, hi, lo, lod])?;
        } else {
            let hi = (dpi >> 8) as u8;
            let lo = (dpi & 0xff) as u8;
            let _ = self.hidpp_long(idx, 0x03, &[0x00, hi, lo])?;
        }
        Ok(())
    }

    fn set_report_rate(&mut self, hz: u32) -> Result<()> {
        let idx = self.feat.report_rate.context("no report-rate feature")?;
        self.ensure_host_mode()?;
        if self.feat.rr_is_ext {
            // Closest rate the device advertised (probe), else the exact one.
            let target = self
                .rr_hz
                .iter()
                .copied()
                .min_by_key(|cand| cand.abs_diff(hz))
                .unwrap_or(hz);
            let best = EXT_RR_HZ
                .iter()
                .position(|&cand| cand == target)
                .context("unsupported report rate")?;
            self.hidpp_long(idx, 0x03, &[best as u8])?;
        } else {
            // classic: set period in ms (1=1000Hz, 2=500, 4=250, 8=125)
            let ms = match hz {
                0..=187 => 8,
                188..=375 => 4,
                376..=750 => 2,
                _ => 1,
            };
            let _ = self.hidpp_long(idx, 0x02, &[ms])?;
        }
        Ok(())
    }

    fn probe_rgb_cluster(&mut self) -> Result<()> {
        let idx = self.feat.rgb.context("no RGB feature")?;
        // GET_INFO device: params [0xFF,0xFF,0x00] -> [cluster,effect,clusterCount,...]
        // Device info (params begin at reply byte [4]): clusterCount at param[2] = [6].
        let dev_info = self.hidpp_long(idx, 0x00, &[0xFF, 0xFF, 0x00])?;
        let cluster_count = dev_info[6];
        if cluster_count == 0 {
            self.has_rgb = false;
            return Ok(());
        }
        self.has_rgb = true;
        // Cluster 0 is the logo. Cluster info: numEffects at param[4] = [8].
        self.rgb_cluster = 0;
        let cl = self.hidpp_long(idx, 0x00, &[self.rgb_cluster, 0xFF, 0x00])?;
        let num_effects = cl[8];
        // Enumerate each slot's effect id (be16 at param[2..4] = [6..8]) → slot map.
        // An unanswered slot fails the whole list, so it is read again later.
        let mut slots = std::collections::HashMap::new();
        let mut missed = Vec::new();
        for slot in 0..num_effects.min(16) {
            let ei = self.hidpp_long(idx, 0x00, &[self.rgb_cluster, slot, 0x00]);
            if let Some(ei) = answered(ei, "effect", &mut missed) {
                let effect_id = (u16::from(ei[6]) << 8) | u16::from(ei[7]);
                slots.insert(effect_id, slot);
            }
        }
        if !missed.is_empty() {
            bail!("RGB effect list incomplete ({})", missed.join("; "));
        }
        self.rgb_slots = slots;
        info!(
            "Logitech RGB cluster: {} effect(s) supported {:?}",
            self.rgb_slots.len(),
            {
                let mut ids: Vec<u16> = self.rgb_slots.keys().copied().collect();
                ids.sort_unstable();
                ids
            }
        );
        Ok(())
    }

    fn apply_lighting(&mut self) -> Result<()> {
        let idx = self.feat.rgb.context("no RGB feature")?;
        let effect = MouseEffectKind::from_str(
            get_setting(&self.settings, SettingKey::Effect)
                .and_then(|v| v.as_str())
                .unwrap_or("static"),
        );
        // Resolve the cluster slot for this effect. Only effects the device
        // enumerated are offered in the UI, so an unknown id means "nothing to
        // do" — never fall back to a guessed slot (that lights the wrong effect).
        let Some(&slot) = self.rgb_slots.get(&effect.effect_id()) else {
            return Ok(());
        };
        // Solaar RGBControl claim: take LED power management from the firmware
        // (PROFILE_MANAGEMENT fn 0x60 [05], else ONBOARD_PROFILES host mode),
        // then SetSWControl fn 0x50 [set=01, mode=03, flags=NVCONFIG 04].
        // Control stays claimed; releasing hands LEDs back to the onboard effect.
        if let Some(pm) = self.feat.profile_mgmt {
            self.hidpp_long(pm, 0x06, &[0x05])?;
        } else {
            self.ensure_host_mode()?;
        }
        self.hidpp_long(idx, 0x05, &[0x01, 0x03, 0x04])?;
        let r = get_setting(&self.settings, SettingKey::ColorR)
            .and_then(|v| v.as_int())
            .unwrap_or(0) as u8;
        let g = get_setting(&self.settings, SettingKey::ColorG)
            .and_then(|v| v.as_int())
            .unwrap_or(200) as u8;
        let b = get_setting(&self.settings, SettingKey::ColorB)
            .and_then(|v| v.as_int())
            .unwrap_or(80) as u8;
        let speed = get_setting(&self.settings, SettingKey::Speed)
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
            .to_string();
        let payload = mouse_effect_payload(self.rgb_cluster, slot, effect, r, g, b, &speed);
        // SetRgbClusterEffect = fn 0x10.
        self.hidpp_long(idx, 0x01, &payload)?;
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, RX_VID, RX_PID, &self.settings)
    }
}

/// The node to talk to and the PRO 2's device index on it; `None` when the
/// mouse did not answer (asleep): it is looked up again on link-up.
fn open_hidpp_path() -> Result<(HidDeviceInfo, HidrawDevice, Option<u8>)> {
    let list = enumerate_hidraw()?;

    // 1) Direct PRO 2 device node (kernel logitech-hidpp-device)
    for info in list
        .iter()
        .filter(|d| d.vendor_id == RX_VID && d.product_id == MOUSE_PID)
    {
        if let Ok(mut dev) = HidrawDevice::open(&info.path) {
            let idx = discover_index(&mut dev).ok();
            return Ok((info.clone(), dev, idx));
        }
    }

    // 2) Receiver interfaces that look like HID++ (prefer those with report id 0x10)
    let mut rx: Vec<_> = list
        .into_iter()
        .filter(|d| d.vendor_id == RX_VID && d.product_id == RX_PID)
        .collect();
    // Prefer higher interface numbers (HID++ is often the third interface)
    rx.sort_by_key(|d| std::cmp::Reverse(d.interface.unwrap_or(0)));

    let mut last_err = None;
    for info in &rx {
        match HidrawDevice::open(&info.path) {
            Ok(mut dev) => {
                let short = crate::hid::report_ids(&info.report_descriptor()).contains(&0x10);
                // The pairing registers name the PRO 2's slot (asleep or
                // not); a receiver that does not answer them is pinged.
                let found = match pairing_slot(&mut dev, short) {
                    Some(Some(slot)) => Ok(slot),
                    Some(None) => Err(anyhow::anyhow!("no PRO 2 paired to this receiver")),
                    None => discover_index(&mut dev),
                };
                match found {
                    Ok(idx) => return Ok((info.clone(), dev, Some(idx))),
                    Err(e) => last_err = Some(format!("{}: {e}", info.path.display())),
                }
            }
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    bail!(
        "no HID++ path for PRO 2 ({})",
        last_err.unwrap_or_else(|| "not found".into())
    )
}

/// The receiver slot paired with the PRO 2 (`Some(None)`: none is), or
/// `None` when the node answers no pairing-register read.
fn pairing_slot(dev: &mut HidrawDevice, short: bool) -> Option<Option<u8>> {
    let mut answered = false;
    for slot in 1..=6u8 {
        let sub = hidpp::pairing_sub(false, slot);
        match hidpp::read_long_register(dev, hidpp::REG_RECEIVER_INFO, sub, short) {
            Ok(r) => {
                answered = true;
                if hidpp::parse_pairing(false, &r).map(|(wpid, _)| wpid) == Some(MOUSE_PID) {
                    return Some(Some(slot));
                }
            }
            // An empty slot is refused.
            Err(e) if hidpp::refused(&e) => answered = true,
            // Silence from the start: this node does not read registers.
            Err(_) if !answered => return None,
            Err(_) => {}
        }
    }
    answered.then_some(None)
}

/// The index whose ROOT ping comes back echoed (an asleep mouse answers
/// nothing, a receiver's error frame is not an echo).
fn discover_index(dev: &mut HidrawDevice) -> Result<u8> {
    for idx in 1u8..=6 {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x11;
        pkt[1] = idx;
        pkt[2] = 0x00; // ROOT
        pkt[3] = 0x1a; // getProtocolVersion fn1 + swid
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0x88;
        hidpp::drain_noise(dev);
        if dev.write(&pkt).is_err() {
            // try short
            let mut s = [0u8; 7];
            s[0] = 0x10;
            s[1] = idx;
            s[2] = 0x00;
            s[3] = 0x1a;
            s[6] = 0x88;
            let _ = dev.write(&s);
        }
        let mut buf = [0u8; 32];
        let deadline = Instant::now() + Duration::from_millis(80);
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            match dev.read_timeout(&mut buf, rem.min(Duration::from_millis(40))) {
                Ok(n) if is_ping_echo(&buf[..n], idx) => return Ok(idx),
                Ok(_) => {}
                Err(e) if hidpp::hard_read_error(&e) => return Err(e),
                Err(_) => {}
            }
        }
    }
    bail!("no hidpp device index")
}

/// The reply to [`discover_index`]'s ping of `idx`: ROOT (index 0) fn 1 with
/// its software id 0xa, the ping byte 0x88 echoed at \[6\]. HID++ error frames
/// (0x8f / 0xff at \[2\]) and notifications do not match.
fn is_ping_echo(buf: &[u8], idx: u8) -> bool {
    buf.len() >= 7
        && matches!(buf[0], 0x10 | 0x11)
        && buf[1] == idx
        && buf[2] == 0x00
        && buf[3] == 0x1a
        && buf[6] == 0x88
}

/// ROOT GetFeature reply → (index, version). Index 0 is ROOT itself, so any
/// other feature reported there is absent, whatever type/version bytes follow.
fn parse_get_feature(resp: &[u8; 20], feature_id: u16) -> Option<(u8, u8)> {
    // params at [4]: feature_index, type, version
    let idx = resp[4];
    (idx != 0 || feature_id == FEAT_ROOT).then_some((idx, resp[6]))
}

struct SysBattery {
    percent: u8,
    charging: bool,
    /// hid-logitech-hidpp clears `online` when the mouse link drops.
    online: bool,
}

/// Battery + link state from the kernel `hidpp_battery` power_supply of the
/// PRO 2 itself (the one whose HID device is 046d:40a8; every other Logitech
/// device has its own).
fn read_sysfs_battery() -> Option<SysBattery> {
    let rd = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for ent in rd.flatten() {
        let uevent = std::fs::read_to_string(ent.path().join("device/uevent")).unwrap_or_default();
        if is_pro2_hid(&uevent) {
            if let Ok(cap) = std::fs::read_to_string(ent.path().join("capacity")) {
                if let Ok(v) = cap.trim().parse() {
                    let status =
                        std::fs::read_to_string(ent.path().join("status")).unwrap_or_default();
                    // sysfs: "Charging" / "Discharging" / "Full" / "Unknown".
                    let charging = status.trim().eq_ignore_ascii_case("charging");
                    let online = std::fs::read_to_string(ent.path().join("online"))
                        .map(|s| s.trim() != "0")
                        .unwrap_or(true);
                    return Some(SysBattery {
                        percent: v,
                        charging,
                        online,
                    });
                }
            }
        }
    }
    None
}

/// `HID_ID=0003:0000046D:000040A8` in a HID device's uevent.
fn is_pro2_hid(uevent: &str) -> bool {
    uevent
        .lines()
        .filter_map(|l| l.strip_prefix("HID_ID="))
        .any(|id| {
            let p: Vec<_> = id.split(':').collect();
            let hex = |i: usize| p.get(i).and_then(|v| u16::from_str_radix(v, 16).ok());
            hex(1) == Some(RX_VID) && hex(2) == Some(MOUSE_PID)
        })
}

/// Lighting defaults only: DPI and report rate come from the mouse (a
/// made-up value would show as live while reads fail).
fn defaults(s: &mut SettingsMap) {
    for (k, v) in [
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::Speed, SettingValue::Text("medium".into())),
        (SettingKey::ColorR, SettingValue::Int(0)),
        (SettingKey::ColorG, SettingValue::Int(200)),
        (SettingKey::ColorB, SettingValue::Int(80)),
    ] {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

fn controls(
    has_rgb: bool,
    effect: MouseEffectKind,
    supported: &std::collections::HashMap<u16, u8>,
    rr_hz: &[u32],
) -> Vec<SettingMeta> {
    use SettingKey::*;
    // Device-advertised rates; the full table until the mouse has been probed.
    let rates = if rr_hz.is_empty() {
        &EXT_RR_HZ[..]
    } else {
        rr_hz
    };
    let mut c = vec![
        SettingMeta::slider(Dpi, "DPI", 100.0, 32000.0, 50.0).unit("dpi"),
        // Discrete choices only — the sensor rejects in-between report rates.
        SettingMeta::dropdown(
            ReportRate,
            "Report rate",
            rates
                .iter()
                .map(|&hz| SettingOption::int(hz as i64, &format!("{hz} Hz")))
                .collect(),
        ),
    ];
    if has_rgb {
        // Only offer effects the device actually enumerated (by effect id).
        let effect_opts: Vec<SettingOption> = [
            (MouseEffectKind::Off, "off", "Off"),
            (MouseEffectKind::Static, "static", "Static"),
            (MouseEffectKind::Pulse, "pulse", "Pulse"),
            (MouseEffectKind::Cycle, "cycle", "Cycle"),
            (MouseEffectKind::Breathe, "breathe", "Breathe"),
        ]
        .into_iter()
        .filter(|(k, _, _)| supported.contains_key(&k.effect_id()))
        .map(|(_, v, l)| SettingOption::text(v, l))
        .collect();
        c.push(SettingMeta::dropdown(Effect, "Effect", effect_opts));
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
    c.extend(super::device::memory_placeholders(Some(BLOCK_SAVE), None));
    c
}

impl DeviceAdapter for LogitechMouseAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Mouse,
            name: "Logitech PRO 2 LIGHTSPEED".into(),
            connected: self.dev.is_some(),
            batteries: self.batteries.clone(),
            settings: self.settings.clone(),
            controls: controls(
                self.has_rgb,
                MouseEffectKind::from_str(
                    get_setting(&self.settings, SettingKey::Effect)
                        .and_then(|v| v.as_str())
                        .unwrap_or("static"),
                ),
                &self.rgb_slots,
                &self.rr_hz,
            ),
            last_error: self.last_error.clone().or_else(|| self.sync_error.clone()),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if !self.online {
            bail!("Logitech PRO 2 is asleep or switched off");
        }
        if !self.index_known {
            bail!("Logitech PRO 2 has not answered yet (asleep?)");
        }
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value.clone());
        let result = match key {
            SettingKey::Dpi => {
                let dpi = value.as_int().unwrap_or(1600) as u16;
                self.set_dpi(dpi)
            }
            SettingKey::ReportRate => {
                let hz = value.as_int().unwrap_or(1000) as u32;
                self.set_report_rate(hz)
            }
            SettingKey::Effect
            | SettingKey::Speed
            | SettingKey::ColorR
            | SettingKey::ColorG
            | SettingKey::ColorB => {
                if !self.has_rgb {
                    Ok(())
                } else {
                    self.apply_lighting()
                }
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            // The device kept its old value; so do the UI and the file.
            self.settings = before;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        self.persist()
            .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        // sysfs reads are cached kernel state (no device I/O), cheap per tick.
        let online = read_sysfs_battery().map(|b| b.online).unwrap_or(true);
        let came_up = online && !self.online;
        self.online = online;
        // Anything still unknown is looked up again, at most every 30 s.
        let incomplete = !self.index_known
            || self.probe_incomplete
            || (self.has_rgb && self.rgb_slots.is_empty());
        if came_up || (online && incomplete && self.last_probe.elapsed() > Duration::from_secs(30))
        {
            info!("Logitech PRO 2 link up; re-initialising");
            self.init_link();
        }
        if self.last_bat.elapsed() > Duration::from_secs(15) {
            self.update_battery();
        }
        if online && self.index_known && self.last_sync.elapsed() > Duration::from_secs(8) {
            self.sync();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

/// One-time migration: fold the legacy on/off toggle into the effect dropdown.
fn migrate_mouse_effect(settings: &mut SettingsMap) {
    if get_setting(settings, SettingKey::Effect).is_none() {
        let enabled = settings
            .get("mouse_rgb_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let effect = if enabled { "static" } else { "off" };
        merge_setting(
            settings,
            SettingKey::Effect,
            SettingValue::Text(effect.into()),
        );
    }
    settings.remove("mouse_rgb_enabled");
}

#[cfg(test)]
mod effect_tests {
    use super::*;
    use hako_core::catalog::{SettingValue, SettingsMap};

    #[test]
    fn battery_belongs_to_the_pro2_hid_device() {
        assert!(is_pro2_hid(
            "DRIVER=logitech-hidpp-device\nHID_ID=0003:0000046D:000040A8\n"
        ));
        // A G915 keyboard's battery on the same machine.
        assert!(!is_pro2_hid("HID_ID=0003:0000046D:0000408E\n"));
        assert!(!is_pro2_hid(""));
    }

    #[test]
    fn migrates_rgb_enabled_to_effect() {
        let mut off: SettingsMap = SettingsMap::new();
        off.insert("mouse_rgb_enabled".into(), SettingValue::Bool(false));
        migrate_mouse_effect(&mut off);
        assert_eq!(off.get("effect"), Some(&SettingValue::Text("off".into())));
        assert!(!off.contains_key("mouse_rgb_enabled"));

        let mut on: SettingsMap = SettingsMap::new();
        on.insert("mouse_rgb_enabled".into(), SettingValue::Bool(true));
        migrate_mouse_effect(&mut on);
        assert_eq!(on.get("effect"), Some(&SettingValue::Text("static".into())));

        // Idempotent: existing mouse_effect is preserved.
        let mut existing: SettingsMap = SettingsMap::new();
        existing.insert("effect".into(), SettingValue::Text("pulse".into()));
        existing.insert("mouse_rgb_enabled".into(), SettingValue::Bool(false));
        migrate_mouse_effect(&mut existing);
        assert_eq!(
            existing.get("effect"),
            Some(&SettingValue::Text("pulse".into()))
        );
        assert!(!existing.contains_key("mouse_rgb_enabled"));
    }

    #[test]
    fn save_to_device_is_shown_blocked() {
        let c = controls(true, MouseEffectKind::Static, &Default::default(), &[]);
        let save = c
            .iter()
            .find(|m| m.key == SettingKey::SaveToDevice)
            .unwrap();
        assert!(save.persistent && save.blocked.is_some());
        // Nothing the owner can turn on, so the worker adds no opt-in toggle.
        assert!(!c.iter().any(|m| m.persistent && m.blocked.is_none()));
    }

    #[test]
    fn index_discovery_takes_only_the_ping_echo() {
        // 11 02 00 1a <protocol 4.2> 88: slot 2's echo.
        let echo = [0x11, 0x02, 0x00, 0x1a, 0x04, 0x02, 0x88, 0, 0, 0];
        assert!(is_ping_echo(&echo, 2));
        assert!(!is_ping_echo(&echo, 1));
        // HID++ 1.0 error from the receiver (slot empty / device offline).
        assert!(!is_ping_echo(
            &[0x10, 0x02, 0x8f, 0x00, 0x1a, 0x09, 0x00],
            2
        ));
        // HID++ 2.0 error frame.
        assert!(!is_ping_echo(
            &[0x11, 0x02, 0xff, 0x00, 0x1a, 0x05, 0x00],
            2
        ));
        // Another feature's notification on the same index.
        assert!(!is_ping_echo(
            &[0x11, 0x02, 0x04, 0x00, 0x01, 0x02, 0x88],
            2
        ));
        // Someone else's ping (other software id) or a short frame.
        assert!(!is_ping_echo(
            &[0x11, 0x02, 0x00, 0x11, 0x04, 0x02, 0x88],
            2
        ));
        assert!(!is_ping_echo(&[0x11, 0x02, 0x00, 0x1a], 2));
    }

    #[test]
    fn feature_at_index_zero_is_absent() {
        let reply = |idx: u8, ty: u8, ver: u8| {
            let mut r = [0u8; 20];
            r[..7].copy_from_slice(&[0x11, 1, 0x00, 0x0a, idx, ty, ver]);
            r
        };
        assert_eq!(
            parse_get_feature(&reply(0x0a, 0, 1), FEAT_EXT_DPI),
            Some((0x0a, 1))
        );
        assert_eq!(parse_get_feature(&reply(0, 0, 0), FEAT_EXT_DPI), None);
        // Nonzero type/version bytes do not turn index 0 (ROOT) into the feature.
        assert_eq!(parse_get_feature(&reply(0, 0x20, 2), FEAT_ONBOARD), None);
        assert_eq!(parse_get_feature(&reply(0, 0, 2), FEAT_ROOT), Some((0, 2)));
    }

    #[test]
    fn dpi_and_rate_are_never_made_up() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        assert_eq!(get_setting(&s, SettingKey::Dpi), None);
        assert_eq!(get_setting(&s, SettingKey::ReportRate), None);
        assert!(get_setting(&s, SettingKey::Effect).is_some());
    }

    /// The pre-refactor inline packet builder of `hidpp_long`, kept verbatim
    /// to prove the shared `hidpp::long_request` sends the same bytes.
    fn legacy_packet(device_index: u8, feature_idx: u8, fnid: u8, params: &[u8]) -> [u8; 20] {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x11;
        pkt[1] = device_index;
        pkt[2] = feature_idx;
        pkt[3] = (fnid << 4) | 0x0a;
        let n = params.len().min(16);
        pkt[4..4 + n].copy_from_slice(&params[..n]);
        pkt
    }

    #[test]
    fn pro2_wire_bytes_unchanged() {
        // Every request shape the PRO 2 path sends (ping, GetFeature,
        // FEATURE_SET, DPI/rate read+write, host mode, RGB claim + effect).
        let payload = mouse_effect_payload(0, 2, MouseEffectKind::Breathe, 9, 8, 7, "slow");
        let calls: [(u8, u8, &[u8]); 12] = [
            (0x00, 0x01, &[0x00, 0x00, 0x88]),
            (0x00, 0x00, &[0x22, 0x02, 0x00]),
            (0x01, 0x00, &[]),
            (0x01, 0x01, &[0x05]),
            (0x0a, 0x05, &[0x00]),
            (0x0a, 0x06, &[0x00, 0x06, 0x40, 0x06, 0x40, 0x02]),
            (0x0b, 0x01, &[]),
            (0x0b, 0x03, &[0x03]),
            (0x0c, 0x01, &[0x02]),
            (0x0d, 0x06, &[0x05]),
            (0x0e, 0x05, &[0x01, 0x03, 0x04]),
            (0x0e, 0x01, &payload),
        ];
        for idx in [1u8, 2, 6] {
            for (feat, fnid, params) in calls {
                assert_eq!(
                    hidpp::long_request(idx, feat, fnid, params),
                    legacy_packet(idx, feat, fnid, params)
                );
            }
        }
        // Report-rate flag decoding matches the old inline filter.
        for flags in 0..=u8::MAX {
            let legacy: Vec<u32> = [(8u8, 125u32), (4, 250), (2, 500), (1, 1000)]
                .into_iter()
                .filter(|(ms, _)| flags & (1 << (ms - 1)) != 0)
                .map(|(_, hz)| hz)
                .collect();
            assert_eq!(rr_supported(flags), legacy);
        }
    }
}
