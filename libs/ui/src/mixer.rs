//! Mixer page (concept A, "Console"): input strips, output masters, and a
//! drawer for the selected strip. Edits go out as `MixerOp`s and are applied
//! to the local copy first (`MixerFile::apply`, as the daemon does), so the
//! page never waits on an answer; the daemon's state is adopted once nothing
//! is in flight.

use crate::theme::{self, pal};
use crate::widgets::{self, Tone};
use crate::{aux, HakoAppUi, UiCommand};
use egui::{pos2, vec2, Color32, Rect, Sense, Stroke, Ui};
use hako_core::ipc::{AuxEffectKind, MeterTargetSpec};
use hako_core::mixer::{
    AudioDevice, ChatMixSide, DeviceDirection, EffectChain, Input, InputKind, MixerOp, MixerState,
    Output, OutputKind, Scene, Source, MAX_INPUTS, MAX_LABEL, MAX_OUTPUTS, MAX_SOURCES,
    TRIM_MAX_DB, TRIM_MIN_DB,
};
use std::collections::HashSet;
use std::sync::Arc;

/// A strip by id (input ids are lowercase, output ids `A1`/`B1`: never equal).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Strip {
    Input(String),
    Output(String),
}

impl Strip {
    fn gain_op(&self, gain_db: f32) -> MixerOp {
        match self.clone() {
            Strip::Input(input) => MixerOp::SetInputGain { input, gain_db },
            Strip::Output(output) => MixerOp::SetOutputGain { output, gain_db },
        }
    }

    fn mute_op(&self, mute: bool) -> MixerOp {
        match self.clone() {
            Strip::Input(input) => MixerOp::SetInputMute { input, mute },
            Strip::Output(output) => MixerOp::SetOutputMute { output, mute },
        }
    }

    fn meter(&self) -> MeterTargetSpec {
        match self.clone() {
            Strip::Input(id) => MeterTargetSpec::MixerInput { id },
            Strip::Output(id) => MeterTargetSpec::MixerOutput { id },
        }
    }

    fn id(&self) -> &str {
        match self {
            Strip::Input(id) | Strip::Output(id) => id,
        }
    }
}

// ── Live fader sends ─────────────────────────────────────────────

/// Shortest gap between two live fader sends (~30 Hz).
pub const LIVE_INTERVAL_S: f64 = 1.0 / 30.0;

/// Fader values waiting to be sent: only the newest per strip, at most one
/// send per `LIVE_INTERVAL_S`, and none while an earlier mixer edit is still
/// unanswered, so nothing queues up behind a slow daemon. The value on
/// release goes out at once.
#[derive(Debug, Default)]
pub struct LiveGains {
    dirty: Vec<(Strip, f32)>,
    release: bool,
    last: Option<f64>,
}

impl LiveGains {
    pub fn set(&mut self, strip: Strip, db: f32, release: bool) {
        match self.dirty.iter_mut().find(|(s, _)| *s == strip) {
            Some(slot) => slot.1 = db,
            None => self.dirty.push((strip, db)),
        }
        self.release |= release;
    }

    pub fn is_idle(&self) -> bool {
        self.dirty.is_empty()
    }

    /// The ops to send now, if any. `busy`: a mixer edit is in flight.
    pub fn flush(&mut self, now: f64, busy: bool) -> Option<Vec<MixerOp>> {
        if self.dirty.is_empty() {
            return None;
        }
        let due = self.last.is_none_or(|t| now - t >= LIVE_INTERVAL_S);
        if !self.release && (busy || !due) {
            return None;
        }
        self.last = Some(now);
        self.release = false;
        Some(self.dirty.drain(..).map(|(s, db)| s.gain_op(db)).collect())
    }
}

// ── Ids and names ────────────────────────────────────────────────

