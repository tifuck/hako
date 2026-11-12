//! Logitech "Centurion" headsets: PRO X 2 LIGHTSPEED (`0af7`), PRO X 3
//! LIGHTSPEED (`0b45`), G522 LIGHTSPEED (`0b18` dongle, `0b19` USB cable).
//!
//! The dongle speaks HID++-like requests in 64-byte frames on its usage page
//! 0xFFA0 node; the headset sits behind a bridge feature (0x0003) and is
//! reached through bridge messages. Feature indexes are looked up by id.
//!
//! A headset on its USB cable (G522 `0b19`) has no bridge: it answers direct
//! requests itself, in 0x50 frames whose address byte is unknown until
//! probed (Root fn 1 to each address 0x00..=0xFF, 5 ms each; the reply
//! carries the address at byte 1). Sources: Solaar `base.py`
//! `probe_centurion_device_addr` and `centurion.py`
//! `create_centurion_receiver` (no bridge = direct device), OpenRGB
//! `LogitechHIDPP20Controller.cpp` `DiscoverTransport` (same sweep); PID
//! `0b19` from Solaar `descriptors.py` and OpenRGB
//! `LOGITECH_G522_LIGHTSPEED_USB_PID`.
//!
//! Layouts reimplemented from HeadsetControl (no code copied):
//! `lib/devices/protocols/logitech_centurion_protocol.hpp` (framing, bridge,
//! lookup, battery, auto-sleep), `logitech_gpro_x2_lightspeed.hpp`,
//! `logitech_gpro_x3_lightspeed.hpp`, `logitech_g522_lightspeed.hpp`
//! (per-model prefix, sidetone, voice prompts, mic mute LED). Solaar
//! `docs/devices/PRO X 2 LIGHTSPEED 0AF7.text` confirms the Centurion
//! transport and feature ids on the PRO X 2.
//!
//! - Frame: `prefix, len, flags, payload` where `len` = payload + 1.
//! - Direct request payload: `[feature index, fn << 4 | swid, params..]`.
//! - Bridge request: direct request to the bridge, fn 1, params
//!   `[size hi (4 bits), size lo]` + sub-message `[0, index, fn << 4 | swid,
//!   params..]`. The bridge acks (swid echoed), then the headset's answer
//!   arrives as a bridge event (swid 0): `[bridge, fn, .., .., 0, index,
//!   ..]`, params from byte 7; index 0xFF there is an error.
//! - Bridge connection event (fn 0, swid 0): byte 3 = headset connected.
//!
//! Not implemented: the equalizers, shown as blocked onboard-memory
//! placeholders (PRO X 2 writes an *onboard* EQ slot, G522 / PRO X 3 layouts
//! are single-source or unverified upstream; see
//! `docs/onboard-writes/logitech.md`), G522 lights
//! (fixed on/off parameters only), the PRO X 2 legacy 0x51 0x08 battery
//! fallback (only for firmware without battery feature 0x0104).

use super::device::VID;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const FRAME: usize = 64;
const SWID: u8 = 0x01;
const MAX_BRIDGE_MESSAGE: usize = 56;
const POLL_EVERY: Duration = Duration::from_secs(15);

const FEAT_ROOT: u16 = 0x0000;
const FEAT_BRIDGE: u16 = 0x0003;
const FEAT_BATTERY: u16 = 0x0104;
const FEAT_AUTO_SLEEP: u16 = 0x0108;
const FEAT_MIC_MUTE_LED: u16 = 0x0601;
const FEAT_SIDETONE: u16 = 0x0604;
const FEAT_VOICE_PROMPTS: u16 = 0x060b;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoSleep {
    /// PRO X 2: `[minutes]`, 0..=255.
    Minutes,
    /// Read `[minutes, lights dim, lights off]`, write it back with new
    /// minutes (the headset rejects a write without the light timers), 0..=90.
    WithLightTimers,
}

#[derive(Debug)]
pub struct Spec {
    pid: u16,
    /// The same headset on its USB cable (direct, address probed).
    wired_pid: Option<u16>,
    prefix: &'static [u8],
    /// Sidetone device range 0..=max.
    sidetone_max: Option<u8>,
    /// Sidetone read reply carries the level at params\[3\] (G522).
    sidetone_readable: bool,
    auto_sleep: AutoSleep,
    voice_prompts: bool,
    mic_led: bool,
    /// Why its (onboard) equalizer write is not offered.
    eq_block: &'static str,
}

