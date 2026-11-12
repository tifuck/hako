//! The hako logo: Pip and the "hako" wordmark, composed from the same
//! `branding/parts` layers as `branding/build.py` and rasterised by resvg at
//! screen resolution, so the app matches the README logo exactly.

use crate::theme::pal;
use egui::{vec2, Rect, Ui};
use std::sync::OnceLock;

macro_rules! part {
    ($path:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../branding/parts/",
            $path
        ))
    };
}

/// Markup between `<svg …>` and `</svg>` (build.py's `part`).
fn inner(svg: &str) -> &str {
    let start = svg
        .find("<svg")
        .and_then(|i| svg[i..].find('>').map(|j| i + j + 1))
        .unwrap_or(0);
    let end = svg.rfind("</svg>").unwrap_or(svg.len());
    svg[start..end].trim()
}

fn svg(view_box: &str, layers: &[&str]) -> Vec<u8> {
    let body: Vec<&str> = layers.iter().map(|l| inner(l)).collect();
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"{view_box}\">\n{}\n</svg>",
        body.join("\n")
    )
    .into_bytes()
}

// Lockup geometry in art units (build.py's "hako-logo-dark" lockup): Pip's
// square, and the wordmark's ink box beside it.
const PIP_BOX: [f32; 4] = [12.0, 14.0, 96.0, 96.0];
const WORD_BOX: [f32; 4] = [114.0, 35.0, 169.0, 62.0];

fn view_box(b: [f32; 4]) -> String {
    format!("{} {} {} {}", b[0], b[1], b[2], b[3])
}

/// Pip with eyes open or closed, with the light sticker edge on dark themes.
/// Index: `dark as usize * 2 + blink as usize`.
fn pip_svg(dark: bool, blink: bool) -> &'static [u8] {
    static CACHE: OnceLock<[Vec<u8>; 4]> = OnceLock::new();
    let all = CACHE.get_or_init(|| {
        [false, true]
            .map(|d| {
                [false, true].map(|b| {
                    let face = if b {
                        part!("face/sleepy.svg")
                    } else {
                        part!("face/default.svg")
                    };
                    let outline = if d { part!("outline.svg") } else { "" };
                    svg(
                        &view_box(PIP_BOX),
                        &[
                            outline,
                            part!("pose/stand.svg"),
                            part!("wear/tuft.back.svg"),
                            part!("body.svg"),
                            face,
                        ],
                    )
                })
            })
            .concat()
            .try_into()
            .unwrap()
    });
    &all[dark as usize * 2 + blink as usize]
}

/// The wordmark in white, for tinting with the theme's text colour.
fn word_svg() -> &'static [u8] {
    static CACHE: OnceLock<Vec<u8>> = OnceLock::new();
    CACHE.get_or_init(|| {
        svg(
            &view_box(WORD_BOX),
            &[&part!("wordmark.svg").replace("currentColor", "#FFFFFF")],
        )
    })
}

/// Pip and the wordmark, Pip `height` px tall, laid out as in the README.
/// Hovering makes Pip waddle.
pub fn logo(ui: &mut Ui, height: f32) {
    egui_extras::install_image_loaders(ui.ctx());
    let s = height / PIP_BOX[3];
    let width = (WORD_BOX[0] + WORD_BOX[2] - PIP_BOX[0]) * s;
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());
    let pose = idle(ui, resp.id, resp.hovered());
    let dark = pal().dark;
    let blink = pose.blink > 0.5;
    let pip = Rect::from_min_size(rect.min - vec2(0.0, pose.lift * s), vec2(height, height));
    egui::Image::from_bytes(
        format!("bytes://hako-pip-{}-{}.svg", dark as u8, blink as u8),
        pip_svg(dark, blink),
    )
    .fit_to_exact_size(pip.size())
    // Lean from the feet.
    .rotate(pose.tilt, vec2(0.5, 0.95))
    .paint_at(ui, pip);
    let word = Rect::from_min_size(
        rect.min + vec2(WORD_BOX[0] - PIP_BOX[0], WORD_BOX[1] - PIP_BOX[1]) * s,
        vec2(WORD_BOX[2], WORD_BOX[3]) * s,
    );
    egui::Image::from_bytes("bytes://hako-wordmark.svg", word_svg())
        .fit_to_exact_size(word.size())
        .tint(pal().text)
        .paint_at(ui, word);
}

/// Pip alone, `size` px square, for empty states; `asleep` while waiting.
pub fn pip(ui: &mut Ui, size: f32, asleep: bool) {
    egui_extras::install_image_loaders(ui.ctx());
    let dark = pal().dark;
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    egui::Image::from_bytes(
        format!("bytes://hako-pip-{}-{}.svg", dark as u8, asleep as u8),
        pip_svg(dark, asleep),
    )
    .fit_to_exact_size(rect.size())
    .paint_at(ui, rect);
}

/// How Pip is posed this frame.
struct Pose {
    /// 0 = eyes open, 1 = closed.
    blink: f32,
    /// Lean in radians.
    tilt: f32,
    /// Hop height in art units.
    lift: f32,
}

/// Pip's idle loop: a blink every few seconds (sometimes a double), and a
/// little waddle now and then or while `excited`. Repaints only while
/// something moves, then sleeps until the next beat.
fn idle(ui: &Ui, id: egui::Id, excited: bool) -> Pose {
    const BLINK_EVERY: f32 = 5.0;
    const BLINK: f32 = 0.16;
    const WADDLE_EVERY: f32 = 17.0;
    const WADDLE: f32 = 1.4;
    let t = ui.input(|i| i.time) as f32;
    let bump = |x: f32| {
        if (0.0..BLINK).contains(&x) {
            (x / BLINK * std::f32::consts::PI).sin()
        } else {
            0.0
        }
    };
    let ph = t % BLINK_EVERY;
    let double = (t / BLINK_EVERY) as u32 % 3 == 2;
    let blink = bump(ph).max(if double { bump(ph - 0.3) } else { 0.0 });
    let wp = (t + 9.0) % WADDLE_EVERY;
    let hover = ui.ctx().animate_bool_with_time(id, excited, 0.3);
    let waddle = if wp < WADDLE {
        (wp / WADDLE * std::f32::consts::PI).sin()
    } else {
        0.0
    };
    let env = hover.max(waddle);
    let beat = (t * std::f32::consts::TAU * 2.2).sin();
    // A double blink keeps ticking through the gap between its two blinks.
    let moving = blink > 0.0 || env > 0.0 || (double && ph < 0.3 + BLINK) || excited;
    if moving {
        ui.ctx().request_repaint();
    } else {
        let next_blink = BLINK_EVERY - ph;
        let next_waddle = WADDLE_EVERY - wp;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f32(
                next_blink.min(next_waddle).max(0.02),
            ));
    }
    Pose {
        blink,
        tilt: env * 0.1 * beat,
        lift: env * 3.0 * beat.abs(),
    }
}
