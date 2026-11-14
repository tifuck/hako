//! Data-driven rendering of daemon-declared controls (`SettingMeta`).
//!
//! The daemon owns ranges / options / units; this module only decides how a
//! given kind of control looks.

use crate::theme::{self, pal};
use crate::widgets;
use crate::UiCommand;
use egui::{pos2, vec2, Color32, Rect, Sense, Stroke, Ui};
use hako_core::catalog::{ControlKind, SettingKey, SettingMeta, SettingValue};
use hako_core::device::{DeviceKind, DeviceSnapshot};

/// Render `metas` as rows separated by hairlines.
pub fn rows(ui: &mut Ui, d: &DeviceSnapshot, metas: &[&SettingMeta], out: &mut Vec<UiCommand>) {
    let mut first = true;
    let mut color_done = false;
    for meta in metas {
        if meta.kind == ControlKind::ColorChannel {
            if color_done {
                continue;
            }
            color_done = true;
        }
        let trio = rgb_trio(d);
        if meta.kind == ControlKind::ColorChannel && trio.is_none() {
            continue;
        }
        if !first {
            widgets::divider(ui);
        }
        first = false;
        ui.add_enabled_ui(writable(d, meta), |ui| match (meta.kind, trio) {
            (ControlKind::ColorChannel, Some(trio)) => color_row(ui, d, meta, trio, out),
            (ControlKind::ColorChannel, None) => {}
            (ControlKind::Toggle, _) => toggle_row(ui, d, meta, out),
            (ControlKind::Dropdown, _) => dropdown_row(ui, d, meta, out),
            (ControlKind::ReadOnly, _) => readonly_row(ui, d, meta),
            (ControlKind::Slider, _) => slider_row(ui, d, meta, out),
        });
    }
}

// ── Onboard-memory gate / blocked controls ──────────────────────────────

