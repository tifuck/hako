//! ROCCAT (VID 0x1e7d; Turtle Beach era keyboards 0x10f5): host-driven
//! lighting for mice, keyboards and headsets, and read-only onboard-profile
//! state for older Kone mice.
//!
//! Reimplemented from public notes; only IDs and byte layouts were taken from:
//! - OpenRGB `Controllers/RoccatController/` (GPL, layouts only):
//!   `RoccatControllerDetect.cpp`, `RoccatVulcanKeyboardController/*`,
//!   `RoccatKoneAimoController/*`, `RoccatKoneXPController/*`,
//!   `RoccatKoneProController/*`, `RoccatKoneProAirController/*`,
//!   `RoccatBurstController/*`, `RoccatHordeAimoController/*`,
//!   `RoccatEloController/*`: <https://github.com/CalcProgrammer1/OpenRGB>
//! - HeadsetControl `lib/devices/roccat_elo_7_1_air.hpp`,
//!   `roccat_elo_7_1_usb.hpp` (GPL, layouts only):
//!   <https://github.com/Sapd/HeadsetControl>
//! - libratbag `src/driver-roccat.c`, `driver-roccat-kone-pure.c`,
//!   `driver-roccat-kone-emp.c`, `data/devices/roccat-*.device` (MIT):
//!   <https://github.com/libratbag/libratbag>
//!
//! Every ROCCAT mode/effect write in those sources is a profile write (report
//! 0x06 on mice, 0x0D/0x11 on keyboards, flagged "automatic save" by OpenRGB),
//! so none is sent here. Only the volatile "direct"/host-control paths are
//! used; brightness is applied host-side by scaling the color. The user's
//! choice is re-applied on reconnect.
//!
//! Onboard-memory writes: only the Kone XTD's profile settings (DPI stages,
//! active stage, report rate; see `profile.rs`). The profile writes of the
//! newer mice and keyboards come from OpenRGB alone and are shown as blocked
//! placeholders. Research: `docs/onboard-writes/roccat.md`.

mod headset;
mod keyboard;
mod mouse;
mod profile;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{enumerate_hidraw, HidDeviceInfo, HidrawDevice};
use anyhow::{bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const ROCCAT: u16 = 0x1e7d;
const TURTLE_BEACH: u16 = 0x10f5;

/// One model: the data a family's open function needs besides the protocol.
struct Spec<F: 'static> {
    name: &'static str,
    vid: u16,
    /// Every PID the device enumerates as; the first names its settings file.
    pids: &'static [u16],
    family: F,
}

macro_rules! model {
    ($kind:ident, $module:ident :: $spec:ident) => {
        Model {
            vendor_id: $module::$spec.vid,
            product_ids: $module::$spec.pids,
            kind: DeviceKind::$kind,
            name: $module::$spec.name,
            usbfs: false,
            multi: false,
            open: |pid, ctx| $module::open(&$module::$spec, pid, ctx),
        }
    };
}

pub const MODELS: &[Model] = &[
    model!(Mouse, mouse::KONE_AIMO),
    model!(Mouse, mouse::KONE_AIMO_16K),
    model!(Mouse, mouse::KONE_XP),
    model!(Mouse, mouse::KONE_PRO),
    model!(Mouse, mouse::KONE_PRO_AIR),
    model!(Mouse, mouse::BURST_PRO),
    model!(Mouse, mouse::BURST_CORE),
    model!(Mouse, profile::KONE_PURE),
    model!(Mouse, profile::KONE_XTD),
    model!(Mouse, profile::KONE_EMP),
    model!(Keyboard, keyboard::VULCAN_120_AIMO),
    model!(Keyboard, keyboard::VULCAN_100_AIMO),
    model!(Keyboard, keyboard::VULCAN_TKL),
    model!(Keyboard, keyboard::VULCAN_PRO),
    model!(Keyboard, keyboard::VULCAN_TKL_PRO),
    model!(Keyboard, keyboard::VULCAN_II),
    model!(Keyboard, keyboard::VULCAN_II_MAX),
    model!(Keyboard, keyboard::VULCAN_II_MINI),
    model!(Keyboard, keyboard::TB_VULCAN_II),
    model!(Keyboard, keyboard::TB_VULCAN_II_TKL),
    model!(Keyboard, keyboard::TB_VULCAN_II_TKL_PRO),
    model!(Keyboard, keyboard::PYRO),
    model!(Keyboard, keyboard::MAGMA),
    model!(Keyboard, keyboard::MAGMA_MINI),
    model!(Keyboard, keyboard::HORDE_AIMO),
    model!(Headset, headset::ELO_AIR),
    model!(Headset, headset::ELO_USB),
];

