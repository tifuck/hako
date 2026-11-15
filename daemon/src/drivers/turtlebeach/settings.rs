//! Settings of the Gen 3 headsets: CoAP keys and value encodings.
//!
//! Source: Turtle Beach Swarm II Mobile 6.27 (official Android app, read for
//! layouts only; `docs/vendor-re/turtlebeach.md` section 4), which agrees
//! with tb-stealth600's `set_kvp` keys. A setting is `PUT set_kvp
//! {"0x<key>":"<value>"}`; values are decimal strings (`1` / `0` for toggles,
//! an integer for levels, a list index for the auto-off time).
//!
//! Two key maps: Mosit (Stealth 500 / 600 / 700, Atlas Air; "CoAP v1") and
//! Corona (Stealth Pro II; "CoAP v2"), which moves the sidetone level from
//! key 630 to 620 and has no sidetone switch. Where the headset stores a value
//! is not established (the app never sends a save), so every control is
//! `.persistent()`: the owner has to allow saving to device memory first.

use anyhow::{bail, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingOption, SettingValue, SettingsMap};
use std::collections::BTreeMap;

/// Which key map a model uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Keys {
    /// Mosit headsets (CoAP v1).
    V1,
    /// Stealth Pro II (CoAP v2).
    V2,
}

#[derive(Clone, Copy)]
enum Kind {
    Toggle,
    Level,
    AutoOff,
}

type Entry = (&'static str, SettingKey, Kind);

const V1: &[Entry] = &[
    ("600", SettingKey::MicMute, Kind::Toggle),
    ("610", SettingKey::MicVolume, Kind::Level),
    ("620", SettingKey::MicSideTone, Kind::Toggle),
    ("630", SettingKey::MicSideToneVolume, Kind::Level),
    ("700", SettingKey::MicNoiseFilter, Kind::Toggle),
    ("280", SettingKey::PmShutdown, Kind::AutoOff),
];

const V2: &[Entry] = &[
    ("600", SettingKey::MicMute, Kind::Toggle),
    ("610", SettingKey::MicVolume, Kind::Level),
    ("620", SettingKey::MicSideToneVolume, Kind::Level),
    ("700", SettingKey::MicNoiseFilter, Kind::Toggle),
    ("750", SettingKey::NoiseCancelling, Kind::Toggle),
    ("280", SettingKey::PmShutdown, Kind::AutoOff),
];

/// Auto-off minutes by the index the headset stores in key 280.
const AUTO_OFF_MINUTES: [i64; 5] = [0, 5, 10, 20, 30];

fn table(keys: Keys) -> &'static [Entry] {
    match keys {
        Keys::V1 => V1,
        Keys::V2 => V2,
    }
}

/// CoAP resources besides GSI (which carries 280) that hold the keys above.
pub(super) fn resources(keys: Keys) -> &'static [&'static str] {
    match keys {
        Keys::V1 => &["Mic", "NG"],
        Keys::V2 => &["Mic", "SAF"],
    }
}

pub(super) fn controls(keys: Keys) -> Vec<SettingMeta> {
    table(keys)
        .iter()
        .map(|&(_, key, kind)| match (key, kind) {
            (SettingKey::MicMute, _) => SettingMeta::toggle(key, "Mic mute"),
            (SettingKey::MicVolume, _) => {
                SettingMeta::slider(key, "Mic sensitivity", 0.0, 100.0, 5.0)
            }
            (SettingKey::MicSideTone, _) => SettingMeta::toggle(key, "Sidetone"),
            (SettingKey::MicSideToneVolume, _) => {
                SettingMeta::slider(key, "Sidetone level", 0.0, 100.0, 5.0)
            }
            (SettingKey::MicNoiseFilter, _) => SettingMeta::toggle(key, "Noise gate"),
            (SettingKey::NoiseCancelling, _) => SettingMeta::toggle(key, "Noise cancelling"),
            (_, _) => SettingMeta::dropdown(
                key,
                "Auto-off",
                AUTO_OFF_MINUTES
                    .iter()
                    .map(|&m| match m {
                        0 => SettingOption::int(0, "Off"),
                        m => SettingOption::int(m, &format!("{m} min")),
                    })
                    .collect(),
            ),
        })
        .map(SettingMeta::persistent)
        .collect()
}