/// The owner allowed this device to save settings to its onboard memory.
pub fn onboard_allowed(d: &DeviceSnapshot) -> bool {
    d.settings
        .get(SettingKey::OnboardWrites.as_str())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The daemon would accept a write now: not blocked, and onboard-memory
/// writes allowed if the control needs them.
pub fn writable(d: &DeviceSnapshot, meta: &SettingMeta) -> bool {
    meta.blocked.is_none() && (!meta.persistent || onboard_allowed(d))
}

/// Why a control is limited or special, shown under its label.
pub fn note(d: &DeviceSnapshot, meta: &SettingMeta) -> Option<String> {
    if let Some(reason) = &meta.blocked {
        return Some(format!("Not available: {reason}"));
    }
    meta.persistent.then(|| {
        if onboard_allowed(d) {
            "Saved to device memory".into()
        } else {
            "Saved to device memory — allow it in the Device tab".into()
        }
    })
}

/// `add` drawn disabled when the control cannot be written, with its note.
pub fn gated(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, add: impl FnOnce(&mut Ui)) {
    ui.add_enabled_ui(writable(d, meta), add);
    if let Some(n) = note(d, meta) {
        widgets::hint(ui, &n);
    }
}

/// Controls to draw (drivers keep irrelevant-but-settable ones `hidden`).
pub fn visible(d: &DeviceSnapshot) -> impl Iterator<Item = &SettingMeta> {
    d.controls.iter().filter(|c| !c.hidden)
}

pub fn meta(d: &DeviceSnapshot, key: SettingKey) -> Option<&SettingMeta> {
    visible(d).find(|c| c.key == key)
}

pub fn num(d: &DeviceSnapshot, key: SettingKey) -> Option<f64> {
    d.settings.get(key.as_str()).and_then(|v| v.as_float())
}

fn set(out: &mut Vec<UiCommand>, d: &DeviceSnapshot, key: SettingKey, value: SettingValue) {
    out.push(UiCommand::Set {
        device_id: d.id.clone(),
        key,
        value,
    });
}

// ── Sliders ──────────────────────────────────────────────────────

fn draft_id(d: &DeviceSnapshot, key: SettingKey) -> egui::Id {
    egui::Id::new(("hako_draft", d.id.key(), key.as_str()))
}

fn pending_id() -> egui::Id {
    egui::Id::new("hako_pending")
}

/// Set by the app each frame: commands are on their way to the daemon.
pub(crate) fn set_pending(ctx: &egui::Context, pending: bool) {
    ctx.data_mut(|m| m.insert_temp(pending_id(), pending));
}

/// A draft has had its answer: nothing is held with the pointer, no command
/// is in flight and none for `d` was queued this frame. Dropping it then shows
/// the daemon's value, including a refused or clamped one.
fn settled(ui: &Ui, d: &DeviceSnapshot, out: &[UiCommand]) -> bool {
    !ui.input(|i| i.pointer.any_down())
        && !ui
            .ctx()
            .data(|m| m.get_temp::<bool>(pending_id()))
            .unwrap_or(false)
        && !out.iter().any(|c| {
            matches!(c, UiCommand::Set { device_id, .. } | UiCommand::SetMany { device_id, .. }
                if *device_id == d.id)
        })
}

/// `meta`'s range, `min` / `max` filling in what it leaves open; `None` when
/// it is one no slider can show (`min > max`, NaN), which clamping would
/// panic on.
pub(crate) fn range(meta: &SettingMeta, min: f64, max: f64) -> Option<(f32, f32)> {
    let (min, max) = (
        meta.min.unwrap_or(min) as f32,
        meta.max.unwrap_or(max) as f32,
    );
    (min <= max).then_some((min, max))
}

/// Stands in for a control whose range is unusable.
fn bad_range(ui: &mut Ui) -> egui::Response {
    ui.label(egui::RichText::new("—").color(pal().text_muted))
        .on_hover_text("hakod reported an invalid range for this control")
}

fn is_int(meta: &SettingMeta) -> bool {
    let whole = |o: Option<f64>| o.is_none_or(|v| v.fract() == 0.0);
    whole(meta.step) && whole(meta.min) && whole(meta.max)
}

fn fmt_value(meta: &SettingMeta, v: f32) -> String {
    let shown = if is_int(meta) {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    };
    match meta.unit.as_deref() {
        Some("%") => format!("{shown}%"),
        Some(u) => format!("{shown} {u}"),
        // A small unitless scale (mic level 1–10) reads as a position on it.
        None => match meta.max {
            Some(max) if is_int(meta) && max <= 20.0 => format!("{shown} / {max}"),
            _ => shown,
        },
    }
}

/// Snap DPI to a magnitude-appropriate step (50 < 1k, 100 < 10k, 1000 above,
/// rounded up to a multiple of the device's step), then onto the device's
/// grid and range.
pub fn snap_dpi(meta: &SettingMeta, v: f64) -> Option<f64> {
    let magnitude = if v < 1000.0 {
        50.0
    } else if v < 10000.0 {
        100.0
    } else {
        1000.0
    };
    let step = meta.step.filter(|s| *s > 0.0).unwrap_or(1.0);
    let step = (magnitude / step).ceil() * step;
    meta.snap((v / step).round() * step)
}

/// Slider that keeps a local draft while dragging and commits on release.
/// Returns the value currently shown and the slider's response.
fn slider(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    meta: &SettingMeta,
    width: f32,
    out: &mut Vec<UiCommand>,
) -> (f32, egui::Response) {
    let key = meta.key;
    let Some((min, max)) = range(meta, 0.0, 100.0) else {
        return (num(d, key).unwrap_or(0.0) as f32, bad_range(ui));
    };
    let int = is_int(meta);
    let dpi = key == SettingKey::Dpi;
    let server = num(d, key).unwrap_or(min as f64) as f32;
    let id = draft_id(d, key);
    let mut v = ui
        .ctx()
        .data(|m| m.get_temp::<f32>(id))
        .unwrap_or(server)
        .clamp(min, max);

    let mut s = egui::Slider::new(&mut v, min..=max)
        .show_value(false)
        .logarithmic(dpi)
        .clamping(egui::SliderClamping::Always);
    if !dpi {
        if let Some(step) = meta.step.filter(|s| *s > 0.0) {
            s = s.step_by(step);
        } else if int {
            s = s.step_by(1.0);
        }
    }
    let resp = widgets::slider(ui, s, width);
    if resp.changed() {
        // Only the user's input snaps; the device's own value shows as is.
        if dpi {
            v = snap_dpi(meta, v.into()).map_or(v, |s| s as f32);
        }
        ui.ctx().data_mut(|m| m.insert_temp(id, v));
    }
    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
        // The grid is computed in f64: an f32 0.7 is below a 0.7 minimum.
        let n = meta.snap(v.into()).unwrap_or(v.into());
        let value = if int {
            SettingValue::Int(n.round() as i64)
        } else {
            SettingValue::Float(n)
        };
        set(out, d, key, value);
    }
    if settled(ui, d, out) {
        ui.ctx().data_mut(|m| m.remove_temp::<f32>(id));
    }
    widgets::knob(ui, &resp, widgets::fraction(v, min, max, dpi));
    (v, resp)
}

