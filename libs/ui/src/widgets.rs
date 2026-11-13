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

/// The one slider look: an accent fill and a solid knob. egui paints its knob
/// in the rail's colour slot, so here its own knob is reduced to the rail
/// colour; call `knob` with the value's position afterwards to paint the real one.
pub fn slider(ui: &mut Ui, s: egui::Slider, width: f32) -> Response {
    ui.scope(|ui| {
        ui.spacing_mut().slider_width = width;
        ui.spacing_mut().interact_size.y = 18.0;
        let vis = ui.visuals_mut();
        vis.selection.bg_fill = pal().accent;
        let w = &mut vis.widgets;
        w.inactive.bg_fill = pal().surface_hover;
        w.hovered.bg_fill = Color32::TRANSPARENT;
        w.active.bg_fill = Color32::TRANSPARENT;
        for state in [
            &mut w.noninteractive,
            &mut w.inactive,
            &mut w.hovered,
            &mut w.active,
        ] {
            state.fg_stroke = Stroke::NONE;
        }
        ui.add(s)
    })
    .inner
}

/// Where `v` sits in `min..=max` (0..1), on a log scale if `log`.
pub fn fraction(v: f32, min: f32, max: f32, log: bool) -> f32 {
    if max <= min {
        return 0.0;
    }
    if log && min > 0.0 {
        (v / min).ln() / (max / min).ln()
    } else {
        (v - min) / (max - min)
    }
}

/// Track span of a `slider` (egui insets it by the knob radius).
pub fn track(resp: &Response) -> egui::Rangef {
    resp.rect.x_range().shrink(resp.rect.height() / 2.5)
}

/// Solid knob at fraction `t` of a `slider`'s track; grows under the pointer.
pub fn knob(ui: &Ui, resp: &Response, t: f32) {
    let x = egui::lerp(track(resp), t.clamp(0.0, 1.0));
    let hot = ui.ctx().animate_bool_with_time(
        resp.id.with("knob"),
        resp.hovered() || resp.dragged(),
        0.12,
    );
    let c = pos2(x, resp.rect.center().y);
    let fill = if ui.is_enabled() {
        Color32::WHITE
    } else {
        pal().text_faint
    };
    let r = 7.5 + 1.5 * hot;
    ui.painter()
        .circle_filled(c + vec2(0.0, 1.0), r, Color32::from_black_alpha(60));
    ui.painter()
        .circle(c, r, fill, Stroke::new(1.0_f32, pal().border_hi));
}

/// A number readout beside a control, in the body face.
pub fn value_text(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).color(pal().text)
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
/// both vertically centered in a row at least `ROW_H` tall. The text wraps in
/// the space the control leaves (its width is remembered from the last
/// frame) and the row grows to fit.
pub fn row(ui: &mut Ui, label: &str, hint: Option<&str>, add: impl FnOnce(&mut Ui)) {
    let w = ui.available_width();
    let ctl_id = ui.id().with(("row_ctl", label));
    let ctl_w = ui.data(|m| m.get_temp::<f32>(ctl_id)).unwrap_or(0.0);
    let text_w = (w - ctl_w - theme::SPACE_LG).max(w * 0.35);
    let gap = 2.0;
    let wrap = |text: egui::RichText, style: egui::TextStyle| {
        egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Wrap), text_w, style)
    };
    let title = wrap(
        egui::RichText::new(label).color(pal().text),
        egui::TextStyle::Body,
    );
    let sub = hint.map(|h| {
        wrap(
            egui::RichText::new(h).small().color(pal().text_faint),
            egui::TextStyle::Small,
        )
    });
    let text_h = title.size().y + sub.as_ref().map_or(0.0, |g| gap + g.size().y);
    let h = theme::ROW_H.max(text_h + 2.0 * theme::SPACE_MD);
    ui.allocate_ui_with_layout(
        vec2(w, h),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(vec2(w, h));
            ui.allocate_ui_with_layout(
                vec2(text_w, text_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.spacing_mut().item_spacing.y = gap;
                    ui.label(title);
                    if let Some(g) = sub {
                        ui.label(g);
                    }
                },
            );
            let ctl = ui
                .with_layout(egui::Layout::right_to_left(egui::Align::Center), add)
                .response
                .rect
                .width();
            if (ctl - ctl_w).abs() > 0.5 {
                ui.data_mut(|m| m.insert_temp(ctl_id, ctl));
                ui.ctx().request_discard("row control width changed");
            }
        },
    );
}

