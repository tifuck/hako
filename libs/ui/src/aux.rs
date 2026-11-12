//! Headset line-in: monitor toggle + ordered processing chain editor.

use crate::theme::{self, pal};
use crate::widgets;
use egui::{pos2, vec2, Rect, Sense, Stroke, Ui};
use hako_core::ipc::{
    AuxEffect, AuxEffectChain, AuxEffectKind, EqBand, EqBandKind, MAX_AUX_EFFECTS as MAX_STAGES,
    MAX_EQ_BANDS,
};

/// Edit `chain` in place. Returns true when the change should be sent to the
/// daemon now (discrete edits, or the end of a drag).
pub fn chain_editor(ui: &mut Ui, chain: &mut AuxEffectChain) -> bool {
    let mut commit = false;
    let mut moved: Option<(usize, usize)> = None;
    let mut removed: Option<usize> = None;

    if chain.effects.is_empty() {
        theme::inset().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(theme::SPACE_MD);
                ui.label(egui::RichText::new("No processing").color(pal().text_muted));
                widgets::hint(
                    ui,
                    "Add a stage. Audio passes through stages from top to bottom.",
                );
                ui.add_space(theme::SPACE_MD);
            });
        });
    }

    let count = chain.effects.len();
    for (index, effect) in chain.effects.iter_mut().enumerate() {
        let row = ui.push_id(effect.id.clone(), |ui| {
            stage_row(
                ui,
                effect,
                index,
                count,
                &mut commit,
                &mut moved,
                &mut removed,
            )
        });
        // Drop another row onto this one to move it here.
        if let Some(from) = row.inner.dnd_release_payload::<usize>() {
            if *from != index {
                moved = Some((*from, index));
            }
        } else if let Some(from) = row.inner.dnd_hover_payload::<usize>() {
            if *from != index {
                let r = row.inner.rect;
                let y = if *from < index {
                    r.bottom() + 3.0
                } else {
                    r.top() - 3.0
                };
                ui.painter().line_segment(
                    [pos2(r.left(), y), pos2(r.right(), y)],
                    Stroke::new(2.0_f32, pal().accent),
                );
            }
        }
        ui.add_space(theme::SPACE_XS);
    }

    if let Some((from, to)) = moved {
        let fx = chain.effects.remove(from);
        chain.effects.insert(to, fx);
        commit = true;
    }
    if let Some(i) = removed {
        chain.effects.remove(i);
        commit = true;
    }

    ui.add_space(theme::SPACE_XS);
    ui.add_enabled_ui(chain.effects.len() < MAX_STAGES, |ui| {
        ui.menu_button("+  Add stage", |ui| {
            for kind in [
                AuxEffectKind::default_gate(),
                AuxEffectKind::default_compressor(),
                AuxEffectKind::default_equalizer(),
            ] {
                if ui.button(kind.label()).clicked() {
                    let id = next_id(chain, kind.id_prefix());
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        ui.make_persistent_id(("stage_open", &id)),
                        true,
                    )
                    .store(ui.ctx());
                    chain.effects.push(AuxEffect {
                        id,
                        enabled: true,
                        kind,
                    });
                    commit = true;
                    ui.close_menu();
                }
            }
        });
    });
    commit
}

pub fn next_id(chain: &AuxEffectChain, prefix: &str) -> String {
    (1..)
        .map(|n| format!("{prefix}-{n}"))
        .find(|c| !chain.effects.iter().any(|e| e.id == *c))
        .expect("unbounded")
}

fn summary(kind: &AuxEffectKind) -> String {
    match kind {
        AuxEffectKind::NoiseGate {
            threshold_db,
            release_ms,
            ..
        } => format!("Opens above {threshold_db:.0} dB · {release_ms:.0} ms release"),
        AuxEffectKind::Compressor {
            threshold_db,
            ratio,
            makeup_db,
            ..
        } => format!("{ratio:.1}:1 above {threshold_db:.0} dB · +{makeup_db:.1} dB makeup"),
        AuxEffectKind::Equalizer { bands } => {
            let active = bands
                .iter()
                .filter(|b| !b.kind.has_gain() || b.gain_db.abs() >= 0.1)
                .count();
            format!("{} bands · {active} shaping", bands.len())
        }
    }
}