/// The device's nodes, lowest interface first.
fn nodes(vid: u16, pid: u16) -> Result<Vec<HidDeviceInfo>> {
    let mut nodes: Vec<_> = enumerate_hidraw()?
        .into_iter()
        .filter(|d| d.vendor_id == vid && d.product_id == pid)
        .collect();
    nodes.sort_by_key(|d| d.interface.unwrap_or(u32::MAX));
    Ok(nodes)
}

/// Open `info`; returns the device and a stable instance key.
fn open_info(info: &HidDeviceInfo) -> Result<(HidrawDevice, String)> {
    let dev = HidrawDevice::open(&info.path)?;
    let instance = if info.uniq.is_empty() {
        info.path.display().to_string()
    } else {
        info.uniq.clone()
    };
    Ok((dev, instance))
}

/// Open the node on USB interface `iface`.
fn open_iface(vid: u16, pid: u16, iface: u32) -> Result<(HidrawDevice, String)> {
    let info = nodes(vid, pid)?
        .into_iter()
        .find(|d| d.interface == Some(iface))
        .with_context(|| format!("no hidraw node for {vid:04x}:{pid:04x} interface {iface}"))?;
    open_info(&info)
}

fn int(s: &SettingsMap, key: SettingKey, default: i64) -> i64 {
    get_setting(s, key)
        .and_then(|v| v.as_int())
        .unwrap_or(default)
}