/// A read-only fact: muted name left, value right, tighter than a `row`.
pub fn fact(ui: &mut Ui, name: &str, value: &str) {
    let w = ui.available_width();
    ui.allocate_ui_with_layout(
        vec2(w, 30.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_size(vec2(w, 30.0));
            ui.label(egui::RichText::new(name).color(pal().text_muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(value).color(pal().text));
            });
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
    section_with(
        ui,
        |ui| {
            ui.label(section_title(title));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), trailing);
        },
        body,
    );
}

/// A card title's text style.
pub fn section_title(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .size(15.0)
        .strong()
        .color(pal().text)
}

/// Card whose title line is drawn by `header` (left to right, centered).
pub fn section_with(ui: &mut Ui, header: impl FnOnce(&mut Ui), body: impl FnOnce(&mut Ui)) {
    theme::card().show(ui, |ui| {
        let w = ui.available_width();
        ui.set_width(w);
        ui.allocate_ui_with_layout(
            vec2(w, theme::CONTROL_H),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_size(vec2(w, theme::CONTROL_H));
                header(ui);
            },
        );
        ui.add_space(theme::SPACE_XS);
        body(ui);
    });
    ui.add_space(theme::SPACE_MD);
}

/// A question over the page with one action and Cancel. `Some(true)` once
/// confirmed, `Some(false)` once dismissed, `None` while it is open.
pub fn confirm(
    ctx: &egui::Context,
    id: egui::Id,
    title: &str,
    body: &str,
    action: &str,
) -> Option<bool> {
    let mut end = None;
    let modal = egui::Modal::new(id).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.label(
            egui::RichText::new(title)
                .size(16.0)
                .strong()
                .color(pal().text),
        );
        ui.add_space(theme::SPACE_SM);
        ui.label(egui::RichText::new(body).color(pal().text_muted));
        ui.add_space(theme::SPACE_LG);
        // Actions bottom-right, the confirming one last.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if primary_button(ui, action).clicked() {
                end = Some(true);
            }
            if button(ui, "Cancel").clicked() {
                end = Some(false);
            }
        });
    });
    end.or(modal.should_close().then_some(false))
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

// ── Level meter ──────────────────────────────────────────────────

/// Bottom of the meter scale (dBFS); anything quieter reads as empty.
pub const METER_FLOOR_DB: f32 = -60.0;
/// Zone edges (dBFS): ok below the first, warn up to the second, danger above.
pub const METER_WARN_DB: f32 = -18.0;
pub const METER_DANGER_DB: f32 = -6.0;
const METER_RELEASE_DB_S: f32 = 20.0;
const METER_HOLD_S: f32 = 1.0;

/// Linear peak → dBFS on the meter scale.
pub fn to_dbfs(linear: f32) -> f32 {
    if linear > 0.0 {
        (20.0 * linear.log10()).clamp(METER_FLOOR_DB, 0.0)
    } else {
        METER_FLOOR_DB
    }
}

/// One meter channel as drawn: instant attack, a 20 dB/s release and a
/// peak tick that holds for a second, then follows the bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ballistics {
    pub level: f32,
    pub peak: f32,
    held: f32,
}

impl Default for Ballistics {
    fn default() -> Self {
        Self {
            level: METER_FLOOR_DB,
            peak: METER_FLOOR_DB,
            held: 0.0,
        }
    }
}

impl Ballistics {
    /// Advance `dt` seconds with the newest peak `db`. `instant` (reduced
    /// motion) shows the input as it is.
    pub fn step(&mut self, db: f32, dt: f32, instant: bool) {
        if instant {
            *self = Self {
                level: db,
                peak: db,
                held: 0.0,
            };
            return;
        }
        self.level = db.max(self.level - METER_RELEASE_DB_S * dt);
        if db >= self.peak {
            self.peak = db;
            self.held = 0.0;
        } else {
            self.held += dt;
            if self.held >= METER_HOLD_S {
                self.peak = self.level;
            }
        }
    }
}

