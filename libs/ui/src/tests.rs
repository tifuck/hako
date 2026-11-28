//! Headless render tests: every page and tab must lay out without panicking.

use super::*;
use hako_core::catalog::{SettingOption, SettingsMap};
use hako_core::device::{AudioSummary, BatteryState, DaemonHealth};
use hako_core::ipc::{AuxEffect, AuxEffectKind};
use hako_core::remap::{MacroRepeat, MacroStep, RemapMode};

fn dev(kind: DeviceKind, controls: Vec<SettingMeta>, settings: SettingsMap) -> DeviceSnapshot {
    DeviceSnapshot {
        id: DeviceId {
            vendor_id: 0x1,
            product_id: kind_order(kind) as u16 + 1,
            instance: format!("{kind:?}"),
        },
        kind,
        name: format!("Test {kind:?}"),
        connected: true,
        batteries: vec![BatteryState {
            label: "Battery".into(),
            percent: Some(12),
            charging: true,
            present: true,
        }],
        settings,
        controls,
        last_error: Some("example warning".into()),
    }
}

fn map(pairs: &[(SettingKey, SettingValue)]) -> SettingsMap {
    pairs
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.clone()))
        .collect()
}

fn sample_state() -> HubState {
    use SettingKey::*;
    let keyboard = dev(
        DeviceKind::Keyboard,
        vec![
            SettingMeta::dropdown(
                Effect,
                "Effect",
                [
                    "off",
                    "static",
                    "spectrum",
                    "wave",
                    "reactive",
                    "breathing",
                    "starlight",
                ]
                .iter()
                .map(|e| SettingOption::text(e, e))
                .collect(),
            ),
            SettingMeta::slider(Brightness, "Brightness", 0.0, 100.0, 1.0).unit("%"),
            SettingMeta::dropdown(
                Direction,
                "Direction",
                vec![
                    SettingOption::text("left", "Left"),
                    SettingOption::text("right", "Right"),
                ],
            ),
            SettingMeta::color(ColorR, "Red"),
            SettingMeta::color(ColorG, "Green"),
            SettingMeta::color(ColorB, "Blue"),
        ],
        map(&[
            (Brightness, SettingValue::Int(75)),
            (Effect, SettingValue::Text("wave".into())),
            (Direction, SettingValue::Text("left".into())),
            (ColorR, SettingValue::Int(255)),
            (ColorG, SettingValue::Int(64)),
            (ColorB, SettingValue::Int(0)),
        ]),
    );
    let mouse = dev(
        DeviceKind::Mouse,
        vec![
            SettingMeta::slider(Dpi, "DPI", 100.0, 32000.0, 50.0).unit("dpi"),
            SettingMeta::dropdown(
                ReportRate,
                "Report rate",
                vec![
                    SettingOption::int(1000, "1000 Hz"),
                    SettingOption::int(8000, "8000 Hz"),
                ],
            ),
            SettingMeta::dropdown(
                Effect,
                "Effect",
                vec![
                    SettingOption::text("static", "Static"),
                    SettingOption::text("cycle", "Cycle"),
                ],
            ),
            SettingMeta::color(ColorR, "Red"),
            SettingMeta::color(ColorG, "Green"),
            SettingMeta::color(ColorB, "Blue"),
        ],
        map(&[
            (Dpi, SettingValue::Int(1600)),
            (ReportRate, SettingValue::Int(1000)),
            (Effect, SettingValue::Text("static".into())),
            (ColorR, SettingValue::Int(0)),
            (ColorG, SettingValue::Int(200)),
            (ColorB, SettingValue::Int(80)),
        ]),
    );
    // Line-in controls as a pre-mixer daemon declared them: never shown.
    let headset = dev(
        DeviceKind::Headset,
        vec![
            SettingMeta::readonly(ChatMixGame, "Game", 0.0, 1.0),
            SettingMeta::readonly(ChatMixChat, "Chat", 0.0, 1.0),
            SettingMeta::toggle(AuxEnabled, "Monitor line-in"),
            SettingMeta::slider(AuxCapturePercent, "Capture level", 0.0, 100.0, 1.0).unit("%"),
            SettingMeta::dropdown(
                PmShutdown,
                "Auto-off",
                vec![
                    SettingOption::int(0, "Never"),
                    SettingOption::int(6, "60 min"),
                ],
            ),
            SettingMeta::toggle(WirelessModeRange, "Range mode"),
            SettingMeta::slider(MicVolume, "Mic level", 1.0, 10.0, 1.0),
        ],
        map(&[
            (ChatMixGame, SettingValue::Float(0.5)),
            (ChatMixChat, SettingValue::Float(1.0)),
            (AuxEnabled, SettingValue::Bool(true)),
            (AuxCapturePercent, SettingValue::Int(66)),
            (PmShutdown, SettingValue::Int(6)),
            (WirelessModeRange, SettingValue::Bool(false)),
            (MicVolume, SettingValue::Int(8)),
        ]),
    );
    let other = dev(
        DeviceKind::Other,
        vec![SettingMeta::slider(Brightness, "Generic", 0.0, 10.0, 1.0)],
        map(&[(Brightness, SettingValue::Int(5))]),
    );
    let mouse_id = mouse.id.clone();
    HubState {
        health: DaemonHealth {
            version: "9.9.9".into(),
            uptime_secs: 1,
            adapter_count: 4,
            last_error: None,
        },
        last_profile: Some("Default".into()),
        last_profile_modified: false,
        profiles: vec!["Default".into()],
        audio: AudioSummary {
            chatmix_active: true,
            ..Default::default()
        },
        devices: vec![keyboard, mouse, headset, other],
        remaps: vec![DeviceRemaps {
            device_id: Some(mouse_id),
            bindings: vec![RemapBinding {
                mode: RemapMode::Macro,
                steps: vec![
                    MacroStep {
                        key: EvdevKey(30),
                        down: true,
                        delay_ms: 0,
                    },
                    MacroStep {
                        key: EvdevKey(30),
                        down: false,
                        delay_ms: 25,
                    },
                ],
                repeat: MacroRepeat::Toggle,
                ..RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(0))
            }],
            capturing: Some(CaptureRole::Record),
            ..Default::default()
        }],
    }
}

fn settings_with_chain() -> AppSettings {
    let mut s = AppSettings::default();
    for (id, kind) in [
        ("gate-1", AuxEffectKind::default_gate()),
        ("eq-1", AuxEffectKind::default_equalizer()),
        ("compressor-1", AuxEffectKind::default_compressor()),
    ] {
        s.aux_effects.effects.push(AuxEffect {
            id: id.into(),
            enabled: true,
            kind,
        });
    }
    s
}

fn input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(1200.0, 800.0),
        )),
        ..Default::default()
    }
}

/// Render every device × tab, with stage editors expanded, for a few frames.
#[test]
fn renders_every_device_tab_without_panicking() {
    let ctx = egui::Context::default();
    let state = sample_state();
    for d in &state.devices {
        for tab in tabs_for(d) {
            let mut app = HakoAppUi {
                state: Some(state.clone()),
                selected: Some(d.id.clone()),
                daemon_error: None,
                settings: Some(settings_with_chain()),
                ..Default::default()
            };
            ctx.data_mut(|m| m.insert_temp(egui::Id::new("tab"), tab));
            let mut out = Vec::new();
            for _ in 0..3 {
                let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
            }
            // Pure rendering must not emit device commands.
            assert!(
                out.iter().all(|c| !matches!(
                    c,
                    UiCommand::Set { .. } | UiCommand::SetMany { .. } | UiCommand::SetRemap { .. }
                )),
                "{:?} tab {tab:?} emitted {out:?}",
                d.kind
            );
        }
    }
}

/// Onboard-memory controls: disabled until allowed, blocked always, the
/// Device tab renders, and the opt-in only sends after confirmation.
#[test]
fn onboard_memory_gate_renders_and_needs_confirmation() {
    use SettingKey::*;
    let mut d = dev(
        DeviceKind::Mouse,
        vec![
            SettingMeta::slider(Dpi, "DPI", 100.0, 26000.0, 50.0).persistent(),
            SettingMeta::slider(DpiStage1, "Stage 1", 100.0, 26000.0, 50.0)
                .persistent()
                .blocked("layout unconfirmed"),
            SettingMeta::toggle(OnboardWrites, "Allow saving to device memory")
                .warning("Writes go to the device."),
            SettingMeta::dropdown(EqPreset, "Equalizer (saved on device)", vec![])
                .persistent()
                .blocked("single source"),
        ],
        map(&[
            (Dpi, SettingValue::Int(800)),
            (OnboardWrites, SettingValue::Bool(false)),
        ]),
    );
    let dpi = d.controls[0].clone();
    let blocked = d.controls[1].clone();
    assert!(!controls::writable(&d, &dpi));
    assert!(controls::note(&d, &dpi).unwrap().contains("Device tab"));
    assert!(!controls::writable(&d, &blocked));
    d.settings
        .insert(OnboardWrites.as_str().into(), SettingValue::Bool(true));
    assert!(controls::writable(&d, &dpi));
    assert!(!controls::writable(&d, &blocked));
    d.settings
        .insert(OnboardWrites.as_str().into(), SettingValue::Bool(false));

    let mut state = sample_state();
    state.devices = vec![d.clone()];
    let ctx = egui::Context::default();
    assert_eq!(tabs_for(&d).last(), Some(&Tab::Device));
    for tab in tabs_for(&d) {
        let mut app = HakoAppUi {
            state: Some(state.clone()),
            selected: Some(d.id.clone()),
            ..Default::default()
        };
        ctx.data_mut(|m| m.insert_temp(egui::Id::new("tab"), tab));
        // The confirmation dialog is open: rendering it must not switch anything on.
        ctx.data_mut(|m| {
            m.insert_temp(
                egui::Id::new(("hako_confirm", d.id.key(), OnboardWrites.as_str())),
                true,
            )
        });
        let mut out = Vec::new();
        for _ in 0..3 {
            let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
        }
        assert!(
            out.iter()
                .all(|c| !matches!(c, UiCommand::Set { .. } | UiCommand::SetMany { .. })),
            "tab {tab:?} emitted {out:?}"
        );
    }
}

/// A dropdown with a warning (e.g. a lift-off change that resets surface
/// calibration) sends nothing while its confirmation is open.
#[test]
fn a_warned_choice_waits_for_confirmation() {
    use hako_core::catalog::SettingOption;
    let opt = |v: i64, label: &str| SettingOption {
        value: SettingValue::Int(v),
        label: label.into(),
    };
    let meta = SettingMeta::dropdown(
        SettingKey::LiftOffDistance,
        "Lift-off distance",
        vec![opt(0, "Low"), opt(1, "High")],
    )
    .warning("Changing it resets the surface calibration.");
    let d = dev(
        DeviceKind::Mouse,
        vec![meta.clone()],
        map(&[(SettingKey::LiftOffDistance, SettingValue::Int(0))]),
    );
    let ctx = egui::Context::default();
    let pending = egui::Id::new(("hako_confirm", d.id.key(), meta.key.as_str()));
    ctx.data_mut(|m| m.insert_temp(pending, SettingValue::Int(1)));
    let mut out = Vec::new();
    let frame = |raw: egui::RawInput, out: &mut Vec<UiCommand>| {
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| controls::choice(ui, &d, &meta, out));
        });
    };
    for _ in 0..3 {
        frame(input(), &mut out);
    }
    assert!(out.is_empty(), "sent before confirming: {out:?}");
    assert_eq!(
        ctx.data(|m| m.get_temp::<SettingValue>(pending)),
        Some(SettingValue::Int(1))
    );
    // Escape dismisses the question: the pick is dropped, nothing sent.
    let mut esc = input();
    esc.events.push(egui::Event::Key {
        key: egui::Key::Escape,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Default::default(),
    });
    frame(esc, &mut out);
    frame(input(), &mut out);
    assert!(out.is_empty(), "sent after dismissing: {out:?}");
    assert_eq!(ctx.data(|m| m.get_temp::<SettingValue>(pending)), None);
}

/// A confirmed warning sends the option that was picked, also when the daemon has
/// listed the options in another order since; one no longer offered drops the question.
#[test]
fn a_warned_choice_confirms_the_option_picked() {
    use egui_kittest::kittest::Queryable;
    let opt = |v: i64, label: &str| SettingOption {
        value: SettingValue::Int(v),
        label: label.into(),
    };
    let d = dev(
        DeviceKind::Mouse,
        Vec::new(),
        map(&[(SettingKey::LiftOffDistance, SettingValue::Int(0))]),
    );
    let pending = egui::Id::new((
        "hako_confirm",
        d.id.key(),
        SettingKey::LiftOffDistance.as_str(),
    ));
    let mut h = egui_kittest::Harness::builder()
        .with_size([600.0, 400.0])
        .build_ui_state(
            |ui, (options, out): &mut (Vec<SettingOption>, Vec<UiCommand>)| {
                let meta = SettingMeta::dropdown(
                    SettingKey::LiftOffDistance,
                    "Lift-off distance",
                    options.clone(),
                )
                .warning("Changing it resets the surface calibration.");
                controls::choice(ui, &d, &meta, out);
            },
            (vec![opt(0, "Low"), opt(1, "High")], Vec::new()),
        );
    let confirm = "I understand, change it";
    h.ctx
        .data_mut(|m| m.insert_temp(pending, SettingValue::Int(1)));
    h.state_mut().0 = vec![opt(1, "High"), opt(0, "Low")];
    h.run_steps(2);
    h.get_by_label(confirm).click();
    h.run_steps(2);
    match std::mem::take(&mut h.state_mut().1).as_slice() {
        [UiCommand::Set { value, .. }] => assert_eq!(*value, SettingValue::Int(1)),
        other => panic!("{other:?}"),
    }

    h.ctx
        .data_mut(|m| m.insert_temp(pending, SettingValue::Int(1)));
    h.state_mut().0 = vec![opt(0, "Low")];
    h.run_steps(2);
    assert!(
        h.query_by_label(confirm).is_none(),
        "asked about a gone option"
    );
    assert!(h.state().1.is_empty(), "{:?}", h.state().1);
    assert_eq!(h.ctx.data(|m| m.get_temp::<SettingValue>(pending)), None);
}

#[test]
fn tabs_share_one_order_and_end_with_device() {
    use Tab::*;
    let st = sample_state();
    let tabs = |k| tabs_for(st.devices.iter().find(|d| d.kind == k).unwrap());
    assert_eq!(tabs(DeviceKind::Keyboard), [Bindings, Lighting, Device]);
    assert_eq!(
        tabs(DeviceKind::Mouse),
        [Bindings, Lighting, Performance, Device]
    );
    // Microphone folds into Audio, power into Device.
    assert_eq!(tabs(DeviceKind::Headset), [Audio, Device]);
}

