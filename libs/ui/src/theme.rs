//! Color themes, typography and shared frames.

use egui::{Color32, CornerRadius, FontId, Frame, Margin, Stroke, TextStyle, Visuals};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Every color the UI paints with. Device illustrations get their own
/// slots so the hardware still reads as an object on light themes.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub rail: Color32,
    pub surface: Color32,
    pub surface_hi: Color32,
    pub surface_hover: Color32,
    pub border: Color32,
    pub border_hi: Color32,
    pub accent: Color32,
    /// Text and icons drawn on an accent fill.
    pub on_accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,
    pub stage: Color32,
    pub deck: Color32,
    pub deck_edge: Color32,
    pub key_side: Color32,
    pub key_top: Color32,
    pub key_top_hi: Color32,
}

pub struct Theme {
    /// Stored in `config.toml`; never rename.
    pub id: &'static str,
    pub label: &'static str,
    pub palette: Palette,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// The first theme is the default.
pub const THEMES: [Theme; 4] = [
    Theme {
        id: "onyx",
        label: "Onyx",
        palette: Palette {
            dark: true,
            bg: rgb(5, 5, 6),
            rail: rgb(9, 9, 11),
            surface: rgb(14, 15, 17),
            surface_hi: rgb(23, 24, 27),
            surface_hover: rgb(31, 33, 37),
            border: rgb(29, 30, 34),
            border_hi: rgb(46, 48, 54),
            accent: rgb(56, 189, 248),
            on_accent: rgb(3, 7, 12),
            ok: rgb(52, 211, 153),
            warn: rgb(251, 191, 36),
            danger: rgb(248, 113, 113),
            text: rgb(244, 245, 247),
            text_muted: rgb(158, 161, 171),
            text_faint: rgb(108, 111, 121),
            stage: rgb(0, 0, 0),
            deck: rgb(15, 16, 19),
            deck_edge: rgb(46, 49, 58),
            key_side: rgb(2, 2, 3),
            key_top: rgb(31, 33, 39),
            key_top_hi: rgb(54, 58, 68),
        },
    },
    Theme {
        id: "graphite",
        label: "Graphite",
        palette: Palette {
            dark: true,
            bg: rgb(12, 13, 16),
            rail: rgb(16, 17, 21),
            surface: rgb(22, 24, 29),
            surface_hi: rgb(30, 33, 40),
            surface_hover: rgb(37, 41, 50),
            border: rgb(36, 39, 47),
            border_hi: rgb(56, 61, 73),
            accent: rgb(56, 189, 248),
            on_accent: rgb(12, 13, 16),
            ok: rgb(52, 211, 153),
            warn: rgb(251, 191, 36),
            danger: rgb(248, 113, 113),
            text: rgb(236, 238, 243),
            text_muted: rgb(150, 156, 170),
            text_faint: rgb(100, 106, 120),
            stage: rgb(9, 10, 12),
            deck: rgb(21, 23, 28),
            deck_edge: rgb(52, 56, 68),
            key_side: rgb(8, 9, 11),
            key_top: rgb(36, 39, 48),
            key_top_hi: rgb(58, 63, 77),
        },
    },
    Theme {
        id: "synthwave",
        label: "Synthwave",
        palette: Palette {
            dark: true,
            bg: rgb(10, 7, 18),
            rail: rgb(14, 10, 24),
            surface: rgb(20, 15, 33),
            surface_hi: rgb(30, 23, 48),
            surface_hover: rgb(39, 30, 61),
            border: rgb(38, 29, 59),
            border_hi: rgb(62, 48, 92),
            accent: rgb(244, 114, 182),
            on_accent: rgb(24, 6, 18),
            ok: rgb(74, 222, 128),
            warn: rgb(250, 204, 21),
            danger: rgb(251, 113, 133),
            text: rgb(246, 242, 255),
            text_muted: rgb(172, 160, 202),
            text_faint: rgb(118, 106, 146),
            stage: rgb(6, 4, 12),
            deck: rgb(21, 16, 35),
            deck_edge: rgb(66, 52, 98),
            key_side: rgb(5, 3, 10),
            key_top: rgb(39, 31, 61),
            key_top_hi: rgb(64, 52, 96),
        },
    },
    Theme {
        id: "daylight",
        label: "Daylight",
        palette: Palette {
            dark: false,
            bg: rgb(243, 244, 246),
            rail: rgb(235, 237, 240),
            surface: rgb(255, 255, 255),
            surface_hi: rgb(240, 242, 245),
            surface_hover: rgb(229, 232, 237),
            border: rgb(222, 225, 230),
            border_hi: rgb(194, 199, 208),
            accent: rgb(10, 116, 240),
            on_accent: rgb(255, 255, 255),
            ok: rgb(22, 150, 72),
            warn: rgb(196, 128, 0),
            danger: rgb(214, 40, 40),
            text: rgb(17, 19, 24),
            text_muted: rgb(82, 88, 101),
            text_faint: rgb(128, 134, 146),
            stage: rgb(231, 234, 239),
            deck: rgb(250, 251, 252),
            deck_edge: rgb(190, 196, 206),
            key_side: rgb(186, 192, 202),
            key_top: rgb(255, 255, 255),
            key_top_hi: rgb(226, 234, 246),
        },
    },
];

// ponytail: one process-wide theme; the client has a single window.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// Switch theme by id; unknown or empty ids fall back to the default.
pub fn set(id: &str) {
    let i = THEMES.iter().position(|t| t.id == id).unwrap_or(0);
    CURRENT.store(i, Ordering::Relaxed);
}

pub fn current() -> &'static Theme {
    &THEMES[CURRENT.load(Ordering::Relaxed)]
}

