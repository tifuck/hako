//! Which settings a model may save to its onboard memory, per PID, and why
//! the others are not offered. Research and sources:
//! `docs/onboard-writes/corsair.md`.
//!
//! Bragi property writes are the persistent path (iCUE keeps the poll rate
//! per onboard profile; HeadsetControl measured sidetone, sleep timer and
//! brightness surviving a power cycle), so every offered control is
//! `persistent` and none is re-sent on connect: the device's own value is
//! read back for display instead. Each write is read-validate-write-verify
//! (`bragi::store`).
//!
//! - Poll rate (property 0x01, 1 = 125 Hz .. 7 = 8000 Hz): ckb-next
//!   `device_bragi.c` `cmd_pollrate_bragi` and OpenLinkHub `UpdatePollingRate`
//!   (`cmdSetPollingRate = 01 01 00` + value) write the same bytes. Offered
//!   on the PIDs listed below with OpenLinkHub's per-model maximum (ckb-next's
//!   1 ms default where only ckb-next lists the PID), wired only: through a
//!   Slipstream receiver the rate belongs to the receiver (OpenLinkHub #396).
//! - Angle snapping (property 0x07, 0/1): ckb-next `dpi_bragi.c`
//!   `cmd_snap_bragi` (every Bragi mouse) and OpenLinkHub `cmdAngleSnapping`.
//! - Headset lights (brightness property 0x02, 0 or 1000, bracketed by
//!   software mode): HeadsetControl `corsair_virtuoso_xt.hpp` for the
//!   Virtuoso XT/SE PIDs, the same values as ckb-next `led_bragi.c`.
//! - V2 headset sidetone (0xD1 / 0x46 / 0x47) and sleep timer (0x0D /
//!   0x0E) on the V2 receivers: HeadsetControl `corsair_void_v2w.hpp`
//!   (2a08, 2a02, 0a97) and OpenLinkHub `voidV2W.go`, `virtuosomaxW.go`,
//!   `hs80maxW.go` (every V2 receiver PID) write the same properties.
//! - Virtuoso XT/SE sleep timer on their receivers: HeadsetControl
//!   `corsair_virtuoso_xt.hpp` and OpenLinkHub `virtuosorgbXTW.go` /
//!   `virtuosoSEW.go` agree (0x0D, 0x0E); on the cable OpenLinkHub also
//!   writes 0x37, so the cable PIDs stay blocked.
//! - NXP mouse poll rate (`07 0A 00 00 <ms>`, write-only, current rate read
//!   from the `0E 01` identification report): ckb-next `device_mouse.c`
//!   `cmd_pollrate` on its `SUPPORTS_ADJRATE` PIDs and OpenLinkHub
//!   `m65rgbelite.go` / `m65prorgb.go` / `glaivergb.go` /
//!   `scimitarprorgb.go` agree byte for byte.

use super::models::{Family, RECEIVERS};
use super::nxp::Leds;

/// What a model may do with one onboard setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Offer {
    /// No such setting (or not researched for this family).
    #[default]
    No,
    Yes,
    /// Known to exist, not offered for this reason.
    Blocked(&'static str),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Onboard {
    pub report_rate: Offer,
    /// Highest poll rate (Hz) a `Yes` report rate may select.
    pub report_rate_max: u32,
    pub angle_snap: Offer,
    /// Bragi headset lights through the brightness property.
    pub lights: Offer,
    pub sidetone: Offer,
    /// Switch active noise cancellation off before turning sidetone on.
    pub anc: bool,
    pub sleep: Offer,
    pub dpi_stages: Offer,
    /// NXP "save to hardware" (whole onboard profile).
    pub save: Offer,
}

