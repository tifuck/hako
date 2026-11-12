//! Assignment panel for the selected key: what it does and its options.
//! Edits stay in a local draft and are committed with `SetRemap`.

use super::{describe, keycap_chip, macros, mode_color, source_name, Cx};
use crate::theme::{self, pal};
use crate::widgets;
use crate::UiCommand;
use egui::{pos2, vec2, Color32, Rect, Sense, Stroke, Ui};
use hako_core::remap::{
    CaptureRole, EvdevKey, MacroRepeat, MacroStep, RemapBinding, RemapMode, MAX_MACRO_STEPS,
    MIN_INTERVAL_MS,
};

/// Local edit state for one source. Server truth is re-adopted whenever the
/// daemon's binding for this source changes.
#[derive(Clone)]
pub(super) struct Draft {
    seen: Option<RemapBinding>,
    pub mode: Option<RemapMode>,
    pub target: EvdevKey,
    pub interval_ms: u32,
    pub repeat: MacroRepeat,
    pub steps: Vec<MacroStep>,
    /// Mode chosen but not yet applicable (no target yet).
    pending: bool,
    pub steps_dirty: bool,
    pub awaiting_record: bool,
}

impl Draft {
    fn from(b: Option<&RemapBinding>) -> Self {
        Self {
            seen: b.cloned(),
            mode: b.map(|b| b.mode),
            target: b.map(|b| b.target).unwrap_or(EvdevKey(0)),
            interval_ms: b.map(|b| b.interval_ms).unwrap_or(50),
            repeat: b.map(|b| b.repeat).unwrap_or_default(),
            steps: b.map(|b| b.steps.clone()).unwrap_or_default(),
            pending: false,
            steps_dirty: false,
            awaiting_record: false,
        }
    }

    fn binding(&self, source: EvdevKey) -> Option<RemapBinding> {
        let b = RemapBinding {
            source,
            target: self.target,
            mode: self.mode?,
            interval_ms: self.interval_ms,
            steps: self.steps.clone(),
            repeat: self.repeat,
        };
        b.validate().ok().map(|_| b)
    }

    /// The saved binding is already this macro (nothing new to save).
    pub fn saved_macro(&self) -> bool {
        self.seen.as_ref().map(|b| b.mode) == Some(RemapMode::Macro)
    }
}

fn draft_id(cx: &Cx, source: EvdevKey) -> egui::Id {
    egui::Id::new(("bind_draft", cx.d.id.key(), source.0))
}

fn load(ctx: &egui::Context, cx: &Cx, source: EvdevKey) -> Draft {
    ctx.data(|m| m.get_temp::<Draft>(draft_id(cx, source)))
        .unwrap_or_else(|| Draft::from(cx.binding(source)))
}

fn store(ctx: &egui::Context, cx: &Cx, source: EvdevKey, draft: Draft) {
    ctx.data_mut(|m| m.insert_temp(draft_id(cx, source), draft));
}

/// Switch `source` to the macro editor without saving anything yet.
pub(super) fn begin_macro(ctx: &egui::Context, cx: &Cx, source: EvdevKey) {
    let mut draft = load(ctx, cx, source);
    draft.mode = Some(RemapMode::Macro);
    draft.pending = false;
    store(ctx, cx, source, draft);
}

pub(super) fn editing_macro(ctx: &egui::Context, cx: &Cx, source: EvdevKey) -> bool {
    load(ctx, cx, source).mode == Some(RemapMode::Macro)
}

/// Append a press + release of `key` to the open macro.
pub(super) fn append_key(ctx: &egui::Context, cx: &Cx, source: EvdevKey, key: EvdevKey) {
    let mut draft = load(ctx, cx, source);
    if draft.steps.len() + 2 > MAX_MACRO_STEPS {
        return;
    }
    let gap = if draft.steps.is_empty() { 0 } else { 30 };
    for (down, delay_ms) in [(true, gap), (false, 30)] {
        draft.steps.push(MacroStep {
            key,
            down,
            delay_ms,
        });
    }
    draft.steps_dirty = true;
    store(ctx, cx, source, draft);
}

pub(super) fn apply(cx: &mut Cx, draft: &mut Draft, source: EvdevKey) {
    if let Some(binding) = draft.binding(source) {
        draft.pending = false;
        cx.set(binding);
    } else {
        draft.pending = true;
    }
}