/// CoAP key and value string for a setting.
pub(super) fn encode(
    keys: Keys,
    key: SettingKey,
    value: &SettingValue,
) -> Result<(&'static str, String)> {
    let Some(&(coap, _, kind)) = table(keys).iter().find(|e| e.1 == key) else {
        bail!("{} is not supported", key.as_str());
    };
    let text = match (kind, value) {
        (Kind::Toggle, SettingValue::Bool(b)) => u8::from(*b).to_string(),
        (Kind::Level, v) => match v.as_int() {
            Some(n) if (0..=100).contains(&n) => n.to_string(),
            _ => bail!("{} must be 0..=100", key.as_str()),
        },
        (Kind::AutoOff, v) => match AUTO_OFF_MINUTES.iter().position(|&m| Some(m) == v.as_int()) {
            Some(i) => i.to_string(),
            None => bail!("auto-off must be 0, 5, 10, 20 or 30 minutes"),
        },
        _ => bail!("wrong value type for {}", key.as_str()),
    };
    Ok((coap, text))
}

/// Settings found in the key/value pairs of a CoAP resource.
pub(super) fn decode(keys: Keys, kvp: &BTreeMap<String, String>, out: &mut SettingsMap) {
    for &(coap, key, kind) in table(keys) {
        let Some(raw) = kvp.get(coap) else { continue };
        let value = match kind {
            Kind::Toggle => match raw.as_str() {
                "1" => SettingValue::Bool(true),
                "0" => SettingValue::Bool(false),
                _ => continue,
            },
            Kind::Level => match raw.parse::<f64>() {
                Ok(n) if (0.0..=100.0).contains(&n) => SettingValue::Int(n.round() as i64),
                _ => continue,
            },
            Kind::AutoOff => match raw
                .parse::<usize>()
                .ok()
                .and_then(|i| AUTO_OFF_MINUTES.get(i))
            {
                Some(&m) => SettingValue::Int(m),
                None => continue,
            },
        };
        out.insert(key.as_str().to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_follow_the_app() {
        let on = SettingValue::Bool(true);
        assert_eq!(
            encode(Keys::V1, SettingKey::MicSideTone, &on).unwrap(),
            ("620", "1".into())
        );
        assert_eq!(
            encode(
                Keys::V1,
                SettingKey::MicSideToneVolume,
                &SettingValue::Int(40)
            )
            .unwrap(),
            ("630", "40".into())
        );
        // CoAP v2 moves the sidetone level to 620 and has no switch.
        assert_eq!(
            encode(
                Keys::V2,
                SettingKey::MicSideToneVolume,
                &SettingValue::Int(40)
            )
            .unwrap(),
            ("620", "40".into())
        );
        assert!(encode(Keys::V2, SettingKey::MicSideTone, &on).is_err());
        // Auto-off is the list index, not minutes.
        assert_eq!(
            encode(Keys::V1, SettingKey::PmShutdown, &SettingValue::Int(20)).unwrap(),
            ("280", "3".into())
        );
        assert!(encode(Keys::V1, SettingKey::PmShutdown, &SettingValue::Int(7)).is_err());
        assert!(encode(Keys::V1, SettingKey::MicVolume, &SettingValue::Int(101)).is_err());
        assert!(encode(Keys::V1, SettingKey::MicMute, &SettingValue::Int(1)).is_err());
        assert_eq!(
            encode(Keys::V2, SettingKey::NoiseCancelling, &on)
                .unwrap()
                .0,
            "750"
        );
    }

    #[test]
    fn decode_reads_resources() {
        let kvp: BTreeMap<String, String> = [
            ("600", "0"),
            ("610", "55"),
            ("620", "1"),
            ("630", "30"),
            ("280", "4"),
            ("700", "x"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut s = SettingsMap::new();
        decode(Keys::V1, &kvp, &mut s);
        assert_eq!(s["mic_mute"], SettingValue::Bool(false));
        assert_eq!(s["mic_volume"], SettingValue::Int(55));
        assert_eq!(s["mic_side_tone"], SettingValue::Bool(true));
        assert_eq!(s["mic_side_tone_volume"], SettingValue::Int(30));
        assert_eq!(s["pm_shutdown"], SettingValue::Int(30));
        assert!(!s.contains_key("mic_noise_filter"));
        let mut s = SettingsMap::new();
        decode(Keys::V2, &kvp, &mut s);
        // 620 is a level in CoAP v2.
        assert_eq!(s["mic_side_tone_volume"], SettingValue::Int(1));
    }

    #[test]
    fn controls_are_gated_and_valid() {
        for keys in [Keys::V1, Keys::V2] {
            let c = controls(keys);
            assert!(c.iter().all(|m| m.persistent && m.blocked.is_none()));
            let auto = c.iter().find(|m| m.key == SettingKey::PmShutdown).unwrap();
            assert!(auto.validate(&SettingValue::Int(10)).is_ok());
            assert!(auto.validate(&SettingValue::Int(11)).is_err());
        }
        assert_eq!(controls(Keys::V1).len(), 6);
    }
}
