//! Host-driven lighting for HyperX devices whose only documented lighting
//! path is OpenRGB's "direct" mode: the host sends colour frames and the
//! firmware falls back to its onboard profile once they stop (OpenRGB keeps
//! a keepalive thread re-sending them every 50 ms, 1 s for some models).
//!
//! One adapter serves every such keyboard and mouse. `onboard` sends nothing
//! (the device plays its own profile); `static`, `breathing` and `spectrum`
//! are rendered here and streamed, every ~50 ms while animated and at the
//! model's keepalive while static. Brightness scales the colour host-side.
//! Nothing is ever written to the device's memory.

use super::{int, lighting_controls, open_node, rgb, text, Node, Spec};
use crate::drivers::{DeviceAdapter, DriverCtx, Opened};
use crate::hid::HidrawDevice;
use anyhow::{Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue, SettingsMap};
use hako_core::config::{merge_setting, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

/// A model's frame layout.
pub(super) trait Frames: Copy + Send + Sync + 'static {
    /// Write one frame showing `color` on every LED.
    fn send(self, dev: &mut HidrawDevice, color: [u8; 3]) -> Result<()>;
    /// How often a static frame must be repeated.
    fn keepalive(self) -> Duration;
    /// Why the model's documented onboard-profile writes are not offered,
    /// shown as a blocked placeholder (`None`: no such writes are known).
    fn memory_blocked(self) -> Option<&'static str> {
        None
    }
}

/// Frame interval of animated effects (OpenRGB's keepalive period).
const ANIMATED: Duration = Duration::from_millis(50);
const IDLE: Duration = Duration::from_secs(1);

const EFFECTS: [(&str, &str); 4] = [
    ("onboard", "Onboard profile"),
    ("static", "Static"),
    ("breathing", "Breathing"),
    ("spectrum", "Spectrum"),
];

fn controls(effect: &str) -> Vec<SettingMeta> {
    let mut c = lighting_controls(
        &EFFECTS,
        effect,
        &["breathing", "spectrum"],
        &["static", "breathing"],
    );
    for m in &mut c {
        if m.key == SettingKey::Brightness {
            m.hidden = effect == "onboard";
        }
    }
    c
}

/// Brightness is applied host-side by scaling the color.
fn scale(color: [u8; 3], pct: i64) -> [u8; 3] {
    color.map(|c| (i64::from(c) * pct.clamp(0, 100) / 100) as u8)
}

/// Effect period per speed choice.
fn period(effect: &str, speed: &str) -> Duration {
    let ms = match (effect, speed) {
        ("spectrum", "slow") => 12_000,
        ("spectrum", "fast") => 2_000,
        ("spectrum", _) => 6_000,
        (_, "slow") => 4_800,
        (_, "fast") => 800,
        _ => 2_800,
    };
    Duration::from_millis(ms)
}

/// Fully saturated hue (0.0..1.0) as RGB.
fn hue(h: f64) -> [u8; 3] {
    let x = (h.rem_euclid(1.0)) * 6.0;
    let f = |n: f64| {
        let k = (n + x) % 6.0;
        (255.0 * (1.0 - (k.min(4.0 - k).clamp(0.0, 1.0)))).round() as u8
    };
    [f(5.0), f(3.0), f(1.0)]
}

/// The colour to show `elapsed` into the effect; `None` = leave the device alone.
fn frame(
    effect: &str,
    color: [u8; 3],
    pct: i64,
    speed: &str,
    elapsed: Duration,
) -> Option<[u8; 3]> {
    let t = elapsed.as_secs_f64() / period(effect, speed).as_secs_f64();
    let c = match effect {
        "onboard" => return None,
        // NGENUITY's breathing curve: colour × sin(π·t).
        "breathing" => {
            let k = (std::f64::consts::PI * t.fract()).sin();
            color.map(|v| (f64::from(v) * k) as u8)
        }
        "spectrum" => hue(t),
        _ => color,
    };
    Some(scale(c, pct))
}

