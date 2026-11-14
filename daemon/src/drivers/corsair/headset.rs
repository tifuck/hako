//! Headsets: battery (and Void sidetone / lights). Bragi headsets keep
//! sidetone, sleep timer and brightness in onboard memory (HeadsetControl
//! `corsair_virtuoso_xt.hpp`), so their writes (lights, V2 sidetone, sleep
//! timer on the models `onboard.rs` lists) are persistent stores, in
//! software mode as HeadsetControl does: on V2 receivers the receiver is
//! switched too (HeadsetControl `corsair_void_v2w.hpp` `initializeDevice`).
//!
//! V2 sidetone (HeadsetControl `corsair_void_v2w.hpp` `setSidetone`,
//! OpenLinkHub `voidV2W.go` / `virtuosomaxW.go` / `hs80maxW.go`
//! `configureHeadset`): on = ANC 0xD1 = 0 (Void V2 / Virtuoso MAX; the
//! HS80 MAX has none in OpenLinkHub), 0x46 = 0, 0x47 = level x 10; off =
//! 0x46 = 1. Sleep timer (both sources, `setInactiveTime` /
//! `setSleepTimer`): 0x0D = 0 off, or 0x0D = 1 then 0x0E = minutes x 60000.
//!
//! Void family (Void / Void PRO / Void ELITE / HS70 wireless receivers):
//! output report `C9 64` asks for status, answered by input report 0x64:
//! `[0x64, buttons, battery|mic<<7, link (177 = connected), state]`, state
//! 1 normal, 2 low, 3 critical, 4 full, 5 charging. Sidetone is feature
//! report 0xFF `FF 0B 00 FF 04 0E FF 05 01 04 00 <level+200>`, level 0..=55,
//! sent on the 0xFF00 collection of interface 0. Output report `C8 <0 on /
//! 1 off> 00` switches the headset lights; status and lights use the 0xFFC5
//! collection of interface 3. Wired (USB) Voids take the lights command too;
//! their sidetone is a USB-audio mixer control (ALSA), not HID.
//! Sources: Linux `drivers/hid/hid-corsair-void.c`; HeadsetControl
//! `lib/devices/corsair_void_rich.hpp`, `corsair_device.hpp`.

use super::bragi::{
    self, Link, MINUTE_MS, PROP_ANC, PROP_SIDETONE_LEVEL, PROP_SIDETONE_OFF, PROP_SLEEP_ENABLED,
    PROP_SLEEP_TIMEOUT,
};
use super::Backend;
use crate::hid::HidrawDevice;
use anyhow::{bail, Context, Result};
use hako_core::device::BatteryState;
use std::time::{Duration, Instant};

pub const VOID_USAGE_PAGE: u16 = 0xffc5;
pub const VOID_IFACE: u32 = 3;
pub const SIDETONE_USAGE_PAGE: u16 = 0xff00;
pub const SIDETONE_IFACE: u32 = 0;
pub const SIDETONE_MAX: u8 = 55;

const STATUS_REQUEST: [u8; 2] = [0xc9, 0x64];
const STATUS_REPORT: u8 = 0x64;
const LINK_CONNECTED: u8 = 177;

/// Boom mic raised (which mutes it; HeadsetControl reports it as muted)
/// from a Void status report; None when no headset is linked.
pub fn void_mic_up(r: &[u8]) -> Option<bool> {
    parse_void_status(r).map(|_| r[2] & 0x80 != 0)
}

/// Battery from a Void status report; None when no headset is linked.
pub fn parse_void_status(r: &[u8]) -> Option<BatteryState> {
    if r.len() < 5 || r[0] != STATUS_REPORT || r[3] != LINK_CONNECTED || r[4] == 0 {
        return None;
    }
    Some(BatteryState {
        label: "Headset".into(),
        percent: Some((r[2] & 0x7f).min(100)),
        charging: r[4] == 5,
        present: true,
    })
}

/// V2 sidetone writes for `pct` (0 = off); `anc` when the headset has ANC.
pub fn v2_sidetone(pct: u8, anc: bool) -> Vec<(u8, u32)> {
    if pct == 0 {
        return vec![(PROP_SIDETONE_OFF, 1)];
    }
    let mut w = Vec::with_capacity(3);
    if anc {
        w.push((PROP_ANC, 0));
    }
    w.push((PROP_SIDETONE_OFF, 0));
    w.push((PROP_SIDETONE_LEVEL, u32::from(pct.min(100)) * 10));
    w
}

/// Sidetone percent from the stored 0x46 / 0x47 values.
pub fn v2_sidetone_pct(off: u32, level: u32) -> Option<u8> {
    match off {
        1 => Some(0),
        0 if level <= bragi::SIDETONE_LEVEL_MAX => Some((level / 10) as u8),
        _ => None,
    }
}