fn text<'a>(s: &'a SettingsMap, key: SettingKey, default: &'a str) -> &'a str {
    get_setting(s, key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
}

/// Saved color scaled by the saved brightness (ROCCAT red by default).
fn color(s: &SettingsMap) -> [u8; 3] {
    let pct = int(s, SettingKey::Brightness, 100).clamp(0, 100);
    [
        int(s, SettingKey::ColorR, 255),
        int(s, SettingKey::ColorG, 0),
        int(s, SettingKey::ColorB, 0),
    ]
    .map(|c| (c.clamp(0, 255) * pct / 100) as u8)
}

/// Feature-report checksum used by ROCCAT: little-endian u16 sum of every
/// byte but the last two, stored in the last two.
fn checksum(buf: &mut [u8]) {
    let n = buf.len() - 2;
    let sum = buf[..n]
        .iter()
        .fold(0u16, |a, &b| a.wrapping_add(u16::from(b)));
    buf[n..].copy_from_slice(&sum.to_le_bytes());
}

/// Poll report `id` (`len` bytes) until byte 1 reads 1, as Swarm does after
/// a control write. Bounded; a device that never reports ready is not an error.
fn wait_ready(dev: &HidrawDevice, id: u8, len: usize, pause: Duration, tries: u32) {
    let mut buf = vec![0u8; len];
    for i in 0..tries {
        if i > 0 {
            std::thread::sleep(pause);
        }
        buf.fill(0);
        buf[0] = id;
        if dev.get_feature(&mut buf).is_ok() && buf[1] == 1 {
            return;
        }
    }
}

/// A device's lighting protocol, driven by the shared [`Adapter`].
trait Lights: Send {
    /// Effect choices; the first is the default ("onboard" when the device
    /// keeps its own lighting until the host takes over).
    fn effects(&self) -> &'static [(&'static str, &'static str)];
    /// Show `effect` (one of [`Lights::effects`]) in `color`.
    fn apply(&mut self, effect: &str, color: [u8; 3]) -> Result<()>;
    /// Controls besides lighting.
    fn extra_controls(&self) -> Vec<SettingMeta> {
        Vec::new()
    }
    /// Apply one of [`Lights::extra_controls`].
    fn set_extra(&mut self, key: SettingKey, _value: &SettingValue) -> Result<()> {
        bail!("{} is not supported", key.as_str())
    }
    /// Hand lighting back to the firmware when the protocol allows it.
    fn release(&mut self) {}
}

/// Placeholder for an onboard lighting save the sources do not confirm.
fn save_blocked(reason: &str) -> SettingMeta {
    SettingMeta::toggle(SettingKey::SaveToDevice, "Save lighting to device memory")
        .persistent()
        .blocked(reason)
}

/// OpenRGB is the only source for these profile writes; Eruption replays
/// captures but documents no layout, libratbag and the kernel lack them.
const OPENRGB_ONLY: &str =
    "single source (OpenRGB profile write); no second source confirms the layout";

const ONBOARD_STATIC: &[(&str, &str)] = &[("onboard", "Onboard profile"), ("static", "Static")];
/// For devices with no documented way back to onboard lighting.
const STICKY_STATIC: &[(&str, &str)] = &[
    ("onboard", "Onboard profile (replug to restore)"),
    ("static", "Static"),
];

struct Adapter {
    id: DeviceId,
    kind: DeviceKind,
    name: &'static str,
    /// VID and canonical PID: name the settings file.
    file: (u16, u16),
    lights: Option<Box<dyn Lights>>,
    settings: SettingsMap,
    paths: ConfigPaths,
    last_error: Option<String>,
}

impl Adapter {
    /// Wrap `lights` and re-apply the saved settings.
    fn open<F>(
        spec: &Spec<F>,
        kind: DeviceKind,
        pid: u16,
        instance: String,
        lights: Box<dyn Lights>,
        ctx: &DriverCtx,
    ) -> Opened {
        let settings = hako_core::config::load_device_settings(&ctx.paths, spec.vid, spec.pids[0])?;
        let mut a = Adapter {
            id: DeviceId {
                vendor_id: spec.vid,
                product_id: pid,
                instance,
            },
            kind,
            name: spec.name,
            file: (spec.vid, spec.pids[0]),
            lights: Some(lights),
            settings,
            paths: ctx.paths.clone(),
            last_error: None,
        };
        info!("{} open", spec.name);
        let mut result = Ok(());
        if get_setting(&a.settings, SettingKey::Effect).is_some() {
            result = a.apply_lighting();
        }
        let extra: Vec<_> = a
            .lights
            .as_ref()
            .map(|l| l.extra_controls())
            .unwrap_or_default();
        for meta in extra {
            if let (Some(v), Some(l)) = (get_setting(&a.settings, meta.key).cloned(), &mut a.lights)
            {
                result = result.and(l.set_extra(meta.key, &v));
            }
        }
        a.last_error = result.err().map(|e| e.to_string());
        Ok(vec![Box::new(a)])
    }

    fn effect(&self) -> &str {
        let default = self.lights.as_ref().map_or("onboard", |l| l.effects()[0].0);
        text(&self.settings, SettingKey::Effect, default)
    }

    fn apply_lighting(&mut self) -> Result<()> {
        let (effect, c) = (self.effect().to_string(), color(&self.settings));
        self.lights
            .as_mut()
            .context("not connected")?
            .apply(&effect, c)
    }
}

/// Effect -> Brightness -> Color; color controls only for `static`.
fn lighting_controls(effects: &[(&str, &str)], effect: &str) -> Vec<SettingMeta> {
    use SettingKey::*;
    let on = effect == "static";
    let mut c = vec![
        SettingMeta::dropdown(
            Effect,
            "Effect",
            effects
                .iter()
                .map(|(v, l)| SettingOption::text(v, l))
                .collect(),
        ),
        SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 1.0)
            .unit("%")
            .hidden(!on),
    ];
    for (key, label) in [(ColorR, "Red"), (ColorG, "Green"), (ColorB, "Blue")] {
        c.push(SettingMeta::color(key, label).hidden(!on));
    }
    c
}