/// Label, slider and value on one line, for a card header (right-to-left).
pub fn inline_slider(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    meta: &SettingMeta,
    out: &mut Vec<UiCommand>,
) {
    let resp = ui.add_enabled_ui(writable(d, meta), |ui| {
        let (vr, _) = ui.allocate_exact_size(vec2(44.0, 20.0), Sense::hover());
        let (v, _) = slider(ui, d, meta, 160.0, out);
        ui.painter().text(
            vr.right_center(),
            egui::Align2::RIGHT_CENTER,
            fmt_value(meta, v),
            egui::TextStyle::Body.resolve(ui.style()),
            pal().text,
        );
        ui.add_space(theme::SPACE_XS);
        ui.label(egui::RichText::new(&meta.label).color(pal().text_muted));
    });
    if let Some(n) = note(d, meta) {
        resp.response.on_hover_text(n);
    }
}

fn slider_row(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    widgets::row(ui, &meta.label, note(d, meta).as_deref(), |ui| {
        let (vr, _) = ui.allocate_exact_size(vec2(theme::VALUE_W, 20.0), Sense::hover());
        let (v, _) = slider(ui, d, meta, theme::SLIDER_W, out);
        ui.painter().text(
            vr.right_center(),
            egui::Align2::RIGHT_CENTER,
            fmt_value(meta, v),
            egui::TextStyle::Body.resolve(ui.style()),
            pal().text,
        );
    });
}

// ── Toggles / dropdowns ──────────────────────────────────────────

fn toggle_row(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    let mut v = d
        .settings
        .get(meta.key.as_str())
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let pending = egui::Id::new(("hako_confirm", d.id.key(), meta.key.as_str()));
    widgets::row(ui, &meta.label, note(d, meta).as_deref(), |ui| {
        if widgets::switch(ui, &mut v).changed() {
            if v && meta.warning.is_some() {
                ui.ctx().data_mut(|m| m.insert_temp(pending, true));
            } else {
                set(out, d, meta.key, SettingValue::Bool(v));
            }
        }
    });
    // Switching on a toggle with a warning needs an explicit confirmation.
    let Some(warning) = &meta.warning else {
        return;
    };
    if !ui
        .ctx()
        .data(|m| m.get_temp::<bool>(pending))
        .unwrap_or(false)
    {
        return;
    }
    let answer = widgets::confirm(
        ui.ctx(),
        pending.with("modal"),
        &meta.label,
        warning,
        "I understand, allow it",
    );
    if answer == Some(true) {
        set(out, d, meta.key, SettingValue::Bool(true));
    }
    if answer.is_some() {
        ui.ctx().data_mut(|m| m.remove_temp::<bool>(pending));
    }
}

fn dropdown_row(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    widgets::row(ui, &meta.label, note(d, meta).as_deref(), |ui| {
        choice(ui, d, meta, out)
    });
}

