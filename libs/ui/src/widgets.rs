//! Small custom widgets shared by every page.

use crate::theme::{self, pal};
use egui::{pos2, vec2, Color32, CornerRadius, Rect, Response, Sense, Stroke, Ui};

/// Pill switch with an animated knob.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.15);
    if ui.is_rect_visible(rect) {
        let r = rect.height() / 2.0;
        let track = pal().surface_hover.lerp_to_gamma(pal().accent, t);
        ui.painter().rect_filled(rect, r, track);
        if resp.hovered() || !pal().dark {
            ui.painter().rect_stroke(
                rect,
                r,
                Stroke::new(1.0_f32, pal().border_hi),
                egui::StrokeKind::Inside,
            );
        }
        let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        ui.painter()
            .circle_filled(pos2(x, rect.center().y), r - 3.0, Color32::WHITE);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Segmented control. Returns the index clicked this frame (if it changed).
/// The selection highlight slides between segments.
pub fn segmented(
    ui: &mut Ui,
    id: egui::Id,
    labels: &[&str],
    selected: Option<usize>,
) -> Option<usize> {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let pad = 14.0;
    let widths: Vec<f32> = labels
        .iter()
        .map(|l| {
            ui.fonts(|f| {
                f.layout_no_wrap(l.to_string(), font.clone(), pal().text)
                    .size()
                    .x
            }) + pad * 2.0
        })
        .collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let (rect, _) = ui.allocate_exact_size(vec2(total, theme::CONTROL_H), Sense::hover());
    let painter = ui.painter_at(rect.expand(1.0));
    let radius = theme::CONTROL_RADIUS as f32;
    painter.rect_filled(rect, radius, pal().bg);
    painter.rect_stroke(
        rect,
        radius,
        Stroke::new(1.0_f32, pal().border),
        egui::StrokeKind::Inside,
    );

    let mut x = rect.left() + 3.0;
    let mut seg_rects = Vec::with_capacity(labels.len());
    for w in &widths {
        seg_rects.push(Rect::from_min_size(
            pos2(x, rect.top() + 3.0),
            vec2(*w, rect.height() - 6.0),
        ));
        x += w;
    }
    if let Some(sel) = selected.filter(|s| *s < seg_rects.len()) {
        let target = seg_rects[sel];
        let left = ui
            .ctx()
            .animate_value_with_time(id.with("l"), target.left(), 0.18);
        let width = ui
            .ctx()
            .animate_value_with_time(id.with("w"), target.width(), 0.18);
        let hl = Rect::from_min_size(pos2(left, target.top()), vec2(width, target.height()));
        painter.rect_filled(hl, 6.0, raised());
        painter.rect_stroke(
            hl,
            6.0,
            Stroke::new(1.0_f32, pal().border_hi),
            egui::StrokeKind::Inside,
        );
    }
    let mut clicked = None;
    for (i, (r, label)) in seg_rects.iter().zip(labels).enumerate() {
        let resp = ui
            .interact(*r, id.with(i), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let active = selected == Some(i);
        let color = if active {
            pal().text
        } else if resp.hovered() {
            pal().text.gamma_multiply(0.85)
        } else {
            pal().text_muted
        };
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            *label,
            font.clone(),
            color,
        );
        if resp.clicked() && !active {
            clicked = Some(i);
        }
    }
    clicked
}

/// Fill for a selected / lifted element inside a control.
pub fn raised() -> Color32 {
    if pal().dark {
        pal().surface_hi
    } else {
        pal().surface
    }
}

/// A settings row: label (+ optional hint) on the left, control right-aligned,
/// both vertically centered in `ROW_H`.
pub fn row(ui: &mut Ui, label: &str, hint: Option<&str>, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        vec2(w, theme::ROW_H),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(vec2(w, theme::ROW_H));
            // Exact text-block height, so the layout can center it.
            let gap = 2.0;
            let mut h = ui.text_style_height(&egui::TextStyle::Body);
            if hint.is_some() {
                h += gap + ui.text_style_height(&egui::TextStyle::Small);
            }
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = gap;
                    ui.label(egui::RichText::new(label).color(pal().text));
                    if let Some(h) = hint {
                        ui.label(egui::RichText::new(h).small().color(pal().text_faint));
                    }
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
        },
    );
}

