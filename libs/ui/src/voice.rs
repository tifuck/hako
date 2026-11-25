//! Voice page: dictation recognised on this computer. Off: one card and
//! Turn on. On: a header with the status and the live transcript, then
//! Model (the whole catalog), Hotkeys, Microphone, Output, Language and
//! Streaming, the rest of dictation as folded sections, then Jarvis (wake
//! word, push-to-command, the user's commands, what it understands).
//! Each edit is its section's `VoiceOp`, checked with `VoiceFile::validate`
//! and applied to the local copy until the daemon answers.

use crate::hotkeys;
use crate::theme::{self, pal};
use crate::widgets;
use crate::{HakoAppUi, UiCommand};
use egui::{pos2, vec2, Color32, Rect, Stroke, Ui};
use hako_core::device::{DeviceId, DeviceKind, DeviceSnapshot};
use hako_core::hotkey::{chord_label, HotkeyAction, HotkeyBinding};
use hako_core::ipc::MeterTargetSpec;
use hako_core::jarvis::{ActionSpec, ACTIONS, DEFAULT_SEARCH_URL};
use hako_core::remap::EvdevKey;
use hako_core::voice::{
    apply_voice_settings, Accelerator, CatalogModel, EngineStatus, JarvisSettings, ModelChoice,
    ModelStatus, OutputMethod, PasteKey, Replacement, SoundTheme, SubmitKey, UserAction,
    UserCommand, VoiceFile, VoiceOp, VoicePartial, VoiceState, CAPTURE_NODE, CATALOG,
    KEY_DELAY_MS_RANGE, MAX_HISTORY, MAX_RECORDING_S, PAUSE_MS_RANGE,
};
use std::collections::{BTreeSet, HashMap};

/// How the catalog is ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    #[default]
    Size,
    /// Lowest English word error rate first; unmeasured last.
    Accuracy,
    Name,
}

impl SortBy {
    pub const ALL: [Self; 3] = [Self::Size, Self::Accuracy, Self::Name];

    pub fn label(self) -> &'static str {
        match self {
            Self::Size => "Smallest first",
            Self::Accuracy => "Most accurate first",
            Self::Name => "By name",
        }
    }
}

/// Which catalog models the page lists.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogFilter {
    /// ISO code the model must know.
    pub language: Option<String>,
    /// Only models that type while you speak.
    pub streaming_only: bool,
    /// Largest download (bytes) of the quant shown.
    pub max_size: Option<u64>,
    pub sort: SortBy,
}

/// Download size limits offered by the filter.
const MAX_SIZES: [(u64, &str); 5] = [
    (100_000_000, "Up to 100 MB"),
    (250_000_000, "Up to 250 MB"),
    (500_000_000, "Up to 500 MB"),
    (1_000_000_000, "Up to 1 GB"),
    (2_000_000_000, "Up to 2 GB"),
];

/// The quant shown for `m`: the one picked, else Q8_0 (or its smallest).
fn quant_of<'a>(m: &'a CatalogModel, quants: &'a HashMap<String, String>) -> &'a str {
    quants
        .get(&m.id)
        .map(String::as_str)
        .filter(|q| m.file(q).is_some())
        .unwrap_or_else(|| m.default_quant())
}

fn size_of(m: &CatalogModel, quant: &str) -> u64 {
    m.file(quant).map_or(0, |f| f.size)
}

/// English WER (%) of this quant, else of any measured one.
fn wer_of(m: &CatalogModel, quant: &str) -> Option<f64> {
    m.wer_en
        .get(quant)
        .or_else(|| m.wer_en.values().next())
        .copied()
}

/// The catalog models `f` lets through, in its order.
pub fn filter_models<'a>(
    models: &'a [CatalogModel],
    f: &CatalogFilter,
    quants: &HashMap<String, String>,
) -> Vec<&'a CatalogModel> {
    let mut list: Vec<&CatalogModel> = models
        .iter()
        .filter(|m| {
            f.language.as_ref().is_none_or(|l| m.languages.contains(l))
                && (!f.streaming_only || m.streaming)
                && f.max_size
                    .is_none_or(|max| size_of(m, quant_of(m, quants)) <= max)
        })
        .collect();
    let name = |m: &CatalogModel| m.name.to_lowercase();
    match f.sort {
        SortBy::Size => list.sort_by_key(|m| (size_of(m, quant_of(m, quants)), name(m))),
        SortBy::Accuracy => list.sort_by(|a, b| {
            let wer = |m: &CatalogModel| wer_of(m, quant_of(m, quants)).unwrap_or(f64::INFINITY);
            wer(a)
                .total_cmp(&wer(b))
                .then_with(|| name(a).cmp(&name(b)))
        }),
        SortBy::Name => list.sort_by_key(|m| name(m)),
    }
    list
}

/// `0.6B`, `44M`.
pub fn fmt_params(p: u64) -> String {
    if p >= 500_000_000 {
        let b = format!("{:.1}", p as f64 / 1e9);
        format!("{}B", b.strip_suffix(".0").unwrap_or(&b))
    } else {
        format!("{}M", ((p as f64 / 1e6).round() as u64).max(1))
    }
}

/// `731 MB`, `1.6 GB`.
pub fn fmt_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{} MB", ((bytes as f64 / 1e6).round() as u64).max(1))
    }
}

/// Names for the catalog's language codes; others show as the code.
const LANGUAGES: [(&str, &str); 60] = [
    ("af", "Afrikaans"),
    ("ar", "Arabic"),
    ("az", "Azerbaijani"),
    ("be", "Belarusian"),
    ("bg", "Bulgarian"),
    ("bn", "Bengali"),
    ("bs", "Bosnian"),
    ("ca", "Catalan"),
    ("cs", "Czech"),
    ("cy", "Welsh"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("eu", "Basque"),
    ("fa", "Persian"),
    ("fi", "Finnish"),
    ("fil", "Filipino"),
    ("fr", "French"),
    ("ga", "Irish"),
    ("gl", "Galician"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hu", "Hungarian"),
    ("hy", "Armenian"),
    ("id", "Indonesian"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ka", "Georgian"),
    ("kk", "Kazakh"),
    ("ko", "Korean"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("mk", "Macedonian"),
    ("ms", "Malay"),
    ("mt", "Maltese"),
    ("nb", "Norwegian Bokmål"),
    ("nl", "Dutch"),
    ("nn", "Norwegian Nynorsk"),
    ("no", "Norwegian"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sr", "Serbian"),
    ("sv", "Swedish"),
    ("sw", "Swahili"),
    ("th", "Thai"),
    ("tl", "Tagalog"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("ur", "Urdu"),
    ("vi", "Vietnamese"),
    ("zh", "Chinese"),
];

pub fn lang_name(code: &str) -> &str {
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(code, |(_, n)| n)
}

/// `English`, or `25 languages`.
pub fn languages_label(langs: &[String]) -> String {
    match langs {
        [] => "Languages unknown".into(),
        [one] => lang_name(one).into(),
        many => format!("{} languages", many.len()),
    }
}

/// Codes sorted by their names.
fn by_name<'a>(codes: impl IntoIterator<Item = &'a String>) -> Vec<&'a String> {
    let mut v: Vec<&String> = codes.into_iter().collect();
    v.sort_by_key(|c| lang_name(c).to_lowercase());
    v
}

fn engine_label(e: &EngineStatus) -> (&'static str, Color32) {
    match e {
        EngineStatus::Off => ("Off", pal().text_faint),
        EngineStatus::NoModel => ("No model", pal().warn),
        EngineStatus::Idle => ("Ready", pal().ok),
        EngineStatus::Loading => ("Loading model", pal().accent),
        EngineStatus::Recording => ("Listening", pal().danger),
        EngineStatus::Transcribing => ("Transcribing", pal().accent),
        EngineStatus::Error { .. } => ("Error", pal().danger),
    }
}

/// The page's state: daemon state, live preview, catalog view and drafts.
#[derive(Default)]
pub struct VoiceUi {
    /// Daemon state plus local edits not yet answered.
    state: Option<VoiceState>,
    /// Newest daemon state, adopted once nothing is in flight.
    remote: Option<VoiceState>,
    /// Why the daemon couldn't report it (e.g. a hakod without voice).
    error: Option<String>,
    /// `PatchVoice` calls sent and not answered yet (the client sets this).
    pub busy: bool,
    /// The utterance being recorded, as recognised so far.
    partial: Option<VoicePartial>,
    pub filter: CatalogFilter,
    /// Quant picked per catalog model (until then Q8_0).
    quants: HashMap<String, String>,
    /// An output method picked whose fields are not filled in yet.
    pending_method: Option<OutputMethod>,
    /// Why the last edit was not sent.
    refused: Option<String>,
    /// The output test box.
    test_text: String,
    /// Why the last Jarvis settings edit was not saved (shown inline).
    jarvis_error: Option<String>,
    /// The user command being added or edited.
    pub(crate) command: Option<CommandDraft>,
    /// Model file whose Delete waits for a second click.
    delete: Option<String>,
    /// Action of a hotkey about to be added.
    add_action: Option<HotkeyAction>,
    /// Replacement being added: (from, to).
    replacement: (String, String),
}

impl VoiceUi {
    /// A `GetVoice` / `PatchVoice` answer or a `VoiceUpdated` event.
    pub fn receive(&mut self, state: VoiceState) {
        self.remote = Some(state);
        self.error = None;
    }

    /// `GetVoice` failed: the page explains instead of showing stale state.
    pub fn unavailable(&mut self, why: String) {
        self.state = None;
        self.remote = None;
        self.error = Some(why);
    }

    pub fn state(&self) -> Option<&VoiceState> {
        self.state.as_ref()
    }

    /// A `VoicePartial` event.
    pub fn partial(&mut self, p: VoicePartial) {
        self.partial = Some(p);
    }

    /// Adopt the daemon's state unless edits are on their way or a slider
    /// is being dragged.
    pub(crate) fn sync(&mut self, dragging: bool) {
        if self.state.is_none() || (!self.busy && !dragging) {
            if let Some(r) = self.remote.take() {
                self.state = Some(r);
            }
        }
        if let Some(st) = &self.state {
            if self.pending_method == Some(st.file.output.method) {
                self.pending_method = None;
            }
        }
    }

    /// Status line under "Voice" in the rail, and its dot colour.
    pub(crate) fn rail_status(&self) -> (String, Color32) {
        let st = match (&self.state, &self.error) {
            (None, Some(_)) => return ("Not available".into(), pal().text_faint),
            (None, None) => return ("Loading…".into(), pal().text_faint),
            (Some(s), _) => s,
        };
        let download = st.installed.iter().find_map(|i| match i.status {
            ModelStatus::Downloading { bytes, total } if total > 0 => {
                Some(bytes.saturating_mul(100) / total)
            }
            _ => None,
        });
        match (&st.engine, download) {
            _ if !st.file.enabled => ("Off".into(), pal().text_faint),
            (_, Some(pct)) => (format!("Downloading {pct}%"), pal().accent),
            (e, None) => {
                let (text, color) = engine_label(e);
                (text.into(), color)
            }
        }
    }
}

/// What a user command does, as the editor's picker offers it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CommandKind {
    #[default]
    Run,
    Keys,
    Type,
    Open,
    Profile,
}

impl CommandKind {
    pub const ALL: [Self; 5] = [Self::Run, Self::Keys, Self::Type, Self::Open, Self::Profile];

    pub fn label(self) -> &'static str {
        match self {
            Self::Run => "Run a program",
            Self::Keys => "Press keys",
            Self::Type => "Type text",
            Self::Open => "Open a website or file",
            Self::Profile => "Apply a profile",
        }
    }
}

/// A user command being added (`index: None`) or edited, as typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandDraft {
    pub index: Option<usize>,
    /// Phrases, one per line.
    pub phrases: String,
    pub kind: CommandKind,
    pub program: String,
    /// Arguments, one per line.
    pub args: String,
    pub keys: Vec<EvdevKey>,
    pub text: String,
    pub target: String,
    pub profile: String,
    pub confirm: bool,
    /// Why the last Save was refused.
    pub error: Option<String>,
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

impl CommandDraft {
    pub fn edit(index: usize, c: &UserCommand) -> Self {
        let mut d = Self {
            index: Some(index),
            phrases: c.say.join("\n"),
            confirm: c.confirm,
            ..Default::default()
        };
        match &c.action {
            UserAction::Run { program, args } => {
                d.program = program.clone();
                d.args = args.join("\n");
            }
            UserAction::Keys { keys } => {
                d.kind = CommandKind::Keys;
                d.keys = keys.clone();
            }
            UserAction::Type { text } => {
                d.kind = CommandKind::Type;
                d.text = text.clone();
            }
            UserAction::Open { target } => {
                d.kind = CommandKind::Open;
                d.target = target.clone();
            }
            UserAction::Profile { name } => {
                d.kind = CommandKind::Profile;
                d.profile = name.clone();
            }
        }
        d
    }

    /// The command as typed (unchecked).
    pub fn build(&self) -> UserCommand {
        let action = match self.kind {
            CommandKind::Run => UserAction::Run {
                program: self.program.trim().to_string(),
                args: lines(&self.args),
            },
            CommandKind::Keys => UserAction::Keys {
                keys: self.keys.clone(),
            },
            CommandKind::Type => UserAction::Type {
                text: self.text.clone(),
            },
            CommandKind::Open => UserAction::Open {
                target: self.target.trim().to_string(),
            },
            CommandKind::Profile => UserAction::Profile {
                name: self.profile.clone(),
            },
        };
        UserCommand {
            say: lines(&self.phrases),
            action,
            confirm: self.confirm,
        }
    }
}

/// `f` with `op` applied, checked as the daemon will check it.
fn check(f: &VoiceFile, op: &VoiceOp) -> Result<(), String> {
    let mut next = f.clone();
    apply_voice_settings(&mut next, op);
    next.validate()
}

/// The Jarvis settings with the draft saved into them, if valid.
pub fn save_command(f: &VoiceFile, d: &CommandDraft) -> Result<JarvisSettings, String> {
    let mut j = f.jarvis.clone();
    let c = d.build();
    match d.index {
        Some(i) if i < j.commands.len() => j.commands[i] = c,
        _ => j.commands.push(c),
    }
    check(f, &VoiceOp::SetJarvis { jarvis: j.clone() })?;
    Ok(j)
}

/// `Run /usr/bin/x --flag`, `Press Ctrl + V`, ...
pub fn user_action_label(a: &UserAction) -> String {
    match a {
        UserAction::Run { program, args } => std::iter::once(format!("Run {program}"))
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" "),
        UserAction::Keys { keys } => format!("Press {}", chord_label(keys, false)),
        UserAction::Type { text } => {
            let short: String = text.chars().take(40).collect();
            let more = if short.len() < text.len() { "…" } else { "" };
            format!("Type \"{}{more}\"", short.replace('\n', " "))
        }
        UserAction::Open { target } => format!("Open {target}"),
        UserAction::Profile { name } => format!("Apply profile {name}"),
    }
}

/// Why Jarvis cannot be turned on yet: it verifies the wake word and hears
/// the command with the dictation model, and spots the word with its own.
pub fn jarvis_blocker(st: &VoiceState) -> Option<&'static str> {
    let model_ready = match &st.file.model {
        Some(ModelChoice::Catalog { id, quant }) => st
            .installed
            .iter()
            .any(|i| &i.id == id && &i.quant == quant && i.status == ModelStatus::Ready),
        Some(ModelChoice::Custom { file }) => st.custom.iter().any(|c| &c.file == file),
        None => false,
    };
    if !model_ready {
        Some("Needs a dictation model: download one under Model and click Use")
    } else if st.wake_model != Some(ModelStatus::Ready) {
        Some("Download the wake-word model first")
    } else {
        None
    }
}