/// One stage: header row (drag handle, name, bypass, order, remove) and an
/// animated expandable editor. Returns the header response for drop targeting.
fn stage_row(
    ui: &mut Ui,
    effect: &mut AuxEffect,
    index: usize,
    count: usize,
    commit: &mut bool,
    moved: &mut Option<(usize, usize)>,
    removed: &mut Option<usize>,
) -> egui::Response {
    let open_id = ui.make_persistent_id(("stage_open", &effect.id));
    let mut state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), open_id, false);
    let openness = state.openness(ui.ctx());

    let frame = egui::Frame::new()
        .fill(
            theme::pal()
                .bg
                .lerp_to_gamma(theme::pal().surface, 0.5 + openness * 0.5),
        )
        .stroke(Stroke::new(
            1.0_f32,
            pal().border.lerp_to_gamma(pal().border_hi, openness),
        ))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(10, 8));
    let resp = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        let header = ui.horizontal(|ui| {
            ui.set_min_height(36.0);
            // Drag handle: payload is this row's index.
            ui.dnd_drag_source(ui.id().with("drag"), index, |ui| {
                let (r, _) = ui.allocate_exact_size(vec2(16.0, 28.0), Sense::hover());
                widgets::icon_grip(ui.painter(), r, pal().text_faint);
            })
            .response
            .on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text("Drag to reorder");

            let (badge, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
            ui.painter()
                .circle_filled(badge.center(), 11.0, pal().surface_hi);
            ui.painter().text(
                badge.center(),
                egui::Align2::CENTER_CENTER,
                (index + 1).to_string(),
                egui::FontId::proportional(11.5),
                if effect.enabled {
                    pal().accent
                } else {
                    pal().text_faint
                },
            );

            let title = ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(egui::RichText::new(effect.kind.label()).strong().color(
                    if effect.enabled {
                        pal().text
                    } else {
                        pal().text_muted
                    },
                ));
                ui.add_space(-6.0);
                ui.label(
                    egui::RichText::new(if effect.enabled {
                        summary(&effect.kind)
                    } else {
                        "Bypassed".into()
                    })
                    .small()
                    .color(pal().text_faint),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(
                    ui,
                    26.0,
                    if state.is_open() { "Collapse" } else { "Edit" },
                    true,
                    |p, r, c| widgets::icon_chevron(p, r, c, !state.is_open()),
                )
                .clicked()
                {
                    state.toggle(ui);
                }
                if widgets::icon_button(ui, 28.0, "Remove stage", true, widgets::icon_close)
                    .clicked()
                {
                    *removed = Some(index);
                }
                if widgets::icon_button(ui, 28.0, "Move down", index + 1 < count, |p, r, c| {
                    widgets::icon_chevron(p, r, c, true)
                })
                .clicked()
                {
                    *moved = Some((index, index + 1));
                }
                if widgets::icon_button(ui, 28.0, "Move up", index > 0, |p, r, c| {
                    widgets::icon_chevron(p, r, c, false)
                })
                .clicked()
                {
                    *moved = Some((index, index - 1));
                }
                ui.add_space(theme::SPACE_SM);
                if widgets::switch(ui, &mut effect.enabled)
                    .on_hover_text("Bypass keeps the stage in place without processing")
                    .changed()
                {
                    *commit = true;
                }
            });
            title.response
        });
        // Clicking the title opens the editor too.
        if header
            .inner
            .interact(Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            state.toggle(ui);
        }

        state.show_body_unindented(ui, |ui| {
            ui.add_space(theme::SPACE_SM);
            widgets::divider(ui);
            ui.add_space(theme::SPACE_SM);
            *commit |= stage_editor(ui, &mut effect.kind);
        });
    });
    resp.response
}

