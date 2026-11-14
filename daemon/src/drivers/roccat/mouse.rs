//! ROCCAT mice with a host-control ("direct") lighting path.
//!
//! Layouts from OpenRGB `RoccatKoneAimoController`, `RoccatKoneXPController`,
//! `RoccatKoneProController`, `RoccatBurstController` and
//! `RoccatKoneProAirController`. All but the Kone Pro Air take host control
//! with feature report `0e 06 01 <on> 00 ff` (OpenRGB's `SwitchControl` /
//! `EnableDirect`; `<on> = 0` "switches to built-in mode") and then accept
//! volatile color frames on report 0x0D. The Kone Pro Air takes a volatile
//! output report (`10 10 ..`; its profile writes use `10 50 ..`, never sent)
//! and has no documented switch back, so it keeps the host color until it is
//! power-cycled.
//!
//! Kone Pro Air battery (read-only, single source: Eruption
//! `roccat_kone_pro_air.rs` `update_device_status`): output report `90 0a`
//! (64 bytes) on the same node as the lighting (dongle interface 2, cable
//! interface 1, as OpenRGB); the reply `90 0a ..` carries the percentage at
//! byte 10.
//!
//! Onboard memory: the lighting profile writes (OpenRGB `SetMode`, report 6
//! with a checksum; Kone XP's 0xAE-byte block also holds DPI and report rate)
//! have no second source and are blocked placeholders. The Kone AIMO has no
//! documented save at all (OpenRGB `@save :x:`).

use super::{
    open_iface, read_until, save_blocked, wait_ready, Adapter, Lights, Spec, ONBOARD_STATIC,
    OPENRGB_ONLY, ROCCAT, STICKY_STATIC,
};
use crate::drivers::{DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::Result;
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption};
use hako_core::device::DeviceKind;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// 46-byte frame `0d 2e` + 11 × `R G B 00` (interface 0).
    Aimo,
    /// 122-byte frame `0d 7a` + 20 × `R G B ff ff ff` (interface 3).
    Xp,
    /// 11-byte frame `0d 0b` + `leds` × `R G B` (interface 3).
    Pro { leds: usize },
    /// 16-byte output report, 2 LEDs (dongle interface 2, cable interface 1).
    ProAir,
}

pub(super) const KONE_AIMO: Spec<Family> = Spec {
    name: "ROCCAT Kone AIMO",
    vid: ROCCAT,
    pids: &[0x2e27],
    family: Family::Aimo,
};
pub(super) const KONE_AIMO_16K: Spec<Family> = Spec {
    name: "ROCCAT Kone AIMO 16K",
    vid: ROCCAT,
    pids: &[0x2e2c],
    family: Family::Aimo,
};
pub(super) const KONE_XP: Spec<Family> = Spec {
    name: "ROCCAT Kone XP",
    vid: ROCCAT,
    pids: &[0x2c8b],
    family: Family::Xp,
};
pub(super) const KONE_PRO: Spec<Family> = Spec {
    name: "ROCCAT Kone Pro",
    vid: ROCCAT,
    pids: &[0x2c88],
    family: Family::Pro { leds: 2 },
};
pub(super) const KONE_PRO_AIR: Spec<Family> = Spec {
    name: "ROCCAT Kone Pro Air",
    vid: ROCCAT,
    pids: &[0x2c8e, 0x2c92],
    family: Family::ProAir,
};
pub(super) const BURST_PRO: Spec<Family> = Spec {
    name: "ROCCAT Burst Pro",
    vid: ROCCAT,
    pids: &[0x2de1],
    family: Family::Pro { leds: 2 },
};
pub(super) const BURST_CORE: Spec<Family> = Spec {
    name: "ROCCAT Burst Core",
    vid: ROCCAT,
    pids: &[0x2de6],
    family: Family::Pro { leds: 1 },
};

#[cfg(test)]
pub(super) const ALL: &[&Spec<Family>] = &[
    &KONE_AIMO,
    &KONE_AIMO_16K,
    &KONE_XP,
    &KONE_PRO,
    &KONE_PRO_AIR,
    &BURST_PRO,
    &BURST_CORE,
];