const SPECS: &[Spec] = &[
    Spec {
        pid: 0x0af7,
        wired_pid: None,
        prefix: &[0x51],
        sidetone_max: Some(100),
        sidetone_readable: false,
        auto_sleep: AutoSleep::Minutes,
        voice_prompts: false,
        mic_led: false,
        eq_block: "onboard EQ slot write: single source (HeadsetControl), host-computed \
                   filter coefficients and undocumented constants, slot fixed at 0",
    },
    Spec {
        pid: 0x0b45,
        wired_pid: None,
        prefix: &[0x50, 0x23],
        sidetone_max: None,
        sidetone_readable: false,
        auto_sleep: AutoSleep::WithLightTimers,
        voice_prompts: false,
        mic_led: false,
        eq_block: "EQ payload not verified upstream (HeadsetControl omits it)",
    },
    Spec {
        pid: 0x0b18,
        wired_pid: Some(0x0b19),
        prefix: &[0x50, 0x23],
        sidetone_max: Some(9),
        sidetone_readable: true,
        auto_sleep: AutoSleep::WithLightTimers,
        voice_prompts: true,
        mic_led: true,
        eq_block: "EQ (feature 0x020d): single source (HeadsetControl); whether it is stored \
                   on the headset is unknown",
    },
];

pub fn spec(pid: u16) -> Option<&'static Spec> {
    SPECS
        .iter()
        .find(|s| s.pid == pid || s.wired_pid == Some(pid))
}

/// Report id of addressed (0x50) frames; a wired headset's address follows.
const ADDRESSED: u8 = 0x50;
const PROBE_WAIT: Duration = Duration::from_millis(5);

// ── Framing ──────────────────────────────────────────────────────────────────

fn frame(prefix: &[u8], payload: &[u8]) -> [u8; FRAME] {
    let mut f = [0u8; FRAME];
    let h = prefix.len();
    f[..h].copy_from_slice(prefix);
    f[h] = (payload.len() + 1) as u8;
    f[h + 1] = 0x00;
    let n = payload.len().min(FRAME - h - 2);
    f[h + 2..h + 2 + n].copy_from_slice(&payload[..n]);
    f
}

/// Payload of a frame (after length and flags), bounded by its length byte.
fn payload<'a>(prefix: &[u8], f: &'a [u8]) -> Option<&'a [u8]> {
    let h = prefix.len();
    if f.len() < h + 3 || &f[..h] != prefix {
        return None;
    }
    let len = usize::from(f[h]);
    (len > 1 && h + 1 + len <= f.len()).then(|| &f[h + 2..h + 1 + len])
}

fn direct_payload(index: u8, function: u8, params: &[u8]) -> Vec<u8> {
    let mut p = vec![index, (function & 0xf0) | SWID];
    p.extend_from_slice(params);
    p
}

/// Bridge send (fn 1) carrying a sub-message for headset feature `index`.
fn bridge_payload(bridge: u8, index: u8, function: u8, params: &[u8]) -> Vec<u8> {
    let mut sub = vec![0x00, index, (function & 0xf0) | SWID];
    sub.extend_from_slice(params);
    let n = sub.len();
    let mut p = vec![bridge, 0x10 | SWID, ((n >> 8) & 0x0f) as u8, n as u8];
    p.extend(sub);
    p
}

#[derive(Debug, PartialEq, Eq)]
enum BridgeReply {
    /// Not for us (another feature, notification, input).
    Other,
    Ack,
    /// Headset connected (true) / disconnected (false).
    Connection(bool),
    /// The bridge refused the request.
    Rejected,
    Answer(Vec<u8>),
    /// The headset answered with an error.
    Error,
}

fn classify_bridge(reply: &[u8], bridge: u8, index: u8) -> BridgeReply {
    if reply.len() >= 4 && reply[0] == 0xff && reply[1] == bridge && reply[2] == 0x10 | SWID {
        return BridgeReply::Rejected;
    }
    if reply.len() < 2 || reply[0] != bridge {
        return BridgeReply::Other;
    }
    if reply[1] == 0x00 && reply.len() >= 4 {
        return BridgeReply::Connection(reply[3] != 0);
    }
    if reply[1] >> 4 != 0x1 {
        return BridgeReply::Other;
    }
    match reply[1] & 0x0f {
        SWID => BridgeReply::Ack,
        0 if reply.len() >= 7 && reply[4] == 0x00 => {
            if reply[5] == index {
                BridgeReply::Answer(reply[7..].to_vec())
            } else if reply[5] == 0xff && reply[6] == index {
                BridgeReply::Error
            } else {
                BridgeReply::Other
            }
        }
        _ => BridgeReply::Other,
    }
}