/// Line-in is a mixer input: the headset page has no Line-in tab, with the
/// driver's controls or an older daemon's line-in ones.
#[test]
fn the_headset_page_has_no_line_in_tab() {
    use SettingKey::*;
    let st = sample_state();
    let old = st
        .devices
        .iter()
        .find(|d| d.kind == DeviceKind::Headset)
        .unwrap();
    assert!(old.controls.iter().any(|c| c.key == AuxEnabled));
    let mut now = old.clone();
    now.controls
        .retain(|c| c.key.group() != SettingGroup::LineIn);
    for d in [old, &now] {
        let labels: Vec<&str> = tabs_for(d).iter().map(|t| t.label()).collect();
        assert_eq!(labels, ["Audio", "Device"]);
    }
    // Only line-in controls (nothing else to show): no page for them either.
    let only = dev(
        DeviceKind::Headset,
        vec![SettingMeta::toggle(AuxEnabled, "Monitor line-in")],
        map(&[(AuxEnabled, SettingValue::Bool(true))]),
    );
    assert_eq!(tabs_for(&only), [Tab::Device]);
}

/// Drafts hold while commands are in flight, then give way to the daemon's
/// value even when it never matched (refused or clamped write).
#[test]
fn drafts_fall_back_to_the_daemon_value_once_answered() {
    let st = sample_state();
    let kb = st.devices[0].clone();
    let slider = egui::Id::new(("hako_draft", kb.id.key(), SettingKey::Brightness.as_str()));
    let color = egui::Id::new(("color_draft", kb.id.key()));
    let sent = egui::Id::new(("color_sent", kb.id.key()));
    let ctx = egui::Context::default();
    ctx.data_mut(|m| {
        m.insert_temp(egui::Id::new("tab"), Tab::Lighting);
        m.insert_temp(slider, 20.0_f32);
        m.insert_temp(color, [1_u8, 2, 3]);
        m.insert_temp(sent, [1_u8, 2, 3]);
    });
    let mut app = HakoAppUi {
        state: Some(st),
        selected: Some(kb.id.clone()),
        daemon_error: None,
        pending: true,
        ..Default::default()
    };
    let drafts = || {
        ctx.data(|m| {
            (
                m.get_temp::<f32>(slider),
                m.get_temp::<[u8; 3]>(color),
                m.get_temp::<[u8; 3]>(sent),
            )
        })
    };
    let mut out = Vec::new();
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    assert_eq!(drafts(), (Some(20.0), Some([1, 2, 3]), Some([1, 2, 3])));
    app.pending = false;
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    assert_eq!(drafts(), (None, None, None));
    assert!(out.is_empty(), "{out:?}");
}

/// A picked colour goes out as one command carrying all three channels,
/// and its draft holds until that command is answered.
#[test]
fn a_picked_color_is_one_command() {
    let st = sample_state();
    let kb = st.devices[0].clone();
    let color = egui::Id::new(("color_draft", kb.id.key()));
    let ctx = egui::Context::default();
    ctx.data_mut(|m| {
        m.insert_temp(egui::Id::new("tab"), Tab::Lighting);
        m.insert_temp(color, [1_u8, 2, 3]);
    });
    let mut app = HakoAppUi {
        state: Some(st),
        selected: Some(kb.id.clone()),
        ..Default::default()
    };
    let mut out = Vec::new();
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    let int = SettingValue::Int;
    match &out[..] {
        [UiCommand::SetMany { device_id, values }] => {
            assert_eq!(device_id, &kb.id);
            assert_eq!(
                values,
                &[
                    (SettingKey::ColorR, int(1)),
                    (SettingKey::ColorG, int(2)),
                    (SettingKey::ColorB, int(3)),
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(ctx.data(|m| m.get_temp::<[u8; 3]>(color)), Some([1, 2, 3]));
}

/// The open page is remembered by what it is, not its position.
#[test]
fn selected_tab_survives_pages_coming_and_going() {
    let mut st = sample_state();
    let id = st.devices[1].id.clone();
    let ctx = egui::Context::default();
    ctx.data_mut(|m| m.insert_temp(egui::Id::new("tab"), Tab::Lighting));
    let shown = |st: &HubState| {
        let mut app = HakoAppUi {
            state: Some(st.clone()),
            selected: Some(id.clone()),
            daemon_error: None,
            ..Default::default()
        };
        let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut Vec::new()));
        ctx.data(|m| m.get_temp::<(egui::Id, f64)>(egui::Id::new("page_transition")))
            .unwrap()
            .0
    };
    let page = |t: Tab| egui::Id::new((id.key(), t));
    assert_eq!(shown(&st), page(Tab::Lighting));
    // Performance disappears: Lighting moves from index 2 to 1.
    st.devices[1]
        .controls
        .retain(|m| !matches!(m.key, SettingKey::Dpi | SettingKey::ReportRate));
    assert_eq!(shown(&st), page(Tab::Lighting));
    // Lighting disappears: the first page takes over.
    st.devices[1]
        .controls
        .retain(|m| m.key.group() != SettingGroup::Lighting);
    assert_eq!(shown(&st), page(Tab::Bindings));
}

/// Switching devices keeps the open page when the next device has it, and a
/// device without it does not forget it.
#[test]
fn selected_tab_carries_over_between_devices() {
    let st = sample_state();
    let ctx = egui::Context::default();
    ctx.data_mut(|m| m.insert_temp(egui::Id::new("tab"), Tab::Lighting));
    let shown = |kind: DeviceKind| {
        let d = st.devices.iter().find(|d| d.kind == kind).unwrap();
        let mut app = HakoAppUi {
            state: Some(st.clone()),
            selected: Some(d.id.clone()),
            daemon_error: None,
            ..Default::default()
        };
        let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut Vec::new()));
        let page = ctx
            .data(|m| m.get_temp::<(egui::Id, f64)>(egui::Id::new("page_transition")))
            .unwrap()
            .0;
        tabs_for(d)
            .into_iter()
            .find(|&t| egui::Id::new((d.id.key(), t)) == page)
            .unwrap()
    };
    assert_eq!(shown(DeviceKind::Keyboard), Tab::Lighting);
    assert_eq!(shown(DeviceKind::Mouse), Tab::Lighting);
    assert_eq!(shown(DeviceKind::Headset), Tab::Audio);
    assert_eq!(shown(DeviceKind::Keyboard), Tab::Lighting);
}

#[test]
fn settings_page_renders_with_and_without_settings() {
    let ctx = egui::Context::default();
    let mut app = HakoAppUi {
        state: Some(sample_state()),
        daemon_error: None,
        show_settings: true,
        ..Default::default()
    };
    let mut out = Vec::new();
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    app.settings = Some(AppSettings::default());
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    assert!(out.iter().all(|c| !matches!(c, UiCommand::SetConfig(_))));
}

#[test]
fn settings_page_says_why_there_are_no_settings() {
    let why = "Settings unavailable: parse config.toml: boom";
    let mut h = drive(HakoAppUi {
        state: Some(sample_state()),
        daemon_error: None,
        show_settings: true,
        settings_error: Some("parse config.toml: boom".into()),
        ..Default::default()
    });
    assert!(shown(&h, why));
    h.state_mut().0.settings = Some(AppSettings::default());
    h.run_steps(4);
    assert!(!shown(&h, why));
}

#[test]
fn latency_labels_cover_the_presets() {
    let values: Vec<_> = AUDIO_LATENCY_LABELS.iter().map(|(v, _)| *v).collect();
    let presets: Vec<_> = hako_core::AUDIO_LATENCIES.iter().map(|(v, _)| *v).collect();
    assert_eq!(values, presets);
}

#[test]
fn offline_and_disconnected_states_render() {
    let ctx = egui::Context::default();
    let mut app = HakoAppUi::default();
    let mut out = Vec::new();
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    let mut st = sample_state();
    for d in &mut st.devices {
        d.connected = false;
        d.controls.clear();
    }
    app.state = Some(st);
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
}

#[test]
fn effect_ids_are_unique_and_reuse_gaps() {
    let mut chain = settings_with_chain().aux_effects;
    assert_eq!(aux::next_id(&chain, "gate"), "gate-2");
    assert_eq!(aux::next_id(&chain, "limiter"), "limiter-1");
    chain.effects.remove(0);
    assert_eq!(aux::next_id(&chain, "gate"), "gate-1");
}

#[test]
fn themes_have_unique_ids_and_unknown_ids_fall_back() {
    let mut ids: Vec<&str> = theme::THEMES.iter().map(|t| t.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), theme::THEMES.len());
    assert_eq!(theme::THEMES[0].id, "onyx", "default theme");
    theme::set("no-such-theme");
    assert_eq!(theme::current().id, "onyx");
}

#[test]
fn profile_names_are_filesystem_safe() {
    assert!(valid_profile_name("Gaming 2"));
    assert!(!valid_profile_name("../etc"));
    assert!(!valid_profile_name("  "));
}

#[test]
fn a_changed_profile_shows_as_modified() {
    let mut st = sample_state();
    assert_eq!(profile_label(Some(&st)), "Default");
    st.last_profile_modified = true;
    assert_eq!(profile_label(Some(&st)), "Default (modified)");
    st.last_profile = None;
    assert_eq!(profile_label(Some(&st)), "No profile");
    assert_eq!(profile_label(None), "No profile");
}

// ── Key bindings ─────────────────────────────────────────────────

fn steps(keys: &[u16]) -> Vec<MacroStep> {
    keys.iter()
        .flat_map(|&k| {
            [(true, 40), (false, 35)].map(|(down, delay_ms)| MacroStep {
                key: EvdevKey(k),
                down,
                delay_ms,
            })
        })
        .collect()
}

/// One binding of every mode, on keys / buttons the illustrations draw.
fn sample_bindings(kind: DeviceKind) -> Vec<RemapBinding> {
    let b = |source: u16, target: u16, mode| RemapBinding {
        mode,
        ..RemapBinding::tap(EvdevKey(source), EvdevKey(target))
    };
    let mac = |source: u16| RemapBinding {
        steps: {
            // Ctrl held across a chord, so the timeline shows overlapping bars.
            let mut s = steps(&[46, 47, 28]);
            s.insert(
                0,
                MacroStep {
                    key: EvdevKey(29),
                    down: true,
                    delay_ms: 0,
                },
            );
            s.push(MacroStep {
                key: EvdevKey(29),
                down: false,
                delay_ms: 20,
            });
            s
        },
        repeat: MacroRepeat::WhileHeld,
        ..b(source, 0, RemapMode::Macro)
    };
    if kind == DeviceKind::Mouse {
        vec![
            b(0x112, 164, RemapMode::Tap),
            b(0x113, 0x110, RemapMode::Turbo),
            mac(0x114),
            b(0x117, 0, RemapMode::Disabled),
        ]
    } else {
        vec![
            b(58, 29, RemapMode::Tap),
            b(57, 57, RemapMode::Turbo),
            b(67, 30, RemapMode::Toggle),
            mac(34),
            b(100, 0, RemapMode::Disabled),
            b(82, 164, RemapMode::Tap),
        ]
    }
}

/// `sample_state` with one device renamed and given `remaps`.
fn bind_state(kind: DeviceKind, name: &str, remaps: DeviceRemaps) -> (HubState, DeviceId) {
    let mut st = sample_state();
    let d = st.devices.iter_mut().find(|d| d.kind == kind).unwrap();
    d.name = name.into();
    let id = d.id.clone();
    st.remaps = vec![DeviceRemaps {
        device_id: Some(id.clone()),
        ..remaps
    }];
    (st, id)
}

/// Open `id` on its bindings tab with `selected` picked (0 = nothing).
fn bind_app(ctx: &egui::Context, st: &HubState, id: &DeviceId, selected: u16) -> HakoAppUi {
    ctx.data_mut(|m| {
        m.insert_temp(egui::Id::new("tab"), Tab::Bindings);
        m.insert_temp(egui::Id::new(("bind_selected", id.key())), selected);
    });
    HakoAppUi {
        state: Some(st.clone()),
        selected: Some(id.clone()),
        daemon_error: None,
        ..Default::default()
    }
}

#[test]
fn binding_pages_render_every_layout_selection_and_capture_state() {
    let narrow = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(900.0, 600.0),
        )),
        ..Default::default()
    };
    let keyboards = ["K70 RGB", "Huntsman V2 TKL", "Origins 65", "Huntsman Mini"];
    for (kind, name) in keyboards
        .map(|n| (DeviceKind::Keyboard, n))
        .into_iter()
        .chain([(DeviceKind::Mouse, "PRO 2")])
    {
        let bindings = sample_bindings(kind);
        // Nothing selected, each bound source, an unbound one, and one not drawn.
        let picks = [0, 44, 0x118]
            .into_iter()
            .chain(bindings.iter().map(|b| b.source.0));
        for selected in picks {
            for capturing in [
                None,
                Some(CaptureRole::Source),
                Some(CaptureRole::Target),
                Some(CaptureRole::Record),
            ] {
                let (st, id) = bind_state(
                    kind,
                    name,
                    DeviceRemaps {
                        bindings: bindings.clone(),
                        capturing,
                        capture_source: (capturing == Some(CaptureRole::Target))
                            .then_some(EvdevKey(selected))
                            .filter(|k| k.0 != 0),
                        recorded: steps(&[30]),
                        last_error: Some("example error".into()),
                        ..Default::default()
                    },
                );
                let ctx = egui::Context::default();
                let mut app = bind_app(&ctx, &st, &id, selected);
                let mut out = Vec::new();
                for input in [input(), input(), narrow.clone()] {
                    let _ = ctx.run(input, |ctx| app.ui(ctx, &mut out));
                }
                assert!(out.is_empty(), "{name} {selected} {capturing:?}: {out:?}");
            }
        }
    }
    // An empty device teaches instead of listing.
    let (st, id) = bind_state(DeviceKind::Keyboard, "K70", DeviceRemaps::default());
    let ctx = egui::Context::default();
    let mut app = bind_app(&ctx, &st, &id, 0);
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut Vec::new()));
}

// ── Offscreen snapshots (GPU) ────────────────────────────────────
// Rendered with wgpu, so ignored by default: `cargo test -p hako-ui -- --ignored`.
// PNGs land in libs/ui/tests/snapshots/ for review; nothing is compared.

const SHOT: [f32; 2] = [1180.0, 780.0];

fn shoot(
    name: &str,
    size: [f32; 2],
    (st, id): (HubState, DeviceId),
    selected: u16,
    script: impl FnOnce(&mut egui_kittest::Harness<'_>),
) {
    let mut app = None;
    let mut out = Vec::new();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(size)
        .wgpu()
        .build(|ctx| {
            app.get_or_insert_with(|| bind_app(ctx, &st, &id, selected))
                .ui(ctx, &mut out)
        });
    harness.run_steps(6);
    script(&mut harness);
    harness.run_steps(6);
    let image = harness.render().expect("offscreen render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    image.save(dir.join(format!("{name}.png"))).unwrap();
}

fn pointer(h: &mut egui_kittest::Harness<'_>, pos: [f32; 2], press: Option<bool>) {
    let pos = egui::pos2(pos[0], pos[1]);
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    if let Some(pressed) = press {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        });
    }
    h.run_steps(2);
}

fn scroll(h: &mut egui_kittest::Harness<'_>, dy: f32) {
    pointer(h, [700.0, 500.0], None);
    h.input_mut().events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -dy),
        modifiers: Default::default(),
    });
    h.run_steps(12);
}

fn remaps(kind: DeviceKind) -> DeviceRemaps {
    DeviceRemaps {
        bindings: sample_bindings(kind),
        grabbed: true,
        ..Default::default()
    }
}

fn tkl(remaps: DeviceRemaps) -> (HubState, DeviceId) {
    bind_state(DeviceKind::Keyboard, "Razer Huntsman V2 TKL", remaps)
}

