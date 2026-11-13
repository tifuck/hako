//! Adapter trait surface shared conceptually with the daemon (types only).

use crate::device::{DeviceId, DeviceSnapshot};
use crate::remap::{DeviceRemaps, RemapCaptured};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AdapterEvent {
    Updated(DeviceSnapshot),
    Disconnected(DeviceId),
    Connected(DeviceSnapshot),
    /// A background failure (nobody's request is waiting for it); pushed once
    /// per distinct error, see [`is_new_error`].
    Error {
        device: Option<DeviceId>,
        message: String,
    },
    RemapsUpdated(DeviceRemaps),
    RemapCaptured(RemapCaptured),
    /// Daemon is stopping; subscribers should disconnect.
    ShuttingDown,
}

/// Records `current` as a background source's error and says whether to push
/// it as [`AdapterEvent::Error`]: only an error that differs from the last one
/// pushed. `None` (recovered) resets it, so a recurrence is pushed again.
pub fn is_new_error(reported: &mut Option<String>, current: Option<&str>) -> bool {
    if reported.as_deref() == current {
        return false;
    }
    *reported = current.map(str::to_owned);
    current.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_is_new_once_until_it_changes_or_clears() {
        let mut reported = None;
        let mut new = |e| is_new_error(&mut reported, e);
        assert!(!new(None));
        assert!(new(Some("a")));
        assert!(!new(Some("a")), "same error again");
        assert!(new(Some("b")), "a different error");
        assert!(!new(None), "recovering pushes nothing");
        assert!(new(Some("b")), "back after recovering");
    }
}