const MODES: [(&str, &str); 6] = [
    ("Default", "Built-in behavior"),
    ("Remap", "Send another key"),
    ("Hold repeat", "Repeats while held"),
    ("Toggle repeat", "Press to start / stop"),
    ("Macro", "Play a sequence"),
    ("Disable", "Do nothing"),
];

fn tile_color(i: usize) -> Color32 {
    match i {
        0 => pal().text_muted,
        i => mode_color(RemapMode::ALL[i - 1]),
    }
}

fn mode_icon(p: &egui::Painter, r: Rect, c: Color32, i: usize) {
    let s = Stroke::new(1.6_f32, c);
    let (cx, cy) = (r.center().x, r.center().y);
    match i {
        // Default: a plain key.
        0 => {
            p.rect_stroke(r.shrink(2.0), 3.0, s, egui::StrokeKind::Inside);
            p.circle_filled(r.center(), 1.6, c);
        }
        // Remap: arrow.
        1 => {
            p.line_segment([pos2(r.left() + 1.0, cy), pos2(r.right() - 1.0, cy)], s);
            p.line_segment(
                [pos2(r.right() - 6.0, cy - 5.0), pos2(r.right() - 1.0, cy)],
                s,
            );
            p.line_segment(
                [pos2(r.right() - 6.0, cy + 5.0), pos2(r.right() - 1.0, cy)],
                s,
            );
        }
        // Hold repeat: pulses.
        2 => {
            for k in 0..4 {
                let x = r.left() + 2.0 + k as f32 * (r.width() - 4.0) / 3.0;
                p.line_segment([pos2(x, cy - 5.0), pos2(x, cy + 5.0)], s);
            }
        }
        // Toggle repeat: switch.
        3 => {
            let track = Rect::from_center_size(r.center(), vec2(r.width() - 1.0, 10.0));
            p.rect_stroke(track, 5.0, s, egui::StrokeKind::Inside);
            p.circle_filled(pos2(track.right() - 5.0, cy), 2.6, c);
        }
        // Macro: step list.
        4 => {
            for k in 0..3 {
                let y = cy + (k as f32 - 1.0) * 5.0;
                p.circle_filled(pos2(r.left() + 2.5, y), 1.4, c);
                p.line_segment([pos2(r.left() + 7.0, y), pos2(r.right() - 1.0, y)], s);
            }
        }
        // Disable: barred circle.
        _ => {
            let rad = r.width() / 2.0 - 2.0;
            p.circle_stroke(r.center(), rad, s);
            let d = rad * 0.7;
            p.line_segment([pos2(cx - d, cy + d), pos2(cx + d, cy - d)], s);
        }
    }
}

