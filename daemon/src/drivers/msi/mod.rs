//! MSI gaming peripherals (VID 0x0db0): Vigor GK30 keyboard and Clutch GM11
//! mouse, lighting only.
//!
//! Sources: MSI Center 2.0.75 (Gaming Gear and Mystic Light modules, static
//! analysis; `docs/vendor-re/msi.md`) and OpenRGB `Controllers/
//! MSIVigorController/*` and `MSIClutchController/*` (GPL-2.0, layouts and
//! IDs only: <https://gitlab.com/CalcProgrammer1/OpenRGB>). Both agree on the
//! reports (`gk30.rs`, `gm11.rs`) and MSI's code adds the state reads.
//!
//! The devices store every lighting change themselves and MSI's software has
//! no volatile mode or save command, so every control is an onboard write
//! (`.persistent()`: refused until the owner allows saving to device memory)
//! and nothing is ever written when the device is opened: the current state is
//! read back instead (`07 fd` on the GK30, `8d` / `85` on the GM11) and each
//! change is verified by reading the style back. DPI, report rate and buttons
//! of the GM11 are not offered: the DPI code table and the report rate codes
//! are not in MSI's managed code.
//!
//! `04d9:8022` is the GM11 as MSI's detection code lists it; nothing says it
//! speaks the protocol above, so it is listed for remaps only.

mod gk30;
mod gm11;

use crate::drivers::{DeviceAdapter, DriverCtx, Model, Opened};
use crate::hid::{find_hidraw, HidrawDevice};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use tracing::{info, warn};

const VID: u16 = 0x0db0;
const VID_HOLTEK: u16 = 0x04d9;

const fn model(
    vendor_id: u16,
    product_ids: &'static [u16],
    kind: DeviceKind,
    name: &'static str,
) -> Model {
    Model {
        vendor_id,
        product_ids,
        kind,
        name,
        usbfs: false,
        multi: false,
        open,
    }
}

pub const MODELS: &[Model] = &[
    model(VID, &[0x0b30], DeviceKind::Keyboard, "MSI Vigor GK30"),
    model(VID, &[0x0d11], DeviceKind::Mouse, "MSI Clutch GM11"),
    model(
        VID_HOLTEK,
        &[0x8022],
        DeviceKind::Mouse,
        "MSI Clutch GM11 (04d9)",
    ),
];

/// One lighting effect of a device.
pub(crate) struct Effect {
    id: &'static str,
    label: &'static str,
    /// Style byte.
    style: u8,
    /// Takes a colour / a speed.
    color: bool,
    speed: bool,
}

impl Effect {
    const fn new(
        id: &'static str,
        label: &'static str,
        style: u8,
        color: bool,
        speed: bool,
    ) -> Self {
        Self {
            id,
            label,
            style,
            color,
            speed,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Proto {
    Gk30,
    Gm11,
    /// Listed for remaps only.
    Passive,
}

fn proto(vid: u16, pid: u16) -> Option<Proto> {
    Some(match (vid, pid) {
        (VID, 0x0b30) => Proto::Gk30,
        (VID, 0x0d11) => Proto::Gm11,
        (VID_HOLTEK, 0x8022) => Proto::Passive,
        _ => return None,
    })
}

impl Proto {
    /// Interface of the command channel (GK30 report 7 collection, GM11
    /// vendor collection).
    fn iface(self) -> Option<u32> {
        match self {
            Proto::Gk30 => Some(1),
            Proto::Gm11 => Some(2),
            Proto::Passive => None,
        }
    }

    fn effects(self) -> &'static [Effect] {
        match self {
            Proto::Gk30 => gk30::EFFECTS,
            Proto::Gm11 => gm11::EFFECTS,
            Proto::Passive => &[],
        }
    }

    /// Brightness range; the GK30 has three levels.
    fn brightness(self) -> (u8, u8) {
        match self {
            Proto::Gk30 => gk30::LEVELS,
            _ => (0, 255),
        }
    }
}

fn effect_of(p: Proto, s: &SettingsMap) -> &'static Effect {
    let effects = p.effects();
    let id = get_setting(s, SettingKey::Effect)
        .and_then(|v| v.as_str())
        .unwrap_or("");
    effects.iter().find(|e| e.id == id).unwrap_or(&effects[0])
}

fn speed_level(s: &SettingsMap) -> u8 {
    match get_setting(s, SettingKey::Speed).and_then(|v| v.as_str()) {
        Some("slow") => 1,
        Some("fast") => 3,
        _ => 2,
    }
}

fn speed_name(level: u8) -> &'static str {
    match level {
        0 | 1 => "slow",
        2 => "medium",
        _ => "fast",
    }
}