/// Hairline between rows inside a card.
pub fn divider(ui: &mut Ui) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, pal().border);
}

/// Card with a title line and optional trailing widgets.
pub fn section(
    ui: &mut Ui,
    title: &str,
    trailing: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui),
) {
    theme::card().show(ui, |ui| {
        let w = ui.available_width();
        ui.set_width(w);
        ui.allocate_ui_with_layout(
            vec2(w, theme::CONTROL_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_size(vec2(w, theme::CONTROL_H));
                ui.label(
                    egui::RichText::new(title)
                        .size(15.0)
                        .strong()
                        .color(pal().text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
            },
        );
        ui.add_space(theme::SPACE_XS);
        body(ui);
    });
    ui.add_space(theme::SPACE_MD);
}

/// Small "i" badge that explains a control on hover.
pub fn info(ui: &mut Ui, text: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
    let hot = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered(), 0.1);
    let color = theme::pal().text_faint.lerp_to_gamma(pal().accent, hot);
    ui.painter()
        .circle_stroke(rect.center(), 6.5, Stroke::new(1.1_f32, color));
    ui.painter().text(
        rect.center() + vec2(0.0, 0.5),
        egui::Align2::CENTER_CENTER,
        "i",
        egui::FontId::proportional(10.0),
        color,
    );
    resp.on_hover_text(text)
}

pub fn hint(ui: &mut Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .small()
            .color(theme::pal().text_faint),
    );
}

/// Small status pill.
pub fn pill(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let font = egui::FontId::proportional(11.5);
    let galley = ui.fonts(|f| f.layout_no_wrap(text.into(), font, color));
    let size = vec2(galley.size().x + 24.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, rect.height() / 2.0, color.gamma_multiply(0.14));
    let dot = pos2(rect.left() + 10.0, rect.center().y);
    ui.painter().circle_filled(dot, 3.0, color);
    ui.painter().galley(
        pos2(rect.left() + 17.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    resp
}

/// Flat icon button drawn by `paint` inside a square hit area.
pub fn icon_button(
    ui: &mut Ui,
    size: f32,
    tooltip: &str,
    enabled: bool,
    paint: impl FnOnce(&egui::Painter, Rect, Color32),
) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), sense);
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.1);
    if hover > 0.0 {
        ui.painter()
            .rect_filled(rect, 6.0, pal().surface_hover.gamma_multiply(hover));
    }
    let color = if !enabled {
        theme::pal().text_faint.gamma_multiply(0.5)
    } else {
        pal().text_muted.lerp_to_gamma(pal().text, hover)
    };
    paint(ui.painter(), rect.shrink(size * 0.28), color);
    let resp = if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    resp.on_hover_text(tooltip)
}

/// Standard text button; shares height and minimum width with every other.
pub fn button(ui: &mut Ui, text: &str) -> Response {
    ui.add(egui::Button::new(text).min_size(vec2(theme::BUTTON_MIN_W, theme::CONTROL_H)))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Primary (accent) button.
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(pal().on_accent).strong())
            .fill(pal().accent)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(theme::CONTROL_RADIUS))
            .min_size(vec2(theme::BUTTON_MIN_W, theme::CONTROL_H)),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The one dropdown look: fixed width, left-aligned value, chevron.
/// Callers add `.close_behavior` etc. and `.show_ui`.
pub fn dropdown(
    id_salt: impl std::hash::Hash,
    selected: impl Into<egui::WidgetText>,
    width: f32,
) -> egui::ComboBox {
    egui::ComboBox::from_id_salt(id_salt)
        .width(width)
        .selected_text(selected)
        .truncate()
        .icon(|ui, rect, _, open, _| {
            let r = Rect::from_center_size(rect.center(), vec2(9.0, 6.0));
            let c = if open { pal().text } else { pal().text_muted };
            icon_chevron(ui.painter(), r, c, !open);
        })
}