/// Segmented control for short option lists, combo box otherwise.
pub fn choice(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    if meta.options.is_empty() {
        // e.g. a blocked placeholder with nothing to choose.
        ui.label(egui::RichText::new("—").color(pal().text_muted));
        return;
    }
    let current = d.settings.get(meta.key.as_str());
    let selected = meta.options.iter().position(|o| current == Some(&o.value));
    let pending = egui::Id::new(("hako_confirm", d.id.key(), meta.key.as_str()));
    // A choice with a warning needs an explicit confirmation, like a toggle.
    let mut pick = |ui: &mut Ui, i: usize| {
        if meta.warning.is_some() {
            ui.ctx().data_mut(|m| m.insert_temp(pending, i));
        } else {
            set(out, d, meta.key, meta.options[i].value.clone());
        }
    };
    let total_chars: usize = meta.options.iter().map(|o| o.label.len()).sum();
    if meta.options.len() <= 7 && total_chars <= 48 {
        let labels: Vec<&str> = meta.options.iter().map(|o| o.label.as_str()).collect();
        let id = egui::Id::new(("seg", d.id.key(), meta.key.as_str()));
        if let Some(i) = widgets::segmented(ui, id, &labels, selected) {
            pick(ui, i);
        }
        confirm_choice(ui, d, meta, pending, out);
        return;
    }
    let text = selected
        .map(|i| meta.options[i].label.clone())
        .unwrap_or_else(|| "—".into());
    widgets::dropdown(
        ("combo", d.id.key(), meta.key.as_str()),
        text,
        theme::DROPDOWN_W,
    )
    .show_ui(ui, |ui| {
        for (i, opt) in meta.options.iter().enumerate() {
            if ui
                .selectable_label(selected == Some(i), &opt.label)
                .clicked()
                && selected != Some(i)
            {
                pick(ui, i);
            }
        }
    });
    confirm_choice(ui, d, meta, pending, out);
}

/// The confirmation a picked option with a warning waits for.
fn confirm_choice(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    meta: &SettingMeta,
    pending: egui::Id,
    out: &mut Vec<UiCommand>,
) {
    let (Some(warning), Some(i)) = (
        &meta.warning,
        ui.ctx().data(|m| m.get_temp::<usize>(pending)),
    ) else {
        return;
    };
    let answer = widgets::confirm(
        ui.ctx(),
        pending.with("modal"),
        &meta.label,
        warning,
        "I understand, change it",
    );
    if answer == Some(true) {
        if let Some(opt) = meta.options.get(i) {
            set(out, d, meta.key, opt.value.clone());
        }
    }
    if answer.is_some() {
        ui.ctx().data_mut(|m| m.remove_temp::<usize>(pending));
    }
}

fn readonly_row(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta) {
    let text = match d.settings.get(meta.key.as_str()) {
        Some(SettingValue::Float(f)) => format!("{f:.2}"),
        Some(SettingValue::Int(i)) => i.to_string(),
        Some(SettingValue::Bool(b)) => if *b { "On" } else { "Off" }.into(),
        Some(SettingValue::Text(s)) => s.clone(),
        None => "—".into(),
    };
    widgets::row(ui, &meta.label, note(d, meta).as_deref(), |ui| {
        ui.label(egui::RichText::new(text).color(pal().text_muted));
    });
}

// ── Color ────────────────────────────────────────────────────────

const SWATCHES: [[u8; 3]; 8] = [
    [255, 255, 255],
    [255, 59, 48],
    [255, 149, 0],
    [255, 214, 10],
    [52, 199, 89],
    [0, 199, 190],
    [10, 132, 255],
    [191, 90, 242],
];

const RGB: [SettingKey; 3] = [SettingKey::ColorR, SettingKey::ColorG, SettingKey::ColorB];

fn rgb_trio(d: &DeviceSnapshot) -> Option<[SettingKey; 3]> {
    RGB.iter().all(|&k| meta(d, k).is_some()).then_some(RGB)
}

pub fn device_color(d: &DeviceSnapshot) -> Color32 {
    // Effects without color controls still preview in the last color.
    let Some(trio) =
        rgb_trio(d).or_else(|| d.settings.contains_key(RGB[0].as_str()).then_some(RGB))
    else {
        return pal().accent;
    };
    let c = |k| num(d, k).unwrap_or(0.0) as u8;
    Color32::from_rgb(c(trio[0]), c(trio[1]), c(trio[2]))
}

