//! Supported products: one row per product, data only.
//!
//! PIDs: ckb-next `src/daemon/usb.h`, OpenRGB `CorsairPeripheralController
//! Detect.cpp` / `CorsairPeripheralV2Devices.h`, OpenLinkHub
//! `src/devices/devices.go` (decimal ids, converted), Linux
//! `hid-corsair-void.c`, HeadsetControl `corsair_*.hpp`, HaloBattery
//! `docs/protocols.md` and OpenLinkHub issue #235 (Virtuoso RGB Wireless).
//! LED counts: ckb-next `led_bragi.c` where listed, else OpenLinkHub
//! `LEDChannels`, or `(colorPacketLength - 2) / 3` where the two disagree
//! (what OpenLinkHub declares on the wire). DPI ranges: OpenLinkHub
//! `minDpiValue`/`maxDpiValue`. Bragi rows were checked against the
//! OpenLinkHub transfer framing (`[0x08, cmd..]`); devices framed otherwise
//! (Sabre v2 Pro, Nightsword v2, Slipstream v2) are not listed.

use super::nxp::Leds;
use super::VID;
use crate::drivers::Model;
use hako_core::device::DeviceKind;

pub struct Spec {
    pub name: &'static str,
    pub kind: DeviceKind,
    pub pids: &'static [u16],
    pub family: Family,
}

#[derive(Clone, Copy, Debug)]
pub enum Family {
    Nxp {
        leds: Leds,
        dpi: Option<(u16, u16)>,
    },
    Bragi {
        leds: u16,
        dpi: Option<(u16, u16)>,
        wireless: bool,
    },
    /// Generic Slipstream receiver; paired devices are looked up by PID.
    Receiver,
    /// Void wireless receiver: battery, sidetone, lights.
    Void,
    /// Void on its USB cable: lights only.
    VoidWired,
    /// `wake`: V2 receivers (interface 4, or 3 in [`V2_IFACE3`]) that need a
    /// heartbeat first.
    BragiHeadset {
        wake: bool,
    },
}

const NXP_KB: Family = Family::Nxp {
    leds: Leds::Keyboard,
    dpi: None,
};
/// ckb-next N_MOUSE_ZONES; the Glaive only has zones 1, 2 and 6.
const ZONES: &[u8] = &[1, 2, 3, 4, 5, 6];
const GLAIVE_ZONES: &[u8] = &[1, 2, 6];

const fn nxp_mouse(zones: &'static [u8], dpi: Option<(u16, u16)>) -> Family {
    Family::Nxp {
        leds: Leds::Mouse(zones),
        dpi,
    }
}

const fn kb(leds: u16) -> Family {
    Family::Bragi {
        leds,
        dpi: None,
        wireless: false,
    }
}

const fn kb_wireless(leds: u16) -> Family {
    Family::Bragi {
        leds,
        dpi: None,
        wireless: true,
    }
}

const fn mouse(leds: u16, min: u16, max: u16) -> Family {
    Family::Bragi {
        leds,
        dpi: Some((min, max)),
        wireless: false,
    }
}

const fn mouse_wireless(leds: u16, min: u16, max: u16) -> Family {
    Family::Bragi {
        leds,
        dpi: Some((min, max)),
        wireless: true,
    }
}

const HEADSET: Family = Family::BragiHeadset { wake: false };
const HEADSET_V2: Family = Family::BragiHeadset { wake: true };

/// Wireless dongles: devices behind them are addressed as 0x08|n.
pub const RECEIVERS: &[u16] = &[
    0x1b81, 0x1b7f, 0x1b66, 0x2b00, 0x1b65, 0x1b94, 0x2b07, 0x1ba6, 0x1bdc, 0x1b62,
];

/// V2 headset receivers on interface 3 (OpenLinkHub `devices.go`).
pub const V2_IFACE3: &[u16] = &[0x2a0c, 0x2a05, 0x2a12];

macro_rules! corsair_models {
    ($($name:literal, $kind:ident, [$($pid:literal),+], $family:expr;)*) => {
        pub const MODELS: &[Model] = &[$(Model {
            vendor_id: VID,
            product_ids: &[$($pid),+],
            kind: DeviceKind::$kind,
            name: $name,
            usbfs: false,
            // Receivers hosting several devices are rescanned for new pairings.
            multi: matches!($family, Family::Receiver),
            open: super::open,
        },)*];

        pub const SPECS: &[Spec] = &[$(Spec {
            name: $name,
            kind: DeviceKind::$kind,
            pids: &[$($pid),+],
            family: $family,
        },)*];
    };
}

