//! Adapter trait surface shared conceptually with the daemon (types only).

use crate::device::{DeviceId, DeviceSnapshot};
use crate::remap::{DeviceRemaps, RemapCaptured};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AdapterEvent {
    Updated(DeviceSnapshot),
    Disconnected(DeviceId),
    Connected(DeviceSnapshot),
    Error {
        device: Option<DeviceId>,
        message: String,
    },
    RemapsUpdated(DeviceRemaps),
    RemapCaptured(RemapCaptured),
    /// Daemon is stopping; subscribers should disconnect.
    ShuttingDown,
}