/// `just now`, `40 s ago`, `5 min ago`, `2 h ago`, `3 days ago`.
pub fn ago(now: u64, at: u64) -> String {
    match now.saturating_sub(at) {
        s if s < 10 => "just now".into(),
        s if s < 60 => format!("{s} s ago"),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s if s < 2 * 86_400 => "1 day ago".into(),
        s => format!("{} days ago", s / 86_400),
    }
}

/// "What can I say": the built-in actions by group.
const SAY_GROUPS: [(&str, &[&str]); 4] = [
    (
        "hako",
        &[
            "apply_profile",
            "strip_volume",
            "strip_mute",
            "mic_mute",
            "chat_mix",
            "lighting",
            "brightness",
            "dpi",
            "battery",
        ],
    ),
    (
        "Apps and web",
        &["open_app", "open_url", "search", "type", "dictate"],
    ),
    (
        "Sound and media",
        &["volume", "volume_step", "system_mute", "media"],
    ),
    ("Timers", &["timer"]),
];

/// Every built-in action by group; any the groups miss come last, under
/// "More".
pub fn say_groups() -> Vec<(&'static str, Vec<&'static ActionSpec>)> {
    let mut groups: Vec<(&str, Vec<&ActionSpec>)> = SAY_GROUPS
        .iter()
        .map(|(g, names)| {
            let specs = names
                .iter()
                .filter_map(|n| ACTIONS.iter().find(|s| s.name == *n))
                .collect();
            (*g, specs)
        })
        .collect();
    let rest: Vec<&ActionSpec> = ACTIONS
        .iter()
        .filter(|s| SAY_GROUPS.iter().all(|(_, n)| !n.contains(&s.name)))
        .collect();
    if !rest.is_empty() {
        groups.push(("More", rest));
    }
    groups
}

/// An edit: sent (checked first), or only shown while a slider is dragged.
enum Edit {
    Send(VoiceOp),
    Preview(VoiceOp),
}

/// A slider's outcome as an edit: final on release, preview while dragged.
fn slid(res: Option<bool>, op: impl FnOnce() -> VoiceOp) -> Option<Edit> {
    res.map(|done| {
        if done {
            Edit::Send(op())
        } else {
            Edit::Preview(op())
        }
    })
}

/// A microphone, for the rail.
pub fn icon_mic(p: &egui::Painter, r: Rect, c: Color32) {
    let s = Stroke::new(1.5_f32, c);
    let head = Rect::from_center_size(
        pos2(r.center().x, r.top() + r.height() * 0.32),
        vec2(r.width() * 0.36, r.height() * 0.56),
    );
    p.rect_stroke(head, head.width() / 2.0, s, egui::StrokeKind::Middle);
    let cup = r.height() * 0.30;
    let mid = pos2(r.center().x, head.center().y + 1.0);
    let pts: Vec<_> = (0..=12)
        .map(|i| {
            let a = std::f32::consts::PI * i as f32 / 12.0;
            mid + vec2(-a.cos() * cup, a.sin() * cup)
        })
        .collect();
    p.add(egui::Shape::line(pts, s));
    let foot = pos2(r.center().x, r.bottom() - 1.0);
    p.line_segment([mid + vec2(0.0, cup), foot], s);
    p.line_segment(
        [foot - vec2(cup * 0.6, 0.0), foot + vec2(cup * 0.6, 0.0)],
        s,
    );
}

/// Folded sections, by title; open state is kept per title.
#[cfg(test)]
pub(crate) const FOLDS: [&str; 6] = [
    "Text",
    "Sounds",
    "History",
    "Advanced",
    "Alternate profile",
    "What can I say",
];

pub(crate) fn fold_id(title: &str) -> egui::Id {
    egui::Id::new(("voice_fold", title))
}

/// A card whose body shows once its title is clicked.
fn fold(ui: &mut Ui, title: &str, hint: &str, body: impl FnOnce(&mut Ui)) {
    let id = fold_id(title);
    let open = ui.data(|m| m.get_temp::<bool>(id)).unwrap_or(false);
    let mut toggle = false;
    widgets::section_with(
        ui,
        |ui| {
            let resp = ui
                .add(egui::Label::new(widgets::section_title(title)).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            ui.label(egui::RichText::new(hint).small().color(pal().text_faint));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let tip = if open { "Collapse" } else { "Expand" };
                let chev = widgets::icon_button(ui, 26.0, tip, true, |p, r, c| {
                    widgets::icon_chevron(p, r, c, !open)
                });
                toggle = chev.clicked() || resp.clicked();
            });
        },
        |ui| {
            if open {
                // Rows are keyed by label: keep them apart from other cards'.
                ui.push_id(title, body);
            }
        },
    );
    ui.data_mut(|m| m.insert_temp(id, open != toggle));
}

/// A switch row; `Some(new)` when flipped.
fn switch_row(ui: &mut Ui, label: &str, hint: Option<&str>, on: bool) -> Option<bool> {
    let mut v = on;
    let mut changed = false;
    widgets::row(ui, label, hint, |ui| {
        let resp = widgets::switch(ui, &mut v);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, v, label));
        changed = resp.changed();
    });
    changed.then_some(v)
}

/// A dropdown row; `Some(option)` when another one is picked.
fn choice_row<T: PartialEq + Clone>(
    ui: &mut Ui,
    label: &str,
    hint: Option<&str>,
    current: &T,
    options: &[(T, String)],
) -> Option<T> {
    let mut picked = None;
    widgets::row(ui, label, hint, |ui| {
        let text = options
            .iter()
            .find(|(v, _)| v == current)
            .map_or_else(String::new, |(_, l)| l.clone());
        widgets::dropdown(("voice_choice", label), text, theme::DROPDOWN_W).show_ui(ui, |ui| {
            for (v, l) in options {
                if ui.selectable_label(v == current, l).clicked() && v != current {
                    picked = Some(v.clone());
                }
            }
        });
    });
    picked
}