/// A button drawn like `dropdown`, for menus with custom content (a combo
/// box popup scrolls and clips anything taller than its first frame).
pub fn dropdown_button(ui: &mut Ui, text: &str, width: f32, open: bool) -> Response {
    let resp = ui
        .allocate_ui_with_layout(
            vec2(width, theme::CONTROL_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                let mut b = egui::Button::new("").min_size(vec2(width, theme::CONTROL_H));
                if open {
                    b = b.stroke(Stroke::new(1.0_f32, pal().accent));
                }
                ui.add(b)
            },
        )
        .inner;
    // Painted, not the button's own label, so it is left-aligned like a combo box.
    let color = if ui.is_enabled() {
        pal().text
    } else {
        pal().text_muted
    };
    let galley = ui.painter().layout_no_wrap(
        text.into(),
        egui::TextStyle::Button.resolve(ui.style()),
        color,
    );
    ui.painter().galley(
        pos2(
            resp.rect.left() + ui.spacing().button_padding.x,
            resp.rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    let chev = Rect::from_center_size(
        resp.rect.right_center() - vec2(ui.spacing().button_padding.x + 7.0, 0.0),
        vec2(9.0, 6.0),
    );
    let c = if open { pal().text } else { pal().text_muted };
    icon_chevron(ui.painter(), chev, c, !open);
    resp
}

/// Single-line text input at control height.
pub fn text_field(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(hint)
            .margin(egui::Margin::symmetric(10, 8))
            .vertical_align(egui::Align::Center)
            .min_size(vec2(width, theme::CONTROL_H))
            // `desired_width` excludes the margins; `width` is the outer size.
            .desired_width(width - 20.0),
    )
}

/// Fade + slight rise when `key` changes (page / tab transitions).
pub fn page_transition(ui: &mut Ui, key: impl std::hash::Hash) {
    let id = egui::Id::new("page_transition");
    let key_id = egui::Id::new(key);
    let now = ui.input(|i| i.time);
    let started = match ui.ctx().data(|d| d.get_temp::<(egui::Id, f64)>(id)) {
        Some((k, t)) if k == key_id => t,
        _ => {
            ui.ctx().data_mut(|d| d.insert_temp(id, (key_id, now)));
            now
        }
    };
    let t = theme::ease(((now - started) as f32) / 0.22);
    if t < 1.0 {
        ui.ctx().request_repaint();
    }
    ui.multiply_opacity(t);
    ui.add_space((1.0 - t) * 10.0);
}

// ── Painter icons ────────────────────────────────────────────────

pub fn icon_close(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.6_f32, c);
    p.line_segment([r.left_top(), r.right_bottom()], s);
    p.line_segment([r.right_top(), r.left_bottom()], s);
}

pub fn icon_chevron(p: &egui::Painter, r: Rect, c: Color32, down: bool) {
    let s = Stroke::new(1.6_f32, c);
    let (a, b, d) = if down {
        (
            r.left_center() + vec2(0.0, -2.0),
            pos2(r.center().x, r.bottom() - 2.0),
            r.right_center() + vec2(0.0, -2.0),
        )
    } else {
        (
            r.left_center() + vec2(0.0, 2.0),
            pos2(r.center().x, r.top() + 2.0),
            r.right_center() + vec2(0.0, 2.0),
        )
    };
    p.line_segment([a, b], s);
    p.line_segment([b, d], s);
}

pub fn icon_plus(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.6_f32, c);
    p.line_segment([r.center_top(), r.center_bottom()], s);
    p.line_segment([r.left_center(), r.right_center()], s);
}

pub fn icon_grip(p: &egui::Painter, r: Rect, c: Color32) {
    for row in 0..3 {
        for col in 0..2 {
            let x = r.center().x + (col as f32 - 0.5) * 5.0;
            let y = r.center().y + (row as f32 - 1.0) * 5.0;
            p.circle_filled(pos2(x, y), 1.4, c);
        }
    }
}

