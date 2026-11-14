//! Key / button assignment: a device illustration to pick the input, an
//! assignment panel for what it does, and an action library to click or drag
//! from. Everything is committed through `SetRemap` / `ClearRemap`.

mod editor;
mod keyboard;
pub mod layouts;
mod library;
mod macros;
mod mouse;

use crate::theme::{self, pal};
use crate::widgets;
use crate::UiCommand;
use egui::{pos2, vec2, Color32, Rect, Sense, Stroke, Ui};
use hako_core::device::{DeviceKind, DeviceSnapshot};
use hako_core::remap::{CaptureRole, DeviceRemaps, EvdevKey, RemapBinding, RemapMode};
use layouts::Layout;
use library::Action;

/// What every part of the page needs: the device, its remaps, the command sink.
pub(crate) struct Cx<'a> {
    pub d: &'a DeviceSnapshot,
    pub remaps: &'a DeviceRemaps,
    pub out: &'a mut Vec<UiCommand>,
}

impl Cx<'_> {
    fn binding(&self, key: EvdevKey) -> Option<&RemapBinding> {
        self.remaps.bindings.iter().find(|b| b.source == key)
    }

    fn cancel_capture(&mut self, keep: bool) {
        self.out.push(UiCommand::CancelCapture {
            device_id: self.d.id.clone(),
            keep,
        });
    }

    fn set(&mut self, binding: RemapBinding) {
        self.out.push(UiCommand::SetRemap {
            device_id: self.d.id.clone(),
            binding,
        });
    }

    fn clear(&mut self, source: EvdevKey) {
        self.out.push(UiCommand::ClearRemap {
            device_id: self.d.id.clone(),
            source,
        });
    }
}

/// Primary click stays fixed so a bad binding can never lock the user out.
pub fn is_locked(key: EvdevKey) -> bool {
    key == EvdevKey::BTN_LEFT
}

/// Names for codes `hako_core::remap::key_label` does not know yet.
// ponytail: UI-side table; fold into hako-core's key_label when core is next touched.
pub fn key_name(key: EvdevKey) -> String {
    let extra = match key.0 {
        55 => "Num *",
        69 => "Num Lock",
        71 => "Num 7",
        72 => "Num 8",
        73 => "Num 9",
        74 => "Num -",
        75 => "Num 4",
        76 => "Num 5",
        77 => "Num 6",
        78 => "Num +",
        79 => "Num 1",
        80 => "Num 2",
        81 => "Num 3",
        82 => "Num 0",
        83 => "Num .",
        96 => "Num Enter",
        98 => "Num /",
        99 => "Print Screen",
        140 => "Calculator",
        144 => "File manager",
        155 => "Mail",
        158 => "Browser back",
        159 => "Browser forward",
        166 => "Stop",
        172 => "Browser home",
        173 => "Refresh",
        217 => "Search",
        224 => "Brightness down",
        225 => "Brightness up",
        248 => "Mic mute",
        c @ 183..=194 => return format!("F{}", c - 170),
        _ => "",
    };
    if !extra.is_empty() {
        return extra.into();
    }
    match key.label().as_str() {
        "Key" => format!("Key {}", key.0),
        l => l.into(),
    }
}

/// Name of an input on this device (mouse buttons are named by position).
fn source_name(cx: &Cx, key: EvdevKey) -> String {
    (cx.d.kind == DeviceKind::Mouse)
        .then(|| mouse::name(key))
        .flatten()
        .map_or_else(|| key_name(key), str::to_string)
}

pub fn mode_color(mode: RemapMode) -> Color32 {
    match mode {
        RemapMode::Tap => theme::pal().accent,
        RemapMode::Turbo | RemapMode::Toggle => theme::MODE_REPEAT,
        RemapMode::Macro => theme::MODE_MACRO,
        RemapMode::Disabled => theme::MODE_OFF,
    }
}

/// One-line description of what a button currently does.
pub fn describe(b: Option<&RemapBinding>) -> String {
    let Some(b) = b else {
        return "Default".into();
    };
    let target = key_name(b.target);
    match b.mode {
        RemapMode::Tap => format!("Sends {target}"),
        RemapMode::Turbo => format!("Hold-repeat {target} · {} ms", b.interval_ms),
        RemapMode::Toggle => format!("Toggle-repeat {target} · {} ms", b.interval_ms),
        RemapMode::Macro => format!("Macro · {} steps · {}", b.steps.len(), b.repeat.label()),
        RemapMode::Disabled => "Disabled".into(),
    }
}

