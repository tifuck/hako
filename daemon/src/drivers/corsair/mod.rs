//! Corsair (VID 0x1b1c): iCUE-class keyboards, mice and headsets.
//!
//! Three wire protocols, all on a vendor HID collection found by usage page:
//! - legacy "NXP" (`nxp.rs`): K70 RGB MK.2, K95 PLATINUM, M65 RGB ELITE...
//! - "Bragi" (`bragi.rs`): K70 RGB PRO, K100, K65 MINI, DARK CORE RGB PRO,
//!   M65 RGB ULTRA, Slipstream receivers, Virtuoso/HS80 headsets.
//! - Void receivers (`headset.rs`): VOID / VOID ELITE / HS70 wireless, and
//!   wired USB Voids (lights only).
//!
//! Lighting and DPI use volatile host ("software") mode: the host takes
//! them while the daemon runs and hands control back to the onboard
//! firmware on shutdown or when the effect is set to "onboard". Bragi
//! property writes persist on the device, so the Bragi poll rate, angle
//! snapping, headset lights, V2 headset sidetone, headset sleep timer and
//! the NXP mouse poll rate are onboard-memory controls (`persistent`,
//! behind the owner's opt-in), offered only on the models `onboard.rs`
//! lists, written only for a user change and read back from the device on
//! connect; other researched onboard writes are shown blocked with the
//! reason (`docs/onboard-writes/corsair.md`). NXP mice get live lift-off
//! height and angle snapping: ckb-next `dpi.c` writes those with byte 3 = 0
//! and only uses byte 3 = 1 to save them to the profile.
//! Brightness scales the colour on the host; the hardware brightness
//! property is left alone.
//!
//! Multi-device Slipstream receivers are `multi`: a rescan only probes
//! pairing slots that have no open adapter yet.
//!
//! Protocol sources (reimplemented, no code copied; all GPL projects are
//! cited for byte layouts only):
//! - ckb-next `src/daemon/` (GPL-2.0): `usb.h`, `nxp_proto.h`,
//!   `led_keyboard.c`, `led_mouse.c`, `dpi.c`, `device_keyboard.c`,
//!   `bragi_proto.h`, `bragi_common.c`, `device_bragi.c`, `led_bragi.c`,
//!   `dpi_bragi.c`, `usb_bragi.c`, `device_vtable.c`, `profile_keyboard.c`,
//!   `profile_mouse.c`.
//! - ckb-next/corsair-protocol (CC BY-SA 4.0 docs): `host-to-device/07/*`.
//! - OpenRGB (GPL-2.0) `Controllers/CorsairPeripheralController/*`,
//!   `Controllers/CorsairPeripheralV2Controller/*`.
//! - OpenLinkHub (GPL-3.0) `src/devices/devices.go` and per-device
//!   `src/devices/<name>/<name>.go` (PIDs, LED counts, DPI ranges).
//! - Linux `drivers/hid/hid-corsair-void.c` (GPL-2.0).
//! - HeadsetControl (GPL-3.0) `lib/devices/corsair_device.hpp`,
//!   `corsair_void_rich.hpp`, `corsair_virtuoso_xt.hpp`, `corsair_void_v2w.hpp`.
//! - github.com/joanjgm/hs80-battery README; HaloBattery `docs/protocols.md`.

mod bragi;
mod headset;
mod hid;
mod models;
mod nxp;
mod onboard;

pub use models::MODELS;