/// 0x0104 fn 0: `[percent, _, charging state]`; 1 or 2 = charging.
fn parse_battery(p: &[u8]) -> Option<(u8, bool)> {
    let level = *p.first()?;
    (level <= 100).then(|| (level, matches!(p.get(2), Some(1 | 2))))
}

/// PRO X 2 sidetone write: version-2 firmware (state reply of 4+ bytes)
/// takes `[mic 1, 0xff, level]`, older `[mic 1, level]`. G522 always v2.
fn sidetone_params(v2: bool, level: u8) -> Vec<u8> {
    if v2 {
        vec![0x01, 0xff, level]
    } else {
        vec![0x01, level]
    }
}

// ── Adapter ──────────────────────────────────────────────────────────────────

pub fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let spec = spec(pid).context("not a Centurion headset")?;
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == VID && d.product_id == pid)
        .filter(|d| crate::hid::usage_pages(&d.report_descriptor()).contains(&0xffa0))
        .collect();
    nodes.sort_by_key(|d| d.interface);
    let info = nodes.first().context("no Centurion (0xFFA0) interface")?;
    let mut dev = HidrawDevice::open(&info.path)?;
    let wired = if pid == spec.pid {
        None
    } else {
        Some(probe_address(&mut dev).context("headset did not answer")?)
    };
    Ok(vec![Box::new(CenturionHeadset::new(
        spec,
        dev,
        wired,
        ctx.paths.clone(),
    )?)])
}

/// Root fn 1 (ping) to each 0x50 address until one answers.
fn probe_address(dev: &mut HidrawDevice) -> Option<u8> {
    dev.drain(Duration::from_millis(1));
    let mut buf = [0u8; FRAME];
    (0..=255u8).find_map(|addr| {
        let f = frame(&[ADDRESSED, addr], &direct_payload(0x00, 0x10, &[0, 0, 0]));
        dev.write(&f).ok()?;
        let n = dev.read_timeout(&mut buf, PROBE_WAIT).ok()?;
        (n >= 2 && buf[0] == ADDRESSED).then_some(buf[1])
    })
}

struct CenturionHeadset {
    id: DeviceId,
    spec: &'static Spec,
    name: &'static str,
    dev: Option<HidrawDevice>,
    /// Frame prefix; a wired headset's carries its probed address.
    prefix: Vec<u8>,
    /// Through the dongle's bridge (false: wired, direct requests).
    bridged: bool,
    paths: ConfigPaths,
    bridge: Option<u8>,
    /// Feature id → headset feature index (None = not supported).
    feats: HashMap<u16, Option<u8>>,
    settings: SettingsMap,
    chosen: SettingsMap,
    battery: BatteryState,
    online: bool,
    last_poll: Instant,
    last_error: Option<String>,
}

impl CenturionHeadset {
    /// `wired`: the probed address of a headset on its cable.
    fn new(
        spec: &'static Spec,
        dev: HidrawDevice,
        wired: Option<u8>,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let chosen = hako_core::config::load_device_settings(&paths, VID, spec.pid)?;
        let name = super::MODELS
            .iter()
            .find(|m| m.product_ids.contains(&spec.pid))
            .map_or("Logitech headset", |m| m.name);
        let mut a = Self {
            id: DeviceId {
                vendor_id: VID,
                product_id: spec.pid,
                instance: "centurion".into(),
            },
            spec,
            name,
            dev: Some(dev),
            prefix: wired.map_or_else(|| spec.prefix.to_vec(), |a| vec![ADDRESSED, a]),
            bridged: wired.is_none(),
            paths,
            bridge: None,
            feats: HashMap::new(),
            settings: chosen.clone(),
            chosen,
            battery: BatteryState {
                label: "Headset".into(),
                percent: None,
                charging: false,
                present: true,
            },
            online: false,
            last_poll: Instant::now(),
            last_error: None,
        };
        a.poll();
        info!("{} open online={}", a.name, a.online);
        Ok(a)
    }

    fn dev(&mut self) -> Result<&mut HidrawDevice> {
        self.dev.as_mut().context("device closed")
    }