/// Mode picker as a grid of tiles. Returns the tile clicked this frame.
fn mode_tiles(ui: &mut Ui, id: egui::Id, current: usize) -> Option<usize> {
    let avail = ui.available_width();
    let cols = if avail >= 430.0 { 3 } else { 2 };
    let gap = 8.0;
    let size = vec2((avail - gap * (cols as f32 - 1.0)) / cols as f32, 50.0);
    let rows = MODES.len().div_ceil(cols);
    let (area, _) = ui.allocate_exact_size(
        vec2(avail, rows as f32 * (size.y + gap) - gap),
        Sense::hover(),
    );
    let mut clicked = None;
    for (i, (label, hint)) in MODES.iter().enumerate() {
        let rect = Rect::from_min_size(
            area.min
                + vec2(
                    (i % cols) as f32 * (size.x + gap),
                    (i / cols) as f32 * (size.y + gap),
                ),
            size,
        );
        let resp = ui
            .interact(rect, id.with(i), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let active = i == current;
        let hot = ui.ctx().animate_bool_with_time(
            id.with((i, "hot")),
            resp.hovered() || resp.has_focus(),
            0.1,
        );
        let on = ui
            .ctx()
            .animate_bool_with_time(id.with((i, "on")), active, 0.14);
        let color = tile_color(i);
        let p = ui.painter();
        p.rect_filled(
            rect,
            9.0,
            pal()
                .surface_hi
                .gamma_multiply(0.55 + 0.45 * hot)
                .lerp_to_gamma(color.gamma_multiply(0.16), on),
        );
        p.rect_stroke(
            rect,
            9.0,
            Stroke::new(
                1.0_f32,
                pal().border.lerp_to_gamma(color, on.max(0.25 * hot)),
            ),
            egui::StrokeKind::Inside,
        );
        let icon = Rect::from_center_size(rect.left_center() + vec2(22.0, 0.0), vec2(18.0, 18.0));
        mode_icon(
            p,
            icon,
            pal().text_faint.lerp_to_gamma(color, on.max(hot * 0.6)),
            i,
        );
        let clip = p.with_clip_rect(rect.shrink(4.0));
        clip.text(
            rect.left_center() + vec2(42.0, -8.0),
            egui::Align2::LEFT_CENTER,
            *label,
            egui::FontId::proportional(13.0),
            if active {
                pal().text
            } else {
                pal().text.gamma_multiply(0.85)
            },
        );
        clip.text(
            rect.left_center() + vec2(42.0, 9.0),
            egui::Align2::LEFT_CENTER,
            *hint,
            egui::FontId::proportional(10.5),
            pal().text_faint,
        );
        if resp.clicked() && !active {
            clicked = Some(i);
        }
    }
    clicked
}

pub(super) fn panel(ui: &mut Ui, cx: &mut Cx, selected: &mut Option<EvdevKey>) {
    let Some(source) = *selected else {
        widgets::section(
            ui,
            "Nothing selected",
            |_| {},
            |ui| {
                ui.label(
                    egui::RichText::new(
                        "Select a key on the device to see and change what it does.",
                    )
                    .color(pal().text_muted),
                );
                widgets::hint(ui, "Keys that are not drawn can be picked with Detect.");
            },
        );
        return;
    };
    let bound = cx.binding(source).cloned();
    let mut draft = load(ui.ctx(), cx, source);

    if draft.seen != bound {
        // A capture created/changed the binding: keep a mode the user picked first.
        let wanted = draft.pending.then_some(draft.mode).flatten();
        let awaiting = draft.awaiting_record;
        let (steps, dirty) = (draft.steps.clone(), draft.steps_dirty);
        draft = Draft::from(bound.as_ref());
        draft.awaiting_record = awaiting;
        if dirty {
            draft.steps = steps;
            draft.steps_dirty = true;
        }
        if let (Some(m), Some(b)) = (wanted, bound.as_ref()) {
            if m != b.mode && m.uses_target() {
                draft.mode = Some(m);
                apply(cx, &mut draft, source);
            }
        }
    }

    let recording = cx.remaps.capturing == Some(CaptureRole::Record);
    if draft.awaiting_record && !recording {
        draft.awaiting_record = false;
        if !cx.remaps.recorded.is_empty() {
            draft.steps = cx.remaps.recorded.clone();
            draft.steps_dirty = true;
        }
    }

    theme::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Header: the key, what it does now, reset.
        ui.horizontal(|ui| {
            ui.set_min_height(44.0);
            let (cap, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
            let color = bound.as_ref().map_or(pal().accent, |b| mode_color(b.mode));
            let p = ui.painter();
            p.rect_filled(cap.translate(vec2(0.0, 3.0)), 9.0, theme::pal().key_side);
            p.rect_filled(cap, 9.0, theme::pal().key_top.lerp_to_gamma(color, 0.2));
            p.rect_stroke(
                cap,
                9.0,
                Stroke::new(1.5_f32, color),
                egui::StrokeKind::Inside,
            );
            widgets::icon_device(p, cap.shrink(13.0), color, cx.d.kind);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add_space(3.0);
                ui.label(theme::eyebrow("Selected"));
                ui.label(
                    egui::RichText::new(source_name(cx, source))
                        .size(17.0)
                        .strong()
                        .color(pal().text),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, 28.0, "Deselect", true, widgets::icon_close).clicked() {
                    *selected = None;
                }
                if ui
                    .add_enabled(bound.is_some(), egui::Button::new("Reset"))
                    .on_hover_text("Back to the device's default behavior")
                    .clicked()
                {
                    draft.mode = None;
                    draft.pending = false;
                    cx.clear(source);
                }
                widgets::pill(ui, &describe(bound.as_ref()), color);
            });
        });
        ui.add_space(theme::SPACE_SM);

        let current = match draft.mode {
            None => 0,
            Some(m) => 1 + RemapMode::ALL.iter().position(|x| *x == m).unwrap_or(0),
        };
        if let Some(i) = mode_tiles(ui, draft_id(cx, source).with("mode"), current) {
            if i == 0 {
                draft.mode = None;
                draft.pending = false;
                if bound.is_some() {
                    cx.clear(source);
                }
            } else {
                draft.mode = Some(RemapMode::ALL[i - 1]);
                if draft.mode != Some(RemapMode::Macro) {
                    apply(cx, &mut draft, source);
                }
            }
        }
        ui.add_space(theme::SPACE_SM);
        widgets::divider(ui);

        match draft.mode {
            None => {
                ui.add_space(theme::SPACE_SM);
                widgets::hint(ui, "Uses the device's built-in behavior.");
            }
            Some(RemapMode::Disabled) => {
                ui.add_space(theme::SPACE_SM);
                widgets::hint(ui, "This input is swallowed and does nothing.");
            }
            Some(RemapMode::Macro) => macros::editor(ui, cx, source, &mut draft),
            Some(mode) => {
                target_row(ui, cx, source, &mut draft);
                if mode.uses_interval() {
                    widgets::divider(ui);
                    if interval_row(ui, "Repeat every", &mut draft.interval_ms) {
                        apply(cx, &mut draft, source);
                    }
                    widgets::hint(
                        ui,
                        if mode == RemapMode::Turbo {
                            "Repeats while you hold the button."
                        } else {
                            "Press once to start repeating, press again to stop."
                        },
                    );
                }
            }
        }
        if let Some(err) = &cx.remaps.last_error {
            ui.add_space(theme::SPACE_SM);
            ui.colored_label(theme::pal().warn, err);
        }
    });
    ui.add_space(theme::SPACE_MD);
    store(ui.ctx(), cx, source, draft);
}

