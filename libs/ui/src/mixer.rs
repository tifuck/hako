//! Mixer page (concept A, "Console"): input strips, output masters, and a
//! drawer for the selected strip. Edits go out as `MixerOp`s and are applied
//! to the local copy first (`MixerFile::apply`, as the daemon does), so the
//! page never waits on an answer; the daemon's state is adopted once nothing
//! is in flight.

use crate::theme::{self, pal};
use crate::widgets::{self, Tone};
use crate::{aux, menu_heading, HakoAppUi, UiCommand};
use egui::{pos2, vec2, Color32, Rect, Sense, Stroke, Ui};
use hako_core::ipc::{AuxEffectKind, MeterTargetSpec};
use hako_core::mixer::{
    AudioDevice, ChatMixSide, DeviceDirection, EffectChain, Input, InputKind, MixerOp, MixerState,
    Output, OutputKind, Scene, Source, MAX_INPUTS, MAX_LABEL, MAX_SOURCES, TRIM_MAX_DB,
    TRIM_MIN_DB,
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
        InputKind::Virtual { node_name } => Some(node_name.as_str()),
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

/// The first free output id with `prefix` (`A` hardware, `B` virtual).
pub fn next_output_id(outputs: &[Output], prefix: char) -> Option<String> {
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
    scene_draft: String,
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
/// Scene menu width (wide enough for the save field inside).
const SCENE_MENU_W: f32 = 232.0;

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
                    "Turning it on takes over ChatMix and line-in from the headset page, with a starting layout that keeps them working as they do now.",
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
            match selected {
                Some(Strip::Input(id)) => {
                    if let Some(i) = scene.inputs.iter().find(|i| i.id == id) {
                        input_drawer(ui, i, &st, &mut edits);
                    }
                }
                Some(Strip::Output(id)) => {
                    if let Some(o) = scene.outputs.iter().find(|o| o.id == id) {
                        output_drawer(ui, o, &st, &mut edits);
                    }
                }
                None => {}
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
                    .on_hover_text("Hands ChatMix and line-in back to the headset page");
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
                self.scene_menu(ui, st, edits);
            });
        });
    }

    /// Scenes: apply or delete a saved layout, or save the current one.
    fn scene_menu(&mut self, ui: &mut Ui, st: &MixerState, edits: &mut Vec<Edit>) {
        let popup_id = egui::Id::new("mixer_scene_popup");
        let open = ui.memory(|m| m.is_popup_open(popup_id));
        let resp = widgets::dropdown_button(ui, "Scenes", 120.0, open);
        if resp.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup_id));
        }
        let draft = &mut self.mixer.scene_draft;
        egui::popup::popup_below_widget(
            ui,
            popup_id,
            &resp,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                let inner = SCENE_MENU_W - ui.spacing().menu_margin.sum().x;
                ui.set_width(inner);
                menu_heading(ui, "Scenes");
                if st.file.scenes.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        widgets::hint(ui, "No saved scenes yet");
                    });
                }
                for s in &st.file.scenes {
                    ui.horizontal(|ui| {
                        let apply = ui
                            .add_sized(
                                vec2(inner - 32.0, 26.0),
                                egui::Button::new(&s.name).frame(false),
                            )
                            .on_hover_text("Make this the live layout");
                        if apply.clicked() {
                            edits.push(Edit::Send(MixerOp::ApplyScene {
                                name: s.name.clone(),
                            }));
                            ui.memory_mut(|m| m.close_popup());
                        }
                        if widgets::icon_button(ui, 24.0, "Delete scene", true, widgets::icon_close)
                            .clicked()
                        {
                            edits.push(Edit::Send(MixerOp::DeleteScene {
                                name: s.name.clone(),
                            }));
                        }
                    });
                }
                ui.separator();
                menu_heading(ui, "Save current layout");
                ui.horizontal(|ui| {
                    let save_w = 64.0;
                    let field_w = inner - save_w - ui.spacing().item_spacing.x;
                    let edit = widgets::text_field(ui, draft, "Scene name", field_w);
                    let valid = valid_label(draft);
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let save = ui
                        .add_enabled(
                            valid,
                            egui::Button::new("Save").min_size(vec2(save_w, theme::CONTROL_H)),
                        )
                        .clicked();
                    if valid && (save || enter) {
                        edits.push(Edit::Send(MixerOp::SaveScene {
                            name: draft.trim().to_string(),
                        }));
                        draft.clear();
                        ui.memory_mut(|m| m.close_popup());
                    }
                });
            },
        );
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