#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_keyboard_layouts() {
    for (file, name) in [
        ("keyboard_full", "Corsair K70 RGB PRO"),
        ("keyboard_tkl", "Razer Huntsman V2 TKL"),
        ("keyboard_75", "HyperX Alloy Origins 65"),
        ("keyboard_60", "Razer Huntsman Mini"),
    ] {
        let st = bind_state(DeviceKind::Keyboard, name, remaps(DeviceKind::Keyboard));
        shoot(file, SHOT, st, 58, |_| {});
    }
    shoot(
        "keyboard_page",
        [1180.0, 1320.0],
        tkl(remaps(DeviceKind::Keyboard)),
        57,
        |_| {},
    );
    shoot(
        "keyboard_min_window",
        [900.0, 600.0],
        tkl(remaps(DeviceKind::Keyboard)),
        57,
        |_| {},
    );
}

#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_mouse() {
    let st = || {
        bind_state(
            DeviceKind::Mouse,
            "Logitech PRO 2 LIGHTSPEED",
            remaps(DeviceKind::Mouse),
        )
    };
    shoot("mouse", SHOT, st(), 0x113, |_| {});
    shoot("mouse_page", [1180.0, 1320.0], st(), 0x113, |_| {});
    shoot("mouse_min_window", [900.0, 600.0], st(), 0x113, |_| {});
}

#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_macro_editor() {
    let st = || tkl(remaps(DeviceKind::Keyboard));
    shoot("macro_editor", SHOT, st(), 34, |h| scroll(h, 330.0));
    shoot("macro_editor_page", [1180.0, 1500.0], st(), 34, |_| {});
    let recording = tkl(DeviceRemaps {
        capturing: Some(CaptureRole::Record),
        recorded: steps(&[17, 30, 31]),
        ..remaps(DeviceKind::Keyboard)
    });
    shoot("macro_recording", SHOT, recording, 34, |h| scroll(h, 330.0));
}

#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_library_and_empty_state() {
    let st = tkl(DeviceRemaps::default());
    let lib = egui::Id::new(("bind_library", st.1.key()));
    shoot("library_empty_state", SHOT, st.clone(), 0, |h| {
        h.ctx
            .data_mut(|m| m.insert_temp(lib, (3_usize, String::new())));
        scroll(h, 330.0);
    });
    shoot("library_search", SHOT, st, 0, |h| {
        h.ctx
            .data_mut(|m| m.insert_temp(lib, (0_usize, "vol".to_string())));
        scroll(h, 330.0);
    });
}

#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_listening() {
    let st = tkl(DeviceRemaps {
        capturing: Some(CaptureRole::Source),
        ..remaps(DeviceKind::Keyboard)
    });
    shoot("listening_detect", SHOT, st, 0, |_| {});
}

#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_dragging_action_onto_key() {
    shoot(
        "dragging",
        SHOT,
        tkl(remaps(DeviceKind::Keyboard)),
        34,
        |h| {
            scroll(h, 330.0);
            // Grab the "A" chip in Actions and carry it over the K key.
            pointer(h, [844.0, 644.0], Some(true));
            pointer(h, [800.0, 500.0], None);
            pointer(h, [706.0, 150.0], None);
            assert!(
                egui::DragAndDrop::has_any_payload(&h.ctx),
                "layout moved: the press no longer lands on a library chip"
            );
        },
    );
}

/// Render `app` after `setup` (tab selection etc.) and save `audit/{name}.png`.
fn shoot_app(
    name: &str,
    size: [f32; 2],
    app: HakoAppUi,
    setup: impl FnOnce(&egui::Context),
    script: impl FnOnce(&mut egui_kittest::Harness<'_>),
) {
    let image = render_app(size, app, setup, script);
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/audit");
    std::fs::create_dir_all(&dir).unwrap();
    image.save(dir.join(format!("{name}.png"))).unwrap();
}

fn render_app(
    size: [f32; 2],
    app: HakoAppUi,
    setup: impl FnOnce(&egui::Context),
    script: impl FnOnce(&mut egui_kittest::Harness<'_>),
) -> image::RgbaImage {
    let mut app = app;
    let mut setup = Some(setup);
    let mut out = Vec::new();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(size)
        .with_step_dt(1.0 / 30.0)
        .wgpu()
        .build(|ctx| {
            if let Some(f) = setup.take() {
                f(ctx);
            }
            app.ui(ctx, &mut out)
        });
    harness.run_steps(8);
    script(&mut harness);
    harness.run_steps(12);
    harness.render().expect("offscreen render")
}

/// Every page and tab, for visual review. `HAKO_UI_STATE` / `HAKO_UI_CONFIG`
/// point at JSON dumps of `GetState` / `GetConfig` to render a live daemon.
#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_audit_gallery() {
    let load = |var: &str| {
        std::env::var(var)
            .ok()
            .map(|p| std::fs::read_to_string(p).unwrap())
    };
    let state: HubState = load("HAKO_UI_STATE")
        .map(|s| serde_json::from_str(&s).unwrap())
        .unwrap_or_else(sample_state);
    let settings: AppSettings = load("HAKO_UI_CONFIG")
        .map(|s| serde_json::from_str(&s).unwrap())
        .unwrap_or_else(settings_with_chain);
    let app = |state: &HubState, selected: Option<DeviceId>| HakoAppUi {
        state: Some(state.clone()),
        selected,
        daemon_error: None,
        settings: Some(settings.clone()),
        ..Default::default()
    };
    for d in &state.devices {
        let tabs = tabs_for(d);
        for &tab in &tabs {
            let name = format!(
                "{:?}_{}",
                d.kind,
                tab.label().to_lowercase().replace(' ', "_")
            )
            .to_lowercase();
            let tab_id = egui::Id::new("tab");
            for (suffix, size) in [("", SHOT), ("_full", [1180.0, 1500.0])] {
                shoot_app(
                    &format!("{name}{suffix}"),
                    size,
                    app(&state, Some(d.id.clone())),
                    |ctx| ctx.data_mut(|m| m.insert_temp(tab_id, tab)),
                    |_| {},
                );
            }
        }
    }
    shoot_app(
        "settings",
        [1180.0, 1100.0],
        HakoAppUi {
            show_settings: true,
            ..app(&state, None)
        },
        |_| {},
        |_| {},
    );
    let mut with_profiles = state.clone();
    with_profiles.profiles = vec!["Default".into(), "FPS".into(), "Late night".into()];
    with_profiles.last_profile = Some("FPS".into());
    shoot_app(
        "profile_menu",
        SHOT,
        app(&with_profiles, None),
        |_| {},
        // The profile dropdown sits left of the settings gear in the top bar;
        // the pointer rests on a row, which shows its delete button.
        |h| {
            pointer(h, [1000.0, 28.0], Some(true));
            pointer(h, [1000.0, 28.0], Some(false));
            h.run_steps(40);
            pointer(h, [1000.0, 186.0], None);
        },
    );
    shoot_app(
        "profile_delete_confirm",
        SHOT,
        HakoAppUi {
            profile_delete: Some("Late night".into()),
            ..app(&with_profiles, None)
        },
        |_| {},
        |_| {},
    );
    // The headset's ChatMix card while the mixer owns ChatMix.
    if let Some(headset) = state.devices.iter().find(|d| d.kind == DeviceKind::Headset) {
        let mut on = app(&state, Some(headset.id.clone()));
        on.mixer.receive(sample_mixer());
        let tab_id = egui::Id::new("tab");
        shoot_app(
            "headset_audio_mixer_on",
            SHOT,
            on,
            |ctx| ctx.data_mut(|m| m.insert_temp(tab_id, Tab::Audio)),
            |_| {},
        );
    }
    let mut toasting = app(&state, None);
    toasting.notify(widgets::Tone::Ok, "DPI saved to device memory");
    toasting.notify(
        widgets::Tone::Warn,
        "Lost connection to hakod. Reconnecting…",
    );
    toasting.notify(
        widgets::Tone::Error,
        "Couldn't change Brightness: device did not acknowledge the write",
    );
    shoot_app("toasts", SHOT, toasting, |_| {}, |_| {});
    // Every theme on the pages with the most surface variety.
    for t in &theme::THEMES {
        let themed = AppSettings {
            theme: t.id.into(),
            ..settings.clone()
        };
        let with = |ui: HakoAppUi| HakoAppUi {
            settings: Some(themed.clone()),
            ..ui
        };
        shoot_app(
            &format!("theme_{}_settings", t.id),
            [1180.0, 1180.0],
            with(HakoAppUi {
                show_settings: true,
                ..app(&state, None)
            }),
            |_| {},
            |_| {},
        );
        for d in &state.devices {
            let tabs = tabs_for(d);
            let tab_id = egui::Id::new("tab");
            for (tab, label) in [(tabs[0], "main"), (tabs[tabs.len() - 1], "last")] {
                shoot_app(
                    &format!("theme_{}_{:?}_{label}", t.id, d.kind).to_lowercase(),
                    [1180.0, 1100.0],
                    with(app(&state, Some(d.id.clone()))),
                    |ctx| ctx.data_mut(|m| m.insert_temp(tab_id, tab)),
                    |_| {},
                );
            }
        }
    }
    shoot_app("offline", SHOT, HakoAppUi::default(), |_| {}, |_| {});
    // Onboard-memory gate (live daemons may predate it).
    let mut mem = sample_state();
    let onboard = {
        use SettingKey::*;
        dev(
            DeviceKind::Mouse,
            vec![
                SettingMeta::slider(Dpi, "DPI", 100.0, 26000.0, 50.0).persistent(),
                SettingMeta::slider(DpiStage1, "Stage 1", 100.0, 26000.0, 50.0)
                    .persistent()
                    .blocked("layout unconfirmed"),
                SettingMeta::toggle(OnboardWrites, "Allow saving to device memory")
                    .warning("Writes go to the device."),
            ],
            map(&[
                (Dpi, SettingValue::Int(800)),
                (OnboardWrites, SettingValue::Bool(false)),
            ]),
        )
    };
    mem.devices = vec![onboard.clone()];
    for (i, tab) in tabs_for(&onboard).into_iter().enumerate() {
        let tab_id = egui::Id::new("tab");
        shoot_app(
            &format!("memory_gate_tab{i}"),
            SHOT,
            app(&mem, Some(onboard.id.clone())),
            |ctx| ctx.data_mut(|m| m.insert_temp(tab_id, tab)),
            |_| {},
        );
    }
}

#[test]
fn dpi_snap_stays_inside_the_device_range() {
    use crate::controls::snap_dpi;
    let snap = |v, min, max, step| {
        snap_dpi(
            &SettingMeta::slider(SettingKey::Dpi, "DPI", min, max, step),
            v,
        )
    };
    // Top of a 100..25600 sensor no longer rounds to an out-of-range 26000.
    assert_eq!(snap(25600.0, 100.0, 25600.0, 50.0), Some(25600.0));
    // A 45000 DPI sensor is reachable; a 50 DPI minimum is selectable.
    assert_eq!(snap(45000.0, 100.0, 45000.0, 50.0), Some(45000.0));
    assert_eq!(snap(50.0, 50.0, 18000.0, 50.0), Some(50.0));
    assert_eq!(snap(1649.0, 100.0, 26000.0, 50.0), Some(1600.0));
    // Magnitude steps stay on a coarser device grid (SteelSeries Sensei 90s).
    assert_eq!(snap(1649.0, 90.0, 5670.0, 90.0), Some(1620.0));
    assert_eq!(snap(12345.0, 100.0, 16000.0, 300.0), Some(12100.0));
}

/// The DPI readout never sends a value on its own: not for a reported DPI
/// outside the range, not for a missing one, not while disabled.
#[test]
fn dpi_control_sends_nothing_without_input() {
    use SettingKey::*;
    let meta = SettingMeta::slider(Dpi, "DPI", 100.0, 1000.0, 50.0).unit("dpi");
    for (dpi, blocked) in [(Some(5000), false), (None, false), (Some(5000), true)] {
        let meta = if blocked {
            meta.clone().blocked("test")
        } else {
            meta.clone()
        };
        let settings = dpi.map_or_else(SettingsMap::new, |v| map(&[(Dpi, SettingValue::Int(v))]));
        let d = dev(DeviceKind::Mouse, vec![meta], settings);
        let ctx = egui::Context::default();
        let mut app = HakoAppUi {
            state: Some(HubState {
                devices: vec![d.clone()],
                ..sample_state()
            }),
            selected: Some(d.id.clone()),
            daemon_error: None,
            ..Default::default()
        };
        let mut out = Vec::new();
        for _ in 0..3 {
            let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
        }
        assert!(out.is_empty(), "dpi {dpi:?} blocked {blocked}: {out:?}");
    }
}

/// A fractional slider sends a value on its grid, not an f32 rounding of it.
#[test]
fn fractional_slider_commits_exact_grid_values() {
    let mut h = egui_kittest::Harness::builder()
        .with_size(egui::vec2(400.0, 60.0))
        .build_ui_state(
            |ui, out: &mut Vec<UiCommand>| {
                let meta =
                    SettingMeta::slider(SettingKey::LiftOffDistance, "Lift-off", 0.7, 2.0, 0.1);
                let d = dev(
                    DeviceKind::Mouse,
                    vec![meta.clone()],
                    map(&[(SettingKey::LiftOffDistance, SettingValue::Float(1.0))]),
                );
                controls::rows(ui, &d, &[&meta], out);
            },
            Vec::new(),
        );
    use egui_kittest::kittest::Queryable;
    h.run_steps(2);
    let slider = h
        .get_by_role(egui::accesskit::Role::Slider)
        .bounding_box()
        .expect("a slider");
    // A click at the left end of the track picks the minimum.
    let at = egui::pos2(
        slider.x0 as f32 + 1.0,
        ((slider.y0 + slider.y1) / 2.0) as f32,
    );
    h.input_mut().events.push(egui::Event::PointerMoved(at));
    h.run_steps(2);
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        });
        h.run_steps(2);
    }
    let sent = h.state();
    assert!(
        matches!(
            sent.last(),
            Some(UiCommand::Set { value: SettingValue::Float(v), .. }) if *v == 0.7
        ),
        "{sent:?}"
    );
}

#[test]
fn meter_ballistics_attack_release_and_hold() {
    use crate::widgets::{to_dbfs, Ballistics, METER_FLOOR_DB};
    assert_eq!(to_dbfs(1.0), 0.0);
    assert!((to_dbfs(0.5) + 6.02).abs() < 0.01);
    assert_eq!(to_dbfs(0.0), METER_FLOOR_DB);
    assert_eq!(to_dbfs(1e-9), METER_FLOOR_DB);
    assert_eq!(to_dbfs(4.0), 0.0, "clipping pins to full scale");

    let mut b = Ballistics::default();
    // Attack is instant.
    b.step(-6.0, 0.016, false);
    assert_eq!((b.level, b.peak), (-6.0, -6.0));
    // Release: 20 dB/s; the peak tick holds.
    b.step(METER_FLOOR_DB, 0.5, false);
    assert!((b.level + 16.0).abs() < 1e-4, "{b:?}");
    assert_eq!(b.peak, -6.0);
    // Never falls below the input.
    b.step(-10.0, 0.4, false);
    assert_eq!(b.level, -10.0);
    assert_eq!(b.peak, -6.0, "held for 0.9 s");
    // After a second the tick drops to the bar.
    b.step(-10.0, 0.2, false);
    assert_eq!(b.peak, b.level);
    // A louder peak takes the tick again.
    b.step(-3.0, 0.016, false);
    assert_eq!(b.peak, -3.0);

    // Reduced motion shows the input as is.
    b.step(-40.0, 0.016, true);
    assert_eq!((b.level, b.peak), (-40.0, -40.0));
}