/// Pip, the mascot, filling the square `r`; the caller paints the tile behind.
/// Shapes and colours mirror `branding/parts/`.
pub fn pip(p: &egui::Painter, r: Rect) {
    const NAVY: Color32 = Color32::from_rgb(30, 41, 59);
    const ORANGE: Color32 = Color32::from_rgb(255, 167, 38);
    const EYE: Color32 = Color32::from_rgb(31, 27, 46);
    let blush = Color32::from_rgba_unmultiplied(255, 122, 154, 128);
    // The art is on a 120-unit canvas; Pip sits inside x 10..110, y 13..113.
    let s = r.width() / 100.0;
    let at = |x: f32, y: f32| pos2(r.left() + (x - 10.0) * s, r.top() + (y - 13.0) * s);
    let oval = |x: f32, y: f32, rx: f32, ry: f32, deg: f32, fill: Color32| {
        let (sin, cos) = deg.to_radians().sin_cos();
        let pts = (0..32)
            .map(|i| {
                let (ty, tx) = (i as f32 / 32.0 * std::f32::consts::TAU).sin_cos();
                let (dx, dy) = (rx * tx, ry * ty);
                at(x + dx * cos - dy * sin, y + dx * sin + dy * cos)
            })
            .collect();
        p.add(egui::Shape::convex_polygon(pts, fill, Stroke::NONE));
    };
    oval(48.0, 100.0, 9.0, 4.5, 0.0, ORANGE);
    oval(72.0, 100.0, 9.0, 4.5, 0.0, ORANGE);
    oval(29.0, 72.0, 6.5, 14.0, 18.0, NAVY);
    oval(91.0, 72.0, 6.5, 14.0, -18.0, NAVY);
    for (x, y) in [(52.0, 26.0), (65.0, 23.0), (71.0, 29.0)] {
        p.line_segment([at(60.0, 36.0), at(x, y)], Stroke::new(3.6 * s, NAVY));
    }
    oval(60.0, 67.0, 31.0, 34.0, 0.0, NAVY);
    oval(49.0, 60.0, 13.0, 13.0, 0.0, Color32::WHITE);
    oval(71.0, 60.0, 13.0, 13.0, 0.0, Color32::WHITE);
    oval(60.0, 77.0, 21.0, 22.0, 0.0, Color32::WHITE);
    for x in [49.0, 71.0] {
        oval(x, 61.0, 5.0, 6.0, 0.0, EYE);
        p.circle_filled(at(x + 1.7, 58.8), 1.9 * s, Color32::WHITE);
    }
    oval(45.5, 70.0, 4.4, 2.6, 0.0, blush);
    oval(74.5, 70.0, 4.4, 2.6, 0.0, blush);
    p.add(egui::Shape::convex_polygon(
        vec![at(54.3, 65.3), at(65.7, 65.3), at(60.0, 73.2)],
        ORANGE,
        Stroke::NONE,
    ));
}

pub fn icon_gear(p: &egui::Painter, r: Rect, c: Color32) {
    let center = r.center();
    let rad = r.width() / 2.0;
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::TAU / 8.0;
        let dir = vec2(a.cos(), a.sin());
        p.line_segment(
            [center + dir * rad * 0.6, center + dir * rad],
            Stroke::new(2.2_f32, c),
        );
    }
    p.circle_stroke(center, rad * 0.62, Stroke::new(1.6_f32, c));
    p.circle_filled(center, rad * 0.22, c);
}

pub fn icon_record(p: &egui::Painter, r: Rect, c: Color32) {
    p.circle_filled(r.center(), r.width() / 2.0, c);
}

pub fn icon_stop(p: &egui::Painter, r: Rect, c: Color32) {
    p.rect_filled(r.shrink(1.0), 2.0, c);
}

