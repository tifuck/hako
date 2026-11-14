//! ASUS ROG wireless headsets on the report-0xCC protocol: ROG Pelta, ROG
//! Delta II. Battery, charging, lighting, sidetone and sleep timer; on the
//! Pelta also strobing, AI noise reduction and the voice prompt language.
//!
//! Protocol: github.com/OthrineTM/pelta-control `pelta/device.py` and README
//! (Pelta, reverse engineered from ASUS `AacAudioHal_x64.dll`: interface 3)
//! and G-Helper `app/Peripherals/Headset/AsusHeadset.cs`, `Models/Pelta.cs`,
//! `Models/DeltaII.cs` (usage page 0xFF00, same commands). Both agree on the
//! `12 07` power reply, `51 37` power write, `51 28` lighting and the
//! `41 11` / `61 11` sidetone writes. For the Pelta they also agree on effect
//! 3 (strobing), `41 20` / `41 02` noise reduction and `12 28` / `41 0A` voice
//! prompt (1 English, 2 Chinese, 0 prompt sound). Neither says whether the
//! headset keeps these; like sidetone they are re-sent on (re)connect.
//!
//! Left out: the same three on the Delta II (G-Helper only; it also sends a
//! `41 10` noise reduction level there), `51 10` (pelta-control: 0 =
//! hardware effects; G-Helper: 0 = lights off), the Pelta equalizer
//! (G-Helper only, 0-24 vs 0-100 gain scales between models), the Pelta
//! cable PID `1b82` (pelta-control: untested). Sidetone levels stop at 20:
//! pelta-control's maximum (G-Helper allows 100 on the Pelta, 20 elsewhere).

use super::protocol::{self as p, Transport, VID};
use super::{effect_of, int_of, lighting_controls, lighting_defaults, rgb_of};
use crate::drivers::DeviceAdapter;
use anyhow::{Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::info;

const REPORT_ID: u8 = 0xcc;
const POLL: Duration = Duration::from_secs(30);
/// While the headset is off or unlinked, look for it more often.
const RELINK_POLL: Duration = Duration::from_secs(5);

pub struct HeadsetSpec {
    pub name: &'static str,
    pub pids: &'static [u16],
    /// Interface of the 0xCC collection; `None` = node with usage page 0xFF00.
    pub iface: Option<u32>,
    /// `12 00 00 01` reports whether the headset is linked to the dongle.
    pub link_check: bool,
    /// Strobing, noise reduction and voice prompt (two sources: Pelta only).
    pub extras: bool,
}

pub const HEADSETS: &[HeadsetSpec] = &[
    // [pelta-control: dongle 1b84, interface 3, link check; G Pelta]
    HeadsetSpec {
        name: "ASUS ROG Pelta",
        pids: &[0x1b84],
        iface: Some(3),
        link_check: true,
        extras: true,
    },
    // [G DeltaII, DeltaIIKjp]
    HeadsetSpec {
        name: "ASUS ROG Delta II",
        pids: &[0x1afa],
        iface: None,
        link_check: false,
        extras: false,
    },
    HeadsetSpec {
        name: "ASUS ROG Delta II KJP",
        pids: &[0x1d41],
        iface: None,
        link_check: false,
        extras: false,
    },
];

pub fn find(pid: u16) -> Option<&'static HeadsetSpec> {
    HEADSETS.iter().find(|h| h.pids.contains(&pid))
}

pub struct HeadsetAdapter {
    spec: &'static HeadsetSpec,
    id: DeviceId,
    t: Option<Transport>,
    settings: SettingsMap,
    battery: BatteryState,
    /// Low-battery level and voice prompt from `12 07`, resent with the sleep timer.
    power: Option<(u8, u8)>,
    online: bool,
    last_poll: Instant,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl HeadsetAdapter {
    pub fn open(spec: &'static HeadsetSpec, pid: u16, paths: ConfigPaths) -> Result<Self> {
        let (t, path) = match spec.iface {
            Some(iface) => Transport::open(pid, iface, REPORT_ID)?,
            None => Transport::open_page(pid, 0xff00, REPORT_ID)?,
        };
        let mut settings = hako_core::config::load_device_settings(&paths, VID, spec.pids[0])?;
        let offered = controls(spec, &settings);
        crate::drivers::sanitize_saved(&mut settings, &offered);
        let mut a = Self {
            spec,
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: path.clone(),
            },
            t: Some(t),
            settings,
            battery: BatteryState {
                label: "Headset".into(),
                percent: None,
                charging: false,
                present: false,
            },
            power: None,
            online: false,
            last_poll: Instant::now(),
            last_error: None,
            paths,
        };
        a.poll();
        info!("{} open via {path} online={}", spec.name, a.online);
        Ok(a)
    }