/// Sleep timer writes for `minutes` (0 = off).
pub fn sleep(minutes: u32) -> Vec<(u8, u32)> {
    if minutes == 0 {
        return vec![(PROP_SLEEP_ENABLED, 0)];
    }
    vec![
        (PROP_SLEEP_ENABLED, 1),
        (PROP_SLEEP_TIMEOUT, minutes.saturating_mul(MINUTE_MS)),
    ]
}

/// Sleep minutes (0 = off) from the stored 0x0D / 0x0E values.
pub fn sleep_minutes(enabled: u32, timeout: u32) -> Option<u32> {
    match enabled {
        0 => Some(0),
        1 if bragi::storable(PROP_SLEEP_TIMEOUT, timeout) => Some(timeout / MINUTE_MS),
        _ => None,
    }
}

pub fn void_sidetone(level: u8) -> [u8; 12] {
    [
        0xff,
        0x0b,
        0x00,
        0xff,
        0x04,
        0x0e,
        0xff,
        0x05,
        0x01,
        0x04,
        0x00,
        level.min(SIDETONE_MAX) + 200,
    ]
}

/// The lights output report: 0x00 turns them on, 0x01 off.
pub fn void_lights(on: bool) -> [u8; 3] {
    [0xc8, u8::from(!on), 0x00]
}

fn absent() -> BatteryState {
    BatteryState {
        label: "Headset".into(),
        percent: None,
        charging: false,
        present: false,
    }
}

pub struct Void {
    /// Status / lights collection.
    dev: HidrawDevice,
    /// Sidetone collection; the status node when it was not found.
    sidetone: Option<HidrawDevice>,
    /// USB headset: no battery, no HID sidetone.
    wired: bool,
    /// From the last status report.
    mic_up: Option<bool>,
}

impl Void {
    pub fn new(dev: HidrawDevice, sidetone: Option<HidrawDevice>, wired: bool) -> Self {
        Self {
            dev,
            sidetone,
            wired,
            mic_up: None,
        }
    }
}

impl Backend for Void {
    fn sidetone(&mut self, level: u8) -> Result<()> {
        if self.wired {
            bail!("wired Void sidetone is an ALSA mixer control");
        }
        self.sidetone
            .as_ref()
            .unwrap_or(&self.dev)
            .set_feature(&mut void_sidetone(level))
    }

    fn lights(&mut self, on: bool) -> Result<()> {
        self.dev.write(&void_lights(on))
    }

    fn battery(&mut self) -> Result<Option<BatteryState>> {
        if self.wired {
            return Ok(None);
        }
        self.dev.drain(Duration::ZERO);
        self.dev.write(&STATUS_REQUEST)?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut buf = [0u8; 64];
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let n = self.dev.read_timeout(&mut buf, left)?;
            if buf[0] == STATUS_REPORT {
                self.mic_up = void_mic_up(&buf[..n]);
                return Ok(Some(parse_void_status(&buf[..n]).unwrap_or_else(absent)));
            }
        }
        bail!("no status report")
    }

    fn mic_up(&self) -> Option<bool> {
        self.mic_up
    }
}

/// Bragi headset behind its receiver (target 0x09) or on a cable (0x08 on
/// the XT, 0x09 on the SE); the first target that answers is kept.
pub struct BragiHeadset {
    link: Link,
    target: Option<u8>,
    /// Void Wireless V2 / Virtuoso MAX / HS80 MAX receivers want a
    /// heartbeat to wake the headset before it answers.
    wake: bool,
}

impl BragiHeadset {
    pub fn new(link: Link, wake: bool) -> Self {
        Self {
            link,
            target: None,
            wake,
        }
    }

    fn level(&mut self, target: u8) -> Result<u32> {
        if self.wake {
            let _ = self.link.get(bragi::SELF, bragi::PROP_APP_VER);
            let _ = self
                .link
                .transact(&bragi::ping(bragi::SELF), Duration::from_millis(200));
            let _ = self
                .link
                .transact(&bragi::ping(target), Duration::from_millis(200));
        }
        let level = self.link.get(target, bragi::PROP_BATTERY_LEVEL)?;
        // Receivers sometimes answer with a non-level value (HeadsetControl).
        if level == 0 || level > 1000 {
            bail!("implausible battery level {level}");
        }
        Ok(level)
    }
}

impl Backend for BragiHeadset {
    fn battery(&mut self) -> Result<Option<BatteryState>> {
        let order = match self.target {
            Some(t) => vec![t],
            None => vec![bragi::SELF | 1, bragi::SELF],
        };
        for t in order {
            if let Ok(level) = self.level(t) {
                self.target = Some(t);
                let status = self.link.get(t, bragi::PROP_BATTERY_STATUS).ok();
                return Ok(Some(bragi::battery(level, status, "Headset")));
            }
        }
        // Receiver present, headset off or out of range.
        self.target = None;
        Ok(Some(absent()))
    }