/// Both channels of meter `id` advanced to this frame with the newest
/// linear peaks (L, R).
fn meter_channels(ui: &Ui, id: egui::Id, levels: [f32; 2]) -> [Ballistics; 2] {
    let now = ui.input(|i| i.time);
    // egui 0.31 has no reduced-motion setting; no animations means none here.
    let instant = ui.style().animation_time <= 0.0;
    let (mut chans, last) = ui
        .data(|m| m.get_temp::<([Ballistics; 2], f64)>(id))
        .unwrap_or_default();
    let dt = (now - last).clamp(0.0, 0.25) as f32;
    for (ch, v) in chans.iter_mut().zip(levels) {
        ch.step(to_dbfs(v), dt, instant);
    }
    ui.data_mut(|m| m.insert_temp(id, (chans, now)));
    chans
}

fn meter_zones() -> [(f32, f32, Color32); 3] {
    [
        (METER_FLOOR_DB, METER_WARN_DB, pal().ok),
        (METER_WARN_DB, METER_DANGER_DB, pal().warn),
        (METER_DANGER_DB, 0.0, pal().danger),
    ]
}

fn zone_color(db: f32) -> Color32 {
    meter_zones()
        .iter()
        .find(|(_, hi, _)| db <= *hi)
        .map_or(pal().danger, |z| z.2)
}

/// Horizontal stereo meter: two thin bars on a −60..0 dBFS scale with
/// ok / warn / danger zones and a peak-hold tick. `levels` is the latest
/// linear peak (L, R); the motion is computed here, per frame.
pub fn stereo_meter(ui: &mut Ui, id_salt: impl std::hash::Hash, levels: [f32; 2], width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 24.0), Sense::hover());
    let chans = meter_channels(ui, ui.id().with(id_salt), levels);
    if !ui.is_rect_visible(rect) {
        return;
    }

    let x = |db: f32| egui::remap_clamp(db, METER_FLOOR_DB..=0.0, rect.x_range());
    let zones = meter_zones();
    let p = ui.painter();
    let (bar_h, gap) = (4.0, 3.0);
    for (i, ch) in chans.iter().enumerate() {
        let top = rect.top() + i as f32 * (bar_h + gap);
        let y = egui::Rangef::new(top, top + bar_h);
        p.rect_filled(Rect::from_x_y_ranges(rect.x_range(), y), 2.0, pal().bg);
        for (lo, hi, color) in zones {
            // The unlit part of each zone stays faintly visible.
            p.rect_filled(
                Rect::from_x_y_ranges(x(lo)..=x(hi), y),
                0.0,
                color.gamma_multiply(0.12),
            );
            if ch.level > lo {
                p.rect_filled(
                    Rect::from_x_y_ranges(x(lo)..=x(ch.level.min(hi)), y),
                    0.0,
                    color,
                );
            }
        }
        if ch.peak > METER_FLOOR_DB {
            let px = x(ch.peak).max(rect.left() + 2.0);
            p.rect_filled(
                Rect::from_x_y_ranges(px - 2.0..=px, y),
                0.0,
                zone_color(ch.peak),
            );
        }
    }
    for db in [METER_FLOOR_DB, METER_WARN_DB, METER_DANGER_DB, 0.0] {
        let align = if db == METER_FLOOR_DB {
            egui::Align2::LEFT_BOTTOM
        } else if db == 0.0 {
            egui::Align2::RIGHT_BOTTOM
        } else {
            egui::Align2::CENTER_BOTTOM
        };
        p.text(
            pos2(x(db), rect.bottom()),
            align,
            format!("{db:.0}"),
            egui::FontId::proportional(9.5),
            pal().text_faint,
        );
    }
}

