//! Action library: categorized, searchable chips to click or drag onto a key.

use super::{key_name, Cx};
use crate::theme::{self, pal};
use crate::widgets;
use egui::{vec2, Color32, Sense, Stroke, Ui};
use hako_core::remap::{EvdevKey, RemapMode};

/// Something the library can put on a key. Also the drag-and-drop payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    /// Send this key or button instead.
    Key(EvdevKey),
    Disable,
    /// Back to the device's own behavior.
    Default,
    /// Open the macro editor with an empty macro.
    NewMacro,
    /// Copy the macro already assigned to this source.
    MacroFrom(EvdevKey),
}

// Only codes the daemon's virtual device can emit (keys < 0x100, BTN_LEFT..TASK).
const LETTERS: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];
const DIGITS_PUNCT: [u16; 21] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 26, 27, 43, 39, 40, 41, 51, 52, 53,
];
const MODIFIERS: [u16; 8] = [42, 54, 29, 97, 56, 100, 125, 126];
const MOUSE: [u16; 8] = [0x110, 0x111, 0x112, 0x113, 0x114, 0x115, 0x116, 0x117];
const MEDIA: [u16; 17] = [
    164, 163, 165, 166, 113, 114, 115, 248, 224, 225, 140, 155, 144, 172, 158, 159, 217,
];
const NAVIGATION: [u16; 19] = [
    103, 108, 105, 106, 102, 107, 104, 109, 110, 111, 14, 28, 15, 1, 57, 58, 99, 70, 119,
];
const FUNCTION: [u16; 24] = [
    59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 87, 88, 183, 184, 185, 186, 187, 188, 189, 190, 191,
    192, 193, 194,
];
const NUMPAD: [u16; 17] = [
    69, 98, 55, 74, 71, 72, 73, 78, 75, 76, 77, 79, 80, 81, 96, 82, 83,
];

const CATEGORIES: [&str; 9] = [
    "Keys",
    "Modifiers",
    "Mouse",
    "Media",
    "Navigation",
    "Function",
    "Numpad",
    "Macros",
    "System",
];

/// Features the daemon cannot do yet; shown disabled so nobody hunts for them.
const SOON: [(&str, &str); 4] = [
    (
        "Second layer",
        "Hypershift / G-Shift style layer held by a modifier key",
    ),
    ("Launch program", "Start an application or run a command"),
    ("Type text", "Type a saved snippet"),
    (
        "Per-app profiles",
        "Switch assignments with the focused window",
    ),
];

fn items(cx: &Cx, category: usize) -> Vec<(String, Action, Color32)> {
    let keys = |codes: &[u16]| -> Vec<(String, Action, Color32)> {
        codes
            .iter()
            .map(|&c| (key_name(EvdevKey(c)), Action::Key(EvdevKey(c)), pal().text))
            .collect()
    };
    match category {
        0 => keys(&[&LETTERS[..], &DIGITS_PUNCT[..]].concat()),
        1 => keys(&MODIFIERS),
        2 => keys(&MOUSE),
        3 => keys(&MEDIA),
        4 => keys(&NAVIGATION),
        5 => keys(&FUNCTION),
        6 => keys(&NUMPAD),
        7 => {
            let mut v = vec![(
                "New macro…".to_string(),
                Action::NewMacro,
                theme::MODE_MACRO,
            )];
            v.extend(
                cx.remaps
                    .bindings
                    .iter()
                    .filter(|b| b.mode == RemapMode::Macro)
                    .map(|b| {
                        (
                            format!("Macro on {} · {} steps", key_name(b.source), b.steps.len()),
                            Action::MacroFrom(b.source),
                            theme::MODE_MACRO,
                        )
                    }),
            );
            v
        }
        _ => vec![
            ("Disable".to_string(), Action::Disable, theme::MODE_OFF),
            ("Default".to_string(), Action::Default, pal().text_muted),
        ],
    }
}

const CHIP_H: f32 = 28.0;
const CHIP_GAP: f32 = 6.0;
/// Visible chip rows before the list scrolls (fits all of "Keys").
const CHIP_ROWS: f32 = 7.0;

/// Draggable, clickable action chip.
fn chip(ui: &mut Ui, label: &str, action: Action, color: Color32, current: bool) -> bool {
    let font = egui::FontId::proportional(12.5);
    let galley = ui.fonts(|f| f.layout_no_wrap(label.into(), font.clone(), color));
    let size = vec2((galley.size().x + 20.0).max(34.0), CHIP_H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let hot = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered() || resp.has_focus(), 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        let fill = pal()
            .surface_hi
            .lerp_to_gamma(theme::pal().surface_hover, hot);
        p.rect_filled(
            rect,
            7.0,
            if current {
                color.gamma_multiply(0.18)
            } else {
                fill
            },
        );
        p.rect_stroke(
            rect,
            7.0,
            Stroke::new(
                1.0_f32,
                if current {
                    color
                } else {
                    pal().border.lerp_to_gamma(pal().border_hi, hot)
                },
            ),
            egui::StrokeKind::Inside,
        );
        p.galley(rect.center() - galley.size() / 2.0, galley, color);
    }
    if resp.drag_started() {
        resp.dnd_set_drag_payload(action);
    }
    if resp.dragged() {
        // The chip follows the pointer until it is dropped on a key.
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let layer = egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("bind_drag"));
            let p = ui.ctx().layer_painter(layer);
            let g = p.layout_no_wrap(label.into(), font, pal().on_accent);
            let ghost =
                egui::Rect::from_center_size(pos - vec2(0.0, 22.0), g.size() + vec2(22.0, 12.0));
            super::glow(&p, ghost, 8, 16, pal().accent.gamma_multiply(0.5));
            p.rect_filled(ghost, 8.0, pal().accent);
            p.galley(ghost.center() - g.size() / 2.0, g, pal().on_accent);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::Grab).clicked()
}