/// A few characters for the legend printed on a bound keycap.
fn short(b: &RemapBinding) -> String {
    match b.mode {
        RemapMode::Tap => key_name(b.target),
        RemapMode::Turbo | RemapMode::Toggle => format!("{} ×", key_name(b.target)),
        RemapMode::Macro => "Macro".into(),
        RemapMode::Disabled => "Off".into(),
    }
}

/// Source the daemon just detected; it now waits for that source's output key.
fn detected(remaps: &DeviceRemaps) -> Option<EvdevKey> {
    remaps
        .capture_source
        .filter(|_| remaps.capturing == Some(CaptureRole::Target))
}

/// Commit a library action to `source`.
fn assign(ctx: &egui::Context, cx: &mut Cx, source: EvdevKey, action: Action) {
    if is_locked(source) || source.0 == 0 {
        return;
    }
    let existing = cx.binding(source).cloned();
    match action {
        Action::Default => {
            if existing.is_some() {
                cx.clear(source);
            }
        }
        Action::Disable => cx.set(RemapBinding {
            mode: RemapMode::Disabled,
            ..RemapBinding::tap(source, EvdevKey(0))
        }),
        Action::Key(target) => {
            // A repeating key keeps repeating with the new output.
            let b = match existing.as_ref().filter(|b| b.mode.uses_target()) {
                Some(b) => RemapBinding {
                    target,
                    ..b.clone()
                },
                None => RemapBinding::tap(source, target),
            };
            if b.validate().is_ok() {
                cx.set(b);
            } else if existing.is_some() {
                // A key mapped to itself is just the default.
                cx.clear(source);
            }
        }
        Action::NewMacro => editor::begin_macro(ctx, cx, source),
        Action::MacroFrom(from) => {
            if let Some(m) = cx.binding(from).cloned() {
                cx.set(RemapBinding { source, ..m });
            }
        }
    }
}

pub fn keyboard_page(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    remaps: &DeviceRemaps,
    out: &mut Vec<UiCommand>,
) {
    page(ui, &mut Cx { d, remaps, out });
}

pub fn mouse_page(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    remaps: &DeviceRemaps,
    out: &mut Vec<UiCommand>,
) {
    page(ui, &mut Cx { d, remaps, out });
}

/// Columns grow with their content; nested scroll areas need a finite, generous cap.
const COLUMN_MAX: f32 = 4000.0;

fn page(ui: &mut Ui, cx: &mut Cx) {
    let mouse = cx.d.kind == DeviceKind::Mouse;
    let sel_id = egui::Id::new(("bind_selected", cx.d.id.key()));
    // 0 = nothing selected. A mouse opens on its back button, the usual first remap.
    let mut selected = ui
        .ctx()
        .data(|m| m.get_temp::<u16>(sel_id))
        .or(mouse.then_some(EvdevKey::BTN_SIDE.0))
        .filter(|c| *c != 0)
        .map(EvdevKey);
    if let Some(k) = detected(cx.remaps) {
        selected = Some(k);
    }

    let layout_id = egui::Id::new(("bind_layout", cx.d.id.key()));
    let mut layout = ui
        .ctx()
        .data_mut(|m| m.get_persisted::<u8>(layout_id))
        .and_then(|i| Layout::ALL.get(i as usize).copied())
        .unwrap_or_else(|| Layout::guess(&cx.d.name));

    toolbar(ui, cx, (!mouse).then_some(&mut layout), layout_id);
    // Concerns every key (e.g. no permission to grab the device), so it is
    // shown whether or not one is selected.
    if let Some(err) = &cx.remaps.last_error {
        ui.colored_label(theme::pal().warn, err);
    }
    ui.add_space(theme::SPACE_SM);

    let dropped = if mouse {
        mouse::hero(ui, cx, &mut selected)
    } else {
        keyboard::hero(ui, cx, layout, &mut selected)
    };
    if let Some((key, action)) = dropped {
        selected = Some(key);
        assign(ui.ctx(), cx, key, action);
    }
    legend(ui);
    ui.add_space(theme::SPACE_MD);

    // Wide: assignment + overview beside the library. Narrow: stacked.
    let full = ui.available_width();
    let mut picked = None;
    if full >= 700.0 {
        let right_w = (full * 0.38).clamp(280.0, 360.0);
        let left_w = full - right_w - theme::SPACE_MD;
        let top = ui.cursor().min;
        let column = |ui: &mut Ui, salt: &str, x: f32, w: f32| {
            ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(salt)
                    .max_rect(Rect::from_min_size(pos2(x, top.y), vec2(w, COLUMN_MAX)))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            )
        };
        let mut left = column(ui, "bind_left", top.x, left_w);
        editor::panel(&mut left, cx, &mut selected);
        overview(&mut left, cx, &mut selected);
        let mut right = column(ui, "bind_right", top.x + left_w + theme::SPACE_MD, right_w);
        picked = library::panel(&mut right, cx, selected);
        let h = left.min_rect().height().max(right.min_rect().height());
        ui.allocate_space(vec2(full, h));
    } else {
        editor::panel(ui, cx, &mut selected);
        picked = picked.or(library::panel(ui, cx, selected));
        overview(ui, cx, &mut selected);
    }

    if let (Some(action), Some(source)) = (picked, selected) {
        match action {
            // While a macro is open, keys from the library become steps.
            Action::Key(k) if editor::editing_macro(ui.ctx(), cx, source) => {
                editor::append_key(ui.ctx(), cx, source, k)
            }
            _ => assign(ui.ctx(), cx, source, action),
        }
    }
    ui.ctx()
        .data_mut(|m| m.insert_temp(sel_id, selected.map_or(0, |k| k.0)));
}