// ── Mixer ────────────────────────────────────────────────────────

/// A full console: 2 hardware + 3 virtual inputs, 4 outputs, one problem.
fn sample_mixer() -> hako_core::mixer::MixerState {
    use hako_core::mixer::*;
    let source = |node: &str, description: &str, trim_db: f32| Source {
        node: node.into(),
        description: description.into(),
        trim_db,
        mute: false,
        alsa_input: None,
        capture_percent: None,
    };
    let fx = |kinds: &[(&str, AuxEffectKind, bool)]| EffectChain {
        effects: kinds
            .iter()
            .map(|(id, kind, enabled)| AuxEffect {
                id: (*id).into(),
                enabled: *enabled,
                kind: kind.clone(),
            })
            .collect(),
    };
    let input = |id: &str, label: &str, kind, chain, gain_db, routes: &[&str]| Input {
        id: id.into(),
        label: label.into(),
        kind,
        chain,
        gain_db,
        mute: false,
        solo: false,
        routes: routes.iter().map(|r| (*r).into()).collect(),
        chatmix: None,
    };
    let virt = |node: &str| InputKind::Virtual {
        node_name: node.into(),
        apps: Vec::new(),
    };
    let output = |id: &str, label: &str, kind, chain, gain_db| Output {
        id: id.into(),
        label: label.into(),
        kind,
        chain,
        gain_db,
        mute: false,
        mono: false,
    };
    let mut inputs = vec![
        input(
            "voice",
            "Voice",
            InputKind::Hardware {
                sources: vec![
                    source("alsa_input.arctis-mic", "Arctis Nova Pro mic", 0.0),
                    source("alsa_input.mg-xu", "MG-XU Input 1", -3.0),
                ],
            },
            fx(&[
                ("gate-1", AuxEffectKind::default_gate(), true),
                ("eq-1", AuxEffectKind::default_equalizer(), true),
                ("compressor-1", AuxEffectKind::default_compressor(), true),
            ]),
            -2.0,
            &["B1"],
        ),
        input(
            "line-in",
            "Line-in",
            InputKind::Hardware {
                sources: vec![Source {
                    alsa_input: Some("Line".into()),
                    capture_percent: Some(66),
                    ..source("alsa_input.ryzen-line", "Ryzen HD Audio Line", 0.0)
                }],
            },
            fx(&[
                ("eq-1", AuxEffectKind::default_equalizer(), true),
                ("gate-1", AuxEffectKind::default_gate(), false),
            ]),
            -6.0,
            &["A1", "A2", "B2"],
        ),
        input(
            "game",
            "Game",
            virt("Hako_Game"),
            EffectChain::default(),
            0.0,
            &["A1", "B2"],
        ),
        input(
            "chat",
            "Chat",
            virt("Hako_Chat"),
            EffectChain::default(),
            -3.0,
            &["A1"],
        ),
        input(
            "music",
            "Music",
            virt("Hako_Music"),
            fx(&[("eq-1", AuxEffectKind::default_equalizer(), true)]),
            -14.0,
            &["A1", "A2", "B2"],
        ),
    ];
    inputs[2].chatmix = Some(ChatMixSide::Game);
    inputs[3].chatmix = Some(ChatMixSide::Chat);
    inputs[0].solo = true;
    let mut outputs = vec![
        output(
            "A1",
            "Headset",
            OutputKind::Hardware {
                sink: "alsa_output.arctis".into(),
                description: "Arctis Nova Pro".into(),
            },
            fx(&[("eq-1", AuxEffectKind::default_equalizer(), true)]),
            0.0,
        ),
        output(
            "A2",
            "Speakers",
            OutputKind::Hardware {
                sink: "alsa_output.mg-xu".into(),
                description: "MG-XU Digital".into(),
            },
            EffectChain::default(),
            -10.0,
        ),
        output(
            "B1",
            "hako Mic",
            OutputKind::Virtual {
                node_name: "Hako_Mic".into(),
            },
            fx(&[("compressor-1", AuxEffectKind::default_compressor(), true)]),
            0.0,
        ),
        output(
            "B2",
            "hako Stream",
            OutputKind::Virtual {
                node_name: "Hako_Stream".into(),
            },
            EffectChain::default(),
            -2.0,
        ),
    ];
    outputs[1].mute = true;
    let device = |node: &str, description: &str, direction, channels| AudioDevice {
        node: node.into(),
        description: description.into(),
        direction,
        channels,
        inputs: Vec::new(),
        capture: false,
        capture_now: None,
    };
    let file = MixerFile {
        enabled: true,
        live: Scene { inputs, outputs },
        ..MixerFile::default()
    };
    file.validate().unwrap();
    MixerState {
        file,
        status: MixerStatus {
            running: true,
            problems: vec![StripProblem {
                strip: "A2".into(),
                message: "MG-XU Digital is unplugged".into(),
            }],
            apps: vec!["Brave".into(), "Spotify".into()],
            devices: vec![
                device(
                    "alsa_input.arctis-mic",
                    "Arctis Nova Pro mic",
                    DeviceDirection::Source,
                    1,
                ),
                device(
                    "alsa_input.mg-xu",
                    "MG-XU Input 1",
                    DeviceDirection::Source,
                    2,
                ),
                AudioDevice {
                    inputs: vec!["Rear Mic".into(), "Front Mic".into(), "Line".into()],
                    capture: true,
                    capture_now: Some(48),
                    ..device(
                        "alsa_input.ryzen-line",
                        "Ryzen HD Audio Line",
                        DeviceDirection::Source,
                        2,
                    )
                },
                device(
                    "alsa_input.webcam",
                    "C920 Webcam",
                    DeviceDirection::Source,
                    2,
                ),
                device(
                    "alsa_output.arctis",
                    "Arctis Nova Pro",
                    DeviceDirection::Sink,
                    2,
                ),
            ],
        },
    }
}

fn mixer_app(mixer: Option<hako_core::mixer::MixerState>) -> HakoAppUi {
    let mut app = HakoAppUi {
        state: Some(sample_state()),
        daemon_error: None,
        show_mixer: true,
        settings: Some(AppSettings::default()),
        // Two strips in the ok zone, one reaching danger.
        meter_levels: vec![
            [0.3, 0.25],
            [0.08, 0.06],
            [0.5, 0.45],
            [0.02, 0.03],
            [0.2, 0.2],
        ]
        .into_iter()
        .chain([[0.6, 0.95], [0.0, 0.0], [0.3, 0.3], [0.4, 0.38]])
        .collect(),
        ..Default::default()
    };
    if let Some(m) = mixer {
        app.mixer.receive(m);
    }
    app
}

/// Levels are drawn on the strip they were measured for: on the frame a strip goes, the
/// levels asked for the old order do not shift onto its neighbours.
#[test]
fn meter_levels_follow_their_strip_when_the_order_changes() {
    use crate::widgets::{to_dbfs, Ballistics};
    let ctx = egui::Context::default();
    let mut out = Vec::new();
    let mut app = mixer_app(Some(sample_mixer()));
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    // What the client streams for those targets: voice loud, line-in quieter.
    app.meter_levels = app
        .meter_targets
        .iter()
        .map(|t| match t {
            MeterTargetSpec::MixerInput { id } if id == "voice" => [0.5; 2],
            MeterTargetSpec::MixerInput { id } if id == "line-in" => [0.25; 2],
            _ => [0.0; 2],
        })
        .collect();
    let mut m = sample_mixer();
    m.file.live.inputs.retain(|i| i.id != "voice");
    app.mixer.receive(m);
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    let strip = mixer::Strip::Input("line-in".into());
    let meter = egui::Id::new(("mixer_strip", &strip)).with("meter");
    let (chans, _) = ctx
        .data(|m| m.get_temp::<([Ballistics; 2], f64)>(meter))
        .unwrap();
    assert!((chans[0].level - to_dbfs(0.25)).abs() < 0.01, "{chans:?}");
}

/// Without devices while hakod is gone, the page says why, not "No devices" from the
/// list kept from before.
#[test]
fn an_offline_daemon_is_explained_over_a_stale_empty_list() {
    let mut st = sample_state();
    st.devices.clear();
    let h = drive(HakoAppUi {
        state: Some(st),
        daemon_error: Some("Daemon stopped".into()),
        ..Default::default()
    });
    assert!(shown(&h, "Daemon stopped"));
    assert!(!shown(&h, "No devices"));
}

#[test]
fn mixer_page_asks_for_meters_only_while_visible() {
    use hako_core::mixer::MixerState;
    let targets = |app: &mut HakoAppUi| {
        let ctx = egui::Context::default();
        let mut out = Vec::new();
        let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
        assert!(out.is_empty(), "rendering sent {out:?}");
        app.meter_targets.clone()
    };
    let mut app = mixer_app(Some(sample_mixer()));
    let all = targets(&mut app);
    let ids: Vec<String> = all
        .iter()
        .map(|t| match t {
            MeterTargetSpec::MixerInput { id } => format!("in:{id}"),
            MeterTargetSpec::MixerOutput { id } => format!("out:{id}"),
            other => panic!("{other:?}"),
        })
        .collect();
    // Display order: hardware inputs, virtual inputs, outputs.
    assert_eq!(
        ids,
        [
            "in:voice",
            "in:line-in",
            "in:game",
            "in:chat",
            "in:music",
            "out:A1",
            "out:A2",
            "out:B1",
            "out:B2"
        ]
    );
    assert!(all.len() <= hako_core::ipc::MAX_METER_TARGETS);
    // Another page, the settings page on top, or the mixer off: no meters.
    app.show_mixer = false;
    assert!(targets(&mut app).is_empty());
    app.show_mixer = true;
    app.show_settings = true;
    assert!(targets(&mut app).is_empty());
    app.show_settings = false;
    let mut off = sample_mixer();
    off.file.enabled = false;
    let mut app = mixer_app(Some(off));
    assert!(targets(&mut app).is_empty());
    let mut app = mixer_app(Some(MixerState::default()));
    assert!(targets(&mut app).is_empty());
}

#[test]
fn mixer_page_renders_every_state() {
    let ctx = egui::Context::default();
    let mut out = Vec::new();
    let mut render = |app: &mut HakoAppUi| {
        for input in [input(), input(), {
            let mut narrow = input();
            narrow.screen_rect = Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(900.0, 600.0),
            ));
            narrow
        }] {
            let _ = ctx.run(input, |ctx| app.ui(ctx, &mut out));
        }
        assert!(out.is_empty(), "rendering sent {out:?}");
    };
    // Loading, unavailable (an older hakod), off, and on.
    render(&mut mixer_app(None));
    let mut app = mixer_app(None);
    app.mixer.unavailable("This hakod has no mixer yet.".into());
    render(&mut app);
    let mut off = sample_mixer();
    off.file.enabled = false;
    render(&mut mixer_app(Some(off)));
    let mut app = mixer_app(Some(sample_mixer()));
    render(&mut app);
    // Every strip's drawer.
    let m = sample_mixer();
    let strips = m
        .file
        .live
        .inputs
        .iter()
        .map(|i| mixer::Strip::Input(i.id.clone()))
        .chain(
            m.file
                .live
                .outputs
                .iter()
                .map(|o| mixer::Strip::Output(o.id.clone())),
        );
    for s in strips {
        app.mixer.select(s);
        render(&mut app);
    }
    // No strips at all.
    let mut empty = hako_core::mixer::MixerState::default();
    empty.file.enabled = true;
    render(&mut mixer_app(Some(empty)));
}

/// The mixer page in its states, at the default and the minimum window.
#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_mixer() {
    let on = || mixer_app(Some(sample_mixer()));
    let mut off = sample_mixer();
    off.file.enabled = false;
    shoot_app("mixer_off", SHOT, mixer_app(Some(off)), |_| {}, |_| {});
    let mut unavailable = mixer_app(None);
    unavailable
        .mixer
        .unavailable("This hakod has no mixer yet. Update hako to use it.".into());
    shoot_app("mixer_unavailable", SHOT, unavailable, |_| {}, |_| {});
    // Voice is soloed in the sample: the rest read as muted by it.
    shoot_app("mixer", SHOT, on(), |_| {}, |_| {});
    let mut no_solo = sample_mixer();
    no_solo.file.live.inputs[0].solo = false;
    shoot_app(
        "mixer_no_solo",
        SHOT,
        mixer_app(Some(no_solo)),
        |_| {},
        |_| {},
    );
    let mut line_in = on();
    line_in.mixer.select(mixer::Strip::Input("line-in".into()));
    shoot_app(
        "mixer_line_in_drawer",
        [1180.0, 1300.0],
        line_in,
        |_| {},
        |_| {},
    );
    // Two merged devices, one with a sound card's jack and input gain.
    let mut merged = sample_mixer();
    merged.status.devices[1].inputs = vec!["Mic".into(), "Line".into()];
    merged.status.devices[1].capture = true;
    merged.status.devices[1].capture_now = Some(48);
    shoot_app(
        "mixer_merged_drawer",
        [1180.0, 1300.0],
        mixer_app(Some(merged)),
        |_| {},
        |_| {},
    );
    shoot_app("mixer_min_window", [900.0, 600.0], on(), |_| {}, |_| {});
    // The drawers at the minimum width (the device row wraps).
    for (name, strip) in [
        ("mixer_min_line_in_drawer", "line-in"),
        ("mixer_min_merged_drawer", "voice"),
    ] {
        let mut app = on();
        app.mixer.select(mixer::Strip::Input(strip.into()));
        shoot_app(name, [900.0, 1100.0], app, |_| {}, |_| {});
    }
    shoot_app("mixer_full", [1180.0, 1700.0], on(), |_| {}, |_| {});
    // Renaming in the drawer title, and the question before a strip goes.
    for (name, button) in [
        ("mixer_rename", "Rename"),
        ("mixer_remove_confirm", "Remove…"),
    ] {
        let mut app = on();
        app.mixer.select(mixer::Strip::Input("line-in".into()));
        shoot_app(
            name,
            [1180.0, 1300.0],
            app,
            |_| {},
            |h| {
                use egui_kittest::kittest::Queryable;
                h.get_by_label(button).click();
            },
        );
    }
    let mut output = on();
    output.mixer.select(mixer::Strip::Output("A2".into()));
    shoot_app(
        "mixer_output_drawer",
        [1180.0, 1300.0],
        output,
        |_| {},
        |_| {},
    );
    let mut virt = on();
    virt.mixer.select(mixer::Strip::Input("game".into()));
    virt.settings = Some(AppSettings {
        theme: "daylight".into(),
        ..AppSettings::default()
    });
    shoot_app("mixer_daylight", [1180.0, 1300.0], virt, |_| {}, |_| {});
    for (name, output, virt) in [
        ("mixer_add_input", false, false),
        ("mixer_add_virtual_output", true, true),
    ] {
        let mut app = on();
        app.mixer.open_add_dialog(output, virt);
        shoot_app(name, SHOT, app, |_| {}, |_| {});
    }
}