/// ASCII letters and digits of `label`, split into words.
fn words(label: &str) -> Vec<String> {
    label
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// `base`, or `base{sep}2`, `base{sep}3`, ... whichever is free.
fn unique(base: String, sep: char, taken: impl Fn(&str) -> bool) -> String {
    (1..)
        .map(|n| {
            if n == 1 {
                base.clone()
            } else {
                format!("{base}{sep}{n}")
            }
        })
        .find(|c| !taken(c))
        .expect("unbounded")
}

/// A new input id from its label: `[a-z0-9-]`, unique among `inputs`.
pub fn new_input_id(label: &str, inputs: &[Input]) -> String {
    let mut base = words(label).join("-").to_ascii_lowercase();
    base.truncate(24);
    let base = base.trim_end_matches('-').to_string();
    let base = if base.is_empty() {
        "input".into()
    } else {
        base
    };
    unique(base, '-', |c| inputs.iter().any(|i| i.id == c))
}

/// Node names hako creates in `scene` (virtual inputs and outputs).
fn created_nodes(scene: &Scene) -> HashSet<&str> {
    let inputs = scene.inputs.iter().filter_map(|i| match &i.kind {
        InputKind::Virtual { node_name, .. } => Some(node_name.as_str()),
        InputKind::Hardware { .. } => None,
    });
    let outputs = scene.outputs.iter().filter_map(|o| match &o.kind {
        OutputKind::Virtual { node_name } => Some(node_name.as_str()),
        OutputKind::Hardware { .. } => None,
    });
    inputs.chain(outputs).collect()
}

/// PipeWire node name for a new virtual strip: `Hako_<Name>` (ASCII words
/// joined by `_`, a leading "hako" dropped), unique in `scene`.
pub fn new_node_name(label: &str, scene: &Scene) -> String {
    let mut w = words(label);
    if w.len() > 1 && w[0].eq_ignore_ascii_case("hako") {
        w.remove(0);
    }
    let mut name = w.join("_");
    name.truncate(48);
    let name = if name.is_empty() {
        "Mixer".into()
    } else {
        name
    };
    let taken = created_nodes(scene);
    unique(format!("Hako_{name}"), '_', |c| taken.contains(c))
}

/// The first free output id with `prefix` (`A` hardware, `B` virtual), or
/// `None` once the scene has `MAX_OUTPUTS` outputs.
pub fn next_output_id(outputs: &[Output], prefix: char) -> Option<String> {
    if outputs.len() >= MAX_OUTPUTS {
        return None;
    }
    (1..=8)
        .map(|n| format!("{prefix}{n}"))
        .find(|id| !outputs.iter().any(|o| &o.id == id))
}

fn valid_label(label: &str) -> bool {
    let l = label.trim();
    !l.is_empty() && l.chars().count() <= MAX_LABEL && !l.chars().any(char::is_control)
}

/// A device description cut to a label.
fn fit_label(text: &str) -> String {
    text.chars()
        .take(MAX_LABEL)
        .collect::<String>()
        .trim()
        .into()
}

pub fn fmt_db(db: f32) -> String {
    if db.abs() < 0.05 {
        "0.0 dB".into()
    } else if db > 0.0 {
        format!("+{db:.1} dB")
    } else {
        format!("{db:.1} dB")
    }
}

// ── Page state ───────────────────────────────────────────────────

enum Dialog {
    AddInput {
        virt: bool,
        name: String,
        picked: Vec<String>,
    },
    AddOutput {
        virt: bool,
        name: String,
        sink: Option<String>,
    },
}

/// What the page does with an edit.
enum Edit {
    /// Apply locally and send.
    Send(MixerOp),
    /// Apply locally only (a control being dragged; `Send` follows).
    Local(MixerOp),
    /// Fader move: local now, sent through `LiveGains` (`true` = released).
    Gain(Strip, f32, bool),
}

/// The mixer as the page shows it.
#[derive(Default)]
pub struct MixerUi {
    /// Daemon state plus local edits not yet answered.
    state: Option<MixerState>,
    /// Newest daemon state, adopted once nothing is in flight.
    remote: Option<MixerState>,
    /// Why the daemon couldn't report the mixer (e.g. a hakod without one).
    error: Option<String>,
    /// Mixer edits sent and not answered yet (the client sets this).
    pub busy: bool,
    selected: Option<Strip>,
    live: LiveGains,
    dialog: Option<Dialog>,
    drawer: Drawer,
}

impl MixerUi {
    /// A `GetMixer` / `PatchMixer` answer or a `MixerUpdated` event.
    pub fn receive(&mut self, state: MixerState) {
        self.remote = Some(state);
        self.error = None;
    }

    /// `GetMixer` failed: the page explains instead of showing stale strips.
    pub fn unavailable(&mut self, why: String) {
        self.state = None;
        self.remote = None;
        self.error = Some(why);
    }

    pub fn state(&self) -> Option<&MixerState> {
        self.state.as_ref()
    }

    /// Show `strip` in the drawer.
    pub fn select(&mut self, strip: Strip) {
        self.selected = Some(strip);
    }

    #[cfg(test)]
    pub(crate) fn open_add_dialog(&mut self, output: bool, virt: bool) {
        self.dialog = Some(if output {
            Dialog::AddOutput {
                virt,
                name: String::new(),
                sink: None,
            }
        } else {
            Dialog::AddInput {
                virt,
                name: String::new(),
                picked: Vec::new(),
            }
        });
    }

    /// Adopt the daemon's state unless local edits are still on their way
    /// (it would jump back) or a control is being dragged.
    pub(crate) fn sync(&mut self, dragging: bool) {
        let settled = !self.busy && !dragging && self.live.is_idle();
        if self.state.is_none() || settled {
            if let Some(r) = self.remote.take() {
                self.state = Some(r);
            }
        }
    }

    pub(crate) fn flush(&mut self, now: f64) -> Option<Vec<MixerOp>> {
        self.live.flush(now, self.busy)
    }

    pub(crate) fn has_unsent(&self) -> bool {
        !self.live.is_idle()
    }

    /// Status line under "Mixer" in the rail, and its dot colour.
    pub(crate) fn rail_status(&self) -> (String, Color32) {
        match (&self.state, &self.error) {
            (None, Some(_)) => ("Not available".into(), pal().text_faint),
            (None, None) => ("Loading…".into(), pal().text_faint),
            (Some(s), _) if !s.file.enabled => ("Off".into(), pal().text_faint),
            (Some(s), _) => (
                format!(
                    "{} in · {} out",
                    s.file.live.inputs.len(),
                    s.file.live.outputs.len()
                ),
                if s.status.running && s.status.problems.is_empty() {
                    pal().ok
                } else {
                    pal().warn
                },
            ),
        }
    }
}

// ── Page ─────────────────────────────────────────────────────────

const STRIP_W: f32 = 88.0;
const STRIP_GAP: f32 = 6.0;
const GROUP_GAP: f32 = 14.0;
const PAD: f32 = 8.0;
const FADER_H: f32 = 168.0;
const BTN_H: f32 = 22.0;
const BTN_GAP: f32 = 4.0;

/// Strip height: name, source, effects, fader, readout, mute row, then one
/// row of route buttons per two outputs.
fn strip_height(outputs: usize) -> f32 {
    let rows = outputs.div_ceil(2).max(1) as f32;
    10.0 + 16.0
        + 14.0
        + 8.0
        + 16.0
        + 8.0
        + FADER_H
        + 6.0
        + 14.0
        + 6.0
        + BTN_H
        + 8.0
        + rows * (BTN_H + BTN_GAP)
        - BTN_GAP
        + 10.0
}

impl HakoAppUi {
    pub(crate) fn mixer_page(&mut self, ui: &mut Ui, out: &mut Vec<UiCommand>) {
        let Some(st) = self.mixer.state.clone() else {
            match &self.mixer.error {
                Some(e) => notice(ui, "Mixer unavailable", &[e.as_str()], None),
                None => notice(ui, "Loading the mixer…", &[], None),
            };
            return;
        };
        let mut edits = Vec::new();
        if !st.file.enabled {
            let on = notice(
                ui,
                "Mixer is off",
                &[
                    "Combine microphones, send line-in to several devices at once, and give Discord or OBS their own virtual microphones.",
                    "Line-in plays through the mixer. Turning it on takes over ChatMix from the headset, with a starting layout that keeps Game and Chat working as they do now.",
                ],
                Some("Turn on mixer"),
            );
            if on {
                edits.push(Edit::Send(MixerOp::SetEnabled { on: true }));
            }
            self.apply_edits(edits, out);
            return;
        }

        self.mixer_header(ui, &st, &mut edits);
        ui.add_space(theme::SPACE_LG);

        let scene = &st.file.live;
        let (hw, virt): (Vec<&Input>, Vec<&Input>) = scene
            .inputs
            .iter()
            .partition(|i| matches!(i.kind, InputKind::Hardware { .. }));
        let order: Vec<Strip> = hw
            .iter()
            .chain(&virt)
            .map(|i| Strip::Input(i.id.clone()))
            .chain(scene.outputs.iter().map(|o| Strip::Output(o.id.clone())))
            .collect();
        let selected = self
            .mixer
            .selected
            .clone()
            .filter(|s| order.contains(s))
            .or_else(|| order.first().cloned());

        if order.is_empty() {
            widgets::section(
                ui,
                "No strips yet",
                |_| {},
                |ui| {
                    widgets::hint(
                        ui,
                        "Add an input for a microphone, line-in or apps, and an output to hear it or record it.",
                    );
                },
            );
        } else {
            // Meters for every strip while the page is drawn.
            // ponytail: also strips scrolled out of view (at most 24, under
            // MAX_METER_TARGETS); filter by the clip rect if the daemon's
            // per-meter cost ever shows.
            let base = self.meter_targets.len();
            self.meter_targets.extend(order.iter().map(Strip::meter));
            let levels = |s: &Strip| {
                order
                    .iter()
                    .position(|o| o == s)
                    .and_then(|k| self.meter_levels.get(base + k).copied())
                    .unwrap_or_default()
            };
            let mut clicked = None;
            let height = strip_height(scene.outputs.len());
            // A visible bar: strips past the edge are easy to miss.
            ui.spacing_mut().scroll = egui::style::ScrollStyle::thin();
            egui::ScrollArea::horizontal()
                .id_salt("mixer_console")
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let mut first = true;
                        for (title, group) in [("Hardware inputs", &hw), ("Virtual inputs", &virt)]
                        {
                            if group.is_empty() {
                                continue;
                            }
                            if !first {
                                ui.add_space(GROUP_GAP);
                            }
                            first = false;
                            strip_group(ui, title, |ui| {
                                for i in group {
                                    let strip = Strip::Input(i.id.clone());
                                    let look = input_look(i, &st, levels(&strip));
                                    let footer = Footer::Input {
                                        solo: i.solo,
                                        routes: &i.routes,
                                        outputs: &scene.outputs,
                                    };
                                    let sel = selected.as_ref() == Some(&strip);
                                    if strip_view(ui, &look, footer, height, sel, &mut edits) {
                                        clicked = Some(strip);
                                    }
                                }
                            });
                        }
                        if !scene.outputs.is_empty() {
                            if !first {
                                console_separator(ui, height);
                            }
                            strip_group(ui, "Outputs", |ui| {
                                for o in &scene.outputs {
                                    let strip = Strip::Output(o.id.clone());
                                    let look = output_look(o, &st, levels(&strip));
                                    let footer = Footer::Output { mono: o.mono };
                                    let sel = selected.as_ref() == Some(&strip);
                                    if strip_view(ui, &look, footer, height, sel, &mut edits) {
                                        clicked = Some(strip);
                                    }
                                }
                            });
                        }
                    });
                    // Room for the floating scroll bar under the strips.
                    ui.add_space(theme::SPACE_MD);
                });
            if let Some(s) = clicked {
                self.mixer.selected = Some(s);
            }
            ui.add_space(theme::SPACE_MD);
            // The drawer reads like the other pages, however wide the console is.
            ui.set_max_width(ui.available_width().min(1040.0));
            let drawer = &mut self.mixer.drawer;
            // A name being edited belongs to the strip it was opened on.
            drawer
                .rename
                .take_if(|r| Some(&r.strip) != selected.as_ref());
            match &selected {
                Some(Strip::Input(id)) => {
                    if let Some(i) = scene.inputs.iter().find(|i| &i.id == id) {
                        input_drawer(ui, drawer, i, &st, &mut edits);
                    }
                }
                Some(Strip::Output(id)) => {
                    if let Some(o) = scene.outputs.iter().find(|o| &o.id == id) {
                        output_drawer(ui, drawer, o, &st, &mut edits);
                    }
                }
                None => {}
            }
        }

        if let Some(strip) = self.mixer.drawer.removing.clone() {
            let (label, op, body) = match &strip {
                Strip::Input(id) => (
                    scene.inputs.iter().find(|i| &i.id == id).map(|i| &i.label),
                    MixerOp::RemoveInput { input: id.clone() },
                    "Its routing and processing chain go with it.",
                ),
                Strip::Output(id) => (
                    scene.outputs.iter().find(|o| &o.id == id).map(|o| &o.label),
                    MixerOp::RemoveOutput { output: id.clone() },
                    "Its processing chain goes with it, and inputs stop sending to it.",
                ),
            };
            // `None`: the strip is gone already, and its question with it.
            let answer = label.map_or(Some(false), |label| {
                let id = egui::Id::new("mixer_remove");
                widgets::confirm(ui.ctx(), id, &format!("Remove {label}?"), body, "Remove")
            });
            if answer == Some(true) {
                edits.push(Edit::Send(op));
            }
            if answer.is_some() {
                self.mixer.drawer.removing = None;
            }
        }

        if let Some(d) = &mut self.mixer.dialog {
            match add_dialog(ui.ctx(), d, &st) {
                DialogEnd::Open => {}
                DialogEnd::Cancel => self.mixer.dialog = None,
                DialogEnd::Create(op, strip) => {
                    edits.push(Edit::Send(op));
                    self.mixer.selected = Some(strip);
                    self.mixer.dialog = None;
                }
            }
        }
        self.apply_edits(edits, out);
    }

    fn mixer_header(&mut self, ui: &mut Ui, st: &MixerState, edits: &mut Vec<Edit>) {
        let scene = &st.file.live;
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_H);
            ui.label(egui::RichText::new("Mixer").heading().color(pal().text));
            if !st.status.running {
                ui.add_space(theme::SPACE_XS);
                widgets::pill(ui, "Not running", pal().warn)
                    .on_hover_text("hakod hasn't built the audio graph for this layout yet.");
            }
            // Every solo, also of strips scrolled out of view.
            let soloed: Vec<&Input> = scene.inputs.iter().filter(|i| i.solo).collect();
            if !soloed.is_empty() {
                ui.add_space(theme::SPACE_XS);
                let labels: Vec<&str> = soloed.iter().map(|i| i.label.as_str()).collect();
                if solo_pill(ui)
                    .on_hover_text(format!(
                        "Only {} can be heard. Click to clear every solo.",
                        labels.join(", ")
                    ))
                    .clicked()
                {
                    edits.extend(clear_solos(&scene.inputs).into_iter().map(Edit::Send));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let off = ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Turn off")
                                .small()
                                .color(pal().text_muted),
                        )
                        .frame(false),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Hands ChatMix back to the headset; line-in stops");
                if off.clicked() {
                    edits.push(Edit::Send(MixerOp::SetEnabled { on: false }));
                }
                let full_out = next_output_id(&scene.outputs, 'A').is_none()
                    && next_output_id(&scene.outputs, 'B').is_none();
                if ui
                    .add_enabled_ui(!full_out, |ui| widgets::button(ui, "+  Add output"))
                    .inner
                    .clicked()
                {
                    self.mixer.dialog = Some(Dialog::AddOutput {
                        virt: false,
                        name: String::new(),
                        sink: None,
                    });
                }
                if ui
                    .add_enabled_ui(scene.inputs.len() < MAX_INPUTS, |ui| {
                        widgets::button(ui, "+  Add input")
                    })
                    .inner
                    .clicked()
                {
                    self.mixer.dialog = Some(Dialog::AddInput {
                        virt: false,
                        name: String::new(),
                        picked: Vec::new(),
                    });
                }
            });
        });
    }

    /// Apply this frame's edits to the local copy and send what is final.
    /// An op the model refuses changes nothing and is not sent.
    fn apply_edits(&mut self, edits: Vec<Edit>, out: &mut Vec<UiCommand>) {
        let MixerUi {
            state: Some(st),
            live,
            selected,
            ..
        } = &mut self.mixer
        else {
            return;
        };
        let mut send = Vec::new();
        let mut refused = None;
        for e in edits {
            match e {
                Edit::Send(op) => match st.file.apply(&op) {
                    Ok(()) => {
                        if matches!(
                            op,
                            MixerOp::RemoveInput { .. } | MixerOp::RemoveOutput { .. }
                        ) {
                            *selected = None;
                        }
                        send.push(op);
                    }
                    Err(e) => refused = Some(e),
                },
                Edit::Local(op) => {
                    let _ = st.file.apply(&op);
                }
                Edit::Gain(strip, db, release) => {
                    if st.file.apply(&strip.gain_op(db)).is_ok() {
                        live.set(strip, db, release);
                    }
                }
            }
        }
        if !send.is_empty() {
            out.push(UiCommand::PatchMixer(send));
        }
        if let Some(e) = refused {
            self.notify(Tone::Error, format!("Couldn't change the mixer: {e}"));
        }
    }
}