fn stage_editor(ui: &mut Ui, kind: &mut AuxEffectKind) -> bool {
    let mut c = false;
    match kind {
        AuxEffectKind::NoiseGate {
            threshold_db,
            reduction_db,
            attack_ms,
            hold_ms,
            release_ms,
            detect_hpf_hz,
        } => {
            let (t, r) = (*threshold_db, *reduction_db);
            curve(ui, |x| if x < t { (x + r).max(-72.0) } else { x }, t);
            c |= param(ui, "Threshold", "The gate opens when sound is louder than this. Raise it to cut more background noise.", threshold_db, -60.0..=0.0, "dB", false);
            c |= param(
                ui,
                "Reduction",
                "How much quieter sound gets while the gate is closed. -72 dB is near silence.",
                reduction_db,
                -72.0..=0.0,
                "dB",
                false,
            );
            c |= param(
                ui,
                "Attack",
                "How fast the gate opens once sound crosses the threshold.",
                attack_ms,
                0.0..=200.0,
                "ms",
                false,
            );
            c |= param(
                ui,
                "Hold",
                "How long the gate stays open after sound drops, so word endings aren't cut off.",
                hold_ms,
                0.0..=1000.0,
                "ms",
                false,
            );
            c |= param(
                ui,
                "Release",
                "How fast the gate closes after the hold time ends.",
                release_ms,
                1.0..=2000.0,
                "ms",
                true,
            );
            c |= param(ui, "Ignore below", "The gate ignores hum and rumble below this frequency when deciding to open. Your audio itself is not filtered. 0 = off.", detect_hpf_hz, 0.0..=500.0, "Hz", false);
        }
        AuxEffectKind::Compressor {
            threshold_db,
            ratio,
            attack_ms,
            release_ms,
            knee_db,
            makeup_db,
        } => {
            let (t, r, k, m) = (*threshold_db, *ratio, *knee_db, *makeup_db);
            curve(ui, |x| compress(x, t, r, k) + m, t);
            c |= param(
                ui,
                "Threshold",
                "Sound louder than this gets turned down.",
                threshold_db,
                -60.0..=0.0,
                "dB",
                false,
            );
            c |= param(ui, "Ratio", "How hard loud parts are reduced. At 4:1, 4 dB over the threshold comes out as 1 dB.", ratio, 1.0..=20.0, ":1", true);
            c |= param(
                ui,
                "Knee",
                "Softens the change at the threshold. Lower values compress more gradually.",
                knee_db,
                -24.0..=0.0,
                "dB",
                false,
            );
            c |= param(
                ui,
                "Attack",
                "How quickly compression kicks in when sound gets loud.",
                attack_ms,
                0.0..=200.0,
                "ms",
                false,
            );
            c |= param(
                ui,
                "Release",
                "How quickly compression lets go after sound gets quieter.",
                release_ms,
                1.0..=2000.0,
                "ms",
                true,
            );
            c |= param(
                ui,
                "Makeup gain",
                "Boosts the output after compression to win back lost volume.",
                makeup_db,
                0.0..=24.0,
                "dB",
                false,
            );
        }
        AuxEffectKind::Equalizer { bands } => {
            c |= eq_graph(ui, bands);
            ui.add_space(theme::SPACE_SM);
            c |= eq_table(ui, bands);
        }
    }
    c
}

/// Labeled slider for one parameter; returns true when the value is final.
fn param(
    ui: &mut Ui,
    label: &str,
    help: &str,
    v: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    unit: &str,
    log: bool,
) -> bool {
    let mut commit = false;
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.allocate_ui_with_layout(
            vec2(130.0, 20.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(130.0);
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    egui::RichText::new(label)
                        .size(13.0)
                        .color(pal().text_muted),
                );
                widgets::info(ui, help);
            },
        );
        let (min, max) = (*range.start(), *range.end());
        let resp = widgets::slider(
            ui,
            egui::Slider::new(v, range)
                .show_value(false)
                .logarithmic(log)
                .clamping(egui::SliderClamping::Always),
            (ui.available_width() - 90.0).clamp(120.0, 420.0),
        );
        widgets::knob(ui, &resp, widgets::fraction(*v, min, max, log));
        commit = resp.drag_stopped() || (resp.changed() && !resp.dragged());
        let text = if unit == "Hz" && *v < 10.0 {
            "Off".to_string()
        } else if unit == ":1" {
            format!("{v:.1}:1")
        } else if *v >= 100.0 || unit == "dB" {
            format!("{v:.0} {unit}")
        } else {
            format!("{v:.1} {unit}")
        };
        ui.label(widgets::value_text(text));
    });
    commit
}

/// Soft-knee static compressor curve (input dB → output dB). The knee width
/// is shown as twice the knee depth.
pub fn compress(x: f32, t: f32, ratio: f32, knee_db: f32) -> f32 {
    let w = (-knee_db * 2.0).max(0.0);
    if w > 0.0 && (x - t).abs() <= w / 2.0 {
        x + (1.0 / ratio - 1.0) * (x - t + w / 2.0).powi(2) / (2.0 * w)
    } else if x > t {
        t + (x - t) / ratio
    } else {
        x
    }
}