/// A slider row: `Some(true)` once the value is final, `Some(false)` while
/// dragging.
fn slider_row(
    ui: &mut Ui,
    label: &str,
    hint: Option<&str>,
    value: &mut f32,
    (min, max): (f32, f32),
    log: bool,
    text: String,
) -> Option<bool> {
    let mut res = None;
    widgets::row(ui, label, hint, |ui| {
        ui.label(widgets::value_text(text));
        let resp = widgets::slider(
            ui,
            egui::Slider::new(value, min..=max)
                .logarithmic(log)
                .show_value(false)
                .clamping(egui::SliderClamping::Always),
            theme::SLIDER_W,
        );
        widgets::knob(ui, &resp, widgets::fraction(*value, min, max, log));
        if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
            res = Some(true);
        } else if resp.changed() {
            res = Some(false);
        }
    });
    res
}

/// Text field that commits on Enter or when it loses focus: `Some(text)`
/// once, when it changed.
fn text_commit(
    ui: &mut Ui,
    salt: &str,
    current: &str,
    hint: &str,
    width: f32,
    multiline: bool,
) -> Option<String> {
    let id = egui::Id::new(("voice_text", salt));
    let mut draft = ui
        .data(|m| m.get_temp::<String>(id))
        .unwrap_or_else(|| current.to_string());
    let resp = if multiline {
        ui.add(
            egui::TextEdit::multiline(&mut draft)
                .hint_text(hint)
                .desired_width(width)
                .desired_rows(3),
        )
    } else {
        widgets::text_field(ui, &mut draft, hint, width)
    };
    if resp.has_focus() {
        ui.data_mut(|m| m.insert_temp(id, draft));
        return None;
    }
    ui.data_mut(|m| m.remove_temp::<String>(id));
    (resp.lost_focus() && draft != current).then_some(draft)
}

/// A text row; empty text is `None`.
fn optional_text_row(
    ui: &mut Ui,
    label: &str,
    hint: Option<&str>,
    current: &Option<String>,
    placeholder: &str,
) -> Option<Option<String>> {
    let mut out = None;
    widgets::row(ui, label, hint, |ui| {
        let now = current.clone().unwrap_or_default();
        if let Some(t) = text_commit(ui, label, &now, placeholder, 260.0, false) {
            let t = t.trim().to_string();
            out = Some((!t.is_empty()).then_some(t));
        }
    });
    out
}

/// Words one per line (or comma separated) as a list.
fn word_list(text: &str) -> Vec<String> {
    text.split(['\n', ','])
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

impl HakoAppUi {
    /// The Voice page. Returns a device whose Bindings page should open with
    /// a hotkey's keys highlighted ("Show on device").
    pub(crate) fn voice_page(
        &mut self,
        ui: &mut Ui,
        out: &mut Vec<UiCommand>,
    ) -> Option<(DeviceId, String)> {
        let st = self.voice.state.clone();
        let mut edits = Vec::new();
        let on = st.as_ref().is_some_and(|s| s.file.enabled);
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_H);
            ui.label(egui::RichText::new("Voice").heading().color(pal().text));
            if on {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut v = true;
                    let resp = widgets::switch(ui, &mut v);
                    resp.widget_info(|| {
                        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, v, "Voice")
                    });
                    if resp.changed() {
                        edits.push(Edit::Send(VoiceOp::SetEnabled { on: false }));
                    }
                });
            }
        });
        ui.label(
            egui::RichText::new(
                "Dictate into any app with a hotkey. Speech is recognised on this computer; nothing is sent anywhere.",
            )
            .color(pal().text_muted),
        );
        ui.add_space(theme::SPACE_LG);

        let Some(st) = st else {
            let body = match &self.voice.error {
                Some(e) => e.as_str(),
                None => "Loading…",
            };
            widgets::section(
                ui,
                "Not available",
                |_| {},
                |ui| {
                    ui.label(egui::RichText::new(body).color(pal().text_muted));
                },
            );
            return None;
        };
        if let Some(e) = &st.load_error {
            ui.colored_label(
                pal().danger,
                format!(
                    "voice.toml could not be read: {e}. Changes are refused until it is fixed."
                ),
            );
            ui.add_space(theme::SPACE_SM);
        }
        if let Some(why) = &self.voice.refused {
            ui.colored_label(pal().danger, format!("Not saved: {why}"));
            ui.add_space(theme::SPACE_SM);
        }
        if !st.file.enabled {
            off_card(ui, &mut edits);
            self.apply_voice(edits, out);
            return None;
        }
        status_card(ui, &st, self.voice.partial.as_ref());
        model_section(ui, &mut self.voice, &st, &mut edits);
        let nav = self.voice_hotkeys(ui, out);
        self.mic_section(ui, &st, &mut edits);
        output_section(ui, &mut self.voice, &st, &mut edits);
        language_section(ui, &st, &mut edits);
        streaming_section(ui, &st.file, &mut edits);
        text_fold(ui, &mut self.voice, &st.file, &mut edits);
        sounds_fold(ui, &st.file, &mut edits);
        history_fold(ui, &st, &mut edits);
        advanced_fold(ui, &st, &mut edits);
        alt_fold(ui, &st, &mut edits);
        self.jarvis_section(ui, &st, &mut edits, out);
        self.commands_section(ui, &st, &mut edits, out);
        say_fold(ui, &st.file);
        self.apply_voice(edits, out);
        nav
    }

    /// Settings edits change the local copy and go out when the result is
    /// valid; a refused one is kept to be shown.
    fn apply_voice(&mut self, edits: Vec<Edit>, out: &mut Vec<UiCommand>) {
        let Some(local) = self.voice.state.as_mut() else {
            return;
        };
        let mut send = Vec::new();
        for e in edits {
            match e {
                Edit::Preview(op) => apply_voice_settings(&mut local.file, &op),
                Edit::Send(op) => {
                    let mut next = local.file.clone();
                    apply_voice_settings(&mut next, &op);
                    let checked = if next == local.file {
                        Ok(())
                    } else {
                        next.validate()
                    };
                    match checked {
                        Ok(()) => {
                            local.file = next;
                            send.push(op);
                        }
                        Err(why) => self.voice.refused = Some(why),
                    }
                }
            }
        }
        if !send.is_empty() {
            self.voice.refused = None;
            out.push(UiCommand::PatchVoice(send));
        }
    }

    /// Voice and Jarvis hotkeys with the shared editor, their devices,
    /// sources and conflicts, and Add.
    fn voice_hotkeys(
        &mut self,
        ui: &mut Ui,
        out: &mut Vec<UiCommand>,
    ) -> Option<(DeviceId, String)> {
        let devices: Vec<DeviceSnapshot> = self
            .state
            .as_ref()
            .map(|s| s.devices.clone())
            .unwrap_or_default();
        let hk = &mut self.hotkeys;
        let add_action = &mut self.voice.add_action;
        let mut nav = None;
        widgets::section(
            ui,
            "Hotkeys",
            |_| {},
            |ui| {
                let Some(st) = hk.state().cloned() else {
                    let body = hk.error().unwrap_or("Loading…").to_string();
                    ui.label(egui::RichText::new(body).color(pal().text_muted));
                    return;
                };
                if let Some(e) = &st.load_error {
                    ui.colored_label(
                        pal().danger,
                        format!("hotkeys.toml could not be read: {e}. Changes are refused until it is fixed."),
                    );
                }
                let list: Vec<HotkeyBinding> = st
                    .file
                    .bindings
                    .iter()
                    .filter(|b| b.action.feature() == "Voice")
                    .cloned()
                    .collect();
                if list.is_empty() {
                    widgets::hint(ui, "No hotkeys yet: add one below.");
                }
                for (i, b) in list.iter().enumerate() {
                    if i > 0 {
                        ui.add_space(theme::SPACE_SM);
                        widgets::divider(ui);
                        ui.add_space(theme::SPACE_SM);
                    }
                    hotkeys::editor(ui, hk, b, None, &devices, out);
                    ui.horizontal_wrapped(|ui| {
                        for d in devices.iter().filter(|d| {
                            matches!(d.kind, DeviceKind::Keyboard | DeviceKind::Mouse)
                                && b.scope.covers(d.id.vendor_id, d.id.product_id)
                        }) {
                            let link = egui::Link::new(
                                egui::RichText::new(format!("Show on {}", d.name))
                                    .small()
                                    .color(pal().accent),
                            );
                            if ui.add(link).clicked() {
                                nav = Some((d.id.clone(), b.id.clone()));
                            }
                        }
                    });
                    hotkeys::status_lines(ui, hk, b);
                    hotkeys::conflicts(ui, hk, b, None, &devices, out);
                }
                hotkeys::new_binding_row(ui, hk, Some("Voice"), out);
                ui.add_space(theme::SPACE_SM);
                widgets::divider(ui);
                let mut action = add_action.unwrap_or(HotkeyAction::VoiceTranscribe);
                widgets::row(
                    ui,
                    "Add a hotkey",
                    Some("Pick what it does, then press the keys"),
                    |ui| {
                        let can = hk.file().is_some() && hk.detecting().is_none();
                        if ui
                            .add_enabled_ui(can, |ui| widgets::button(ui, "Add…"))
                            .inner
                            .clicked()
                        {
                            if let Some(file) = hk.file() {
                                let id = hotkeys::new_id(file, action);
                                hk.detect(HotkeyBinding::new(&id, action, &[]), out);
                            }
                        }
                        hotkeys::action_picker(ui, "voice_add_action", &mut action);
                    },
                );
                *add_action = Some(action);
                widgets::hint(
                    ui,
                    "Scripts and desktop shortcuts can also run hako voice start | stop | toggle | cancel.",
                );
            },
        );
        nav
    }

    fn mic_section(&mut self, ui: &mut Ui, st: &VoiceState, edits: &mut Vec<Edit>) {
        // ponytail: the default source's node name is the daemon's to know,
        // so "Default" meters Voice's own capture stream (live while it
        // records or keeps the mic open).
        let target = MeterTargetSpec::Node {
            name: st
                .file
                .mic
                .clone()
                .unwrap_or_else(|| CAPTURE_NODE.to_string()),
            monitor_sink: false,
        };
        self.meter_targets.push(target.clone());
        let level = self
            .metered
            .iter()
            .position(|t| *t == target)
            .and_then(|k| self.meter_levels.get(k).copied())
            .unwrap_or_default();
        widgets::section(
            ui,
            "Microphone",
            |_| {},
            |ui| {
                let mut options = vec![(None, "Default".to_string())];
                options.extend(
                    st.sources
                        .iter()
                        .map(|(name, desc)| (Some(name.clone()), desc.clone())),
                );
                if let Some(m) = &st.file.mic {
                    if !options.iter().any(|(v, _)| v.as_ref() == Some(m)) {
                        options.push((Some(m.clone()), format!("{m} (not found)")));
                    }
                }
                if let Some(mic) = choice_row(ui, "Input", None, &st.file.mic, &options) {
                    edits.push(Edit::Send(VoiceOp::SetMic { mic }));
                }
                widgets::row(
                    ui,
                    "Level",
                    st.file.mic.is_none().then_some("Shows while Voice records"),
                    |ui| widgets::stereo_meter(ui, "voice_mic", level, theme::SLIDER_W),
                );
                widgets::hint(
                    ui,
                    "Pick a mixer virtual microphone (such as Hako_Mic) so AI noise removal runs before Voice hears you.",
                );
            },
        );
    }

    /// Jarvis: on / off, the wake-word model and word, the push-to-command
    /// hotkey, and the last command heard.
    fn jarvis_section(
        &mut self,
        ui: &mut Ui,
        st: &VoiceState,
        edits: &mut Vec<Edit>,
        out: &mut Vec<UiCommand>,
    ) {
        let devices: Vec<DeviceSnapshot> = self
            .state
            .as_ref()
            .map(|s| s.devices.clone())
            .unwrap_or_default();
        let hk = &mut self.hotkeys;
        let v = &mut self.voice;
        let f = &st.file;
        let (status, color) = if st.jarvis_listening {
            ("Listening", pal().ok)
        } else if f.jarvis.enabled {
            ("Not listening", pal().warn)
        } else {
            ("Off", pal().text_faint)
        };
        widgets::section(
            ui,
            "Jarvis",
            |ui| {
                widgets::pill(ui, status, color);
            },
            |ui| {
                widgets::hint(
                    ui,
                    "Say the wake word and a command in one breath: \"Jarvis, open Firefox\". Recognised on this computer like dictation.",
                );
                let mut j = f.jarvis.clone();
                let mut send = false;
                let blocker = jarvis_blocker(st);
                let hint = match blocker {
                    Some(why) if !j.enabled => why,
                    _ => "The microphone stays open while it listens",
                };
                let flipped = ui
                    .add_enabled_ui(j.enabled || blocker.is_none(), |ui| {
                        switch_row(ui, "Listen for the wake word", Some(hint), j.enabled)
                    })
                    .inner;
                if let Some(on) = flipped {
                    j.enabled = on;
                    send = true;
                }
                if st.jarvis_listening {
                    ui.colored_label(
                        pal().ok,
                        format!("Listening for « {} »", f.jarvis.wake_word),
                    );
                } else if f.jarvis.enabled {
                    widgets::hint(ui, "Not listening right now: Status above says why.");
                }
                wake_model_row(ui, st, edits);
                widgets::row(
                    ui,
                    "Wake word",
                    Some("1 to 3 English words; uncommon words wake less by mistake"),
                    |ui| {
                        if let Some(w) =
                            text_commit(ui, "wake_word", &j.wake_word, "jarvis", 200.0, false)
                        {
                            j.wake_word = w.trim().to_lowercase();
                            send = true;
                        }
                    },
                );
                let mut sens = j.sensitivity * 100.0;
                let res = slider_row(
                    ui,
                    "Sensitivity",
                    Some("Higher wakes more easily, and more often by mistake"),
                    &mut sens,
                    (0.0, 100.0),
                    false,
                    format!("{:.0}%", j.sensitivity * 100.0),
                );
                j.sensitivity = (sens.round() / 100.0).clamp(0.0, 1.0);
                if let Some(on) = switch_row(
                    ui,
                    "Pause in fullscreen apps",
                    Some("Games and videos don't wake it"),
                    j.pause_when_fullscreen,
                ) {
                    j.pause_when_fullscreen = on;
                    send = true;
                }
                if let Some(u) = optional_text_row(
                    ui,
                    "Search URL",
                    Some("For \"search for …\"; {q} is the query. Empty uses DuckDuckGo"),
                    &j.search_url,
                    DEFAULT_SEARCH_URL,
                ) {
                    j.search_url = u;
                    send = true;
                }
                if send {
                    let op = VoiceOp::SetJarvis { jarvis: j };
                    match check(f, &op) {
                        Ok(()) => {
                            v.jarvis_error = None;
                            edits.push(Edit::Send(op));
                        }
                        Err(why) => v.jarvis_error = Some(why),
                    }
                } else {
                    edits.extend(slid(res, || VoiceOp::SetJarvis { jarvis: j }));
                }
                if let Some(why) = &v.jarvis_error {
                    ui.colored_label(pal().danger, format!("Not saved: {why}"));
                }
                ui.add_space(theme::SPACE_SM);
                widgets::divider(ui);
                ui.add_space(theme::SPACE_SM);
                last_command(ui, st);
                ui.add_space(theme::SPACE_SM);
                widgets::divider(ui);
                ui.add_space(theme::SPACE_SM);
                push_to_command(ui, hk, &devices, out);
            },
        );
    }

    /// The user's own commands: list, reorder, remove, and the editor.
    fn commands_section(
        &mut self,
        ui: &mut Ui,
        st: &VoiceState,
        edits: &mut Vec<Edit>,
        out: &mut Vec<UiCommand>,
    ) {
        let profiles = self
            .state
            .as_ref()
            .map(|s| s.profiles.clone())
            .unwrap_or_default();
        let hk = &mut self.hotkeys;
        let v = &mut self.voice;
        let f = &st.file;
        let cmds = &f.jarvis.commands;
        widgets::section(
            ui,
            "Your commands",
            |ui| {
                ui.label(
                    egui::RichText::new(cmds.len().to_string())
                        .small()
                        .color(pal().text_faint),
                );
            },
            |ui| {
                widgets::hint(
                    ui,
                    "Phrases that run what you choose. They are checked before the built-in commands.",
                );
                let mut next = cmds.clone();
                for (i, c) in cmds.iter().enumerate() {
                    ui.push_id(("command", i), |ui| {
                        ui.horizontal(|ui| {
                            ui.set_min_height(theme::CONTROL_H);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if widgets::icon_button(
                                        ui,
                                        24.0,
                                        "Remove command",
                                        true,
                                        widgets::icon_close,
                                    )
                                    .clicked()
                                    {
                                        next.remove(i);
                                    }
                                    if widgets::icon_button(
                                        ui,
                                        24.0,
                                        "Edit command",
                                        true,
                                        widgets::icon_pencil,
                                    )
                                    .clicked()
                                    {
                                        v.command = Some(CommandDraft::edit(i, c));
                                    }
                                    if widgets::icon_button(
                                        ui,
                                        24.0,
                                        "Move down",
                                        i + 1 < cmds.len(),
                                        |p, r, c| widgets::icon_chevron(p, r, c, true),
                                    )
                                    .clicked()
                                    {
                                        next.swap(i, i + 1);
                                    }
                                    if widgets::icon_button(
                                        ui,
                                        24.0,
                                        "Move up",
                                        i > 0,
                                        |p, r, c| widgets::icon_chevron(p, r, c, false),
                                    )
                                    .clicked()
                                    {
                                        next.swap(i - 1, i);
                                    }
                                    if c.confirm {
                                        widgets::pill(ui, "Asks first", pal().text_muted);
                                    }
                                    ui.with_layout(
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(c.say.join("  ·  "))
                                                        .color(pal().text),
                                                )
                                                .truncate(),
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(user_action_label(
                                                        &c.action,
                                                    ))
                                                    .small()
                                                    .color(pal().text_muted),
                                                )
                                                .truncate(),
                                            );
                                        },
                                    );
                                },
                            );
                        });
                    });
                    ui.add_space(theme::SPACE_XS);
                }
                if next != *cmds {
                    // Indices moved: an open editor would save to the wrong one.
                    v.command = None;
                    let mut j = f.jarvis.clone();
                    j.commands = next;
                    edits.push(Edit::Send(VoiceOp::SetJarvis { jarvis: j }));
                }
                let mut done = false;
                if let Some(d) = v.command.as_mut() {
                    match command_editor(ui, d, hk, &profiles, out) {
                        Some(true) => match save_command(f, d) {
                            Ok(jarvis) => {
                                edits.push(Edit::Send(VoiceOp::SetJarvis { jarvis }));
                                done = true;
                            }
                            Err(why) => d.error = Some(why),
                        },
                        Some(false) => done = true,
                        None => {}
                    }
                } else {
                    if cmds.is_empty() {
                        widgets::hint(ui, "None yet.");
                    }
                    if widgets::button(ui, "Add a command").clicked() {
                        v.command = Some(CommandDraft::default());
                    }
                }
                if done {
                    if hk.detecting_chord(COMMAND_KEYS) {
                        hk.cancel_detect(out);
                    }
                    v.command = None;
                }
            },
        );
    }
}