fn rgb_of(s: &SettingsMap) -> [u8; 3] {
    [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB].map(|k| {
        get_setting(s, k)
            .and_then(|v| v.as_int())
            .unwrap_or(255)
            .clamp(0, 255) as u8
    })
}

fn defaults(p: Proto, s: &mut SettingsMap) {
    let (_, hi) = p.brightness();
    for (k, v) in [
        (SettingKey::Effect, SettingValue::Text("static".into())),
        (SettingKey::Brightness, SettingValue::Int(i64::from(hi))),
        (SettingKey::Speed, SettingValue::Text("medium".into())),
        (SettingKey::ColorR, SettingValue::Int(255)),
        (SettingKey::ColorG, SettingValue::Int(255)),
        (SettingKey::ColorB, SettingValue::Int(255)),
    ] {
        s.entry(k.as_str().into()).or_insert(v);
    }
}

fn controls(p: Proto, s: &SettingsMap) -> Vec<SettingMeta> {
    use SettingKey as K;
    if p == Proto::Passive {
        return vec![];
    }
    let effect = effect_of(p, s);
    let (lo, hi) = p.brightness();
    let mut c = vec![
        SettingMeta::dropdown(
            K::Effect,
            "Effect",
            p.effects()
                .iter()
                .map(|e| SettingOption::text(e.id, e.label))
                .collect(),
        ),
        SettingMeta::slider(
            K::Brightness,
            "Brightness",
            f64::from(lo),
            f64::from(hi),
            1.0,
        )
        .hidden(effect.style == 0),
        SettingMeta::dropdown(
            K::Speed,
            "Speed",
            vec![
                SettingOption::text("slow", "Slow"),
                SettingOption::text("medium", "Medium"),
                SettingOption::text("fast", "Fast"),
            ],
        )
        .hidden(!effect.speed),
    ];
    for (key, label) in [
        (K::ColorR, "Red"),
        (K::ColorG, "Green"),
        (K::ColorB, "Blue"),
    ] {
        c.push(SettingMeta::color(key, label).hidden(!effect.color));
    }
    c.into_iter().map(SettingMeta::persistent).collect()
}

fn open(pid: u16, ctx: &DriverCtx) -> Opened {
    let model = MODELS
        .iter()
        .find(|m| m.vendor_id == ctx.vendor_id && m.product_ids.contains(&pid))
        .with_context(|| format!("no MSI model for {:04x}:{pid:04x}", ctx.vendor_id))?;
    let p = proto(ctx.vendor_id, pid).with_context(|| format!("no protocol for {pid:04x}"))?;
    let info = find_hidraw(ctx.vendor_id, pid, p.iface())?;
    let dev = HidrawDevice::open(&info.path)?;
    let mut a = Lighting {
        name: model.name,
        kind: model.kind,
        proto: p,
        id: DeviceId {
            vendor_id: ctx.vendor_id,
            product_id: pid,
            instance: if info.uniq.is_empty() {
                info.path.display().to_string()
            } else {
                info.uniq
            },
        },
        dev: Some(dev),
        settings: SettingsMap::new(),
        last_error: None,
    };
    defaults(p, &mut a.settings);
    // The device keeps its own look: read it, never write on open.
    if let Err(e) = a.init_and_read() {
        warn!("{}: read lighting state: {e:#}", model.name);
        a.last_error = Some(format!("{e:#}"));
    }
    info!("{} open via {}", model.name, info.path.display());
    Ok(vec![Box::new(a)])
}

struct Lighting {
    name: &'static str,
    kind: DeviceKind,
    proto: Proto,
    id: DeviceId,
    dev: Option<HidrawDevice>,
    settings: SettingsMap,
    last_error: Option<String>,
}