/// Centered explanation with an optional primary action; true when clicked.
fn notice(ui: &mut Ui, title: &str, lines: &[&str], action: Option<&str>) -> bool {
    let mut clicked = false;
    // A readable column in the middle of the page.
    let w = ui.available_width().min(520.0);
    let column = Rect::from_min_size(
        ui.cursor().min + vec2((ui.available_width() - w) / 2.0, 0.0),
        vec2(w, f32::INFINITY),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(theme::SPACE_XL * 2.0);
            let (tile, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
            ui.painter().rect(
                tile,
                16.0,
                pal().surface,
                Stroke::new(1.0_f32, pal().border),
                egui::StrokeKind::Inside,
            );
            widgets::icon_mixer(ui.painter(), tile.shrink(18.0), pal().accent);
            ui.add_space(theme::SPACE_MD);
            ui.label(egui::RichText::new(title).heading().color(pal().text));
            ui.add_space(theme::SPACE_SM);
            for l in lines {
                ui.label(egui::RichText::new(*l).color(pal().text_muted));
            }
            if let Some(a) = action {
                ui.add_space(theme::SPACE_LG);
                clicked = widgets::primary_button(ui, a).clicked();
            }
        });
    });
    clicked
}

/// Group caption above its strips.
fn strip_group(ui: &mut Ui, title: &str, strips: impl FnOnce(&mut Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::SPACE_SM;
        ui.label(
            egui::RichText::new(title)
                .size(11.0)
                .color(pal().text_faint),
        );
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = STRIP_GAP;
            strips(ui);
        });
    });
}

/// Hairline between the inputs and the output masters.
fn console_separator(ui: &mut Ui, height: f32) {
    let caption = 11.0 + theme::SPACE_SM + 6.0;
    let (r, _) = ui.allocate_exact_size(
        vec2(2.0 * GROUP_GAP + 1.0, caption + height),
        Sense::hover(),
    );
    let x = r.center().x;
    ui.painter().line_segment(
        [pos2(x, r.top() + caption), pos2(x, r.bottom())],
        Stroke::new(1.0_f32, pal().border),
    );
}

// ── Strips ───────────────────────────────────────────────────────

struct StripLook<'a> {
    strip: Strip,
    label: &'a str,
    /// Output id shown before the label.
    badge: Option<&'a str>,
    summary: String,
    chain: &'a EffectChain,
    gain_db: f32,
    mute: bool,
    /// Silent by another input's solo (inputs only): why.
    solo_mute: Option<String>,
    virt: bool,
    problem: Option<String>,
    levels: [f32; 2],
}

enum Footer<'a> {
    Input {
        solo: bool,
        routes: &'a [String],
        outputs: &'a [Output],
    },
    Output {
        mono: bool,
    },
}

fn problems(st: &MixerState, id: &str) -> Option<String> {
    let msgs: Vec<&str> = st
        .status
        .problems
        .iter()
        .filter(|p| p.strip == id)
        .map(|p| p.message.as_str())
        .collect();
    (!msgs.is_empty()).then(|| msgs.join("\n"))
}

fn device<'a>(st: &'a MixerState, node: &str) -> Option<&'a AudioDevice> {
    st.status.devices.iter().find(|d| d.node == node)
}

/// Ops that clear every solo (sent as one patch).
pub(crate) fn clear_solos(inputs: &[Input]) -> Vec<MixerOp> {
    inputs
        .iter()
        .filter(|i| i.solo)
        .map(|i| MixerOp::SetInputSolo {
            input: i.id.clone(),
            solo: false,
        })
        .collect()
}

/// Why `i` is silent although not muted itself: another input's solo
/// ("Muted by solo on <labels>"). `None` when nothing is soloed or `i` is.
pub(crate) fn solo_mute_reason(i: &Input, inputs: &[Input]) -> Option<String> {
    if i.solo {
        return None;
    }
    let by: Vec<&str> = inputs
        .iter()
        .filter(|o| o.solo)
        .map(|o| o.label.as_str())
        .collect();
    (!by.is_empty()).then(|| format!("Muted by solo on {}", by.join(", ")))
}

fn input_look<'a>(i: &'a Input, st: &MixerState, levels: [f32; 2]) -> StripLook<'a> {
    let (summary, virt) = match &i.kind {
        InputKind::Hardware { sources } => (
            match sources.as_slice() {
                [] => "No device".to_string(),
                [s] => source_name(s).to_string(),
                more => format!("{} devices merged", more.len()),
            },
            false,
        ),
        InputKind::Virtual { .. } => ("Virtual input".to_string(), true),
    };
    StripLook {
        strip: Strip::Input(i.id.clone()),
        label: &i.label,
        badge: None,
        summary,
        chain: &i.chain,
        gain_db: i.gain_db,
        mute: i.mute,
        solo_mute: solo_mute_reason(i, &st.file.live.inputs),
        virt,
        problem: problems(st, &i.id),
        levels,
    }
}

fn output_look<'a>(o: &'a Output, st: &MixerState, levels: [f32; 2]) -> StripLook<'a> {
    let (summary, virt) = match &o.kind {
        OutputKind::Hardware { sink, description } => (
            device(st, sink).map_or_else(
                || sink_name(sink, description).to_string(),
                |d| d.description.clone(),
            ),
            false,
        ),
        OutputKind::Virtual { .. } => ("Virtual microphone".to_string(), true),
    };
    StripLook {
        strip: Strip::Output(o.id.clone()),
        label: &o.label,
        badge: Some(&o.id),
        summary,
        chain: &o.chain,
        gain_db: o.gain_db,
        mute: o.mute,
        solo_mute: None,
        virt,
        problem: problems(st, &o.id),
        levels,
    }
}

/// An output's device by the name it had when picked (it may be unplugged).
fn sink_name<'a>(sink: &'a str, description: &'a str) -> &'a str {
    if description.is_empty() {
        sink
    } else {
        description
    }
}

fn source_name(s: &Source) -> &str {
    if s.description.is_empty() {
        &s.node
    } else {
        &s.description
    }
}

/// One line of text cut with "…" to `width`.
fn elided(ui: &Ui, text: &str, size: f32, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.into(),
        egui::FontId::proportional(size),
        color,
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    ui.fonts(|f| f.layout_job(job))
}

/// How a lit `chip_button` looks.
#[derive(Clone, Copy)]
enum Lit {
    /// Tinted fill and text (mute, solo, mono).
    Tint(Color32),
    /// Solid fill with this text colour (routes).
    Solid(Color32, Color32),
    /// Outline and text only: on because of something else (a solo's mute).
    Outline(Color32),
}

