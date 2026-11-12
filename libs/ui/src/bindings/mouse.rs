//! Top-down mouse illustration with labelled callouts.

use super::library::Action;
use super::{describe, glow, is_locked, key_name, mode_color, Cx};
use crate::theme::{self, pal};
use egui::{pos2, vec2, Color32, Pos2, Rect, Sense, Stroke, Ui};
use hako_core::remap::{CaptureRole, EvdevKey};

/// Buttons drawn on the body, in callout order (left column, then right).
const DRAWN: [EvdevKey; 5] = [
    EvdevKey::BTN_LEFT,
    EvdevKey::BTN_EXTRA,
    EvdevKey::BTN_SIDE,
    EvdevKey::BTN_RIGHT,
    EvdevKey::BTN_MIDDLE,
];
/// Buttons some mice have that the picture does not place.
const EXTRA: [EvdevKey; 3] = [
    EvdevKey::BTN_FORWARD,
    EvdevKey::BTN_BACK,
    EvdevKey::BTN_TASK,
];

/// Position-based names (the core labels describe the evdev code instead).
pub(super) fn name(key: EvdevKey) -> Option<&'static str> {
    Some(match key {
        EvdevKey::BTN_LEFT => "Left click",
        EvdevKey::BTN_RIGHT => "Right click",
        EvdevKey::BTN_MIDDLE => "Wheel click",
        EvdevKey::BTN_EXTRA => "Side forward",
        EvdevKey::BTN_SIDE => "Side back",
        EvdevKey::BTN_FORWARD => "Extra forward",
        EvdevKey::BTN_BACK => "Extra back",
        EvdevKey::BTN_TASK => "Task button",
        _ => return None,
    })
}

/// Egg-shaped shell outline: shorter nose, longer palm, slightly squared.
fn shell(body: Rect, waist: f32) -> Vec<Pos2> {
    let c = pos2(body.center().x, body.top() + body.height() * waist);
    let pow = |v: f32| v.signum() * v.abs().powf(0.82);
    (0..72)
        .map(|i| {
            let a = i as f32 / 72.0 * std::f32::consts::TAU;
            let ry = if a.sin() < 0.0 {
                c.y - body.top()
            } else {
                body.bottom() - c.y
            };
            c + vec2(pow(a.cos()) * body.width() / 2.0, pow(a.sin()) * ry)
        })
        .collect()
}

