//! The wire shape, pinned by hand-written JSON: a renamed field or variant
//! fails here even though a same-type round trip would still pass.

use hako_core::{
    CaptureRole, ControlKind, DeviceId, DeviceKind, EvdevKey, HubState, IpcRequest, MacroRepeat,
    MacroStep, Method, RemapMode, SetManyParams, SetParams, SettingKey, SettingValue,
};

fn json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}

fn arctis() -> DeviceId {
    DeviceId {
        vendor_id: 0x1038,
        product_id: 0x12e5,
        instance: "usb-1".into(),
    }
}

const SET: &str = r#"{
    "device_id": {"vendor_id": 4152, "product_id": 4837, "instance": "usb-1"},
    "key": "mic_volume",
    "value": {"t": "Int", "v": 8}
}"#;

#[test]
fn set_params_wire_shape() {
    let p: SetParams = serde_json::from_value(json(SET)).unwrap();
    assert_eq!(p.device_id, arctis());
    assert_eq!(p.key, SettingKey::MicVolume);
    assert_eq!(p.value, SettingValue::Int(8));
    assert_eq!(serde_json::to_value(&p).unwrap(), json(SET));
}

const SET_MANY: &str = r#"{
    "device_id": {"vendor_id": 5426, "product_id": 619, "instance": "kbd"},
    "values": [
        ["color_r", {"t": "Int", "v": 255}],
        ["effect", {"t": "Text", "v": "wave"}],
        ["angle_snapping", {"t": "Bool", "v": true}],
        ["speed", {"t": "Float", "v": 0.5}]
    ]
}"#;

#[test]
fn set_many_params_wire_shape() {
    let p: SetManyParams = serde_json::from_value(json(SET_MANY)).unwrap();
    assert_eq!(
        p.device_id,
        DeviceId {
            vendor_id: 0x1532,
            product_id: 0x026b,
            instance: "kbd".into(),
        }
    );
    assert_eq!(
        p.values,
        [
            (SettingKey::ColorR, SettingValue::Int(255)),
            (SettingKey::Effect, SettingValue::Text("wave".into())),
            (SettingKey::AngleSnapping, SettingValue::Bool(true)),
            (SettingKey::Speed, SettingValue::Float(0.5)),
        ]
    );
    assert_eq!(serde_json::to_value(&p).unwrap(), json(SET_MANY));
}

