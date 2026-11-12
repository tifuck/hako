//! Device identity and snapshots.

use crate::catalog::{SettingMeta, SettingsMap};
use crate::remap::DeviceRemaps;
use serde::{Deserialize, Serialize};

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
    pub controls: Vec<SettingMeta>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioSummary {
    pub chatmix_active: bool,
    pub aux_active: bool,
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