fn color_row(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    meta: &SettingMeta,
    trio: [SettingKey; 3],
    out: &mut Vec<UiCommand>,
) {
    let server = device_color(d);
    let draft_id = egui::Id::new(("color_draft", d.id.key()));
    let mut rgb = ui
        .ctx()
        .data(|m| m.get_temp::<[u8; 3]>(draft_id))
        .unwrap_or([server.r(), server.g(), server.b()]);
    let mut commit = None;

    widgets::row(ui, "Color", note(d, meta).as_deref(), |ui| {
        ui.label(
            egui::RichText::new(format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]))
                .color(pal().text_muted),
        );
        ui.add_space(theme::SPACE_XS);
        // Custom colour: a hue ring around the current colour, picker in a popup.
        // Commits on pointer release.
        let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
        let custom = !SWATCHES.contains(&rgb);
        let p = ui.painter();
        let n = 36;
        for i in 0..n {
            let a = |k: usize| k as f32 / n as f32 * std::f32::consts::TAU;
            let at = |k| r.center() + vec2(a(k).cos(), a(k).sin()) * 8.5;
            p.line_segment(
                [at(i), at(i + 1)],
                Stroke::new(3.0_f32, hue(i as f32 / n as f32)),
            );
        }
        p.circle_filled(r.center(), 5.0, Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        if custom {
            p.circle_stroke(r.center(), 11.0, Stroke::new(1.5_f32, pal().text));
        }
        let popup = egui::Id::new(("color_popup", d.id.key()));
        let resp = resp
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text("Custom color");
        if resp.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup));
        }
        egui::popup::popup_below_widget(
            ui,
            popup,
            &resp,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                let mut c = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                if egui::color_picker::color_picker_color32(
                    ui,
                    &mut c,
                    egui::color_picker::Alpha::Opaque,
                ) {
                    rgb = [c.r(), c.g(), c.b()];
                    ui.ctx().data_mut(|m| m.insert_temp(draft_id, rgb));
                }
            },
        );
        if ui.input(|i| !i.pointer.any_down()) && rgb != [server.r(), server.g(), server.b()] {
            commit = Some(rgb);
        }
        ui.add_space(theme::SPACE_XS);
        for sw in SWATCHES.iter().rev() {
            let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
            let col = Color32::from_rgb(sw[0], sw[1], sw[2]);
            let active = *sw == rgb;
            let grow = ui
                .ctx()
                .animate_bool_with_time(resp.id, resp.hovered() || active, 0.12);
            ui.painter()
                .circle_filled(r.center(), 7.0 + grow * 2.0, col);
            // A hairline so white stays visible on light themes.
            if !pal().dark {
                ui.painter().circle_stroke(
                    r.center(),
                    7.0 + grow * 2.0,
                    Stroke::new(1.0_f32, pal().border_hi),
                );
            }
            if active {
                ui.painter()
                    .circle_stroke(r.center(), 11.0, Stroke::new(1.5_f32, pal().text));
            }
            if resp
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                rgb = *sw;
                commit = Some(rgb);
            }
        }
    });

    if let Some(rgb) = commit {
        let sent_id = egui::Id::new(("color_sent", d.id.key()));
        if ui.ctx().data(|m| m.get_temp::<[u8; 3]>(sent_id)) != Some(rgb) {
            ui.ctx().data_mut(|m| {
                m.insert_temp(draft_id, rgb);
                m.insert_temp(sent_id, rgb);
            });
            out.push(UiCommand::SetMany {
                device_id: d.id.clone(),
                values: trio
                    .iter()
                    .zip(rgb)
                    .map(|(k, v)| (*k, SettingValue::Int(v.into())))
                    .collect(),
            });
        }
    }
    if settled(ui, d, out) {
        ui.ctx().data_mut(|m| {
            m.remove_temp::<[u8; 3]>(draft_id);
            m.remove_temp::<[u8; 3]>(egui::Id::new(("color_sent", d.id.key())));
        });
    }
}

// ── Lighting effect tiles ────────────────────────────────────────