/// Device-kind glyphs for the rail.
pub fn icon_device(p: &egui::Painter, r: Rect, c: Color32, kind: hako_core::device::DeviceKind) {
    use hako_core::device::DeviceKind;
    let s = Stroke::new(1.5_f32, c);
    match kind {
        DeviceKind::Keyboard => {
            let body = Rect::from_center_size(r.center(), vec2(r.width(), r.height() * 0.62));
            p.rect_stroke(body, 3.0, s, egui::StrokeKind::Inside);
            for row in 0..2 {
                for col in 0..5 {
                    let x = body.left() + 4.0 + col as f32 * (body.width() - 8.0) / 4.0;
                    let y = body.top() + 4.5 + row as f32 * 3.6;
                    p.circle_filled(pos2(x, y), 0.9, c);
                }
            }
            p.line_segment(
                [
                    pos2(body.left() + 6.0, body.bottom() - 3.5),
                    pos2(body.right() - 6.0, body.bottom() - 3.5),
                ],
                s,
            );
        }
        DeviceKind::Mouse => {
            let body = Rect::from_center_size(r.center(), vec2(r.width() * 0.62, r.height()));
            p.rect_stroke(body, body.width() / 2.0, s, egui::StrokeKind::Inside);
            p.line_segment(
                [
                    body.center_top(),
                    pos2(body.center().x, body.top() + body.height() * 0.38),
                ],
                s,
            );
            p.line_segment(
                [
                    pos2(body.left(), body.top() + body.height() * 0.38),
                    pos2(body.right(), body.top() + body.height() * 0.38),
                ],
                Stroke::new(1.0_f32, c.gamma_multiply(0.6)),
            );
        }
        DeviceKind::Headset => {
            let center = pos2(r.center().x, r.center().y + 1.0);
            let rad = r.width() * 0.42;
            let pts: Vec<_> = (0..=16)
                .map(|i| {
                    let a = std::f32::consts::PI + i as f32 / 16.0 * std::f32::consts::PI;
                    center + vec2(a.cos(), a.sin()) * rad
                })
                .collect();
            p.add(egui::Shape::line(pts, s));
            for side in [-1.0, 1.0] {
                let cup = Rect::from_center_size(center + vec2(side * rad, 3.0), vec2(5.0, 9.0));
                p.rect_filled(cup, 2.0, c);
            }
        }
        DeviceKind::Other => {
            p.circle_stroke(r.center(), r.width() / 2.0, s);
        }
    }
}

/// Compact horizontal battery gauge.
pub fn battery(ui: &mut Ui, percent: Option<u8>, charging: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(26.0, 12.0), Sense::hover());
    paint_battery(ui.painter(), rect, percent, charging);
}

pub fn paint_battery(p: &egui::Painter, rect: Rect, percent: Option<u8>, charging: bool) {
    let body = Rect::from_min_size(rect.min, vec2(rect.width() - 3.0, rect.height()));
    p.rect_stroke(
        body,
        3.0,
        Stroke::new(1.2_f32, pal().text_muted),
        egui::StrokeKind::Inside,
    );
    p.rect_filled(
        Rect::from_min_size(
            pos2(body.right() + 0.5, body.center().y - 2.5),
            vec2(2.0, 5.0),
        ),
        1.0,
        pal().text_muted,
    );
    if let Some(pct) = percent {
        let inner = body.shrink(2.5);
        let fill = Rect::from_min_size(
            inner.min,
            vec2(inner.width() * pct as f32 / 100.0, inner.height()),
        );
        p.rect_filled(fill, 1.5, battery_color(pct, charging));
    }
}

pub fn battery_color(pct: u8, charging: bool) -> Color32 {
    if charging {
        pal().accent
    } else if pct <= 15 {
        theme::pal().danger
    } else if pct <= 30 {
        theme::pal().warn
    } else {
        theme::pal().ok
    }
}

// ── Notifications ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Info,
    Warn,
    Error,
}

impl Tone {
    fn color(self) -> Color32 {
        match self {
            Tone::Ok => pal().ok,
            Tone::Info => pal().accent,
            Tone::Warn => pal().warn,
            Tone::Error => pal().danger,
        }
    }