/// `stereo_meter` standing up, painted into `rect` (two bars side by side,
/// silence at the bottom), same scale, zones and ballistics.
pub fn vertical_meter(ui: &Ui, id: egui::Id, rect: Rect, levels: [f32; 2]) {
    let chans = meter_channels(ui, id, levels);
    if !ui.is_rect_visible(rect) {
        return;
    }
    let y = |db: f32| egui::remap_clamp(db, METER_FLOOR_DB..=0.0, rect.bottom()..=rect.top());
    let p = ui.painter();
    let gap = 2.0;
    let bar_w = (rect.width() - gap) / 2.0;
    for (i, ch) in chans.iter().enumerate() {
        let left = rect.left() + i as f32 * (bar_w + gap);
        let x = egui::Rangef::new(left, left + bar_w);
        p.rect_filled(Rect::from_x_y_ranges(x, rect.y_range()), 2.0, pal().bg);
        for (lo, hi, color) in meter_zones() {
            p.rect_filled(
                Rect::from_x_y_ranges(x, y(hi)..=y(lo)),
                0.0,
                color.gamma_multiply(0.12),
            );
            if ch.level > lo {
                p.rect_filled(
                    Rect::from_x_y_ranges(x, y(ch.level.min(hi))..=y(lo)),
                    0.0,
                    color,
                );
            }
        }
        if ch.peak > METER_FLOOR_DB {
            let py = y(ch.peak).min(rect.bottom() - 2.0);
            p.rect_filled(
                Rect::from_x_y_ranges(x, py..=py + 2.0),
                0.0,
                zone_color(ch.peak),
            );
        }
    }
}

// ── Gain fader ───────────────────────────────────────────────────

/// Fader range (dB), the mixer's.
pub const FADER_MIN_DB: f32 = hako_core::mixer::GAIN_MIN_DB;
pub const FADER_MAX_DB: f32 = hako_core::mixer::GAIN_MAX_DB;
/// Drags this close to 0 dB land on it.
pub const FADER_DETENT_DB: f32 = 0.75;

/// Taper: (dB, travel) breakpoints, linear between them. Like a console
/// fader, most of the travel sits around 0 dB and the bottom 20 dB are short.
const TAPER: [(f32, f32); 6] = [
    (FADER_MIN_DB, 0.0),
    (-40.0, 0.12),
    (-20.0, 0.36),
    (-10.0, 0.56),
    (0.0, 0.8),
    (FADER_MAX_DB, 1.0),
];

/// Fader travel (0 bottom .. 1 top) for a gain.
pub fn fader_pos(db: f32) -> f32 {
    let db = db.clamp(FADER_MIN_DB, FADER_MAX_DB);
    TAPER
        .windows(2)
        .find(|w| db <= w[1].0)
        .map_or(1.0, |w| egui::remap(db, w[0].0..=w[1].0, w[0].1..=w[1].1))
}

/// Gain at fader travel `pos`.
pub fn fader_db(pos: f32) -> f32 {
    let pos = pos.clamp(0.0, 1.0);
    TAPER
        .windows(2)
        .find(|w| pos <= w[1].1)
        .map_or(FADER_MAX_DB, |w| {
            egui::remap(pos, w[0].1..=w[1].1, w[0].0..=w[1].0)
        })
}

/// A dragged gain as stored: 0.1 dB steps, caught by the 0 dB detent.
pub fn snap_gain(db: f32) -> f32 {
    if db.abs() < FADER_DETENT_DB {
        return 0.0;
    }
    ((db * 10.0).round() / 10.0).clamp(FADER_MIN_DB, FADER_MAX_DB)
}

