//! Device identity and snapshots.

use crate::catalog::{SettingMeta, SettingsMap};
use crate::remap::DeviceRemaps;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Keyboard,
    Mouse,
    Headset,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId {
    pub vendor_id: u16,
    pub product_id: u16,
    /// Stable instance key (serial or sys path hash).
    pub instance: String,
}

impl DeviceId {
    pub fn key(&self) -> String {
        format!(
            "{:04x}_{:04x}_{}",
            self.vendor_id, self.product_id, self.instance
        )
    }

    pub fn config_stem(&self) -> String {
        format!("{:04x}_{:04x}", self.vendor_id, self.product_id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatteryState {
    pub label: String,
    /// 0–100 when known.
    pub percent: Option<u8>,
    pub charging: bool,
    pub present: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub id: DeviceId,
    pub kind: DeviceKind,
    pub name: String,
    pub connected: bool,
    pub batteries: Vec<BatteryState>,
    pub settings: SettingsMap,
    /// Controls this build cannot read (a newer daemon's keys) or draw (an
    /// inverted range) are skipped, so one bad control never hides the whole
    /// state or crashes the UI.
    #[serde(deserialize_with = "known_controls")]
    pub controls: Vec<SettingMeta>,
    pub last_error: Option<String>,
}

fn known_controls<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SettingMeta>, D::Error> {
    Ok(Vec::<serde_json::Value>::deserialize(d)?
        .into_iter()
        .filter_map(|v| serde_json::from_value::<SettingMeta>(v).ok())
        .filter(|m| m.min.zip(m.max).is_none_or(|(lo, hi)| lo <= hi))
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSummary {
    pub chatmix_active: bool,
    /// Legacy (line-in is a mixer input now): always false.
    pub aux_active: bool,
    /// Legacy, like `aux_active`.
    #[serde(default)]
    pub aux_processing_active: bool,
    #[serde(default)]
    pub last_error: Option<String>,
    pub default_sink: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonHealth {
    pub version: String,
    pub uptime_secs: u64,
    pub adapter_count: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubState {
    pub health: DaemonHealth,
    pub last_profile: Option<String>,
    pub profiles: Vec<String>,
    pub audio: AudioSummary,
    pub devices: Vec<DeviceSnapshot>,
    /// Host-side button remaps, keyed by device in `remaps.device_id`.
    #[serde(default)]
    pub remaps: Vec<DeviceRemaps>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SettingKey;

    #[test]
    fn unknown_control_keys_are_skipped_not_fatal() {
        let snap = DeviceSnapshot {
            id: DeviceId {
                vendor_id: 1,
                product_id: 2,
                instance: "x".into(),
            },
            kind: DeviceKind::Mouse,
            name: "m".into(),
            connected: true,
            batteries: vec![],
            settings: Default::default(),
            controls: vec![
                SettingMeta::slider(SettingKey::Dpi, "DPI", 100.0, 1000.0, 1.0),
                SettingMeta::toggle(SettingKey::AngleSnapping, "Snap"),
                SettingMeta::slider(SettingKey::Brightness, "Bad", 100.0, 0.0, 1.0),
            ],
            last_error: None,
        };
        let json = serde_json::to_string(&snap)
            .unwrap()
            .replace("\"key\":\"dpi\"", "\"key\":\"from_the_future\"");
        let back: DeviceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.controls, snap.controls[1..2]);
    }
}