/// An input silenced by another's solo says so, wherever the soloed strip
/// is; the header pill clears every solo in one patch.
#[test]
fn a_solo_shows_on_the_inputs_it_silences() {
    let mut m = sample_mixer();
    let inputs = &m.file.live.inputs;
    // Voice is soloed: everything else is muted by it, Voice is not.
    assert_eq!(mixer::solo_mute_reason(&inputs[0], inputs), None);
    assert_eq!(
        mixer::solo_mute_reason(&inputs[1], inputs).as_deref(),
        Some("Muted by solo on Voice")
    );
    m.file.live.inputs[4].solo = true;
    let inputs = &m.file.live.inputs;
    assert_eq!(
        mixer::solo_mute_reason(&inputs[2], inputs).as_deref(),
        Some("Muted by solo on Voice, Music")
    );
    // No solo anywhere: nothing implied.
    let mut none = inputs.clone();
    for i in &mut none {
        i.solo = false;
    }
    assert!(none
        .iter()
        .all(|i| mixer::solo_mute_reason(i, &none).is_none()));

    let clear = mixer::clear_solos(inputs);
    assert_eq!(clear.len(), 2);
    let mut app = mixer_app(Some(m.clone()));
    let ctx = egui::Context::default();
    let mut out = Vec::new();
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    assert!(out.is_empty(), "rendering a solo sent {out:?}");
    let mut file = m.file.clone();
    for op in &clear {
        file.apply(op).unwrap();
    }
    assert!(file.live.inputs.iter().all(|i| !i.solo));
}

/// Picking "Mixer" in the rail opens the page and asks for the mixer once.
#[test]
fn mixer_rail_entry_opens_the_page() {
    let click = |pos: egui::Pos2, pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    // Below the device entries; found by clicking down the rail.
    for y in (300..760).step_by(10) {
        let ctx = egui::Context::default();
        let mut app = mixer_app(Some(sample_mixer()));
        app.show_mixer = false;
        let mut out = Vec::new();
        let at = egui::pos2(100.0, y as f32);
        let mut press = input();
        press.events = vec![egui::Event::PointerMoved(at), click(at, true)];
        let mut release = input();
        release.events = vec![click(at, false)];
        for ev in [input(), press, release, input()] {
            let _ = ctx.run(ev, |ctx| app.ui(ctx, &mut out));
        }
        if app.show_mixer {
            assert!(matches!(out.as_slice(), [UiCommand::GetMixer]), "{out:?}");
            // Clicking it again while open doesn't ask again.
            out.clear();
            let mut press = input();
            press.events = vec![egui::Event::PointerMoved(at), click(at, true)];
            let mut release = input();
            release.events = vec![click(at, false)];
            for ev in [press, release] {
                let _ = ctx.run(ev, |ctx| app.ui(ctx, &mut out));
            }
            assert!(out.is_empty(), "{out:?}");
            return;
        }
    }
    panic!("no Mixer entry in the rail");
}

/// The daemon's state is held back while edits are in flight, then adopted.
#[test]
fn mixer_adopts_daemon_state_once_settled() {
    let ctx = egui::Context::default();
    let mut app = mixer_app(Some(sample_mixer()));
    let mut out = Vec::new();
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    let before = app.mixer.state().unwrap().file.clone();
    let mut answer = sample_mixer();
    answer.file.live.inputs[0].gain_db = -9.0;
    app.mixer.busy = true;
    app.mixer.receive(answer.clone());
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    assert_eq!(app.mixer.state().unwrap().file, before);
    app.mixer.busy = false;
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    assert_eq!(app.mixer.state().unwrap().file, answer.file);
    assert!(out.is_empty(), "{out:?}");
}

// ── Clicking through (headless) ──────────────────────────────────

type Driven = egui_kittest::Harness<'static, (HakoAppUi, Vec<UiCommand>)>;

/// The whole UI in a tall window, with the commands it sent.
fn drive(app: HakoAppUi) -> Driven {
    let mut h = egui_kittest::Harness::builder()
        .with_size([1200.0, 1400.0])
        .build_state(
            |ctx, (app, out): &mut (HakoAppUi, Vec<UiCommand>)| app.ui(ctx, out),
            (app, Vec::new()),
        );
    h.run_steps(4);
    h
}

fn click(h: &mut Driven, label: &str) {
    use egui_kittest::kittest::Queryable;
    h.get_by_label(label).click();
    h.run_steps(4);
}

fn shown(h: &Driven, label: &str) -> bool {
    use egui_kittest::kittest::Queryable;
    h.query_all_by_label(label).next().is_some()
}

fn sent(h: &mut Driven) -> Vec<UiCommand> {
    std::mem::take(&mut h.state_mut().1)
}

fn sent_ops(h: &mut Driven) -> Vec<MixerOp> {
    sent(h)
        .into_iter()
        .flat_map(|c| match c {
            UiCommand::PatchMixer(ops) => ops,
            other => panic!("{other:?}"),
        })
        .collect()
}

fn drawer(strip: mixer::Strip) -> Driven {
    let mut app = mixer_app(Some(sample_mixer()));
    app.mixer.select(strip);
    drive(app)
}

/// "Remove…" only asks; the strip goes once the question is answered yes.
#[test]
fn removing_a_strip_asks_first() {
    let mut h = drawer(mixer::Strip::Input("line-in".into()));
    click(&mut h, "Remove…");
    assert!(shown(&h, "Remove Line-in?"));
    assert!(sent_ops(&mut h).is_empty());
    click(&mut h, "Cancel");
    assert!(!shown(&h, "Remove Line-in?"));
    assert!(sent_ops(&mut h).is_empty());
    click(&mut h, "Remove…");
    click(&mut h, "Remove");
    assert_eq!(
        sent_ops(&mut h),
        [MixerOp::RemoveInput {
            input: "line-in".into()
        }]
    );
    assert!(!shown(&h, "Remove Line-in?"));
    let inputs = &h.state().0.mixer.state().unwrap().file.live.inputs;
    assert!(inputs.iter().all(|i| i.id != "line-in"));

    // Outputs ask the same way.
    let mut h = drawer(mixer::Strip::Output("B2".into()));
    click(&mut h, "Remove…");
    assert!(sent_ops(&mut h).is_empty());
    click(&mut h, "Remove");
    assert_eq!(
        sent_ops(&mut h),
        [MixerOp::RemoveOutput {
            output: "B2".into()
        }]
    );
}

/// The title turns into a field: Enter sends the new name, Escape drops it.
#[test]
fn a_strip_is_renamed_from_its_drawer_title() {
    let type_name = |h: &mut Driven, end: egui::Key| {
        click(h, "Rename");
        h.press_key_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        h.run_steps(2);
        h.input_mut().events.push(egui::Event::Text("Desk".into()));
        h.run_steps(2);
        h.press_key(end);
        h.run_steps(4);
    };
    let mut h = drawer(mixer::Strip::Input("line-in".into()));
    type_name(&mut h, egui::Key::Escape);
    assert!(sent_ops(&mut h).is_empty());
    assert!(shown(&h, "Line-in"));
    type_name(&mut h, egui::Key::Enter);
    match sent_ops(&mut h).as_slice() {
        [MixerOp::PutInput { input }] => {
            let was = &sample_mixer().file.live.inputs[1];
            assert_eq!((input.label.as_str(), &input.id), ("Desk", &was.id));
            assert_eq!((&input.kind, &input.routes), (&was.kind, &was.routes));
        }
        other => panic!("{other:?}"),
    }
    assert!(shown(&h, "Desk") && !shown(&h, "Line-in"));
}

/// The drawer has each control once: no name row, no second remove, and a
/// lone device has no balance, mute or remove of its own.
#[test]
fn the_drawer_shows_each_control_once() {
    use egui_kittest::kittest::Queryable;
    let h = drawer(mixer::Strip::Input("line-in".into()));
    for there in ["Device", "Jack", "Input gain", "+ Merge another device"] {
        assert_eq!(h.query_all_by_label(there).count(), 1, "{there}");
    }
    for gone in [
        "Name",
        "Remove input",
        "Remove",
        "Mix balance",
        "Remove this device",
        "Headset dial",
        "Scenes",
    ] {
        assert!(!shown(&h, gone), "{gone}");
    }
    // Two devices: a balance and a remove each.
    let h = drawer(mixer::Strip::Input("voice".into()));
    assert!(shown(&h, "Devices") && !shown(&h, "Device"));
    assert_eq!(h.query_all_by_label("Mix balance").count(), 2);
    assert_eq!(h.query_all_by_label("Remove this device").count(), 2);
    // A virtual input: what apps see, and the dial.
    let h = drawer(mixer::Strip::Input("game".into()));
    assert!(shown(&h, "Shown to apps as") && shown(&h, "hako Game"));
    assert!(shown(&h, "Headset dial") && !shown(&h, "Device"));
    // Outputs: the strip's Mono button is the only mono.
    let h = drawer(mixer::Strip::Output("A2".into()));
    assert!(shown(&h, "Device") && !shown(&h, "Mono") && !shown(&h, "Remove output"));
    let h = drawer(mixer::Strip::Output("B1".into()));
    assert!(shown(&h, "Shown to apps as") && shown(&h, "hako Mic"));
}

/// A merged device's balance goes back to 0 dB on a double-click.
#[test]
fn double_clicking_a_mix_balance_resets_it() {
    use egui_kittest::kittest::Queryable;
    let mut h = drawer(mixer::Strip::Input("voice".into()));
    // The second device's slider (-3 dB in the sample), a quarter along.
    let slider = h
        .get_all_by_role(egui::accesskit::Role::Slider)
        .nth(1)
        .and_then(|n| n.bounding_box())
        .expect("two mix balance sliders");
    let at = egui::pos2(
        (slider.x0 + (slider.x1 - slider.x0) * 0.25) as f32,
        ((slider.y0 + slider.y1) / 2.0) as f32,
    );
    h.input_mut().events.push(egui::Event::PointerMoved(at));
    h.run_steps(2);
    // Both clicks inside one frame: well within the double-click time.
    for pressed in [true, false, true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        });
    }
    h.run_steps(4);
    let ops = sent_ops(&mut h);
    assert_eq!(
        ops.last(),
        Some(&MixerOp::SetSourceTrim {
            input: "voice".into(),
            source: 1,
            trim_db: 0.0,
        }),
        "{ops:?}"
    );
}

/// An input takes at most `MAX_SOURCES` apps: past that, the add dialog's
/// unticked apps can't be picked, so Create never sends a refused input.
#[test]
fn the_add_dialog_stops_at_the_source_limit() {
    use egui_kittest::kittest::Queryable;
    use hako_core::mixer::{InputKind, MAX_SOURCES};
    let mut m = sample_mixer();
    m.status.apps = (1..=MAX_SOURCES + 1).map(|n| format!("App {n}")).collect();
    let mut app = mixer_app(Some(m));
    app.mixer.open_add_dialog(false, true);
    let mut h = drive(app);
    for n in 1..=MAX_SOURCES + 1 {
        click(&mut h, &format!("App {n}"));
    }
    assert!(h
        .get_by_label(&format!("App {}", MAX_SOURCES + 1))
        .is_disabled());
    h.get_all_by_label("Add input")
        .find(|n| n.role() == egui::accesskit::Role::Button)
        .expect("Create button")
        .click();
    h.run_steps(4);
    match sent_ops(&mut h).as_slice() {
        [MixerOp::PutInput { input }] => {
            let InputKind::Virtual { apps, .. } = &input.kind else {
                panic!("{input:?}");
            };
            assert_eq!(apps.len(), MAX_SOURCES);
        }
        other => panic!("{other:?}"),
    }
}

/// The TKL's macro key selected, with an earlier take still in `recorded`.
fn record_harness() -> Driven {
    let (st, id) = tkl(DeviceRemaps {
        recorded: steps(&[30]),
        ..remaps(DeviceKind::Keyboard)
    });
    let app = bind_app(&egui::Context::default(), &st, &id, 34);
    let mut h = drive(app);
    h.ctx.data_mut(|m| {
        m.insert_temp(egui::Id::new("tab"), Tab::Bindings);
        m.insert_temp(egui::Id::new(("bind_selected", id.key())), 34_u16);
    });
    h.run_steps(4);
    h
}

/// The daemon reports a capture state and take.
fn set_record(h: &mut Driven, capturing: Option<CaptureRole>, recorded: Vec<MacroStep>) {
    let r = &mut h.state_mut().0.state.as_mut().unwrap().remaps[0];
    r.capturing = capturing;
    r.recorded = recorded;
    h.run_steps(4);
}

/// Right after Record, `recorded` still holds the previous take: it is not
/// adopted; the take of the capture the daemon then runs is.
#[test]
fn record_adopts_only_the_new_take() {
    let mut h = record_harness();
    click(&mut h, "     Record");
    assert!(matches!(
        sent(&mut h).as_slice(),
        [UiCommand::StartCapture { .. }]
    ));
    h.run_steps(4);
    assert!(!shown(&h, "Unsaved changes"));
    set_record(&mut h, Some(CaptureRole::Record), Vec::new());
    assert!(!shown(&h, "Unsaved changes"));
    set_record(&mut h, None, steps(&[17]));
    assert!(shown(&h, "Unsaved changes"));
}

/// A recorded take the daemon would refuse (BTN_MISC can't be emitted) is not
/// shown as saved: nothing is sent, the changes stay unsaved, and why is shown.
#[test]
fn a_macro_the_daemon_would_refuse_stays_unsaved() {
    let mut h = record_harness();
    click(&mut h, "     Record");
    assert!(matches!(
        sent(&mut h).as_slice(),
        [UiCommand::StartCapture { .. }]
    ));
    set_record(&mut h, Some(CaptureRole::Record), Vec::new());
    set_record(&mut h, None, steps(&[0x100]));
    assert!(shown(&h, "Unsaved changes"));
    click(&mut h, "Save macro");
    assert!(sent(&mut h).is_empty());
    assert!(shown(&h, "Unsaved changes"));
    use egui_kittest::kittest::Queryable;
    assert!(h
        .query_all_by_label_contains("emittable key")
        .next()
        .is_some());
}

/// The profile menu: update the current profile under its own name, and
/// delete one only after the question.
#[test]
fn the_profile_menu_updates_and_deletes_with_confirmation() {
    let mut st = sample_state();
    st.profiles = vec!["Default".into(), "FPS".into(), "Late night".into()];
    st.last_profile = Some("FPS".into());
    let mut h = drive(HakoAppUi {
        state: Some(st),
        daemon_error: None,
        ..Default::default()
    });
    let open = |h: &mut Driven| {
        h.ctx
            .memory_mut(|m| m.open_popup(egui::Id::new("profile_popup")));
        h.run_steps(3);
    };
    open(&mut h);
    assert!(shown(
        &h,
        "Profiles save device settings and the mixer layout."
    ));
    click(&mut h, "Update “FPS”");
    assert!(matches!(sent(&mut h).as_slice(), [UiCommand::SaveProfile(n)] if n == "FPS"));
    open(&mut h);
    click(&mut h, "Delete Late night");
    assert!(shown(&h, "Delete profile “Late night”?"));
    assert!(sent(&mut h).is_empty());
    click(&mut h, "Cancel");
    assert!(sent(&mut h).is_empty());
    open(&mut h);
    click(&mut h, "Delete Late night");
    click(&mut h, "Delete");
    assert!(matches!(sent(&mut h).as_slice(), [UiCommand::DeleteProfile(n)] if n == "Late night"));
    assert!(!shown(&h, "Delete profile “Late night”?"));
    // No profile selected: nothing to update.
    h.state_mut().0.state.as_mut().unwrap().last_profile = None;
    open(&mut h);
    assert!(!shown(&h, "Update “FPS”"));
}

