//! Data-driven rendering of daemon-declared controls (`SettingMeta`).
//!
//! The daemon owns ranges / options / units; this module only decides how a
//! given kind of control looks.

use crate::theme::{self, pal};
use crate::widgets;
use crate::UiCommand;
use egui::{pos2, vec2, Color32, Rect, Sense, Stroke, Ui};
use hako_core::catalog::{ControlKind, SettingKey, SettingMeta, SettingValue};
use hako_core::device::DeviceSnapshot;

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
            "Saved to device memory — allow it in the Device memory tab".into()
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
        None => shown,
    }
}

/// Snap DPI to a magnitude-appropriate step (50 < 1k, 100 < 10k, 1000 above,
/// never finer than the device's step), kept inside the device's range.
pub fn snap_dpi(v: f32, min: f32, max: f32, step: f32) -> f32 {
    let magnitude: f32 = if v < 1000.0 {
        50.0
    } else if v < 10000.0 {
        100.0
    } else {
        1000.0
    };
    let step = magnitude.max(step);
    ((v / step).round() * step).clamp(min, max)
}

/// Slider that keeps a local draft while dragging and commits on release.
/// Returns the value currently shown.
fn slider(
    ui: &mut Ui,
    d: &DeviceSnapshot,
    meta: &SettingMeta,
    width: f32,
    out: &mut Vec<UiCommand>,
) -> f32 {
    let key = meta.key;
    let min = meta.min.unwrap_or(0.0) as f32;
    let max = meta.max.unwrap_or(100.0) as f32;
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
    ui.spacing_mut().slider_width = width;
    let resp = ui.add(s);
    if dpi {
        v = snap_dpi(v, min, max, meta.step.unwrap_or(1.0) as f32);
    }
    if resp.changed() {
        ui.ctx().data_mut(|m| m.insert_temp(id, v));
    }
    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
        let value = if int {
            SettingValue::Int(v.round() as i64)
        } else {
            SettingValue::Float(v as f64)
        };
        set(out, d, key, value);
    }
    // Server caught up → drop the draft.
    if !resp.dragged() && (v - server).abs() < if int { 0.51 } else { 0.02 } {
        ui.ctx().data_mut(|m| m.remove_temp::<f32>(id));
    }
    v
}

