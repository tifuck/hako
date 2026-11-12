//! Keyboard illustration: chassis, sculpted keycaps, per-key lighting glow.

use super::layouts::Layout;
use super::library::Action;
use super::{glow, is_locked, key_name, mode_color, short, Cx};
use crate::theme::{self, pal};
use egui::{pos2, vec2, Rect, Sense, Stroke, Ui};
use hako_core::remap::{CaptureRole, EvdevKey};

/// Draws the keyboard; returns an action dropped onto a key this frame.
pub(super) fn hero(
    ui: &mut Ui,
    cx: &mut Cx,
    layout: Layout,
    selected: &mut Option<EvdevKey>,
) -> Option<(EvdevKey, Action)> {
    let caps = layout.caps();
    let (cols, rows) = layout.size();
    // Chassis adds a 0.45u bezel around the keys.
    let bezel = 0.45;
    let width = ui.available_width();
    let unit = ((width - 40.0) / (cols + 2.0 * bezel)).clamp(20.0, 46.0);
    let deck_size = vec2((cols + 2.0 * bezel) * unit, (rows + 2.0 * bezel) * unit);
    let (area, _) = ui.allocate_exact_size(vec2(width, deck_size.y + 48.0), Sense::hover());
    super::stage(ui, area);
    let p = ui.painter_at(area);
    let lit = crate::controls::device_color(cx.d);
    let dragging = egui::DragAndDrop::has_payload_of_type::<Action>(ui.ctx());

    let deck = Rect::from_center_size(area.center(), deck_size);
    glow(&p, deck.shrink(10.0), 20, 56, lit.gamma_multiply(0.30));
    p.rect_filled(deck.translate(vec2(0.0, 5.0)), 16.0, theme::pal().key_side);
    p.rect_filled(deck, 16.0, theme::pal().deck);
    p.rect_stroke(
        deck,
        16.0,
        Stroke::new(1.0_f32, theme::pal().deck_edge),
        egui::StrokeKind::Inside,
    );
    // Recessed plate the switches sit in; the lighting pools here.
    let plate = deck.shrink(unit * bezel * 0.55);
    p.rect_filled(plate, 9.0, theme::pal().key_side);
    p.rect_filled(plate, 9.0, lit.gamma_multiply(0.07));

    let origin = deck.min + vec2(bezel * unit, bezel * unit);
    let gap = (unit * 0.055).max(1.5);
    let legend = egui::FontId::proportional((unit * 0.27).clamp(8.0, 12.5));
    let sub = egui::FontId::proportional((unit * 0.22).clamp(7.5, 10.0));
    let mut dropped = None;

    for (i, cap) in caps.iter().enumerate() {
        let key = EvdevKey(cap.code);
        let live = cap.code != 0 && !is_locked(key);
        let rect = Rect::from_min_size(
            origin + vec2(cap.x, cap.y) * unit,
            vec2(cap.w, cap.h) * unit,
        )
        .shrink(gap);
        let id = ui.id().with(("cap", layout as u8, i));
        let resp = ui.interact(rect, id, if live { Sense::click() } else { Sense::hover() });
        let bound = cx.binding(key).filter(|_| live);
        let is_sel = live && *selected == Some(key);
        let target = live && dragging && resp.dnd_hover_payload::<Action>().is_some();
        let hot = ui.ctx().animate_bool_with_time(
            id,
            live && (resp.hovered() || is_sel || target || resp.has_focus()),
            0.1,
        );
        let tint = bound.map(|b| mode_color(b.mode));

        // Backlight bleeding out from under the cap.
        p.rect_filled(rect.expand(gap), 5.0, lit.gamma_multiply(0.10 + 0.22 * hot));
        if is_sel || target {
            let ring = if target {
                theme::pal().ok
            } else {
                pal().accent
            };
            glow(&p, rect, 6, 14, ring.gamma_multiply(0.55));
        }
        // Skirt, then the top face lifted a little (more on hover).
        p.rect_filled(rect, 5.0, theme::pal().key_side);
        let face = Rect::from_min_max(
            rect.min + vec2(unit * 0.05, unit * 0.02 - hot),
            rect.max - vec2(unit * 0.05, unit * 0.13 + hot),
        );
        let mut top = theme::pal()
            .key_top
            .lerp_to_gamma(theme::pal().key_top_hi, hot);
        if let Some(c) = tint {
            top = top.lerp_to_gamma(c, 0.26);
        }
        if !live {
            top = top.gamma_multiply(0.6);
        }
        p.rect_filled(face, 4.0, top);
        // Light catching the top edge of the dish.
        p.line_segment(
            [
                face.left_top() + vec2(4.0, 0.5),
                face.right_top() + vec2(-4.0, 0.5),
            ],
            Stroke::new(1.0_f32, theme::pal().key_top_hi.gamma_multiply(0.9)),
        );
        if let Some(c) = tint {
            p.rect_filled(
                Rect::from_min_max(
                    pos2(face.left() + 4.0, face.bottom() - 3.0),
                    face.max - vec2(4.0, 1.0),
                ),
                1.0,
                c,
            );
        }
        if is_sel || target {
            p.rect_stroke(
                rect,
                5.0,
                Stroke::new(
                    2.0_f32,
                    if target {
                        theme::pal().ok
                    } else {
                        pal().accent
                    },
                ),
                egui::StrokeKind::Inside,
            );
        } else if resp.has_focus() {
            p.rect_stroke(
                rect,
                5.0,
                Stroke::new(1.0_f32, pal().text_muted),
                egui::StrokeKind::Inside,
            );
        }

        let ink = if !live {
            pal().text_faint
        } else if let Some(c) = tint {
            c.lerp_to_gamma(pal().text, 0.35)
        } else {
            pal().text_muted.lerp_to_gamma(pal().text, hot)
        };
        let dir = match cap.label {
            "←" => Some(vec2(-1.0, 0.0)),
            "→" => Some(vec2(1.0, 0.0)),
            "↑" => Some(vec2(0.0, -1.0)),
            "↓" => Some(vec2(0.0, 1.0)),
            _ => None,
        };
        if let Some(dir) = dir {
            super::arrow(&p, face.center(), dir, unit * 0.13, ink);
        } else {
            // Long legends (PrtSc, Pause) drop a size so they stay on the cap.
            let font = if cap.label.len() > 4 && cap.w < 1.25 {
                sub.clone()
            } else {
                legend.clone()
            };
            p.with_clip_rect(face).text(
                face.left_top() + vec2(unit * 0.12, unit * 0.09),
                egui::Align2::LEFT_TOP,
                cap.label,
                font,
                ink,
            );
        }
        // What it does now, printed small when the cap has room for it.
        if let Some(b) = bound {
            let galley = p.layout_no_wrap(short(b), sub.clone(), pal().text);
            if unit >= 40.0 && galley.size().x <= face.width() - unit * 0.2 {
                p.galley(
                    pos2(
                        face.left() + unit * 0.12,
                        face.bottom() - 5.0 - galley.size().y,
                    ),
                    galley,
                    pal().text,
                );
            }
        }

        if !live {
            if cap.code == 0 {
                resp.on_hover_text("Fn is handled inside the keyboard and cannot be remapped");
            }
            continue;
        }
        if let Some(action) = resp.dnd_release_payload::<Action>() {
            dropped = Some((key, *action));
        }
        if resp.clicked() {
            *selected = Some(key);
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(format!("{} — {}", key_name(key), super::describe(bound)));
    }

    if cx.remaps.capturing == Some(CaptureRole::Source) {
        super::detect_overlay(ui, area, "key");
    }
    dropped
}