impl DeviceAdapter for Adapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        let mut controls = Vec::new();
        if let Some(l) = &self.lights {
            controls = lighting_controls(l.effects(), self.effect());
            controls.extend(l.extra_controls());
        }
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.into(),
            connected: self.lights.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls,
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        use SettingKey::*;
        let result = match key {
            Effect | Brightness | ColorR | ColorG | ColorB => {
                merge_setting(&mut self.settings, key, value);
                self.apply_lighting()
            }
            _ => {
                let r = self
                    .lights
                    .as_mut()
                    .context("not connected")?
                    .set_extra(key, &value);
                if r.is_ok() {
                    merge_setting(&mut self.settings, key, value);
                }
                r
            }
        };
        self.last_error = result.as_ref().err().map(|e| e.to_string());
        hako_core::config::save_device_settings(
            &self.paths,
            self.file.0,
            self.file.1,
            &self.settings,
        )?;
        result
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(mut l) = self.lights.take() {
            l.release();
        }
    }
}

/// Reads input reports until `stop` matches or `timeout` passes.
fn read_until(dev: &mut HidrawDevice, timeout: Duration, stop: impl Fn(&[u8]) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 64];
    for _ in 0..32 {
        let left = deadline.saturating_duration_since(Instant::now());
        match dev.read_timeout(&mut buf, left) {
            Ok(n) if stop(&buf[..n]) => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_is_registered_once() {
        let specs = [
            mouse::ALL
                .iter()
                .map(|s| (s.vid, s.name))
                .collect::<Vec<_>>(),
            profile::ALL.iter().map(|s| (s.vid, s.name)).collect(),
            keyboard::ALL.iter().map(|s| (s.vid, s.name)).collect(),
            headset::ALL.iter().map(|s| (s.vid, s.name)).collect(),
        ]
        .concat();
        assert_eq!(MODELS.len(), specs.len());
        let names: Vec<_> = MODELS.iter().map(|m| (m.vendor_id, m.name)).collect();
        for s in &specs {
            assert!(names.contains(s), "{} not in MODELS", s.1);
        }
        let mut seen = std::collections::HashSet::new();
        for m in MODELS {
            assert!(matches!(m.vendor_id, ROCCAT | TURTLE_BEACH), "{}", m.name);
            for &p in m.product_ids {
                assert!(seen.insert((m.vendor_id, p)), "{:04x} twice", p);
            }
        }
    }

    #[test]
    fn color_scales_and_checksum() {
        let mut s = SettingsMap::new();
        assert_eq!(color(&s), [255, 0, 0]);
        merge_setting(&mut s, SettingKey::Brightness, SettingValue::Int(50));
        merge_setting(&mut s, SettingKey::ColorG, SettingValue::Int(100));
        assert_eq!(color(&s), [127, 50, 0]);
        let mut b = [0x18, 0x18, 0xff, 0xff, 0, 0];
        checksum(&mut b);
        assert_eq!(b[4..], [0x2e, 0x02]);
    }

    #[test]
    fn lighting_controls_validate_defaults() {
        for effects in [ONBOARD_STATIC, STICKY_STATIC, headset::ELO_EFFECTS] {
            for (e, _) in effects {
                let c = lighting_controls(effects, e);
                let pick = c.iter().find(|m| m.key == SettingKey::Effect).unwrap();
                assert!(pick.validate(&SettingValue::Text((*e).into())).is_ok());
                for m in &c {
                    let v = match m.key {
                        SettingKey::Effect => continue,
                        SettingKey::Brightness => 100,
                        _ => 0,
                    };
                    assert!(m.validate(&SettingValue::Int(v)).is_ok());
                    assert_eq!(m.hidden, *e != "static");
                }
            }
        }
    }
}