/// Vertical gain fader over `rect`, dragged relative to where it was
/// grabbed (the knob never jumps to the pointer); double-click resets to
/// 0 dB. `changed` while it moves, `drag_stopped` on release.
pub fn vertical_fader(ui: &Ui, rect: Rect, id: egui::Id, db: &mut f32, fill: Color32) -> Response {
    let mut resp = ui
        .interact(rect, id, Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::ResizeVertical);
    let knob_h = 12.0;
    let track = rect.y_range().shrink(knob_h / 2.0);
    let raw_id = id.with("raw");
    if resp.drag_started() {
        ui.data_mut(|m| m.insert_temp(raw_id, fader_pos(*db)));
    }
    if resp.dragged() {
        let raw = ui
            .data(|m| m.get_temp::<f32>(raw_id))
            .unwrap_or(fader_pos(*db));
        let raw = (raw - resp.drag_delta().y / track.span()).clamp(0.0, 1.0);
        ui.data_mut(|m| m.insert_temp(raw_id, raw));
        let v = snap_gain(fader_db(raw));
        if v != *db {
            *db = v;
            resp.mark_changed();
        }
    }
    if resp.double_clicked() && *db != 0.0 {
        *db = 0.0;
        resp.mark_changed();
    }
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let y = |db: f32| egui::lerp(track.max..=track.min, fader_pos(db));
    let cx = rect.center().x;
    let groove = Rect::from_x_y_ranges(cx - 2.0..=cx + 2.0, track);
    p.rect_filled(groove, 2.0, pal().surface_hi);
    let knob_y = y(*db);
    p.rect_filled(
        Rect::from_x_y_ranges(cx - 2.0..=cx + 2.0, knob_y..=track.max),
        2.0,
        fill,
    );
    // Scale ticks; 0 dB is the long one.
    for (tick, w) in [
        (12.0, 3.0),
        (0.0, 7.0),
        (-10.0, 3.0),
        (-20.0, 3.0),
        (-40.0, 3.0),
    ] {
        let ty = y(tick);
        for side in [-1.0, 1.0] {
            let x0 = cx + side * 6.0;
            p.line_segment(
                [pos2(x0, ty), pos2(x0 + side * w, ty)],
                Stroke::new(1.0_f32, pal().border_hi),
            );
        }
    }
    let hot =
        ui.ctx()
            .animate_bool_with_time(id.with("hot"), resp.hovered() || resp.dragged(), 0.12);
    let knob = Rect::from_center_size(pos2(cx, knob_y), vec2(22.0 + 2.0 * hot, knob_h));
    p.rect_filled(
        knob.translate(vec2(0.0, 1.5)),
        4.0,
        Color32::from_black_alpha(90),
    );
    let face = if ui.is_enabled() {
        Color32::from_rgb(232, 234, 239)
    } else {
        pal().text_faint
    };
    p.rect(
        knob,
        4.0,
        face,
        Stroke::new(1.0_f32, pal().border_hi),
        egui::StrokeKind::Inside,
    );
    p.line_segment(
        [
            pos2(knob.left() + 4.0, knob_y),
            pos2(knob.right() - 4.0, knob_y),
        ],
        Stroke::new(2.0_f32, Color32::from_rgb(108, 111, 121)),
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
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tooltip));
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

/// A pencil, tip at the bottom left.
pub fn icon_pencil(p: &egui::Painter, r: Rect, c: Color32) {
    let at = |x: f32, y: f32| r.min + vec2(x * r.width(), y * r.height());
    let s = Stroke::new(1.4_f32, c);
    let body = [
        at(0.0, 1.0),
        at(0.1, 0.66),
        at(0.7, 0.06),
        at(0.94, 0.3),
        at(0.34, 0.9),
    ];
    p.add(egui::Shape::closed_line(body.to_vec(), s));
    p.line_segment([at(0.56, 0.2), at(0.8, 0.44)], s);
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

/// Three fader slots with their knobs at different heights.
pub fn icon_mixer(p: &egui::Painter, r: Rect, c: Color32) {
    for (i, knob) in [0.7_f32, 0.3, 0.55].into_iter().enumerate() {
        let x = r.left() + r.width() * (0.18 + 0.32 * i as f32);
        p.line_segment(
            [pos2(x, r.top()), pos2(x, r.bottom())],
            Stroke::new(1.5_f32, c),
        );
        let y = egui::lerp(r.top()..=r.bottom(), knob);
        p.rect_filled(Rect::from_center_size(pos2(x, y), vec2(6.0, 4.0)), 1.0, c);
    }
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

/// A downloaded newer release, offered on top of the toasts until the user answers.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateOffer {
    pub version: String,
    /// Install in progress.
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
                egui::RichText::new(format!("hako v{} is ready to install", o.version))
                    .color(pal().text)
                    .strong(),
            );
            ui.add_space(theme::SPACE_SM);
            if o.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(egui::RichText::new("Installing…").color(pal().text_muted));
                });
                return;
            }
            ui.label(
                egui::RichText::new("hako restarts to finish. Audio routing pauses for a moment.")
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