    fn store(&mut self, writes: &[(u8, u32)]) -> Result<()> {
        let target = self.target.context("headset is off or out of range")?;
        let path: &[u8] = if self.wake && target != bragi::SELF {
            &[bragi::SELF, target]
        } else {
            &[target]
        };
        bragi::store_in_software(&mut self.link, path, writes, false)
    }

    fn stored(&mut self, prop: u8) -> Option<u32> {
        let target = self.target?;
        self.link.get(target, prop).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn void_status_reports() {
        // 80 %, mic up, connected, normal.
        let b = parse_void_status(&[0x64, 0x00, 0x80 | 80, 177, 1]).unwrap();
        assert_eq!((b.percent, b.charging), (Some(80), false));
        // Charging.
        assert!(parse_void_status(&[0x64, 0, 55, 177, 5]).unwrap().charging);
        // Fully charged is not charging.
        assert!(!parse_void_status(&[0x64, 0, 100, 177, 4]).unwrap().charging);
        // Receiver searching (51) or state 0: no headset.
        assert!(parse_void_status(&[0x64, 0, 0, 51, 1]).is_none());
        assert!(parse_void_status(&[0x64, 0, 50, 177, 0]).is_none());
        // Firmware report 0x66 is not a status.
        assert!(parse_void_status(&[0x66, 1, 2, 3, 4]).is_none());
        // Bit 7 of the battery byte: mic raised (Linux CORSAIR_VOID_MIC_MASK).
        assert_eq!(void_mic_up(&[0x64, 0x00, 0x80 | 80, 177, 1]), Some(true));
        assert_eq!(void_mic_up(&[0x64, 0x00, 80, 177, 1]), Some(false));
        assert_eq!(void_mic_up(&[0x64, 0, 0x80, 51, 1]), None);
    }

    #[test]
    fn void_sidetone_packet() {
        assert_eq!(
            void_sidetone(10),
            [0xff, 0x0b, 0x00, 0xff, 0x04, 0x0e, 0xff, 0x05, 0x01, 0x04, 0x00, 210]
        );
        assert_eq!(void_sidetone(200)[11], 255);
    }

    #[test]
    fn v2_writes_match_sources() {
        // HeadsetControl setSidetone: ANC off, 0x46 = 0, then 0x47 LE.
        assert_eq!(v2_sidetone(50, true), [(0xd1, 0), (0x46, 0), (0x47, 500)]);
        assert_eq!(v2_sidetone(100, false), [(0x46, 0), (0x47, 1000)]);
        // Off: OpenLinkHub configureHeadset writes 0x46 = 1 only.
        assert_eq!(v2_sidetone(0, true), [(0x46, 1)]);
        // On the wire (HeadsetControl sidetone_data): 00 02 09 01 47 00 f4 01.
        let w = bragi::wire(&bragi::set(0x09, 0x47, 500), true, true, 64);
        assert_eq!(
            w[..10],
            [0x00, 0x02, 0x09, 0x01, 0x47, 0x00, 0xf4, 0x01, 0, 0]
        );
        assert_eq!(w.len(), 65);
        // setInactiveTime / setSleepTimer: 0x0d then 0x0e in ms LE32.
        assert_eq!(sleep(15), [(0x0d, 1), (0x0e, 900_000)]);
        assert_eq!(sleep(0), [(0x0d, 0)]);
        let w = bragi::wire(&bragi::set(0x09, 0x0e, 90 * 60_000), true, true, 64);
        assert_eq!(
            w[..10],
            [0x00, 0x02, 0x09, 0x01, 0x0e, 0x00, 0xc0, 0x65, 0x52, 0x00]
        );
    }

    #[test]
    fn v2_stored_values() {
        assert_eq!(v2_sidetone_pct(1, 700), Some(0));
        assert_eq!(v2_sidetone_pct(0, 700), Some(70));
        assert_eq!(v2_sidetone_pct(2, 0), None);
        assert_eq!(v2_sidetone_pct(0, 1010), None);
        assert_eq!(sleep_minutes(0, 123), Some(0));
        assert_eq!(sleep_minutes(1, 1_800_000), Some(30));
        assert_eq!(sleep_minutes(1, 1_800_001), None);
    }

    #[test]
    fn void_lights_packet() {
        assert_eq!(void_lights(true), [0xc8, 0x00, 0x00]);
        assert_eq!(void_lights(false), [0xc8, 0x01, 0x00]);
    }
}