/// Layout picker (keyboards), Detect, and remapper status.
fn toolbar(ui: &mut Ui, cx: &mut Cx, layout: Option<&mut Layout>, layout_id: egui::Id) {
    ui.horizontal(|ui| {
        ui.set_min_height(32.0);
        match layout {
            Some(layout) => {
                let labels: Vec<&str> = Layout::ALL.iter().map(|l| l.label()).collect();
                let sel = Layout::ALL.iter().position(|l| l == layout);
                if let Some(i) = widgets::segmented(ui, layout_id.with("seg"), &labels, sel) {
                    *layout = Layout::ALL[i];
                    ui.ctx()
                        .data_mut(|m| m.insert_persisted(layout_id, i as u8));
                }
            }
            None => {
                ui.label(
                    egui::RichText::new("Pick a button, then choose what it does").color(pal().text_muted),
                );
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if cx.remaps.capturing == Some(CaptureRole::Source) {
                if ui.button("Cancel").clicked() {
                    cx.cancel_capture(false);
                }
            } else if ui
                .button("Detect…")
                .on_hover_text(
                    "Press a key or button on the device to select it, including ones not drawn here",
                )
                .clicked()
            {
                cx.out.push(UiCommand::StartCapture {
                    device_id: cx.d.id.clone(),
                    role: CaptureRole::Source,
                    source: None,
                });
            }
            let n = cx.remaps.bindings.len();
            if n > 0 {
                let live = if cx.remaps.grabbed { " · live" } else { "" };
                widgets::pill(
                    ui,
                    &format!("{n} assigned{live}"),
                    if cx.remaps.grabbed { theme::pal().ok } else { pal().accent },
                );
            }
        });
    });
}

/// Colour key for the marks on the illustration.
fn legend(ui: &mut Ui) {
    ui.add_space(theme::SPACE_XS);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for (label, color) in [
            ("Remap", theme::pal().accent),
            ("Repeat", theme::MODE_REPEAT),
            ("Macro", theme::MODE_MACRO),
            ("Disabled", theme::MODE_OFF),
        ] {
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().rect_filled(r, 2.0, color);
            ui.label(egui::RichText::new(label).small().color(pal().text_faint));
            ui.add_space(8.0);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::hint(ui, "Click to select · drag an action onto a key to assign");
        });
    });
}

/// Rounded chip naming a key, drawn like a small keycap.
pub(crate) fn keycap_chip(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    let galley =
        ui.fonts(|f| f.layout_no_wrap(text.into(), egui::FontId::proportional(12.0), color));
    let size = vec2((galley.size().x + 16.0).max(30.0), 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect.translate(vec2(0.0, 1.5)), 6.0, theme::pal().key_side);
    p.rect_filled(rect, 6.0, theme::pal().key_top);
    p.rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0_f32, color.gamma_multiply(0.35)),
        egui::StrokeKind::Inside,
    );
    p.galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