/// Voice is off: what it does and Turn on.
fn off_card(ui: &mut Ui, edits: &mut Vec<Edit>) {
    widgets::section(
        ui,
        "Voice is off",
        |_| {},
        |ui| {
            for line in [
                "Hold a hotkey (Ctrl + Space by default), speak, and the words are typed into the app in focus.",
                "Recognition runs on this computer with a model you pick and download. While Voice is off nothing listens and nothing is loaded.",
            ] {
                ui.label(egui::RichText::new(line).color(pal().text_muted));
            }
            ui.add_space(theme::SPACE_MD);
            if widgets::primary_button(ui, "Turn on").clicked() {
                edits.push(Edit::Send(VoiceOp::SetEnabled { on: true }));
            }
        },
    );
}

/// Engine status, problems, and the utterance being recognised.
fn status_card(ui: &mut Ui, st: &VoiceState, partial: Option<&VoicePartial>) {
    widgets::section(
        ui,
        "Status",
        |ui| {
            let (text, color) = engine_label(&st.engine);
            if let Some(caps) = &st.loaded {
                widgets::pill(ui, &caps.backend, pal().text_muted)
                    .on_hover_text("Where the model runs");
            }
            widgets::pill(ui, text, color);
        },
        |ui| {
            match &st.engine {
                EngineStatus::NoModel => {
                    ui.colored_label(
                        pal().warn,
                        "Voice cannot dictate until a model is ready: download one below and click Use.",
                    );
                }
                EngineStatus::Error { message } => {
                    ui.colored_label(pal().danger, message);
                }
                _ => {}
            }
            for p in &st.problems {
                ui.colored_label(pal().warn, p);
            }
            let live = matches!(
                st.engine,
                EngineStatus::Recording | EngineStatus::Transcribing
            );
            theme::inset().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(40.0);
                match partial
                    .filter(|p| live && !(p.committed.is_empty() && p.tentative.is_empty()))
                {
                    Some(p) => {
                        let mut job = egui::text::LayoutJob::default();
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let fmt = |color| egui::TextFormat::simple(font.clone(), color);
                        job.append(&p.committed, 0.0, fmt(pal().text));
                        if !p.tentative.is_empty() {
                            let gap = if p.committed.is_empty() { 0.0 } else { 4.0 };
                            job.append(&p.tentative, gap, fmt(pal().text_faint));
                        }
                        ui.label(job);
                    }
                    None if live => widgets::hint(ui, "Listening…"),
                    None => {
                        widgets::hint(ui, "Words you dictate show here while they are recognised.")
                    }
                }
            });
        },
    );
}

