//! ASUS ROG / TUF peripherals (VID 0x0b05): mice, Aura keyboards, ROG
//! headsets, and the ROG Omni receiver hosting a mouse and a keyboard.
//!
//! Protocol sources (cite every one):
//! - libratbag `src/asus.c`, `src/asus.h`, `src/driver-asus.c` (MIT) and
//!   `data/devices/asus-*.device`: command set (`12 xx` reads, `51 31`
//!   settings, `51 28` LEDs, `50 03` save), settings layout, DPI encoding,
//!   PIDs, Omni receiver report id 3, `FF AA` error reply.
//! - G-Helper `app/Peripherals/Mouse/AsusMouse.cs` and `Models/*.cs`
//!   (GPL-3.0, cited only): battery `12 07`, energy `51 37`, lift-off
//!   `51 35`, DPI colours `12 04 03`, active stage, per-model capabilities.
//!   Its live colour streaming sends `51 28` without `50 03`, so LED writes
//!   take effect without a save.
//! - OpenRGB `Controllers/AsusAuraUSBController/AsusAuraMouseController/*` and
//!   `AsusAuraKeyboardController/*` (GPL-2.0, cited only): LED packet, zones,
//!   mode values, vendor interfaces, keyboard `51 2C` effect packet and speed
//!   ranges; save (`50 03` / `50 55`) is a separate, manual step there.
//! - github.com/BillyNeverDies/rog-mouse-battery (MIT): Gladius III Wireless
//!   battery percent at reply byte 4, vendor interface 0, unnumbered reports.
//! - github.com/OthrineTM/pelta-control (GPL-3.0, cited only): ROG Pelta.
//! - G-Helper `app/Peripherals/PeripheralsProvider.cs`, `Keyboard/*`,
//!   `Headset/*` (GPL-3.0, cited only): Omni pairing list (`A0` on report 1;
//!   one mouse on report 3, one keyboard on report 2), Harpe II receiver,
//!   keyboard battery `12 01`, keyboard / headset PIDs and capabilities,
//!   mouse debounce `51 31 <stages+1>` (field also in libratbag) and active
//!   stage `51 31 09`.
//!
//! No save command is ever sent: every write is volatile and stored settings
//! are re-applied on (re)connect. Profiles and onboard button maps are untouched.

mod headset;
mod keyboards;
mod mice;
mod mouse_models;
mod protocol;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use anyhow::Context;
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::get_setting;
use hako_core::device::{DeviceId, DeviceKind};
use protocol::{Effect, Transport};
use tracing::debug;

const N_MICE: usize = mouse_models::MICE.len();
const N_KB: usize = keyboards::KEYBOARDS.len();
const N_HS: usize = headset::HEADSETS.len();
const N: usize = N_MICE + N_KB + N_HS + 1;

/// libratbag `asus-rog-omni-receiver.device`, G-Helper `OMNI_PID`.
const OMNI_PIDS: &[u16] = &[0x1ace];

const fn model(
    name: &'static str,
    product_ids: &'static [u16],
    kind: DeviceKind,
    open: fn(u16, &DriverCtx) -> Opened,
) -> Model {
    Model {
        vendor_id: protocol::VID,
        product_ids,
        kind,
        name,
        usbfs: false,
        multi: false,
        open,
    }
}

/// Registry rows generated from the per-family tables.
const ALL: [Model; N] = {
    const EMPTY: Model = model("", &[], DeviceKind::Other, open_mouse);
    let mut out = [EMPTY; N];
    let mut i = 0;
    while i < N_MICE {
        let m = &mouse_models::MICE[i];
        out[i] = model(m.name, m.pids, DeviceKind::Mouse, open_mouse);
        i += 1;
    }
    let mut j = 0;
    while j < N_KB {
        let k = &keyboards::KEYBOARDS[j];
        out[N_MICE + j] = model(k.name, k.pids, DeviceKind::Keyboard, open_keyboard);
        j += 1;
    }
    let mut h = 0;
    while h < N_HS {
        let s = &headset::HEADSETS[h];
        out[N_MICE + N_KB + h] = model(s.name, s.pids, DeviceKind::Headset, open_headset);
        h += 1;
    }
    out[N - 1] = Model {
        multi: true,
        ..model(
            "ASUS ROG Omni Receiver",
            OMNI_PIDS,
            DeviceKind::Mouse,
            open_omni,
        )
    };
    out
};

pub const MODELS: &[Model] = &ALL;

fn open_mouse(pid: u16, ctx: &DriverCtx) -> Opened {
    let spec = mice::find(pid).context("unknown ASUS mouse PID")?;
    let a = mice::MouseAdapter::open(spec, pid, ctx.paths.clone())?;
    Ok(vec![Box::new(a) as Box<dyn DeviceAdapter>])
}

fn open_keyboard(pid: u16, ctx: &DriverCtx) -> Opened {
    let spec = keyboards::find(pid).context("unknown ASUS keyboard PID")?;
    let a = keyboards::KeyboardAdapter::open(spec, pid, ctx.paths.clone())?;
    Ok(vec![Box::new(a) as Box<dyn DeviceAdapter>])
}

fn open_headset(pid: u16, ctx: &DriverCtx) -> Opened {
    let spec = headset::find(pid).context("unknown ASUS headset PID")?;
    let a = headset::HeadsetAdapter::open(spec, pid, ctx.paths.clone())?;
    Ok(vec![Box::new(a) as Box<dyn DeviceAdapter>])
}