/// "Solo on · Clear": a pill that clears every solo.
fn solo_pill(ui: &mut Ui) -> egui::Response {
    let pill = widgets::pill(ui, "Solo on · Clear", pal().warn);
    ui.interact(pill.rect, ui.id().with("solo_clear"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn chip_button(
    ui: &Ui,
    rect: Rect,
    id: egui::Id,
    text: &str,
    on: bool,
    lit: Lit,
) -> egui::Response {
    let resp = ui
        .interact(rect, id, Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let hot = resp.hovered();
    let (fill, stroke, ink) = match (on, lit) {
        (false, _) => (
            pal().surface_hi,
            if hot { pal().border_hi } else { pal().border },
            if hot { pal().text } else { pal().text_muted },
        ),
        (true, Lit::Tint(c)) => (c.gamma_multiply(0.18), c.gamma_multiply(0.5), c),
        (true, Lit::Solid(c, ink)) => (c, c, ink),
        (true, Lit::Outline(c)) => (pal().surface_hi, c, c),
    };
    let p = ui.painter();
    p.rect(
        rect,
        6.0,
        fill,
        Stroke::new(1.0_f32, stroke),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.0),
        ink,
    );
    resp
}

fn fx_abbrev(kind: &AuxEffectKind) -> &'static str {
    match kind {
        AuxEffectKind::NoiseGate { .. } => "Gate",
        AuxEffectKind::Compressor { .. } => "Comp",
        AuxEffectKind::Equalizer { .. } => "EQ",
        AuxEffectKind::NoiseSuppression { .. } => "AI NR",
        AuxEffectKind::DeEsser { .. } => "DeEss",
        AuxEffectKind::Limiter { .. } => "Lim",
        AuxEffectKind::Distortion { .. } => "Dist",
        AuxEffectKind::PitchShift { .. } => "Pitch",
        AuxEffectKind::AutoTune { .. } => "Tune",
        AuxEffectKind::Vocoder { .. } => "Voc",
        AuxEffectKind::Robot { .. } => "Robot",
        AuxEffectKind::Bitcrusher { .. } => "Crush",
        AuxEffectKind::Reverb { .. } => "Verb",
        AuxEffectKind::Delay { .. } => "Delay",
        AuxEffectKind::Chorus { .. } => "Chor",
        AuxEffectKind::Flanger { .. } => "Flang",
    }
}

/// Stage abbreviations, lit when the stage runs; "+n" for those that don't fit.
fn fx_chips(ui: &Ui, rect: Rect, chain: &EffectChain) {
    let p = ui.painter();
    let font = egui::FontId::proportional(9.5);
    let chip = |x: f32, text: &str, on: bool| {
        let ink = if on { pal().accent } else { pal().text_faint };
        let g = p.layout_no_wrap(text.into(), font.clone(), ink);
        let r = Rect::from_min_size(pos2(x, rect.top()), vec2(g.size().x + 6.0, rect.height()));
        (r, g, on)
    };
    let draw = |(r, g, on): (Rect, Arc<egui::Galley>, bool)| {
        let fill = if on {
            pal().accent.gamma_multiply(0.14)
        } else {
            pal().surface_hi
        };
        p.rect_filled(r, 4.0, fill);
        p.galley(r.center() - g.size() / 2.0, g, pal().text_faint);
        r.right() + 3.0
    };
    if chain.effects.is_empty() {
        draw(chip(rect.left(), "—", false));
        return;
    }
    let mut x = rect.left();
    for (n, e) in chain.effects.iter().enumerate() {
        let c = chip(x, fx_abbrev(&e.kind), e.enabled);
        let rest = chain.effects.len() - n;
        // Keep room for "+n" unless this is the last one.
        let more_w = if rest > 1 { 20.0 } else { 0.0 };
        if c.0.right() + more_w > rect.right() {
            let more = chip(x, &format!("+{rest}"), false);
            if more.0.right() <= rect.right() {
                draw(more);
            }
            return;
        }
        x = draw(c);
    }
}

/// One channel strip. Returns true when it was picked (clicked or grabbed).
fn strip_view(
    ui: &mut Ui,
    s: &StripLook,
    footer: Footer,
    height: f32,
    selected: bool,
    edits: &mut Vec<Edit>,
) -> bool {
    let (rect, bg) = ui.allocate_exact_size(vec2(STRIP_W, height), Sense::click());
    let bg = bg.on_hover_cursor(egui::CursorIcon::PointingHand);
    let id = egui::Id::new(("mixer_strip", &s.strip));
    let tint = if s.virt { theme::VIRTUAL } else { pal().accent };
    let fill = if s.virt {
        pal().surface.lerp_to_gamma(theme::VIRTUAL, 0.05)
    } else {
        pal().surface
    };
    let stroke = if selected {
        Stroke::new(1.5_f32, tint)
    } else if bg.hovered() {
        Stroke::new(1.0_f32, pal().border_hi)
    } else {
        Stroke::new(1.0_f32, pal().border)
    };
    ui.painter()
        .rect(rect, 12.0, fill, stroke, egui::StrokeKind::Inside);
    let inner = rect.shrink2(vec2(PAD, 10.0));
    let p = ui.painter();
    let mut y = inner.top();

    // Name, then the output id badge and what the strip plays from / to.
    let name = elided(ui, s.label, 13.0, pal().text, inner.width());
    p.galley(
        pos2(inner.left(), y + 8.0 - name.size().y / 2.0),
        name,
        pal().text,
    );
    y += 16.0;
    let mut x = inner.left();
    if let Some(b) = s.badge {
        let g = p.layout_no_wrap(b.into(), egui::FontId::proportional(9.5), tint);
        let r = Rect::from_min_size(pos2(x, y), vec2(g.size().x + 6.0, 13.0));
        p.rect_filled(r, 3.0, pal().surface_hi);
        p.galley(r.center() - g.size() / 2.0, g, tint);
        x = r.right() + 4.0;
    }
    let sub = elided(ui, &s.summary, 10.5, pal().text_muted, inner.right() - x);
    p.galley(pos2(x, y + 6.5 - sub.size().y / 2.0), sub, pal().text_muted);
    y += 14.0 + 8.0;
    // Effects, and a marker on the right when something is wrong.
    let chips_end = if let Some(msg) = &s.problem {
        let c = pos2(inner.right() - 7.0, y + 8.0);
        p.circle_filled(c, 7.0, pal().warn);
        p.text(
            c,
            egui::Align2::CENTER_CENTER,
            "!",
            egui::FontId::proportional(11.0),
            pal().bg,
        );
        ui.interact(
            Rect::from_center_size(c, vec2(18.0, 18.0)),
            id.with("problem"),
            Sense::hover(),
        )
        .on_hover_text(msg);
        inner.right() - 18.0
    } else {
        inner.right()
    };
    fx_chips(
        ui,
        Rect::from_x_y_ranges(inner.left()..=chips_end, y..=y + 16.0),
        s.chain,
    );
    y += 16.0 + 8.0;

    // Meter beside the fader, the meter spanning the fader's travel.
    let body_w = 12.0 + 6.0 + 26.0;
    let left = inner.center().x - body_w / 2.0;
    let meter = Rect::from_min_size(pos2(left, y + 6.0), vec2(12.0, FADER_H - 12.0));
    widgets::vertical_meter(ui, id.with("meter"), meter, s.levels);
    let fader = Rect::from_min_size(pos2(left + 18.0, y), vec2(26.0, FADER_H));
    let mut db = s.gain_db;
    let implied = !s.mute && s.solo_mute.is_some();
    let fill = if s.mute || implied {
        pal().text_faint
    } else {
        tint
    };
    let f = widgets::vertical_fader(ui, fader, id.with("fader"), &mut db, fill);
    if implied {
        // Silenced by a solo elsewhere: the strip's level reads as off.
        ui.painter().rect_filled(
            meter.union(fader).expand(2.0),
            4.0,
            pal().surface.gamma_multiply(0.6),
        );
    }
    if f.changed() || f.drag_stopped() {
        edits.push(Edit::Gain(s.strip.clone(), db, !f.dragged()));
    }
    y += FADER_H + 6.0;
    ui.painter().text(
        pos2(inner.center().x, y + 7.0),
        egui::Align2::CENTER_CENTER,
        fmt_db(db),
        egui::FontId::proportional(11.5),
        pal().text_muted,
    );
    y += 14.0 + 6.0;

    let half = (inner.width() - BTN_GAP) / 2.0;
    let first = Rect::from_min_size(pos2(inner.left(), y), vec2(half, BTN_H));
    let second = first.translate(vec2(half + BTN_GAP, 0.0));
    let (lit, tip) = match &s.solo_mute {
        Some(why) if implied => (Lit::Outline(pal().danger), why.clone()),
        _ => (
            Lit::Tint(pal().danger),
            if s.mute { "Unmute" } else { "Mute" }.to_string(),
        ),
    };
    if chip_button(ui, first, id.with("mute"), "M", s.mute || implied, lit)
        .on_hover_text(tip)
        .clicked()
    {
        edits.push(Edit::Send(s.strip.mute_op(!s.mute)));
    }
    y += BTN_H + 8.0;
    match footer {
        Footer::Input {
            solo,
            routes,
            outputs,
        } => {
            if chip_button(
                ui,
                second,
                id.with("solo"),
                "S",
                solo,
                Lit::Tint(pal().warn),
            )
            .on_hover_text("Solo: hear only soloed inputs")
            .clicked()
            {
                edits.push(Edit::Send(MixerOp::SetInputSolo {
                    input: s.strip.id().into(),
                    solo: !solo,
                }));
            }
            if outputs.is_empty() {
                let g = elided(ui, "No outputs yet", 10.0, pal().text_faint, inner.width());
                ui.painter().galley(
                    pos2(inner.center().x - g.size().x / 2.0, y + 4.0),
                    g,
                    pal().text_faint,
                );
            }
            for (n, o) in outputs.iter().enumerate() {
                let r = Rect::from_min_size(
                    pos2(
                        inner.left() + (n % 2) as f32 * (half + BTN_GAP),
                        y + (n / 2) as f32 * (BTN_H + BTN_GAP),
                    ),
                    vec2(half, BTN_H),
                );
                let on = routes.contains(&o.id);
                let lit = if matches!(o.kind, OutputKind::Virtual { .. }) {
                    Lit::Solid(theme::VIRTUAL, theme::ON_VIRTUAL)
                } else {
                    Lit::Solid(pal().accent, pal().on_accent)
                };
                if chip_button(ui, r, id.with(("route", &o.id)), &o.id, on, lit)
                    .on_hover_text(format!(
                        "{} {}",
                        if on { "Stop sending to" } else { "Send to" },
                        o.label
                    ))
                    .clicked()
                {
                    edits.push(Edit::Send(MixerOp::SetRoute {
                        input: s.strip.id().into(),
                        output: o.id.clone(),
                        on: !on,
                    }));
                }
            }
        }
        Footer::Output { mono } => {
            if chip_button(
                ui,
                second,
                id.with("mono"),
                "Mono",
                mono,
                Lit::Tint(pal().accent),
            )
            .on_hover_text("Fold left and right into both channels")
            .clicked()
            {
                edits.push(Edit::Send(MixerOp::SetOutputMono {
                    output: s.strip.id().into(),
                    mono: !mono,
                }));
            }
            let lines = if s.virt {
                ["Shows as a mic", "in other apps"]
            } else {
                ["Plays on", "this device"]
            };
            let mid = (rect.bottom() - 10.0 + y) / 2.0;
            for (k, line) in lines.iter().enumerate() {
                ui.painter().text(
                    pos2(inner.center().x, mid + (k as f32 - 0.5) * 13.0),
                    egui::Align2::CENTER_CENTER,
                    *line,
                    egui::FontId::proportional(10.0),
                    pal().text_faint,
                );
            }
        }
    }
    bg.clicked() || f.drag_started() || f.clicked()
}

// ── Drawer ───────────────────────────────────────────────────────

/// The drawer's transient state: the name being edited, and the strip whose
/// removal waits for a yes.
#[derive(Default)]
struct Drawer {
    rename: Option<Rename>,
    removing: Option<Strip>,
}

struct Rename {
    strip: Strip,
    draft: String,
    /// The field has asked for the keyboard (once).
    focused: bool,
}

/// How a rename field ends this frame.
#[derive(Debug, PartialEq)]
pub(crate) enum RenameEnd {
    Open,
    Commit(String),
    Cancel,
}

/// Enter or leaving the field commits a changed, usable name; Escape (or
/// nothing to change) leaves the strip as it was.
pub(crate) fn rename_end(label: &str, draft: &str, lost_focus: bool, escape: bool) -> RenameEnd {
    if !lost_focus {
        return RenameEnd::Open;
    }
    let name = fit_label(draft);
    if escape || !valid_label(&name) || name == label {
        RenameEnd::Cancel
    } else {
        RenameEnd::Commit(name)
    }
}

/// What apps and PipeWire tools call a strip's nodes: `hako <label>`, once
/// (the rule `hako-audio`'s mixer graph describes them by).
pub(crate) fn shown_as(label: &str) -> String {
    if label
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("hako"))
    {
        label.to_string()
    } else {
        format!("hako {label}")
    }
}

/// What an input's drawer shows for where its sound comes from.
#[derive(Debug, PartialEq)]
pub(crate) enum DeviceRows {
    /// "Shown to apps as".
    Virtual,
    /// One "Device" row (also while no device is picked): the fader is the
    /// only volume, so no trim, mute or remove.
    Single,
    /// The "Devices" list: mix balance, mute and remove per device.
    Merged,
}

pub(crate) fn device_rows(i: &Input) -> DeviceRows {
    match &i.kind {
        InputKind::Virtual { .. } => DeviceRows::Virtual,
        InputKind::Hardware { sources } if sources.len() > 1 => DeviceRows::Merged,
        InputKind::Hardware { .. } => DeviceRows::Single,
    }
}

/// The "Headset dial" row: for inputs apps play into, and any input already
/// linked to the dial (so it can be unlinked).
pub(crate) fn shows_dial(i: &Input) -> bool {
    i.chatmix.is_some() || matches!(i.kind, InputKind::Virtual { .. })
}

fn new_source(d: &AudioDevice) -> Source {
    Source {
        node: d.node.clone(),
        description: d.description.clone(),
        trim_db: 0.0,
        mute: false,
        alsa_input: None,
        capture_percent: None,
    }
}

/// `i` with its devices edited, as one whole-strip op (everything else kept).
fn with_sources(i: &Input, edit: impl FnOnce(&mut Vec<Source>)) -> MixerOp {
    let mut input = i.clone();
    if let InputKind::Hardware { sources } = &mut input.kind {
        edit(sources);
    }
    MixerOp::PutInput { input }
}

/// Record from `d` instead (single-device inputs). The old card's jack and
/// input gain don't carry over.
pub(crate) fn set_device(i: &Input, d: &AudioDevice) -> MixerOp {
    with_sources(i, |s| *s = vec![new_source(d)])
}

/// Mix `d` into the strip as well.
pub(crate) fn merge_device(i: &Input, d: &AudioDevice) -> MixerOp {
    with_sources(i, |s| s.push(new_source(d)))
}

/// Take device `k` out of the mix. With one left, the model folds its trim
/// and mute into the strip (`Scene::normalize`).
pub(crate) fn unmerge_device(i: &Input, k: usize) -> MixerOp {
    with_sources(i, |s| {
        if k < s.len() {
            s.remove(k);
        }
    })
}

/// A drawer card's title line: the strip's name (click it or the pencil to
/// rename in place), what kind of strip it is, and "Remove…" (asked again
/// before anything goes). Returns the new name once one is committed.
fn drawer_header(
    ui: &mut Ui,
    drawer: &mut Drawer,
    key: &Strip,
    label: &str,
    kind: &str,
    virt: bool,
) -> Option<String> {
    if let Strip::Output(id) = key {
        ui.label(egui::RichText::new(id).size(15.0).color(pal().text_muted));
    }
    let mut end = RenameEnd::Open;
    match drawer.rename.as_mut().filter(|r| &r.strip == key) {
        Some(r) => {
            let field = widgets::text_field(ui, &mut r.draft, "Name", theme::DROPDOWN_W);
            if r.focused {
                let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
                end = rename_end(label, &r.draft, field.lost_focus(), escape);
            } else {
                field.request_focus();
                r.focused = true;
            }
        }
        None => {
            let title = ui
                .add(egui::Label::new(widgets::section_title(label)).sense(Sense::click()))
                .on_hover_cursor(egui::CursorIcon::Text)
                .on_hover_text("Rename");
            let pencil = widgets::icon_button(ui, 24.0, "Rename", true, widgets::icon_pencil);
            if title.clicked() || pencil.clicked() {
                drawer.rename = Some(Rename {
                    strip: key.clone(),
                    draft: label.to_string(),
                    focused: false,
                });
            }
        }
    }
    kind_pill(ui, kind, virt);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if widgets::button(ui, "Remove…").clicked() {
            drawer.removing = Some(key.clone());
        }
    });
    match end {
        RenameEnd::Open => None,
        RenameEnd::Cancel => {
            drawer.rename = None;
            None
        }
        RenameEnd::Commit(name) => {
            drawer.rename = None;
            Some(name)
        }
    }
}