impl Lighting {
    /// GM11 command: feature write, then the answer every command gets.
    fn gm11_xfer(&self, c: &gm11::Cmd) -> Result<[u8; 9]> {
        let dev = self.dev.as_ref().context("not connected")?;
        let mut out = *c;
        dev.set_feature(&mut out)?;
        let mut reply = [0u8; 9];
        dev.get_feature(&mut reply)?;
        Ok(reply)
    }

    fn gm11_style(&self) -> Result<u8> {
        Ok(self.gm11_xfer(&gm11::cmd(&gm11::READ_STYLE))?[2])
    }

    fn gk30_read(&self) -> Result<[u8; 8]> {
        let dev = self.dev.as_ref().context("not connected")?;
        let mut req = gk30::READ_REQUEST;
        dev.set_feature(&mut req)?;
        let mut r = [0u8; 8];
        r[0] = 0x07;
        dev.get_feature(&mut r)?;
        Ok(r)
    }

    fn init_and_read(&mut self) -> Result<()> {
        if self.proto == Proto::Gm11 {
            self.gm11_xfer(&gm11::handshake())?;
        }
        self.read_state()
    }

    /// Show what the device currently does.
    fn read_state(&mut self) -> Result<()> {
        match self.proto {
            Proto::Passive => {}
            Proto::Gk30 => {
                let s = gk30::parse(&self.gk30_read()?);
                let Some(e) = gk30::EFFECTS.iter().find(|e| e.style == s.style) else {
                    bail!("unknown style {:#04x}", s.style);
                };
                self.show(
                    e,
                    s.brightness,
                    s.speed,
                    gk30::PALETTE.get(s.colour as usize),
                );
            }
            Proto::Gm11 => {
                let style = self.gm11_style()?;
                let Some(e) = gm11::EFFECTS.iter().find(|e| e.style == style) else {
                    bail!("unknown style {style:#04x}");
                };
                let r = self.gm11_xfer(&gm11::read_params(style))?;
                let (bright, speed, colour) = gm11::parse_params(&r);
                // Colour 7 is the custom entry, which is not read back.
                self.show(e, bright, speed, gk30::PALETTE.get(colour as usize));
            }
        }
        Ok(())
    }

    fn show(&mut self, e: &Effect, brightness: u8, speed: u8, rgb: Option<&[u8; 3]>) {
        let (lo, hi) = self.proto.brightness();
        let s = &mut self.settings;
        merge_setting(s, SettingKey::Effect, SettingValue::Text(e.id.into()));
        if e.style != 0 {
            let b = brightness.clamp(lo, hi);
            merge_setting(s, SettingKey::Brightness, SettingValue::Int(i64::from(b)));
        }
        if e.speed {
            merge_setting(
                s,
                SettingKey::Speed,
                SettingValue::Text(speed_name(speed).into()),
            );
        }
        if let (true, Some(rgb)) = (e.color, rgb) {
            for (k, v) in [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB]
                .into_iter()
                .zip(rgb)
            {
                merge_setting(s, k, SettingValue::Int(i64::from(*v)));
            }
        }
    }

    /// Write the look in `settings`, then read the style back.
    fn apply(&mut self) -> Result<()> {
        let e = effect_of(self.proto, &self.settings);
        let bright = get_setting(&self.settings, SettingKey::Brightness)
            .and_then(|v| v.as_int())
            .unwrap_or(0);
        let (lo, hi) = self.proto.brightness();
        let bright = bright.clamp(i64::from(lo), i64::from(hi)) as u8;
        let speed = speed_level(&self.settings);
        let rgb = rgb_of(&self.settings);
        match self.proto {
            Proto::Passive => bail!("{} has no lighting controls", self.name),
            Proto::Gk30 => {
                let mut r = gk30::write(e, gk30::nearest(rgb), speed, bright);
                let dev = self.dev.as_ref().context("not connected")?;
                dev.set_feature(&mut r)?;
                let back = gk30::parse(&self.gk30_read()?);
                ensure!(
                    back.style == e.style,
                    "the keyboard reports style {:#04x} instead of {:#04x}",
                    back.style,
                    e.style
                );
            }
            Proto::Gm11 => {
                if e.color {
                    self.gm11_xfer(&gm11::palette_cmd())?;
                    self.dev
                        .as_mut()
                        .context("not connected")?
                        .write(&gm11::palette_report(rgb))?;
                }
                for c in gm11::style(e, bright, speed) {
                    self.gm11_xfer(&c)?;
                }
                let style = self.gm11_style()?;
                ensure!(
                    style == e.style,
                    "the mouse reports style {style:#04x} instead of {:#04x}",
                    e.style
                );
            }
        }
        Ok(())
    }
}