    fn t(&mut self) -> Result<&mut Transport> {
        self.t.as_mut().context("device closed")
    }

    /// Battery and charging; on the first answer after an absence, read
    /// unset values from the headset and push every setting.
    fn poll(&mut self) {
        self.last_poll = Instant::now();
        let was_online = self.online;
        let r = self.read_power();
        self.online = matches!(r, Ok(Some(_)));
        self.battery.present = self.online;
        match r {
            Ok(Some((pct, charging))) => {
                self.battery.percent = Some(pct);
                self.battery.charging = charging;
                if !was_online {
                    self.connect();
                }
            }
            Ok(None) => {
                self.battery.percent = None;
                self.last_error = Some("headset not linked to the dongle".into());
            }
            Err(e) => {
                self.battery.percent = None;
                self.last_error = Some(e.to_string());
            }
        }
    }

    fn read_power(&mut self) -> Result<Option<(u8, bool)>> {
        let link_check = self.spec.link_check;
        let t = self.t()?;
        if link_check && t.transact(p::HS_LINK)?[4] == 0 {
            return Ok(None);
        }
        let power = p::hs_power(&t.transact(p::GET_BATTERY)?);
        let Some(pct) = power.battery else {
            return Ok(None);
        };
        let charging = t.transact(p::HS_CHARGING)?[4] == 1;
        self.power = Some((power.low_battery, power.low_battery_voice));
        if p::HS_SLEEP_MIN.contains(&power.sleep) {
            self.settings
                .entry(SettingKey::PmShutdown.as_str().into())
                .or_insert(SettingValue::Int(power.sleep.into()));
        }
        Ok(Some((pct, charging)))
    }

    fn connect(&mut self) {
        if let Err(e) = self.read_unset() {
            self.last_error = Some(format!("read settings: {e}"));
        }
        lighting_defaults(&mut self.settings, false);
        let mut errors = Vec::new();
        for key in [
            SettingKey::Effect,
            SettingKey::MicSideTone,
            SettingKey::PmShutdown,
            SettingKey::MicNoiseFilter,
            SettingKey::VoicePrompts,
        ] {
            if let Err(e) = self.apply(key) {
                errors.push(format!("{}: {e}", key.as_str()));
            }
        }
        self.last_error = (!errors.is_empty()).then(|| errors.join("; "));
    }

    fn read_unset(&mut self) -> Result<()> {
        let mut hw = SettingsMap::new();
        let extras = self.spec.extras;
        let t = self.t()?;
        let led = p::hs_parse_led(&t.transact(p::HS_LIGHTING)?)
            .filter(|&(effect, ..)| effect != p::HS_STROBE || extras);
        if let Some((effect, brightness, [r, g, b])) = led {
            let name = p::HS_EFFECTS
                .iter()
                .find(|&&(w, _)| w == effect)
                .map_or("static", |&(_, n)| n);
            for (k, v) in [
                (SettingKey::Effect, SettingValue::Text(name.into())),
                (SettingKey::Brightness, SettingValue::Int(brightness.into())),
                (SettingKey::ColorR, SettingValue::Int(r.into())),
                (SettingKey::ColorG, SettingValue::Int(g.into())),
                (SettingKey::ColorB, SettingValue::Int(b.into())),
            ] {
                merge_setting(&mut hw, k, v);
            }
        }
        let on = t.transact(p::HS_SIDETONE_ON)?[4] == 1;
        let level = t.transact(p::HS_SIDETONE_LEVEL)?[4];
        if level <= p::HS_SIDETONE_MAX {
            let v = if on { level } else { 0 };
            merge_setting(
                &mut hw,
                SettingKey::MicSideTone,
                SettingValue::Int(v.into()),
            );
        }
        if extras {
            if let Ok(r) = t.transact(p::HS_NOISE_REDUCTION) {
                let on = SettingValue::Bool(r[4] == 1);
                merge_setting(&mut hw, SettingKey::MicNoiseFilter, on);
            }
            if let Ok(r) = t.transact(p::HS_VOICE_PROMPT) {
                if p::HS_VOICE_PROMPTS.iter().any(|&(w, _)| w == r[4]) {
                    let v = SettingValue::Int(r[4].into());
                    merge_setting(&mut hw, SettingKey::VoicePrompts, v);
                }
            }
        }
        for (k, v) in hw {
            self.settings.entry(k).or_insert(v);
        }
        Ok(())
    }