/// Wired Bragi PIDs whose poll-rate write a source applies to that exact PID,
/// with the highest rate offered there (OpenLinkHub `PollingRates`, else
/// ckb-next's assumed 1000 Hz).
pub const REPORT_RATE: &[(u16, u32)] = &[
    // Keyboards, 8 kHz.
    (0x1bc4, 8000),
    (0x1bc6, 8000),
    (0x1bb3, 8000),
    (0x1bd4, 8000),
    (0x1b73, 8000),
    (0x1bb9, 8000),
    (0x1bb6, 8000),
    (0x1bc0, 8000),
    (0x1baf, 8000),
    (0x1bc3, 8000),
    (0x1bcf, 8000),
    (0x1bd7, 8000),
    (0x1b7d, 8000),
    (0x1b7c, 8000),
    (0x1bc5, 8000),
    (0x1bab, 8000),
    // Keyboards, 1 kHz.
    (0x1bfd, 1000),
    (0x1bff, 1000),
    (0x2b0a, 1000),
    (0x2b01, 1000),
    (0x2b02, 1000),
    (0x1b89, 1000),
    (0x1ba0, 1000),
    (0x1bad, 1000),
    (0x1b8d, 1000),
    (0x1bc7, 1000),
    (0x1ba4, 1000),
    (0x1ba1, 1000),
    (0x1bfe, 1000),
    (0x2b05, 1000),
    (0x2b20, 1000),
    (0x1b6e, 1000),
    // Mice, 8 kHz.
    (0x1b9e, 8000),
    (0x1bf0, 8000),
    (0x1bf1, 8000),
    (0x2b08, 8000),
    (0x1b79, 8000),
    (0x1b7a, 8000),
    // Mice, 1 kHz.
    (0x1b80, 1000),
    (0x1b7e, 1000),
    (0x1b4c, 1000),
    (0x2b32, 1000),
    (0x1b5e, 1000),
    (0x2b2e, 1000),
    (0x1b93, 1000),
    (0x1bac, 1000),
    (0x1b70, 1000),
    (0x2b03, 1000),
    (0x1bb5, 1000),
    (0x1bd6, 1000),
    (0x1bf2, 1000),
    (0x1be3, 1000),
    (0x1bdb, 1000),
    (0x2b22, 1000),
    (0x1b98, 1000),
    (0x1bb8, 1000),
    (0x1bb2, 1000),
];

/// Wired Bragi PIDs with a poll-rate write that is known but not offered.
const REPORT_RATE_BLOCKED: &[(u16, &str)] = &[(
    0x2b14,
    "boot loops reported on this model (OpenLinkHub #326)",
)];

/// Bragi mice whose angle-snapping write a source applies to that exact PID.
pub const ANGLE_SNAP: &[u16] = &[
    0x1b7e, 0x1b80, 0x1bb2, 0x2b2e, 0x1b5e, 0x2b32, 0x1b4c, 0x2b03, 0x1b9e, 0x1bb5, 0x1bd6, 0x1bf2,
    0x1bf0, 0x1bf1, 0x2b08, 0x1bb8, 0x1b7a, 0x1b79, 0x1b98, 0x1be3, 0x2b22, 0x1bdb, 0x1b70, 0x1b93,
    0x1bac,
];

/// Virtuoso XT (receiver, cable) and SE (receiver, cable): HeadsetControl.
pub const HEADSET_LIGHTS: &[u16] = &[0x0a64, 0x0a62, 0x0a3e, 0x0a3d];
/// Virtuoso XT / SE receivers: sleep timer agreed by both sources.
const XT_SE_RECEIVERS: &[u16] = &[0x0a64, 0x0a3e];
/// Virtuoso XT: sidetone properties with a disputed enable polarity.
const XT: &[u16] = &[0x0a64, 0x0a62];
/// V2 receivers (OpenLinkHub `devices.go`): sidetone and sleep timer, with
/// whether the headset has ANC to switch off (OpenLinkHub writes 0xD1 for
/// the Void V2 and Virtuoso MAX, not the HS80 MAX).
pub const V2: &[(u16, bool)] = &[
    (0x2a08, true),
    (0x2a05, true),
    (0x2a12, true),
    (0x2a02, true),
    (0x2a03, true),
    (0x0a97, false),
    (0x2a0f, false),
    (0x2a0c, false),
];
/// NXP mice in ckb-next `usb.h` `SUPPORTS_ADJRATE`.
pub const NXP_REPORT_RATE: &[u16] = &[
    0x1b12, 0x1b2e, 0x1b5a, 0x1b14, 0x1b19, 0x1b2f, 0x1b32, 0x1b1e, 0x1b3e, 0x1b8b, 0x1b3c, 0x1b75,
    0x1b34, 0x1b74, 0x1b22, 0x1b5c,
];

const NO_SOURCE: &str = "no source writes it for this model";
const VIA_RECEIVER: &str = "set on the wireless receiver, which is not offered";
const SLEEP: &str = "single source (OpenLinkHub) and an unexplained extra property 0x37";
const SLEEP_HEADSET: &str =
    "sources disagree: OpenLinkHub also writes property 0x37, HeadsetControl does not";