/// What the daemon reports wrong with this strip, atop its drawer.
fn problem_line(ui: &mut Ui, st: &MixerState, id: &str) {
    if let Some(msg) = problems(st, id) {
        ui.colored_label(pal().warn, msg);
        ui.add_space(theme::SPACE_XS);
    }
}

fn kind_pill(ui: &mut Ui, text: &str, virt: bool) {
    widgets::pill(ui, text, if virt { theme::VIRTUAL } else { pal().accent });
}

/// The name apps list a virtual strip under (read-only: it follows the
/// strip's name).
fn shown_row(ui: &mut Ui, label: &str, node_name: &str, hint: &str) {
    widgets::row(ui, "Shown to apps as", Some(hint), |ui| {
        ui.label(widgets::value_text(shown_as(label)))
            .on_hover_text(format!("PipeWire node {node_name}"));
    });
}

/// The stage editor bound to a strip's chain: drags apply locally, finished
/// edits are sent.
fn chain_section(
    ui: &mut Ui,
    key: &Strip,
    chain: &EffectChain,
    op: impl Fn(EffectChain) -> MixerOp,
    edits: &mut Vec<Edit>,
) {
    let n = chain.effects.iter().filter(|e| e.enabled).count();
    widgets::section(
        ui,
        "Processing chain",
        |ui| {
            ui.label(
                egui::RichText::new(format!("{n} active"))
                    .small()
                    .color(pal().text_faint),
            );
        },
        |ui| {
            ui.push_id(("mixer_chain", key), |ui| {
                let mut draft = chain.clone();
                let commit = aux::chain_editor(ui, &mut draft);
                if commit {
                    edits.push(Edit::Send(op(draft)));
                } else if &draft != chain {
                    edits.push(Edit::Local(op(draft)));
                }
            });
        },
    );
}

fn input_drawer(
    ui: &mut Ui,
    drawer: &mut Drawer,
    i: &Input,
    st: &MixerState,
    edits: &mut Vec<Edit>,
) {
    let key = Strip::Input(i.id.clone());
    let put = |f: &dyn Fn(&mut Input)| {
        let mut next = i.clone();
        f(&mut next);
        Edit::Send(MixerOp::PutInput { input: next })
    };
    let virt = device_rows(i) == DeviceRows::Virtual;
    let kind = if virt {
        "Virtual input"
    } else {
        "Hardware input"
    };
    let mut renamed = None;
    widgets::section_with(
        ui,
        |ui| renamed = drawer_header(ui, drawer, &key, &i.label, kind, virt),
        |ui| {
            problem_line(ui, st, &i.id);
            match &i.kind {
                InputKind::Virtual { node_name, apps } => {
                    shown_row(
                        ui,
                        &i.label,
                        node_name,
                        "Pick this as the output in a game, a call or a music app",
                    );
                    apps_row(ui, i, apps, st, edits);
                }
                InputKind::Hardware { sources } => {
                    if device_rows(i) == DeviceRows::Merged {
                        ui.add_space(theme::SPACE_SM);
                        ui.label(egui::RichText::new("Devices").color(pal().text));
                        for (k, s) in sources.iter().enumerate() {
                            ui.push_id(k, |ui| merged_row(ui, i, k, s, st, edits));
                        }
                    } else {
                        device_row(ui, i, sources.first(), st, edits);
                    }
                    if !sources.is_empty() {
                        merge_link(ui, i, sources, st, edits);
                    }
                }
            }
            if shows_dial(i) {
                widgets::divider(ui);
                widgets::row(
                    ui,
                    "Headset dial",
                    Some("The headset's ChatMix dial turns this input up or down"),
                    |ui| {
                        let current = match i.chatmix {
                            None => 0,
                            Some(ChatMixSide::Game) => 1,
                            Some(ChatMixSide::Chat) => 2,
                        };
                        let seg_id = egui::Id::new(("mixer_chatmix", &i.id));
                        let labels = ["Not linked", "Game side", "Chat side"];
                        if let Some(k) = widgets::segmented(ui, seg_id, &labels, Some(current)) {
                            let side = [None, Some(ChatMixSide::Game), Some(ChatMixSide::Chat)][k];
                            edits.push(put(&|n| n.chatmix = side));
                        }
                    },
                );
            }
        },
    );
    if let Some(label) = renamed {
        edits.push(put(&|n| n.label = label.clone()));
    }
    chain_section(
        ui,
        &key,
        &i.chain,
        |chain| MixerOp::SetInputChain {
            input: i.id.clone(),
            chain,
        },
        edits,
    );
}

/// Apps playing now that no input but `except` takes (an app plays into one).
pub(crate) fn free_apps<'a>(st: &'a MixerState, except: &str) -> Vec<&'a str> {
    let taken: HashSet<&str> = st
        .file
        .live
        .inputs
        .iter()
        .filter(|i| i.id != except)
        .flat_map(|i| match &i.kind {
            InputKind::Virtual { apps, .. } => apps.as_slice(),
            InputKind::Hardware { .. } => &[],
        })
        .map(String::as_str)
        .collect();
    st.status
        .apps
        .iter()
        .map(String::as_str)
        .filter(|a| !taken.contains(a))
        .collect()
}

/// The input with `app` added (`on`) or removed.
pub(crate) fn set_app(i: &Input, app: &str, on: bool) -> MixerOp {
    let mut next = i.clone();
    if let InputKind::Virtual { apps, .. } = &mut next.kind {
        apps.retain(|a| a != app);
        if on {
            apps.push(app.to_string());
        }
    }
    MixerOp::PutInput { input: next }
}

/// A virtual input's apps: their sound is moved into it.
fn apps_row(ui: &mut Ui, i: &Input, apps: &[String], st: &MixerState, edits: &mut Vec<Edit>) {
    let hint = "Their sound plays through this input, whatever output they picked";
    widgets::row(ui, "Apps", Some(hint), |ui| {
        let current = if apps.is_empty() {
            "None".to_string()
        } else {
            apps.join(", ")
        };
        widgets::dropdown(("mixer_apps", &i.id), current, theme::DROPDOWN_W).show_ui(ui, |ui| {
            let mut list: Vec<&str> = apps.iter().map(String::as_str).collect();
            list.extend(
                free_apps(st, &i.id)
                    .into_iter()
                    .filter(|a| !apps.iter().any(|x| x == a)),
            );
            if list.is_empty() {
                widgets::hint(ui, "No app is playing sound");
            }
            for app in list {
                let on = apps.iter().any(|a| a == app);
                let text = if st.status.apps.iter().any(|a| a == app) {
                    app.to_string()
                } else {
                    format!("{app} (not playing)")
                };
                let free = on || apps.len() < MAX_SOURCES;
                if ui
                    .add_enabled(free, egui::SelectableLabel::new(on, text))
                    .clicked()
                {
                    edits.push(Edit::Send(set_app(i, app, !on)));
                }
            }
        });
    });
}

/// Width at which a device's jack and input gain fit beside its dropdown.
const DEVICE_ONE_LINE_W: f32 = 780.0;

fn channels(d: &AudioDevice) -> &'static str {
    if d.channels == 1 {
        "mono"
    } else {
        "stereo"
    }
}

/// A number readout of fixed width, right-aligned (so sliders don't shift).
fn value_cell(ui: &mut Ui, text: String, width: f32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
    ui.painter().text(
        r.right_center(),
        egui::Align2::RIGHT_CENTER,
        text,
        egui::TextStyle::Body.resolve(ui.style()),
        pal().text,
    );
    resp
}

fn caption(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(11.5).color(pal().text_muted));
}

/// A hardware input's one device: which one, and its card's jack and input
/// gain when it has them. The strip's fader is its only volume.
fn device_row(
    ui: &mut Ui,
    i: &Input,
    source: Option<&Source>,
    st: &MixerState,
    edits: &mut Vec<Edit>,
) {
    let dev = source.and_then(|s| device(st, &s.node));
    let card = source
        .zip(dev)
        .filter(|(_, d)| !d.inputs.is_empty() || d.capture);
    let one_line = ui.available_width() >= DEVICE_ONE_LINE_W;
    let rtl = egui::Layout::right_to_left(egui::Align::Center);
    ui.horizontal(|ui| {
        ui.set_min_height(theme::ROW_H);
        ui.label(egui::RichText::new("Device").color(pal().text));
        ui.with_layout(rtl, |ui| {
            if let Some((s, d)) = card.filter(|_| one_line) {
                capture_controls(ui, i, 0, s, d, edits);
                ui.add_space(theme::SPACE_SM);
            }
            let current = match (source, dev) {
                (None, _) => "Pick a device".to_string(),
                (Some(_), Some(d)) => d.description.clone(),
                (Some(s), None) => format!("{} (offline)", source_name(s)),
            };
            widgets::dropdown(("mixer_source", &i.id), current, theme::DROPDOWN_W).show_ui(
                ui,
                |ui| {
                    let devices = capture_devices(st);
                    if devices.is_empty() {
                        widgets::hint(ui, "No capture devices found");
                    }
                    for d in devices {
                        let picked = source.is_some_and(|s| s.node == d.node);
                        let text = format!("{} · {}", d.description, channels(d));
                        if ui.selectable_label(picked, text).clicked() && !picked {
                            edits.push(Edit::Send(set_device(i, d)));
                        }
                    }
                },
            );
        });
    });
    let Some((s, d)) = card else {
        return;
    };
    if !one_line {
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), rtl, |ui| {
            capture_controls(ui, i, 0, s, d, edits);
        });
    }
    if d.capture {
        widgets::hint(
            ui,
            "How loud the device records. Set it once, then use the fader for volume.",
        );
    }
}

