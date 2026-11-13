use hako_core::{
    AppConfig, AppSettings, AuxEffect, AuxEffectChain, AuxEffectKind, DeviceId, EqBand, EqBandKind,
    IpcRequest, IpcResponse, Method, SetParams, SettingKey, SettingValue,
};

#[test]
fn set_params_roundtrip() {
    let p = SetParams {
        device_id: DeviceId {
            vendor_id: 0x1038,
            product_id: 0x12e5,
            instance: "test".into(),
        },
        key: SettingKey::MicVolume,
        value: SettingValue::Int(8),
    };
    let v = serde_json::to_value(&p).unwrap();
    let back: SetParams = serde_json::from_value(v).unwrap();
    assert_eq!(back.key, SettingKey::MicVolume);
}

#[test]
fn request_response_json() {
    let req = IpcRequest {
        id: serde_json::json!(1),
        method: Method::Ping,
        params: serde_json::Value::Null,
    };
    let s = serde_json::to_string(&req).unwrap();
    let r: IpcRequest = serde_json::from_str(&s).unwrap();
    assert_eq!(r.method, Method::Ping);

    let resp = IpcResponse::ok(serde_json::json!(1), serde_json::json!({"pong": true}));
    let s = serde_json::to_string(&resp).unwrap();
    assert!(s.contains("\"ok\":true"));
}

fn sample_chain() -> AuxEffectChain {
    let fx = |id: &str, kind| AuxEffect {
        id: id.into(),
        enabled: true,
        kind,
    };
    AuxEffectChain {
        effects: vec![
            fx("gate", AuxEffectKind::default_gate()),
            fx("eq", AuxEffectKind::default_equalizer()),
            fx("compressor", AuxEffectKind::default_compressor()),
        ],
    }
}

fn gate(threshold: f32) -> AuxEffectKind {
    let mut k = AuxEffectKind::default_gate();
    if let AuxEffectKind::NoiseGate { threshold_db, .. } = &mut k {
        *threshold_db = threshold;
    }
    k
}

fn comp(threshold: f32, r: f32) -> AuxEffectKind {
    let mut k = AuxEffectKind::default_compressor();
    if let AuxEffectKind::Compressor {
        threshold_db,
        ratio,
        ..
    } = &mut k
    {
        *threshold_db = threshold;
        *ratio = r;
    }
    k
}

fn eq_band(freq: f32, gain: f32, q: f32) -> AuxEffectKind {
    AuxEffectKind::Equalizer {
        bands: vec![EqBand {
            kind: EqBandKind::Peaking,
            freq_hz: freq,
            gain_db: gain,
            q,
        }],
    }
}

#[test]
fn aux_effect_chain_json_roundtrip_preserves_order_and_kinds() {
    let chain = sample_chain();
    let json = serde_json::to_string(&chain).unwrap();
    let back: AuxEffectChain = serde_json::from_str(&json).unwrap();

    assert_eq!(back, chain);
    assert!(json.contains(r#""type":"noise_gate""#));
    assert!(json.contains(r#""type":"equalizer""#));
    assert!(json.contains(r#""type":"compressor""#));
}

#[test]
fn aux_effect_chain_toml_roundtrip() {
    let mut config = AppConfig::default();
    config.aux.effects = sample_chain();
    let toml = toml::to_string(&config).unwrap();
    let back: AppConfig = toml::from_str(&toml).unwrap();

    assert_eq!(back.aux.effects, config.aux.effects);
}

#[test]
fn aux_effect_chain_default_is_empty_and_valid() {
    let chain = AuxEffectChain::default();
    assert!(chain.effects.is_empty());
    assert!(chain.validate().is_ok());
    assert!(sample_chain().validate().is_ok());
}

#[test]
fn v1_effects_load_with_parameter_defaults() {
    let config: AppConfig = toml::from_str(
        "version = 1\n[[aux.effects.effects]]\nid = \"gate\"\nenabled = true\n[aux.effects.effects.kind]\ntype = \"noise_gate\"\nthreshold_db = -40.0\n",
    )
    .unwrap();
    assert_eq!(config.aux.effects.effects[0].kind, gate(-40.0));
}

#[test]
fn aux_effect_chain_rejects_invalid_ids_duplicates_and_length() {
    let mut chain = sample_chain();
    chain.effects[0].id.clear();
    assert!(chain.validate().is_err());

    chain.effects[0].id = "not safe".into();
    assert!(chain.validate().is_err());

    chain.effects[0].id = "compressor".into();
    assert!(chain.validate().is_err());

    chain.effects = (0..17)
        .map(|n| AuxEffect {
            id: format!("effect-{n}"),
            enabled: true,
            kind: gate(-30.0),
        })
        .collect();
    assert!(chain.validate().is_err());
}

#[test]
fn aux_effect_chain_rejects_out_of_range_and_nonfinite_parameters() {
    let invalid_kinds = [
        gate(-60.1),
        gate(0.1),
        gate(f32::NAN),
        comp(-60.1, 3.0),
        comp(0.1, 3.0),
        comp(-20.0, 0.9),
        comp(-20.0, 20.1),
        comp(f32::INFINITY, 3.0),
        comp(-20.0, f32::NEG_INFINITY),
        eq_band(10.0, 0.0, 1.0),
        eq_band(1000.0, 30.0, 1.0),
        eq_band(1000.0, 0.0, 0.0),
        eq_band(f32::NAN, 0.0, 1.0),
        AuxEffectKind::Equalizer { bands: vec![] },
    ];

    for kind in invalid_kinds {
        let chain = AuxEffectChain {
            effects: vec![AuxEffect {
                id: "effect".into(),
                enabled: true,
                kind,
            }],
        };
        assert!(chain.validate().is_err());
    }
}

#[test]
fn missing_fields_use_semantic_defaults() {
    let config: AppConfig = toml::from_str("version = 1").unwrap();
    assert!(config.chatmix_set_default_sink);
    assert!(config.check_updates);
    assert_eq!(config.aux.effects, AuxEffectChain::default());

    let settings: AppSettings = serde_json::from_value(serde_json::json!({
        "log_level": "info",
        "chatmix_set_default_sink": true,
        "aux_set_default_sink": false,
        "aux_auto_enable": false
    }))
    .unwrap();
    assert_eq!(settings.aux_effects, AuxEffectChain::default());
    assert!(settings.check_updates);
}

#[test]
fn profile_names_cannot_escape_the_profiles_dir() {
    use hako_core::validate_profile_name;
    assert!(validate_profile_name("Gaming 2").is_ok());
    for bad in ["../x", "a/b", "", "  ", ".hidden", " Gaming", "Gaming "] {
        assert!(validate_profile_name(bad).is_err(), "{bad:?}");
    }
}
