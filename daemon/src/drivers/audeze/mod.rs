//! Audeze (VID 0x3329): Maxwell wireless headsets.
//!
//! Reimplemented from public notes; only IDs and byte layouts were taken from
//! HeadsetControl `lib/devices/audeze_maxwell_device.hpp`, `audeze_maxwell.hpp`,
//! `audeze_maxwell2.hpp` (GPL, layouts only):
//! <https://github.com/Sapd/HeadsetControl/tree/master/lib/devices>. VID and the
//! Xbox dongle's collections: HeadsetControl issue #460.
//!
//! Nothing here writes the headset's EQ storage or firmware.

mod maxwell;

use crate::drivers::Model;
use hako_core::device::DeviceKind;

const AUDEZE: u16 = 0x3329;

macro_rules! model {
    ($spec:ident) => {
        Model {
            vendor_id: AUDEZE,
            product_ids: maxwell::$spec.pids,
            kind: DeviceKind::Headset,
            name: maxwell::$spec.name,
            usbfs: false,
            multi: false,
            open: |pid, ctx| maxwell::open(&maxwell::$spec, pid, ctx),
        }
    };
}

pub const MODELS: &[Model] = &[model!(MAXWELL_2), model!(MAXWELL)];