/// Every active assignment with quick edit / clear.
fn overview(ui: &mut Ui, cx: &mut Cx, selected: &mut Option<EvdevKey>) {
    let n = cx.remaps.bindings.len();
    widgets::section(
        ui,
        "Assignments",
        |ui| {
            ui.label(
                egui::RichText::new(match n {
                    0 => "none yet".to_string(),
                    n => format!("{n} active"),
                })
                .small()
                .color(pal().text_faint),
            );
        },
        |ui| {
            if n == 0 {
                teach(ui);
                return;
            }
            let bindings = cx.remaps.bindings.clone();
            for (i, b) in bindings.iter().enumerate() {
                if i > 0 {
                    widgets::divider(ui);
                }
                let color = mode_color(b.mode);
                let row = ui.horizontal(|ui| {
                    ui.set_min_height(40.0);
                    keycap_chip(ui, &source_name(cx, b.source), pal().text);
                    let (r, _) = ui.allocate_exact_size(vec2(16.0, 12.0), Sense::hover());
                    arrow(
                        ui.painter(),
                        r.center(),
                        vec2(1.0, 0.0),
                        6.0,
                        pal().text_faint,
                    );
                    ui.label(egui::RichText::new(describe(Some(b))).color(color));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button(
                            ui,
                            26.0,
                            "Reset to default",
                            true,
                            widgets::icon_close,
                        )
                        .clicked()
                        {
                            cx.clear(b.source);
                        }
                        if ui.button("Edit").clicked() {
                            *selected = Some(b.source);
                        }
                    });
                });
                if *selected == Some(b.source) {
                    let r = row.response.rect;
                    ui.painter().rect_filled(
                        Rect::from_min_size(
                            r.left_top() - vec2(10.0, -8.0),
                            vec2(3.0, r.height() - 16.0),
                        ),
                        2.0,
                        color,
                    );
                }
            }
        },
    );
}

/// Empty state: the three steps, numbered.
fn teach(ui: &mut Ui) {
    for (i, (title, body)) in [
        (
            "Pick a key",
            "Click it on the device above, or use Detect for one that is not drawn.",
        ),
        (
            "Choose an action",
            "Click one in Actions, or drag it straight onto the key.",
        ),
        (
            "That's it",
            "Assignments apply instantly and survive restarts. Reset any key to undo.",
        ),
    ]
    .iter()
    .enumerate()
    {
        ui.horizontal(|ui| {
            ui.set_min_height(40.0);
            let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            ui.painter()
                .circle_filled(r.center(), 12.0, pal().surface_hi);
            ui.painter()
                .circle_stroke(r.center(), 12.0, Stroke::new(1.0_f32, pal().border));
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                (i + 1).to_string(),
                egui::FontId::proportional(12.0),
                pal().accent,
            );
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(egui::RichText::new(*title).color(pal().text));
                ui.label(egui::RichText::new(*body).small().color(pal().text_faint));
            });
        });
    }
}

/// Banner over the illustration while the daemon waits for a source press.
fn detect_overlay(ui: &Ui, area: Rect, what: &str) {
    let t = ui.input(|i| i.time) as f32;
    ui.ctx().request_repaint();
    let p = ui.painter_at(area);
    p.rect_filled(area, 16.0, theme::pal().stage.gamma_multiply(0.82));
    let pulse = 0.5 + 0.5 * (t * 3.0).sin();
    p.circle_stroke(
        area.center() - vec2(0.0, 26.0),
        10.0 + 8.0 * pulse,
        Stroke::new(1.5_f32, pal().accent.gamma_multiply(1.0 - pulse)),
    );
    p.circle_filled(area.center() - vec2(0.0, 26.0), 6.0, pal().accent);
    p.text(
        area.center() + vec2(0.0, 8.0),
        egui::Align2::CENTER_CENTER,
        format!("Press the {what} you want to change"),
        egui::FontId::proportional(17.0),
        pal().text,
    );
    p.text(
        area.center() + vec2(0.0, 32.0),
        egui::Align2::CENTER_CENTER,
        "Esc cancels",
        egui::FontId::proportional(12.0),
        pal().text_muted,
    );
}

/// Small arrow pointing along `dir` (the bundled fonts have no arrow glyphs).
fn arrow(p: &egui::Painter, center: egui::Pos2, dir: egui::Vec2, half: f32, color: Color32) {
    let s = Stroke::new(1.5_f32, color);
    let tip = center + dir * half;
    let side = dir.rot90() * half * 0.6;
    p.line_segment([center - dir * half, tip], s);
    p.line_segment([tip - dir * half * 0.7 + side, tip], s);
    p.line_segment([tip - dir * half * 0.7 - side, tip], s);
}