/// Grid of effect tiles with live animated previews.
pub fn effect_tiles(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    let current = d.settings.get(meta.key.as_str());
    let color = device_color(d);
    let time = ui.input(|i| i.time) as f32;
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));

    let gap = theme::SPACE_SM;
    // Fewest rows that fit, then spread tiles evenly so no row is a lone orphan.
    let n = meta.options.len().max(1);
    let fit = (((ui.available_width() + gap) / (112.0 + gap)).floor() as usize).max(2);
    let rows = n.div_ceil(fit);
    let cols = n.div_ceil(rows);
    let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
    for chunk in meta.options.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for opt in chunk {
                let selected = current == Some(&opt.value);
                let (rect, resp) = ui.allocate_exact_size(vec2(w, 84.0), Sense::click());
                let hover = ui
                    .ctx()
                    .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                let sel = ui
                    .ctx()
                    .animate_bool_with_time(resp.id.with("sel"), selected, 0.18);
                let p = ui.painter_at(rect.expand(2.0));
                p.rect_filled(
                    rect,
                    10.0,
                    theme::pal().bg.lerp_to_gamma(pal().surface_hi, hover),
                );
                p.rect_stroke(
                    rect,
                    10.0,
                    Stroke::new(
                        1.0 + sel,
                        pal()
                            .border
                            .lerp_to_gamma(pal().border_hi, hover)
                            .lerp_to_gamma(pal().accent, sel),
                    ),
                    egui::StrokeKind::Inside,
                );
                let preview =
                    Rect::from_min_size(rect.min + vec2(8.0, 8.0), vec2(rect.width() - 16.0, 44.0));
                paint_effect(
                    &p,
                    preview,
                    opt.value.as_str().unwrap_or(""),
                    color,
                    time,
                    d.kind == DeviceKind::Keyboard,
                );
                p.text(
                    pos2(rect.left() + 12.0, rect.bottom() - 16.0),
                    egui::Align2::LEFT_CENTER,
                    &opt.label,
                    egui::FontId::proportional(13.0),
                    if selected {
                        pal().text
                    } else {
                        pal().text_muted
                    },
                );
                if resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                    && !selected
                {
                    set(out, d, meta.key, opt.value.clone());
                }
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
}

fn hue(h: f32) -> Color32 {
    egui::ecolor::Hsva::new(h.rem_euclid(1.0), 0.85, 1.0, 1.0).into()
}

/// Horizontal gradient from `colors` sampled at `n` stops.
fn gradient(p: &egui::Painter, r: Rect, n: usize, color_at: impl Fn(f32) -> Color32) {
    let mut mesh = egui::Mesh::default();
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let x = egui::lerp(r.left()..=r.right(), t);
        let c = color_at(t);
        mesh.colored_vertex(pos2(x, r.top()), c);
        mesh.colored_vertex(pos2(x, r.bottom()), c);
        if i > 0 {
            let b = (i as u32) * 2;
            mesh.add_triangle(b - 2, b - 1, b);
            mesh.add_triangle(b - 1, b, b + 1);
        }
    }
    p.add(mesh);
}

/// Small looping animation that hints at what each effect looks like. On a
/// keyboard it lights a grid of keycaps, elsewhere a single glow.
pub fn paint_effect(p: &egui::Painter, r: Rect, effect: &str, color: Color32, t: f32, keys: bool) {
    let p = &p.with_clip_rect(r);
    p.rect_filled(r, 6.0, Color32::from_rgb(8, 9, 11));
    let unlit = Color32::from_rgb(26, 28, 34);
    // Colour of a key at column position `x` (0..1) with a per-key `seed`.
    let light = |x: f32, seed: f32| -> Option<Color32> {
        Some(match effect {
            "static" => color,
            "spectrum" | "cycle" => hue(t * 0.12),
            "wave" => hue(x * 0.9 - t * 0.25),
            "breathing" | "breathe" | "pulse" => {
                color.gamma_multiply(0.12 + 0.88 * (0.5 - 0.5 * (t * 2.2).cos()))
            }
            "reactive" => {
                // Keys "pressed" at random, then fading out.
                let phase = (t * 0.55 + seed * 5.0).fract();
                unlit.lerp_to_gamma(color, (1.0 - phase * 3.0).max(0.0))
            }
            "starlight" => {
                let phase = (t * 0.4 + seed * 7.0).fract();
                let a = if phase < 0.35 {
                    (phase / 0.35 * std::f32::consts::PI).sin()
                } else {
                    0.0
                };
                unlit.lerp_to_gamma(color, a)
            }
            _ => return None,
        })
    };
    if !keys {
        let inner = r.shrink(1.0);
        match (effect, light(0.5, 0.0)) {
            ("wave", _) => gradient(p, inner, 24, |x| hue(x * 0.9 - t * 0.25)),
            ("reactive" | "starlight", _) | (_, None) => {
                p.text(
                    inner.center(),
                    egui::Align2::CENTER_CENTER,
                    "Off",
                    egui::FontId::proportional(11.0),
                    theme::pal().text_faint,
                );
            }
            (_, Some(c)) => {
                p.rect_filled(inner, 5.0, c);
            }
        }
        return;
    }
    // Square caps: as many columns as the width allows at four rows.
    let rows = 4;
    let inner = r.shrink(5.0);
    let ch = inner.height() / rows as f32;
    let cols = ((inner.width() / ch).round() as usize).max(4);
    let cw = inner.width() / cols as f32;
    for i in 0..cols * rows {
        let (col, row) = (i % cols, i / cols);
        let seed = ((i as u32).wrapping_mul(2654435761) >> 8) as f32 / (u32::MAX >> 8) as f32;
        let x = (col as f32 + 0.5) / cols as f32;
        let key = Rect::from_min_size(
            pos2(
                inner.left() + col as f32 * cw,
                inner.top() + row as f32 * ch,
            ),
            vec2(cw, ch),
        )
        .shrink(1.2);
        p.rect_filled(key, 2.0, light(x, seed).unwrap_or(unlit));
    }
}