fn model_section(ui: &mut Ui, v: &mut VoiceUi, st: &VoiceState, edits: &mut Vec<Edit>) {
    let shown = filter_models(&CATALOG.models, &v.filter, &v.quants);
    widgets::section(
        ui,
        "Model",
        |ui| {
            ui.label(
                egui::RichText::new(format!("{} of {}", shown.len(), CATALOG.models.len()))
                    .small()
                    .color(pal().text_faint),
            );
        },
        |ui| {
            let in_use = match &st.file.model {
                Some(ModelChoice::Catalog { id, quant }) => Some(format!(
                    "{} {quant}",
                    CATALOG.model(id).map_or(id.as_str(), |m| m.name.as_str())
                )),
                Some(ModelChoice::Custom { file }) => Some(file.clone()),
                None => None,
            };
            if let Some(name) = in_use {
                widgets::row(ui, "In use", None, |ui| {
                    ui.label(egui::RichText::new(name).color(pal().text));
                });
            } else {
                widgets::hint(
                    ui,
                    "No model chosen yet. Live models type while you speak; Batch models type at pauses. Smaller models are faster, larger ones more accurate.",
                );
                ui.add_space(theme::SPACE_SM);
            }
            filters(ui, &mut v.filter);
            ui.add_space(theme::SPACE_SM);
            let downloading = st
                .installed
                .iter()
                .any(|i| matches!(i.status, ModelStatus::Downloading { .. }));
            // The whole catalog is long: it scrolls on its own.
            egui::ScrollArea::vertical()
                .id_salt("voice_models")
                .max_height(520.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for m in &shown {
                        model_row(ui, v, st, m, downloading, edits);
                    }
                });
            if shown.is_empty() {
                widgets::hint(ui, "No model matches these filters.");
            }
            if !st.vad_ready {
                widgets::hint(
                    ui,
                    "The first download also fetches Silero VAD (about 2 MB), which skips silence.",
                );
            }
            if !st.custom.is_empty() {
                ui.add_space(theme::SPACE_SM);
                ui.label(theme::eyebrow("Custom"));
                for c in &st.custom {
                    custom_row(ui, v, st, c, edits);
                }
            }
            widgets::hint(
                ui,
                "Your own models: put a .gguf file into ~/.local/share/hako/models.",
            );
        },
    );
}

fn filters(ui: &mut Ui, f: &mut CatalogFilter) {
    ui.horizontal_wrapped(|ui| {
        let langs: BTreeSet<&String> = CATALOG
            .models
            .iter()
            .flat_map(|m| m.languages.iter())
            .collect();
        let current = f
            .language
            .as_deref()
            .map_or("Any language".to_string(), |l| lang_name(l).to_string());
        widgets::dropdown("voice_lang_filter", current, 170.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut f.language, None, "Any language");
            for l in by_name(langs) {
                ui.selectable_value(&mut f.language, Some(l.clone()), lang_name(l));
            }
        });
        let size = MAX_SIZES
            .iter()
            .find(|(s, _)| Some(*s) == f.max_size)
            .map_or("Any size", |(_, l)| l);
        widgets::dropdown("voice_size_filter", size, 140.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut f.max_size, None, "Any size");
            for (s, l) in MAX_SIZES {
                ui.selectable_value(&mut f.max_size, Some(s), l);
            }
        });
        widgets::dropdown("voice_sort", f.sort.label(), 170.0).show_ui(ui, |ui| {
            for s in SortBy::ALL {
                ui.selectable_value(&mut f.sort, s, s.label());
            }
        });
        ui.add_space(theme::SPACE_SM);
        let resp = widgets::switch(ui, &mut f.streaming_only);
        resp.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Checkbox,
                true,
                f.streaming_only,
                "Live only",
            )
        });
        ui.label(egui::RichText::new("Live only").color(pal().text_muted));
    });
}

/// One catalog model: what it is, its quant, and Download / Use / Delete.
fn model_row(
    ui: &mut Ui,
    v: &mut VoiceUi,
    st: &VoiceState,
    m: &CatalogModel,
    downloading: bool,
    edits: &mut Vec<Edit>,
) {
    let quant = quant_of(m, &v.quants).to_string();
    let file = m.file(&quant);
    let inst = st
        .installed
        .iter()
        .find(|i| i.id == m.id && i.quant == quant);
    let choice = ModelChoice::Catalog {
        id: m.id.clone(),
        quant: quant.clone(),
    };
    let in_use = st.file.model.as_ref() == Some(&choice);
    let chosen = matches!(&st.file.model, Some(ModelChoice::Catalog { id, .. }) if *id == m.id);
    let mut frame = theme::inset();
    if chosen {
        frame = frame.stroke(Stroke::new(1.5_f32, pal().accent));
    }
    let delete_key = format!("{}/{quant}", m.id);
    ui.push_id(&m.id, |ui| {
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_height(theme::CONTROL_H);
                ui.label(egui::RichText::new(&m.name).strong().color(pal().text));
                if m.streaming {
                    widgets::pill(ui, "Live", pal().accent).on_hover_text("Types while you speak");
                } else {
                    widgets::pill(ui, "Batch", pal().text_muted)
                        .on_hover_text("Types at pauses and when you stop");
                }
                if in_use {
                    widgets::pill(ui, "In use", pal().ok);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match inst.map(|i| &i.status) {
                        Some(ModelStatus::Downloading { .. }) => {
                            if widgets::button(ui, "Cancel").clicked() {
                                edits.push(Edit::Send(VoiceOp::CancelDownload));
                            }
                        }
                        Some(ModelStatus::Ready) | Some(ModelStatus::Corrupt { .. }) => {
                            if v.delete.as_deref() == Some(delete_key.as_str()) {
                                if widgets::button(ui, "Keep").clicked() {
                                    v.delete = None;
                                }
                                let del = ui.add(
                                    egui::Button::new(
                                        egui::RichText::new("Delete").color(pal().on_accent),
                                    )
                                    .fill(pal().danger)
                                    .min_size(vec2(theme::BUTTON_MIN_W, theme::CONTROL_H)),
                                );
                                if del.clicked() {
                                    edits.push(Edit::Send(VoiceOp::DeleteModel {
                                        id: m.id.clone(),
                                        quant: quant.clone(),
                                    }));
                                    v.delete = None;
                                }
                            } else if widgets::button(ui, "Delete…").clicked() {
                                v.delete = Some(delete_key.clone());
                            }
                            if matches!(inst.map(|i| &i.status), Some(ModelStatus::Corrupt { .. }))
                            {
                                if ui
                                    .add_enabled_ui(!downloading, |ui| {
                                        widgets::button(ui, "Download again")
                                    })
                                    .inner
                                    .clicked()
                                {
                                    edits.push(Edit::Send(VoiceOp::Download {
                                        id: m.id.clone(),
                                        quant: quant.clone(),
                                    }));
                                }
                            } else if !in_use && widgets::primary_button(ui, "Use").clicked() {
                                edits.push(Edit::Send(VoiceOp::SetModel {
                                    model: Some(choice.clone()),
                                }));
                            }
                        }
                        None => {
                            let resp = ui
                                .add_enabled_ui(!downloading && file.is_some(), |ui| {
                                    widgets::button(ui, "Download")
                                })
                                .inner
                                .on_disabled_hover_text("One download at a time");
                            if resp.clicked() {
                                edits.push(Edit::Send(VoiceOp::Download {
                                    id: m.id.clone(),
                                    quant: quant.clone(),
                                }));
                            }
                        }
                    }
                    let size = |q: &str| m.file(q).map_or(String::new(), |f| fmt_size(f.size));
                    widgets::dropdown(
                        ("quant", &m.id),
                        format!("{quant} · {}", size(&quant)),
                        170.0,
                    )
                    .show_ui(ui, |ui| {
                        for f in &m.files {
                            let ready = st.installed.iter().any(|i| {
                                i.id == m.id && i.quant == f.quant && i.status == ModelStatus::Ready
                            });
                            let label = format!(
                                "{} · {}{}",
                                f.quant,
                                fmt_size(f.size),
                                if ready { " · downloaded" } else { "" }
                            );
                            if ui.selectable_label(f.quant == quant, label).clicked() {
                                v.quants.insert(m.id.clone(), f.quant.clone());
                            }
                        }
                    })
                    .response
                    .on_hover_text("Quantization: smaller files, slightly less accurate");
                });
            });
            details(ui, m, &quant);
            match inst.map(|i| &i.status) {
                Some(ModelStatus::Downloading { bytes, total }) => {
                    let frac = if *total > 0 {
                        *bytes as f32 / *total as f32
                    } else {
                        0.0
                    };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_height(6.0)
                            .fill(pal().accent),
                    );
                    widgets::hint(
                        ui,
                        &format!("Downloading {} of {}", fmt_size(*bytes), fmt_size(*total)),
                    );
                }
                Some(ModelStatus::Corrupt { reason }) => {
                    ui.colored_label(pal().warn, format!("Damaged download: {reason}"));
                }
                _ => {}
            }
        });
    });
    ui.add_space(theme::SPACE_XS);
}

/// Parameters, languages, WER, speed and licence, in one small line.
fn details(ui: &mut Ui, m: &CatalogModel, quant: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let small = |t: String| egui::RichText::new(t).small().color(pal().text_muted);
        let dot = |ui: &mut Ui| ui.label(small("  ·  ".into()));
        if let Some(p) = m.params {
            ui.label(small(format!("{} parameters", fmt_params(p))));
            dot(ui);
        }
        let langs = ui.label(small(languages_label(&m.languages)));
        if m.languages.len() > 1 {
            let list: Vec<&str> = by_name(&m.languages)
                .into_iter()
                .map(|c| lang_name(c))
                .collect();
            langs.on_hover_text(list.join(", "));
        }
        if m.translate {
            dot(ui);
            ui.label(small("translates to English".into()));
        }
        if let Some(w) = wer_of(m, quant) {
            dot(ui);
            ui.label(small(format!("WER {w:.1}%")))
                .on_hover_text("Word error rate on English speech (LibriSpeech); lower is better");
        }
        if let Some(r) = m.rtf_cpu {
            dot(ui);
            let x = if r >= 10.0 {
                format!("{r:.0}")
            } else {
                format!("{r:.1}")
            };
            ui.label(small(format!("{x}× real time")))
                .on_hover_text("Speed on a laptop CPU; higher is faster");
        }
        if !m.license.is_empty() {
            dot(ui);
            if m.non_commercial() {
                ui.label(
                    egui::RichText::new(format!("{}: not for commercial use", m.license))
                        .small()
                        .color(pal().warn),
                );
            } else {
                ui.label(small(m.license.clone()));
            }
        }
    });
}