fn slider_row(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    widgets::row(ui, &meta.label, note(d, meta).as_deref(), |ui| {
        let (vr, _) = ui.allocate_exact_size(vec2(theme::VALUE_W, 20.0), Sense::hover());
        let v = slider(ui, d, meta, theme::SLIDER_W, out);
        ui.painter().text(
            vr.right_center(),
            egui::Align2::RIGHT_CENTER,
            fmt_value(meta, v),
            egui::FontId::monospace(12.5),
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
    let mut close = false;
    let modal = egui::Modal::new(pending.with("modal")).show(ui.ctx(), |ui| {
        ui.set_width(420.0);
        ui.label(
            egui::RichText::new(&meta.label)
                .size(16.0)
                .strong()
                .color(pal().text),
        );
        ui.add_space(theme::SPACE_SM);
        ui.label(egui::RichText::new(warning).color(pal().text_muted));
        ui.add_space(theme::SPACE_LG);
        // Actions bottom-right, the confirming one last.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::primary_button(ui, "I understand, allow it").clicked() {
                set(out, d, meta.key, SettingValue::Bool(true));
                close = true;
            }
            if widgets::button(ui, "Cancel").clicked() {
                close = true;
            }
        });
    });
    if close || modal.should_close() {
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
    let total_chars: usize = meta.options.iter().map(|o| o.label.len()).sum();
    if meta.options.len() <= 7 && total_chars <= 48 {
        let labels: Vec<&str> = meta.options.iter().map(|o| o.label.as_str()).collect();
        let id = egui::Id::new(("seg", d.id.key(), meta.key.as_str()));
        if let Some(i) = widgets::segmented(ui, id, &labels, selected) {
            set(out, d, meta.key, meta.options[i].value.clone());
        }
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
                set(out, d, meta.key, opt.value.clone());
            }
        }
    });
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
                .monospace()
                .color(pal().text_muted),
        );
        // Custom picker: commit on popup close / pointer release.
        let resp = ui.color_edit_button_srgb(&mut rgb);
        if resp.changed() {
            ui.ctx().data_mut(|m| m.insert_temp(draft_id, rgb));
        }
        if ui.input(|i| !i.pointer.any_down()) && rgb != [server.r(), server.g(), server.b()] {
            commit = Some(rgb);
        }
        ui.add_space(theme::SPACE_SM);
        for sw in SWATCHES.iter().rev() {
            let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
            let col = Color32::from_rgb(sw[0], sw[1], sw[2]);
            let active = *sw == rgb;
            let grow = ui
                .ctx()
                .animate_bool_with_time(resp.id, resp.hovered() || active, 0.12);
            ui.painter()
                .circle_filled(r.center(), 7.0 + grow * 2.0, col);
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
            for (k, v) in trio.iter().zip(rgb) {
                set(out, d, *k, SettingValue::Int(v as i64));
            }
        }
    }
    if [server.r(), server.g(), server.b()] == rgb {
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
                paint_effect(&p, preview, opt.value.as_str().unwrap_or(""), color, time);
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

/// Small looping animation that hints at what each effect looks like.
pub fn paint_effect(p: &egui::Painter, r: Rect, effect: &str, color: Color32, t: f32) {
    let p = &p.with_clip_rect(r);
    p.rect_filled(r, 6.0, Color32::from_rgb(8, 9, 11));
    let inner = r.shrink(1.0);
    match effect {
        "static" => {
            p.rect_filled(inner, 5.0, color);
        }
        "spectrum" | "cycle" => {
            p.rect_filled(inner, 5.0, hue(t * 0.12));
        }
        "wave" => gradient(p, inner, 24, |x| hue(x * 0.9 - t * 0.25)),
        "breathing" | "breathe" | "pulse" => {
            let a = 0.15 + 0.85 * (0.5 - 0.5 * (t * 2.2).cos());
            p.rect_filled(inner, 5.0, color.gamma_multiply(a));
        }
        "reactive" | "starlight" => {
            // Deterministic pseudo-random "keys" that flash and fade.
            let cols = 14;
            let rows = 3;
            let cw = inner.width() / cols as f32;
            let ch = inner.height() / rows as f32;
            let speed = if effect == "reactive" { 1.6 } else { 0.9 };
            for i in 0..cols * rows {
                let seed =
                    ((i as u32).wrapping_mul(2654435761) >> 7) as f32 / (u32::MAX >> 7) as f32;
                let phase = (t * speed + seed * 7.0).fract();
                let a = (1.0 - phase * 4.0).max(0.0);
                if a > 0.0 {
                    let x = inner.left() + (i % cols) as f32 * cw;
                    let y = inner.top() + (i / cols) as f32 * ch;
                    let cell = Rect::from_min_size(pos2(x, y), vec2(cw, ch)).shrink(1.5);
                    p.rect_filled(cell, 1.5, color.gamma_multiply(a));
                }
            }
        }
        _ => {
            p.text(
                inner.center(),
                egui::Align2::CENTER_CENTER,
                "Off",
                egui::FontId::proportional(11.0),
                theme::pal().text_faint,
            );
        }
    }
}

// ── Headset: ChatMix ─────────────────────────────────────────────

/// Two level meters driven by the base-station dial.
pub fn chatmix(ui: &mut Ui, d: &DeviceSnapshot) {
    let game = num(d, SettingKey::ChatMixGame).unwrap_or(1.0) as f32;
    let chat = num(d, SettingKey::ChatMixChat).unwrap_or(1.0) as f32;
    for (label, sink, v) in [("Game", "Hako_Game", game), ("Chat", "Hako_Chat", chat)] {
        widgets::row(ui, label, Some(sink), |ui| {
            let (vr, _) = ui.allocate_exact_size(vec2(theme::VALUE_W, 20.0), Sense::hover());
            ui.painter().text(
                vr.right_center(),
                egui::Align2::RIGHT_CENTER,
                format!("{:.0}%", v * 100.0),
                egui::FontId::monospace(12.5),
                pal().text,
            );
            let (r, resp) = ui.allocate_exact_size(vec2(theme::SLIDER_W, 6.0), Sense::hover());
            let shown = ui
                .ctx()
                .animate_value_with_time(resp.id, v.clamp(0.0, 1.0), 0.25);
            ui.painter().rect_filled(r, 4.0, pal().surface_hi);
            let mut fill = r;
            fill.set_width(r.width() * shown);
            ui.painter().rect_filled(fill, 4.0, pal().accent);
        });
        if label == "Game" {
            widgets::divider(ui);
        }
    }
}

// ── Mouse: DPI ───────────────────────────────────────────────────

const DPI_PRESETS: [i64; 6] = [400, 800, 1200, 1600, 3200, 6400];

/// Large DPI readout, slider, and preset chips.
pub fn dpi(ui: &mut Ui, d: &DeviceSnapshot, meta: &SettingMeta, out: &mut Vec<UiCommand>) {
    let server = num(d, meta.key).unwrap_or(1600.0);
    let live = ui
        .ctx()
        .data(|m| m.get_temp::<f32>(draft_id(d, meta.key)))
        .map(|v| v as f64)
        .unwrap_or(server);
    ui.horizontal(|ui| {
        let shown = ui.ctx().animate_value_with_time(
            egui::Id::new(("dpi_shown", d.id.key())),
            live as f32,
            0.2,
        );
        ui.label(
            egui::RichText::new(format!("{}", shown.round() as i64))
                .size(40.0)
                .color(pal().text),
        );
        ui.vertical(|ui| {
            ui.add_space(20.0);
            ui.label(egui::RichText::new("DPI").color(pal().text_muted));
        });
    });
    ui.add_space(theme::SPACE_XS);
    slider(ui, d, meta, (ui.available_width() - 8.0).max(200.0), out);
    ui.add_space(theme::SPACE_SM);
    ui.horizontal_wrapped(|ui| {
        for preset in DPI_PRESETS {
            let active = (live as i64) == preset;
            let text = egui::RichText::new(preset.to_string()).color(if active {
                pal().text
            } else {
                pal().text_muted
            });
            let btn = egui::Button::new(text)
                .fill(if active {
                    pal().surface_hi
                } else {
                    theme::pal().bg
                })
                .stroke(Stroke::new(
                    1.0_f32,
                    if active { pal().accent } else { pal().border },
                ))
                .corner_radius(14.0);
            if ui
                .add(btn)
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
                && !active
            {
                ui.ctx()
                    .data_mut(|m| m.insert_temp(draft_id(d, meta.key), preset as f32));
                set(out, d, meta.key, SettingValue::Int(preset));
            }
        }
    });
}
