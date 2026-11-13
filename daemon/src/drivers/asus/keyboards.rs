//! ASUS ROG / TUF Aura keyboards: whole-board lighting effects, battery.
//!
//! Row sources: \[O\] OpenRGB `AsusAuraKeyboardControllerDetect.cpp` and
//! `RGBController_AsusAuraKeyboard.cpp` (PIDs, interface + usage page
//! 0xFF00, per-model speed ranges, K1/K5 without per-key layout); \[G\]
//! G-Helper `app/Peripherals/Keyboard/Models/*.cs` and
//! `PeripheralsProvider.cs` (PIDs, `mi_xx` interface or usage page 0xFF00,
//! `AuraSpeedByte`, `HasBattery` / `ParseBattery`, Omni pairing-list PIDs).
//! Speed bytes run from slow to fast (255→0 on newer boards, 15→0 on older
//! ones, 0→2 on the K1); slow / medium / fast use the source's values.
//!
//! Left out: ROG Claymore (O drives it with a different packet and a
//! remote-control handshake), TUF K3 Gen II Miku `1c60` (G) vs `1c5e` (O).
//!
//! Save to device memory (owner opt-in): `50 55` after each user change, and
//! once when the mode is turned on, stores the active profile's lighting (O
//! `SaveMode`, G `SaveLighting`; see docs/onboard-writes/asus.md). Neither
//! source reads the stored lighting back, so the save cannot be verified
//! beyond the board's echo. Never sent on open.

use super::protocol::{self as p, Exchange, Transport, VID};
use super::{effect_of, int_of, lighting_controls, lighting_defaults, rgb_of};
use crate::drivers::DeviceAdapter;
use anyhow::{Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue, SettingsMap};
use hako_core::config::{get_setting, merge_setting, ConfigPaths};
use hako_core::device::{BatteryState, DeviceId, DeviceKind, DeviceSnapshot};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const BATTERY_POLL: Duration = Duration::from_secs(30);

pub struct KeyboardSpec {
    pub name: &'static str,
    /// Cable PID first (canonical), then the 2.4 GHz dongle.
    pub pids: &'static [u16],
    /// Vendor interface; `None` = the node with usage page 0xFF00.
    pub iface: Option<u32>,
    /// Speed byte for slow, medium, fast.
    pub speed: [u8; 3],
    /// TUF K1 / K5: one zone, RGB without the per-key `02` marker.
    pub flat: bool,
    /// `12 01` battery byte; `None` = wired only.
    pub battery: Option<usize>,
    /// PIDs that report no battery (cable-only variant of a wireless board).
    pub no_battery: &'static [u16],
    /// PIDs the Omni receiver's pairing list reports for this board.
    pub omni: &'static [u16],
    /// Why `50 55` is not offered, if it is not.
    pub save_blocked: Option<&'static str>,
}

/// O: range 255 (slow) ..= 0 (fast), default 30.
const S255: [u8; 3] = [255, 30, 0];
/// O: range 15 (slow) ..= 0 (fast), default 8.
const S15: [u8; 3] = [15, 8, 0];
/// G `AuraSpeedByte`: 255 / 30 / 10.
const G255: [u8; 3] = [255, 30, 10];
/// G Claymore II, TUF K3: 12 / 8 / 4.
const G12: [u8; 3] = [12, 8, 4];
/// O / G TUF K1: 0 (slow) ..= 2 (fast).
const K1: [u8; 3] = [0, 1, 2];
/// G `AsusKeyboard.ParseBattery` byte 5; Falchion boards byte 10.
const BAT: Option<usize> = Some(5);
const BAT_FALCHION: Option<usize> = Some(10);

const BASE: KeyboardSpec = KeyboardSpec {
    name: "",
    pids: &[],
    iface: Some(1),
    speed: S255,
    flat: false,
    battery: None,
    no_battery: &[],
    omni: &[],
    save_blocked: None,
};

const fn kb(name: &'static str, pids: &'static [u16], speed: [u8; 3]) -> KeyboardSpec {
    KeyboardSpec {
        name,
        pids,
        speed,
        ..BASE
    }
}