pub(super) fn open(spec: &'static Spec<Family>, pid: u16, ctx: &DriverCtx) -> Opened {
    let iface = match (spec.family, pid) {
        (Family::Aimo, _) => 0,
        (Family::ProAir, 0x2c92) => 1,
        (Family::ProAir, _) => 2,
        _ => 3,
    };
    let (dev, instance) = open_iface(spec.vid, pid, iface)?;
    let lights = Mouse {
        dev,
        family: spec.family,
        host: false,
    };
    Adapter::open(
        spec,
        DeviceKind::Mouse,
        pid,
        instance,
        Box::new(lights),
        ctx,
    )
}

/// Host control on (`true`) or back to the onboard profile.
fn switch(on: bool) -> [u8; 6] {
    [0x0e, 0x06, 0x01, u8::from(on), 0x00, 0xff]
}

fn aimo_frame([r, g, b]: [u8; 3]) -> [u8; 46] {
    let mut p = [0u8; 46];
    p[..2].copy_from_slice(&[0x0d, 0x2e]);
    for led in p[2..].as_chunks_mut::<4>().0 {
        led[..3].copy_from_slice(&[r, g, b]);
    }
    p
}

fn xp_frame([r, g, b]: [u8; 3]) -> [u8; 0x7a] {
    let mut p = [0u8; 0x7a];
    p[..2].copy_from_slice(&[0x0d, 0x7a]);
    for led in p[2..].as_chunks_mut::<6>().0 {
        *led = [r, g, b, 0xff, 0xff, 0xff];
    }
    p
}

fn pro_frame(leds: usize, color: [u8; 3]) -> [u8; 11] {
    let mut p = [0u8; 11];
    p[..2].copy_from_slice(&[0x0d, 0x0b]);
    for led in p[2..].as_chunks_mut::<3>().0.iter_mut().take(leds) {
        *led = color;
    }
    p
}

/// Direct mode 0x09, per-LED brightness 0x64, global brightness 0x64,
/// speed 0x0b, then both LEDs.
fn pro_air_frame([r, g, b]: [u8; 3]) -> [u8; 16] {
    [
        0x00, 0x10, 0x10, 0x0b, 0x00, 0x09, 0x64, 0x64, 0x64, 0x0b, r, g, b, r, g, b,
    ]
}

/// Kone Pro Air battery query: report id 0, then `90 0a`.
fn battery_query() -> [u8; 65] {
    let mut p = [0u8; 65];
    p[1..3].copy_from_slice(&[0x90, 0x0a]);
    p
}

/// Percentage from a `90 0a` reply (byte 10); `None` for other reports or
/// readings above 100.
fn parse_battery(r: &[u8]) -> Option<u8> {
    match r {
        [0x90, 0x0a, ..] => r.get(10).copied().filter(|&p| p <= 100),
        _ => None,
    }
}

/// Blocked onboard-memory placeholders for `f`.
fn memory_controls(f: Family) -> Vec<SettingMeta> {
    use SettingKey::*;
    match f {
        Family::Aimo => Vec::new(),
        Family::Pro { .. } | Family::ProAir => vec![save_blocked(OPENRGB_ONLY)],
        Family::Xp => {
            let mut c = vec![save_blocked(OPENRGB_ONLY)];
            for (i, key) in [DpiStage1, DpiStage2, DpiStage3, DpiStage4, DpiStage5]
                .into_iter()
                .enumerate()
            {
                c.push(
                    SettingMeta::slider(key, &format!("DPI stage {}", i + 1), 50.0, 19000.0, 50.0)
                        .unit("dpi")
                        .persistent()
                        .blocked(OPENRGB_ONLY),
                );
            }
            c.push(
                SettingMeta::dropdown(
                    ReportRate,
                    "Report rate",
                    [125, 250, 500, 1000]
                        .map(|r| SettingOption::int(r, &format!("{r} Hz")))
                        .to_vec(),
                )
                .persistent()
                .blocked(OPENRGB_ONLY),
            );
            c
        }
    }
}

struct Mouse {
    dev: HidrawDevice,
    family: Family,
    /// Host control is on (we switched it on and have not switched back).
    host: bool,
}

impl Mouse {
    fn take_control(&mut self) -> Result<()> {
        if self.host || self.family == Family::ProAir {
            return Ok(());
        }
        self.dev.set_feature(&mut switch(true))?;
        if self.family == Family::Xp {
            wait_ready(&self.dev, 0x04, 4, Duration::from_millis(200), 5);
        }
        self.host = true;
        Ok(())
    }
}

