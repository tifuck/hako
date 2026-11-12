//! Logitech Zone Wired (`0aad`) and Zone 750 (`0ade`) USB headsets: 20-byte
//! feature reports with report id 0x22.
//!
//! Layouts reimplemented from HeadsetControl
//! `lib/devices/logitech_zone_wired.hpp` (no code copied):
//! `22 F1 04 00 <cmd> <sub> <value>`, zero padded to 20 bytes.
//! - Sidetone: cmd 0x04, sub 0x3D, value 0..=10.
//! - Voice prompts: cmd 0x05, sub 0x3D, value 0 / 1.
//!
//! Write-only: the UI shows the last values set, re-applied on open.
//! Rotate-to-mute (cmd 0x05, sub 0x6D) has no setting key yet.

use super::device::VID;
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use tracing::warn;

#[cfg(test)]
pub const PIDS: [u16; 2] = [0x0aad, 0x0ade];
const REPORT_ID: u8 = 0x22;
const SIDETONE_MAX: u8 = 10;

fn report(key: SettingKey, value: u8) -> Option<[u8; 20]> {
    let (cmd, sub) = match key {
        SettingKey::MicSideTone => (0x04, 0x3d),
        SettingKey::VoicePrompts => (0x05, 0x3d),
        _ => return None,
    };
    let mut r = [0u8; 20];
    r[..7].copy_from_slice(&[REPORT_ID, 0xf1, 0x04, 0x00, cmd, sub, value]);
    Some(r)
}

fn wire_value(value: &SettingValue) -> Option<u8> {
    value
        .as_int()
        .map(|v| v as u8)
        .or_else(|| value.as_bool().map(u8::from))
}

pub fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == VID && d.product_id == pid)
        .filter(|d| crate::hid::report_ids(&d.report_descriptor()).contains(&REPORT_ID))
        .collect();
    nodes.sort_by_key(|d| d.interface);
    let info = nodes.first().context("no Zone (report 0x22) interface")?;
    let dev = HidrawDevice::open(&info.path)?;
    let settings = hako_core::config::load_device_settings(&ctx.paths, VID, pid)?;
    let name = super::MODELS
        .iter()
        .find(|m| m.product_ids.contains(&pid))
        .map_or("Logitech Zone", |m| m.name);
    let mut a = Zone {
        id: DeviceId {
            vendor_id: VID,
            product_id: pid,
            instance: "zone".into(),
        },
        name,
        dev: Some(dev),
        paths: ctx.paths.clone(),
        settings,
        last_error: None,
    };
    for key in [SettingKey::MicSideTone, SettingKey::VoicePrompts] {
        if let Some(v) = get_setting(&a.settings, key).and_then(wire_value) {
            if let Err(e) = a.write(key, v) {
                warn!("{}: restore {}: {e}", a.name, key.as_str());
            }
        }
    }
    Ok(vec![Box::new(a)])
}

struct Zone {
    id: DeviceId,
    name: &'static str,
    dev: Option<HidrawDevice>,
    paths: ConfigPaths,
    settings: SettingsMap,
    last_error: Option<String>,
}

impl Zone {
    fn write(&mut self, key: SettingKey, value: u8) -> Result<()> {
        let mut r = report(key, value).context("unsupported setting")?;
        self.dev
            .as_ref()
            .context("device closed")?
            .set_feature(&mut r)
    }
}

impl DeviceAdapter for Zone {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: vec![
                SettingMeta::slider(
                    SettingKey::MicSideTone,
                    "Sidetone",
                    0.0,
                    SIDETONE_MAX.into(),
                    1.0,
                ),
                SettingMeta::toggle(SettingKey::VoicePrompts, "Voice prompts"),
            ],
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let Some(v) = wire_value(&value) else {
            bail!("unexpected value for {}", key.as_str());
        };
        if let Err(e) = self.write(key, v) {
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
    fn reports_match_headsetcontrol() {
        let r = report(SettingKey::MicSideTone, 7).unwrap();
        assert_eq!(&r[..7], &[0x22, 0xf1, 0x04, 0x00, 0x04, 0x3d, 7]);
        assert!(r[7..].iter().all(|&b| b == 0));
        let r = report(SettingKey::VoicePrompts, 1).unwrap();
        assert_eq!(&r[..7], &[0x22, 0xf1, 0x04, 0x00, 0x05, 0x3d, 1]);
        assert!(report(SettingKey::Dpi, 1).is_none());
        assert_eq!(wire_value(&SettingValue::Bool(true)), Some(1));
        assert_eq!(wire_value(&SettingValue::Int(10)), Some(10));
    }
}
