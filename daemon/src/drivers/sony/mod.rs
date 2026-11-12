//! Sony (VID 0x054c): INZONE and PULSE wireless headsets.
//!
//! Reimplemented from public notes; only IDs and byte layouts were taken from:
//! - HeadsetControl `lib/devices/protocols/sony_inzone_protocol.hpp`, `sony_inzone_h5.hpp`,
//!   `sony_inzone_h9_ii.hpp`, `sony_inzone_buds.hpp`, `sony_pulse_3d.hpp` (GPL, layouts only):
//!   <https://github.com/Sapd/HeadsetControl/tree/master/lib/devices>
//! - zoneout `SPECS.md`, `src/zoneout/protocol.py` (MIT; INZONE H9 II and Buds value
//!   ranges verified on hardware): <https://github.com/marcinjakubowski/zoneout>
//! - OpenInzone `docs/PROTOCOL.md` (GPL, layouts only; INZONE Buds event ids, battery
//!   order, sidetone/volume/balance ranges): <https://github.com/penguinwokrs/openinzone>
//!
//! `inzone.rs` speaks the INZONE dongles' HCI-over-HID protocol, `pulse.rs` reads the
//! PULSE 3D's status reports. Nothing here writes firmware or stored profiles.

mod inzone;
mod pulse;

use crate::drivers::Model;
use crate::hid::{enumerate_hidraw, HidrawDevice};
use anyhow::{Context, Result};
use hako_core::device::DeviceKind;

const SONY: u16 = 0x054c;

macro_rules! model {
    ($module:ident :: $spec:ident) => {
        Model {
            vendor_id: SONY,
            product_ids: $module::$spec.pids,
            kind: DeviceKind::Headset,
            name: $module::$spec.name,
            usbfs: false,
            multi: false,
            open: |pid, ctx| $module::open(&$module::$spec, pid, ctx),
        }
    };
}

pub const MODELS: &[Model] = &[
    model!(inzone::H9_II),
    model!(inzone::H5),
    model!(inzone::BUDS),
    model!(pulse::PULSE_3D),
];

/// Open the hidraw node of `vid:pid` whose report descriptor (read from sysfs)
/// satisfies `wanted`; returns the device and a stable instance key.
fn open_node(pid: u16, wanted: impl Fn(&[u8]) -> bool) -> Result<(HidrawDevice, String)> {
    let info = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == SONY && d.product_id == pid)
        .find(|d| wanted(&d.report_descriptor()))
        .with_context(|| format!("no control node for {SONY:04x}:{pid:04x}"))?;
    let dev = HidrawDevice::open(&info.path)?;
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq
    };
    Ok((dev, instance))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_is_registered_once() {
        assert_eq!(MODELS.len(), inzone::ALL.len() + 1);
        for spec in inzone::ALL {
            assert!(MODELS.iter().any(|m| m.name == spec.name));
        }
        assert!(MODELS.iter().all(|m| m.vendor_id == SONY));
    }
}