fn input_look<'a>(i: &'a Input, st: &MixerState, levels: [f32; 2]) -> StripLook<'a> {
    let (summary, virt) = match &i.kind {
        InputKind::Hardware { sources } => (
            match sources.as_slice() {
                [] => "No source".to_string(),
                [s] => source_name(s).to_string(),
                more => format!("{} sources merged", more.len()),
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
        virt,
        problem: problems(st, &i.id),
        levels,
    }
}

fn output_look<'a>(o: &'a Output, st: &MixerState, levels: [f32; 2]) -> StripLook<'a> {
    let (summary, virt) = match &o.kind {
        OutputKind::Hardware { sink } => (
            device(st, sink).map_or_else(|| sink.clone(), |d| d.description.clone()),
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
        virt,
        problem: problems(st, &o.id),
        levels,
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
    let fill = if s.mute { pal().text_faint } else { tint };
    let f = widgets::vertical_fader(ui, fader, id.with("fader"), &mut db, fill);
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
    if chip_button(
        ui,
        first,
        id.with("mute"),
        "M",
        s.mute,
        Lit::Tint(pal().danger),
    )
    .on_hover_text(if s.mute { "Unmute" } else { "Mute" })
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

/// Name field that commits on Enter or when it loses focus.
fn label_field(ui: &mut Ui, key: &Strip, label: &str, width: f32) -> Option<String> {
    let id = egui::Id::new(("mixer_label", key));
    let mut draft = ui
        .data(|m| m.get_temp::<String>(id))
        .unwrap_or_else(|| label.to_string());
    let resp = widgets::text_field(ui, &mut draft, "Name", width);
    if resp.has_focus() {
        ui.data_mut(|m| m.insert_temp(id, draft));
        return None;
    }
    ui.data_mut(|m| m.remove_temp::<String>(id));
    (resp.lost_focus() && valid_label(&draft) && draft.trim() != label)
        .then(|| draft.trim().to_string())
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

fn input_drawer(ui: &mut Ui, i: &Input, st: &MixerState, edits: &mut Vec<Edit>) {
    let key = Strip::Input(i.id.clone());
    let put = |f: &dyn Fn(&mut Input)| {
        let mut next = i.clone();
        f(&mut next);
        Edit::Send(MixerOp::PutInput { input: next })
    };
    let virt = matches!(i.kind, InputKind::Virtual { .. });
    widgets::section(
        ui,
        &i.label,
        |ui| {
            kind_pill(
                ui,
                if virt {
                    "Virtual input"
                } else {
                    "Hardware input"
                },
                virt,
            )
        },
        |ui| {
            problem_line(ui, st, &i.id);
            let hint = match &i.kind {
                InputKind::Virtual { node_name } => {
                    format!("Apps that play to “{}” land here ({node_name})", i.label)
                }
                InputKind::Hardware { .. } => {
                    "Sources are summed after their own trim, then processed once".into()
                }
            };
            widgets::row(ui, "Name", Some(&hint), |ui| {
                if let Some(label) = label_field(ui, &key, &i.label, theme::DROPDOWN_W) {
                    edits.push(put(&|n| n.label = label.clone()));
                }
            });
            if let InputKind::Hardware { sources } = &i.kind {
                widgets::divider(ui);
                ui.add_space(theme::SPACE_SM);
                for (k, s) in sources.iter().enumerate() {
                    ui.push_id(k, |ui| source_row(ui, i, k, s, st, edits));
                }
                if sources.is_empty() {
                    widgets::hint(ui, "No sources: add a capture device.");
                }
                ui.add_space(theme::SPACE_XS);
                let created = created_nodes(&st.file.live);
                let free: Vec<&AudioDevice> = st
                    .status
                    .devices
                    .iter()
                    .filter(|d| {
                        d.direction == DeviceDirection::Source
                            && !created.contains(d.node.as_str())
                            && !sources.iter().any(|s| s.node == d.node)
                    })
                    .collect();
                ui.add_enabled_ui(sources.len() < MAX_SOURCES, |ui| {
                    ui.menu_button("+  Add source", |ui| {
                        if free.is_empty() {
                            widgets::hint(ui, "No other capture devices found");
                        }
                        for d in &free {
                            if ui.button(&d.description).clicked() {
                                edits.push(put(&|n| {
                                    if let InputKind::Hardware { sources } = &mut n.kind {
                                        sources.push(Source {
                                            node: d.node.clone(),
                                            description: d.description.clone(),
                                            trim_db: 0.0,
                                            mute: false,
                                        });
                                    }
                                }));
                                ui.close_menu();
                            }
                        }
                    });
                });
                ui.add_space(theme::SPACE_SM);
            }
            widgets::divider(ui);
            widgets::row(
                ui,
                "ChatMix",
                Some("The headset's dial turns this input up or down"),
                |ui| {
                    let current = match i.chatmix {
                        None => 0,
                        Some(ChatMixSide::Game) => 1,
                        Some(ChatMixSide::Chat) => 2,
                    };
                    let seg_id = egui::Id::new(("mixer_chatmix", &i.id));
                    if let Some(k) =
                        widgets::segmented(ui, seg_id, &["None", "Game", "Chat"], Some(current))
                    {
                        let side = [None, Some(ChatMixSide::Game), Some(ChatMixSide::Chat)][k];
                        edits.push(put(&|n| n.chatmix = side));
                    }
                },
            );
            widgets::divider(ui);
            widgets::row(ui, "Remove input", None, |ui| {
                if widgets::button(ui, "Remove").clicked() {
                    edits.push(Edit::Send(MixerOp::RemoveInput {
                        input: i.id.clone(),
                    }));
                }
            });
        },
    );
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

/// One source of a merged input: name, channels, trim, mute, remove.
fn source_row(
    ui: &mut Ui,
    i: &Input,
    k: usize,
    s: &Source,
    st: &MixerState,
    edits: &mut Vec<Edit>,
) {
    let dev = device(st, &s.node);
    let trim_w = 150.0;
    let value_w = 64.0;
    ui.horizontal(|ui| {
        ui.set_min_height(36.0);
        let (dot, _) = ui.allocate_exact_size(vec2(8.0, 20.0), Sense::hover());
        ui.painter().circle_filled(
            dot.center(),
            3.0,
            if dev.is_some() {
                pal().ok
            } else {
                pal().danger
            },
        );
        let fixed = trim_w + value_w + 30.0 + 26.0 + 5.0 * ui.spacing().item_spacing.x;
        let name_w = (ui.available_width() - fixed).max(80.0);
        let channels = match dev {
            None => "offline",
            Some(d) if d.channels == 1 => "mono",
            Some(_) => "stereo",
        };
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
            format!("· {channels}"),
            egui::FontId::proportional(11.5),
            pal().text_faint,
        );
        let mut trim = s.trim_db;
        let resp = widgets::slider(
            ui,
            egui::Slider::new(&mut trim, TRIM_MIN_DB..=TRIM_MAX_DB)
                .show_value(false)
                .clamping(egui::SliderClamping::Always),
            trim_w,
        )
        .on_hover_text("Trim: this source's level before the sources are summed");
        widgets::knob(
            ui,
            &resp,
            widgets::fraction(trim, TRIM_MIN_DB, TRIM_MAX_DB, false),
        );
        let trim = (trim * 10.0).round() / 10.0;
        let op = MixerOp::SetSourceTrim {
            input: i.id.clone(),
            source: k,
            trim_db: trim,
        };
        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
            edits.push(Edit::Send(op));
        } else if resp.changed() {
            edits.push(Edit::Local(op));
        }
        ui.allocate_ui_with_layout(
            vec2(value_w, 20.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.set_min_width(value_w);
                ui.label(widgets::value_text(fmt_db(trim)));
            },
        );
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
            "Unmute source"
        } else {
            "Mute source"
        })
        .clicked()
        {
            edits.push(Edit::Send(MixerOp::SetSourceMute {
                input: i.id.clone(),
                source: k,
                mute: !s.mute,
            }));
        }
        if widgets::icon_button(ui, 26.0, "Remove source", true, widgets::icon_close).clicked() {
            let mut next = i.clone();
            if let InputKind::Hardware { sources } = &mut next.kind {
                sources.remove(k);
            }
            edits.push(Edit::Send(MixerOp::PutInput { input: next }));
        }
    });
}