// Wired PID first (it names remap/settings files), dongle PID after it.
corsair_models! {
    // Legacy NXP keyboards, 24-bit colour (ckb-next IS_FULLRANGE).
    "Corsair K70 RGB MK.2", Keyboard, [0x1b49], NXP_KB;
    "Corsair K70 RGB MK.2 SE", Keyboard, [0x1b6b], NXP_KB;
    "Corsair K70 RGB MK.2 Low Profile", Keyboard, [0x1b55], NXP_KB;
    "Corsair K70 LUX RGB", Keyboard, [0x1b33], NXP_KB;
    "Corsair K70 RGB RAPIDFIRE", Keyboard, [0x1b38], NXP_KB;
    "Corsair K95 RGB PLATINUM", Keyboard, [0x1b2d], NXP_KB;
    "Corsair K95 RGB PLATINUM SE", Keyboard, [0x1b82], NXP_KB;
    "Corsair K65 LUX RGB", Keyboard, [0x1b37], NXP_KB;
    "Corsair K65 RGB RAPIDFIRE", Keyboard, [0x1b39], NXP_KB;
    "Corsair K68 RGB", Keyboard, [0x1b4f], NXP_KB;
    "Corsair STRAFE RGB", Keyboard, [0x1b20], NXP_KB;
    "Corsair STRAFE RGB MK.2", Keyboard, [0x1b48], NXP_KB;
    "Corsair K55 RGB", Keyboard, [0x1b3d], Family::Nxp { leds: Leds::K55, dpi: None };

    // Legacy NXP mice.
    "Corsair M65 RGB ELITE", Mouse, [0x1b5a], nxp_mouse(ZONES, Some((100, 18000)));
    "Corsair M65 PRO RGB", Mouse, [0x1b2e], nxp_mouse(ZONES, None);
    "Corsair IRONCLAW RGB", Mouse, [0x1b5d], nxp_mouse(ZONES, Some((200, 18000)));
    "Corsair SCIMITAR RGB ELITE", Mouse, [0x1b8b], nxp_mouse(ZONES, Some((100, 18000)));
    "Corsair SCIMITAR PRO RGB", Mouse, [0x1b3e], nxp_mouse(ZONES, None);
    "Corsair HARPOON RGB PRO", Mouse, [0x1b75], nxp_mouse(ZONES, Some((200, 12000)));
    "Corsair HARPOON RGB", Mouse, [0x1b3c], nxp_mouse(ZONES, None);
    "Corsair GLAIVE RGB PRO", Mouse, [0x1b74], nxp_mouse(GLAIVE_ZONES, Some((100, 18000)));
    "Corsair GLAIVE RGB", Mouse, [0x1b34], nxp_mouse(GLAIVE_ZONES, None);
    "Corsair NIGHTSWORD RGB", Mouse, [0x1b5c], nxp_mouse(ZONES, Some((100, 18000)));
    "Corsair SABRE RGB", Mouse, [0x1b2f], nxp_mouse(ZONES, None);
    "Corsair SABRE RGB Optical", Mouse, [0x1b32, 0x1b14], nxp_mouse(ZONES, None);
    "Corsair SABRE RGB Laser", Mouse, [0x1b19], nxp_mouse(ZONES, None);
    "Corsair M65 RGB", Mouse, [0x1b12], nxp_mouse(ZONES, None);
    "Corsair SCIMITAR RGB", Mouse, [0x1b1e], nxp_mouse(ZONES, Some((100, 12000)));
    "Corsair KATAR", Mouse, [0x1b22], nxp_mouse(ZONES, None);

    // Bragi keyboards.
    "Corsair K70 RGB PRO", Keyboard, [0x1bc4, 0x1bc6], kb(193);
    "Corsair K70 RGB PRO V2", Keyboard, [0x1bb3], kb(193);
    "Corsair K70 PRO OPTI-MECHANICAL", Keyboard, [0x1bd4], kb(193);
    "Corsair K70 RGB TKL", Keyboard, [0x1b73], kb(193);
    "Corsair K70 RGB TKL Champion Series", Keyboard, [0x1bb9], kb(193);
    "Corsair K70 CORE RGB", Keyboard, [0x1bfd, 0x1bff, 0x2b0a], kb(123);
    "Corsair K70 CORE TKL", Keyboard, [0x2b01], kb(125);
    "Corsair K70 CORE TKL WIRELESS", Keyboard, [0x2b02], kb_wireless(123);
    "Corsair K70 PRO TKL", Keyboard, [0x2b14], kb(131);
    "Corsair K70 PRO MINI WIRELESS", Keyboard, [0x1bb6], kb_wireless(123);
    "Corsair K70 MAX", Keyboard, [0x1bc0], kb(142);
    "Corsair K65 RGB MINI", Keyboard, [0x1baf, 0x1bc3, 0x1bcf], kb(123);
    "Corsair K65 PRO MINI", Keyboard, [0x1bd7], kb(130);
    "Corsair K65 PLUS WIRELESS", Keyboard, [0x2b10, 0x2b11, 0x2b23, 0x2b07], kb_wireless(123);
    "Corsair K100 RGB", Keyboard, [0x1b7d], kb(193);
    "Corsair K100 RGB OPTICAL", Keyboard, [0x1b7c, 0x1bc5], kb(193);
    "Corsair K100 AIR WIRELESS", Keyboard, [0x1bab], kb_wireless(137);
    "Corsair K95 RGB PLATINUM XT", Keyboard, [0x1b89], kb(156);
    "Corsair K60 RGB PRO", Keyboard, [0x1ba0], kb(123);
    "Corsair K60 RGB PRO Low Profile", Keyboard, [0x1bad], kb(123);
    "Corsair K60 RGB PRO SE", Keyboard, [0x1b8d], kb(123);
    "Corsair K60 PRO TKL", Keyboard, [0x1bc7, 0x1bed], kb(123);
    "Corsair K55 RGB PRO", Keyboard, [0x1ba4], kb(6);
    "Corsair K55 RGB PRO XT", Keyboard, [0x1ba1], kb(137);
    "Corsair K55 CORE RGB", Keyboard, [0x1bfe, 0x2b05], kb(11);
    "Corsair K55 CORE TKL RGB", Keyboard, [0x2b20], kb(9);
    "Corsair K57 RGB WIRELESS", Keyboard, [0x1b6e, 0x1b62], kb_wireless(137);

    // Bragi mice.
    "Corsair DARK CORE RGB PRO", Mouse, [0x1b80, 0x1b81], mouse_wireless(12, 100, 18000);
    "Corsair DARK CORE RGB PRO SE", Mouse, [0x1b7e, 0x1b7f], mouse_wireless(12, 100, 18000);
    "Corsair IRONCLAW RGB WIRELESS", Mouse, [0x1b4c, 0x1b66], mouse_wireless(6, 200, 18000);
    "Corsair IRONCLAW WIRELESS SE", Mouse, [0x2b32, 0x2b00], mouse_wireless(6, 100, 26000);
    "Corsair HARPOON RGB WIRELESS", Mouse, [0x1b5e, 0x1b65], mouse_wireless(2, 200, 10000);
    "Corsair HARPOON V2 WIRELESS", Mouse, [0x2b2e], mouse_wireless(2, 100, 26000);
    "Corsair KATAR PRO", Mouse, [0x1b93], mouse(1, 200, 12400);
    "Corsair KATAR PRO XT", Mouse, [0x1bac], mouse(1, 100, 18000);
    "Corsair KATAR PRO WIRELESS", Mouse, [0x1b94], mouse_wireless(1, 200, 10000);
    "Corsair M55 RGB PRO", Mouse, [0x1b70], mouse(2, 200, 12400);
    "Corsair M55", Mouse, [0x2b03], mouse(1, 100, 16000);
    // Only seen behind a Slipstream receiver (OpenLinkHub `slipstream.go`).
    "Corsair M55 WIRELESS", Mouse, [0x1bfb], mouse_wireless(1, 100, 16000);
    "Corsair M65 RGB ULTRA", Mouse, [0x1b9e], mouse(3, 100, 26000);
    "Corsair M65 RGB ULTRA WIRELESS", Mouse, [0x1bb5, 0x1bd6], mouse_wireless(2, 100, 26000);
    "Corsair M75", Mouse, [0x1bf0], mouse(2, 100, 26000);
    "Corsair M75 WIRELESS", Mouse, [0x1bf1, 0x2b08], mouse_wireless(2, 100, 26000);
    "Corsair M75 AIR WIRELESS", Mouse, [0x1bf2], mouse_wireless(1, 100, 26000);
    "Corsair SCIMITAR RGB ELITE (2023)", Mouse, [0x1be3], mouse(5, 100, 18000);
    "Corsair SCIMITAR RGB ELITE WIRELESS", Mouse, [0x1bdb], mouse_wireless(4, 100, 26000);
    "Corsair SCIMITAR ELITE WIRELESS SE", Mouse, [0x2b22], mouse_wireless(4, 100, 33000);
    "Corsair SABRE RGB PRO", Mouse, [0x1b79], mouse(5, 100, 18000);
    "Corsair SABRE PRO CHAMPION SERIES", Mouse, [0x1b7a], mouse(1, 100, 18000);
    "Corsair SABRE RGB PRO WIRELESS", Mouse, [0x1b98], mouse_wireless(4, 100, 26000);
    "Corsair NIGHTSABRE WIRELESS", Mouse, [0x1bb8], mouse_wireless(15, 100, 26000);
    "Corsair DARKSTAR RGB WIRELESS", Mouse, [0x1bb2], mouse_wireless(13, 100, 26000);

    // Multi-device Slipstream receivers.
    "Corsair SLIPSTREAM Wireless Receiver", Mouse, [0x1ba6], Family::Receiver;
    "Corsair SLIPSTREAM Wireless Receiver (2)", Mouse, [0x1bdc], Family::Receiver;

    // Headsets: Void protocol.
    "Corsair VOID Wireless", Headset, [0x1b27, 0x1b23, 0x1b25, 0x0a0c, 0x0a2b], Family::Void;
    "Corsair VOID PRO Wireless", Headset, [0x0a14, 0x0a16, 0x0a1a], Family::Void;
    "Corsair VOID ELITE Wireless", Headset, [0x0a55, 0x0a51, 0x0a75], Family::Void;
    "Corsair HS70 Wireless", Headset, [0x0a38], Family::Void;
    "Corsair HS70 PRO Wireless", Headset, [0x0a4f], Family::Void;
    "Corsair VOID USB", Headset, [0x1b2a, 0x1b29, 0x1b1c], Family::VoidWired;
    "Corsair VOID PRO USB", Headset, [0x0a17, 0x0a1d], Family::VoidWired;
    "Corsair VOID ELITE USB", Headset, [0x0a52, 0x0a56], Family::VoidWired;

    // Headsets: Bragi (receiver first, then the headset on a cable).
    "Corsair VIRTUOSO RGB Wireless", Headset, [0x0a42, 0x0a44, 0x0a41, 0x0a43], HEADSET;
    "Corsair VIRTUOSO RGB Wireless SE", Headset, [0x0a3e, 0x0a40, 0x0a3d, 0x0a3f], HEADSET;
    "Corsair VIRTUOSO RGB Wireless XT", Headset, [0x0a64, 0x0a62], HEADSET;
    "Corsair HS80 RGB Wireless", Headset, [0x0a6b, 0x0a69], HEADSET;
    "Corsair HS80 MAX Wireless", Headset, [0x0a97, 0x2a0f, 0x2a0c], HEADSET_V2;
    "Corsair VOID Wireless V2", Headset, [0x2a08], HEADSET_V2;
    "Corsair VOID MAX Wireless V2", Headset, [0x2a12, 0x2a05], HEADSET_V2;
    "Corsair VIRTUOSO MAX Wireless", Headset, [0x2a02, 0x2a03], HEADSET_V2;
}

