//! SteelSeries headsets, mice and keyboards.
//!
//! - `nova_pro`: Arctis Nova Pro Wireless GameDAC (ChatMix / AUX audio graph).
//! - `headset`: other Arctis / Arctis Nova headsets and GameBuds. Byte
//!   layouts from HeadsetControl `lib/devices/steelseries_*.hpp` and
//!   `lib/devices/protocols/steelseries_protocol.hpp` (github.com/Sapd/HeadsetControl),
//!   cross-checked against Linux-Arctis-Manager `daemon/device-configs/*.yaml`
//!   (github.com/elegos/Linux-Arctis-Manager).
//! - `mouse`: Rival / Aerox / Prime / Sensei / Kana / Kinzu. Byte layouts from
//!   rivalcfg `rivalcfg/devices/*.py`, `rivalcfg/handlers/*.py` and
//!   `rivalcfg/devices/dpi/*.py` (github.com/flozz/rivalcfg).
//! - `keyboard`: Apex. Byte layouts from OpenRGB
//!   `Controllers/SteelSeriesController/SteelSeriesApex*`,
//!   `SteelSeriesOldApexController` and `SteelSeriesDevices.h`
//!   (gitlab.com/CalcProgrammer1/OpenRGB); Apex Pro actuation from
//!   apex-analog `apex_analog.py` (github.com/milescoviello/apex-analog).
//!
//! All are cited as references only; nothing is copied. Setters are
//! volatile. A model's flash "save" (headsets: HeadsetControl "save state" /
//! Linux-Arctis-Manager `save_to_flash`; mice: rivalcfg `save_command`,
//! confirmed by libratbag `driver-steelseries.c` or OpenRGB) is only sent from
//! `set` while the owner-enabled "Save changes to device memory" toggle is on,
//! and only where `docs/onboard-writes/steelseries.md` rates it HIGH; other
//! models list it blocked. Keyboard onboard-profile writes and the Apex Pro
//! flash save are never sent. Exception: `nova_pro` (hardware-verified) has
//! always followed each setter with `06 09`, which both sources call the save
//! command; it is left byte-identical pending the owner's decision.

mod headset;
mod keyboard;
mod models;
mod mouse;
mod nova_pro;

pub use models::MODELS;

use crate::hid::{HidDeviceInfo, HidrawDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x1038;

/// Zero-pad a report to the length the device expects.
fn pad(cmd: &[u8], len: usize) -> Vec<u8> {
    let mut v = cmd.to_vec();
    v.resize(len.max(cmd.len()), 0);
    v
}

/// Serial when the node has one, else its path (stable while plugged).
fn instance(info: &HidDeviceInfo) -> String {
    if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq.clone()
    }
}

/// Write `req`, then return the first input report `accept` takes within
/// `timeout`. `Ok(None)` = no answer (e.g. a wireless device that is off).
fn request(
    dev: &mut HidrawDevice,
    req: &[u8],
    accept: impl Fn(&[u8]) -> bool,
    timeout: Duration,
) -> anyhow::Result<Option<Vec<u8>>> {
    dev.drain(Duration::ZERO);
    dev.write(req)?;
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 64];
    while let Ok(n) = dev.read_timeout(&mut buf, deadline.saturating_duration_since(Instant::now()))
    {
        if accept(&buf[..n]) {
            return Ok(Some(buf[..n].to_vec()));
        }
    }
    Ok(None)
}

/// `get_setting` as an integer.
fn int(
    settings: &hako_core::catalog::SettingsMap,
    key: hako_core::catalog::SettingKey,
) -> Option<i64> {
    hako_core::config::get_setting(settings, key).and_then(|v| v.as_int())
}