    fn read(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let prefix = self.prefix.clone();
        let dev = self.dev()?;
        let mut buf = [0u8; FRAME];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("Centurion timeout");
            }
            if let Ok(n) = dev.read_timeout(&mut buf, left) {
                if let Some(p) = payload(&prefix, &buf[..n]) {
                    return Ok(p.to_vec());
                }
            }
        }
    }

    /// Direct request to a dongle feature; the reply must echo index + fn.
    fn direct(&mut self, index: u8, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        let f = frame(&self.prefix, &direct_payload(index, function, params));
        let dev = self.dev()?;
        dev.drain(Duration::from_millis(1));
        dev.write(&f)?;
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let r = self.read(deadline)?;
            if r.len() >= 2 && r[0] == index && r[1] == (function & 0xf0) | SWID {
                return Ok(r[2..].to_vec());
            }
        }
    }

    /// Request to headset feature `index` through the bridge.
    fn bridged(&mut self, index: u8, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        let bridge = self.bridge.context("bridge not found")?;
        let p = bridge_payload(bridge, index, function, params);
        if p.len() - 4 > MAX_BRIDGE_MESSAGE {
            bail!("Centurion message too long");
        }
        let f = frame(&self.prefix, &p);
        let dev = self.dev()?;
        dev.drain(Duration::from_millis(1));
        dev.write(&f)?;
        let deadline = Instant::now() + Duration::from_millis(800);
        loop {
            match classify_bridge(&self.read(deadline)?, bridge, index) {
                BridgeReply::Answer(params) => return Ok(params),
                BridgeReply::Connection(false) => {
                    self.online = false;
                    bail!("headset is off or out of range");
                }
                BridgeReply::Rejected => bail!("bridge rejected the request (headset off?)"),
                BridgeReply::Error => bail!("headset rejected the request"),
                BridgeReply::Other | BridgeReply::Ack | BridgeReply::Connection(true) => {}
            }
        }
    }

    /// Headset feature index of `fid` (Root.getFeature, through the bridge
    /// unless wired).
    fn feature(&mut self, fid: u16) -> Result<u8> {
        if !self.bridged {
            if let Some(&known) = self.feats.get(&fid) {
                return known.context("feature not supported");
            }
            let r = self.direct(0x00, 0x00, &fid.to_be_bytes())?;
            let idx = r.first().copied().filter(|&i| i != 0 || fid == FEAT_ROOT);
            self.feats.insert(fid, idx);
            return idx.context("feature not supported");
        }
        if self.bridge.is_none() {
            // Root (index 0) fn 0 [id] on the dongle → [index, flags, version].
            let r = self.direct(0x00, 0x00, &FEAT_BRIDGE.to_be_bytes())?;
            match r.first() {
                Some(&i) if i != 0 => self.bridge = Some(i),
                _ => bail!("no Centurion bridge"),
            }
        }
        if let Some(&known) = self.feats.get(&fid) {
            return known.context("feature not supported");
        }
        let r = self.bridged(0x00, 0x00, &fid.to_be_bytes())?;
        let idx = r.first().copied().filter(|&i| i != 0 || fid == FEAT_ROOT);
        self.feats.insert(fid, idx);
        idx.context("feature not supported")
    }

    fn call(&mut self, fid: u16, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        let idx = self.feature(fid)?;
        if self.bridged {
            self.bridged(idx, function, params)
        } else {
            self.direct(idx, function, params)
        }
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        let was = self.online;
        match self
            .call(FEAT_BATTERY, 0x00, &[])
            .map(|p| parse_battery(&p))
        {
            Ok(Some((pct, charging))) => {
                self.online = true;
                self.battery.percent = Some(pct);
                self.battery.charging = charging;
            }
            Ok(None) | Err(_) => {
                // On its cable the headset is there whenever its node is.
                self.online = !self.bridged;
                self.battery.percent = None;
                self.battery.charging = false;
            }
        }
        if !self.online {
            return;
        }
        if !was {
            self.restore();
        }
        self.sync();
    }

    /// Put back what the user chose (the headset may have been power-cycled).
    fn restore(&mut self) {
        for key in [
            SettingKey::MicSideTone,
            SettingKey::PmShutdown,
            SettingKey::VoicePrompts,
            SettingKey::MicLedBrightness,
        ] {
            if let Some(v) = get_setting(&self.chosen, key).cloned() {
                if self.controls().iter().any(|c| c.key == key) {
                    if let Err(e) = self.write(key, &v) {
                        warn!("{}: restore {}: {e}", self.name, key.as_str());
                    }
                }
            }
        }
    }

    fn sync(&mut self) {
        if self.spec.sidetone_readable {
            // G522: [mic id, ?, ?, level 0..=9].
            if let Ok(p) = self.call(FEAT_SIDETONE, 0x00, &[]) {
                if let Some(&l) = p.get(3).filter(|&&l| Some(l) <= self.spec.sidetone_max) {
                    merge_setting(
                        &mut self.settings,
                        SettingKey::MicSideTone,
                        SettingValue::Int(l.into()),
                    );
                }
            }
        }
        if let Ok(p) = self.call(FEAT_AUTO_SLEEP, 0x00, &[]) {
            if let Some(&m) = p.first() {
                merge_setting(
                    &mut self.settings,
                    SettingKey::PmShutdown,
                    SettingValue::Int(m.into()),
                );
            }
        }
    }

    fn write(&mut self, key: SettingKey, value: &SettingValue) -> Result<()> {
        let n = value
            .as_int()
            .or_else(|| value.as_bool().map(i64::from))
            .context("expected a number")? as u8;
        match key {
            SettingKey::MicSideTone => {
                let v2 = if self.spec.pid == 0x0af7 {
                    self.call(FEAT_SIDETONE, 0x00, &[])?.len() >= 4
                } else {
                    true
                };
                self.call(FEAT_SIDETONE, 0x10, &sidetone_params(v2, n))?;
            }
            SettingKey::PmShutdown => {
                let params = match self.spec.auto_sleep {
                    AutoSleep::Minutes => vec![n],
                    AutoSleep::WithLightTimers => {
                        let cur = self.call(FEAT_AUTO_SLEEP, 0x00, &[])?;
                        if cur.len() < 3 {
                            bail!("unexpected auto-sleep reply");
                        }
                        vec![n, cur[1], cur[2]]
                    }
                };
                self.call(FEAT_AUTO_SLEEP, 0x10, &params)?;
            }
            SettingKey::VoicePrompts => {
                self.call(FEAT_VOICE_PROMPTS, 0x50, &[0x00, u8::from(n != 0)])?;
            }
            SettingKey::MicLedBrightness => {
                self.call(FEAT_MIC_MUTE_LED, 0x20, &[u8::from(n != 0)])?;
            }
            _ => bail!("{} has no {} control", self.name, key.as_str()),
        }
        Ok(())
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        let mut c = Vec::new();
        if let Some(max) = self.spec.sidetone_max {
            c.push(SettingMeta::slider(
                MicSideTone,
                "Sidetone",
                0.0,
                max.into(),
                1.0,
            ));
        }
        let max = match self.spec.auto_sleep {
            AutoSleep::Minutes => 255.0,
            AutoSleep::WithLightTimers => 90.0,
        };
        c.push(SettingMeta::slider(PmShutdown, "Auto-off", 0.0, max, 1.0).unit("min"));
        if self.spec.voice_prompts {
            c.push(SettingMeta::toggle(VoicePrompts, "Voice prompts"));
        }
        if self.spec.mic_led {
            c.push(SettingMeta::dropdown(
                MicLedBrightness,
                "Mute LED",
                vec![SettingOption::int(0, "Off"), SettingOption::int(1, "On")],
            ));
        }
        c.extend(super::device::memory_placeholders(
            None,
            Some(self.spec.eq_block),
        ));
        c
    }
}