pub fn spec(pid: u16) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.pids.contains(&pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn table_is_consistent() {
        let mut seen = HashSet::new();
        for (m, s) in MODELS.iter().zip(SPECS) {
            assert_eq!((m.name, m.product_ids, m.kind), (s.name, s.pids, s.kind));
            for &pid in s.pids {
                assert!(seen.insert(pid), "{pid:04x} listed twice ({})", s.name);
            }
            // Every model offers a control or a battery.
            let useful = match s.family {
                Family::Nxp { .. }
                | Family::Void
                | Family::VoidWired
                | Family::BragiHeadset { .. } => true,
                Family::Bragi {
                    leds,
                    dpi,
                    wireless,
                } => leds > 0 || dpi.is_some() || wireless,
                Family::Receiver => s.pids.iter().all(|p| RECEIVERS.contains(p)),
            };
            assert!(useful, "{} exposes nothing", s.name);
            if let Family::Bragi {
                dpi: Some((lo, hi)),
                ..
            }
            | Family::Nxp {
                dpi: Some((lo, hi)),
                ..
            } = s.family
            {
                assert!(
                    lo < hi && lo % 50 == 0 && hi % 50 == 0,
                    "{} dpi range",
                    s.name
                );
            }
        }
        assert_eq!(MODELS.len(), SPECS.len());
    }

    #[test]
    fn receivers_belong_to_wireless_models() {
        for &pid in RECEIVERS {
            let s = spec(pid).unwrap_or_else(|| panic!("receiver {pid:04x} has no model"));
            assert!(
                matches!(
                    s.family,
                    Family::Receiver | Family::Bragi { wireless: true, .. }
                ),
                "{} behind a receiver must be wireless",
                s.name
            );
        }
    }

    #[test]
    fn only_multi_device_receivers_are_rescanned() {
        for (m, s) in MODELS.iter().zip(SPECS) {
            assert_eq!(m.multi, matches!(s.family, Family::Receiver), "{}", s.name);
        }
        for &pid in V2_IFACE3 {
            assert!(
                matches!(
                    spec(pid).map(|s| s.family),
                    Some(Family::BragiHeadset { wake: true })
                ),
                "{pid:04x}"
            );
        }
    }

    #[test]
    fn lookups() {
        assert_eq!(
            spec(0x1b81).map(|s| s.name),
            Some("Corsair DARK CORE RGB PRO")
        );
        assert_eq!(spec(0x0a6b).map(|s| s.kind), Some(DeviceKind::Headset));
        assert!(spec(0x1b13).is_none(), "9-bit K70 RGB is not supported");
    }
}