impl Lights for Mouse {
    fn effects(&self) -> &'static [(&'static str, &'static str)] {
        if self.family == Family::ProAir {
            STICKY_STATIC
        } else {
            ONBOARD_STATIC
        }
    }

    fn apply(&mut self, effect: &str, color: [u8; 3]) -> Result<()> {
        if effect != "static" {
            return self.release();
        }
        self.take_control()?;
        match self.family {
            Family::Aimo => {
                self.dev.set_feature(&mut aimo_frame(color))?;
                // Swarm reads the frame report back; the reply is unused.
                let _ = self.dev.get_feature(&mut [0x0d, 0, 0]);
            }
            Family::Xp => self.dev.set_feature(&mut xp_frame(color))?,
            Family::Pro { leds } => self.dev.set_feature(&mut pro_frame(leds, color))?,
            Family::ProAir => self.dev.write(&pro_air_frame(color))?,
        }
        Ok(())
    }

    fn extra_controls(&self) -> Vec<SettingMeta> {
        memory_controls(self.family)
    }

    fn release(&mut self) -> Result<()> {
        if self.host {
            self.dev.set_feature(&mut switch(false))?;
            self.host = false;
        }
        Ok(())
    }

    fn battery_label(&self) -> Option<&'static str> {
        (self.family == Family::ProAir).then_some("Mouse")
    }

    fn battery(&mut self) -> Result<Option<u8>> {
        self.dev.write(&battery_query())?;
        let r = read_until(&mut self.dev, Duration::from_millis(200), |r| {
            r.starts_with(&[0x90, 0x0a])
        })?;
        Ok(r.and_then(|r| parse_battery(&r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_writes_are_blocked_placeholders() {
        assert!(memory_controls(Family::Aimo).is_empty());
        for f in [Family::Xp, Family::Pro { leds: 2 }, Family::ProAir] {
            let c = memory_controls(f);
            assert!(!c.is_empty());
            assert!(c.iter().all(|m| m.persistent && m.blocked.is_some()));
        }
    }

    #[test]
    fn pro_air_battery() {
        let q = battery_query();
        assert_eq!(q[..4], [0x00, 0x90, 0x0a, 0x00]);
        assert!(q[3..].iter().all(|&b| b == 0));
        let mut r = [0u8; 64];
        r[..2].copy_from_slice(&[0x90, 0x0a]);
        r[10] = 73;
        assert_eq!(parse_battery(&r), Some(73));
        r[10] = 0xff;
        assert_eq!(parse_battery(&r), None);
        // The `90 70` signal reply is not a battery reading.
        r[1] = 0x70;
        r[10] = 50;
        assert_eq!(parse_battery(&r), None);
    }

    #[test]
    fn switch_packets() {
        assert_eq!(switch(true), [0x0e, 0x06, 0x01, 0x01, 0x00, 0xff]);
        assert_eq!(switch(false), [0x0e, 0x06, 0x01, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn aimo_frame_layout() {
        let f = aimo_frame([1, 2, 3]);
        assert_eq!(f[..10], [0x0d, 0x2e, 1, 2, 3, 0, 1, 2, 3, 0]);
        // LED 10 (lower right) at 2 + 40.
        assert_eq!(f[42..46], [1, 2, 3, 0]);
    }

    #[test]
    fn xp_frame_layout() {
        let f = xp_frame([1, 2, 3]);
        assert_eq!(f[..8], [0x0d, 0x7a, 1, 2, 3, 0xff, 0xff, 0xff]);
        // LED 19 (DPI button) at 2 + 19 * 6.
        assert_eq!(f[116..122], [1, 2, 3, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn pro_frames() {
        assert_eq!(
            pro_frame(2, [1, 2, 3]),
            [0x0d, 0x0b, 1, 2, 3, 1, 2, 3, 0, 0, 0]
        );
        assert_eq!(
            pro_frame(1, [1, 2, 3]),
            [0x0d, 0x0b, 1, 2, 3, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            pro_air_frame([1, 2, 3]),
            [0, 0x10, 0x10, 0x0b, 0, 0x09, 0x64, 0x64, 0x64, 0x0b, 1, 2, 3, 1, 2, 3]
        );
    }
}
