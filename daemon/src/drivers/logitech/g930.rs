//! Logitech G930 wireless headset (`0a1f`): a pre-HID++ protocol of 64-byte
//! feature reports (report id 0xFF) on its usage page 0xFF00 node.
//!
//! Layouts reimplemented from HeadsetControl `lib/devices/logitech_g930.hpp`
//! (no code copied):
//! - Battery: send `FF 09 00 FD F4 10 05 B1 BF A0 04`, then read the feature
//!   report three times 100 ms apart; byte 13 is a raw level, 44 (empty) to
//!   91 (full).
//! - Sidetone: send `FF 0A 00 FF F4 10 05 DA 8F F2 01 <level 0..=128>`.
//!
//! Sidetone cannot be read back: the UI shows the last value set, which is
//! re-applied whenever the dongle is opened.

use super::device::VID;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::warn;

#[cfg(test)]
pub const PIDS: [u16; 1] = [0x0a1f];
const REPORT: usize = 64;
const BATTERY_EVERY: Duration = Duration::from_secs(30);
const RAW_EMPTY: u8 = 44;
const RAW_FULL: u8 = 91;

fn battery_request() -> [u8; REPORT] {
    let mut b = [0u8; REPORT];
    b[..11].copy_from_slice(&[
        0xff, 0x09, 0x00, 0xfd, 0xf4, 0x10, 0x05, 0xb1, 0xbf, 0xa0, 0x04,
    ]);
    b
}

fn sidetone_request(level: u8) -> [u8; REPORT] {
    let mut b = [0u8; REPORT];
    b[..12].copy_from_slice(&[
        0xff, 0x0a, 0x00, 0xff, 0xf4, 0x10, 0x05, 0xda, 0x8f, 0xf2, 0x01, level,
    ]);
    b
}

/// Raw level (byte 13) → percent, linear between the empty and full marks.
fn battery_percent(raw: u8) -> u8 {
    let raw = raw.clamp(RAW_EMPTY, RAW_FULL);
    ((u16::from(raw - RAW_EMPTY) * 100) / u16::from(RAW_FULL - RAW_EMPTY)) as u8
}

/// Byte 13 of a battery reply of `len` bytes, if it reaches that far.
fn battery_raw(reply: &[u8], len: usize) -> Option<u8> {
    reply[..len.min(reply.len())].get(13).copied()
}

pub fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == VID && d.product_id == pid)
        .filter(|d| crate::hid::usage_pages(&d.report_descriptor()).contains(&0xff00))
        .collect();
    nodes.sort_by_key(|d| d.interface);
    let info = nodes.first().context("no G930 vendor (0xFF00) interface")?;
    let dev = HidrawDevice::open(&info.path)?;
    let mut settings = hako_core::config::load_device_settings(&ctx.paths, VID, pid)?;
    crate::drivers::sanitize_saved(&mut settings, &controls());
    let mut a = G930 {
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: "g930".into(),
        },
        dev: Some(dev),
        paths: ctx.paths.clone(),
        settings,
        battery: BatteryState {
            label: "Headset".into(),
            percent: None,
            charging: false,
            present: true,
        },
        last_poll: Instant::now(),
        last_error: None,
    };
    let saved = get_setting(&a.settings, SettingKey::MicSideTone).and_then(|v| v.as_int());
    if let Some(v) = saved.and_then(|v| u8::try_from(v).ok()) {
        if let Err(e) = a.send(&mut sidetone_request(v)) {
            warn!("G930: restore sidetone: {e}");
        }
    }
    a.poll_battery();
    Ok(vec![Box::new(a)])
}

fn controls() -> Vec<SettingMeta> {
    vec![SettingMeta::slider(
        SettingKey::MicSideTone,
        "Sidetone",
        0.0,
        128.0,
        1.0,
    )]
}

struct G930 {
    id: DeviceId,
    dev: Option<HidrawDevice>,
    paths: ConfigPaths,
    settings: SettingsMap,
    battery: BatteryState,
    last_poll: Instant,
    last_error: Option<String>,
}

impl G930 {
    fn send(&mut self, report: &mut [u8; REPORT]) -> Result<()> {
        self.dev
            .as_mut()
            .context("device closed")?
            .set_feature(report)
    }

    fn poll_battery(&mut self) {
        self.last_poll = Instant::now();
        let read = || -> Result<u8> {
            let dev = self.dev.as_ref().context("device closed")?;
            dev.set_feature(&mut battery_request())?;
            let mut raw = None;
            for i in 0..3 {
                // A fresh buffer: a short read must not keep the last reply.
                let mut buf = [0u8; REPORT];
                buf[0] = 0xff;
                let len = dev.get_feature_len(&mut buf)?;
                raw = battery_raw(&buf, len);
                if i < 2 {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            raw.context("short battery reply")
        };
        self.battery.percent = read().ok().map(battery_percent);
    }
}

impl DeviceAdapter for G930 {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: "Logitech G930".into(),
            connected: self.dev.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let SettingKey::MicSideTone = key else {
            bail!("Logitech G930 has no {} control", key.as_str());
        };
        let level = value.as_int().context("expected a number")?;
        let level =
            u8::try_from(level).with_context(|| format!("sidetone {level} is out of range"))?;
        if let Err(e) = self.send(&mut sidetone_request(level)) {
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        merge_setting(&mut self.settings, key, value);
        hako_core::config::save_device_settings(
            &self.paths,
            VID,
            self.id.product_id,
            &self.settings,
        )
        .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn tick(&mut self) -> Result<()> {
        if self.dev.is_some() && self.last_poll.elapsed() >= BATTERY_EVERY {
            self.poll_battery();
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
    fn reports_match_headsetcontrol() {
        let b = battery_request();
        assert_eq!(
            &b[..12],
            &[0xff, 0x09, 0x00, 0xfd, 0xf4, 0x10, 0x05, 0xb1, 0xbf, 0xa0, 0x04, 0x00]
        );
        let s = sidetone_request(100);
        assert_eq!(
            &s[..12],
            &[0xff, 0x0a, 0x00, 0xff, 0xf4, 0x10, 0x05, 0xda, 0x8f, 0xf2, 0x01, 100]
        );
        assert!(s[12..].iter().all(|&x| x == 0));
    }

    #[test]
    fn battery_scale() {
        assert_eq!(battery_percent(44), 0);
        assert_eq!(battery_percent(91), 100);
        assert_eq!(battery_percent(67), 48);
        assert_eq!(battery_percent(10), 0);
        assert_eq!(battery_percent(200), 100);
        let mut reply = [0u8; REPORT];
        reply[13] = 67;
        assert_eq!(battery_raw(&reply, REPORT), Some(67));
        assert_eq!(battery_raw(&reply, 13), None);
    }
}