#[test]
fn request_envelope_wire_shape() {
    let text = format!(r#"{{"id": 42, "method": "Set", "params": {SET}}}"#);
    let req: IpcRequest = serde_json::from_str(&text).unwrap();
    assert_eq!(req.id, serde_json::json!(42));
    assert_eq!(req.method, Method::Set);
    let p: SetParams = serde_json::from_value(req.params.clone()).unwrap();
    assert_eq!(p.key, SettingKey::MicVolume);
    assert_eq!(serde_json::to_value(&req).unwrap(), json(&text));

    // `params` may be left out (`Ping`).
    let req: IpcRequest = serde_json::from_str(r#"{"id": "a", "method": "Ping"}"#).unwrap();
    assert_eq!(req.method, Method::Ping);
    assert!(req.params.is_null());
}

const HUB: &str = r#"{
    "health": {"version": "0.2.9", "uptime_secs": 12, "adapter_count": 1, "last_error": "e"},
    "last_profile": "Gaming",
    "last_profile_modified": true,
    "profiles": ["Gaming"],
    "audio": {
        "chatmix_active": true,
        "aux_active": false,
        "aux_processing_active": false,
        "last_error": null,
        "default_sink": "Hako_Game"
    },
    "devices": [{
        "id": {"vendor_id": 4152, "product_id": 4837, "instance": "usb-1"},
        "kind": "headset",
        "name": "Arctis Nova Pro Wireless",
        "connected": true,
        "batteries": [{"label": "Headset", "percent": 80, "charging": true, "present": true}],
        "settings": {"mic_volume": {"t": "Int", "v": 8}},
        "controls": [{
            "key": "mic_volume",
            "label": "Mic volume",
            "kind": "slider",
            "min": 0.0,
            "max": 10.0,
            "step": 1.0,
            "read_only": false,
            "options": [],
            "unit": "%",
            "hidden": false,
            "persistent": true,
            "blocked": null,
            "warning": null
        }],
        "last_error": null
    }],
    "remaps": [{
        "device_id": {"vendor_id": 4152, "product_id": 4837, "instance": "usb-1"},
        "bindings": [{
            "source": 275,
            "target": 30,
            "mode": "macro",
            "interval_ms": 50,
            "steps": [{"key": 30, "down": true, "delay_ms": 20}],
            "repeat": "while_held"
        }],
        "capturing": "target",
        "capture_source": 275,
        "last_error": null,
        "grabbed": true,
        "recorded": [{"key": 31, "down": false, "delay_ms": 5}]
    }]
}"#;

#[test]
fn hub_state_wire_shape() {
    let st: HubState = serde_json::from_value(json(HUB)).unwrap();
    assert_eq!(st.health.version, "0.2.9");
    assert_eq!(st.health.uptime_secs, 12);
    assert_eq!(st.health.adapter_count, 1);
    assert_eq!(st.health.last_error.as_deref(), Some("e"));
    assert_eq!(st.last_profile.as_deref(), Some("Gaming"));
    assert!(st.last_profile_modified);
    assert_eq!(st.profiles, ["Gaming"]);
    assert!(st.audio.chatmix_active);
    assert_eq!(st.audio.default_sink.as_deref(), Some("Hako_Game"));

    let [d] = &st.devices[..] else {
        panic!("{:?}", st.devices)
    };
    assert_eq!(d.id, arctis());
    assert_eq!(d.kind, DeviceKind::Headset);
    assert_eq!(d.name, "Arctis Nova Pro Wireless");
    assert!(d.connected);
    assert_eq!(d.batteries[0].label, "Headset");
    assert_eq!(d.batteries[0].percent, Some(80));
    assert!(d.batteries[0].charging && d.batteries[0].present);
    assert_eq!(d.settings["mic_volume"], SettingValue::Int(8));
    let c = &d.controls[0];
    assert_eq!(c.key, SettingKey::MicVolume);
    assert_eq!(c.kind, ControlKind::Slider);
    assert_eq!((c.min, c.max, c.step), (Some(0.0), Some(10.0), Some(1.0)));
    assert_eq!(c.unit.as_deref(), Some("%"));
    assert!(c.persistent);

    let r = &st.remaps[0];
    assert_eq!(r.device_id, Some(arctis()));
    let b = &r.bindings[0];
    assert_eq!((b.source, b.target), (EvdevKey(275), EvdevKey(30)));
    assert_eq!(b.mode, RemapMode::Macro);
    assert_eq!(b.interval_ms, 50);
    assert_eq!(b.repeat, MacroRepeat::WhileHeld);
    assert_eq!(
        b.steps,
        [MacroStep {
            key: EvdevKey(30),
            down: true,
            delay_ms: 20,
        }]
    );
    assert_eq!(r.capturing, Some(CaptureRole::Target));
    assert_eq!(r.capture_source, Some(EvdevKey(275)));
    assert!(r.grabbed);
    assert_eq!(r.recorded[0].key, EvdevKey(31));

    assert_eq!(serde_json::to_value(&st).unwrap(), json(HUB));
}

/// What a newer daemon may send: an unknown device kind, remap mode and
/// capture role, and a non-finite float (serialised as `null`). Each loses
/// only itself; the rest of the state still reads.
#[test]
fn hub_state_from_a_newer_daemon_still_reads() {
    let newer = HUB
        .replace(r#""kind": "headset""#, r#""kind": "controller""#)
        .replace(r#""capturing": "target""#, r#""capturing": "gesture""#)
        .replace(
            r#""settings": {"mic_volume": {"t": "Int", "v": 8}}"#,
            r#""settings": {"mic_volume": {"t": "Int", "v": 8}, "volume": {"t": "Float", "v": null}, "tilt": {"t": "Vec2", "v": [1, 2]}}"#,
        )
        .replace(
            r#""bindings": [{"#,
            r#""bindings": [{"source": 276, "target": 30, "mode": "chord"}, {"#,
        );
    assert_ne!(newer, HUB);
    let st: HubState = serde_json::from_str(&newer).unwrap();
    let d = &st.devices[0];
    assert_eq!(d.kind, DeviceKind::Other);
    assert_eq!(d.settings.len(), 1, "{:?}", d.settings);
    assert_eq!(d.settings["mic_volume"], SettingValue::Int(8));
    assert_eq!(d.controls.len(), 1);
    let r = &st.remaps[0];
    assert_eq!(r.bindings.len(), 1, "{:?}", r.bindings);
    assert_eq!(r.bindings[0].source, EvdevKey(275));
    assert_eq!(r.capturing, None);
    assert!(r.grabbed);
}