// ── Headset: ChatMix ─────────────────────────────────────────────

/// What the ChatMix card says the dial does: with the mixer on it turns the
/// mixer's faders, otherwise the two sinks apps play into.
pub fn chatmix_hint(mixer_on: bool) -> &'static str {
    if mixer_on {
        "Turns the Game and Chat faders in the Mixer."
    } else {
        "Follows the dial on the base station. Send games to Hako_Game and voice chat to Hako_Chat."
    }
}

/// Where the base-station dial sits between game and chat (read-only). The
/// sink names show only while the headset's own sinks exist (mixer off).
pub fn chatmix(ui: &mut Ui, d: &DeviceSnapshot, mixer_on: bool) {
    let game = num(d, SettingKey::ChatMixGame)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0) as f32;
    let chat = num(d, SettingKey::ChatMixChat)
        .unwrap_or(1.0)
        .clamp(0.0, 1.0) as f32;
    let height = if mixer_on { 46.0 } else { 66.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let pos = ui
        .ctx()
        .animate_value_with_time(resp.id, (1.0 + chat - game) / 2.0, 0.25);
    let p = ui.painter();
    let body = egui::FontId::proportional(13.5);
    let small = egui::FontId::proportional(11.5);
    for (left_side, name, sink, v) in [
        (true, "Game", "Hako_Game", game),
        (false, "Chat", "Hako_Chat", chat),
    ] {
        // Name on the outside, level towards the middle.
        let pct = format!("{:.0}%", v * 100.0);
        let name_g = p.layout_no_wrap(name.into(), body.clone(), pal().text);
        let pct_g = p.layout_no_wrap(pct, body.clone(), pal().text_muted);
        let gap = 8.0;
        let total = name_g.size().x + gap + pct_g.size().x;
        let y = rect.top() + 8.0 - name_g.size().y / 2.0;
        let (name_x, pct_x) = if left_side {
            (rect.left(), rect.left() + name_g.size().x + gap)
        } else {
            (rect.right() - name_g.size().x, rect.right() - total)
        };
        p.galley(pos2(name_x, y), name_g, pal().text);
        p.galley(pos2(pct_x, y), pct_g, pal().text_muted);
        if mixer_on {
            continue;
        }
        p.text(
            pos2(
                if left_side { rect.left() } else { rect.right() },
                rect.bottom() - 8.0,
            ),
            if left_side {
                egui::Align2::LEFT_CENTER
            } else {
                egui::Align2::RIGHT_CENTER
            },
            sink,
            small.clone(),
            pal().text_faint,
        );
    }
    let track = Rect::from_center_size(
        pos2(rect.center().x, rect.top() + 33.0),
        vec2(rect.width(), 6.0),
    );
    p.rect_filled(track, 3.0, pal().surface_hover);
    let x = egui::lerp(track.x_range(), pos);
    let mid = track.center().x;
    let fill = Rect::from_x_y_ranges(mid.min(x)..=mid.max(x), track.y_range());
    p.rect_filled(fill, 3.0, pal().accent);
    p.rect_filled(
        Rect::from_center_size(pos2(mid, track.center().y), vec2(2.0, 14.0)),
        1.0,
        pal().border_hi,
    );
    p.circle(
        pos2(
            x.clamp(track.left() + 8.0, track.right() - 8.0),
            track.center().y,
        ),
        8.0,
        Color32::WHITE,
        Stroke::new(1.0_f32, pal().border_hi),
    );
}