    fn apply(&mut self, key: SettingKey) -> Result<()> {
        match key {
            SettingKey::Effect
            | SettingKey::Brightness
            | SettingKey::ColorR
            | SettingKey::ColorG
            | SettingKey::ColorB => {
                let effect = effect_wire(self.spec, &self.settings);
                let brightness = int_of(&self.settings, SettingKey::Brightness, 100) as u8;
                let rgb = rgb_of(&self.settings);
                self.t()?
                    .transact(&p::hs_led(effect, brightness, rgb))
                    .map(drop)
            }
            SettingKey::MicSideTone => {
                let Some(level) = get_setting(&self.settings, key).and_then(|v| v.as_int()) else {
                    return Ok(());
                };
                let level = level as u8;
                let t = self.t()?;
                t.transact(&p::hs_sidetone(level > 0))?;
                if level > 0 {
                    t.transact(&p::hs_sidetone_level(level))?;
                }
                Ok(())
            }
            SettingKey::PmShutdown => {
                let Some(sleep) = get_setting(&self.settings, key).and_then(|v| v.as_int()) else {
                    return Ok(());
                };
                let (low, voice) = self.power.context("power state not read yet")?;
                self.t()?
                    .transact(&p::hs_set_power(sleep as u8, low, voice))
                    .map(drop)
            }
            SettingKey::MicNoiseFilter => {
                let Some(on) = get_setting(&self.settings, key).and_then(|v| v.as_bool()) else {
                    return Ok(());
                };
                self.t()?.transact(&p::hs_set_noise_reduction(on)).map(drop)
            }
            SettingKey::VoicePrompts => {
                let Some(v) = get_setting(&self.settings, key).and_then(|v| v.as_int()) else {
                    return Ok(());
                };
                self.t()?
                    .transact(&p::hs_set_voice_prompt(v as u8))
                    .map(drop)
            }
            _ => Ok(()),
        }
    }
}

/// Wire value of the chosen effect; static when unknown or not offered.
fn effect_wire(spec: &HeadsetSpec, settings: &SettingsMap) -> u8 {
    let name = get_setting(settings, SettingKey::Effect)
        .and_then(|v| v.as_str())
        .unwrap_or("static");
    p::HS_EFFECTS
        .iter()
        .find(|&&(w, n)| n == name && (w != p::HS_STROBE || spec.extras))
        .map_or(1, |&(w, _)| w)
}

pub fn controls(spec: &HeadsetSpec, settings: &SettingsMap) -> Vec<SettingMeta> {
    let mut c = lighting_controls(effect_of(settings), 1.0, false);
    if spec.extras {
        if let Some(m) = c.iter_mut().find(|m| m.key == SettingKey::Effect) {
            m.options
                .insert(2, SettingOption::text("strobing", "Strobing"));
        }
    }
    c.push(SettingMeta::slider(
        SettingKey::MicSideTone,
        "Sidetone",
        0.0,
        p::HS_SIDETONE_MAX.into(),
        1.0,
    ));
    if spec.extras {
        c.push(SettingMeta::toggle(
            SettingKey::MicNoiseFilter,
            "AI noise reduction",
        ));
        let opts = p::HS_VOICE_PROMPTS
            .iter()
            .map(|&(v, label)| SettingOption::int(v.into(), label))
            .collect();
        c.push(SettingMeta::dropdown(
            SettingKey::VoicePrompts,
            "Voice prompt",
            opts,
        ));
    }
    let opts = p::HS_SLEEP_MIN
        .iter()
        .map(|&m| match m {
            0 => SettingOption::int(0, "Never"),
            m => SettingOption::int(m.into(), &format!("{m} min")),
        })
        .collect();
    c.push(SettingMeta::dropdown(
        SettingKey::PmShutdown,
        "Sleep timer",
        opts,
    ));
    c
}