/// Square input→output transfer plot over −72..0 dB with a threshold marker.
fn curve(ui: &mut Ui, f: impl Fn(f32) -> f32, threshold: f32) {
    let size = vec2(ui.available_width().min(520.0), 150.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 8.0, theme::pal().bg);
    let (lo, hi) = (-72.0f32, 6.0f32);
    let map = |x: f32, y: f32| {
        pos2(
            egui::remap(x, lo..=0.0, rect.left() + 8.0..=rect.right() - 8.0),
            egui::remap(
                y.clamp(lo, hi),
                lo..=hi,
                rect.bottom() - 8.0..=rect.top() + 8.0,
            ),
        )
    };
    for db in (-60..=0).step_by(12) {
        let a = map(db as f32, lo);
        let b = map(db as f32, hi);
        p.line_segment([a, b], Stroke::new(1.0_f32, pal().border));
        let c = map(lo, db as f32);
        let d = map(0.0, db as f32);
        p.line_segment([c, d], Stroke::new(1.0_f32, pal().border));
    }
    p.line_segment(
        [map(lo, lo), map(0.0, 0.0)],
        Stroke::new(1.0_f32, pal().border_hi),
    );
    let tx = map(threshold, lo).x;
    p.line_segment(
        [pos2(tx, rect.top() + 8.0), pos2(tx, rect.bottom() - 8.0)],
        Stroke::new(1.0_f32, theme::pal().warn.gamma_multiply(0.6)),
    );
    let pts: Vec<_> = (0..=96)
        .map(|i| {
            let x = egui::lerp(lo..=0.0, i as f32 / 96.0);
            map(x, f(x))
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(2.0_f32, pal().accent)));
    p.text(
        rect.left_top() + vec2(10.0, 8.0),
        egui::Align2::LEFT_TOP,
        "out",
        egui::FontId::proportional(10.0),
        pal().text_faint,
    );
    p.text(
        rect.right_bottom() - vec2(10.0, 8.0),
        egui::Align2::RIGHT_BOTTOM,
        "in",
        egui::FontId::proportional(10.0),
        pal().text_faint,
    );
    ui.add_space(theme::SPACE_SM);
}

// ── Equalizer ────────────────────────────────────────────────────

const FS: f32 = 48_000.0;
const EQ_RANGE_DB: f32 = 18.0;

