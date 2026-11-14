//! Macro editor: record, a timeline of the take, and an editable step list.

use super::editor::{apply, interval_row, Draft};
use super::{key_name, keycap_chip, Cx};
use crate::theme::{self, pal};
use crate::widgets;
use crate::UiCommand;
use egui::{pos2, vec2, Rect, Sense, Stroke, Ui};
use hako_core::remap::{
    CaptureRole, EvdevKey, MacroRepeat, MacroStep, RemapMode, MAX_MACRO_STEPS, MAX_STEP_DELAY_MS,
};

/// Drag payload for reordering: index of the step being moved.
#[derive(Clone, Copy)]
struct StepDrag(usize);

fn fmt_ms(ms: u32) -> String {
    if ms >= 1000 {
        format!("{:.2} s", ms as f32 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

/// Legend color on the violet macro bars, on every theme.
const MACRO_INK: egui::Color32 = egui::Color32::from_rgb(22, 16, 40);

/// Key-hold bars over time, one lane per key (G HUB / iCUE style).
fn timeline(ui: &mut Ui, steps: &[MacroStep], live: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 66.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 8.0, theme::pal().stage);
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(
            1.0_f32,
            if live {
                theme::pal().danger.gamma_multiply(0.6)
            } else {
                pal().border
            },
        ),
        egui::StrokeKind::Inside,
    );
    if steps.is_empty() {
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            if live {
                "Listening — press keys or buttons on any device"
            } else {
                "Record a sequence, or click keys in Actions to add steps"
            },
            egui::FontId::proportional(12.0),
            pal().text_faint,
        );
        return;
    }
    let mut at = Vec::with_capacity(steps.len());
    let mut t = 0u32;
    for s in steps {
        t += s.delay_ms;
        at.push(t);
    }
    let total = t.max(1) as f32;
    let mut lanes: Vec<EvdevKey> = Vec::new();
    for s in steps {
        if !lanes.contains(&s.key) && lanes.len() < 4 {
            lanes.push(s.key);
        }
    }
    let inner = rect.shrink2(vec2(10.0, 8.0));
    let track = Rect::from_min_max(inner.min, pos2(inner.right(), inner.bottom() - 12.0));
    let lane_h = track.height() / lanes.len() as f32;
    let x = |ms: u32| track.left() + ms as f32 / total * (track.width() - 8.0);
    for (i, s) in steps.iter().enumerate().filter(|(_, s)| s.down) {
        // Held until its release, or to the end when the take never lets go.
        let end = steps[i + 1..]
            .iter()
            .position(|r| r.key == s.key && !r.down)
            .map_or(t, |j| at[i + 1 + j]);
        // Keys past the fourth share the last lane.
        let lane = lanes
            .iter()
            .position(|k| *k == s.key)
            .unwrap_or(lanes.len() - 1);
        let bar = Rect::from_min_max(
            pos2(x(at[i]), track.top() + lane as f32 * lane_h + 1.5),
            pos2(
                (x(end)).max(x(at[i]) + 8.0),
                track.top() + (lane + 1) as f32 * lane_h - 1.5,
            ),
        );
        p.rect_filled(bar, 3.0, theme::MODE_MACRO.gamma_multiply(0.75));
        if lane_h >= 11.0 {
            let g = p.layout_no_wrap(key_name(s.key), egui::FontId::proportional(9.5), MACRO_INK);
            if g.size().x + 6.0 <= bar.width() {
                p.galley(
                    pos2(bar.left() + 3.0, bar.center().y - g.size().y / 2.0),
                    g,
                    MACRO_INK,
                );
            }
        }
    }
    let axis = egui::FontId::proportional(9.5);
    p.text(
        inner.left_bottom(),
        egui::Align2::LEFT_BOTTOM,
        "0",
        axis.clone(),
        pal().text_faint,
    );
    p.text(
        inner.right_bottom(),
        egui::Align2::RIGHT_BOTTOM,
        fmt_ms(t),
        axis,
        pal().text_faint,
    );
}

fn icon_arrow(p: &egui::Painter, r: Rect, c: egui::Color32, down: bool) {
    let s = Stroke::new(1.6_f32, c);
    let (tip, tail) = if down {
        (r.center_bottom(), r.center_top())
    } else {
        (r.center_top(), r.center_bottom())
    };
    let wing = if down { -4.0 } else { 4.0 };
    p.line_segment([tail, tip], s);
    p.line_segment([tip + vec2(-4.0, wing), tip], s);
    p.line_segment([tip + vec2(4.0, wing), tip], s);
}