fn category_tab(ui: &mut Ui, label: &str, active: bool) -> bool {
    let color = if active {
        pal().on_accent
    } else {
        pal().text_muted
    };
    let galley =
        ui.fonts(|f| f.layout_no_wrap(label.into(), egui::FontId::proportional(12.0), color));
    let (rect, resp) = ui.allocate_exact_size(galley.size() + vec2(18.0, 10.0), Sense::click());
    let hot = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered() || resp.has_focus(), 0.1);
    let p = ui.painter();
    let r = rect.height() / 2.0;
    if active {
        p.rect_filled(rect, r, pal().accent);
    } else {
        p.rect_filled(rect, r, pal().surface_hi.gamma_multiply(0.5 + 0.5 * hot));
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0_f32, pal().border),
            egui::StrokeKind::Inside,
        );
    }
    p.galley(rect.center() - galley.size() / 2.0, galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// Returns the action clicked this frame (drags are delivered to the key).
pub(super) fn panel(ui: &mut Ui, cx: &mut Cx, selected: Option<EvdevKey>) -> Option<Action> {
    let id = egui::Id::new(("bind_library", cx.d.id.key()));
    let (mut category, mut search) = ui
        .ctx()
        .data(|m| m.get_temp::<(usize, String)>(id))
        .unwrap_or_default();
    let bound = selected.and_then(|k| cx.binding(k));
    // The chip matching the selected key's current assignment stays lit.
    let current = bound.map(|b| match b.mode {
        RemapMode::Disabled => Action::Disable,
        RemapMode::Macro => Action::MacroFrom(b.source),
        _ => Action::Key(b.target),
    });
    let macro_open = selected.is_some_and(|k| super::editor::editing_macro(ui.ctx(), cx, k));
    let mut picked = None;

    widgets::section(
        ui,
        "Actions",
        |ui| {
            widgets::info(
                ui,
                "Click an action to assign it to the selected key, or drag it onto any key.",
            );
        },
        |ui| {
            let w = ui.available_width();
            widgets::text_field(ui, &mut search, "Search keys, buttons, media…", w);
            ui.add_space(theme::SPACE_XS);
            let query = search.trim().to_lowercase();
            if query.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    for (i, name) in CATEGORIES.iter().enumerate() {
                        if category_tab(ui, name, i == category) {
                            category = i;
                        }
                    }
                });
            }
            ui.add_space(theme::SPACE_XS);
            widgets::hint(
                ui,
                if macro_open {
                    "Macro open: clicking a key adds it as a step."
                } else if selected.is_some() {
                    "Click to assign, or drag onto any key."
                } else {
                    "Drag an action onto a key, or select a key first."
                },
            );
            ui.add_space(theme::SPACE_XS);

            let list: Vec<_> = if query.is_empty() {
                items(cx, category.min(CATEGORIES.len() - 1))
            } else {
                (0..CATEGORIES.len())
                    .flat_map(|c| items(cx, c))
                    .filter(|(label, ..)| label.to_lowercase().contains(&query))
                    .collect()
            };
            egui::ScrollArea::vertical()
                .id_salt("bind_library_scroll")
                // Whole rows only, so the last visible row is never cut in half.
                .max_height(CHIP_ROWS * (CHIP_H + CHIP_GAP) - CHIP_GAP)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if list.is_empty() {
                        widgets::hint(ui, "Nothing matches. Detect can capture any key directly.");
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(6.0, CHIP_GAP);
                        ui.spacing_mut().interact_size.y = CHIP_H;
                        for (label, action, color) in &list {
                            if chip(ui, label, *action, *color, current == Some(*action)) {
                                picked = Some(*action);
                            }
                        }
                    });
                });

            ui.add_space(theme::SPACE_SM);
            widgets::divider(ui);
            ui.add_space(theme::SPACE_XS);
            ui.label(theme::eyebrow("Coming soon"));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for (label, what) in SOON {
                    let galley = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label.into(),
                            egui::FontId::proportional(12.0),
                            pal().text_faint,
                        )
                    });
                    let (rect, resp) =
                        ui.allocate_exact_size(galley.size() + vec2(18.0, 10.0), Sense::hover());
                    ui.painter().rect_stroke(
                        rect,
                        7.0,
                        Stroke::new(1.0_f32, pal().border),
                        egui::StrokeKind::Inside,
                    );
                    ui.painter().galley(
                        rect.center() - galley.size() / 2.0,
                        galley,
                        pal().text_faint,
                    );
                    resp.on_hover_text(format!("{what}. Not supported by the daemon yet."));
                }
            });
        },
    );
    ui.ctx().data_mut(|m| m.insert_temp(id, (category, search)));
    picked
}