/// Backdrop behind a device illustration.
fn stage(ui: &Ui, area: Rect) {
    let p = ui.painter();
    p.rect_filled(area, 16.0, theme::pal().stage);
    p.rect_stroke(
        area,
        16.0,
        Stroke::new(1.0_f32, pal().border),
        egui::StrokeKind::Inside,
    );
}

/// Soft halo in `color` around `rect` (device lighting, selection).
fn glow(p: &egui::Painter, rect: Rect, radius: u8, blur: u8, color: Color32) {
    p.add(
        egui::Shadow {
            offset: [0, 0],
            blur,
            spread: 0,
            color,
        }
        .as_shape(rect, radius),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::remap::{MacroStep, RemapBinding};
    use hako_core::DeviceId;

    fn run(bindings: Vec<RemapBinding>, source: EvdevKey, action: Action) -> Vec<UiCommand> {
        let d = DeviceSnapshot {
            id: DeviceId {
                vendor_id: 1,
                product_id: 2,
                instance: "t".into(),
            },
            kind: DeviceKind::Keyboard,
            name: "Test".into(),
            connected: true,
            batteries: Vec::new(),
            settings: Default::default(),
            controls: Vec::new(),
            last_error: None,
        };
        let remaps = DeviceRemaps {
            bindings,
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut cx = Cx {
            d: &d,
            remaps: &remaps,
            out: &mut out,
        };
        assign(&egui::Context::default(), &mut cx, source, action);
        out
    }

    #[test]
    fn library_actions_become_valid_commands() {
        let (a, b) = (EvdevKey(30), EvdevKey(48));
        let turbo = RemapBinding {
            mode: RemapMode::Turbo,
            interval_ms: 80,
            ..RemapBinding::tap(a, b)
        };
        // Plain key: a tap remap.
        assert!(matches!(
            &run(vec![], a, Action::Key(b))[..],
            [UiCommand::SetRemap { binding, .. }]
                if binding.mode == RemapMode::Tap && binding.target == b
        ));
        // A repeating key keeps its mode and interval.
        assert!(matches!(
            &run(vec![turbo], a, Action::Key(EvdevKey(57)))[..],
            [UiCommand::SetRemap { binding, .. }]
                if binding.mode == RemapMode::Turbo
                    && binding.interval_ms == 80
                    && binding.target == EvdevKey(57)
        ));
        // A key onto itself is the default, never an invalid binding.
        assert!(run(vec![], a, Action::Key(a)).is_empty());
        assert!(matches!(
            &run(vec![RemapBinding::tap(a, b)], a, Action::Key(a))[..],
            [UiCommand::ClearRemap { .. }]
        ));
        assert!(matches!(
            &run(vec![], a, Action::Disable)[..],
            [UiCommand::SetRemap { binding, .. }] if binding.mode == RemapMode::Disabled
        ));
        assert!(run(vec![], a, Action::Default).is_empty());
        // Left click is never rebindable; a new macro only opens the editor.
        assert!(run(vec![], EvdevKey::BTN_LEFT, Action::Disable).is_empty());
        assert!(run(vec![], a, Action::NewMacro).is_empty());
        // Copying a macro moves its steps to the new source.
        let mac = RemapBinding {
            mode: RemapMode::Macro,
            steps: vec![MacroStep {
                key: b,
                down: true,
                delay_ms: 0,
            }],
            ..RemapBinding::tap(b, EvdevKey(0))
        };
        let out = run(vec![mac], a, Action::MacroFrom(b));
        let [UiCommand::SetRemap { binding, .. }] = &out[..] else {
            panic!("expected one SetRemap, got {out:?}");
        };
        assert_eq!((binding.source, binding.steps.len()), (a, 1));
        assert!(binding.validate().is_ok());
    }

    #[test]
    fn unknown_codes_still_get_a_distinct_name() {
        assert_eq!(key_name(EvdevKey(183)), "F13");
        assert_eq!(key_name(EvdevKey(82)), "Num 0");
        assert_eq!(key_name(EvdevKey(30)), "A");
        assert_eq!(key_name(EvdevKey(240)), "Key 240");
    }
}