/// RBJ cookbook biquad magnitude in dB at `f` Hz.
pub fn band_db(band: &EqBand, f: f32) -> f32 {
    use std::f32::consts::PI;
    let w0 = 2.0 * PI * band.freq_hz / FS;
    let (cw, sw) = (w0.cos(), w0.sin());
    let a = 10f32.powf(band.gain_db / 40.0);
    let alpha = sw / (2.0 * band.q.max(0.05));
    let sa = 2.0 * a.sqrt() * alpha;
    let (b, aa) = match band.kind {
        EqBandKind::Peaking => (
            [1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a],
            [1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a],
        ),
        EqBandKind::LowShelf => (
            [
                a * ((a + 1.0) - (a - 1.0) * cw + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                a * ((a + 1.0) - (a - 1.0) * cw - sa),
            ],
            [
                (a + 1.0) + (a - 1.0) * cw + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                (a + 1.0) + (a - 1.0) * cw - sa,
            ],
        ),
        EqBandKind::HighShelf => (
            [
                a * ((a + 1.0) + (a - 1.0) * cw + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                a * ((a + 1.0) + (a - 1.0) * cw - sa),
            ],
            [
                (a + 1.0) - (a - 1.0) * cw + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * cw),
                (a + 1.0) - (a - 1.0) * cw - sa,
            ],
        ),
        EqBandKind::LowPass => (
            [(1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0],
            [1.0 + alpha, -2.0 * cw, 1.0 - alpha],
        ),
        EqBandKind::HighPass => (
            [(1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0],
            [1.0 + alpha, -2.0 * cw, 1.0 - alpha],
        ),
    };
    let w = 2.0 * PI * f / FS;
    let mag2 = |c: [f32; 3]| {
        let re = c[0] + c[1] * w.cos() + c[2] * (2.0 * w).cos();
        let im = c[1] * w.sin() + c[2] * (2.0 * w).sin();
        re * re + im * im
    };
    10.0 * (mag2(b) / mag2(aa)).max(1e-12).log10()
}

fn freq_to_x(f: f32, r: Rect) -> f32 {
    egui::remap(
        f.log10(),
        20f32.log10()..=20000f32.log10(),
        r.left()..=r.right(),
    )
}

fn x_to_freq(x: f32, r: Rect) -> f32 {
    10f32.powf(egui::remap(
        x,
        r.left()..=r.right(),
        20f32.log10()..=20000f32.log10(),
    ))
}

/// Frequency-response plot with one draggable handle per band.
fn eq_graph(ui: &mut Ui, bands: &mut [EqBand]) -> bool {
    let size = vec2(ui.available_width(), 190.0);
    let (outer, _) = ui.allocate_exact_size(size, Sense::hover());
    let r = outer.shrink2(vec2(12.0, 14.0));
    let p = ui.painter_at(outer);
    p.rect_filled(outer, 8.0, theme::pal().bg);
    let y_of = |db: f32| {
        egui::remap(
            db.clamp(-EQ_RANGE_DB, EQ_RANGE_DB),
            -EQ_RANGE_DB..=EQ_RANGE_DB,
            r.bottom()..=r.top(),
        )
    };

    for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
        let x = freq_to_x(f, r);
        p.line_segment(
            [pos2(x, r.top()), pos2(x, r.bottom())],
            Stroke::new(1.0_f32, pal().border),
        );
        let label = if f >= 1000.0 {
            format!("{}k", f / 1000.0)
        } else {
            format!("{f}")
        };
        p.text(
            pos2(x, outer.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            egui::FontId::proportional(9.5),
            pal().text_faint,
        );
    }
    for db in [-12.0, -6.0, 6.0, 12.0] {
        p.line_segment(
            [pos2(r.left(), y_of(db)), pos2(r.right(), y_of(db))],
            Stroke::new(1.0_f32, pal().border),
        );
    }
    p.line_segment(
        [pos2(r.left(), y_of(0.0)), pos2(r.right(), y_of(0.0))],
        Stroke::new(1.0_f32, pal().border_hi),
    );

    // Summed response + filled area.
    let n = 160;
    let pts: Vec<_> = (0..=n)
        .map(|i| {
            let x = egui::lerp(r.left()..=r.right(), i as f32 / n as f32);
            let f = x_to_freq(x, r);
            let db: f32 = bands.iter().map(|b| band_db(b, f)).sum();
            pos2(x, y_of(db))
        })
        .collect();
    let mut fill = egui::Mesh::default();
    for (i, pt) in pts.iter().enumerate() {
        fill.colored_vertex(*pt, pal().accent.gamma_multiply(0.18));
        fill.colored_vertex(pos2(pt.x, y_of(0.0)), pal().accent.gamma_multiply(0.02));
        if i > 0 {
            let b = (i as u32) * 2;
            fill.add_triangle(b - 2, b - 1, b);
            fill.add_triangle(b - 1, b, b + 1);
        }
    }
    p.add(fill);
    p.add(egui::Shape::line(pts, Stroke::new(2.0_f32, pal().accent)));

    let mut commit = false;
    for (i, band) in bands.iter_mut().enumerate() {
        let center = pos2(
            freq_to_x(band.freq_hz, r),
            y_of(if band.kind.has_gain() {
                band.gain_db
            } else {
                0.0
            }),
        );
        let hit = Rect::from_center_size(center, vec2(18.0, 18.0));
        let resp = ui
            .interact(hit, ui.id().with(("eq_handle", i)), Sense::drag())
            .on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text(format!("{} · drag to tune", band.kind.label()));
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                band.freq_hz = x_to_freq(pos.x.clamp(r.left(), r.right()), r).clamp(20.0, 20000.0);
                if band.kind.has_gain() {
                    band.gain_db = egui::remap(
                        pos.y.clamp(r.top(), r.bottom()),
                        r.bottom()..=r.top(),
                        -EQ_RANGE_DB..=EQ_RANGE_DB,
                    )
                    .clamp(-24.0, 24.0);
                }
            }
        }
        commit |= resp.drag_stopped();
        let active =
            ui.ctx()
                .animate_bool_with_time(resp.id, resp.hovered() || resp.dragged(), 0.1);
        p.circle_filled(center, 7.0 + active * 2.0, theme::pal().surface_hi);
        p.circle_stroke(
            center,
            7.0 + active * 2.0,
            Stroke::new(2.0_f32, pal().accent),
        );
        p.text(
            center,
            egui::Align2::CENTER_CENTER,
            (i + 1).to_string(),
            egui::FontId::proportional(9.5),
            pal().text,
        );
    }
    commit
}

fn eq_table(ui: &mut Ui, bands: &mut Vec<EqBand>) -> bool {
    let mut commit = false;
    let mut remove = None;
    let can_remove = bands.len() > 1;
    egui::Grid::new(ui.id().with("eq_table"))
        .num_columns(6)
        .spacing(vec2(12.0, 6.0))
        .show(ui, |ui| {
            for (h, help) in [
                ("#", ""),
                ("Type", "Bell boosts or cuts around a frequency; shelves move everything above/below it; passes remove what's below (high pass) or above (low pass)."),
                ("Frequency", "Center or corner frequency of the band. Drag a handle on the graph to tune it."),
                ("Gain", "How much the band boosts (+) or cuts (-). Pass filters ignore it."),
                ("Q", "Band width. Higher Q affects a narrower range."),
                ("", ""),
            ] {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    ui.label(theme::eyebrow(h));
                    if !help.is_empty() {
                        widgets::info(ui, help);
                    }
                });
            }
            ui.end_row();
            for (i, band) in bands.iter_mut().enumerate() {
                ui.label(egui::RichText::new((i + 1).to_string()).color(pal().text_muted));
                widgets::dropdown(ui.id().with(("band_kind", i)), band.kind.label(), 120.0)
                    .show_ui(ui, |ui| {
                        for k in EqBandKind::ALL {
                            if ui.selectable_value(&mut band.kind, k, k.label()).changed() {
                                commit = true;
                            }
                        }
                    });
                commit |= drag(ui, &mut band.freq_hz, 20.0..=20000.0, " Hz", 0, true);
                ui.add_enabled_ui(band.kind.has_gain(), |ui| {
                    commit |= drag(ui, &mut band.gain_db, -24.0..=24.0, " dB", 1, false);
                });
                commit |= drag(ui, &mut band.q, 0.1..=10.0, "", 2, true);
                if widgets::icon_button(ui, 24.0, "Remove band", can_remove, widgets::icon_close).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        bands.remove(i);
        commit = true;
    }
    ui.add_space(theme::SPACE_XS);
    if ui
        .add_enabled(bands.len() < MAX_EQ_BANDS, egui::Button::new("+  Add band"))
        .clicked()
    {
        bands.push(EqBand {
            kind: EqBandKind::Peaking,
            freq_hz: 1000.0,
            gain_db: 0.0,
            q: 1.0,
        });
        commit = true;
    }
    commit
}

fn drag(
    ui: &mut Ui,
    v: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
    decimals: usize,
    log: bool,
) -> bool {
    let speed = if log { (*v * 0.01).max(0.01) } else { 0.1 };
    let resp = ui.add(
        egui::DragValue::new(v)
            .range(range)
            .speed(speed)
            .suffix(suffix)
            .fixed_decimals(decimals),
    );
    resp.drag_stopped() || (resp.changed() && !resp.dragged())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biquad_curves_hit_their_design_points() {
        let bell = EqBand {
            kind: EqBandKind::Peaking,
            freq_hz: 1000.0,
            gain_db: 6.0,
            q: 1.0,
        };
        assert!((band_db(&bell, 1000.0) - 6.0).abs() < 0.05);
        assert!(band_db(&bell, 50.0).abs() < 0.3);

        let shelf = EqBand {
            kind: EqBandKind::LowShelf,
            freq_hz: 200.0,
            gain_db: -9.0,
            q: 0.707,
        };
        assert!((band_db(&shelf, 25.0) + 9.0).abs() < 0.5);
        assert!(band_db(&shelf, 10000.0).abs() < 0.2);

        let hp = EqBand {
            kind: EqBandKind::HighPass,
            freq_hz: 100.0,
            gain_db: 0.0,
            q: 0.707,
        };
        assert!((band_db(&hp, 100.0) + 3.0).abs() < 0.2);
        assert!(band_db(&hp, 20.0) < -20.0);
    }

    #[test]
    fn compressor_curve_is_continuous_and_respects_ratio() {
        assert_eq!(compress(-40.0, -20.0, 4.0, 0.0), -40.0);
        assert!((compress(0.0, -20.0, 4.0, 0.0) + 15.0).abs() < 1e-4);
        // Soft knee joins both segments without a jump.
        let w = 12.0;
        let lo = compress(-20.0 - w / 2.0, -20.0, 4.0, -6.0);
        let hi = compress(-20.0 + w / 2.0, -20.0, 4.0, -6.0);
        assert!((lo - (-26.0)).abs() < 1e-3);
        assert!((hi - (-20.0 + 6.0 / 4.0)).abs() < 1e-3);
    }
}