fn icon_clock(p: &egui::Painter, r: Rect, c: egui::Color32) {
    let s = Stroke::new(1.4_f32, c);
    p.circle_stroke(r.center(), r.width() / 2.0, s);
    p.line_segment([r.center(), r.center() - vec2(0.0, r.height() * 0.3)], s);
    p.line_segment([r.center(), r.center() + vec2(r.width() * 0.22, 0.0)], s);
}

enum Edit {
    Remove(usize),
    Move(usize, usize),
}

fn step_list(ui: &mut Ui, id: egui::Id, draft: &mut Draft) {
    let mut edit = None;
    let n = draft.steps.len();
    let dragging = egui::DragAndDrop::has_payload_of_type::<StepDrag>(ui.ctx());
    egui::ScrollArea::vertical()
        .id_salt(id.with("steps"))
        .max_height(232.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (i, step) in draft.steps.iter_mut().enumerate() {
                // The wait before a step is its own row, as in vendor editors.
                if step.delay_ms > 0 {
                    ui.horizontal(|ui| {
                        ui.set_min_height(24.0);
                        ui.add_space(34.0);
                        let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                        icon_clock(ui.painter(), r, pal().text_faint);
                        ui.label(egui::RichText::new("wait").small().color(pal().text_faint));
                        if ui
                            .add(
                                egui::DragValue::new(&mut step.delay_ms)
                                    .range(0..=MAX_STEP_DELAY_MS)
                                    .suffix(" ms")
                                    .speed(1.0),
                            )
                            .changed()
                        {
                            draft.steps_dirty = true;
                        }
                    });
                }
                let row = ui.horizontal(|ui| {
                    ui.set_min_height(30.0);
                    let (grip, gresp) = ui.allocate_exact_size(vec2(16.0, 26.0), Sense::drag());
                    widgets::icon_grip(
                        ui.painter(),
                        grip,
                        if gresp.hovered() || gresp.dragged() {
                            pal().text
                        } else {
                            pal().text_faint
                        },
                    );
                    if gresp.drag_started() {
                        gresp.dnd_set_drag_payload(StepDrag(i));
                    }
                    gresp
                        .on_hover_cursor(egui::CursorIcon::Grab)
                        .on_hover_text("Drag to reorder");
                    ui.add_sized(
                        [20.0, 18.0],
                        egui::Label::new(
                            egui::RichText::new(format!("{}", i + 1))
                                .small()
                                .color(pal().text_faint),
                        ),
                    );
                    let color = if step.down {
                        pal().accent
                    } else {
                        pal().text_muted
                    };
                    let (ir, _) = ui.allocate_exact_size(vec2(12.0, 14.0), Sense::hover());
                    icon_arrow(ui.painter(), ir, color, step.down);
                    let edge = egui::Label::new(
                        egui::RichText::new(if step.down { "Press" } else { "Release" })
                            .color(color),
                    )
                    .sense(Sense::click());
                    if ui
                        .add_sized([52.0, 18.0], edge)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Click to flip between press and release")
                        .clicked()
                    {
                        step.down = !step.down;
                        draft.steps_dirty = true;
                    }
                    keycap_chip(ui, &key_name(step.key), pal().text);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if widgets::icon_button(ui, 24.0, "Delete step", true, widgets::icon_close)
                            .clicked()
                        {
                            edit = Some(Edit::Remove(i));
                        }
                        if widgets::icon_button(ui, 24.0, "Move down", i + 1 < n, |p, r, c| {
                            widgets::icon_chevron(p, r, c, true)
                        })
                        .clicked()
                        {
                            edit = Some(Edit::Move(i, i + 1));
                        }
                        if widgets::icon_button(ui, 24.0, "Move up", i > 0, |p, r, c| {
                            widgets::icon_chevron(p, r, c, false)
                        })
                        .clicked()
                        {
                            edit = Some(Edit::Move(i, i - 1));
                        }
                        if step.delay_ms == 0
                            && widgets::icon_button(
                                ui,
                                24.0,
                                "Insert a delay before this step",
                                true,
                                icon_clock,
                            )
                            .clicked()
                        {
                            step.delay_ms = 50;
                            draft.steps_dirty = true;
                        }
                    });
                });
                if dragging {
                    let resp = ui.interact(row.response.rect, id.with(("drop", i)), Sense::hover());
                    if let Some(from) = resp.dnd_hover_payload::<StepDrag>() {
                        // A step dragged down lands after this row, up before it.
                        let r = row.response.rect;
                        let y = if from.0 < i { r.bottom() } else { r.top() };
                        ui.painter().line_segment(
                            [pos2(r.left(), y), pos2(r.right(), y)],
                            Stroke::new(2.0_f32, pal().accent),
                        );
                    }
                    if let Some(from) = resp.dnd_release_payload::<StepDrag>() {
                        edit = Some(Edit::Move(from.0, i));
                    }
                }
            }
        });
    match edit {
        Some(Edit::Remove(i)) => {
            draft.steps.remove(i);
        }
        Some(Edit::Move(from, to)) if from != to && from < n && to < n => {
            let s = draft.steps.remove(from);
            draft.steps.insert(to, s);
        }
        _ => return,
    }
    draft.steps_dirty = true;
}