/// The first known mouse and the first known keyboard on the receiver's
/// pairing list (G-Helper drives one of each: mice share report 3,
/// keyboards report 2), minus those already open.
fn open_omni(pid: u16, ctx: &DriverCtx) -> Opened {
    let (mut list, path) = Transport::open(pid, protocol::OMNI_IFACE, protocol::OMNI_LIST_REPORT)?;
    let paired = protocol::parse_paired(&list.request(protocol::OMNI_LIST)?);
    drop(list);
    debug!("Omni receiver {path}: paired {paired:04x?}");
    let is_open = |paired: u16| {
        ctx.open.contains(&DeviceId {
            vendor_id: protocol::VID,
            product_id: paired,
            instance: path.clone(),
        })
    };
    let mut out: Vec<Box<dyn DeviceAdapter>> = Vec::new();
    let mouse = paired
        .iter()
        .find_map(|&d| mice::find_omni(d).map(|s| (s, d)));
    if let Some((spec, d)) = mouse.filter(|&(_, d)| !is_open(d)) {
        let a = mice::MouseAdapter::open_paired(spec, pid, d, ctx.paths.clone())?;
        out.push(Box::new(a));
    }
    let keyboard = paired
        .iter()
        .find_map(|&d| keyboards::find_omni(d).map(|s| (s, d)));
    if let Some((spec, d)) = keyboard.filter(|&(_, d)| !is_open(d)) {
        let a = keyboards::KeyboardAdapter::open_paired(spec, pid, d, ctx.paths.clone())?;
        out.push(Box::new(a));
    }
    Ok(out)
}

// ── Lighting settings shared by mice and keyboards ─────────────────────────

fn lighting_defaults(s: &mut SettingsMap, speed: bool) {
    let mut defaults = vec![
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::Brightness, SettingValue::Int(100)),
        (SettingKey::ColorR, SettingValue::Int(0)),
        (SettingKey::ColorG, SettingValue::Int(120)),
        (SettingKey::ColorB, SettingValue::Int(255)),
    ];
    if speed {
        defaults.push((SettingKey::Speed, SettingValue::Text("medium".into())));
    }
    for (k, v) in defaults {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

fn effect_of(s: &SettingsMap) -> Effect {
    Effect::parse(
        get_setting(s, SettingKey::Effect)
            .and_then(|v| v.as_str())
            .unwrap_or("static"),
    )
}

fn int_of(s: &SettingsMap, key: SettingKey, default: i64) -> i64 {
    get_setting(s, key)
        .and_then(|v| v.as_int())
        .unwrap_or(default)
}

fn rgb_of(s: &SettingsMap) -> [u8; 3] {
    [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB].map(|k| int_of(s, k, 0) as u8)
}

/// Effect → Brightness → Speed → Color, irrelevant ones hidden.
fn lighting_controls(effect: Effect, brightness_step: f64, speed: bool) -> Vec<SettingMeta> {
    use SettingKey::*;
    let mut c = vec![
        SettingMeta::dropdown(
            Effect,
            "Effect",
            vec![
                SettingOption::text("static", "Static"),
                SettingOption::text("breathing", "Breathing"),
                SettingOption::text("cycle", "Color cycle"),
            ],
        ),
        SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, brightness_step).unit("%"),
    ];
    if speed {
        c.push(
            SettingMeta::dropdown(
                Speed,
                "Speed",
                vec![
                    SettingOption::text("slow", "Slow"),
                    SettingOption::text("medium", "Medium"),
                    SettingOption::text("fast", "Fast"),
                ],
            )
            .hidden(effect == protocol::Effect::Static),
        );
    }
    for (key, label) in [(ColorR, "Red"), (ColorG, "Green"), (ColorB, "Blue")] {
        c.push(SettingMeta::color(key, label).hidden(!effect.has_color()));
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn registry_matches_tables_without_duplicate_pids() {
        assert_eq!(MODELS.len(), N);
        let mut seen = HashSet::new();
        for m in MODELS {
            assert!(
                !m.name.is_empty() && !m.product_ids.is_empty(),
                "{}",
                m.name
            );
            for &pid in m.product_ids {
                assert!(seen.insert(pid), "{pid:04x} listed twice ({})", m.name);
                let family = match m.kind {
                    _ if m.multi => OMNI_PIDS.contains(&pid).then_some(m.name),
                    DeviceKind::Mouse => mice::find(pid).map(|s| s.name),
                    DeviceKind::Keyboard => keyboards::find(pid).map(|s| s.name),
                    _ => headset::find(pid).map(|s| s.name),
                };
                assert_eq!(family, Some(m.name), "{pid:04x}");
            }
        }
    }

    #[test]
    fn lighting_controls_hide_what_the_effect_ignores() {
        let c = lighting_controls(Effect::Cycle, 25.0, true);
        let hidden = |k| c.iter().find(|m| m.key == k).unwrap().hidden;
        assert!(hidden(SettingKey::ColorR));
        assert!(!hidden(SettingKey::Speed));
        let c = lighting_controls(Effect::Static, 1.0, false);
        assert!(c.iter().all(|m| m.key != SettingKey::Speed));
        assert!(
            !c.iter()
                .find(|m| m.key == SettingKey::ColorB)
                .unwrap()
                .hidden
        );

        let mut s = SettingsMap::new();
        lighting_defaults(&mut s, true);
        assert_eq!(effect_of(&s), Effect::Static);
        assert_eq!(rgb_of(&s), [0, 120, 255]);
    }
}