fn output_drawer(ui: &mut Ui, o: &Output, st: &MixerState, edits: &mut Vec<Edit>) {
    let key = Strip::Output(o.id.clone());
    let put = |f: &dyn Fn(&mut Output)| {
        let mut next = o.clone();
        f(&mut next);
        Edit::Send(MixerOp::PutOutput { output: next })
    };
    let virt = matches!(o.kind, OutputKind::Virtual { .. });
    widgets::section(
        ui,
        &format!("{} · {}", o.id, o.label),
        |ui| {
            kind_pill(
                ui,
                if virt {
                    "Virtual microphone"
                } else {
                    "Hardware output"
                },
                virt,
            )
        },
        |ui| {
            problem_line(ui, st, &o.id);
            let hint = match &o.kind {
                OutputKind::Virtual { node_name } => Some(format!(
                    "Other apps record from “{}” ({node_name})",
                    o.label
                )),
                OutputKind::Hardware { .. } => None,
            };
            widgets::row(ui, "Name", hint.as_deref(), |ui| {
                if let Some(label) = label_field(ui, &key, &o.label, theme::DROPDOWN_W) {
                    edits.push(put(&|n| n.label = label.clone()));
                }
            });
            if let OutputKind::Hardware { sink } = &o.kind {
                widgets::divider(ui);
                let current = device(st, sink)
                    .map_or_else(|| format!("{sink} (offline)"), |d| d.description.clone());
                widgets::row(ui, "Device", None, |ui| {
                    widgets::dropdown(("mixer_sink", &o.id), current, theme::DROPDOWN_W).show_ui(
                        ui,
                        |ui| {
                            for d in sinks(st) {
                                if ui
                                    .selectable_label(&d.node == sink, &d.description)
                                    .clicked()
                                    && &d.node != sink
                                {
                                    edits.push(put(&|n| {
                                        n.kind = OutputKind::Hardware {
                                            sink: d.node.clone(),
                                        }
                                    }));
                                }
                            }
                        },
                    );
                });
            }
            widgets::divider(ui);
            widgets::row(
                ui,
                "Mono",
                Some("Fold left and right into both channels"),
                |ui| {
                    let mut mono = o.mono;
                    if widgets::switch(ui, &mut mono).changed() {
                        edits.push(Edit::Send(MixerOp::SetOutputMono {
                            output: o.id.clone(),
                            mono,
                        }));
                    }
                },
            );
            widgets::divider(ui);
            widgets::row(
                ui,
                "Remove output",
                Some("Inputs stop sending to it"),
                |ui| {
                    if widgets::button(ui, "Remove").clicked() {
                        edits.push(Edit::Send(MixerOp::RemoveOutput {
                            output: o.id.clone(),
                        }));
                    }
                },
            );
        },
    );
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
                let created = created_nodes(scene);
                if !*virt {
                    let devices: Vec<&AudioDevice> = st
                        .status
                        .devices
                        .iter()
                        .filter(|d| {
                            d.direction == DeviceDirection::Source
                                && !created.contains(d.node.as_str())
                        })
                        .collect();
                    if devices.is_empty() {
                        widgets::hint(ui, "No capture devices found.");
                    }
                    egui::ScrollArea::vertical()
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for dev in devices {
                                let mut on = picked.contains(&dev.node);
                                let text = format!(
                                    "{} · {}",
                                    dev.description,
                                    if dev.channels == 1 { "mono" } else { "stereo" }
                                );
                                if ui.checkbox(&mut on, text).changed() {
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
                } else if valid_label(name) {
                    widgets::hint(ui, &format!("Node name: {}", new_node_name(name, scene)));
                }
                let ok = valid_label(name) && (*virt || !picked.is_empty());
                if ok {
                    let label = name.trim().to_string();
                    let kind = if *virt {
                        InputKind::Virtual {
                            node_name: new_node_name(&label, scene),
                        }
                    } else {
                        InputKind::Hardware {
                            sources: st
                                .status
                                .devices
                                .iter()
                                .filter(|d| picked.contains(&d.node))
                                .map(|d| Source {
                                    node: d.node.clone(),
                                    description: d.description.clone(),
                                    trim_db: 0.0,
                                    mute: false,
                                })
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
                        (None, _) => format!("All eight {prefix} outputs are in use."),
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
                        Some(s) if !*virt => OutputKind::Hardware { sink: s.clone() },
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
            },
            chain: EffectChain::default(),
            gain_db: 0.0,
            mute: false,
            solo: false,
            routes: Vec::new(),
            chatmix: None,
        }
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