use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{anyhow, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use models::{Family, Spec};
use onboard::{Offer, Onboard};
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub const VID: u16 = 0x1b1c;

const KEEPALIVE: Duration = Duration::from_secs(20);
const BATTERY_POLL: Duration = Duration::from_secs(30);
/// Consecutive failed polls before a device counts as gone (asleep, unplugged).
const MAX_FAILURES: u32 = 3;

/// One protocol family's device I/O. Defaults cover what a family lacks.
trait Backend: Send {
    /// Take (true) or hand back (false) control from the onboard firmware.
    fn software(&mut self, _on: bool) -> Result<()> {
        Ok(())
    }
    fn color(&mut self, _rgb: [u8; 3]) -> Result<()> {
        Ok(())
    }
    fn dpi(&mut self, _dpi: u16) -> Result<()> {
        Ok(())
    }
    fn sidetone(&mut self, _level: u8) -> Result<()> {
        anyhow::bail!("no sidetone control")
    }
    fn lift(&mut self, _level: u8) -> Result<()> {
        anyhow::bail!("no lift-off control")
    }
    fn angle_snap(&mut self, _on: bool) -> Result<()> {
        anyhow::bail!("no angle snapping control")
    }
    fn lights(&mut self, _on: bool) -> Result<()> {
        anyhow::bail!("no lights switch")
    }
    /// None when the device has no battery.
    fn battery(&mut self) -> Result<Option<BatteryState>> {
        Ok(None)
    }
    fn keepalive(&mut self) -> Result<()> {
        Ok(())
    }
    fn read_dpi(&mut self) -> Option<u16> {
        None
    }
    fn report_rate(&mut self) -> Option<u32> {
        None
    }
    /// Store the poll rate in onboard memory; Bragi property 0x01 here.
    fn set_report_rate(&mut self, hz: u32) -> Result<()> {
        let v = bragi::pollrate_value(hz)
            .with_context(|| format!("{hz} Hz is not a Bragi poll rate"))?;
        self.store(&[(bragi::PROP_POLLRATE, v)])
    }
    /// Write Bragi properties kept in onboard memory, in order
    /// (`bragi::store`).
    fn store(&mut self, _writes: &[(u8, u32)]) -> Result<()> {
        anyhow::bail!("no onboard settings")
    }
    /// A Bragi property's current value.
    fn stored(&mut self, _prop: u8) -> Option<u32> {
        None
    }
    /// Boom mic raised, from the last battery poll.
    fn mic_up(&self) -> Option<bool> {
        None
    }
}

/// What the UI may offer for a device.
#[derive(Clone, Copy, Default)]
struct Caps {
    lighting: bool,
    dpi: Option<(u16, u16)>,
    /// NXP mouse lift-off height and angle snapping.
    tuning: bool,
    sidetone: bool,
    /// Headset lights on/off (the effect dropdown with "on" / "off").
    lights: bool,
    /// Settings kept in onboard memory, for the opened PID.
    onboard: Onboard,
}

impl Caps {
    fn of(family: Family) -> Self {
        match family {
            Family::Nxp { leds, dpi } => Self {
                lighting: true,
                dpi,
                tuning: matches!(leds, nxp::Leds::Mouse(_)),
                ..Self::default()
            },
            Family::Bragi { leds, dpi, .. } => Self {
                lighting: leds > 0,
                dpi,
                ..Self::default()
            },
            Family::Void => Self {
                sidetone: true,
                lights: true,
                ..Self::default()
            },
            Family::VoidWired => Self {
                lights: true,
                ..Self::default()
            },
            Family::Receiver | Family::BragiHeadset { .. } => Self::default(),
        }
    }

    /// Caps of `family` opened as `pid`, with its onboard settings.
    fn opened(family: Family, pid: u16, via_receiver: bool) -> Self {
        Self {
            onboard: onboard::of(family, pid, via_receiver),
            ..Self::of(family)
        }
    }
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let spec = models::spec(pid).context("unknown Corsair PID")?;
    if models::RECEIVERS.contains(&pid) {
        return open_receiver(pid, spec, ctx);
    }
    let (info, backend): (_, Box<dyn Backend>) = match spec.family {
        Family::Nxp { leds, .. } => {
            let info = hid::find_node(VID, pid, nxp::USAGE_PAGE, Some(nxp::IFACE))?;
            let dev = HidrawDevice::open(&info.path)?;
            (info, Box::new(nxp::Nxp::open(dev, leds)))
        }
        Family::Bragi { leds, wireless, .. } => {
            let info = hid::find_node(VID, pid, bragi::USAGE_PAGE, Some(1))?;
            let mut b = bragi::Bragi::new(
                bragi::Link::open(&info, false)?,
                bragi::SELF,
                leds,
                wireless,
            );
            b.probe()?;
            (info, Box::new(b))
        }
        Family::Void | Family::VoidWired => {
            let wired = matches!(spec.family, Family::VoidWired);
            let info = hid::find_node(
                VID,
                pid,
                headset::VOID_USAGE_PAGE,
                Some(headset::VOID_IFACE),
            )?;
            let dev = HidrawDevice::open(&info.path)?;
            let sidetone = if wired {
                None
            } else {
                hid::find_node(
                    VID,
                    pid,
                    headset::SIDETONE_USAGE_PAGE,
                    Some(headset::SIDETONE_IFACE),
                )
                .and_then(|n| HidrawDevice::open(&n.path))
                .inspect_err(|e| warn!("{}: sidetone node: {e}", spec.name))
                .ok()
            };
            (info, Box::new(headset::Void::new(dev, sidetone, wired)))
        }
        Family::BragiHeadset { wake } => {
            let iface = if wake && !models::V2_IFACE3.contains(&pid) {
                4
            } else {
                3
            };
            let info = hid::find_node(VID, pid, bragi::USAGE_PAGE, Some(iface))?;
            let link = bragi::Link::open(&info, true)?;
            (info, Box::new(headset::BragiHeadset::new(link, wake)))
        }
        Family::Receiver => unreachable!("receiver PIDs are listed in RECEIVERS"),
    };
    let id = DeviceId {
        vendor_id: VID,
        product_id: pid,
        instance: hid::instance(&info),
    };
    info!("{} open via {}", spec.name, info.path.display());
    Ok(vec![Box::new(Peripheral::new(
        spec.name.into(),
        spec.kind,
        Caps::opened(spec.family, pid, false),
        id,
        spec.pids[0],
        backend,
        ctx.paths.clone(),
    )?)])
}

/// Instance key of the device in pairing slot `addr` behind a receiver.
fn slot_instance(receiver: &str, addr: u8) -> String {
    format!("{receiver}#{addr:02x}")
}

/// A slot whose device already has an adapter (`ctx.open` on a rescan).
fn slot_open(open: &[DeviceId], receiver: &str, addr: u8) -> bool {
    let key = slot_instance(receiver, addr);
    open.iter().any(|id| id.instance == key)
}

/// One adapter per device paired with (and awake behind) the receiver,
/// except those already open.
fn open_receiver(pid: u16, spec: &'static Spec, ctx: &DriverCtx) -> Opened {
    let info = hid::find_node(VID, pid, bragi::USAGE_PAGE, Some(1))?;
    let receiver = hid::instance(&info);
    let paired = bragi::paired(&mut bragi::Link::open(&info, false)?, |addr| {
        slot_open(&ctx.open, &receiver, addr)
    })?;
    let mut out: Vec<Box<dyn DeviceAdapter>> = Vec::new();
    for (addr, child_pid) in paired {
        // Paired devices report their own (wired) PID.
        let child = models::spec(child_pid)
            .filter(|s| matches!(s.family, Family::Bragi { .. }))
            .or(matches!(spec.family, Family::Bragi { .. }).then_some(spec));
        let (name, kind, family, settings_pid) = match child {
            Some(s) => (s.name.to_string(), s.kind, s.family, s.pids[0]),
            None => (
                format!("Corsair wireless device {child_pid:04x}"),
                spec.kind,
                Family::Bragi {
                    leds: 0,
                    dpi: None,
                    wireless: true,
                },
                child_pid,
            ),
        };
        let Family::Bragi { leds, .. } = family else {
            continue;
        };
        let mut b = bragi::Bragi::new(bragi::Link::open(&info, false)?, addr, leds, true);
        if let Err(e) = b.probe() {
            warn!("{name} behind {pid:04x}: {e}");
            continue;
        }
        // The device's own PID: devices behind one receiver stay apart (remaps,
        // profiles) and match their wired mode. `Model::remap_pids` adds the
        // receiver's PID, which its input nodes carry.
        let id = DeviceId {
            vendor_id: VID,
            product_id: settings_pid,
            instance: slot_instance(&receiver, addr),
        };
        info!("{name} open behind receiver {pid:04x} (addr {addr:#04x})");
        out.push(Box::new(Peripheral::new(
            name,
            kind,
            Caps::opened(family, settings_pid, true),
            id,
            settings_pid,
            Box::new(b),
            ctx.paths.clone(),
        )?));
    }
    Ok(out)
}

const EFFECTS: [(&str, &str); 3] = [
    ("static", "Static"),
    ("off", "Off"),
    ("onboard", "Onboard (device)"),
];

fn defaults(s: &mut SettingsMap) {
    for (k, v) in [
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::Brightness, SettingValue::Int(100)),
        (SettingKey::ColorR, SettingValue::Int(0)),
        (SettingKey::ColorG, SettingValue::Int(120)),
        (SettingKey::ColorB, SettingValue::Int(255)),
    ] {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

fn int(s: &SettingsMap, key: SettingKey) -> Option<i64> {
    get_setting(s, key).and_then(|v| v.as_int())
}

fn flag(s: &SettingsMap, key: SettingKey) -> Option<bool> {
    get_setting(s, key).and_then(|v| v.as_bool())
}

const LIGHTS: [(&str, &str); 2] = [("on", "On"), ("off", "Off")];

fn effect(s: &SettingsMap) -> &str {
    get_setting(s, SettingKey::Effect)
        .and_then(|v| v.as_str())
        .unwrap_or("static")
}

/// The colour the LEDs get: brightness is applied on the host.
fn frame_color(s: &SettingsMap) -> [u8; 3] {
    if effect(s) == "off" {
        return [0; 3];
    }
    let pct = int(s, SettingKey::Brightness).unwrap_or(100).clamp(0, 100);
    [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB]
        .map(|k| (int(s, k).unwrap_or(0).clamp(0, 255) * pct / 100) as u8)
}

struct Peripheral {
    id: DeviceId,
    name: String,
    kind: DeviceKind,
    caps: Caps,
    settings_pid: u16,
    paths: ConfigPaths,
    backend: Box<dyn Backend>,
    settings: SettingsMap,
    batteries: Vec<BatteryState>,
    report_rate: Option<u32>,
    /// Highest poll rate the device itself reports (ckb-next property 0x96).
    max_report_rate: Option<u32>,
    /// The device answered the angle-snapping property on connect.
    angle_snap: bool,
    connected: bool,
    last_error: Option<String>,
    last_keepalive: Instant,
    last_battery: Option<Instant>,
    failures: u32,
}

impl Peripheral {
    fn new(
        name: String,
        kind: DeviceKind,
        caps: Caps,
        id: DeviceId,
        settings_pid: u16,
        backend: Box<dyn Backend>,
        paths: ConfigPaths,
    ) -> Result<Self> {
        // An unreadable file stays an error: defaults would overwrite it on
        // the next write.
        let mut settings = hako_core::config::load_device_settings(&paths, VID, settings_pid)?;
        if caps.lighting {
            defaults(&mut settings);
        }
        let mut p = Self {
            id,
            name,
            kind,
            caps,
            settings_pid,
            paths,
            backend,
            settings,
            batteries: Vec::new(),
            report_rate: None,
            max_report_rate: None,
            angle_snap: false,
            connected: true,
            last_error: None,
            last_keepalive: Instant::now(),
            last_battery: None,
            failures: 0,
        };
        let offered = p.controls();
        crate::drivers::sanitize_saved(&mut p.settings, &offered);
        // Lighting is volatile: re-apply the saved state on every connect.
        if let Err(e) = p.apply(true) {
            warn!("{}: {e}", p.name);
            p.last_error = Some(e.to_string());
        }
        if caps.dpi.is_some() && int(&p.settings, SettingKey::Dpi).is_none() {
            if let Some(dpi) = p.backend.read_dpi().filter(|&d| d >= 50) {
                merge_setting(
                    &mut p.settings,
                    SettingKey::Dpi,
                    SettingValue::Int(dpi.into()),
                );
            }
        }
        p.report_rate = p.backend.report_rate();
        if let Some(hz) = p.report_rate {
            merge_setting(
                &mut p.settings,
                SettingKey::ReportRate,
                SettingValue::Int(hz.into()),
            );
        }
        p.poll_battery();
        p.read_onboard();
        Ok(p)
    }

    /// Show what the device keeps in onboard memory; never written here.
    fn read_onboard(&mut self) {
        let o = self.caps.onboard;
        if o.report_rate == Offer::Yes {
            self.max_report_rate = self
                .backend
                .stored(bragi::PROP_MAX_POLLRATE)
                .and_then(bragi::pollrate_hz);
        }
        if o.angle_snap == Offer::Yes {
            let snap = self
                .backend
                .stored(bragi::PROP_ANGLE_SNAP)
                .filter(|&v| v <= 1);
            self.angle_snap = snap.is_some();
            if let Some(v) = snap {
                merge_setting(
                    &mut self.settings,
                    SettingKey::AngleSnapping,
                    SettingValue::Bool(v == 1),
                );
            }
        }
        if o.sidetone == Offer::Yes {
            let off = self.backend.stored(bragi::PROP_SIDETONE_OFF);
            let level = self.backend.stored(bragi::PROP_SIDETONE_LEVEL);
            if let Some(pct) = off
                .zip(level)
                .and_then(|(o, l)| headset::v2_sidetone_pct(o, l))
            {
                merge_setting(
                    &mut self.settings,
                    SettingKey::MicSideTone,
                    SettingValue::Int(pct.into()),
                );
            }
        }
        if o.sleep == Offer::Yes {
            let on = self.backend.stored(bragi::PROP_SLEEP_ENABLED);
            let ms = self.backend.stored(bragi::PROP_SLEEP_TIMEOUT);
            if let Some(min) = on.zip(ms).and_then(|(e, t)| headset::sleep_minutes(e, t)) {
                merge_setting(
                    &mut self.settings,
                    SettingKey::PmShutdown,
                    SettingValue::Int(min.into()),
                );
            }
        }
        if o.lights == Offer::Yes {
            if let Some(v) = self
                .backend
                .stored(bragi::PROP_BRIGHTNESS)
                .filter(|&v| v <= bragi::BRIGHTNESS_MAX)
            {
                let state = if v == 0 { "off" } else { "on" };
                merge_setting(
                    &mut self.settings,
                    SettingKey::Effect,
                    SettingValue::Text(state.into()),
                );
            }
        }
    }

    /// Poll rates the onboard write may select.
    fn report_rates(&self) -> Vec<u32> {
        let max = self
            .max_report_rate
            .map_or(self.caps.onboard.report_rate_max, |m| {
                m.min(self.caps.onboard.report_rate_max)
            });
        (1..=7)
            .filter_map(bragi::pollrate_hz)
            .filter(|&hz| hz <= max)
            .collect()
    }

    fn onboard(&self) -> bool {
        effect(&self.settings) == "onboard"
    }

    /// Push lighting, and the saved DPI / tuning when `with_dpi`, in host
    /// mode; "onboard" hands everything back to the firmware instead.
    /// Headset lights are only sent once the user picked a state.
    fn apply(&mut self, with_dpi: bool) -> Result<()> {
        if self.caps.lights {
            if let Some(state) = get_setting(&self.settings, SettingKey::Effect) {
                self.backend.lights(state.as_str() != Some("off"))?;
            }
        }
        if !self.caps.lighting {
            return Ok(());
        }
        if self.onboard() {
            return self.backend.software(false);
        }
        self.backend.software(true)?;
        self.backend.color(frame_color(&self.settings))?;
        if !with_dpi {
            return Ok(());
        }
        if let (Some(dpi), Some((min, max))) = (int(&self.settings, SettingKey::Dpi), self.caps.dpi)
        {
            self.backend
                .dpi(dpi.clamp(i64::from(min), i64::from(max)) as u16)?;
        }
        if self.caps.tuning {
            if let Some(on) = flag(&self.settings, SettingKey::AngleSnapping) {
                self.backend.angle_snap(on)?;
            }
            if let Some(level) = int(&self.settings, SettingKey::LiftOffDistance) {
                let level = level.clamp(nxp::LIFT_MIN.into(), nxp::LIFT_MAX.into());
                self.backend.lift(level as u8)?;
            }
        }
        Ok(())
    }

    fn poll_battery(&mut self) {
        self.last_battery = Some(Instant::now());
        match self.backend.battery() {
            Ok(b) => {
                self.failures = 0;
                self.batteries = b.into_iter().collect();
                if let Some(up) = self.backend.mic_up() {
                    merge_setting(
                        &mut self.settings,
                        SettingKey::MicMute,
                        SettingValue::Bool(up),
                    );
                }
            }
            Err(e) => self.failed(e),
        }
    }

    fn failed(&mut self, e: anyhow::Error) {
        self.failures += 1;
        if self.failures >= MAX_FAILURES && self.connected {
            // Asleep, out of range or unplugged: discovery reopens it.
            info!("{}: lost ({e})", self.name);
            self.connected = false;
            self.last_error = Some(format!("not responding: {e}"));
        }
    }

    fn controls(&self) -> Vec<SettingMeta> {
        use SettingKey::*;
        let mut c = Vec::new();
        if let Some((min, max)) = self.caps.dpi {
            c.push(
                SettingMeta::slider(Dpi, "DPI", min.into(), max.into(), 50.0)
                    .unit("dpi")
                    .hidden(self.onboard()),
            );
        }
        if self.caps.tuning {
            c.push(SettingMeta::toggle(AngleSnapping, "Angle snapping").hidden(self.onboard()));
            c.push(
                SettingMeta::slider(
                    LiftOffDistance,
                    "Lift-off height",
                    nxp::LIFT_MIN.into(),
                    nxp::LIFT_MAX.into(),
                    1.0,
                )
                .hidden(self.onboard()),
            );
        }
        let o = self.caps.onboard;
        if self.caps.dpi.is_some() && !self.caps.tuning {
            match o.angle_snap {
                Offer::Yes if self.angle_snap => {
                    c.push(SettingMeta::toggle(AngleSnapping, "Angle snapping").persistent())
                }
                Offer::Blocked(why) => c.push(
                    SettingMeta::toggle(AngleSnapping, "Angle snapping")
                        .persistent()
                        .blocked(why),
                ),
                _ => {}
            }
        }
        if let (Some((min, max)), Offer::Blocked(why)) = (self.caps.dpi, o.dpi_stages) {
            c.push(
                SettingMeta::slider(
                    DpiStage1,
                    "Onboard DPI stages",
                    min.into(),
                    max.into(),
                    50.0,
                )
                .unit("dpi")
                .persistent()
                .blocked(why),
            );
        }
        if self.report_rate.is_some() {
            let rate = match o.report_rate {
                Offer::Yes => SettingMeta::dropdown(
                    ReportRate,
                    "Report rate",
                    self.report_rates()
                        .into_iter()
                        .map(|hz| SettingOption::int(hz.into(), &format!("{hz} Hz")))
                        .collect(),
                )
                .persistent(),
                Offer::Blocked(why) => {
                    SettingMeta::readonly(ReportRate, "Report rate", 125.0, 8000.0)
                        .persistent()
                        .blocked(why)
                }
                Offer::No => SettingMeta::readonly(ReportRate, "Report rate", 125.0, 8000.0),
            };
            c.push(rate.unit("Hz"));
        }
        match o.sleep {
            // 0 switches the timer off (HeadsetControl, OpenLinkHub).
            Offer::Yes => c.push(
                SettingMeta::slider(
                    PmShutdown,
                    "Sleep timer (0 = off)",
                    0.0,
                    (bragi::SLEEP_MAX_MS / bragi::MINUTE_MS).into(),
                    1.0,
                )
                .unit("min")
                .persistent(),
            ),
            Offer::Blocked(why) => c.push(
                SettingMeta::slider(PmShutdown, "Sleep timer", 1.0, 60.0, 1.0)
                    .unit("min")
                    .persistent()
                    .blocked(why),
            ),
            Offer::No => {}
        }
        let lights = || {
            SettingMeta::dropdown(
                Effect,
                "Lights",
                LIGHTS
                    .iter()
                    .map(|(v, l)| SettingOption::text(v, l))
                    .collect(),
            )
        };
        match o.lights {
            Offer::Yes => c.push(lights().persistent()),
            Offer::Blocked(why) => c.push(lights().persistent().blocked(why)),
            Offer::No => {}
        }
        let sidetone = || {
            SettingMeta::slider(MicSideTone, "Sidetone", 0.0, 100.0, 1.0)
                .unit("%")
                .persistent()
        };
        match o.sidetone {
            Offer::Yes => c.push(sidetone()),
            Offer::Blocked(why) => c.push(sidetone().blocked(why)),
            Offer::No => {}
        }
        if let Offer::Blocked(why) = o.save {
            c.push(
                SettingMeta::toggle(SaveToDevice, "Save changes to device memory")
                    .persistent()
                    .blocked(why),
            );
        }
        if self.caps.lights {
            c.push(lights());
        }
        if self.caps.lighting {
            let static_color = effect(&self.settings) == "static";
            c.push(SettingMeta::dropdown(
                Effect,
                "Effect",
                EFFECTS
                    .iter()
                    .map(|(v, l)| SettingOption::text(v, l))
                    .collect(),
            ));
            c.push(
                SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 1.0)
                    .unit("%")
                    .hidden(!static_color),
            );
            for (key, label) in [(ColorR, "Red"), (ColorG, "Green"), (ColorB, "Blue")] {
                c.push(SettingMeta::color(key, label).hidden(!static_color));
            }
        }
        if self.caps.sidetone {
            c.push(SettingMeta::slider(
                MicSideTone,
                "Sidetone",
                0.0,
                headset::SIDETONE_MAX.into(),
                1.0,
            ));
            c.push(SettingMeta::readonly(
                MicMute,
                "Muted (mic raised)",
                0.0,
                1.0,
            ));
        }
        c
    }

    /// Put `value` for `key` on the device (`self.settings` already holds it).
    fn write(&mut self, key: SettingKey, value: &SettingValue) -> Result<()> {
        match key {
            SettingKey::ReportRate => {
                let hz = value.as_int().unwrap_or(0) as u32;
                self.backend
                    .set_report_rate(hz)
                    .map(|()| self.report_rate = Some(hz))
            }
            SettingKey::AngleSnapping if !self.caps.tuning => self.backend.store(&[(
                bragi::PROP_ANGLE_SNAP,
                u32::from(value.as_bool().unwrap_or(false)),
            )]),
            SettingKey::Effect if self.caps.onboard.lights == Offer::Yes => {
                let on = value.as_str() != Some("off");
                self.backend.store(&[(
                    bragi::PROP_BRIGHTNESS,
                    if on { bragi::BRIGHTNESS_MAX } else { 0 },
                )])
            }
            SettingKey::MicSideTone if self.caps.onboard.sidetone == Offer::Yes => {
                let pct = value.as_int().unwrap_or(0).clamp(0, 100) as u8;
                self.backend
                    .store(&headset::v2_sidetone(pct, self.caps.onboard.anc))
            }
            SettingKey::PmShutdown if self.caps.onboard.sleep == Offer::Yes => {
                let min = value.as_int().unwrap_or(0).max(0) as u32;
                self.backend.store(&headset::sleep(min))
            }
            SettingKey::Dpi if self.onboard() => Ok(()),
            SettingKey::Dpi => self.backend.dpi(value.as_int().unwrap_or(0) as u16),
            SettingKey::MicSideTone => self.backend.sidetone(value.as_int().unwrap_or(0) as u8),
            SettingKey::AngleSnapping | SettingKey::LiftOffDistance if self.onboard() => Ok(()),
            SettingKey::AngleSnapping => self.backend.angle_snap(value.as_bool().unwrap_or(false)),
            SettingKey::LiftOffDistance => self.backend.lift(value.as_int().unwrap_or(1) as u8),
            SettingKey::Effect if self.caps.lights => {
                self.backend.lights(value.as_str() != Some("off"))
            }
            // Leaving "onboard" takes host mode again, so DPI goes back too.
            SettingKey::Effect => self.apply(true),
            _ => self.apply(false),
        }
    }

    fn persist(&self) -> Result<()> {
        hako_core::config::save_device_settings(&self.paths, VID, self.settings_pid, &self.settings)
    }
}

impl DeviceAdapter for Peripheral {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.clone(),
            connected: self.connected,
            batteries: self.batteries.clone(),
            settings: self.settings.clone(),
            controls: self.controls(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let before = self.settings.clone();
        let old = get_setting(&before, key).cloned();
        merge_setting(&mut self.settings, key, value.clone());
        if let Err(mut e) = self.write(key, &value) {
            // The UI and the file keep the old value. Host mode with its
            // colour, both DPI axes, the onboard sidetone / sleep properties
            // are several writes that may stop midway: send the old value
            // back so the device matches ("onboard" hands back to hardware).
            self.settings = before;
            if let Some(old) = old {
                if let Err(re) = self.write(key, &old) {
                    e = anyhow!(
                        "{e:#}; restoring the previous value also failed ({re:#}), the device may hold part of the change"
                    );
                }
            }
            self.last_error = Some(format!("{e:#}"));
            return Err(e);
        }
        self.last_error = None;
        self.persist()
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }
        if self.caps.lighting && !self.onboard() && self.last_keepalive.elapsed() >= KEEPALIVE {
            self.last_keepalive = Instant::now();
            match self.backend.keepalive() {
                Ok(()) => self.failures = 0,
                Err(e) => self.failed(e),
            }
        }
        if self
            .last_battery
            .is_none_or(|t| t.elapsed() >= BATTERY_POLL)
        {
            self.poll_battery();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if self.connected && self.caps.lighting {
            if let Err(e) = self.backend.software(false) {
                warn!("{}: restore hardware mode: {e}", self.name);
            }
        }
        self.connected = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Log(Arc<Mutex<Vec<String>>>);

    impl Backend for Log {
        fn software(&mut self, on: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("sw {on}"));
            Ok(())
        }
        /// Refuses [9, 9, 9], like a device that stops answering.
        fn color(&mut self, rgb: [u8; 3]) -> Result<()> {
            if rgb == [9, 9, 9] {
                anyhow::bail!("no reply");
            }
            self.0.lock().unwrap().push(format!("color {rgb:?}"));
            Ok(())
        }
        fn dpi(&mut self, dpi: u16) -> Result<()> {
            self.0.lock().unwrap().push(format!("dpi {dpi}"));
            Ok(())
        }
        fn lift(&mut self, level: u8) -> Result<()> {
            self.0.lock().unwrap().push(format!("lift {level}"));
            Ok(())
        }
        fn angle_snap(&mut self, on: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("snap {on}"));
            Ok(())
        }
        fn lights(&mut self, on: bool) -> Result<()> {
            self.0.lock().unwrap().push(format!("lights {on}"));
            Ok(())
        }
        fn report_rate(&mut self) -> Option<u32> {
            Some(1000)
        }
        /// Refuses 250 Hz, like a device whose read-back differs.
        fn store(&mut self, writes: &[(u8, u32)]) -> Result<()> {
            for &(prop, value) in writes {
                if (prop, value) == (bragi::PROP_POLLRATE, 2) {
                    anyhow::bail!("reads back 4");
                }
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("store {prop:#04x} {value}"));
            }
            Ok(())
        }
        /// Angle snapping on, device max 1000 Hz, lights off, sidetone on
        /// at 40 %, sleep timer on at 30 minutes.
        fn stored(&mut self, prop: u8) -> Option<u32> {
            match prop {
                bragi::PROP_ANGLE_SNAP => Some(1),
                bragi::PROP_MAX_POLLRATE => Some(4),
                bragi::PROP_BRIGHTNESS => Some(0),
                bragi::PROP_SIDETONE_OFF => Some(0),
                bragi::PROP_SIDETONE_LEVEL => Some(400),
                bragi::PROP_SLEEP_ENABLED => Some(1),
                bragi::PROP_SLEEP_TIMEOUT => Some(1_800_000),
                _ => None,
            }
        }
    }

    fn control(p: &Peripheral, key: SettingKey) -> SettingMeta {
        p.controls()
            .into_iter()
            .find(|c| c.key == key)
            .unwrap_or_else(|| panic!("no {key:?} control"))
    }

    fn mouse(log: Arc<Mutex<Vec<String>>>) -> Peripheral {
        let caps = Caps {
            lighting: true,
            dpi: Some((100, 18000)),
            ..Caps::default()
        };
        peripheral(log, caps, "mouse")
    }

    fn peripheral(log: Arc<Mutex<Vec<String>>>, caps: Caps, tag: &str) -> Peripheral {
        let dir =
            std::env::temp_dir().join(format!("hako-corsair-test-{tag}-{}", std::process::id()));
        let paths = ConfigPaths {
            root: dir.clone(),
            config_toml: dir.join("config.toml"),
            devices_dir: dir.join("devices"),
            profiles_dir: dir.join("profiles"),
            remaps_dir: dir.join("remaps"),
        };
        Peripheral::new(
            "test".into(),
            DeviceKind::Mouse,
            caps,
            DeviceId {
                vendor_id: VID,
                product_id: 0xffff,
                instance: "t".into(),
            },
            0xffff,
            Box::new(Log(log)),
            paths,
        )
        .unwrap()
    }

    #[test]
    fn brightness_scales_on_host() {
        let mut s = SettingsMap::new();
        defaults(&mut s);
        assert_eq!(frame_color(&s), [0, 120, 255]);
        merge_setting(&mut s, SettingKey::Brightness, SettingValue::Int(50));
        assert_eq!(frame_color(&s), [0, 60, 127]);
        merge_setting(&mut s, SettingKey::Effect, SettingValue::Text("off".into()));
        assert_eq!(frame_color(&s), [0, 0, 0]);
    }

    #[test]
    fn onboard_hands_back_and_shutdown_restores() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut p = mouse(log.clone());
        assert_eq!(*log.lock().unwrap(), ["sw true", "color [0, 120, 255]"]);
        log.lock().unwrap().clear();

        p.set(SettingKey::Effect, SettingValue::Text("onboard".into()))
            .unwrap();
        // DPI is only remembered while the firmware owns the device.
        p.set(SettingKey::Dpi, SettingValue::Int(800)).unwrap();
        assert!(p
            .controls()
            .iter()
            .any(|c| c.key == SettingKey::Dpi && c.hidden));
        p.set(SettingKey::Effect, SettingValue::Text("static".into()))
            .unwrap();
        p.shutdown();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "sw false",
                "sw true",
                "color [0, 120, 255]",
                "dpi 800",
                "sw false"
            ]
        );
        assert!(!p.snapshot().connected);
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    /// Leaving "onboard" switches to host mode before the colour goes out:
    /// if the colour fails, the device is handed back to its firmware, as
    /// the rolled-back setting says (no keepalive runs for "onboard").
    #[test]
    fn failed_switch_to_host_mode_hands_back() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut p = peripheral(
            log.clone(),
            Caps {
                lighting: true,
                ..Caps::default()
            },
            "handback",
        );
        p.set(SettingKey::Effect, SettingValue::Text("onboard".into()))
            .unwrap();
        for key in [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB] {
            p.set(key, SettingValue::Int(9)).unwrap();
        }
        log.lock().unwrap().clear();
        let err = p
            .set(SettingKey::Effect, SettingValue::Text("static".into()))
            .unwrap_err();
        assert!(err.to_string().contains("no reply"), "{err:#}");
        assert_eq!(*log.lock().unwrap(), ["sw true", "sw false"]);
        assert!(p.onboard());
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    /// Saved values out of range are fixed before connect sends them.
    #[test]
    fn saved_values_are_sanitized_before_connect() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let dir =
            std::env::temp_dir().join(format!("hako-corsair-test-sane-{}", std::process::id()));
        let paths = ConfigPaths {
            root: dir.clone(),
            config_toml: dir.join("config.toml"),
            devices_dir: dir.join("devices"),
            profiles_dir: dir.join("profiles"),
            remaps_dir: dir.join("remaps"),
        };
        paths.ensure().unwrap();
        let mut s = SettingsMap::new();
        s.insert("dpi".into(), SettingValue::Int(70000));
        hako_core::config::save_device_settings(&paths, VID, 0xffff, &s).unwrap();
        let p = peripheral(
            log.clone(),
            Caps::of(models::spec(0x1b5a).unwrap().family),
            "sane",
        );
        let max = p.caps.dpi.unwrap().1;
        assert!(log.lock().unwrap().contains(&format!("dpi {max}")));
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    #[test]
    fn nxp_tuning_is_live_and_reapplied() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let caps = Caps::of(models::spec(0x1b5a).unwrap().family);
        assert!(caps.tuning && caps.dpi.is_some());
        let mut p = peripheral(log.clone(), caps, "tuning");
        let keys: Vec<_> = p.controls().iter().map(|c| c.key).collect();
        assert!(keys.contains(&SettingKey::AngleSnapping));
        assert!(keys.contains(&SettingKey::LiftOffDistance));
        log.lock().unwrap().clear();

        p.set(SettingKey::AngleSnapping, SettingValue::Bool(true))
            .unwrap();
        p.set(SettingKey::LiftOffDistance, SettingValue::Int(4))
            .unwrap();
        // Back from "onboard": host mode, colour, then the saved tuning.
        p.set(SettingKey::Effect, SettingValue::Text("onboard".into()))
            .unwrap();
        p.set(SettingKey::LiftOffDistance, SettingValue::Int(2))
            .unwrap();
        p.set(SettingKey::Effect, SettingValue::Text("static".into()))
            .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "snap true",
                "lift 4",
                "sw false",
                "sw true",
                "color [0, 120, 255]",
                "snap true",
                "lift 2"
            ]
        );
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    #[test]
    fn void_lights_follow_the_effect_dropdown() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let caps = Caps::of(models::spec(0x0a17).unwrap().family);
        assert!(caps.lights && !caps.sidetone && !caps.lighting);
        let mut p = peripheral(log.clone(), caps, "lights");
        // Nothing is sent before the user picks a state.
        assert!(log.lock().unwrap().is_empty());
        let lights = p
            .controls()
            .into_iter()
            .find(|c| c.key == SettingKey::Effect)
            .unwrap();
        assert!(lights.validate(&SettingValue::Text("off".into())).is_ok());
        assert!(lights
            .validate(&SettingValue::Text("static".into()))
            .is_err());
        p.set(SettingKey::Effect, SettingValue::Text("off".into()))
            .unwrap();
        p.set(SettingKey::Effect, SettingValue::Text("on".into()))
            .unwrap();
        // Reconnect re-applies the saved state.
        let p2 = peripheral(log.clone(), caps, "lights");
        assert_eq!(
            *log.lock().unwrap(),
            ["lights false", "lights true", "lights true"]
        );
        let _ = std::fs::remove_dir_all(&p2.paths.root);
    }

    #[test]
    fn receiver_rescan_skips_open_slots() {
        let open = [DeviceId {
            vendor_id: VID,
            product_id: 0x1b80,
            instance: slot_instance("SERIAL", 0x09),
        }];
        assert_eq!(open[0].instance, "SERIAL#09");
        assert!(slot_open(&open, "SERIAL", 0x09));
        assert!(!slot_open(&open, "SERIAL", 0x0a));
        assert!(!slot_open(&open, "OTHER", 0x09));
        assert!(!slot_open(&[], "SERIAL", 0x09));
    }

    #[test]
    fn bragi_onboard_settings_are_read_never_reapplied() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // M65 RGB ULTRA on its cable: up to 8 kHz, but the device says 1 kHz.
        let caps = Caps::opened(models::spec(0x1b9e).unwrap().family, 0x1b9e, false);
        let mut p = peripheral(log.clone(), caps, "bragi-onboard");
        // Connect wrote host lighting only; the stored values are displayed.
        assert_eq!(*log.lock().unwrap(), ["sw true", "color [0, 120, 255]"]);
        assert_eq!(
            p.settings.get("angle_snapping"),
            Some(&SettingValue::Bool(true))
        );
        let rate = control(&p, SettingKey::ReportRate);
        assert!(rate.persistent && rate.blocked.is_none() && !rate.read_only);
        let hz: Vec<_> = rate.options.iter().map(|o| o.value.clone()).collect();
        assert_eq!(hz, [125, 250, 500, 1000].map(SettingValue::Int));
        let snap = control(&p, SettingKey::AngleSnapping);
        assert!(snap.persistent && snap.blocked.is_none());
        assert!(control(&p, SettingKey::DpiStage1).blocked.is_some());
        log.lock().unwrap().clear();

        // Writes go through the onboard path, also while the firmware owns
        // the lighting.
        p.set(SettingKey::Effect, SettingValue::Text("onboard".into()))
            .unwrap();
        p.set(SettingKey::ReportRate, SettingValue::Int(500))
            .unwrap();
        p.set(SettingKey::AngleSnapping, SettingValue::Bool(false))
            .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            ["sw false", "store 0x01 3", "store 0x07 0"]
        );
        assert_eq!(p.report_rate, Some(500));
        // A failed store keeps the old value.
        assert!(p
            .set(SettingKey::ReportRate, SettingValue::Int(250))
            .is_err());
        assert_eq!(p.settings.get("report_rate"), Some(&SettingValue::Int(500)));

        // Reconnect: nothing is written back, the device's values win.
        log.lock().unwrap().clear();
        let p2 = peripheral(log.clone(), caps, "bragi-onboard");
        assert!(!log.lock().unwrap().iter().any(|l| l.starts_with("store")));
        assert_eq!(
            p2.settings.get("report_rate"),
            Some(&SettingValue::Int(1000))
        );
        let _ = std::fs::remove_dir_all(&p2.paths.root);
    }

    #[test]
    fn blocked_onboard_settings_stay_visible() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // Dark Core RGB PRO behind its receiver: rate shown, write blocked.
        let caps = Caps::opened(models::spec(0x1b80).unwrap().family, 0x1b80, true);
        let p = peripheral(log.clone(), caps, "bragi-blocked");
        let rate = control(&p, SettingKey::ReportRate);
        assert!(rate.read_only && rate.persistent && rate.blocked.is_some());
        assert!(control(&p, SettingKey::PmShutdown).blocked.is_some());
        let _ = std::fs::remove_dir_all(&p.paths.root);
        // NXP: the profile save exists but is blocked.
        let caps = Caps::opened(models::spec(0x1b49).unwrap().family, 0x1b49, false);
        let p = peripheral(log.clone(), caps, "nxp-blocked");
        let save = control(&p, SettingKey::SaveToDevice);
        assert!(save.persistent && save.blocked.is_some());
        assert!(p
            .controls()
            .iter()
            .all(|c| !c.persistent || c.blocked.is_some()));
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    #[test]
    fn bragi_headset_lights_are_stored() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let caps = Caps::opened(models::spec(0x0a62).unwrap().family, 0x0a62, false);
        let mut p = peripheral(log.clone(), caps, "xt-lights");
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(
            p.settings.get("effect"),
            Some(&SettingValue::Text("off".into()))
        );
        assert!(control(&p, SettingKey::Effect).persistent);
        assert!(control(&p, SettingKey::MicSideTone).blocked.is_some());
        p.set(SettingKey::Effect, SettingValue::Text("on".into()))
            .unwrap();
        assert_eq!(*log.lock().unwrap(), ["store 0x02 1000"]);
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    #[test]
    fn v2_headset_sidetone_and_sleep_are_stored() {
        let log = Arc::new(Mutex::new(Vec::new()));
        // Void Wireless V2 receiver.
        let caps = Caps::opened(models::spec(0x2a08).unwrap().family, 0x2a08, false);
        let mut p = peripheral(log.clone(), caps, "v2");
        // Connect only reads: the device's values are shown.
        assert!(log.lock().unwrap().is_empty());
        assert_eq!(
            p.settings.get("mic_side_tone"),
            Some(&SettingValue::Int(40))
        );
        assert_eq!(p.settings.get("pm_shutdown"), Some(&SettingValue::Int(30)));
        for key in [SettingKey::MicSideTone, SettingKey::PmShutdown] {
            let c = control(&p, key);
            assert!(c.persistent && c.blocked.is_none(), "{key:?}");
        }
        assert!(control(&p, SettingKey::PmShutdown)
            .validate(&SettingValue::Int(90))
            .is_ok());
        p.set(SettingKey::MicSideTone, SettingValue::Int(25))
            .unwrap();
        p.set(SettingKey::MicSideTone, SettingValue::Int(0))
            .unwrap();
        p.set(SettingKey::PmShutdown, SettingValue::Int(10))
            .unwrap();
        p.set(SettingKey::PmShutdown, SettingValue::Int(0)).unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [
                "store 0xd1 0",
                "store 0x46 0",
                "store 0x47 250",
                "store 0x46 1",
                "store 0x0d 1",
                "store 0x0e 600000",
                "store 0x0d 0"
            ]
        );
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    #[test]
    fn nxp_mouse_rate_is_offered() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let caps = Caps::opened(models::spec(0x1b5a).unwrap().family, 0x1b5a, false);
        let p = peripheral(log.clone(), caps, "nxp-rate");
        let rate = control(&p, SettingKey::ReportRate);
        assert!(rate.persistent && rate.blocked.is_none() && !rate.read_only);
        let hz: Vec<_> = rate.options.iter().map(|o| o.value.clone()).collect();
        assert_eq!(hz, [125, 250, 500, 1000].map(SettingValue::Int));
        let _ = std::fs::remove_dir_all(&p.paths.root);
    }

    #[test]
    fn caps_follow_family() {
        for s in models::SPECS {
            let c = Caps::of(s.family);
            match s.kind {
                DeviceKind::Headset => assert!(!c.lighting && c.dpi.is_none(), "{}", s.name),
                DeviceKind::Mouse if !matches!(s.family, Family::Receiver) => {
                    assert!(c.lighting, "{}", s.name)
                }
                _ => {}
            }
        }
    }
}