/// Draws the mouse; returns an action dropped onto a button this frame.
pub(super) fn hero(
    ui: &mut Ui,
    cx: &mut Cx,
    selected: &mut Option<EvdevKey>,
) -> Option<(EvdevKey, Action)> {
    // Undrawn buttons: the usual extras, anything bound, and a detected one.
    let mut extra: Vec<EvdevKey> = EXTRA.to_vec();
    for k in cx.remaps.bindings.iter().map(|b| b.source).chain(*selected) {
        if !DRAWN.contains(&k) && !extra.contains(&k) {
            extra.push(k);
        }
    }
    let right_rows = 2 + extra.len();
    let width = ui.available_width();
    let height = (64.0 + right_rows as f32 * 52.0).max(380.0);
    let (area, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    super::stage(ui, area);
    let p = ui.painter_at(area);
    let lit = crate::controls::device_color(cx.d);
    let dragging = egui::DragAndDrop::has_payload_of_type::<Action>(ui.ctx());

    let body = Rect::from_center_size(
        pos2(area.center().x, area.top() + 190.0),
        vec2(172.0, 300.0),
    );
    let outline = shell(body, 0.40);
    let split_y = body.top() + body.height() * 0.43;
    let mid = body.center().x;
    let wheel = Rect::from_center_size(pos2(mid, body.top() + 74.0), vec2(16.0, 46.0));
    let side = |from: f32, to: f32| {
        Rect::from_min_max(
            pos2(body.left() - 5.0, body.top() + body.height() * from),
            pos2(body.left() + 9.0, body.top() + body.height() * to),
        )
    };
    // Hit area and callout anchor per drawn button, same order as DRAWN.
    let regions = [
        Rect::from_min_max(body.min, pos2(mid - 14.0, split_y)),
        side(0.40, 0.52),
        side(0.54, 0.68),
        Rect::from_min_max(pos2(mid + 14.0, body.top()), pos2(body.right(), split_y)),
        wheel.expand(4.0),
    ];
    let anchors = [
        pos2(mid - 48.0, body.top() + 62.0),
        regions[1].left_center(),
        regions[2].left_center(),
        pos2(mid + 48.0, body.top() + 62.0),
        pos2(wheel.right(), wheel.center().y),
    ];

    // Callout cards: three on the left, the rest on the right.
    let card_w = ((width - body.width() - 112.0) / 2.0).clamp(128.0, 250.0);
    let card = |left: bool, y: f32| {
        let x = if left {
            area.left() + 16.0
        } else {
            area.right() - 16.0 - card_w
        };
        Rect::from_min_size(pos2(x, y - 22.0), vec2(card_w, 44.0))
    };
    let right_y = |i: usize| {
        let more = if i >= 2 { 22.0 } else { 0.0 };
        area.top() + 54.0 + i as f32 * 52.0 + more
    };
    let cards = [
        card(true, body.top() + 50.0),
        card(true, body.top() + 132.0),
        card(true, body.top() + 196.0),
        card(false, right_y(0)),
        card(false, right_y(1)),
    ];

    // Shell.
    glow(&p, body.shrink(34.0), 60, 70, lit.gamma_multiply(0.34));
    let shadow: Vec<Pos2> = outline.iter().map(|q| *q + vec2(0.0, 7.0)).collect();
    p.add(egui::Shape::convex_polygon(
        shadow,
        theme::pal().key_side,
        Stroke::NONE,
    ));
    p.add(egui::Shape::convex_polygon(
        outline.clone(),
        theme::pal().deck,
        Stroke::new(1.2_f32, theme::pal().deck_edge),
    ));
    // Palm sheen: a lighter inner shell below the buttons.
    let palm = Rect::from_min_max(
        pos2(body.left() + 24.0, split_y + 18.0),
        body.max - vec2(24.0, 22.0),
    );
    p.add(egui::Shape::convex_polygon(
        shell(palm, 0.34),
        theme::pal().key_top.gamma_multiply(0.30),
        Stroke::NONE,
    ));

    let mut dropped = None;
    let mut spots: Vec<_> = DRAWN
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, cards[i], Some((regions[i], anchors[i]))))
        .collect();
    spots.extend(
        extra
            .iter()
            .enumerate()
            .map(|(i, k)| (*k, card(false, right_y(2 + i)), None)),
    );
    p.text(
        pos2(area.right() - 16.0 - card_w, right_y(2) - 40.0),
        egui::Align2::LEFT_CENTER,
        "MORE BUTTONS",
        egui::FontId::proportional(10.0),
        pal().text_faint,
    );

    for (i, (key, card, drawn)) in spots.into_iter().enumerate() {
        let locked = is_locked(key);
        let sense = if locked {
            Sense::hover()
        } else {
            Sense::click()
        };
        let id = ui.id().with(("mouse_btn", key.0));
        let cresp = ui.interact(card, id.with("card"), sense);
        let bresp = drawn.map(|(region, _)| ui.interact(region, id, sense));
        let over = |f: &dyn Fn(&egui::Response) -> bool| f(&cresp) || bresp.as_ref().is_some_and(f);
        let bound = cx.binding(key).filter(|_| !locked);
        let is_sel = *selected == Some(key);
        let target = !locked && dragging && over(&|r| r.dnd_hover_payload::<Action>().is_some());
        let hot = ui.ctx().animate_bool_with_time(
            id,
            over(&|r| r.hovered() || r.has_focus()) || is_sel || target,
            0.12,
        );
        let tint = bound.map_or(pal().accent, |b| mode_color(b.mode));
        let ring = if target { theme::pal().ok } else { tint };
        let mark = if bound.is_some() { 0.30 } else { 0.0 };
        let face = theme::pal()
            .key_top
            .lerp_to_gamma(tint, mark)
            .lerp_to_gamma(ring, 0.30 * hot);
        let edge = theme::pal()
            .deck_edge
            .lerp_to_gamma(ring, hot.max(mark * 2.0));

        if let Some((region, anchor)) = drawn {
            match i {
                // Main buttons: the shell itself, clipped to this half.
                0 | 3 => {
                    p.with_clip_rect(region).add(egui::Shape::convex_polygon(
                        outline.clone(),
                        face,
                        Stroke::new(1.5_f32, edge),
                    ));
                    let inner = if i == 0 {
                        region.right()
                    } else {
                        region.left()
                    };
                    p.line_segment(
                        [
                            pos2(inner, region.top() + 12.0),
                            pos2(inner, region.bottom()),
                        ],
                        Stroke::new(1.0_f32, edge),
                    );
                    p.line_segment(
                        [region.left_bottom(), region.right_bottom()],
                        Stroke::new(1.0_f32, edge),
                    );
                }
                4 => {
                    p.rect_filled(wheel.expand2(vec2(6.0, 8.0)), 11.0, theme::pal().key_side);
                    p.rect_filled(wheel, 7.0, face);
                    p.rect_stroke(
                        wheel,
                        7.0,
                        Stroke::new(1.2_f32, edge),
                        egui::StrokeKind::Inside,
                    );
                    for k in 0..5 {
                        let y = wheel.top() + 9.0 + k as f32 * 7.0;
                        p.line_segment(
                            [pos2(wheel.left() + 4.0, y), pos2(wheel.right() - 4.0, y)],
                            Stroke::new(1.0_f32, theme::pal().key_side),
                        );
                    }
                }
                _ => {
                    p.rect_filled(region.translate(vec2(0.0, 2.0)), 5.0, theme::pal().key_side);
                    p.rect_filled(region, 5.0, face);
                    p.rect_stroke(
                        region,
                        5.0,
                        Stroke::new(1.2_f32, edge),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            // Leader line: out from the button, then level into the card.
            let left = card.center().x < mid;
            let end = if left {
                card.right_center()
            } else {
                card.left_center()
            };
            let elbow = pos2(end.x + if left { 18.0 } else { -18.0 }, end.y);
            let line = Stroke::new(1.0_f32, pal().border_hi.lerp_to_gamma(ring, hot));
            p.line_segment([anchor, elbow], line);
            p.line_segment([elbow, end], line);
            p.circle_filled(anchor, 3.0, theme::pal().stage);
            p.circle_stroke(
                anchor,
                3.0,
                Stroke::new(
                    1.5_f32,
                    pal().border_hi.lerp_to_gamma(ring, hot.max(mark * 3.0)),
                ),
            );
        }

        // Card.
        if is_sel || target {
            glow(&p, card, 10, 14, ring.gamma_multiply(0.35));
        }
        p.rect_filled(
            card,
            10.0,
            pal().surface.lerp_to_gamma(pal().surface_hi, hot),
        );
        p.rect_stroke(
            card,
            10.0,
            Stroke::new(
                if is_sel || target { 1.5_f32 } else { 1.0 },
                pal()
                    .border
                    .lerp_to_gamma(ring, if is_sel || target { 1.0 } else { 0.35 * hot }),
            ),
            egui::StrokeKind::Inside,
        );
        if bound.is_some() {
            p.rect_filled(
                Rect::from_min_size(
                    card.left_top() + vec2(0.0, 10.0),
                    vec2(3.0, card.height() - 20.0),
                ),
                1.5,
                tint,
            );
        }
        // Text that does not fit ends in "…"; the full line is in the tooltip.
        let room = card.width() - 24.0;
        let line = |text: String, size: f32, color: Color32, y: f32| {
            let mut job = egui::text::LayoutJob::simple_singleline(
                text,
                egui::FontId::proportional(size),
                color,
            );
            job.wrap = egui::text::TextWrapping::truncate_at_width(room);
            let galley = p.fonts(|f| f.layout_job(job));
            let cut = galley.elided;
            p.galley(
                pos2(
                    card.left() + 14.0,
                    card.center().y + y - galley.size().y / 2.0,
                ),
                galley,
                color,
            );
            cut
        };
        let title = name(key).map_or_else(|| key_name(key), str::to_string);
        let title_cut = line(
            title.clone(),
            13.0,
            if locked { pal().text_muted } else { pal().text },
            -8.0,
        );
        let (summary, color): (String, Color32) = if locked {
            ("Primary click · locked".into(), pal().text_faint)
        } else {
            (
                describe(bound),
                if bound.is_some() {
                    tint
                } else {
                    pal().text_faint
                },
            )
        };
        let summary_cut = line(summary.clone(), 11.0, color, 9.0);
        let cresp = if title_cut || summary_cut {
            cresp.on_hover_text(format!("{title}\n{summary}"))
        } else {
            cresp
        };

        if locked {
            cresp.on_hover_text("Left click stays fixed so a bad binding can never lock you out");
            continue;
        }
        for resp in std::iter::once(cresp).chain(bresp) {
            if let Some(action) = resp.dnd_release_payload::<Action>() {
                dropped = Some((key, *action));
            }
            if resp.clicked() {
                *selected = Some(key);
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
    }

    // DPI button and logo are decoration; the logo carries the lighting color.
    p.rect_filled(
        Rect::from_center_size(pos2(mid, wheel.bottom() + 22.0), vec2(12.0, 18.0)),
        4.0,
        theme::pal().key_side,
    );
    let logo = pos2(mid, body.bottom() - 74.0);
    p.circle_filled(logo, 22.0, lit.gamma_multiply(0.10));
    p.circle_filled(logo, 9.0, lit.gamma_multiply(0.85));

    if cx.remaps.capturing == Some(CaptureRole::Source) {
        super::detect_overlay(ui, area, "button");
    }
    dropped
}