fn target_row(ui: &mut Ui, cx: &mut Cx, source: EvdevKey, draft: &mut Draft) {
    let listening = cx.remaps.capturing == Some(CaptureRole::Target)
        && cx.remaps.capture_source == Some(source);
    widgets::row(
        ui,
        "Sends",
        Some("Press a key, or pick one from Actions"),
        |ui| {
            if listening {
                if ui.button("Cancel").clicked() {
                    cx.cancel_capture(false);
                }
                let t = ui.input(|i| i.time) as f32;
                ui.ctx().request_repaint();
                ui.label(
                    egui::RichText::new("Press any key…  Esc cancels").color(
                        pal()
                            .accent
                            .gamma_multiply(0.6 + 0.4 * (t * 4.0).sin().abs()),
                    ),
                );
                return;
            }
            if ui
                .button("Press a key…")
                .on_hover_text("Click, then press the key or button to send")
                .clicked()
            {
                draft.pending = draft.binding(source).is_none() || draft.pending;
                cx.out.push(UiCommand::StartCapture {
                    device_id: cx.d.id.clone(),
                    role: CaptureRole::Target,
                    source: Some(source),
                });
            }
            if draft.target.0 == 0 {
                ui.label(egui::RichText::new("Not set").color(theme::pal().warn));
            } else {
                let mode = draft.mode.unwrap_or_default();
                keycap_chip(ui, &super::key_name(draft.target), mode_color(mode));
            }
        },
    );
}

/// Log slider for repeat periods. Returns true when the value is final.
pub(super) fn interval_row(ui: &mut Ui, label: &str, ms: &mut u32) -> bool {
    let mut commit = false;
    widgets::row(ui, label, None, |ui| {
        let rate = 1000.0 / *ms as f32;
        ui.label(widgets::value_text(format!("{ms} ms · {rate:.1}/s")));
        let mut v = *ms as f32;
        // Above 2 s a repeat stops feeling like one; the daemon allows more.
        let (min, max) = (MIN_INTERVAL_MS as f32, 2000.0);
        let width = (ui.available_width() - 16.0).clamp(80.0, 220.0);
        let resp = widgets::slider(
            ui,
            egui::Slider::new(&mut v, min..=max)
                .logarithmic(true)
                .show_value(false),
            width,
        );
        widgets::knob(ui, &resp, widgets::fraction(v, min, max, true));
        *ms = v.round() as u32;
        commit = resp.drag_stopped() || (resp.changed() && !resp.dragged());
    });
    commit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_only_builds_valid_bindings() {
        let mut d = Draft::from(None);
        d.mode = Some(RemapMode::Tap);
        assert!(
            d.binding(EvdevKey::BTN_SIDE).is_none(),
            "tap without a target"
        );
        d.target = EvdevKey(57);
        assert_eq!(d.binding(EvdevKey::BTN_SIDE).unwrap().target, EvdevKey(57));
        d.mode = Some(RemapMode::Macro);
        assert!(
            d.binding(EvdevKey::BTN_SIDE).is_none(),
            "macro without steps"
        );
        d.mode = Some(RemapMode::Disabled);
        assert!(d.binding(EvdevKey::BTN_SIDE).is_some());
    }
}