/// With the mixer on, the headset's ChatMix card points at the mixer
/// instead of naming sinks that are no longer the way in.
#[test]
fn the_chatmix_card_follows_the_mixer() {
    let headset = |mixer: Option<hako_core::mixer::MixerState>| {
        let mut app = mixer_app(mixer);
        app.show_mixer = false;
        app.selected = Some(sample_state().devices[2].id.clone());
        drive(app)
    };
    let mut off = sample_mixer();
    off.file.enabled = false;
    for mixer in [None, Some(off)] {
        let h = headset(mixer);
        assert!(shown(&h, controls::chatmix_hint(false)));
        assert!(!shown(&h, "Open Mixer"));
    }
    assert!(controls::chatmix_hint(false).contains("Hako_Game"));
    assert!(!controls::chatmix_hint(true).contains("Hako_"));
    let mut h = headset(Some(sample_mixer()));
    assert!(shown(&h, controls::chatmix_hint(true)));
    click(&mut h, "Open Mixer");
    let (app, out) = h.state();
    assert!(app.show_mixer);
    assert!(matches!(out.as_slice(), [UiCommand::GetMixer]), "{out:?}");
}

// ── README screenshots ───────────────────────────────────────────

/// Bottom of the page content: the last row right of the rail that differs
/// from the page background, plus a margin.
fn content_height(img: &image::RgbaImage) -> u32 {
    let (w, h) = img.dimensions();
    let bg = *img.get_pixel(w - 4, h - 4);
    let last = (0..h)
        .rev()
        .find(|&y| (240..w - 4).any(|x| *img.get_pixel(x, y) != bg))
        .unwrap_or(h);
    (last + 28).min(h)
}

/// The README screenshots, rendered from a live daemon: point `HAKO_UI_STATE`,
/// `HAKO_UI_CONFIG`, `HAKO_UI_MIXER` and `HAKO_UI_NET` at dumps of `GetState` /
/// `GetConfig` / `GetMixer` / `GetNet` and run `cargo test -p hako-ui snapshot_readme -- --ignored`.
/// Writes `docs/screenshots/`.
#[test]
#[ignore = "needs a GPU (wgpu) and a live daemon dump"]
fn snapshot_readme() {
    let read = |var: &str| {
        let path = std::env::var(var).unwrap_or_else(|_| panic!("set {var}"));
        std::fs::read_to_string(path).unwrap()
    };
    let mut state: HubState = serde_json::from_str(&read("HAKO_UI_STATE")).unwrap();
    let settings: AppSettings = serde_json::from_str(&read("HAKO_UI_CONFIG")).unwrap();
    let mut mixer_state: hako_core::mixer::MixerState =
        serde_json::from_str(&read("HAKO_UI_MIXER")).unwrap();
    let net_state: hako_core::net::NetState = serde_json::from_str(&read("HAKO_UI_NET")).unwrap();
    // Staged where a live setup is sparse: sample bindings on the keyboard,
    // no leftover device warnings, every mixer stage on.
    for d in &mut state.devices {
        d.last_error = None;
    }
    state.audio.last_error = None;
    for i in &mut mixer_state.file.live.inputs {
        for e in &mut i.chain.effects {
            e.enabled = true;
        }
    }
    let find = |k: DeviceKind| {
        state
            .devices
            .iter()
            .find(|d| d.kind == k)
            .cloned()
            .unwrap_or_else(|| panic!("no {k:?} in the dump"))
    };
    let (kb, mouse, headset) = (
        find(DeviceKind::Keyboard),
        find(DeviceKind::Mouse),
        find(DeviceKind::Headset),
    );
    state
        .remaps
        .retain(|r| r.device_id.as_ref() != Some(&kb.id));
    state.remaps.push(DeviceRemaps {
        device_id: Some(kb.id.clone()),
        bindings: sample_bindings(DeviceKind::Keyboard),
        grabbed: true,
        ..Default::default()
    });

    let page = |d: &DeviceSnapshot, tab: Tab, selected: u16, theme: &str, height: f32| {
        let key = d.id.key();
        let mut app = HakoAppUi {
            state: Some(state.clone()),
            selected: Some(d.id.clone()),
            daemon_error: None,
            settings: Some(AppSettings {
                theme: theme.into(),
                ..settings.clone()
            }),
            ..Default::default()
        };
        app.mixer.receive(mixer_state.clone());
        app.net.receive(net_state.clone());
        render_app(
            [1180.0, height],
            app,
            move |ctx| {
                ctx.data_mut(|m| {
                    m.insert_temp(egui::Id::new("tab"), tab);
                    m.insert_temp(egui::Id::new(("bind_selected", key)), selected);
                })
            },
            |_| {},
        )
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
    let save = |name: &str, img: image::RgbaImage, height: Option<u32>| {
        let h = height.unwrap_or_else(|| content_height(&img));
        let img = image::imageops::crop_imm(&img, 0, 0, img.width(), h).to_image();
        image::DynamicImage::ImageRgba8(img)
            .to_rgb8()
            .save(dir.join(format!("{name}.png")))
            .unwrap();
    };
    save("hero", page(&kb, Tab::Bindings, 57, "onyx", 1700.0), None);
    save("lighting", page(&kb, Tab::Lighting, 0, "onyx", 900.0), None);
    save(
        "mouse-performance",
        page(&mouse, Tab::Performance, 0, "onyx", 900.0),
        None,
    );
    save(
        "mouse-buttons",
        page(&mouse, Tab::Bindings, 0, "onyx", 900.0),
        Some(712),
    );
    save(
        "chatmix",
        page(&headset, Tab::Audio, 0, "onyx", 900.0),
        None,
    );

    // The mixer with line-in's drawer open. A dump has no audio, so the
    // meters are staged.
    let console = || {
        let strips = mixer_state.file.live.inputs.len() + mixer_state.file.live.outputs.len();
        let mut app = HakoAppUi {
            state: Some(state.clone()),
            daemon_error: None,
            settings: Some(AppSettings {
                theme: "onyx".into(),
                ..settings.clone()
            }),
            show_mixer: true,
            meter_levels: [
                [0.4, 0.36],
                [0.22, 0.2],
                [0.05, 0.06],
                [0.55, 0.5],
                [0.16, 0.14],
            ]
            .into_iter()
            .chain([[0.6, 0.56], [0.24, 0.22]])
            .cycle()
            .take(strips)
            .collect(),
            ..Default::default()
        };
        app.mixer.receive(mixer_state.clone());
        app.net.receive(net_state.clone());
        app.mixer.select(mixer::Strip::Input("line-in".into()));
        app
    };
    save(
        "mixer",
        render_app([1180.0, 1400.0], console(), |_| {}, |_| {}),
        None,
    );
    // The same drawer's processing chain with every stage open.
    let top = std::cell::Cell::new(0);
    let chain = render_app(
        [1180.0, 2600.0],
        console(),
        |_| {},
        |h| {
            use egui_kittest::kittest::Queryable;
            loop {
                let Some(edit) = h.query_all_by_label("Edit").next() else {
                    break;
                };
                edit.click();
                h.run_steps(12);
            }
            let title = h.get_by_label("Processing chain").bounding_box().unwrap();
            top.set(title.y0 as u32 - 34);
        },
    );
    let chain = image::imageops::crop_imm(&chain, 248, top.get(), 908, chain.height() - top.get())
        .to_image();
    save("mixer-chain", chain, None);

    // The same page in every theme, half size, two by two.
    let (w, h, gap) = (590, 350, 10);
    let mut grid =
        image::RgbaImage::from_pixel(w * 2 + gap, h * 2 + gap, image::Rgba([5, 5, 6, 255]));
    for (i, t) in theme::THEMES.iter().enumerate() {
        let shot = page(&kb, Tab::Lighting, 0, t.id, 700.0);
        let small = image::imageops::resize(&shot, w, h, image::imageops::FilterType::Lanczos3);
        let (x, y) = ((i as u32 % 2) * (w + gap), (i as u32 / 2) * (h + gap));
        image::imageops::overlay(&mut grid, &small, x.into(), y.into());
    }
    save("themes", grid, Some(h * 2 + gap));
}

/// While hakod is unreachable, controls acting on its stale state send nothing.
#[test]
fn controls_are_disabled_while_the_daemon_is_offline() {
    let mut h = drive(HakoAppUi {
        state: Some(sample_state()),
        daemon_error: Some("Disconnected from hakod. Reconnecting…".into()),
        settings: Some(AppSettings::default()),
        show_settings: true,
        ..Default::default()
    });
    click(&mut h, "Reset");
    assert!(sent(&mut h).is_empty());
    h.state_mut().0.daemon_error = None;
    h.run_steps(2);
    click(&mut h, "Reset");
    assert!(matches!(sent(&mut h).as_slice(), [UiCommand::ResetAudio]));
}

/// A range no slider can show (min > max, NaN) renders a placeholder
/// instead of panicking in `clamp`.
#[test]
fn invalid_slider_ranges_do_not_panic() {
    use SettingKey::*;
    assert_eq!(
        controls::range(&SettingMeta::slider(Volume, "V", 0.0, 10.0, 1.0), 0.0, 1.0),
        Some((0.0, 10.0))
    );
    assert_eq!(
        controls::range(&SettingMeta::slider(Volume, "V", 5.0, 1.0, 1.0), 0.0, 1.0),
        None
    );
    assert_eq!(
        controls::range(
            &SettingMeta::slider(Volume, "V", f64::NAN, 1.0, 1.0),
            0.0,
            1.0
        ),
        None
    );
    let d = dev(
        DeviceKind::Mouse,
        vec![
            SettingMeta::slider(Dpi, "DPI", 26000.0, 100.0, 50.0),
            SettingMeta::slider(LiftOffDistance, "Lift-off", 3.0, f64::NAN, 1.0),
        ],
        map(&[(Dpi, SettingValue::Int(800))]),
    );
    let ctx = egui::Context::default();
    let mut app = HakoAppUi {
        state: Some(HubState {
            devices: vec![d.clone()],
            ..sample_state()
        }),
        selected: Some(d.id.clone()),
        daemon_error: None,
        ..Default::default()
    };
    let mut out = Vec::new();
    for _ in 0..2 {
        let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
    }
    assert!(out.is_empty(), "{out:?}");
}

/// A remapper problem (e.g. no permission to grab) shows on the page even
/// with no key selected.
#[test]
fn remap_errors_show_without_a_selected_key() {
    let err = "cannot grab /dev/input/event5: permission denied";
    let (st, id) = tkl(DeviceRemaps {
        last_error: Some(err.into()),
        ..remaps(DeviceKind::Keyboard)
    });
    let ctx = egui::Context::default();
    let app = bind_app(&ctx, &st, &id, 0);
    let mut h = egui_kittest::Harness::builder()
        .with_size([1200.0, 1400.0])
        .build_state(
            |ctx, (app, out): &mut (HakoAppUi, Vec<UiCommand>)| app.ui(ctx, out),
            (app, Vec::new()),
        );
    h.ctx.data_mut(|m| {
        m.insert_temp(egui::Id::new("tab"), Tab::Bindings);
        m.insert_temp(egui::Id::new(("bind_selected", id.key())), 0u16);
    });
    h.run_steps(4);
    assert!(shown(&h, "Nothing selected"));
    assert!(shown(&h, err));
}

#[test]
fn a_burst_of_toasts_keeps_the_errors() {
    let toast = |tone, text: &str| widgets::Toast {
        tone,
        text: text.into(),
        shown: None,
    };
    let mut list = vec![toast(widgets::Tone::Error, "failed")];
    list.extend((0..5).map(|i| toast(widgets::Tone::Ok, &format!("ok {i}"))));
    widgets::trim_toasts(&mut list);
    let texts: Vec<&str> = list.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, ["failed", "ok 2", "ok 3", "ok 4"]);
    // Only errors: the oldest goes.
    let mut list: Vec<_> = (0..5)
        .map(|i| toast(widgets::Tone::Error, &format!("e{i}")))
        .collect();
    widgets::trim_toasts(&mut list);
    assert_eq!(list[0].text, "e1");
}

// ── Voice and hotkeys ────────────────────────────────────────────

fn sample_voice() -> hako_core::voice::VoiceState {
    use hako_core::voice::*;
    let mut file = VoiceFile {
        enabled: true,
        model: Some(ModelChoice::Catalog {
            id: CATALOG.models[0].id.clone(),
            quant: CATALOG.models[0].default_quant().into(),
        }),
        ..Default::default()
    };
    file.text.replacements.push(Replacement {
        from: "new line".into(),
        to: "\n".into(),
    });
    let m1 = &CATALOG.models[1];
    VoiceState {
        file,
        engine: EngineStatus::Idle,
        loaded: Some(ModelCaps {
            backend: "CPU".into(),
            ..Default::default()
        }),
        installed: vec![
            InstalledModel {
                id: CATALOG.models[0].id.clone(),
                quant: CATALOG.models[0].default_quant().into(),
                file: "a.gguf".into(),
                size: 1,
                status: ModelStatus::Ready,
            },
            InstalledModel {
                id: m1.id.clone(),
                quant: m1.default_quant().into(),
                file: "b.gguf".into(),
                size: 1,
                status: ModelStatus::Corrupt {
                    reason: "hash mismatch".into(),
                },
            },
        ],
        custom: vec![CustomModel {
            file: "mine.gguf".into(),
            size: 123_000_000,
        }],
        vad_ready: false,
        history: vec![HistoryEntry {
            at: 1,
            text: "hello world".into(),
            model: "m".into(),
            audio: None,
        }],
        outputs: vec![OutputCheck {
            method: OutputMethod::Paste,
            ok: false,
            reason: Some("no clipboard".into()),
        }],
        accelerators: vec!["CPU".into(), "Vulkan0".into()],
        layout: Some("us".into()),
        sources: vec![("alsa_input.usb".into(), "USB mic".into())],
        wake_model: None,
        jarvis_listening: false,
        last_command: None,
        problems: vec!["example problem".into()],
        load_error: None,
    }
}

fn voice_app(voice: Option<hako_core::voice::VoiceState>) -> HakoAppUi {
    let mut app = HakoAppUi {
        state: Some(sample_state()),
        daemon_error: None,
        show_voice: true,
        settings: Some(AppSettings::default()),
        ..Default::default()
    };
    if let Some(v) = voice {
        app.voice.receive(v);
    }
    app.hotkeys
        .receive(hako_core::hotkey::HotkeysState::default());
    app
}

