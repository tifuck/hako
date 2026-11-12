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
        profiles: vec!["Default".into()],
        audio: AudioSummary {
            chatmix_active: true,
            aux_active: true,
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
        for tab in 0..tabs_for(d).len() {
            let mut app = HakoAppUi {
                state: Some(state.clone()),
                selected: Some(d.id.clone()),
                daemon_error: None,
                settings: Some(settings_with_chain()),
                ..Default::default()
            };
            ctx.data_mut(|m| m.insert_temp(egui::Id::new(("tab", d.id.key())), tab));
            let mut out = Vec::new();
            for _ in 0..3 {
                let _ = ctx.run(input(), |ctx| app.ui(ctx, &mut out));
            }
            // Pure rendering must not emit device commands.
            assert!(
                out.iter()
                    .all(|c| !matches!(c, UiCommand::Set { .. } | UiCommand::SetRemap { .. })),
                "{:?} tab {tab} emitted {out:?}",
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
    for tab in 0..tabs_for(&d).len() {
        let mut app = HakoAppUi {
            state: Some(state.clone()),
            selected: Some(d.id.clone()),
            ..Default::default()
        };
        ctx.data_mut(|m| m.insert_temp(egui::Id::new(("tab", d.id.key())), tab));
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
            out.iter().all(|c| !matches!(c, UiCommand::Set { .. })),
            "tab {tab} emitted {out:?}"
        );
    }
}

#[test]
fn tabs_follow_the_device_kind_and_end_with_device() {
    use Tab::*;
    let st = sample_state();
    let tabs = |k| tabs_for(st.devices.iter().find(|d| d.kind == k).unwrap());
    assert_eq!(tabs(DeviceKind::Keyboard), [Lighting, Bindings, Device]);
    assert_eq!(
        tabs(DeviceKind::Mouse),
        [Performance, Bindings, Lighting, Device]
    );
    // Microphone folds into Audio, power into Device.
    assert_eq!(tabs(DeviceKind::Headset), [Audio, LineIn, Device]);
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
    let d = st.devices.iter().find(|d| &d.id == id).unwrap();
    ctx.data_mut(|m| {
        let bindings = tabs_for(d)
            .iter()
            .position(|t| *t == Tab::Bindings)
            .unwrap();
        m.insert_temp(egui::Id::new(("tab", id.key())), bindings);
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
    let image = harness.render().expect("offscreen render");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/audit");
    std::fs::create_dir_all(&dir).unwrap();
    image.save(dir.join(format!("{name}.png"))).unwrap();
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
        for (i, tab) in tabs.iter().enumerate() {
            let name = format!(
                "{:?}_{}",
                d.kind,
                tab.label(d.kind).to_lowercase().replace(' ', "_")
            )
            .to_lowercase();
            let tab_id = egui::Id::new(("tab", d.id.key()));
            for (suffix, size) in [("", SHOT), ("_full", [1180.0, 1500.0])] {
                shoot_app(
                    &format!("{name}{suffix}"),
                    size,
                    app(&state, Some(d.id.clone())),
                    |ctx| ctx.data_mut(|m| m.insert_temp(tab_id, i)),
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
        // The profile dropdown sits left of the settings gear in the top bar.
        |h| {
            pointer(h, [1000.0, 28.0], Some(true));
            pointer(h, [1000.0, 28.0], Some(false));
            h.run_steps(40);
        },
    );
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
            let last = tabs_for(d).len() - 1;
            let tab_id = egui::Id::new(("tab", d.id.key()));
            for (tab, label) in [(0, "main"), (last, "last")] {
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
    for (i, _) in tabs_for(&onboard).iter().enumerate() {
        let tab_id = egui::Id::new(("tab", onboard.id.key()));
        shoot_app(
            &format!("memory_gate_tab{i}"),
            SHOT,
            app(&mem, Some(onboard.id.clone())),
            |ctx| ctx.data_mut(|m| m.insert_temp(tab_id, i)),
            |_| {},
        );
    }
}

#[test]
fn dpi_snap_stays_inside_the_device_range() {
    use crate::controls::snap_dpi;
    // Top of a 100..25600 sensor no longer rounds to an out-of-range 26000.
    assert_eq!(snap_dpi(25600.0, 100.0, 25600.0, 50.0), 25600.0);
    // A 45000 DPI sensor is reachable; a 50 DPI minimum is selectable.
    assert_eq!(snap_dpi(45000.0, 100.0, 45000.0, 50.0), 45000.0);
    assert_eq!(snap_dpi(50.0, 50.0, 18000.0, 50.0), 50.0);
    assert_eq!(snap_dpi(1649.0, 100.0, 26000.0, 50.0), 1600.0);
}