/// Colors of the active theme.
pub fn pal() -> &'static Palette {
    &current().palette
}

// Binding-mode hues read the same on every theme (remap uses the accent).
pub const MODE_REPEAT: Color32 = rgb(251, 146, 60);
pub const MODE_MACRO: Color32 = rgb(167, 139, 250);
pub const MODE_OFF: Color32 = rgb(160, 120, 128);
/// Mixer: virtual strips and B (virtual microphone) outputs; hardware ones
/// use the accent.
pub const VIRTUAL: Color32 = rgb(167, 139, 250);
pub const ON_VIRTUAL: Color32 = rgb(20, 13, 38);

// ── Spacing ──────────────────────────────────────────────────────
pub const SPACE_XS: f32 = 4.0;
pub const SPACE_SM: f32 = 8.0;
pub const SPACE_MD: f32 = 12.0;
pub const SPACE_LG: f32 = 20.0;
pub const SPACE_XL: f32 = 32.0;

/// Cards; controls use `CONTROL_RADIUS`.
pub const RADIUS: u8 = 12;
pub const CONTROL_RADIUS: u8 = 8;
pub const ROW_H: f32 = 48.0;
/// Height shared by buttons, dropdowns, text fields and segmented controls.
pub const CONTROL_H: f32 = 32.0;
/// Every dropdown is this wide unless it lives in a dense table.
pub const DROPDOWN_W: f32 = 200.0;
/// Minimum width of a text button, so row actions line up.
pub const BUTTON_MIN_W: f32 = 84.0;
/// Slider track and value readout in settings rows (meters reuse them).
pub const SLIDER_W: f32 = 220.0;
pub const VALUE_W: f32 = 64.0;

/// Seconds for page / panel transitions.
pub const ANIM: f32 = 0.18;

pub fn apply(ctx: &egui::Context) {
    let p = pal();
    let mut v = if p.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0_f32, p.border_hi);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = egui::Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 170 } else { 38 }),
    };
    v.popup_shadow = v.window_shadow;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.surface;
    v.code_bg_color = p.surface_hi;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.danger;
    v.slider_trailing_fill = true;
    v.selection.bg_fill = p.accent.gamma_multiply(if p.dark { 0.35 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.0_f32, p.accent);
    v.text_cursor.stroke = Stroke::new(2.0_f32, p.accent);

    let r = CornerRadius::same(CONTROL_RADIUS);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, p.text_muted);
    w.inactive.bg_fill = p.surface_hi;
    w.inactive.weak_bg_fill = p.surface_hi;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, p.border);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.hovered.bg_fill = p.surface_hover;
    w.hovered.weak_bg_fill = p.surface_hover;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, p.border_hi);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, p.text);
    w.active.bg_fill = p.surface_hover;
    w.active.weak_bg_fill = p.surface_hover;
    w.active.bg_stroke = Stroke::new(1.0_f32, p.accent);
    w.active.fg_stroke = Stroke::new(1.0_f32, p.text);
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
    ] {
        s.corner_radius = r;
        s.expansion = 0.0;
    }
    w.open = w.active;
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(22.0)),
            (TextStyle::Body, FontId::proportional(13.5)),
            (TextStyle::Button, FontId::proportional(13.5)),
            (TextStyle::Small, FontId::proportional(11.5)),
            (TextStyle::Monospace, FontId::monospace(12.5)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(SPACE_SM, SPACE_SM);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = CONTROL_H;
        s.spacing.slider_width = SLIDER_W;
        s.spacing.slider_rail_height = 4.0;
        s.spacing.menu_margin = Margin::same(6);
        s.spacing.combo_width = DROPDOWN_W;
        s.spacing.combo_height = 360.0;
        s.animation_time = ANIM;
        s.interaction.selectable_labels = false;
    });
}

pub fn card() -> Frame {
    let p = pal();
    Frame::new()
        .fill(p.surface)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::symmetric(20, 16))
}

pub fn inset() -> Frame {
    let p = pal();
    Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
}

/// Uppercase eyebrow label above sections.
pub fn eyebrow(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase())
        .size(10.5)
        .color(pal().text_faint)
        .strong()
}

/// Smoothstep easing for 0..1 animation progress.
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