const SIDETONE: &str =
    "sources disagree on the enable property 0x46 (HeadsetControl 1 = on, OpenLinkHub 0 = on)";
const DPI_STAGES: &str = "layout only noted in a ckb-next comment; no implementation writes it";
const NXP_SAVE: &str = "single implementation (ckb-next); unsupported on boards with hardware \
                        animations and reports of lost onboard profiles";
const NXP_RATE: &str = "not in ckb-next's list of rate-adjustable mice";

/// Onboard settings of the device with `pid` opened as `family`;
/// `via_receiver` when it sits behind a wireless receiver.
pub fn of(family: Family, pid: u16, via_receiver: bool) -> Onboard {
    let mut o = Onboard::default();
    match family {
        Family::Nxp { leds, .. } => {
            o.save = Offer::Blocked(NXP_SAVE);
            if matches!(leds, Leds::Mouse(_)) {
                o.report_rate_max = 1000;
                o.report_rate = if NXP_REPORT_RATE.contains(&pid) {
                    Offer::Yes
                } else {
                    Offer::Blocked(NXP_RATE)
                };
            }
        }
        Family::Bragi { dpi, wireless, .. } => {
            let wired = !via_receiver && !RECEIVERS.contains(&pid);
            if !wired {
                o.report_rate = Offer::Blocked(VIA_RECEIVER);
            } else if let Some(&(_, max)) = REPORT_RATE.iter().find(|(p, _)| *p == pid) {
                o.report_rate = Offer::Yes;
                o.report_rate_max = max;
            } else {
                o.report_rate = REPORT_RATE_BLOCKED
                    .iter()
                    .find(|(p, _)| *p == pid)
                    .map_or(Offer::Blocked(NO_SOURCE), |&(_, why)| Offer::Blocked(why));
            }
            if dpi.is_some() {
                o.angle_snap = if ANGLE_SNAP.contains(&pid) {
                    Offer::Yes
                } else {
                    Offer::Blocked(NO_SOURCE)
                };
                o.dpi_stages = Offer::Blocked(DPI_STAGES);
            }
            if wireless {
                o.sleep = Offer::Blocked(SLEEP);
            }
        }
        Family::BragiHeadset { .. } => {
            o.lights = if HEADSET_LIGHTS.contains(&pid) {
                Offer::Yes
            } else {
                Offer::Blocked(NO_SOURCE)
            };
            o.sleep = if XT_SE_RECEIVERS.contains(&pid) {
                Offer::Yes
            } else if HEADSET_LIGHTS.contains(&pid) {
                Offer::Blocked(SLEEP_HEADSET)
            } else {
                Offer::Blocked(NO_SOURCE)
            };
            if XT.contains(&pid) {
                o.sidetone = Offer::Blocked(SIDETONE);
            }
            if let Some(&(_, anc)) = V2.iter().find(|(p, _)| *p == pid) {
                o.sidetone = Offer::Yes;
                o.anc = anc;
                o.sleep = Offer::Yes;
            }
        }
        Family::Receiver | Family::Void | Family::VoidWired => {}
    }
    o
}

#[cfg(test)]
mod tests {
    use super::super::models::{spec, SPECS};
    use super::*;
    use std::collections::HashSet;

    fn family(pid: u16) -> Family {
        spec(pid)
            .map(|s| s.family)
            .unwrap_or_else(|| panic!("{pid:04x} has no model"))
    }

    #[test]
    fn tables_name_wired_bragi_models() {
        let mut seen = HashSet::new();
        for &(pid, max) in REPORT_RATE {
            assert!(seen.insert(pid), "{pid:04x} listed twice");
            assert!(matches!(family(pid), Family::Bragi { .. }), "{pid:04x}");
            assert!(!RECEIVERS.contains(&pid), "{pid:04x} is a receiver");
            assert!(max == 1000 || max == 8000, "{pid:04x}");
        }
        for &(pid, _) in REPORT_RATE_BLOCKED {
            assert!(seen.insert(pid), "{pid:04x} both offered and blocked");
        }
        for &pid in ANGLE_SNAP {
            assert!(
                matches!(family(pid), Family::Bragi { dpi: Some(_), .. }),
                "{pid:04x} is not a Bragi mouse"
            );
            assert!(seen.contains(&pid), "{pid:04x} has no poll-rate entry");
        }
        for &pid in HEADSET_LIGHTS {
            assert!(matches!(family(pid), Family::BragiHeadset { wake: false }));
        }
        for &(pid, _) in V2 {
            assert!(matches!(family(pid), Family::BragiHeadset { wake: true }));
        }
        for &pid in NXP_REPORT_RATE {
            assert!(
                matches!(
                    family(pid),
                    Family::Nxp {
                        leds: Leds::Mouse(_),
                        ..
                    }
                ),
                "{pid:04x} is not an NXP mouse"
            );
        }
    }