/// "+ Merge another device": a second (third, ...) device mixed into the
/// same strip.
fn merge_link(ui: &mut Ui, i: &Input, sources: &[Source], st: &MixerState, edits: &mut Vec<Edit>) {
    let free: Vec<&AudioDevice> = capture_devices(st)
        .into_iter()
        .filter(|d| !sources.iter().any(|s| s.node == d.node))
        .collect();
    ui.add_enabled_ui(sources.len() < MAX_SOURCES, |ui| {
        // A line of text, not a control-height button.
        ui.spacing_mut().interact_size.y = 20.0;
        let link = egui::Button::new(
            egui::RichText::new("+ Merge another device")
                .small()
                .color(pal().accent),
        )
        .frame(false);
        egui::menu::menu_custom_button(ui, link, |ui| {
            if free.is_empty() {
                widgets::hint(ui, "No other capture devices found");
            }
            for d in &free {
                let text = format!("{} · {}", d.description, channels(d));
                if ui.button(text).clicked() {
                    edits.push(Edit::Send(merge_device(i, d)));
                    ui.close_menu();
                }
            }
        })
        .response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Mix two or more devices into this one strip");
    });
}

/// One device of a merged input: name, channels, its share of the mix (the
/// trim), mute, remove, then its card's jack and input gain.
fn merged_row(
    ui: &mut Ui,
    i: &Input,
    k: usize,
    s: &Source,
    st: &MixerState,
    edits: &mut Vec<Edit>,
) {
    let dev = device(st, &s.node);
    let rtl = egui::Layout::right_to_left(egui::Align::Center);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 36.0), rtl, |ui| {
        if widgets::icon_button(ui, 26.0, "Remove this device", true, widgets::icon_close).clicked()
        {
            edits.push(Edit::Send(unmerge_device(i, k)));
        }
        let (m, _) = ui.allocate_exact_size(vec2(30.0, BTN_H), Sense::hover());
        if chip_button(
            ui,
            m,
            ui.id().with("mute"),
            "M",
            s.mute,
            Lit::Tint(pal().danger),
        )
        .on_hover_text(if s.mute {
            "Unmute this device"
        } else {
            "Mute this device"
        })
        .clicked()
        {
            edits.push(Edit::Send(MixerOp::SetSourceMute {
                input: i.id.clone(),
                source: k,
                mute: !s.mute,
            }));
        }
        value_cell(ui, fmt_db(s.trim_db), 64.0);
        let mut trim = s.trim_db;
        let resp = widgets::slider(
            ui,
            egui::Slider::new(&mut trim, TRIM_MIN_DB..=TRIM_MAX_DB)
                .show_value(false)
                .clamping(egui::SliderClamping::Always),
            150.0,
        )
        .on_hover_text("This device's level in the mix. Double-click to reset.");
        let reset = resp.hovered()
            && ui.input(|inp| {
                inp.pointer
                    .button_double_clicked(egui::PointerButton::Primary)
            });
        let trim = if reset {
            0.0
        } else {
            (trim * 10.0).round() / 10.0
        };
        widgets::knob(
            ui,
            &resp,
            widgets::fraction(trim, TRIM_MIN_DB, TRIM_MAX_DB, false),
        );
        let op = MixerOp::SetSourceTrim {
            input: i.id.clone(),
            source: k,
            trim_db: trim,
        };
        if reset || resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
            edits.push(Edit::Send(op));
        } else if resp.changed() {
            edits.push(Edit::Local(op));
        }
        caption(ui, "Mix balance");
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 20.0), Sense::hover());
            let ok = if dev.is_some() {
                pal().ok
            } else {
                pal().danger
            };
            ui.painter().circle_filled(dot.center(), 3.0, ok);
            let name_w = ui.available_width().max(80.0);
            let (r, _) = ui.allocate_exact_size(vec2(name_w, 20.0), Sense::hover());
            let g = elided(ui, source_name(s), 13.5, pal().text, name_w - 60.0);
            let gw = g.size().x;
            ui.painter().galley(
                pos2(r.left(), r.center().y - g.size().y / 2.0),
                g,
                pal().text,
            );
            ui.painter().text(
                pos2(r.left() + gw + 6.0, r.center().y),
                egui::Align2::LEFT_CENTER,
                format!("· {}", dev.map_or("offline", channels)),
                egui::FontId::proportional(11.5),
                pal().text_faint,
            );
        });
    });
    if let Some(d) = dev.filter(|d| !d.inputs.is_empty() || d.capture) {
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            ui.add_space(8.0 + ui.spacing().item_spacing.x);
            capture_controls(ui, i, k, s, d, edits);
        });
    }
}

/// A device's sound card: the jack it records from and its input gain (the
/// card's capture level), when the card has them. Laid out in the row's
/// direction.
fn capture_controls(
    ui: &mut Ui,
    i: &Input,
    k: usize,
    s: &Source,
    d: &AudioDevice,
    edits: &mut Vec<Edit>,
) {
    #[derive(Clone, Copy)]
    enum Part {
        JackLabel,
        Jack,
        Gap,
        GainLabel,
        Gain,
        GainValue,
    }
    let op = |alsa_input: Option<String>, capture_percent: Option<u8>| MixerOp::SetSourceCapture {
        input: i.id.clone(),
        source: k,
        alsa_input,
        capture_percent,
    };
    let mut parts = Vec::new();
    if !d.inputs.is_empty() {
        parts.extend([Part::JackLabel, Part::Jack]);
    }
    if d.capture {
        if !parts.is_empty() {
            parts.push(Part::Gap);
        }
        parts.extend([Part::GainLabel, Part::Gain, Part::GainValue]);
    }
    if ui.layout().main_dir() == egui::Direction::RightToLeft {
        parts.reverse();
    }
    for part in parts {
        match part {
            Part::JackLabel => caption(ui, "Jack"),
            Part::Jack => {
                let current = s.alsa_input.as_deref().unwrap_or("Leave as is");
                widgets::dropdown(("mixer_jack", k), current, 140.0)
                    .show_ui(ui, |ui| {
                        let choices = std::iter::once(None).chain(d.inputs.iter().map(Some));
                        for item in choices {
                            let text = item.map_or("Leave as is", String::as_str);
                            let picked = s.alsa_input.as_ref() == item;
                            if ui.selectable_label(picked, text).clicked() && !picked {
                                edits.push(Edit::Send(op(item.cloned(), s.capture_percent)));
                            }
                        }
                    })
                    .response
                    .on_hover_text("Which jack of the sound card this device records from");
            }
            Part::Gap => {
                ui.add_space(theme::SPACE_SM);
            }
            Part::GainLabel => caption(ui, "Input gain"),
            Part::Gain => {
                // Unset: the card's own level, so the slider never shows a
                // number the card does not have.
                let mut level = f32::from(s.capture_percent.or(d.capture_now).unwrap_or(0));
                let resp = widgets::slider(
                    ui,
                    egui::Slider::new(&mut level, 0.0..=100.0)
                        .show_value(false)
                        .clamping(egui::SliderClamping::Always),
                    150.0,
                )
                .on_hover_text("The sound card's recording level");
                widgets::knob(ui, &resp, level / 100.0);
                let percent = Some(level.round() as u8);
                if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                    edits.push(Edit::Send(op(s.alsa_input.clone(), percent)));
                } else if resp.changed() {
                    edits.push(Edit::Local(op(s.alsa_input.clone(), percent)));
                }
            }
            Part::GainValue => {
                let value = s
                    .capture_percent
                    .or(d.capture_now)
                    .map_or("—".to_string(), |p| format!("{p}%"));
                value_cell(ui, value, 40.0).on_hover_text(if s.capture_percent.is_some() {
                    "Set on the card while the mixer runs"
                } else {
                    "The card's own level; drag to have hako set it"
                });
            }
        }
    }
}

fn output_drawer(
    ui: &mut Ui,
    drawer: &mut Drawer,
    o: &Output,
    st: &MixerState,
    edits: &mut Vec<Edit>,
) {
    let key = Strip::Output(o.id.clone());
    let put = |f: &dyn Fn(&mut Output)| {
        let mut next = o.clone();
        f(&mut next);
        Edit::Send(MixerOp::PutOutput { output: next })
    };
    let virt = matches!(o.kind, OutputKind::Virtual { .. });
    let kind = if virt {
        "Virtual microphone"
    } else {
        "Hardware output"
    };
    let mut renamed = None;
    widgets::section_with(
        ui,
        |ui| renamed = drawer_header(ui, drawer, &key, &o.label, kind, virt),
        |ui| {
            problem_line(ui, st, &o.id);
            match &o.kind {
                OutputKind::Virtual { node_name } => shown_row(
                    ui,
                    &o.label,
                    node_name,
                    "Pick this as the microphone in Discord, OBS or a game",
                ),
                OutputKind::Hardware { sink, description } => {
                    let current = device(st, sink).map_or_else(
                        || format!("{} (offline)", sink_name(sink, description)),
                        |d| d.description.clone(),
                    );
                    widgets::row(ui, "Device", None, |ui| {
                        widgets::dropdown(("mixer_sink", &o.id), current, theme::DROPDOWN_W)
                            .show_ui(ui, |ui| {
                                for d in sinks(st) {
                                    if ui
                                        .selectable_label(&d.node == sink, &d.description)
                                        .clicked()
                                        && &d.node != sink
                                    {
                                        edits.push(put(&|n| {
                                            n.kind = OutputKind::Hardware {
                                                sink: d.node.clone(),
                                                description: d.description.clone(),
                                            }
                                        }));
                                    }
                                }
                            });
                    });
                }
            }
        },
    );
    if let Some(label) = renamed {
        edits.push(put(&|n| n.label = label.clone()));
    }
    chain_section(
        ui,
        &key,
        &o.chain,
        |chain| MixerOp::SetOutputChain {
            output: o.id.clone(),
            chain,
        },
        edits,
    );
}

/// Capture devices an input can record from (not hako's own nodes: a
/// virtual microphone fed back in would loop).
fn capture_devices(st: &MixerState) -> Vec<&AudioDevice> {
    let created = created_nodes(&st.file.live);
    st.status
        .devices
        .iter()
        .filter(|d| d.direction == DeviceDirection::Source && !created.contains(d.node.as_str()))
        .collect()
}

/// Playback devices an output can use (not hako's own nodes).
fn sinks(st: &MixerState) -> Vec<&AudioDevice> {
    let created = created_nodes(&st.file.live);
    st.status
        .devices
        .iter()
        .filter(|d| d.direction == DeviceDirection::Sink && !created.contains(d.node.as_str()))
        .collect()
}

// ── Add dialogs ──────────────────────────────────────────────────