impl DeviceAdapter for CenturionHeadset {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if !self.online {
            bail!("{} is off or out of range", self.name);
        }
        if let Err(e) = self.write(key, &value) {
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        merge_setting(&mut self.settings, key, value.clone());
        merge_setting(&mut self.chosen, key, value);
        hako_core::config::save_device_settings(&self.paths, VID, self.id.product_id, &self.chosen)
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_some() && self.last_poll.elapsed() >= POLL_EVERY {
            self.poll();
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
    fn frames_follow_headsetcontrol_layout() {
        // PRO X 2: Root.getFeature(bridge 0x0003) on the dongle.
        let f = frame(&[0x51], &direct_payload(0x00, 0x00, &[0x00, 0x03]));
        assert_eq!(&f[..7], &[0x51, 0x05, 0x00, 0x00, 0x01, 0x00, 0x03]);
        assert!(f[7..].iter().all(|&b| b == 0));
        assert_eq!(f.len(), 64);
        // G522 prefix 50 23, battery (headset index 4) through bridge 2.
        let p = bridge_payload(0x02, 0x04, 0x00, &[]);
        assert_eq!(p, [0x02, 0x11, 0x00, 0x03, 0x00, 0x04, 0x01]);
        let f = frame(&[0x50, 0x23], &p);
        assert_eq!(
            &f[..11],
            &[0x50, 0x23, 0x08, 0x00, 0x02, 0x11, 0x00, 0x03, 0x00, 0x04, 0x01]
        );
        // Sidetone write fn 1 (0x10) carries [mic 1, 0xff, level].
        assert_eq!(
            bridge_payload(0x02, 0x07, 0x10, &sidetone_params(true, 5)),
            [0x02, 0x11, 0x00, 0x06, 0x00, 0x07, 0x11, 0x01, 0xff, 0x05]
        );
        assert_eq!(sidetone_params(false, 80), [0x01, 80]);
    }

    #[test]
    fn payload_respects_length_byte() {
        let mut f = [0u8; 64];
        f[..6].copy_from_slice(&[0x51, 0x04, 0x00, 0x02, 0x11, 0x00]);
        assert_eq!(payload(&[0x51], &f), Some(&[0x02, 0x11, 0x00][..]));
        assert_eq!(payload(&[0x50, 0x23], &f), None);
        f[1] = 0x01;
        assert_eq!(payload(&[0x51], &f), None);
    }

    #[test]
    fn bridge_replies() {
        // Ack of our send (swid 1), then the answer event (swid 0).
        assert_eq!(classify_bridge(&[0x02, 0x11, 0, 3], 2, 4), BridgeReply::Ack);
        assert_eq!(
            classify_bridge(&[0x02, 0x10, 0x00, 0x06, 0x00, 0x04, 0x01, 87, 0, 1], 2, 4),
            BridgeReply::Answer(vec![87, 0, 1])
        );
        // Answer for another feature; headset error; bridge refusal.
        assert_eq!(
            classify_bridge(&[0x02, 0x10, 0, 6, 0, 0x05, 1, 9], 2, 4),
            BridgeReply::Other
        );
        assert_eq!(
            classify_bridge(&[0x02, 0x10, 0, 6, 0, 0xff, 0x04, 9], 2, 4),
            BridgeReply::Error
        );
        assert_eq!(
            classify_bridge(&[0xff, 0x02, 0x11, 0x00], 2, 4),
            BridgeReply::Rejected
        );
        // Connection events: [bridge, 0, 0, connected].
        assert_eq!(
            classify_bridge(&[0x02, 0x00, 0x00, 0x00], 2, 4),
            BridgeReply::Connection(false)
        );
        assert_eq!(
            classify_bridge(&[0x02, 0x00, 0x00, 0x01], 2, 4),
            BridgeReply::Connection(true)
        );
        assert_eq!(classify_bridge(&[0x07, 0x10], 2, 4), BridgeReply::Other);
    }

    #[test]
    fn battery_parsing() {
        assert_eq!(parse_battery(&[87, 0, 0]), Some((87, false)));
        assert_eq!(parse_battery(&[50, 0, 1]), Some((50, true)));
        assert_eq!(parse_battery(&[100, 0, 2]), Some((100, true)));
        assert_eq!(parse_battery(&[101, 0, 0]), None);
        assert_eq!(parse_battery(&[]), None);
    }

    #[test]
    fn specs_and_controls() {
        for s in SPECS {
            assert!(super::super::MODELS
                .iter()
                .any(|m| m.product_ids.contains(&s.pid)));
        }
        for s in SPECS {
            let eq = super::super::device::memory_placeholders(None, Some(s.eq_block));
            assert!(eq[0].persistent && eq[0].blocked.as_deref() == Some(s.eq_block));
        }
        assert_eq!(spec(0x0b18).unwrap().prefix, &[0x50, 0x23]);
        // Wired G522 shares the dongle's spec; its address is probed with
        // Root fn 1 in a 0x50 frame (Solaar probe_centurion_device_addr).
        assert_eq!(spec(0x0b19).unwrap().pid, 0x0b18);
        let f = frame(&[ADDRESSED, 0x23], &direct_payload(0x00, 0x10, &[0, 0, 0]));
        assert_eq!(&f[..10], &[0x50, 0x23, 0x06, 0x00, 0x00, 0x11, 0, 0, 0, 0]);
        assert_eq!(spec(0x0af7).unwrap().prefix, &[0x51]);
        assert!(spec(0x0aba).is_none());
    }
}