fn custom_row(
    ui: &mut Ui,
    v: &mut VoiceUi,
    st: &VoiceState,
    c: &hako_core::voice::CustomModel,
    edits: &mut Vec<Edit>,
) {
    let choice = ModelChoice::Custom {
        file: c.file.clone(),
    };
    let in_use = st.file.model.as_ref() == Some(&choice);
    let mut frame = theme::inset();
    if in_use {
        frame = frame.stroke(Stroke::new(1.5_f32, pal().accent));
    }
    ui.push_id(("custom", &c.file), |ui| {
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_height(theme::CONTROL_H);
                ui.label(egui::RichText::new(&c.file).strong().color(pal().text));
                ui.label(
                    egui::RichText::new(fmt_size(c.size))
                        .small()
                        .color(pal().text_muted),
                );
                if in_use {
                    widgets::pill(ui, "In use", pal().ok);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let key = format!("custom/{}", c.file);
                    if v.delete.as_deref() == Some(key.as_str()) {
                        if widgets::button(ui, "Keep").clicked() {
                            v.delete = None;
                        }
                        if widgets::button(ui, "Delete").clicked() {
                            edits.push(Edit::Send(VoiceOp::DeleteCustom {
                                file: c.file.clone(),
                            }));
                            v.delete = None;
                        }
                    } else if widgets::button(ui, "Delete…").clicked() {
                        v.delete = Some(key);
                    }
                    if !in_use && widgets::primary_button(ui, "Use").clicked() {
                        edits.push(Edit::Send(VoiceOp::SetModel {
                            model: Some(choice.clone()),
                        }));
                    }
                });
            });
        });
    });
    ui.add_space(theme::SPACE_XS);
}

fn output_section(ui: &mut Ui, v: &mut VoiceUi, st: &VoiceState, edits: &mut Vec<Edit>) {
    let saved = &st.file.output;
    let mut next = saved.clone();
    next.method = v.pending_method.unwrap_or(saved.method);
    let mut send = false;
    let mut preview = None;
    widgets::section(
        ui,
        "Output",
        |_| {},
        |ui| {
            let check = |m: OutputMethod| st.outputs.iter().find(|c| c.method == m);
            let options: Vec<(OutputMethod, String)> = OutputMethod::ALL
                .iter()
                .map(|m| {
                    let off = check(*m).is_some_and(|c| !c.ok);
                    let label = if off {
                        format!("{} (unavailable)", m.label())
                    } else {
                        m.label().to_string()
                    };
                    (*m, label)
                })
                .collect();
            if let Some(m) = choice_row(
                ui,
                "Method",
                Some("How text gets into the app in focus"),
                &next.method,
                &options,
            ) {
                v.pending_method = Some(m);
                next.method = m;
                send = true;
            }
            if let Some(c) = check(next.method).filter(|c| !c.ok) {
                ui.colored_label(
                    pal().warn,
                    c.reason
                        .as_deref()
                        .unwrap_or("Not available on this system"),
                );
            }
            match next.method {
                OutputMethod::Type => {
                    let (lo, hi) = KEY_DELAY_MS_RANGE;
                    let mut ms = next.key_delay_ms as f32;
                    let res = slider_row(
                        ui,
                        "Delay between keys",
                        Some("Raise it if an app drops letters"),
                        &mut ms,
                        (lo as f32, hi as f32),
                        false,
                        format!("{} ms", next.key_delay_ms),
                    );
                    next.key_delay_ms = ms.round() as u32;
                    preview = preview.or(res);
                    if let Some(on) = switch_row(
                        ui,
                        "Fix up at the end",
                        Some("Live models: retype words the final pass changed"),
                        next.fix_up_at_end,
                    ) {
                        next.fix_up_at_end = on;
                        send = true;
                    }
                    let detected = st
                        .layout
                        .as_deref()
                        .map_or("Keyboard layout to type with".to_string(), |l| {
                            format!("Detected: {l}. Empty uses it.")
                        });
                    if let Some(l) = optional_text_row(ui, "Keyboard layout", Some(&detected), &next.layout, "Detect") {
                        next.layout = l;
                        send = true;
                    }
                }
                OutputMethod::Paste => {
                    let keys = [
                        (PasteKey::ShiftInsert, "Shift + Insert".to_string()),
                        (PasteKey::CtrlV, "Ctrl + V".to_string()),
                        (PasteKey::CtrlShiftV, "Ctrl + Shift + V".to_string()),
                    ];
                    if let Some(k) = choice_row(ui, "Paste key", Some("Shift + Insert works in apps and terminals"), &next.paste_key, &keys) {
                        next.paste_key = k;
                        send = true;
                    }
                    let mut ms = next.restore_ms as f32;
                    let res = slider_row(
                        ui,
                        "Restore clipboard after",
                        None,
                        &mut ms,
                        (0.0, 5000.0),
                        false,
                        format!("{} ms", next.restore_ms),
                    );
                    next.restore_ms = (ms / 50.0).round() as u32 * 50;
                    preview = preview.or(res);
                }
                OutputMethod::External => {
                    let tools: Vec<(Option<String>, String)> =
                        std::iter::once((None, "Pick a tool".to_string()))
                            .chain(
                                ["wtype", "ydotool", "dotool", "xdotool"]
                                    .iter()
                                    .map(|t| (Some(t.to_string()), t.to_string())),
                            )
                            .collect();
                    if let Some(t) = choice_row(ui, "Tool", Some("Must be installed"), &next.external_tool, &tools) {
                        next.external_tool = t;
                        send = true;
                    }
                }
                OutputMethod::Command => {
                    if let Some(c) = optional_text_row(
                        ui,
                        "Command",
                        Some("Absolute path of a program: gets the text on stdin, what it prints is typed"),
                        &next.command,
                        "/usr/local/bin/fix-text",
                    ) {
                        next.command = c;
                        send = true;
                    }
                }
                OutputMethod::Clipboard => {
                    widgets::hint(ui, "The text is copied; paste it where you want it.");
                }
            }
            widgets::divider(ui);
            if let Some(on) = switch_row(ui, "Space after each text", None, next.trailing_space) {
                next.trailing_space = on;
                send = true;
            }
            let submit = [
                (None, "Off".to_string()),
                (Some(SubmitKey::Enter), "Enter".to_string()),
                (Some(SubmitKey::CtrlEnter), "Ctrl + Enter".to_string()),
                (Some(SubmitKey::ShiftEnter), "Shift + Enter".to_string()),
            ];
            if let Some(k) = choice_row(
                ui,
                "Press after the text",
                Some("Sends a chat message at once"),
                &next.auto_submit,
                &submit,
            ) {
                next.auto_submit = k;
                send = true;
            }
            widgets::divider(ui);
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Try it").color(pal().text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, "Clear").clicked() {
                        v.test_text.clear();
                    }
                });
            });
            ui.add(
                egui::TextEdit::multiline(&mut v.test_text)
                    .hint_text("Click here, then dictate with your hotkey")
                    .desired_width(f32::INFINITY)
                    .desired_rows(3),
            );
        },
    );
    if send || preview == Some(true) {
        edits.push(Edit::Send(VoiceOp::SetOutput { output: next }));
    } else if preview == Some(false) {
        edits.push(Edit::Preview(VoiceOp::SetOutput { output: next }));
    }
}

/// The languages and abilities of the chosen model.
fn model_caps(st: &VoiceState) -> (Vec<String>, bool, bool) {
    match &st.file.model {
        Some(ModelChoice::Catalog { id, .. }) => CATALOG
            .model(id)
            .map(|m| (m.languages.clone(), m.translate, m.lang_detect))
            .unwrap_or_default(),
        _ => st
            .loaded
            .as_ref()
            .map(|c| (c.languages.clone(), c.translate, c.lang_detect))
            .unwrap_or_default(),
    }
}

fn language_section(ui: &mut Ui, st: &VoiceState, edits: &mut Vec<Edit>) {
    let (langs, translate, detect) = model_caps(st);
    widgets::section(
        ui,
        "Language",
        |_| {},
        |ui| {
            if let [only] = langs.as_slice() {
                widgets::row(
                    ui,
                    "Language",
                    Some("The only language of this model"),
                    |ui| {
                        ui.label(egui::RichText::new(lang_name(only)).color(pal().text));
                    },
                );
            } else {
                let mut options = vec![(
                    None,
                    if detect || langs.is_empty() {
                        "Detect automatically".to_string()
                    } else {
                        "Model default".to_string()
                    },
                )];
                options.extend(
                    by_name(&langs)
                        .into_iter()
                        .map(|l| (Some(l.clone()), lang_name(l).to_string())),
                );
                if let Some(l) = &st.file.language {
                    if !langs.contains(l) {
                        options.push((Some(l.clone()), lang_name(l).to_string()));
                    }
                }
                if let Some(language) = choice_row(
                    ui,
                    "Language",
                    Some("What you speak"),
                    &st.file.language,
                    &options,
                ) {
                    edits.push(Edit::Send(VoiceOp::SetLanguage { language }));
                }
            }
            if translate {
                if let Some(on) = switch_row(
                    ui,
                    "Translate to English",
                    Some("Speak any language, get English text"),
                    st.file.translate,
                ) {
                    edits.push(Edit::Send(VoiceOp::SetTranslate { on }));
                }
            }
        },
    );
}

fn streaming_section(ui: &mut Ui, f: &VoiceFile, edits: &mut Vec<Edit>) {
    widgets::section(
        ui,
        "Streaming",
        |_| {},
        |ui| {
            let mut s = f.streaming.clone();
            if let Some(on) = switch_row(
                ui,
                "Type at pauses",
                Some("Batch models: type each sentence when you pause, not all at the end"),
                s.transcribe_at_pauses,
            ) {
                s.transcribe_at_pauses = on;
                edits.push(Edit::Send(VoiceOp::SetStreaming {
                    streaming: s.clone(),
                }));
            }
            let (lo, hi) = PAUSE_MS_RANGE;
            let mut ms = s.pause_ms as f32;
            let res = slider_row(
                ui,
                "Pause length",
                Some("Silence that ends a sentence"),
                &mut ms,
                (lo as f32, hi as f32),
                false,
                format!("{} ms", s.pause_ms),
            );
            s.pause_ms = (ms / 50.0).round() as u32 * 50;
            edits.extend(slid(res, || VoiceOp::SetStreaming { streaming: s }));
            widgets::hint(ui, "Live models type while you speak either way.");
        },
    );
}