pub const KEYBOARDS: &[KeyboardSpec] = &[
    kb("ASUS ROG Strix Scope", &[0x18f8], S255),
    kb("ASUS ROG Strix Scope TKL", &[0x190c], S255),
    kb("ASUS ROG Strix Scope TKL PNK LTD", &[0x1954], S255),
    kb("ASUS ROG Strix Scope RX", &[0x1951], S255),
    // [G StrixScopeRXEVA, no interface named]
    KeyboardSpec {
        iface: None,
        ..kb("ASUS ROG Strix Scope RX EVA Edition", &[0x1a55], G255)
    },
    kb("ASUS ROG Strix Scope RX EVA-02 Edition", &[0x1b12], S255),
    // [O 1a05; G StrixScopeRXTKLWired 1a05 (no battery), Wireless 1a07 (battery)]
    KeyboardSpec {
        battery: BAT,
        no_battery: &[0x1a05],
        ..kb(
            "ASUS ROG Strix Scope RX TKL Wireless Deluxe",
            &[0x1a05, 0x1a07],
            S255,
        )
    },
    // Not in OpenRGB's speed table: its default range applies.
    kb(
        "ASUS ROG Strix Scope NX TKL Moonlight White",
        &[0x19d0],
        S15,
    ),
    kb(
        "ASUS ROG Strix Scope NX Wireless Deluxe",
        &[0x19f6, 0x19f8],
        S255,
    ),
    kb("ASUS ROG Strix Scope II", &[0x1ab3], S255),
    kb("ASUS ROG Strix Scope II RX", &[0x1ab5], S255),
    KeyboardSpec {
        battery: BAT,
        omni: &[0x1ab0],
        ..kb("ASUS ROG Strix Scope II 96 Wireless", &[0x1aae], S255)
    },
    KeyboardSpec {
        battery: BAT,
        omni: &[0x1b7a],
        ..kb("ASUS ROG Strix Scope II 96 RX Wireless", &[0x1b78], S255)
    },
    KeyboardSpec {
        battery: BAT,
        omni: &[0x1a85],
        ..kb("ASUS ROG Azoth", &[0x1a83, 0x1a85], S255)
    },
    // [G AzothExtreme, AzothExtremeSE, AzothX(Wireless): mi_01]
    KeyboardSpec {
        battery: BAT,
        omni: &[0x1b42],
        ..kb("ASUS ROG Azoth Extreme", &[0x1b3f], G255)
    },
    KeyboardSpec {
        battery: BAT,
        omni: &[0x1cf1],
        ..kb("ASUS ROG Azoth Extreme SE", &[0x1cef], G255)
    },
    KeyboardSpec {
        battery: BAT,
        ..kb("ASUS ROG Azoth X", &[0x1c24, 0x1c25], G255)
    },
    KeyboardSpec {
        battery: BAT_FALCHION,
        ..kb("ASUS ROG Falchion", &[0x193c, 0x193e], S255)
    },
    // [G FalchionRX (+ FalchionRXLowProfileOmni), FalchionAce, FalchionAceHFX]
    KeyboardSpec {
        iface: None,
        battery: BAT_FALCHION,
        omni: &[0x1b06],
        ..kb("ASUS ROG Falchion RX Low Profile", &[0x1b04], G255)
    },
    KeyboardSpec {
        iface: None,
        ..kb("ASUS ROG Falchion Ace", &[0x1a64], G255)
    },
    KeyboardSpec {
        iface: None,
        ..kb("ASUS ROG Falchion Ace HFX", &[0x1b7e], G255)
    },
    // [G ClaymoreII]; O: "not available for Claymore" (DeviceSaveMode).
    KeyboardSpec {
        iface: None,
        battery: BAT,
        save_blocked: Some(
            "only G-Helper saves on this board; OpenRGB disables saving on the Claymore line",
        ),
        ..kb("ASUS ROG Claymore II", &[0x196b], G12)
    },
    kb("ASUS ROG Strix Flare", &[0x1875], S15),
    kb("ASUS ROG Strix Flare PNK LTD", &[0x18cf], S15),
    kb("ASUS ROG Strix Flare CoD Black Ops 4", &[0x18af], S15),
    kb("ASUS ROG Strix Flare II", &[0x19fe], S255),
    kb("ASUS ROG Strix Flare II Animate", &[0x19fc], S255),
    // [O, G TUFK1: interface 2, no per-key layout]
    KeyboardSpec {
        iface: Some(2),
        flat: true,
        ..kb("ASUS TUF Gaming K1", &[0x1945], K1)
    },
    kb("ASUS TUF Gaming K3", &[0x194b], S15),
    kb("ASUS TUF Gaming K3 Gen II", &[0x1b30], S15),
    kb("ASUS TUF Gaming K3 Gen II Miku Edition", &[0x1c5e], S15),
    // [O: interface 2, no per-key layout]
    KeyboardSpec {
        iface: Some(2),
        flat: true,
        ..kb("ASUS TUF Gaming K5", &[0x1899], S255)
    },
    kb("ASUS TUF Gaming K7", &[0x18aa], S15),
    // [G TX98 (TUF K3 Gen II class: mi_01)]
    kb("ASUS TX 98", &[0x1b3a], G12),
];