    /// Seconds on screen; problems stay long enough to read.
    fn life(self) -> f64 {
        match self {
            Tone::Ok | Tone::Info => 3.5,
            Tone::Warn => 6.0,
            Tone::Error => 9.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub tone: Tone,
    pub text: String,
    /// First frame it was drawn.
    pub shown: Option<f64>,
}

/// A newer release, offered on top of the toasts until the user answers.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateOffer {
    pub version: String,
    /// Download and install in progress.
    pub busy: bool,
}

const TOAST_W: f32 = 340.0;
const MAX_TOASTS: usize = 4;

/// Toast stack floating over the bottom-right corner; it never moves the
/// layout underneath. Expired or dismissed toasts are removed from `list`.
/// Returns the answer to `offer`: `Some(true)` = update now, `Some(false)` = later.
pub fn toasts(
    ctx: &egui::Context,
    list: &mut Vec<Toast>,
    offer: Option<&UpdateOffer>,
) -> Option<bool> {
    let excess = list.len().saturating_sub(MAX_TOASTS);
    list.drain(..excess);
    if list.is_empty() && offer.is_none() {
        return None;
    }
    let now = ctx.input(|i| i.time);
    let mut closed = None;
    let mut answer = None;
    egui::Area::new(egui::Id::new("toasts"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::RIGHT_BOTTOM, vec2(-20.0, -20.0))
        .show(ctx, |ui| {
            ui.set_width(TOAST_W);
            ui.spacing_mut().item_spacing.y = theme::SPACE_SM;
            if let Some(o) = offer {
                answer = update_card(ui, o);
            }
            for (i, t) in list.iter_mut().enumerate() {
                let age = now - *t.shown.get_or_insert(now);
                let fade = theme::ease((t.tone.life() - age) as f32 / 0.3)
                    * theme::ease(age as f32 / 0.15);
                let (resp, close) = ui
                    .scope(|ui| {
                        ui.multiply_opacity(fade);
                        toast_card(ui, t)
                    })
                    .inner;
                if close {
                    closed = Some(i);
                } else if resp.hovered() {
                    // Hovering holds a toast so it can be read.
                    t.shown = Some(now - 0.3);
                }
            }
        });
    if let Some(i) = closed {
        list.remove(i);
    }
    list.retain(|t| t.shown.is_none_or(|s| now - s < t.tone.life()));
    ctx.request_repaint_after(std::time::Duration::from_millis(50));
    answer
}

/// Persistent card asking to restart into a new release.
fn update_card(ui: &mut Ui, o: &UpdateOffer) -> Option<bool> {
    let mut answer = None;
    egui::Frame::new()
        .fill(pal().surface)
        .stroke(Stroke::new(1.0_f32, pal().accent))
        .corner_radius(CornerRadius::same(10))
        .shadow(ui.visuals().popup_shadow)
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(TOAST_W - 28.0);
            ui.label(
                egui::RichText::new(format!("Hako v{} is available", o.version))
                    .color(pal().text)
                    .strong(),
            );
            ui.add_space(theme::SPACE_SM);
            if o.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        egui::RichText::new("Downloading and installing…").color(pal().text_muted),
                    );
                });
                return;
            }
            ui.label(
                egui::RichText::new("Hako restarts to finish. Audio routing pauses for a moment.")
                    .color(pal().text_muted),
            );
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                if primary_button(ui, "Restart and update").clicked() {
                    answer = Some(true);
                }
                if button(ui, "Later").clicked() {
                    answer = Some(false);
                }
            });
        });
    answer
}

/// One toast; returns its response and whether it was dismissed.
fn toast_card(ui: &mut Ui, t: &Toast) -> (Response, bool) {
    let color = t.tone.color();
    let mut close = false;
    let frame = egui::Frame::new()
        .fill(pal().surface)
        .stroke(Stroke::new(1.0_f32, pal().border_hi))
        .corner_radius(CornerRadius::same(10))
        .shadow(ui.visuals().popup_shadow)
        .inner_margin(egui::Margin {
            left: 14,
            right: 6,
            top: 8,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(TOAST_W - 20.0);
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(vec2(8.0, 24.0), Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, color);
                ui.add_space(2.0);
                ui.allocate_ui_with_layout(
                    vec2(TOAST_W - 74.0, 24.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.add_space(3.0);
                        ui.add(
                            egui::Label::new(egui::RichText::new(&t.text).color(pal().text)).wrap(),
                        );
                    },
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    close = icon_button(ui, 24.0, "Dismiss", true, icon_close).clicked();
                });
            });
        });
    (frame.response.interact(Sense::hover()), close)
}