impl DeviceAdapter for HeadsetAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Headset,
            name: self.spec.name.into(),
            connected: self.t.is_some(),
            batteries: vec![self.battery.clone()],
            settings: self.settings.clone(),
            controls: controls(self.spec, &self.settings),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        if !self.online {
            anyhow::bail!("{} is not linked to the dongle", self.spec.name);
        }
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        if let Err(e) = self.apply(key) {
            self.settings = before;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        self.last_error = None;
        hako_core::config::save_device_settings(&self.paths, VID, self.spec.pids[0], &self.settings)
            .map_err(|e| anyhow::anyhow!("applied, but not saved: {e:#}"))
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        let every = if self.online { POLL } else { RELINK_POLL };
        if self.last_poll.elapsed() >= every {
            self.poll();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.t = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_and_controls() {
        for h in HEADSETS {
            assert_eq!(find(h.pids[0]).map(|s| s.name), Some(h.name));
        }
        let mut s = SettingsMap::new();
        lighting_defaults(&mut s, false);
        let c = controls(&HEADSETS[1], &s);
        let side = c.iter().find(|m| m.key == SettingKey::MicSideTone).unwrap();
        assert!(side.validate(&SettingValue::Int(20)).is_ok());
        assert!(side.validate(&SettingValue::Int(21)).is_err());
        let sleep = c.iter().find(|m| m.key == SettingKey::PmShutdown).unwrap();
        for m in p::HS_SLEEP_MIN {
            assert!(sleep.validate(&SettingValue::Int(m.into())).is_ok());
        }
        assert!(sleep.validate(&SettingValue::Int(1)).is_err());
        assert!(c.iter().all(|m| m.key != SettingKey::Speed));
        // Delta II: no strobing, noise reduction or voice prompt.
        let strobing = SettingValue::Text("strobing".into());
        let effect = c.iter().find(|m| m.key == SettingKey::Effect).unwrap();
        assert!(effect.validate(&strobing).is_err());
        assert!(c
            .iter()
            .all(|m| m.key != SettingKey::MicNoiseFilter && m.key != SettingKey::VoicePrompts));
        merge_setting(&mut s, SettingKey::Effect, strobing.clone());
        assert_eq!(effect_wire(&HEADSETS[1], &s), 1);
    }

    #[test]
    fn saved_values_are_sanitized_before_they_are_sent() {
        // `as u8` would send 266 as 10 and a 256-minute sleep timer as 0 (never).
        let mut s = SettingsMap::new();
        merge_setting(&mut s, SettingKey::MicSideTone, SettingValue::Int(266));
        merge_setting(&mut s, SettingKey::PmShutdown, SettingValue::Int(256));
        let c = controls(&HEADSETS[1], &s);
        crate::drivers::sanitize_saved(&mut s, &c);
        assert_eq!(
            get_setting(&s, SettingKey::MicSideTone),
            Some(&SettingValue::Int(p::HS_SIDETONE_MAX.into()))
        );
        assert_eq!(get_setting(&s, SettingKey::PmShutdown), None);
    }

    #[test]
    fn pelta_extras() {
        let pelta = find(0x1b84).unwrap();
        let mut s = SettingsMap::new();
        lighting_defaults(&mut s, false);
        let c = controls(pelta, &s);
        let effect = c.iter().find(|m| m.key == SettingKey::Effect).unwrap();
        let names: Vec<_> = effect.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(names, ["Static", "Breathing", "Strobing", "Color cycle"]);
        let nr = c
            .iter()
            .find(|m| m.key == SettingKey::MicNoiseFilter)
            .unwrap();
        assert!(nr.validate(&SettingValue::Bool(true)).is_ok());
        let vp = c
            .iter()
            .find(|m| m.key == SettingKey::VoicePrompts)
            .unwrap();
        for v in [0, 1, 2] {
            assert!(vp.validate(&SettingValue::Int(v)).is_ok());
        }
        assert!(vp.validate(&SettingValue::Int(3)).is_err());
        for (name, wire) in [
            ("static", 1),
            ("breathing", 2),
            ("strobing", 3),
            ("cycle", 4),
        ] {
            merge_setting(&mut s, SettingKey::Effect, SettingValue::Text(name.into()));
            assert_eq!(effect_wire(pelta, &s), wire);
        }
    }
}