fn text_fold(ui: &mut Ui, v: &mut VoiceUi, f: &VoiceFile, edits: &mut Vec<Edit>) {
    fold(
        ui,
        "Text",
        "Vocabulary, fillers, replacements, punctuation",
        |ui| {
            let mut t = f.text.clone();
            let mut send = false;
            ui.label(egui::RichText::new("Custom words").color(pal().text));
            widgets::hint(ui, "Names and jargon, one per line");
            if let Some(text) = text_commit(
                ui,
                "custom_words",
                &t.custom_words.join("\n"),
                "hako\nPipeWire",
                f32::INFINITY,
                true,
            ) {
                t.custom_words = word_list(&text);
                send = true;
            }
            ui.add_space(theme::SPACE_SM);
            if let Some(on) = switch_row(ui, "Remove filler words", None, t.remove_fillers) {
                t.remove_fillers = on;
                send = true;
            }
            if t.remove_fillers {
                widgets::row(
                    ui,
                    "Fillers",
                    Some("Comma separated; empty uses the built-in list"),
                    |ui| {
                        if let Some(text) = text_commit(
                            ui,
                            "fillers",
                            &t.fillers.join(", "),
                            "um, uh",
                            260.0,
                            false,
                        ) {
                            t.fillers = word_list(&text);
                            send = true;
                        }
                    },
                );
            }
            widgets::divider(ui);
            ui.add_space(theme::SPACE_SM);
            ui.label(egui::RichText::new("Replacements").color(pal().text));
            widgets::hint(
                ui,
                "Whole phrases, any case; \\n in the replacement is a new line",
            );
            let mut remove = None;
            for (i, r) in t.replacements.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&r.from).color(pal().text));
                    ui.label(
                        egui::RichText::new("becomes")
                            .small()
                            .color(pal().text_faint),
                    );
                    ui.label(egui::RichText::new(r.to.replace('\n', "\\n")).color(pal().text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button(ui, 24.0, "Remove", true, widgets::icon_close)
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = remove {
                t.replacements.remove(i);
                send = true;
            }
            ui.horizontal(|ui| {
                let (from, to) = &mut v.replacement;
                widgets::text_field(ui, from, "new paragraph", 180.0);
                widgets::text_field(ui, to, "\\n\\n", 180.0);
                let ok = !from.trim().is_empty();
                if ui
                    .add_enabled_ui(ok, |ui| widgets::button(ui, "Add"))
                    .inner
                    .clicked()
                {
                    t.replacements.push(Replacement {
                        from: from.trim().to_string(),
                        to: to.replace("\\n", "\n"),
                    });
                    from.clear();
                    to.clear();
                    send = true;
                }
            });
            ui.add_space(theme::SPACE_SM);
            widgets::divider(ui);
            if let Some(on) = switch_row(
                ui,
                "Spoken punctuation",
                Some("Say \"comma\", \"period\", \"new line\""),
                t.spoken_punctuation,
            ) {
                t.spoken_punctuation = on;
                send = true;
            }
            let punct = [
                (None, "Model default".to_string()),
                (Some(true), "On".to_string()),
                (Some(false), "Off".to_string()),
            ];
            if let Some(p) =
                choice_row(ui, "Punctuation and capitals", None, &t.punctuation, &punct)
            {
                t.punctuation = p;
                send = true;
            }
            if send {
                edits.push(Edit::Send(VoiceOp::SetText { text: t }));
            }
        },
    );
}

fn sounds_fold(ui: &mut Ui, f: &VoiceFile, edits: &mut Vec<Edit>) {
    fold(
        ui,
        "Sounds",
        "Pings when recording starts and stops",
        |ui| {
            let mut s = f.sounds.clone();
            let mut send = false;
            for (label, on) in [
                ("Start", &mut s.start),
                ("Stop", &mut s.stop),
                ("Cancel", &mut s.cancel),
                ("Error", &mut s.error),
            ] {
                if let Some(v) = switch_row(ui, label, None, *on) {
                    *on = v;
                    send = true;
                }
            }
            let themes = [
                (SoundTheme::Soft, "Soft".to_string()),
                (SoundTheme::Click, "Click".to_string()),
            ];
            if let Some(t) = choice_row(ui, "Sound", None, &s.theme, &themes) {
                s.theme = t;
                send = true;
            }
            let mut pct = s.volume * 100.0;
            let res = slider_row(
                ui,
                "Volume",
                None,
                &mut pct,
                (0.0, 100.0),
                false,
                format!("{:.0}%", s.volume * 100.0),
            );
            s.volume = (pct.round() / 100.0).clamp(0.0, 1.0);
            if send {
                edits.push(Edit::Send(VoiceOp::SetSounds { sounds: s }));
            } else {
                edits.extend(slid(res, || VoiceOp::SetSounds { sounds: s }));
            }
        },
    );
}

fn history_fold(ui: &mut Ui, st: &VoiceState, edits: &mut Vec<Edit>) {
    fold(ui, "History", "Recent transcripts", |ui| {
        let mut h = st.file.history.clone();
        let mut keep = h.keep as f32;
        let res = slider_row(
            ui,
            "Keep",
            Some("0 keeps none"),
            &mut keep,
            (0.0, MAX_HISTORY as f32),
            false,
            format!("{}", h.keep),
        );
        h.keep = keep.round() as u32;
        if let Some(on) = switch_row(ui, "Keep the audio too", None, h.keep_audio) {
            h.keep_audio = on;
            edits.push(Edit::Send(VoiceOp::SetHistory { history: h.clone() }));
        } else {
            edits.extend(slid(res, || VoiceOp::SetHistory { history: h }));
        }
        widgets::divider(ui);
        if st.history.is_empty() {
            ui.add_space(theme::SPACE_SM);
            widgets::hint(ui, "Nothing yet.");
        }
        for (i, e) in st.history.iter().enumerate() {
            ui.push_id(("history", i), |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(theme::CONTROL_H);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // ponytail: "Insert again" copies too until the daemon
                        // can type a history entry.
                        let insert = widgets::button(ui, "Insert again")
                            .on_hover_text("Copies it: paste with Ctrl + V")
                            .clicked();
                        let copy = widgets::button(ui, "Copy").clicked();
                        if insert || copy {
                            ui.ctx().copy_text(e.text.clone());
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&e.text).color(pal().text))
                                    .truncate(),
                            )
                            .on_hover_text(format!("{}\n\n{}", e.text, e.model));
                        });
                    });
                });
            });
        }
        if !st.history.is_empty() {
            ui.add_space(theme::SPACE_SM);
            if widgets::button(ui, "Clear history").clicked() {
                edits.push(Edit::Send(VoiceOp::ClearHistory));
            }
        }
    });
}

fn advanced_fold(ui: &mut Ui, st: &VoiceState, edits: &mut Vec<Edit>) {
    fold(
        ui,
        "Advanced",
        "Silence detection, accelerator, memory",
        |ui| {
            let mut a = st.file.advanced.clone();
            let mut send = false;
            if let Some(on) = switch_row(
                ui,
                "Skip silence",
                Some("Silero VAD, before batch transcription"),
                a.vad,
            ) {
                a.vad = on;
                send = true;
            }
            let mut accel = vec![
                (Accelerator::Auto, "Automatic".to_string()),
                (Accelerator::Cpu, "CPU".to_string()),
            ];
            accel.extend(
                st.accelerators
                    .iter()
                    .filter(|n| *n != "CPU")
                    .map(|n| (Accelerator::Device { name: n.clone() }, n.clone())),
            );
            if let Accelerator::Device { name } = &a.accelerator {
                if !st.accelerators.contains(name) {
                    accel.push((a.accelerator.clone(), format!("{name} (not found)")));
                }
            }
            if let Some(x) = choice_row(
                ui,
                "Run on",
                Some("Automatic uses a GPU when one is found"),
                &a.accelerator,
                &accel,
            ) {
                a.accelerator = x;
                send = true;
            }
            let mut unload = vec![
                (None, "Never".to_string()),
                (Some(0), "Immediately".to_string()),
                (Some(60), "After 1 minute".to_string()),
                (Some(300), "After 5 minutes".to_string()),
                (Some(900), "After 15 minutes".to_string()),
                (Some(3600), "After 1 hour".to_string()),
            ];
            if !unload.iter().any(|(v, _)| *v == a.unload_after_s) {
                unload.push((
                    a.unload_after_s,
                    format!("After {} s", a.unload_after_s.unwrap_or(0)),
                ));
            }
            if let Some(u) = choice_row(
                ui,
                "Free the model's memory",
                Some("When Voice was idle this long"),
                &a.unload_after_s,
                &unload,
            ) {
                a.unload_after_s = u;
                send = true;
            }
            for (label, hint, on) in [
                ("Load at startup", "Instant first use", &mut a.preload),
                (
                    "Keep the microphone open",
                    "No start delay; the mic shows as in use",
                    &mut a.always_on_mic,
                ),
                (
                    "Mute while recording",
                    "Silences the speakers until you stop",
                    &mut a.mute_while_recording,
                ),
            ] {
                if let Some(v) = switch_row(ui, label, Some(hint), *on) {
                    *on = v;
                    send = true;
                }
            }
            let (lo, hi) = MAX_RECORDING_S;
            let mut s = a.max_seconds as f32;
            let res = slider_row(
                ui,
                "Longest recording",
                Some("Stops by itself after this"),
                &mut s,
                (lo as f32, hi as f32),
                true,
                fmt_secs(a.max_seconds),
            );
            a.max_seconds = s.round() as u32;
            if send {
                edits.push(Edit::Send(VoiceOp::SetAdvanced { advanced: a }));
            } else {
                edits.extend(slid(res, || VoiceOp::SetAdvanced { advanced: a }));
            }
        },
    );
}