#[test]
fn voice_page_renders_every_state() {
    use hako_core::voice::{EngineStatus, ModelStatus, VoicePartial};
    let ctx = egui::Context::default();
    let mut out = Vec::new();
    // Every tab, wide and narrow.
    let mut render = |app: &mut HakoAppUi| {
        for tab in voice::VoiceTab::ALL {
            app.voice.tab = tab;
            for input in [input(), input(), {
                let mut narrow = input();
                narrow.screen_rect = Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(900.0, 600.0),
                ));
                narrow
            }] {
                let _ = ctx.run(input, |ctx| app.ui(ctx, &mut out));
            }
        }
        assert!(out.is_empty(), "rendering sent {out:?}");
    };
    // Loading, an older hakod, off.
    render(&mut voice_app(None));
    let mut app = voice_app(None);
    app.voice.unavailable("Voice needs a newer hakod.".into());
    render(&mut app);
    let mut off = sample_voice();
    off.file.enabled = false;
    off.engine = EngineStatus::Off;
    render(&mut voice_app(Some(off)));
    // No model, downloading, idle, recording with a partial, an error.
    let mut none = sample_voice();
    none.file.model = None;
    none.engine = EngineStatus::NoModel;
    render(&mut voice_app(Some(none)));
    let mut downloading = sample_voice();
    downloading.installed[0].status = ModelStatus::Downloading {
        bytes: 10,
        total: 100,
    };
    let mut app = voice_app(Some(downloading));
    render(&mut app);
    assert_eq!(app.voice.rail_status().0, "Downloading 10%");
    render(&mut voice_app(Some(sample_voice())));
    let mut rec = sample_voice();
    rec.engine = EngineStatus::Recording;
    let mut app = voice_app(Some(rec));
    app.voice.partial(VoicePartial {
        session: 1,
        committed: "hello".into(),
        tentative: "wor".into(),
    });
    render(&mut app);
    let mut err = sample_voice();
    err.engine = EngineStatus::Error {
        message: "worker crashed".into(),
    };
    err.load_error = Some("parse error".into());
    render(&mut voice_app(Some(err)));
    // Hotkeys unavailable while Voice works.
    let mut app = voice_app(Some(sample_voice()));
    app.hotkeys
        .unavailable("Hotkeys need a newer hakod.".into());
    render(&mut app);
}

#[test]
fn voice_settings_are_split_into_tabs() {
    use hako_core::hotkey::HotkeyAction;
    use hako_core::voice::EngineStatus;
    let mut h = drive(voice_app(Some(sample_voice())));
    // Dictation first: hotkeys with their options folded into a summary.
    assert!(shown(&h, "Microphone"));
    assert!(!shown(&h, "Recognition"));
    assert!(!shown(&h, "Pass-through"));
    assert!(shown(&h, "Smart · Listen · All keyboards and mice"));
    {
        use egui_kittest::kittest::Queryable;
        h.query_all_by_label("Show options").next().unwrap().click();
        h.run_steps(4);
    }
    assert!(shown(&h, "Pass-through"));
    click(&mut h, "Model");
    assert!(shown(&h, "Recognition"));
    assert!(!shown(&h, "Microphone"));
    click(&mut h, "Jarvis");
    assert!(shown(&h, "Push-to-command"));
    assert!(sent(&mut h).is_empty(), "switching tabs sends nothing");

    // No model: the status card leads to the Model tab.
    let mut none = sample_voice();
    none.file.model = None;
    none.engine = EngineStatus::NoModel;
    let mut h = drive(voice_app(Some(none)));
    click(&mut h, "Choose a model");
    assert_eq!(h.state().0.voice.tab, voice::VoiceTab::Model);
    assert!(!shown(&h, "Choose a model"), "not offered on its own tab");

    // A Bindings page's "Open …" link lands on the tab with that hotkey.
    assert_eq!(
        voice::VoiceTab::of(HotkeyAction::JarvisListen),
        voice::VoiceTab::Jarvis
    );
    assert_eq!(
        voice::VoiceTab::of(HotkeyAction::VoiceCancel),
        voice::VoiceTab::Dictation
    );
}

#[test]
fn an_older_hakod_is_explained_on_the_voice_page() {
    let mut app = voice_app(None);
    app.voice.unavailable("Voice needs a newer hakod.".into());
    let h = drive(app);
    assert!(shown(&h, "Voice needs a newer hakod."));
}

#[test]
fn voice_turns_on_from_the_off_card() {
    use hako_core::voice::VoiceOp;
    let mut off = sample_voice();
    off.file.enabled = false;
    let mut h = drive(voice_app(Some(off)));
    click(&mut h, "Turn on");
    let sent = sent(&mut h);
    assert!(
        matches!(sent.as_slice(), [UiCommand::PatchVoice(ops)] if ops == &[VoiceOp::SetEnabled { on: true }]),
        "{sent:?}"
    );
}

#[test]
fn voice_page_meters_the_picked_microphone_only_while_visible() {
    let ctx = egui::Context::default();
    let mut app = voice_app(Some(sample_voice()));
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut Vec::new()));
    assert_eq!(
        app.meter_targets,
        [MeterTargetSpec::Node {
            name: hako_core::voice::CAPTURE_NODE.into(),
            monitor_sink: false
        }]
    );
    app.show_voice = false;
    let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut Vec::new()));
    assert!(app.meter_targets.is_empty());
}

#[test]
fn catalog_filters_and_sorts() {
    use hako_core::voice::CATALOG;
    use std::collections::HashMap;
    use voice::{filter_models, CatalogFilter, SortBy};
    let models = &CATALOG.models;
    let quants = HashMap::new();
    let all = filter_models(models, &CatalogFilter::default(), &quants);
    assert_eq!(all.len(), models.len());
    let size = |m: &hako_core::voice::CatalogModel| m.file(m.default_quant()).unwrap().size;
    assert!(
        all.windows(2).all(|w| size(w[0]) <= size(w[1])),
        "smallest first"
    );

    let f = CatalogFilter {
        language: Some("de".into()),
        streaming_only: true,
        max_size: Some(1_000_000_000),
        sort: SortBy::Name,
    };
    let some = filter_models(models, &f, &quants);
    assert!(some.len() < all.len());
    assert!(some
        .iter()
        .all(|m| m.streaming && m.languages.iter().any(|l| l == "de") && size(m) <= 1_000_000_000));
    let names: Vec<String> = some.iter().map(|m| m.name.to_lowercase()).collect();
    assert!(names.windows(2).all(|w| w[0] <= w[1]));

    let acc = filter_models(
        models,
        &CatalogFilter {
            sort: SortBy::Accuracy,
            ..Default::default()
        },
        &quants,
    );
    let wer = |m: &hako_core::voice::CatalogModel| {
        m.wer_en
            .get(m.default_quant())
            .or(m.wer_en.values().next())
            .copied()
            .unwrap_or(f64::INFINITY)
    };
    assert!(
        acc.windows(2).all(|w| wer(w[0]) <= wer(w[1])),
        "unmeasured last"
    );

    // A picked quant decides the size the filter sees.
    let big = all.last().unwrap();
    let small_quant = big.files.iter().min_by_key(|f| f.size).unwrap();
    let mut quants = HashMap::new();
    quants.insert(big.id.clone(), small_quant.quant.clone());
    let f = CatalogFilter {
        max_size: Some(small_quant.size),
        ..Default::default()
    };
    assert!(filter_models(models, &f, &quants)
        .iter()
        .any(|m| m.id == big.id));

    assert_eq!(voice::fmt_params(600_000_000), "0.6B");
    assert_eq!(voice::fmt_params(44_000_000), "44M");
    assert_eq!(voice::fmt_params(1_000_000_000), "1B");
    assert_eq!(voice::fmt_size(731_000_000), "731 MB");
    assert_eq!(voice::fmt_size(1_600_000_000), "1.6 GB");
    assert_eq!(voice::languages_label(&["en".into()]), "English");
    assert_eq!(
        voice::languages_label(&["en".into(), "de".into()]),
        "2 languages"
    );
}

#[test]
fn hotkey_edits_are_validated_before_they_are_sent() {
    use hako_core::hotkey::*;
    use hako_core::remap::EvdevKey;
    let mut hk = hotkeys::HotkeysUi::default();
    hk.receive(HotkeysState::default());
    hk.sync(false);
    let mut out = Vec::new();
    // Esc alone is for cancel only: refused, nothing sent, the reason kept.
    let mut b = hk.file().unwrap().bindings[0].clone();
    b.keys = vec![EvdevKey(KEY_ESC)];
    assert!(!hk.send("dictate", vec![HotkeyOp::Set { binding: b }], &mut out));
    assert!(out.is_empty());

    // Detect for a new binding: a chord already in use is refused...
    let new = HotkeyBinding::new("dictate-2", HotkeyAction::VoiceTranscribeAlt, &[]);
    hk.detect(new.clone(), &mut out);
    assert!(matches!(out.as_slice(), [UiCommand::StartHotkeyCapture]));
    out.clear();
    hk.capture_update(HotkeyCaptureState {
        held: vec![],
        chord: vec![EvdevKey(KEY_RIGHTCTRL), EvdevKey(KEY_SPACE)],
        done: true,
        cancelled: false,
    });
    hk.frame(&mut out);
    assert!(out.is_empty(), "{out:?}");
    assert!(hk.detecting().is_none());
    // ... a free one is added, locally at once.
    hk.detect(new, &mut out);
    out.clear();
    hk.capture_update(HotkeyCaptureState {
        chord: vec![EvdevKey(183)],
        done: true,
        ..Default::default()
    });
    hk.frame(&mut out);
    let [UiCommand::PatchHotkeys(ops)] = out.as_slice() else {
        panic!("{out:?}");
    };
    assert!(matches!(&ops[..], [HotkeyOp::Set { binding }] if binding.keys == [EvdevKey(183)]));
    assert_eq!(hk.file().unwrap().bindings.len(), 3);
    // A cancelled capture changes nothing.
    out.clear();
    hk.detect(hk.file().unwrap().bindings[0].clone(), &mut out);
    out.clear();
    hk.capture_update(HotkeyCaptureState {
        done: true,
        cancelled: true,
        ..Default::default()
    });
    hk.frame(&mut out);
    assert!(out.is_empty());
    assert_eq!(
        hotkeys::new_id(hk.file().unwrap(), HotkeyAction::VoiceTranscribe),
        "dictate-3"
    );
    assert_eq!(
        hotkeys::cap_label(EvdevKey(KEY_RIGHTCTRL), false),
        hotkeys::cap_label(EvdevKey(KEY_LEFTCTRL), false)
    );
}

#[test]
fn bindings_pages_mark_the_keys_of_hotkeys_covering_the_device() {
    use hako_core::hotkey::*;
    let mut file = HotkeysFile::default();
    // Ctrl + Space on every device; F13 only on another one.
    let mut other = HotkeyBinding::new("dictate-alt", HotkeyAction::VoiceTranscribeAlt, &[183]);
    other.scope = HotkeyScope::Device {
        vendor_id: 9,
        product_id: 9,
    };
    file.bindings.push(other);
    let marks = hotkeys::key_marks(&file, 1, 1);
    let label = |code: u16| {
        marks
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, m)| m.label.as_str())
    };
    assert_eq!(label(KEY_SPACE), Some("Voice"));
    assert_eq!(label(KEY_LEFTCTRL), Some("+Voice"));
    assert_eq!(label(KEY_RIGHTCTRL), Some("+Voice"), "either side");
    assert_eq!(label(183), None, "scoped to another device");
    assert!(marks
        .iter()
        .find(|(c, _)| *c == KEY_SPACE)
        .is_some_and(|(_, m)| m.summary == "Voice: Dictate (Smart)"));
    // A device no hotkey covers gets no marks.
    file.bindings.retain(|b| b.id == "dictate-alt");
    assert!(hotkeys::key_marks(&file, 1, 1).is_empty());
    assert_eq!(hotkeys::key_marks(&file, 9, 9).len(), 1);

    // The keyboard and mouse pages render them, the Hotkey tile included,
    // without sending anything.
    for (kind, key) in [
        (DeviceKind::Keyboard, KEY_SPACE),
        (DeviceKind::Mouse, 0x113),
    ] {
        let (st, id) = bind_state(kind, "Test", DeviceRemaps::default());
        let ctx = egui::Context::default();
        let mut app = bind_app(&ctx, &st, &id, key);
        let mut hk = HotkeysFile::default();
        hk.bindings.push(HotkeyBinding::new(
            "side",
            HotkeyAction::VoiceTranscribeAlt,
            &[0x113],
        ));
        app.hotkeys.receive(HotkeysState {
            file: hk,
            ..Default::default()
        });
        ctx.data_mut(|m| {
            m.insert_temp(
                egui::Id::new(bindings::HIGHLIGHT_ID),
                ("dictate".to_string(), 0.0),
            )
        });
        let mut out = Vec::new();
        for _ in 0..3 {
            let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
        }
        assert!(out.is_empty(), "{kind:?}: {out:?}");
    }
}

#[test]
fn a_hotkey_conflict_offers_three_answers() {
    use hako_core::hotkey::*;
    use hako_core::remap::EvdevKey;
    let (st, id) = bind_state(DeviceKind::Keyboard, "Test", DeviceRemaps::default());
    let ctx = egui::Context::default();
    let app = bind_app(&ctx, &st, &id, 0);
    let mut h = drive(app);
    h.state_mut().0.hotkeys.receive(HotkeysState {
        file: HotkeysFile::default(),
        status: vec![HotkeyStatus {
            id: "dictate".into(),
            source: HotkeySource::Evdev,
            active: true,
            devices: vec![],
            conflicts: vec![HotkeyConflict::Remap {
                device: "Test".into(),
                vendor_id: id.vendor_id,
                product_id: id.product_id,
                key: EvdevKey(KEY_SPACE),
            }],
            problems: vec![],
        }],
        ..Default::default()
    });
    h.run_steps(4);
    click(&mut h, "Both");
    let sent = sent(&mut h);
    assert!(
        matches!(sent.as_slice(), [UiCommand::PatchHotkeys(ops)]
            if matches!(&ops[..], [HotkeyOp::Set { binding }] if binding.with_remaps)),
        "{sent:?}"
    );
}

// ── Jarvis ───────────────────────────────────────────────────────

/// The whole UI on the Voice page's Jarvis tab, in a window tall enough for
/// all of it.
fn drive_voice(mut app: HakoAppUi) -> Driven {
    app.voice.tab = voice::VoiceTab::Jarvis;
    let mut h = egui_kittest::Harness::builder()
        .with_size([1200.0, 5000.0])
        .build_state(
            |ctx, (app, out): &mut (HakoAppUi, Vec<UiCommand>)| app.ui(ctx, out),
            (app, Vec::new()),
        );
    h.run_steps(4);
    h
}

/// Voice ready for Jarvis: the dictation model and the wake model are ready.
fn jarvis_voice() -> hako_core::voice::VoiceState {
    let mut v = sample_voice();
    v.wake_model = Some(hako_core::voice::ModelStatus::Ready);
    v
}

fn user_commands() -> Vec<hako_core::voice::UserCommand> {
    use hako_core::remap::EvdevKey;
    use hako_core::voice::{UserAction, UserCommand};
    let cmd = |say: &str, action| UserCommand {
        say: vec![say.into()],
        action,
        confirm: false,
    };
    vec![
        cmd(
            "browser {slot}",
            UserAction::Run {
                program: "/usr/bin/firefox".into(),
                args: vec!["{slot}".into()],
            },
        ),
        cmd(
            "paste",
            UserAction::Keys {
                keys: vec![EvdevKey(hako_core::hotkey::KEY_LEFTCTRL), EvdevKey(47)],
            },
        ),
        cmd(
            "sign off",
            UserAction::Type {
                text: "Best regards".into(),
            },
        ),
        cmd(
            "mail",
            UserAction::Open {
                target: "https://mail.example.com".into(),
            },
        ),
        UserCommand {
            confirm: true,
            ..cmd(
                "game mode",
                UserAction::Profile {
                    name: "Gaming".into(),
                },
            )
        },
    ]
}