pub(super) fn open<F: Frames>(
    spec: &'static Spec<F>,
    kind: DeviceKind,
    node: Node,
    pid: u16,
    ctx: &DriverCtx,
) -> Opened {
    let (dev, instance) = open_node(spec.vid, pid, node)?;
    info!("{} open via {}", spec.name, dev.path.display());
    Ok(vec![Box::new(Direct {
        spec,
        kind,
        id: DeviceId {
            vendor_id: spec.vid,
            product_id: pid,
            instance,
        },
        dev: Some(dev),
        settings: hako_core::config::load_device_settings(&ctx.paths, spec.vid, spec.pids[0])?,
        started: Instant::now(),
        failures: 0,
        last_error: None,
        paths: ctx.paths.clone(),
    })])
}

struct Direct<F: 'static> {
    spec: &'static Spec<F>,
    kind: DeviceKind,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    started: Instant,
    failures: u8,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl<F: Frames> Direct<F> {
    fn effect(&self) -> &str {
        text(&self.settings, SettingKey::Effect, "onboard")
    }

    fn send_frame(&mut self) -> Result<()> {
        let s = &self.settings;
        let Some(color) = frame(
            self.effect(),
            rgb(s),
            int(s, SettingKey::Brightness, 100),
            text(s, SettingKey::Speed, "medium"),
            self.started.elapsed(),
        ) else {
            return Ok(());
        };
        let dev = self.dev.as_mut().context("not connected")?;
        self.spec.family.send(dev, color)
    }
}

impl<F: Frames> DeviceAdapter for Direct<F> {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.spec.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: controls(self.effect())
                .into_iter()
                .chain(self.spec.family.memory_blocked().map(|reason| {
                    SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory")
                        .persistent()
                        .blocked(reason)
                }))
                .collect(),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        merge_setting(&mut self.settings, key, value);
        self.started = Instant::now();
        let result = self.send_frame();
        self.last_error = result.as_ref().err().map(|e| e.to_string());
        hako_core::config::save_device_settings(
            &self.paths,
            self.spec.vid,
            self.spec.pids[0],
            &self.settings,
        )?;
        result
    }

    /// Keepalive / next animation frame.
    fn tick(&mut self) -> Result<()> {
        if self.dev.is_none() {
            return Ok(());
        }
        let result = self.send_frame();
        self.last_error = result.as_ref().err().map(|e| e.to_string());
        if result.is_err() {
            self.failures = self.failures.saturating_add(1);
            // Stop streaming into a node that keeps failing (unplugged).
            if self.failures >= 3 {
                self.dev = None;
            }
        } else {
            self.failures = 0;
        }
        result
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }

    fn tick_interval(&self) -> Duration {
        match self.effect() {
            "onboard" => IDLE,
            "static" => self.spec.family.keepalive(),
            _ => ANIMATED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_follow_effect() {
        let ms = Duration::from_millis;
        assert_eq!(frame("onboard", [9; 3], 100, "medium", ms(0)), None);
        assert_eq!(
            frame("static", [200, 100, 0], 50, "medium", ms(0)),
            Some([100, 50, 0])
        );
        // Breathing: dark at the start of a period, full colour halfway.
        assert_eq!(
            frame("breathing", [200, 0, 0], 100, "medium", ms(0)),
            Some([0, 0, 0])
        );
        assert_eq!(
            frame("breathing", [200, 0, 0], 100, "medium", ms(1400)),
            Some([200, 0, 0])
        );
        assert_eq!(hue(0.0), [255, 0, 0]);
        assert_eq!(hue(1.0 / 3.0), [0, 255, 0]);
        assert_eq!(hue(2.0 / 3.0), [0, 0, 255]);
        assert_eq!(
            frame("spectrum", [0; 3], 100, "fast", ms(0)),
            Some([255, 0, 0])
        );
    }

    #[test]
    fn controls_hide_what_does_not_apply() {
        let onboard = controls("onboard");
        assert!(onboard
            .iter()
            .filter(|c| c.key != SettingKey::Effect)
            .all(|c| c.hidden));
        let spectrum = controls("spectrum");
        let shown = |key| !spectrum.iter().find(|c| c.key == key).unwrap().hidden;
        assert!(shown(SettingKey::Speed) && shown(SettingKey::Brightness));
        assert!(!shown(SettingKey::ColorR));
        for (e, _) in EFFECTS {
            let c = controls(e);
            let meta = c.iter().find(|c| c.key == SettingKey::Effect).unwrap();
            assert!(meta.validate(&SettingValue::Text(e.into())).is_ok());
        }
    }
}