/// `45 s`, `5 min`, `5 min 30 s`.
fn fmt_secs(s: u32) -> String {
    match (s / 60, s % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

fn alt_fold(ui: &mut Ui, st: &VoiceState, edits: &mut Vec<Edit>) {
    fold(
        ui,
        "Alternate profile",
        "For the Dictate (alternate) hotkey",
        |ui| {
            widgets::hint(
                ui,
                "What the alternate hotkey does differently; the rest is as above.",
            );
            let mut a = st.file.alt.clone();
            let mut send = false;
            let mut models = vec![(None, "Same as above".to_string())];
            models.extend(
                st.installed
                    .iter()
                    .filter(|i| i.status == ModelStatus::Ready)
                    .map(|i| {
                        let name = CATALOG
                            .model(&i.id)
                            .map_or(i.id.as_str(), |m| m.name.as_str());
                        (
                            Some(ModelChoice::Catalog {
                                id: i.id.clone(),
                                quant: i.quant.clone(),
                            }),
                            format!("{name} {}", i.quant),
                        )
                    }),
            );
            models.extend(st.custom.iter().map(|c| {
                (
                    Some(ModelChoice::Custom {
                        file: c.file.clone(),
                    }),
                    c.file.clone(),
                )
            }));
            if a.model.is_some() && !models.iter().any(|(m, _)| *m == a.model) {
                models.push((a.model.clone(), "A model not downloaded".to_string()));
            }
            if let Some(m) = choice_row(ui, "Model", None, &a.model, &models) {
                a.model = m;
                send = true;
            }
            if let Some(l) = optional_text_row(
                ui,
                "Language",
                Some("Code such as en or de; empty is as above"),
                &a.language,
                "Same",
            ) {
                a.language = l;
                send = true;
            }
            let same = |t: &str| t.to_string();
            let translate = [
                (None, same("Same as above")),
                (Some(true), same("Translate to English")),
                (Some(false), same("Don't translate")),
            ];
            if let Some(t) = choice_row(ui, "Translate", None, &a.translate, &translate) {
                a.translate = t;
                send = true;
            }
            let mut methods = vec![(None, same("Same as above"))];
            methods.extend(
                OutputMethod::ALL
                    .iter()
                    .map(|m| (Some(*m), m.label().to_string())),
            );
            if let Some(m) = choice_row(ui, "Output", None, &a.method, &methods) {
                a.method = m;
                send = true;
            }
            if send {
                edits.push(Edit::Send(VoiceOp::SetAlt { alt: a }));
            }
        },
    );
}

/// Owner of the Detect that records a user command's keys.
const COMMAND_KEYS: &str = "jarvis_command";

/// The wake-word spotter: Download, progress, Ready, or damaged.
fn wake_model_row(ui: &mut Ui, st: &VoiceState, edits: &mut Vec<Edit>) {
    let kws = &CATALOG.kws;
    let busy = st
        .installed
        .iter()
        .any(|i| matches!(i.status, ModelStatus::Downloading { .. }));
    let hint = format!("Spots the wake word · {}", kws.license);
    widgets::row(ui, "Wake-word model", Some(&hint), |ui| {
        match &st.wake_model {
            None => {
                let resp = ui
                    .add_enabled_ui(!busy, |ui| {
                        widgets::button(ui, &format!("Download {}", fmt_size(kws.size)))
                    })
                    .inner
                    .on_disabled_hover_text("One download at a time");
                if resp.clicked() {
                    edits.push(Edit::Send(VoiceOp::DownloadWake));
                }
            }
            Some(ModelStatus::Downloading { .. }) => {
                if widgets::button(ui, "Cancel").clicked() {
                    edits.push(Edit::Send(VoiceOp::CancelDownload));
                }
            }
            Some(ModelStatus::Ready) => {
                ui.colored_label(pal().ok, "Ready");
            }
            Some(ModelStatus::Corrupt { .. }) => {
                let resp = ui
                    .add_enabled_ui(!busy, |ui| widgets::button(ui, "Download again"))
                    .inner
                    .on_disabled_hover_text("One download at a time");
                if resp.clicked() {
                    edits.push(Edit::Send(VoiceOp::DownloadWake));
                }
            }
        }
    });
    match &st.wake_model {
        Some(ModelStatus::Downloading { bytes, total }) => {
            let frac = if *total > 0 {
                *bytes as f32 / *total as f32
            } else {
                0.0
            };
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_height(6.0)
                    .fill(pal().accent),
            );
            widgets::hint(
                ui,
                &format!("Downloading {} of {}", fmt_size(*bytes), fmt_size(*total)),
            );
        }
        Some(ModelStatus::Corrupt { reason }) => {
            ui.colored_label(pal().warn, format!("Damaged download: {reason}"));
        }
        _ => {}
    }
}

/// What Jarvis heard last, what it made of it and how that went.
fn last_command(ui: &mut Ui, st: &VoiceState) {
    ui.label(egui::RichText::new("Last command").color(pal().text));
    let Some(c) = &st.last_command else {
        widgets::hint(ui, "Nothing yet. Commands you speak show here.");
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    theme::inset().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("\u{201c}{}\u{201d}", c.heard))
                    .strong()
                    .color(pal().text),
            );
            if c.ok {
                ui.colored_label(pal().ok, "Done");
            } else {
                ui.colored_label(pal().danger, "Failed");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::hint(ui, &ago(now, c.at));
            });
        });
        let understood = match &c.action {
            Some(a) => format!("Understood as: {a}"),
            None => "Not understood".to_string(),
        };
        ui.label(egui::RichText::new(understood).color(pal().text_muted));
        if let Some(m) = &c.message {
            let color = if c.ok { pal().text_muted } else { pal().danger };
            ui.colored_label(color, m);
        }
    });
}

/// The push-to-command hotkeys with the shared editor, or Add.
fn push_to_command(
    ui: &mut Ui,
    hk: &mut hotkeys::HotkeysUi,
    devices: &[DeviceSnapshot],
    out: &mut Vec<UiCommand>,
) {
    ui.label(egui::RichText::new("Push-to-command").color(pal().text));
    widgets::hint(
        ui,
        "Hold a hotkey, say a command, let go: no wake word needed, even while listening is off.",
    );
    let Some(st) = hk.state().cloned() else {
        let body = hk.error().unwrap_or("Loading…").to_string();
        ui.label(egui::RichText::new(body).color(pal().text_muted));
        return;
    };
    let list: Vec<HotkeyBinding> = st
        .file
        .bindings
        .iter()
        .filter(|b| b.action == HotkeyAction::JarvisListen)
        .cloned()
        .collect();
    for b in &list {
        hotkeys::editor(ui, hk, b, None, devices, out);
        hotkeys::status_lines(ui, hk, b);
        hotkeys::conflicts(ui, hk, b, None, devices, out);
    }
    hotkeys::new_binding_row(ui, hk, Some("Jarvis"), out);
    let adding = hk
        .detecting()
        .is_some_and(|d| d.action == HotkeyAction::JarvisListen);
    if list.is_empty() && !adding {
        let can = hk.file().is_some() && hk.detecting().is_none();
        ui.add_space(theme::SPACE_XS);
        let resp = ui
            .add_enabled_ui(can, |ui| widgets::button(ui, "Add push-to-command hotkey"))
            .inner
            .on_hover_text("Then press the keys on any keyboard or mouse");
        if resp.clicked() {
            if let Some(file) = hk.file() {
                let id = hotkeys::new_id(file, HotkeyAction::JarvisListen);
                hk.detect(
                    HotkeyBinding::new(&id, HotkeyAction::JarvisListen, &[]),
                    out,
                );
            }
        }
    }
}

/// The editor of one user command: `Some(true)` on Save, `Some(false)` on
/// Cancel.
fn command_editor(
    ui: &mut Ui,
    d: &mut CommandDraft,
    hk: &mut hotkeys::HotkeysUi,
    profiles: &[String],
    out: &mut Vec<UiCommand>,
) -> Option<bool> {
    if let Some(keys) = hk.take_chord(COMMAND_KEYS) {
        d.keys = keys;
    }
    let mut end = None;
    ui.add_space(theme::SPACE_SM);
    theme::inset().show(ui, |ui| {
        ui.set_width(ui.available_width());
        let title = if d.index.is_some() {
            "Edit command"
        } else {
            "New command"
        };
        ui.label(egui::RichText::new(title).strong().color(pal().text));
        ui.add_space(theme::SPACE_SM);
        ui.label(egui::RichText::new("When I say").color(pal().text));
        widgets::hint(
            ui,
            "One phrase per line. {slot} takes the rest of what you say: \"note {slot}\".",
        );
        ui.add(
            egui::TextEdit::multiline(&mut d.phrases)
                .hint_text("lights out\nbedtime")
                .desired_width(f32::INFINITY)
                .desired_rows(2),
        );
        let kinds: Vec<(CommandKind, String)> = CommandKind::ALL
            .iter()
            .map(|k| (*k, k.label().to_string()))
            .collect();
        if let Some(k) = choice_row(ui, "Do", None, &d.kind, &kinds) {
            d.kind = k;
        }
        match d.kind {
            CommandKind::Run => {
                widgets::row(
                    ui,
                    "Program",
                    Some("Absolute path; no shell, so | and > are plain text"),
                    |ui| {
                        widgets::text_field(ui, &mut d.program, "/usr/bin/firefox", 260.0);
                    },
                );
                ui.label(egui::RichText::new("Arguments").color(pal().text));
                widgets::hint(ui, "One per line; {slot} is replaced by what you said.");
                ui.add(
                    egui::TextEdit::multiline(&mut d.args)
                        .hint_text("--new-window\n{slot}")
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                );
            }
            CommandKind::Keys => {
                widgets::row(
                    ui,
                    "Keys",
                    Some("Pressed on hako's virtual keyboard"),
                    |ui| {
                        if hk.detecting_chord(COMMAND_KEYS) {
                            if widgets::button(ui, "Cancel").clicked() {
                                hk.cancel_detect(out);
                            }
                            hk.capture_caps(ui);
                        } else {
                            let can = hk.detecting().is_none();
                            if ui
                                .add_enabled_ui(can, |ui| widgets::button(ui, "Detect keys…"))
                                .inner
                                .on_hover_text("Press the keys; Esc alone cancels")
                                .clicked()
                            {
                                hk.detect_chord(COMMAND_KEYS, out);
                            }
                            if d.keys.is_empty() {
                                widgets::hint(ui, "None yet");
                            } else {
                                hotkeys::chord_caps(ui, &d.keys, true, theme::HOTKEY);
                            }
                        }
                    },
                );
            }
            CommandKind::Type => {
                ui.label(egui::RichText::new("Text").color(pal().text));
                ui.add(
                    egui::TextEdit::multiline(&mut d.text)
                        .hint_text("Typed into the app in focus")
                        .desired_width(f32::INFINITY)
                        .desired_rows(2),
                );
            }
            CommandKind::Open => {
                widgets::row(
                    ui,
                    "Open",
                    Some("A website or a file; {slot} allowed"),
                    |ui| {
                        widgets::text_field(ui, &mut d.target, "https://example.com", 260.0);
                    },
                );
            }
            CommandKind::Profile => {
                if profiles.is_empty() {
                    widgets::hint(ui, "No saved profiles yet: save one from the top bar.");
                }
                let mut options: Vec<(String, String)> =
                    profiles.iter().map(|p| (p.clone(), p.clone())).collect();
                if !d.profile.is_empty() && !profiles.contains(&d.profile) {
                    options.push((d.profile.clone(), format!("{} (not found)", d.profile)));
                }
                if let Some(p) = choice_row(ui, "Profile", None, &d.profile, &options) {
                    d.profile = p;
                }
            }
        }
        if let Some(on) = switch_row(
            ui,
            "Ask first",
            Some("Jarvis asks you to say \"yes\" before it runs this"),
            d.confirm,
        ) {
            d.confirm = on;
        }
        if let Some(why) = &d.error {
            ui.colored_label(pal().danger, format!("Not saved: {why}"));
        }
        ui.add_space(theme::SPACE_SM);
        ui.horizontal(|ui| {
            if widgets::primary_button(ui, "Save command").clicked() {
                end = Some(true);
            }
            if widgets::button(ui, "Cancel").clicked() {
                end = Some(false);
            }
        });
    });
    if end == Some(true) {
        d.error = None;
    }
    end
}

/// Everything Jarvis understands, by group, and the user's own commands.
fn say_fold(ui: &mut Ui, f: &VoiceFile) {
    fold(ui, "What can I say", "Built-in commands and yours", |ui| {
        widgets::hint(
            ui,
            "After the wake word. Close variants work too; names of apps, profiles and mixer strips are matched loosely.",
        );
        let quoted = |list: &mut dyn Iterator<Item = &str>| {
            list.map(|e| format!("\u{201c}{e}\u{201d}"))
                .collect::<Vec<_>>()
                .join("   ")
        };
        for (group, specs) in say_groups() {
            ui.add_space(theme::SPACE_SM);
            ui.label(theme::eyebrow(group));
            for s in specs {
                ui.push_id(s.name, |ui| {
                    ui.label(egui::RichText::new(s.description).color(pal().text));
                    widgets::hint(ui, &quoted(&mut s.examples.iter().copied()));
                });
            }
        }
        if !f.jarvis.commands.is_empty() {
            ui.add_space(theme::SPACE_SM);
            ui.label(theme::eyebrow("Yours"));
            for (i, c) in f.jarvis.commands.iter().enumerate() {
                ui.push_id(("yours", i), |ui| {
                    ui.label(egui::RichText::new(user_action_label(&c.action)).color(pal().text));
                    widgets::hint(ui, &quoted(&mut c.say.iter().map(String::as_str)));
                });
            }
        }
    });
}