// ── Mouse: DPI ───────────────────────────────────────────────────

/// Reference marks under the DPI track (those inside the device's range).
const DPI_MARKS: [i64; 7] = [400, 800, 1600, 3200, 6400, 12800, 25600];

/// DPI readout you can drag or type into, a log-scale slider, and
/// clickable reference marks along it.
pub fn dpi(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    let key = meta.key;
    let id = draft_id(d, key);
    let Some((min, max)) = range(meta, 100.0, 32000.0) else {
        bad_range(ui);
        return;
    };
    let step = meta.step.unwrap_or(1.0).max(1.0) as f32;
    let snap = |v: f32| meta.snap(v.into()).map_or(v, |s| s as f32);
    let server = num(d, key).unwrap_or(1600.0) as f32;
    let live = ui.ctx().data(|m| m.get_temp::<f32>(id)).unwrap_or(server);
    let commit = |ui: &Ui, out: &mut Vec<UiCommand>, v: f32| {
        ui.ctx().data_mut(|m| m.insert_temp(id, v));
        set(out, d, key, SettingValue::Int(v.round() as i64));
    };

    ui.horizontal(|ui| {
        let mut v = live;
        let resp = ui
            .scope(|ui| {
                ui.style_mut()
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(40.0));
                // No side padding, so the digits line up with the card's edge.
                ui.spacing_mut().button_padding = vec2(0.0, 0.0);
                let w = &mut ui.visuals_mut().widgets;
                w.inactive.weak_bg_fill = Color32::TRANSPARENT;
                w.inactive.bg_stroke = Stroke::NONE;
                w.hovered.weak_bg_fill = pal().surface_hi;
                ui.add(
                    egui::DragValue::new(&mut v)
                        .range(min..=max)
                        .clamp_existing_to_range(false)
                        .speed(step.max(10.0))
                        .max_decimals(0)
                        .update_while_editing(false),
                )
            })
            .inner
            .on_hover_text("Drag sideways, or click to type an exact value");
        let snapped = snap(v);
        if resp.changed() {
            if resp.dragged() {
                ui.ctx().data_mut(|m| m.insert_temp(id, snapped));
            } else {
                commit(ui, out, snapped);
            }
        }
        if resp.drag_stopped() {
            commit(ui, out, snapped);
        }
        ui.label(
            egui::RichText::new("DPI")
                .size(15.0)
                .color(pal().text_muted),
        );
    });
    ui.add_space(theme::SPACE_XS);
    let width = ui.available_width();
    let (v, resp) = slider(ui, d, meta, width, out);
    let span = widgets::track(&resp);
    let (row, _) = ui.allocate_exact_size(vec2(width, 26.0), Sense::hover());
    let font = egui::FontId::proportional(12.0);
    for mark in DPI_MARKS
        .into_iter()
        .filter(|m| (min..=max).contains(&(*m as f32)))
    {
        let x = egui::lerp(span, widgets::fraction(mark as f32, min, max, true));
        let galley = ui
            .painter()
            .layout_no_wrap(mark.to_string(), font.clone(), pal().text);
        let hit = Rect::from_center_size(
            pos2(x, row.center().y + 2.0),
            galley.size() + vec2(12.0, 10.0),
        );
        let r = ui
            .interact(hit, id.with(mark), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let current = v.round() as i64 == mark;
        let color = if current {
            pal().accent
        } else if r.hovered() {
            pal().text
        } else {
            pal().text_faint
        };
        ui.painter().rect_filled(
            Rect::from_center_size(pos2(x, row.top() + 2.0), vec2(1.0, 4.0)),
            0.0,
            pal().border_hi,
        );
        ui.painter().galley(
            pos2(x - galley.size().x / 2.0, row.top() + 7.0),
            galley,
            color,
        );
        if r.clicked() && !current {
            commit(ui, out, snap(mark as f32));
        }
    }
}