enum DialogEnd {
    Open,
    Cancel,
    Create(MixerOp, Strip),
}

fn add_dialog(ctx: &egui::Context, d: &mut Dialog, st: &MixerState) -> DialogEnd {
    let scene = &st.file.live;
    let mut end = DialogEnd::Open;
    let modal = egui::Modal::new(egui::Id::new("mixer_add")).show(ctx, |ui| {
        ui.set_width(440.0);
        let (title, labels, virt) = match d {
            Dialog::AddInput { virt, .. } => ("Add input", ["Hardware", "Virtual"], virt),
            Dialog::AddOutput { virt, .. } => {
                ("Add output", ["Hardware", "Virtual microphone"], virt)
            }
        };
        ui.label(
            egui::RichText::new(title)
                .size(16.0)
                .strong()
                .color(pal().text),
        );
        ui.add_space(theme::SPACE_SM);
        if let Some(k) = widgets::segmented(
            ui,
            egui::Id::new("mixer_add_kind"),
            &labels,
            Some(usize::from(*virt)),
        ) {
            *virt = k == 1;
            // Devices and apps share the list of what was ticked.
            if let Dialog::AddInput { picked, .. } = d {
                picked.clear();
            }
        }
        ui.add_space(theme::SPACE_SM);
        let mut create = None;
        match d {
            Dialog::AddInput { virt, name, picked } => {
                widgets::hint(
                    ui,
                    if *virt {
                        "A new playback device for apps: send a game, a call or music here."
                    } else {
                        "Pick one or more capture devices; they are merged into one strip."
                    },
                );
                ui.add_space(theme::SPACE_SM);
                widgets::text_field(ui, name, "Name", 440.0);
                ui.add_space(theme::SPACE_SM);
                if !*virt {
                    let devices = capture_devices(st);
                    if devices.is_empty() {
                        widgets::hint(ui, "No capture devices found.");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for dev in devices {
                                let mut on = picked.contains(&dev.node);
                                let free = on || picked.len() < MAX_SOURCES;
                                let text = format!("{} · {}", dev.description, channels(dev));
                                if ui
                                    .add_enabled(free, egui::Checkbox::new(&mut on, text))
                                    .changed()
                                {
                                    if on {
                                        picked.push(dev.node.clone());
                                        if name.trim().is_empty() {
                                            *name = fit_label(&dev.description);
                                        }
                                    } else {
                                        picked.retain(|n| n != &dev.node);
                                    }
                                }
                            }
                        });
                } else {
                    let apps = free_apps(st, "");
                    if !apps.is_empty() {
                        caption(ui, "Take the sound of these apps (optional)");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for app in apps {
                                let mut on = picked.iter().any(|p| p == app);
                                let free = on || picked.len() < MAX_SOURCES;
                                if ui
                                    .add_enabled(free, egui::Checkbox::new(&mut on, app))
                                    .changed()
                                {
                                    if on {
                                        picked.push(app.to_string());
                                        if name.trim().is_empty() {
                                            *name = fit_label(app);
                                        }
                                    } else {
                                        picked.retain(|p| p != app);
                                    }
                                }
                            }
                        });
                    if valid_label(name) {
                        widgets::hint(ui, &format!("Node name: {}", new_node_name(name, scene)));
                    }
                }
                let ok = valid_label(name) && (*virt || !picked.is_empty());
                if ok {
                    let label = name.trim().to_string();
                    let kind = if *virt {
                        InputKind::Virtual {
                            node_name: new_node_name(&label, scene),
                            apps: picked.clone(),
                        }
                    } else {
                        InputKind::Hardware {
                            sources: st
                                .status
                                .devices
                                .iter()
                                .filter(|d| picked.contains(&d.node))
                                .map(new_source)
                                .collect(),
                        }
                    };
                    let id = new_input_id(&label, &scene.inputs);
                    create = Some((
                        MixerOp::PutInput {
                            input: Input {
                                id: id.clone(),
                                label,
                                kind,
                                chain: EffectChain::default(),
                                gain_db: 0.0,
                                mute: false,
                                solo: false,
                                routes: Vec::new(),
                                chatmix: None,
                            },
                        },
                        Strip::Input(id),
                    ));
                }
            }
            Dialog::AddOutput { virt, name, sink } => {
                let prefix = if *virt { 'B' } else { 'A' };
                let id = next_output_id(&scene.outputs, prefix);
                widgets::hint(
                    ui,
                    &match (&id, *virt) {
                        (None, _) => format!("All {MAX_OUTPUTS} outputs are in use."),
                        (Some(id), true) => {
                            format!("{id}: a microphone Discord, OBS or a game can record from.")
                        }
                        (Some(id), false) => format!("{id}: plays on a speaker or headset."),
                    },
                );
                ui.add_space(theme::SPACE_SM);
                widgets::text_field(ui, name, "Name", 440.0);
                ui.add_space(theme::SPACE_SM);
                if !*virt {
                    let all = sinks(st);
                    let current = sink
                        .as_ref()
                        .and_then(|s| all.iter().find(|d| &d.node == s))
                        .map_or("Pick a device".to_string(), |d| d.description.clone());
                    widgets::dropdown("mixer_add_sink", current, 440.0).show_ui(ui, |ui| {
                        for dev in &all {
                            if ui
                                .selectable_label(
                                    sink.as_ref() == Some(&dev.node),
                                    &dev.description,
                                )
                                .clicked()
                            {
                                *sink = Some(dev.node.clone());
                                if name.trim().is_empty() {
                                    *name = fit_label(&dev.description);
                                }
                            }
                        }
                    });
                } else if valid_label(name) {
                    widgets::hint(ui, &format!("Node name: {}", new_node_name(name, scene)));
                }
                if let Some(id) = id.filter(|_| valid_label(name) && (*virt || sink.is_some())) {
                    let label = name.trim().to_string();
                    let kind = match sink {
                        Some(s) if !*virt => OutputKind::Hardware {
                            sink: s.clone(),
                            description: device(st, s)
                                .map_or_else(String::new, |d| d.description.clone()),
                        },
                        _ => OutputKind::Virtual {
                            node_name: new_node_name(&label, scene),
                        },
                    };
                    create = Some((
                        MixerOp::PutOutput {
                            output: Output {
                                id: id.clone(),
                                label,
                                kind,
                                chain: EffectChain::default(),
                                gain_db: 0.0,
                                mute: false,
                                mono: false,
                            },
                        },
                        Strip::Output(id),
                    ));
                }
            }
        }
        ui.add_space(theme::SPACE_LG);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let add = ui
                .add_enabled_ui(create.is_some(), |ui| widgets::primary_button(ui, title))
                .inner;
            if add.clicked() {
                if let Some((op, strip)) = create.take() {
                    end = DialogEnd::Create(op, strip);
                }
            }
            if widgets::button(ui, "Cancel").clicked() {
                end = DialogEnd::Cancel;
            }
        });
    });
    if matches!(end, DialogEnd::Open) && modal.should_close() {
        end = DialogEnd::Cancel;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{fader_db, fader_pos, snap_gain, FADER_MAX_DB, FADER_MIN_DB};

    #[test]
    fn fader_taper_round_trips_and_favours_zero() {
        assert_eq!(fader_pos(FADER_MIN_DB), 0.0);
        assert_eq!(fader_pos(FADER_MAX_DB), 1.0);
        assert_eq!(fader_pos(-100.0), 0.0, "clamped below");
        assert_eq!(fader_pos(40.0), 1.0, "clamped above");
        assert_eq!(fader_db(-0.5), FADER_MIN_DB);
        assert_eq!(fader_db(2.0), FADER_MAX_DB);
        let mut last = -1.0;
        for tenth in -600..=120 {
            let db = tenth as f32 / 10.0;
            let pos = fader_pos(db);
            assert!(pos > last, "monotonic at {db}");
            last = pos;
            assert!((fader_db(pos) - db).abs() < 1e-3, "round trip at {db}");
        }
        // More travel per dB around 0 than at the bottom of the range.
        let near_zero = fader_pos(0.0) - fader_pos(-6.0);
        let bottom = fader_pos(-54.0) - fader_pos(-60.0);
        assert!(near_zero > 2.0 * bottom);
    }

    #[test]
    fn dragged_gains_snap_to_the_detent_and_stay_in_range() {
        assert_eq!(snap_gain(0.6), 0.0);
        assert_eq!(snap_gain(-0.7), 0.0);
        assert_eq!(snap_gain(0.8), 0.8);
        assert_eq!(snap_gain(-3.04), -3.0);
        assert_eq!(snap_gain(-61.0), FADER_MIN_DB);
        assert_eq!(snap_gain(13.0), FADER_MAX_DB);
    }

    #[test]
    fn live_gains_coalesce_and_throttle() {
        let a = Strip::Input("voice".into());
        let b = Strip::Output("A1".into());
        let mut live = LiveGains::default();
        assert_eq!(live.flush(0.0, false), None);
        live.set(a.clone(), -3.0, false);
        live.set(a.clone(), -4.0, false);
        live.set(b.clone(), 2.0, false);
        // Only the newest value per strip goes out.
        assert_eq!(
            live.flush(1.0, false).unwrap(),
            [a.gain_op(-4.0), b.gain_op(2.0)]
        );
        // Within 1/30 s of the last send: held, then sent.
        live.set(a.clone(), -5.0, false);
        assert_eq!(live.flush(1.01, false), None);
        assert_eq!(live.flush(1.04, false).unwrap(), [a.gain_op(-5.0)]);
        // While an edit is in flight nothing queues; values just get replaced.
        for k in 0..100 {
            live.set(a.clone(), -k as f32 / 10.0, false);
            assert_eq!(live.flush(2.0 + k as f64, true), None);
        }
        assert_eq!(live.flush(200.0, false).unwrap(), [a.gain_op(-9.9)]);
        // Release goes out at once, busy or not.
        live.set(a.clone(), -1.0, false);
        live.set(a.clone(), 0.0, true);
        assert_eq!(live.flush(200.001, true).unwrap(), [a.gain_op(0.0)]);
        assert!(live.is_idle());
    }

    /// Edits land in the local copy at once; finished ones go out as one
    /// patch, fader moves through `LiveGains`, and a refused op changes
    /// nothing, sends nothing and says why.
    #[test]
    fn edits_apply_locally_and_send_ops() {
        let mut app = HakoAppUi::default();
        let mut st = MixerState::default();
        st.file.enabled = true;
        st.file.live.inputs.push(virt_input("game", "Hako_Game"));
        app.mixer.receive(st);
        app.mixer.sync(false);
        let output = Output {
            id: "A1".into(),
            label: "Headset".into(),
            kind: OutputKind::Hardware {
                sink: "alsa_output.x".into(),
                description: String::new(),
            },
            chain: EffectChain::default(),
            gain_db: 0.0,
            mute: false,
            mono: false,
        };
        let route = MixerOp::SetRoute {
            input: "game".into(),
            output: "A1".into(),
            on: true,
        };
        let game = Strip::Input("game".into());
        let mut out = Vec::new();
        app.apply_edits(
            vec![
                Edit::Send(MixerOp::PutOutput { output }),
                Edit::Send(route.clone()),
                Edit::Gain(game.clone(), -6.0, false),
                Edit::Send(MixerOp::SetInputGain {
                    input: "game".into(),
                    gain_db: 40.0,
                }),
            ],
            &mut out,
        );
        let live = &app.mixer.state().unwrap().file.live;
        assert_eq!(live.inputs[0].routes, ["A1"]);
        assert_eq!(live.inputs[0].gain_db, -6.0);
        match out.as_slice() {
            [UiCommand::PatchMixer(ops)] => {
                assert_eq!(ops.len(), 2);
                assert_eq!(ops[1], route);
            }
            other => panic!("{other:?}"),
        }
        assert!(app.toasts[0].text.contains("gain"), "{:?}", app.toasts);
        assert_eq!(app.mixer.flush(0.0).unwrap(), [game.gain_op(-6.0)]);
        // A daemon answer waits until the fader values are out.
        app.mixer.live.set(game.clone(), -7.0, false);
        app.mixer.receive(MixerState::default());
        app.mixer.sync(false);
        assert_eq!(app.mixer.state().unwrap().file.live.inputs.len(), 1);
    }

    fn virt_input(id: &str, node: &str) -> Input {
        Input {
            id: id.into(),
            label: id.into(),
            kind: InputKind::Virtual {
                node_name: node.into(),
                apps: Vec::new(),
            },
            chain: EffectChain::default(),
            gain_db: 0.0,
            mute: false,
            solo: false,
            routes: Vec::new(),
            chatmix: None,
        }
    }

    fn hw_input(id: &str, nodes: &[&str]) -> Input {
        Input {
            kind: InputKind::Hardware {
                sources: nodes.iter().map(|n| new_source(&mic(n))).collect(),
            },
            ..virt_input(id, "unused")
        }
    }

    fn mic(node: &str) -> AudioDevice {
        AudioDevice {
            node: node.into(),
            description: format!("{node} mic"),
            direction: DeviceDirection::Source,
            channels: 1,
            inputs: Vec::new(),
            capture: false,
            capture_now: None,
        }
    }

    fn sources(i: &Input) -> &[Source] {
        match &i.kind {
            InputKind::Hardware { sources } => sources,
            InputKind::Virtual { .. } => &[],
        }
    }

    fn put(op: MixerOp) -> Input {
        match op {
            MixerOp::PutInput { input } => input,
            other => panic!("{other:?}"),
        }
    }

    /// An app another input takes is not offered; ticking one adds or drops it.
    #[test]
    fn apps_are_offered_to_one_input_and_toggle() {
        let mut st = MixerState::default();
        st.status.apps = vec!["Brave".into(), "Spotify".into()];
        let music = put(set_app(&virt_input("music", "Hako_Music"), "Spotify", true));
        st.file.live.inputs = vec![music.clone(), virt_input("web", "Hako_Web")];
        assert_eq!(free_apps(&st, "web"), ["Brave"]);
        assert_eq!(free_apps(&st, "music"), ["Brave", "Spotify"]);
        let both = put(set_app(&music, "Brave", true));
        assert!(
            matches!(&both.kind, InputKind::Virtual { apps, .. } if apps == &["Spotify", "Brave"])
        );
        let one = put(set_app(&both, "Spotify", false));
        assert!(matches!(&one.kind, InputKind::Virtual { apps, .. } if apps == &["Brave"]));
    }

    /// One device: a "Device" row and nothing to balance. Two or more: the
    /// "Devices" list. The dial row is for virtual inputs and linked ones.
    #[test]
    fn the_drawer_shows_one_device_or_a_list_and_the_dial_where_it_applies() {
        let game = virt_input("game", "Hako_Game");
        let empty = hw_input("empty", &[]);
        let one = hw_input("one", &["a"]);
        let two = hw_input("two", &["a", "b"]);
        assert_eq!(device_rows(&game), DeviceRows::Virtual);
        assert_eq!(device_rows(&empty), DeviceRows::Single);
        assert_eq!(device_rows(&one), DeviceRows::Single);
        assert_eq!(device_rows(&two), DeviceRows::Merged);
        assert!(shows_dial(&game));
        assert!(!shows_dial(&one) && !shows_dial(&two));
        let linked = Input {
            chatmix: Some(ChatMixSide::Chat),
            ..one
        };
        assert!(shows_dial(&linked), "a linked input can be unlinked");
    }

    #[test]
    fn picking_another_device_replaces_it_and_keeps_the_strip() {
        let mut line = hw_input("line", &["a"]);
        line.label = "Line-in".into();
        line.gain_db = -6.0;
        line.routes = vec!["A1".into()];
        if let InputKind::Hardware { sources } = &mut line.kind {
            sources[0].alsa_input = Some("Line".into());
            sources[0].capture_percent = Some(66);
        }
        let next = put(set_device(&line, &mic("b")));
        // The other card's jack and gain are not this one's.
        assert_eq!(sources(&next), [new_source(&mic("b"))]);
        let same = Input {
            kind: line.kind.clone(),
            ..next
        };
        assert_eq!(same, line, "name, fader, routes and chain stay");
        // An input without a device gets its first one the same way.
        let picked = put(set_device(&hw_input("empty", &[]), &mic("a")));
        assert_eq!(sources(&picked).len(), 1);
    }

    /// Merging adds a device with its own balance; taking one of two out
    /// again leaves a single device, whose balance the model folds into the
    /// fader.
    #[test]
    fn merging_and_unmerging_devices() {
        let mut file = hako_core::mixer::MixerFile::default();
        let mut apply = |op: MixerOp| {
            file.apply(&op).unwrap();
            file.live.inputs[0].clone()
        };
        let voice = apply(MixerOp::PutInput {
            input: hw_input("voice", &["a"]),
        });
        let voice = apply(merge_device(&voice, &mic("b")));
        assert_eq!(device_rows(&voice), DeviceRows::Merged);
        assert_eq!(sources(&voice)[1].node, "b");
        let voice = apply(MixerOp::SetSourceTrim {
            input: "voice".into(),
            source: 1,
            trim_db: -3.0,
        });
        assert_eq!((voice.gain_db, sources(&voice)[1].trim_db), (0.0, -3.0));
        let voice = apply(unmerge_device(&voice, 0));
        assert_eq!(device_rows(&voice), DeviceRows::Single);
        assert_eq!(sources(&voice)[0].node, "b");
        assert_eq!((voice.gain_db, sources(&voice)[0].trim_db), (-3.0, 0.0));
        // Out of range: nothing to remove.
        assert_eq!(put(unmerge_device(&voice, 4)), voice);
    }

    #[test]
    fn a_rename_commits_on_enter_or_blur_and_cancels_on_escape() {
        use RenameEnd::*;
        assert_eq!(rename_end("Voice", "Desk", false, false), Open);
        assert_eq!(
            rename_end("Voice", " Desk ", true, false),
            Commit("Desk".into())
        );
        assert_eq!(rename_end("Voice", "Desk", true, true), Cancel);
        // Unchanged or empty: nothing to send.
        assert_eq!(rename_end("Voice", "Voice ", true, false), Cancel);
        assert_eq!(rename_end("Voice", "  ", true, false), Cancel);
        // Too long is cut to what a label holds.
        let Commit(long) = rename_end("Voice", &"x".repeat(80), true, false) else {
            panic!("a long name is cut, not refused");
        };
        assert_eq!(long.chars().count(), MAX_LABEL);
    }

    #[test]
    fn virtual_strips_are_shown_to_apps_as_hako_once() {
        assert_eq!(shown_as("Game"), "hako Game");
        assert_eq!(shown_as("hako Mic"), "hako Mic");
        assert_eq!(shown_as("Hako Stream"), "Hako Stream");
        assert_eq!(shown_as("Ünï"), "hako Ünï");
    }

    #[test]
    fn ids_and_node_names_are_valid_and_unique() {
        let scene = Scene {
            inputs: vec![
                virt_input("game", "Hako_Game"),
                virt_input("mic", "Hako_Mic"),
            ],
            outputs: vec![Output {
                id: "B1".into(),
                label: "hako Stream".into(),
                kind: OutputKind::Virtual {
                    node_name: "Hako_Stream".into(),
                },
                chain: EffectChain::default(),
                gain_db: 0.0,
                mute: false,
                mono: false,
            }],
        };
        assert_eq!(new_input_id("Desk Mic", &scene.inputs), "desk-mic");
        assert_eq!(new_input_id("Game", &scene.inputs), "game-2");
        assert_eq!(new_input_id("Ünïcode ☺", &scene.inputs), "n-code");
        assert_eq!(new_input_id("☺", &scene.inputs), "input");
        let long = new_input_id(&"x".repeat(80), &scene.inputs);
        assert!(long.len() <= MAX_LABEL);

        assert_eq!(new_node_name("Music", &scene), "Hako_Music");
        assert_eq!(new_node_name("hako Mic", &scene), "Hako_Mic_2");
        assert_eq!(new_node_name("hako Stream", &scene), "Hako_Stream_2");
        assert_eq!(
            new_node_name("My \"quoted\" mic", &scene),
            "Hako_My_quoted_mic"
        );
        assert_eq!(new_node_name("…", &scene), "Hako_Mixer");

        assert_eq!(next_output_id(&scene.outputs, 'A').as_deref(), Some("A1"));
        assert_eq!(next_output_id(&scene.outputs, 'B').as_deref(), Some("B2"));

        // Ids run out where the model's output limit is, not at A8 + B8.
        let mut file = hako_core::mixer::MixerFile::default();
        let mut virt = false;
        while let Some(id) = next_output_id(&file.live.outputs, if virt { 'B' } else { 'A' }) {
            let kind = if virt {
                OutputKind::Virtual {
                    node_name: format!("Hako_{id}"),
                }
            } else {
                OutputKind::Hardware {
                    sink: format!("alsa_output.{id}"),
                    description: String::new(),
                }
            };
            let output = Output {
                id: id.clone(),
                label: id,
                kind,
                chain: EffectChain::default(),
                gain_db: 0.0,
                mute: false,
                mono: false,
            };
            file.apply(&MixerOp::PutOutput { output }).unwrap();
            virt = !virt;
        }
        assert_eq!(file.live.outputs.len(), MAX_OUTPUTS);
        assert_eq!(next_output_id(&file.live.outputs, 'B'), None);

        // Whatever these produce passes the model's own validation.
        let mut file = hako_core::mixer::MixerFile::default();
        for label in ["Desk Mic", "Game", "Ünïcode ☺", "☺", "hako Mic"] {
            let input = virt_input(
                &new_input_id(label, &file.live.inputs),
                &new_node_name(label, &file.live),
            );
            file.apply(&MixerOp::PutInput { input }).unwrap();
        }
    }
}