#[test]
fn jarvis_section_renders_every_state() {
    use hako_core::hotkey::{HotkeyAction, HotkeyBinding, HotkeysFile, HotkeysState};
    use hako_core::voice::{CommandOutcome, ModelStatus};
    let ctx = egui::Context::default();
    let mut out = Vec::new();
    let mut render = |app: &mut HakoAppUi| {
        app.voice.tab = voice::VoiceTab::Jarvis;
        for _ in 0..3 {
            let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
        }
        assert!(out.is_empty(), "rendering sent {out:?}");
    };
    // Wake model missing, downloading, damaged, ready.
    for wake in [
        None,
        Some(ModelStatus::Downloading {
            bytes: 1_000_000,
            total: 3_000_000,
        }),
        Some(ModelStatus::Corrupt {
            reason: "hash mismatch".into(),
        }),
        Some(ModelStatus::Ready),
    ] {
        let mut v = sample_voice();
        v.wake_model = wake;
        render(&mut voice_app(Some(v)));
    }
    // Listening, with a command that worked and one that failed, the user's
    // commands, a push-to-command hotkey.
    for ok in [true, false] {
        let mut v = jarvis_voice();
        v.file.jarvis.enabled = true;
        v.jarvis_listening = ok;
        v.file.jarvis.commands = user_commands();
        v.last_command = Some(CommandOutcome {
            at: 0,
            heard: "open firefox".into(),
            action: ok.then(|| "open firefox".into()),
            ok,
            message: (!ok).then(|| "no such app".into()),
        });
        let mut app = voice_app(Some(v));
        let mut file = HotkeysFile::default();
        file.bindings.push(HotkeyBinding::new(
            "jarvis",
            HotkeyAction::JarvisListen,
            &[183],
        ));
        app.hotkeys.receive(HotkeysState {
            file,
            ..Default::default()
        });
        render(&mut app);
    }
    // The command editor open on every action kind.
    for kind in voice::CommandKind::ALL {
        let mut app = voice_app(Some(jarvis_voice()));
        app.voice.command = Some(voice::CommandDraft {
            kind,
            ..Default::default()
        });
        render(&mut app);
    }
}

#[test]
fn jarvis_shows_what_it_hears_and_did() {
    use hako_core::voice::CommandOutcome;
    let mut v = jarvis_voice();
    v.file.jarvis.enabled = true;
    v.jarvis_listening = true;
    v.last_command = Some(CommandOutcome {
        at: 0,
        heard: "open steam".into(),
        action: Some("open steam".into()),
        ok: false,
        message: Some("no such app".into()),
    });
    let h = drive_voice(voice_app(Some(v)));
    assert!(shown(&h, "Listening for « jarvis »"));
    assert!(shown(&h, "\u{201c}open steam\u{201d}"));
    assert!(shown(&h, "Failed"));
    assert!(shown(&h, "Understood as: open steam"));
    assert!(shown(&h, "no such app"));
    assert!(shown(&h, "Ready"), "wake model ready");

    assert_eq!(voice::ago(100, 95), "just now");
    assert_eq!(voice::ago(100, 50), "50 s ago");
    assert_eq!(voice::ago(1000, 400), "10 min ago");
    assert_eq!(voice::ago(7200, 0), "2 h ago");
    assert_eq!(voice::ago(100_000, 0), "1 day ago");
    assert_eq!(voice::ago(0, 5), "just now", "a clock behind the daemon's");
}

#[test]
fn jarvis_turns_on_only_once_both_models_are_ready() {
    use egui_kittest::kittest::Queryable;
    use hako_core::voice::{ModelStatus, VoiceOp};
    let mut none = jarvis_voice();
    none.file.model = None;
    assert_eq!(
        voice::jarvis_blocker(&none),
        Some("Needs a dictation model: download one under Model and click Use")
    );
    let mut no_wake = jarvis_voice();
    no_wake.wake_model = Some(ModelStatus::Downloading { bytes: 1, total: 2 });
    let why = "Download the wake-word model first";
    assert_eq!(voice::jarvis_blocker(&no_wake), Some(why));
    assert_eq!(voice::jarvis_blocker(&jarvis_voice()), None);

    // Disabled with the reason; the wake model can be downloaded.
    no_wake.wake_model = None;
    let mut h = drive_voice(voice_app(Some(no_wake)));
    assert!(shown(&h, why));
    fn toggle(h: &Driven) -> egui_kittest::kittest::Node<'_> {
        h.get_by_role_and_label(egui::accesskit::Role::CheckBox, "Listen for the wake word")
    }
    assert!(toggle(&h).is_disabled());
    let label = format!(
        "Download {}",
        voice::fmt_size(hako_core::voice::CATALOG.kws.size)
    );
    h.get_by_label(&label).click();
    h.run_steps(4);
    let download = sent(&mut h);
    assert!(
        matches!(download.as_slice(), [UiCommand::PatchVoice(ops)] if ops == &[VoiceOp::DownloadWake]),
        "{download:?}"
    );

    // Ready: on.
    let mut h = drive_voice(voice_app(Some(jarvis_voice())));
    toggle(&h).click();
    h.run_steps(4);
    let sent = sent(&mut h);
    assert!(
        matches!(sent.as_slice(), [UiCommand::PatchVoice(ops)]
            if matches!(&ops[..], [VoiceOp::SetJarvis { jarvis }] if jarvis.enabled)),
        "{sent:?}"
    );
}

#[test]
fn command_drafts_build_every_action_kind() {
    use hako_core::remap::EvdevKey;
    use hako_core::voice::{UserAction, VoiceFile};
    use voice::{save_command, CommandDraft, CommandKind};
    let f = VoiceFile::default();
    let draft = |kind| CommandDraft {
        phrases: "  do it \n\nnow {slot}\n".into(),
        kind,
        program: " /usr/bin/foot ".into(),
        args: "-e\n{slot}\n".into(),
        keys: vec![EvdevKey(hako_core::hotkey::KEY_LEFTCTRL), EvdevKey(47)],
        text: "hello\nworld".into(),
        target: "https://example.com".into(),
        profile: "Gaming".into(),
        confirm: true,
        ..Default::default()
    };
    for kind in CommandKind::ALL {
        let j = save_command(&f, &draft(kind)).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        let [c] = j.commands.as_slice() else {
            panic!("{kind:?}: {:?}", j.commands)
        };
        assert_eq!(c.say, ["do it", "now {slot}"]);
        assert!(c.confirm);
        let want = match kind {
            CommandKind::Run => UserAction::Run {
                program: "/usr/bin/foot".into(),
                args: vec!["-e".into(), "{slot}".into()],
            },
            CommandKind::Keys => UserAction::Keys {
                keys: vec![EvdevKey(hako_core::hotkey::KEY_LEFTCTRL), EvdevKey(47)],
            },
            CommandKind::Type => UserAction::Type {
                text: "hello\nworld".into(),
            },
            CommandKind::Open => UserAction::Open {
                target: "https://example.com".into(),
            },
            CommandKind::Profile => UserAction::Profile {
                name: "Gaming".into(),
            },
        };
        assert_eq!(c.action, want);
        // Editing it again gives the same command back.
        assert_eq!(CommandDraft::edit(0, c).build(), *c);
    }
    // Refused: no phrase, a relative program, no keys, nothing to type, to
    // open or to apply.
    let bad = [
        CommandDraft {
            phrases: " \n".into(),
            ..draft(CommandKind::Run)
        },
        CommandDraft {
            program: "foot".into(),
            ..draft(CommandKind::Run)
        },
        CommandDraft {
            keys: vec![],
            ..draft(CommandKind::Keys)
        },
        CommandDraft {
            text: String::new(),
            ..draft(CommandKind::Type)
        },
        CommandDraft {
            target: " ".into(),
            ..draft(CommandKind::Open)
        },
        CommandDraft {
            profile: String::new(),
            ..draft(CommandKind::Profile)
        },
    ];
    for d in bad {
        assert!(save_command(&f, &d).is_err(), "{d:?}");
    }
    // Editing replaces in place.
    let mut f = VoiceFile::default();
    f.jarvis.commands = user_commands();
    let d = CommandDraft {
        index: Some(1),
        ..draft(CommandKind::Type)
    };
    let j = save_command(&f, &d).unwrap();
    assert_eq!(j.commands.len(), 5);
    assert_eq!(j.commands[1].say, ["do it", "now {slot}"]);
    assert_eq!(j.commands[0], f.jarvis.commands[0]);
}

#[test]
fn the_command_editor_saves_valid_commands_and_explains_invalid_ones() {
    use hako_core::voice::{UserAction, VoiceOp};
    use voice::{CommandDraft, CommandKind};
    let mut app = voice_app(Some(jarvis_voice()));
    app.voice.command = Some(CommandDraft {
        phrases: "launch terminal".into(),
        kind: CommandKind::Run,
        program: "foot".into(),
        ..Default::default()
    });
    let mut h = drive_voice(app);
    click(&mut h, "Save command");
    assert!(sent(&mut h).is_empty());
    assert!(shown(
        &h,
        "Not saved: command: the program is an absolute path"
    ));

    h.state_mut().0.voice.command.as_mut().unwrap().program = "/usr/bin/foot".into();
    h.run_steps(2);
    click(&mut h, "Save command");
    let sent = sent(&mut h);
    let [UiCommand::PatchVoice(ops)] = sent.as_slice() else {
        panic!("{sent:?}")
    };
    let [VoiceOp::SetJarvis { jarvis }] = ops.as_slice() else {
        panic!("{ops:?}")
    };
    assert_eq!(jarvis.commands.len(), 1);
    assert_eq!(
        jarvis.commands[0].action,
        UserAction::Run {
            program: "/usr/bin/foot".into(),
            args: vec![]
        }
    );
    assert!(h.state().0.voice.command.is_none(), "editor closed");
    assert!(shown(&h, "Run /usr/bin/foot"), "listed");
}

#[test]
fn a_command_chord_is_recorded_with_detect() {
    use hako_core::hotkey::HotkeyCaptureState;
    use hako_core::remap::EvdevKey;
    use voice::{CommandDraft, CommandKind};
    let mut app = voice_app(Some(jarvis_voice()));
    app.voice.command = Some(CommandDraft {
        kind: CommandKind::Keys,
        ..Default::default()
    });
    let mut h = drive_voice(app);
    click(&mut h, "Detect keys…");
    let sent_now = sent(&mut h);
    assert!(
        matches!(sent_now.as_slice(), [UiCommand::StartHotkeyCapture]),
        "{sent_now:?}"
    );
    h.state_mut().0.hotkeys.capture_update(HotkeyCaptureState {
        chord: vec![EvdevKey(hako_core::hotkey::KEY_LEFTCTRL), EvdevKey(47)],
        done: true,
        ..Default::default()
    });
    h.run_steps(2);
    assert!(sent(&mut h).is_empty(), "no hotkey is changed");
    let d = h.state().0.voice.command.clone().unwrap();
    assert_eq!(
        d.keys,
        [EvdevKey(hako_core::hotkey::KEY_LEFTCTRL), EvdevKey(47)]
    );
}

#[test]
fn add_push_to_command_hotkey_detects_then_sets_a_jarvis_binding() {
    use hako_core::hotkey::{HotkeyAction, HotkeyCaptureState, HotkeyOp};
    use hako_core::remap::EvdevKey;
    let mut h = drive_voice(voice_app(Some(jarvis_voice())));
    click(&mut h, "Add push-to-command hotkey");
    let sent_now = sent(&mut h);
    assert!(
        matches!(sent_now.as_slice(), [UiCommand::StartHotkeyCapture]),
        "{sent_now:?}"
    );
    h.state_mut().0.hotkeys.capture_update(HotkeyCaptureState {
        chord: vec![EvdevKey(183)],
        done: true,
        ..Default::default()
    });
    h.run_steps(2);
    let sent = sent(&mut h);
    let [UiCommand::PatchHotkeys(ops)] = sent.as_slice() else {
        panic!("{sent:?}")
    };
    assert!(
        matches!(&ops[..], [HotkeyOp::Set { binding }]
            if binding.action == HotkeyAction::JarvisListen
                && binding.id == "jarvis"
                && binding.keys == [EvdevKey(183)]),
        "{ops:?}"
    );
    // Now it is listed with the editor, not offered again.
    h.run_steps(2);
    assert!(!shown(&h, "Add push-to-command hotkey"));
    assert!(shown(&h, "Jarvis: listen"));
}

#[test]
fn what_can_i_say_lists_every_action() {
    use hako_core::jarvis::ACTIONS;
    // Every action in exactly one group.
    let listed: Vec<&str> = voice::say_groups()
        .iter()
        .flat_map(|(_, specs)| specs.iter().map(|s| s.name))
        .collect();
    assert_eq!(listed.len(), ACTIONS.len());
    assert!(ACTIONS.iter().all(|a| listed.contains(&a.name)));

    let mut v = jarvis_voice();
    v.file.jarvis.commands = user_commands();
    let h = drive_voice(voice_app(Some(v.clone())));
    for a in ACTIONS {
        assert!(shown(&h, a.description), "{}", a.name);
    }
    assert!(shown(&h, "Apply profile Gaming"), "the user's own");
}

/// The Voice page off, each tab while recording, and a keyboard's Bindings
/// page with the default hotkey on it.
#[test]
#[ignore = "needs a GPU (wgpu)"]
fn snapshot_voice() {
    let mut off = sample_voice();
    off.file.enabled = false;
    shoot_app("voice_off", SHOT, voice_app(Some(off)), |_| {}, |_| {});
    for tab in voice::VoiceTab::ALL {
        let mut rec = jarvis_voice();
        rec.engine = hako_core::voice::EngineStatus::Recording;
        rec.file.jarvis.commands = user_commands();
        let mut app = voice_app(Some(rec));
        app.voice.tab = tab;
        app.voice.partial(hako_core::voice::VoicePartial {
            session: 1,
            committed: "Hello from the".into(),
            tentative: "keyboard".into(),
        });
        let name = format!("voice_{}", tab.label().to_lowercase());
        shoot_app(&name, [1180.0, 2600.0], app, |_| {}, |_| {});
    }
    let mut app = voice_app(Some(jarvis_voice()));
    app.voice.tab = voice::VoiceTab::Jarvis;
    shoot_app("voice_jarvis_narrow", [900.0, 2400.0], app, |_| {}, |_| {});
    let (st, id) = bind_state(
        DeviceKind::Keyboard,
        "Huntsman V2 TKL",
        DeviceRemaps::default(),
    );
    let mut app = bind_app(&egui::Context::default(), &st, &id, 57);
    app.hotkeys
        .receive(hako_core::hotkey::HotkeysState::default());
    shoot_app(
        "voice_bindings",
        [1180.0, 1400.0],
        app,
        |ctx| {
            ctx.data_mut(|m| {
                m.insert_temp(egui::Id::new(("bind_selected", id.key())), 57_u16);
            })
        },
        |_| {},
    );
}