pub(super) fn editor(ui: &mut Ui, cx: &mut Cx, source: EvdevKey, draft: &mut Draft) {
    let recording = cx.remaps.capturing == Some(CaptureRole::Record);
    let id = egui::Id::new(("bind_macro", cx.d.id.key(), source.0));
    ui.add_space(theme::SPACE_SM);
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        if recording {
            let t = ui.input(|i| i.time) as f32;
            ui.ctx().request_repaint();
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            ui.painter().circle_filled(
                r.center(),
                5.0,
                theme::pal()
                    .danger
                    .gamma_multiply(0.5 + 0.5 * (t * 3.0).sin().abs()),
            );
            ui.label(
                egui::RichText::new(format!("Recording · {} events", cx.remaps.recorded.len()))
                    .color(pal().text),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Discard").clicked() {
                    draft.awaiting_record = false;
                    cx.cancel_capture(false);
                }
                if widgets::primary_button(ui, "Stop").clicked() {
                    cx.cancel_capture(true);
                }
            });
        } else {
            let rec = ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("     Record").color(theme::pal().danger),
                    )
                    .min_size(vec2(104.0, theme::CONTROL_H)),
                )
                .on_hover_text("Records keys and buttons with their timing. Esc stops.");
            ui.painter().circle_filled(
                rec.rect.left_center() + vec2(19.0, 0.0),
                5.0,
                theme::pal().danger,
            );
            if rec.clicked() {
                draft.awaiting_record = true;
                cx.out.push(UiCommand::StartCapture {
                    device_id: cx.d.id.clone(),
                    role: CaptureRole::Record,
                    source: Some(source),
                });
            }
            if ui
                .add_enabled(!draft.steps.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                draft.steps.clear();
                draft.steps_dirty = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let total: u32 = draft.steps.iter().map(|s| s.delay_ms).sum();
                let n = draft.steps.len();
                ui.label(
                    egui::RichText::new(format!(
                        "{n} / {MAX_MACRO_STEPS} steps · {}",
                        fmt_ms(total)
                    ))
                    .small()
                    .color(if n >= MAX_MACRO_STEPS {
                        theme::pal().warn
                    } else {
                        pal().text_muted
                    }),
                );
            });
        }
    });
    ui.add_space(theme::SPACE_XS);

    if recording {
        timeline(ui, &cx.remaps.recorded, true);
        ui.add_space(theme::SPACE_XS);
        widgets::hint(
            ui,
            "Perform the sequence. Stop (or Esc) keeps the take; Discard drops it.",
        );
        return;
    }
    timeline(ui, &draft.steps, false);
    ui.add_space(theme::SPACE_XS);
    if !draft.steps.is_empty() {
        step_list(ui, id, draft);
    }

    ui.add_space(theme::SPACE_XS);
    widgets::divider(ui);
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(theme::ROW_H);
        ui.label(egui::RichText::new("Playback").color(pal().text));
        let labels: Vec<&str> = MacroRepeat::ALL.iter().map(|r| r.label()).collect();
        let sel = MacroRepeat::ALL.iter().position(|r| *r == draft.repeat);
        if let Some(i) = widgets::segmented(ui, id.with("repeat"), &labels, sel) {
            draft.repeat = MacroRepeat::ALL[i];
            draft.steps_dirty = true;
        }
    });
    if draft.repeat != MacroRepeat::Once
        && interval_row(ui, "Gap between loops", &mut draft.interval_ms)
    {
        draft.steps_dirty = true;
    }

    ui.add_space(theme::SPACE_SM);
    ui.horizontal(|ui| {
        let can_save = draft.steps_dirty || !draft.saved_macro();
        ui.add_enabled_ui(can_save && !draft.steps.is_empty(), |ui| {
            if widgets::primary_button(ui, "Save macro").clicked() {
                draft.mode = Some(RemapMode::Macro);
                draft.steps_dirty = false;
                apply(cx, draft, source);
            }
        });
        if draft.steps_dirty {
            ui.label(
                egui::RichText::new("Unsaved changes")
                    .small()
                    .color(theme::pal().warn),
            );
        }
    });
}