    #[test]
    fn decisions() {
        // K70 RGB PRO: offered up to 8 kHz, nothing else on a keyboard.
        let k70 = of(family(0x1bc4), 0x1bc4, false);
        assert_eq!((k70.report_rate, k70.report_rate_max), (Offer::Yes, 8000));
        assert_eq!((k70.angle_snap, k70.dpi_stages), (Offer::No, Offer::No));
        // Dark Core RGB PRO on its cable vs behind its receiver.
        let wired = of(family(0x1b80), 0x1b80, false);
        assert_eq!(wired.report_rate, Offer::Yes);
        assert_eq!(wired.angle_snap, Offer::Yes);
        assert!(matches!(wired.sleep, Offer::Blocked(_)));
        assert!(matches!(wired.dpi_stages, Offer::Blocked(_)));
        let dongle = of(family(0x1b80), 0x1b80, true);
        assert_eq!(dongle.report_rate, Offer::Blocked(VIA_RECEIVER));
        // K70 PRO TKL is known but blocked; K65 PLUS has no source.
        assert!(matches!(
            of(family(0x2b14), 0x2b14, false).report_rate,
            Offer::Blocked(r) if r.contains("#326")
        ));
        assert_eq!(
            of(family(0x2b10), 0x2b10, false).report_rate,
            Offer::Blocked(NO_SOURCE)
        );
        // Headsets: lights on the XT/SE only, sidetone blocked on the XT.
        let xt = of(family(0x0a64), 0x0a64, false);
        assert_eq!(xt.lights, Offer::Yes);
        assert!(matches!(xt.sidetone, Offer::Blocked(_)));
        let se = of(family(0x0a3d), 0x0a3d, false);
        assert_eq!((se.lights, se.sidetone), (Offer::Yes, Offer::No));
        // Sleep timer on the XT/SE receivers, blocked on their cables.
        assert_eq!(xt.sleep, Offer::Yes);
        assert_eq!(of(family(0x0a3e), 0x0a3e, false).sleep, Offer::Yes);
        assert_eq!(se.sleep, Offer::Blocked(SLEEP_HEADSET));
        assert_eq!(
            of(family(0x0a62), 0x0a62, false).sleep,
            Offer::Blocked(SLEEP_HEADSET)
        );
        // V2 receivers: sidetone and sleep; ANC only where it exists.
        let void_v2 = of(family(0x2a08), 0x2a08, false);
        assert_eq!((void_v2.sidetone, void_v2.sleep), (Offer::Yes, Offer::Yes));
        assert!(void_v2.anc);
        let hs80_max = of(family(0x0a97), 0x0a97, false);
        assert_eq!((hs80_max.sidetone, hs80_max.anc), (Offer::Yes, false));
        assert_eq!(
            of(family(0x0a6b), 0x0a6b, false).lights,
            Offer::Blocked(NO_SOURCE)
        );
        // NXP: only the blocked profile save; Void headsets: nothing.
        let nxp = of(family(0x1b49), 0x1b49, false);
        assert!(matches!(nxp.save, Offer::Blocked(_)));
        assert_eq!(nxp.report_rate, Offer::No);
        // NXP mice: poll rate up to 1 kHz where ckb-next adjusts it.
        let m65 = of(family(0x1b5a), 0x1b5a, false);
        assert_eq!((m65.report_rate, m65.report_rate_max), (Offer::Yes, 1000));
        assert_eq!(
            of(family(0x1b5d), 0x1b5d, false).report_rate,
            Offer::Blocked(NXP_RATE)
        );
        assert_eq!(of(family(0x0a14), 0x0a14, false), Onboard::default());
    }

    #[test]
    fn every_bragi_model_has_a_decision() {
        for s in SPECS {
            if let Family::Bragi { .. } = s.family {
                for &pid in s.pids {
                    assert_ne!(
                        of(s.family, pid, false).report_rate,
                        Offer::No,
                        "{}",
                        s.name
                    );
                }
            }
        }
    }
}