pub fn find(pid: u16) -> Option<&'static KeyboardSpec> {
    KEYBOARDS.iter().find(|k| k.pids.contains(&pid))
}

/// The board the Omni receiver lists as `paired`.
pub fn find_omni(paired: u16) -> Option<&'static KeyboardSpec> {
    KEYBOARDS.iter().find(|k| k.omni.contains(&paired))
}

pub struct KeyboardAdapter {
    spec: &'static KeyboardSpec,
    id: DeviceId,
    t: Option<Transport>,
    settings: SettingsMap,
    battery: Option<BatteryState>,
    last_poll: Instant,
    last_error: Option<String>,
    paths: ConfigPaths,
}

impl KeyboardAdapter {
    pub fn open(spec: &'static KeyboardSpec, pid: u16, paths: ConfigPaths) -> Result<Self> {
        let (t, path) = match spec.iface {
            Some(iface) => Transport::open(pid, iface, 0)?,
            None => Transport::open_page(pid, 0xff00, 0)?,
        };
        Self::with(spec, pid, t, path, paths)
    }

    /// A board the Omni receiver `receiver` lists as `paired`.
    pub fn open_paired(
        spec: &'static KeyboardSpec,
        receiver: u16,
        paired: u16,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let (t, path) = Transport::open(receiver, p::OMNI_IFACE, p::OMNI_KEYBOARD_REPORT)?;
        Self::with(spec, paired, t, path, paths)
    }

    fn with(
        spec: &'static KeyboardSpec,
        pid: u16,
        t: Transport,
        path: String,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let mut settings = hako_core::config::load_device_settings(&paths, VID, spec.pids[0])?;
        lighting_defaults(&mut settings, true);
        let has_battery = spec.battery.is_some() && !spec.no_battery.contains(&pid);
        let mut a = Self {
            spec,
            id: DeviceId {
                vendor_id: VID,
                product_id: pid,
                instance: path.clone(),
            },
            t: Some(t),
            settings,
            battery: has_battery.then(|| BatteryState {
                label: "Keyboard".into(),
                percent: None,
                charging: false,
                present: true,
            }),
            last_poll: Instant::now(),
            last_error: None,
            paths,
        };
        // Lighting is volatile (no `50 55` save): restore it on every open.
        if let Err(e) = a.apply() {
            warn!("{}: restore lighting: {e}", spec.name);
            a.last_error = Some(e.to_string());
        }
        a.poll_battery();
        info!("{} open via {path}", spec.name);
        Ok(a)
    }

    fn apply(&mut self) -> Result<()> {
        let effect = effect_of(&self.settings);
        let speed = match get_setting(&self.settings, SettingKey::Speed).and_then(|v| v.as_str()) {
            Some("slow") => self.spec.speed[0],
            Some("fast") => self.spec.speed[2],
            _ => self.spec.speed[1],
        };
        let brightness = int_of(&self.settings, SettingKey::Brightness, 100).clamp(0, 100) as u8;
        let rgb = rgb_of(&self.settings);
        let t = self.t.as_mut().context("device closed")?;
        if self.spec.flat {
            t.send(&p::kb_effect_flat(effect, speed, brightness, rgb))
        } else {
            t.send(&p::kb_effect(effect, speed, brightness, rgb))
        }
    }

    fn saving(&self) -> bool {
        get_setting(&self.settings, SettingKey::SaveToDevice).and_then(|v| v.as_bool())
            == Some(true)
    }

    fn store(&mut self) -> Result<()> {
        store_lighting(self.t.as_mut().context("device closed")?)
    }

    fn poll_battery(&mut self) {
        self.last_poll = Instant::now();
        let (Some(at), Some(t)) = (self.spec.battery, self.t.as_mut()) else {
            return;
        };
        let Some(state) = self.battery.as_mut() else {
            return;
        };
        match t.transact(p::KB_POWER) {
            Ok(r) => {
                let (pct, charging) = p::kb_battery(&r, at);
                state.percent = Some(pct);
                state.charging = charging;
            }
            Err(_) => {
                state.percent = None;
                state.charging = false;
            }
        }
    }
}