impl DeviceAdapter for Lighting {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: self.kind,
            name: self.name.into(),
            connected: self.dev.is_some(),
            batteries: vec![],
            settings: self.settings.clone(),
            controls: controls(self.proto, &self.settings),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        if let Err(e) = self.apply() {
            self.settings = before;
            let e = anyhow!("{e:#}");
            self.last_error = Some(format!("{e:#}"));
            return Err(e);
        }
        self.last_error = None;
        Ok(())
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown(&mut self) {
        self.dev = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn models_have_a_protocol_and_unique_ids() {
        let mut seen = HashSet::new();
        for m in MODELS {
            for &pid in m.product_ids {
                assert!(seen.insert((m.vendor_id, pid)));
                assert!(proto(m.vendor_id, pid).is_some(), "{}", m.name);
            }
        }
        assert_eq!(proto(VID, 0x0b30).unwrap().iface(), Some(1));
        assert_eq!(proto(VID, 0x0d11).unwrap().iface(), Some(2));
        assert_eq!(proto(VID_HOLTEK, 0x8022), Some(Proto::Passive));
        assert!(proto(VID, 0x0001).is_none());
    }

    #[test]
    fn lighting_controls_are_onboard_writes_and_validate() {
        for p in [Proto::Gk30, Proto::Gm11] {
            let mut s = SettingsMap::new();
            defaults(p, &mut s);
            for e in p.effects() {
                merge_setting(&mut s, SettingKey::Effect, SettingValue::Text(e.id.into()));
                let c = controls(p, &s);
                assert!(!c.is_empty());
                for m in &c {
                    assert!(m.persistent && m.blocked.is_none(), "{p:?} {:?}", m.key);
                    if let (false, Some(v)) = (m.hidden, get_setting(&s, m.key)) {
                        assert!(m.validate(v).is_ok(), "{p:?} {} {:?}", e.id, m.key);
                    }
                }
                let hidden = |k| c.iter().find(|m| m.key == k).unwrap().hidden;
                assert_eq!(hidden(SettingKey::ColorR), !e.color, "{}", e.id);
                assert_eq!(hidden(SettingKey::Speed), !e.speed, "{}", e.id);
            }
        }
        assert!(controls(Proto::Passive, &SettingsMap::new()).is_empty());
    }

    #[test]
    fn state_read_back_fills_the_settings() {
        let mut a = Lighting {
            name: "t",
            kind: DeviceKind::Keyboard,
            proto: Proto::Gk30,
            id: DeviceId {
                vendor_id: VID,
                product_id: 0x0b30,
                instance: "t".into(),
            },
            dev: None,
            settings: SettingsMap::new(),
            last_error: None,
        };
        defaults(Proto::Gk30, &mut a.settings);
        let e = &gk30::EFFECTS[1];
        a.show(e, 2, 3, gk30::PALETTE.get(4));
        let s = &a.settings;
        assert_eq!(effect_of(Proto::Gk30, s).id, "breathing");
        assert_eq!(speed_level(s), 3);
        assert_eq!(rgb_of(s), [0, 0, 255]);
        assert_eq!(
            get_setting(s, SettingKey::Brightness),
            Some(&SettingValue::Int(2))
        );
        // Not connected: a write fails and changes nothing.
        let before = a.settings.clone();
        assert!(a.set(SettingKey::Brightness, SettingValue::Int(1)).is_err());
        assert_eq!(a.settings, before);
        assert!(a.last_error.is_some());
    }
}