impl DeviceAdapter for KeyboardAdapter {
    fn id(&self) -> &DeviceId {
        &self.id
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            id: self.id.clone(),
            kind: DeviceKind::Keyboard,
            name: self.spec.name.into(),
            connected: self.t.is_some(),
            batteries: self.battery.iter().cloned().collect(),
            settings: self.settings.clone(),
            controls: controls(self.spec, &self.settings),
            last_error: self.last_error.clone(),
        }
    }

    fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
        let before = self.settings.clone();
        merge_setting(&mut self.settings, key, value);
        // Turning save mode on stores the current lighting once; off writes nothing.
        let result = match key {
            SettingKey::SaveToDevice if self.saving() => self.store(),
            SettingKey::SaveToDevice => Ok(()),
            _ => self.apply(),
        };
        if let Err(e) = result {
            // The device kept its old value; so do the UI and the file.
            self.settings = before;
            self.last_error = Some(e.to_string());
            return Err(e);
        }
        let stored = if key != SettingKey::SaveToDevice && self.saving() {
            self.store()
                .context("changed, but not saved to device memory")
        } else {
            Ok(())
        };
        self.last_error = stored.as_ref().err().map(|e| format!("{e:#}"));
        hako_core::config::save_device_settings(
            &self.paths,
            VID,
            self.spec.pids[0],
            &self.settings,
        )?;
        stored
    }

    fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
        crate::drivers::set_colors(self, values, |a| vec![&mut a.settings])
    }

    fn tick(&mut self) -> Result<()> {
        if self.battery.is_some() && self.last_poll.elapsed() >= BATTERY_POLL {
            self.poll_battery();
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.t = None;
    }
}

/// `50 55`: the board echoes it once the profile is stored.
fn store_lighting(t: &mut impl Exchange) -> Result<()> {
    t.transact(p::KB_SAVE)
        .context("save to device memory")
        .map(drop)
}

fn controls(spec: &KeyboardSpec, settings: &SettingsMap) -> Vec<SettingMeta> {
    // OpenRGB: four brightness levels, sent as level * 25.
    let mut c = lighting_controls(effect_of(settings), 25.0, true);
    let save =
        SettingMeta::toggle(SettingKey::SaveToDevice, "Save changes to device memory").persistent();
    c.push(match spec.save_blocked {
        Some(reason) => save.blocked(reason),
        None => save,
    });
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBoard(Vec<Vec<u8>>, bool);

    impl Exchange for FakeBoard {
        fn transact(&mut self, cmd: &[u8]) -> Result<p::Packet> {
            self.0.push(cmd.to_vec());
            if self.1 {
                anyhow::bail!("no reply to 50 55");
            }
            Ok(p::packet(cmd))
        }
    }

    #[test]
    fn save_is_offered_where_both_sources_save() {
        let mut board = FakeBoard(vec![], false);
        store_lighting(&mut board).unwrap();
        assert_eq!(board.0, vec![vec![0x50, 0x55]]);
        assert!(store_lighting(&mut FakeBoard(vec![], true)).is_err());

        let save = |pid| {
            controls(find(pid).unwrap(), &SettingsMap::new())
                .into_iter()
                .find(|m| m.key == SettingKey::SaveToDevice)
                .unwrap()
        };
        // Strix Scope RX: OpenRGB and G-Helper both send 50 55.
        let scope = save(0x1951);
        assert!(scope.persistent && scope.blocked.is_none());
        let claymore = save(0x196b);
        assert!(claymore.persistent && claymore.blocked.is_some());
        assert!(claymore.validate(&SettingValue::Bool(true)).is_err());
    }

    #[test]
    fn table_lookup_and_speed_order() {
        for k in KEYBOARDS {
            assert_eq!(find(k.pids[0]).map(|s| s.name), Some(k.name));
            let [slow, mid, fast] = k.speed;
            assert!(
                (slow > mid && mid > fast) || (slow < mid && mid < fast),
                "{}",
                k.name
            );
            assert!(k.no_battery.iter().all(|p| k.pids.contains(p)));
            for &pid in k.omni {
                assert_eq!(find_omni(pid).map(|s| s.name), Some(k.name));
            }
        }
        assert_eq!(find(0x193e).unwrap().name, "ASUS ROG Falchion");
        // G-Helper KeyboardFromOmniPid.
        assert_eq!(
            find_omni(0x1b06).unwrap().name,
            "ASUS ROG Falchion RX Low Profile"
        );
        assert!(find_omni(0x1a94).is_none());
        assert!(find(0x1945).unwrap().flat);
    }
}
