//! Mixer model: inputs (one or more sources, merged) → effects → fader →
//! routes to outputs (hardware sinks "A", virtual microphones "B").
//! Design: `docs/superpowers/specs/2026-11-13-mixer-design.md`.
//!
//! The daemon owns the file and the PipeWire graph; the UI edits through
//! [`MixerOp`]s, which [`MixerFile::apply`] interprets the same way on both
//! sides.

use crate::ipc::AuxEffectChain;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

/// Effects of an input or output; same stages as the line-in chain.
pub type EffectChain = AuxEffectChain;

pub const MIXER_VERSION: u32 = 1;
pub const MAX_INPUTS: usize = 16;
pub const MAX_OUTPUTS: usize = 8;
pub const MAX_SOURCES: usize = 8;
pub const MAX_SCENES: usize = 16;
/// Fader range (VoiceMeeter's): dB.
pub const GAIN_MIN_DB: f32 = -60.0;
pub const GAIN_MAX_DB: f32 = 12.0;
/// Per-source trim inside a merged input: dB.
pub const TRIM_MIN_DB: f32 = -24.0;
pub const TRIM_MAX_DB: f32 = 24.0;
/// Longest label shown on a strip.
pub const MAX_LABEL: usize = 32;

/// Serialises every read-modify-write of `mixer.toml` in this process.
static MIXER_LOCK: Mutex<()> = Mutex::new(());

/// `mixer.toml`: the live layout plus saved scenes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MixerFile {
    pub version: u32,
    /// The mixer owns ChatMix routing while on; line-in plays only through
    /// it. Off (the default) leaves ChatMix to the headset's own loopbacks.
    #[serde(default)]
    pub enabled: bool,
    /// The layout being played and edited.
    #[serde(default)]
    pub live: Scene,
    /// Layouts saved by name before profiles carried the layout; the daemon
    /// turns them into profiles at start. Kept for older files and UIs.
    #[serde(default)]
    pub scenes: Vec<NamedScene>,
    /// The headset's pre-mixer line-in settings were moved in; never redone.
    #[serde(default)]
    pub line_in_moved: bool,
}

impl Default for MixerFile {
    fn default() -> Self {
        Self {
            version: MIXER_VERSION,
            enabled: false,
            live: Scene::default(),
            scenes: Vec::new(),
            line_in_moved: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    #[serde(default)]
    pub inputs: Vec<Input>,
    #[serde(default)]
    pub outputs: Vec<Output>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedScene {
    pub name: String,
    pub scene: Scene,
}

/// One channel strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    /// Stable id (`[a-z0-9_-]`), unique among inputs.
    pub id: String,
    pub label: String,
    pub kind: InputKind,
    #[serde(default)]
    pub chain: EffectChain,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub solo: bool,
    /// Output ids this input plays into.
    #[serde(default)]
    pub routes: Vec<String>,
    /// The headset's ChatMix dial scales this input (`Game` / `Chat` side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatmix: Option<ChatMixSide>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputKind {
    /// Capture devices, each with its own trim, summed before the chain.
    Hardware { sources: Vec<Source> },
    /// A sink apps can play into, named `node_name` (e.g. `Hako_Game`), shown
    /// to apps as the input's label. The playback streams of `apps` (by
    /// [`MixerStatus::apps`] name) are moved into it while it runs.
    Virtual {
        node_name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        apps: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// PipeWire `node.name` of the capture device (stable across reboots).
    pub node: String,
    /// Device description when it was picked, shown while it is absent.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub trim_db: f32,
    #[serde(default)]
    pub mute: bool,
    /// ALSA "Input Source" item selected on the device's card while the
    /// mixer runs (e.g. `Line`); the card's previous item comes back when
    /// the source goes or the mixer stops. `None`: the card is left alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alsa_input: Option<String>,
    /// ALSA "Capture" level of the device's card, in percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_percent: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatMixSide {
    Game,
    Chat,
}

/// One output master.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Output {
    /// `A<n>` for hardware, `B<n>` for virtual (n = 1..=8), unique.
    pub id: String,
    pub label: String,
    pub kind: OutputKind,
    #[serde(default)]
    pub chain: EffectChain,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub mono: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputKind {
    /// Plays on a device sink (`node.name`); `description` is the device's
    /// name when it was picked, shown while it is absent.
    Hardware {
        sink: String,
        #[serde(default)]
        description: String,
    },
    /// A virtual microphone other apps record from, named `node_name`.
    Virtual { node_name: String },
}

impl OutputKind {
    pub fn id_prefix(&self) -> char {
        match self {
            Self::Hardware { .. } => 'A',
            Self::Virtual { .. } => 'B',
        }
    }
}

/// One edit. Small ops (gain, mute, route) are sent live while dragging;
/// structural ones replace a whole strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MixerOp {
    SetEnabled {
        on: bool,
    },
    SetInputGain {
        input: String,
        gain_db: f32,
    },
    SetInputMute {
        input: String,
        mute: bool,
    },
    SetInputSolo {
        input: String,
        solo: bool,
    },
    SetRoute {
        input: String,
        output: String,
        on: bool,
    },
    SetSourceTrim {
        input: String,
        source: usize,
        trim_db: f32,
    },
    SetSourceMute {
        input: String,
        source: usize,
        mute: bool,
    },
    /// The ALSA capture settings of a source's card (applied live).
    SetSourceCapture {
        input: String,
        source: usize,
        alsa_input: Option<String>,
        capture_percent: Option<u8>,
    },
    SetInputChain {
        input: String,
        chain: EffectChain,
    },
    SetOutputGain {
        output: String,
        gain_db: f32,
    },
    SetOutputMute {
        output: String,
        mute: bool,
    },
    SetOutputMono {
        output: String,
        mono: bool,
    },
    SetOutputChain {
        output: String,
        chain: EffectChain,
    },
    /// Add (`input.id` new) or replace (same id) a whole input.
    PutInput {
        input: Input,
    },
    RemoveInput {
        input: String,
    },
    MoveInput {
        input: String,
        index: usize,
    },
    /// Add or replace a whole output.
    PutOutput {
        output: Output,
    },
    /// Also drops every route to it.
    RemoveOutput {
        output: String,
    },
    /// Copy the live layout into `name` (replacing a scene of that name).
    SaveScene {
        name: String,
    },
    /// Make scene `name` the live layout.
    ApplyScene {
        name: String,
    },
    DeleteScene {
        name: String,
    },
}

impl MixerFile {
    /// Apply `op`, keeping the file valid: on error nothing changed.
    pub fn apply(&mut self, op: &MixerOp) -> Result<(), String> {
        let mut next = self.clone();
        next.apply_unchecked(op)?;
        // Checked as given: normalising would hide a bad trim or gain.
        next.validate()?;
        next.live.normalize();
        *self = next;
        Ok(())
    }

    fn apply_unchecked(&mut self, op: &MixerOp) -> Result<(), String> {
        let live = &mut self.live;
        match op {
            MixerOp::SetEnabled { on } => self.enabled = *on,
            MixerOp::SetInputGain { input, gain_db } => live.input(input)?.gain_db = *gain_db,
            MixerOp::SetInputMute { input, mute } => live.input(input)?.mute = *mute,
            MixerOp::SetInputSolo { input, solo } => live.input(input)?.solo = *solo,
            MixerOp::SetRoute { input, output, on } => {
                live.output_ref(output)?;
                let routes = &mut live.input(input)?.routes;
                routes.retain(|r| r != output);
                if *on {
                    routes.push(output.clone());
                }
            }
            MixerOp::SetSourceTrim {
                input,
                source,
                trim_db,
            } => {
                live.balance_source(input, *source, *trim_db != 0.0)?
                    .trim_db = *trim_db
            }
            MixerOp::SetSourceMute {
                input,
                source,
                mute,
            } => live.balance_source(input, *source, *mute)?.mute = *mute,
            MixerOp::SetSourceCapture {
                input,
                source,
                alsa_input,
                capture_percent,
            } => {
                let s = live.source(input, *source)?;
                s.alsa_input = alsa_input.clone();
                s.capture_percent = *capture_percent;
            }
            MixerOp::SetInputChain { input, chain } => live.input(input)?.chain = chain.clone(),
            MixerOp::SetOutputGain { output, gain_db } => live.output(output)?.gain_db = *gain_db,
            MixerOp::SetOutputMute { output, mute } => live.output(output)?.mute = *mute,
            MixerOp::SetOutputMono { output, mono } => live.output(output)?.mono = *mono,
            MixerOp::SetOutputChain { output, chain } => live.output(output)?.chain = chain.clone(),
            MixerOp::PutInput { input } => {
                match live.inputs.iter_mut().find(|i| i.id == input.id) {
                    Some(slot) => *slot = input.clone(),
                    None => live.inputs.push(input.clone()),
                }
            }
            MixerOp::RemoveInput { input } => {
                let before = live.inputs.len();
                live.inputs.retain(|i| &i.id != input);
                if live.inputs.len() == before {
                    return Err(format!("no input {input:?}"));
                }
            }
            MixerOp::MoveInput { input, index } => {
                let from = live
                    .inputs
                    .iter()
                    .position(|i| &i.id == input)
                    .ok_or_else(|| format!("no input {input:?}"))?;
                let strip = live.inputs.remove(from);
                live.inputs.insert((*index).min(live.inputs.len()), strip);
            }
            MixerOp::PutOutput { output } => {
                match live.outputs.iter_mut().find(|o| o.id == output.id) {
                    Some(slot) => *slot = output.clone(),
                    None => live.outputs.push(output.clone()),
                }
            }
            MixerOp::RemoveOutput { output } => {
                let before = live.outputs.len();
                live.outputs.retain(|o| &o.id != output);
                if live.outputs.len() == before {
                    return Err(format!("no output {output:?}"));
                }
                for i in &mut live.inputs {
                    i.routes.retain(|r| r != output);
                }
            }
            MixerOp::SaveScene { name } => {
                validate_scene_name(name)?;
                let scene = self.live.clone();
                match self.scenes.iter_mut().find(|s| &s.name == name) {
                    Some(slot) => slot.scene = scene,
                    None => self.scenes.push(NamedScene {
                        name: name.clone(),
                        scene,
                    }),
                }
            }
            MixerOp::ApplyScene { name } => {
                self.live = self
                    .scenes
                    .iter()
                    .find(|s| &s.name == name)
                    .ok_or_else(|| format!("no scene {name:?}"))?
                    .scene
                    .clone();
            }
            MixerOp::DeleteScene { name } => {
                let before = self.scenes.len();
                self.scenes.retain(|s| &s.name != name);
                if self.scenes.len() == before {
                    return Err(format!("no scene {name:?}"));
                }
            }
        }
        Ok(())
    }

    /// Every rule the daemon relies on before touching PipeWire.
    pub fn validate(&self) -> Result<(), String> {
        if self.version > MIXER_VERSION {
            return Err(format!(
                "mixer.toml version {} is newer than this hako ({MIXER_VERSION})",
                self.version
            ));
        }
        if self.scenes.len() > MAX_SCENES {
            return Err(format!("at most {MAX_SCENES} scenes"));
        }
        let mut names = HashSet::new();
        for s in &self.scenes {
            validate_scene_name(&s.name)?;
            if !names.insert(s.name.as_str()) {
                return Err(format!("duplicate scene {:?}", s.name));
            }
            s.scene
                .validate()
                .map_err(|e| format!("scene {:?}: {e}", s.name))?;
        }
        self.live.validate()
    }
}

impl Scene {
    /// An input with one source has one volume, its fader: the source's trim
    /// and mute (which only balance merged sources) fold into the strip. The
    /// trim sat before the effects, so level thresholds move with it and the
    /// strip sounds the same ([`crate::ipc::AuxEffectKind::level_shifted`]); a trim the
    /// fader or a stage cannot absorb stays on the source.
    pub fn normalize(&mut self) {
        for input in &mut self.inputs {
            let InputKind::Hardware { sources } = &mut input.kind else {
                continue;
            };
            let [source] = sources.as_mut_slice() else {
                continue;
            };
            input.mute |= std::mem::take(&mut source.mute);
            let trim = source.trim_db;
            if trim == 0.0 || !trim.is_finite() {
                continue;
            }
            let gain = input.gain_db + trim;
            let shifted: Option<Vec<_>> = input
                .chain
                .effects
                .iter()
                .map(|e| e.kind.level_shifted(trim))
                .collect();
            let Some(shifted) = shifted else {
                continue;
            };
            if !(GAIN_MIN_DB..=GAIN_MAX_DB).contains(&gain) {
                continue;
            }
            source.trim_db = 0.0;
            input.gain_db = gain;
            for (effect, kind) in input.chain.effects.iter_mut().zip(shifted) {
                effect.kind = kind;
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.inputs.len() > MAX_INPUTS {
            return Err(format!("at most {MAX_INPUTS} inputs"));
        }
        if self.outputs.len() > MAX_OUTPUTS {
            return Err(format!("at most {MAX_OUTPUTS} outputs"));
        }
        // Node names hako creates: unique, and never captured by an input
        // (a virtual output fed back into the mixer would loop).
        let mut created = HashSet::new();
        let mut output_ids = HashSet::new();
        for o in &self.outputs {
            validate_label(&o.label)?;
            validate_gain(&o.gain_db)?;
            o.chain
                .validate()
                .map_err(|e| format!("output {}: {e}", o.id))?;
            let prefix = o.kind.id_prefix();
            let n = o.id.strip_prefix(prefix).and_then(|n| n.parse::<u8>().ok());
            if !matches!(n, Some(1..=8)) || o.id.len() > 2 {
                return Err(format!("output id {:?} must be {prefix}1..{prefix}8", o.id));
            }
            if !output_ids.insert(o.id.as_str()) {
                return Err(format!("duplicate output {:?}", o.id));
            }
            match &o.kind {
                OutputKind::Hardware { sink, .. } => validate_node_name(sink)?,
                OutputKind::Virtual { node_name } => {
                    validate_node_name(node_name)?;
                    if !created.insert(node_name.as_str()) {
                        return Err(format!("node name {node_name:?} used twice"));
                    }
                }
            }
        }
        let mut input_ids = HashSet::new();
        let mut apps = HashSet::new();
        for i in &self.inputs {
            validate_id(&i.id)?;
            if !input_ids.insert(i.id.as_str()) {
                return Err(format!("duplicate input {:?}", i.id));
            }
            validate_label(&i.label)?;
            validate_gain(&i.gain_db)?;
            i.chain
                .validate()
                .map_err(|e| format!("input {}: {e}", i.id))?;
            let mut routes = HashSet::new();
            for r in &i.routes {
                if !output_ids.contains(r.as_str()) {
                    return Err(format!("input {} routes to unknown output {r:?}", i.id));
                }
                if !routes.insert(r.as_str()) {
                    return Err(format!("input {} routes to {r} twice", i.id));
                }
            }
            match &i.kind {
                InputKind::Hardware { sources } => {
                    if sources.len() > MAX_SOURCES {
                        return Err(format!("input {}: at most {MAX_SOURCES} sources", i.id));
                    }
                    for s in sources {
                        validate_node_name(&s.node)?;
                        if !s.trim_db.is_finite()
                            || !(TRIM_MIN_DB..=TRIM_MAX_DB).contains(&s.trim_db)
                        {
                            return Err(format!(
                                "trim must be in {TRIM_MIN_DB}..={TRIM_MAX_DB} dB"
                            ));
                        }
                        if let Some(item) = &s.alsa_input {
                            validate_alsa_item(item)?;
                        }
                        if s.capture_percent.is_some_and(|p| p > 100) {
                            return Err("capture level must be 0..=100 %".into());
                        }
                    }
                }
                InputKind::Virtual {
                    node_name,
                    apps: captured,
                } => {
                    validate_node_name(node_name)?;
                    if !created.insert(node_name.as_str()) {
                        return Err(format!("node name {node_name:?} used twice"));
                    }
                    if captured.len() > MAX_SOURCES {
                        return Err(format!("input {}: at most {MAX_SOURCES} apps", i.id));
                    }
                    for app in captured {
                        if app.trim().is_empty()
                            || app.chars().count() > 128
                            || app.chars().any(char::is_control)
                        {
                            return Err(format!("invalid app name {app:?}"));
                        }
                        // Two inputs would take the stream from each other.
                        if !apps.insert(app.as_str()) {
                            return Err(format!("{app} is captured by two inputs"));
                        }
                    }
                }
            }
        }
        // Nor played into one (a virtual input's sink).
        for o in &self.outputs {
            if let OutputKind::Hardware { sink, .. } = &o.kind {
                if created.contains(sink.as_str()) {
                    return Err(format!(
                        "output {} cannot play into {sink:?}: hako makes that node (feedback loop)",
                        o.id
                    ));
                }
            }
        }
        for i in &self.inputs {
            if let InputKind::Hardware { sources } = &i.kind {
                if let Some(s) = sources.iter().find(|s| created.contains(s.node.as_str())) {
                    return Err(format!(
                        "input {} cannot capture {:?}: hako makes that node (feedback loop)",
                        i.id, s.node
                    ));
                }
                // A peer's audio sent straight back to it echoes at once.
                for s in sources {
                    let Some((peer, false)) = crate::net::peer_of_node(&s.node) else {
                        continue;
                    };
                    let back = crate::net::send_node(peer);
                    if let Some(o) = self.outputs.iter().find(|o| {
                        i.routes.contains(&o.id)
                            && matches!(&o.kind, OutputKind::Hardware { sink, .. } if *sink == back)
                    }) {
                        return Err(format!(
                            "input {} plays the network audio it receives back to its sender ({}): echo",
                            i.id, o.id
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn input(&mut self, id: &str) -> Result<&mut Input, String> {
        self.inputs
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or_else(|| format!("no input {id:?}"))
    }

    fn output(&mut self, id: &str) -> Result<&mut Output, String> {
        self.outputs
            .iter_mut()
            .find(|o| o.id == id)
            .ok_or_else(|| format!("no output {id:?}"))
    }

    fn output_ref(&self, id: &str) -> Result<&Output, String> {
        self.outputs
            .iter()
            .find(|o| o.id == id)
            .ok_or_else(|| format!("no output {id:?}"))
    }

    /// A source to give its own trim or mute (`level`): only one of merged
    /// sources, since a lone source's level is the strip's (`normalize`
    /// folds it there, so setting it again would add up). Clearing is allowed.
    fn balance_source(
        &mut self,
        input: &str,
        index: usize,
        level: bool,
    ) -> Result<&mut Source, String> {
        let lone = matches!(&self.input(input)?.kind, InputKind::Hardware { sources } if sources.len() == 1);
        if lone && level {
            return Err(format!(
                "input {input} has one source: use the strip's fader and mute"
            ));
        }
        self.source(input, index)
    }

    fn source(&mut self, input: &str, index: usize) -> Result<&mut Source, String> {
        match &mut self.input(input)?.kind {
            InputKind::Hardware { sources } => sources
                .get_mut(index)
                .ok_or_else(|| format!("input {input} has no source {index}")),
            InputKind::Virtual { .. } => Err(format!("input {input} is virtual")),
        }
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > MAX_LABEL
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
    {
        return Err(format!("invalid id {id:?} (a-z, 0-9, _ and -)"));
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), String> {
    if label.trim().is_empty()
        || label.chars().count() > MAX_LABEL
        || label.chars().any(char::is_control)
    {
        return Err(format!(
            "label must be 1..={MAX_LABEL} printable characters"
        ));
    }
    Ok(())
}

fn validate_gain(db: &f32) -> Result<(), String> {
    if !db.is_finite() || !(GAIN_MIN_DB..=GAIN_MAX_DB).contains(db) {
        return Err(format!("gain must be in {GAIN_MIN_DB}..={GAIN_MAX_DB} dB"));
    }
    Ok(())
}

/// PipeWire node names: printable, no quotes or whitespace (they end up in
/// generated PipeWire config and link names).
fn validate_node_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 255
        || !name
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
    {
        return Err(format!("invalid PipeWire node name {name:?}"));
    }
    Ok(())
}

/// ALSA mixer item names (`amixer` argument, no shell): printable ASCII,
/// no `'` (amixer quotes items with it).
fn validate_alsa_item(item: &str) -> Result<(), String> {
    if item.trim().is_empty()
        || item.len() > 64
        || !item
            .chars()
            .all(|c| (c.is_ascii_graphic() || c == ' ') && c != '\'')
    {
        return Err(format!("invalid ALSA input source {item:?}"));
    }
    Ok(())
}

fn validate_scene_name(name: &str) -> Result<(), String> {
    validate_label(name).map_err(|_| format!("scene name must be 1..={MAX_LABEL} characters"))
}

/// `mixer.toml` next to `config.toml`.
pub fn mixer_path(paths: &crate::ConfigPaths) -> PathBuf {
    paths.root.join("mixer.toml")
}

/// The mixer file; absent = default (off, empty). Unparsable or invalid is an
/// error, never replaced by defaults.
pub fn load_mixer(paths: &crate::ConfigPaths) -> Result<MixerFile> {
    read_mixer(paths).map(|(file, _)| file)
}

/// The file as [`load_mixer`] gives it, and whether normalising changed it.
fn read_mixer(paths: &crate::ConfigPaths) -> Result<(MixerFile, bool)> {
    let path = mixer_path(paths);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((MixerFile::default(), false))
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let raw: MixerFile =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if let Err(e) = raw.validate() {
        bail!("{}: {e}", path.display());
    }
    let mut file = raw.clone();
    file.live.normalize();
    for s in &mut file.scenes {
        s.scene.normalize();
    }
    let changed = file != raw;
    Ok((file, changed))
}

/// Write back what [`load_mixer`] normalises in memory (an older file's
/// single-source trim), so the file does not keep it. `true`: rewritten.
pub fn normalize_mixer_file(paths: &crate::ConfigPaths) -> Result<bool> {
    let _guard = MIXER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (file, changed) = read_mixer(paths)?;
    if changed {
        crate::config::write_atomic(&mixer_path(paths), &toml::to_string_pretty(&file)?)?;
    }
    Ok(changed)
}

/// Read-modify-write `mixer.toml` atomically; `edit` sees the current file.
/// Nothing is written when `edit` fails or leaves the file invalid.
pub fn update_mixer<T>(
    paths: &crate::ConfigPaths,
    edit: impl FnOnce(&mut MixerFile) -> Result<T>,
) -> Result<(MixerFile, T)> {
    let _guard = MIXER_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut file = load_mixer(paths)?;
    let out = edit(&mut file)?;
    file.version = MIXER_VERSION;
    if let Err(e) = file.validate() {
        bail!("{e}");
    }
    paths.ensure()?;
    crate::config::write_atomic(&mixer_path(paths), &toml::to_string_pretty(&file)?)?;
    Ok((file, out))
}

// ── IPC ──────────────────────────────────────────────────────────

/// `PatchMixer` params: applied in order, all or nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchMixerParams {
    pub ops: Vec<MixerOp>,
}

/// Most ops one `PatchMixer` may carry.
pub const MAX_MIXER_OPS: usize = 64;

/// `GetMixer` / `PatchMixer` result and the `MixerUpdated` event payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MixerState {
    pub file: MixerFile,
    pub status: MixerStatus,
}

/// What the daemon sees right now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MixerStatus {
    /// The graph for `file.live` is built (mixer on and PipeWire reachable).
    pub running: bool,
    /// Per input / output id: why it is not fully working (missing device,
    /// plugin missing, ...).
    #[serde(default)]
    pub problems: Vec<StripProblem>,
    /// Capture devices and sinks an input / output can use.
    #[serde(default)]
    pub devices: Vec<AudioDevice>,
    /// Apps playing audio now (`application.name`, else the binary), for
    /// a virtual input's `apps`.
    #[serde(default)]
    pub apps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StripProblem {
    /// Input or output id.
    pub strip: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioDevice {
    pub node: String,
    pub description: String,
    pub direction: DeviceDirection,
    /// 1 = mono, 2 = stereo, ...
    pub channels: u8,
    /// Capture devices on an ALSA card: its "Input Source" items (empty
    /// when it has none), for [`Source::alsa_input`].
    #[serde(default)]
    pub inputs: Vec<String>,
    /// The card has a "Capture" level, for [`Source::capture_percent`].
    #[serde(default)]
    pub capture: bool,
    /// That level as the card has it (0-100), shown while a source sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_now: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceDirection {
    /// A capture device (microphone, line-in): an input source.
    Source,
    /// A playback device: a hardware output.
    Sink,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::AuxEffectKind;

    fn hw(id: &str, nodes: &[&str], routes: &[&str]) -> Input {
        Input {
            id: id.into(),
            label: id.into(),
            kind: InputKind::Hardware {
                sources: nodes
                    .iter()
                    .map(|n| Source {
                        node: (*n).into(),
                        description: String::new(),
                        trim_db: 0.0,
                        mute: false,
                        alsa_input: None,
                        capture_percent: None,
                    })
                    .collect(),
            },
            chain: EffectChain::default(),
            gain_db: 0.0,
            mute: false,
            solo: false,
            routes: routes.iter().map(|r| (*r).into()).collect(),
            chatmix: None,
        }
    }

    fn out(id: &str, kind: OutputKind) -> Output {
        Output {
            id: id.into(),
            label: id.into(),
            kind,
            chain: EffectChain::default(),
            gain_db: 0.0,
            mute: false,
            mono: false,
        }
    }

    fn sample() -> MixerFile {
        MixerFile {
            live: Scene {
                inputs: vec![hw("voice", &["alsa_input.usb-mic.mono-fallback"], &["B1"])],
                outputs: vec![
                    out(
                        "A1",
                        OutputKind::Hardware {
                            sink: "alsa_output.usb-headset.analog-stereo".into(),
                            description: "USB headset".into(),
                        },
                    ),
                    out(
                        "B1",
                        OutputKind::Virtual {
                            node_name: "Hako_Mic".into(),
                        },
                    ),
                ],
            },
            ..MixerFile::default()
        }
    }

    #[test]
    fn ops_edit_the_live_scene_and_failures_change_nothing() {
        let mut m = sample();
        m.apply(&MixerOp::SetRoute {
            input: "voice".into(),
            output: "A1".into(),
            on: true,
        })
        .unwrap();
        assert_eq!(m.live.inputs[0].routes, ["B1", "A1"]);
        let before = m.clone();
        assert!(m
            .apply(&MixerOp::SetInputGain {
                input: "voice".into(),
                gain_db: 40.0,
            })
            .is_err());
        assert!(m
            .apply(&MixerOp::SetRoute {
                input: "voice".into(),
                output: "B7".into(),
                on: true,
            })
            .is_err());
        assert_eq!(m, before);
        m.apply(&MixerOp::RemoveOutput {
            output: "B1".into(),
        })
        .unwrap();
        assert_eq!(m.live.inputs[0].routes, ["A1"]);
    }

    #[test]
    fn a_virtual_output_cannot_feed_an_input() {
        let mut m = sample();
        let err = m
            .apply(&MixerOp::PutInput {
                input: hw("loop", &["Hako_Mic"], &[]),
            })
            .unwrap_err();
        assert!(err.contains("feedback"), "{err}");
    }

    #[test]
    fn an_output_cannot_play_into_a_virtual_input() {
        let game = || Input {
            kind: InputKind::Virtual {
                node_name: "Hako_Game".into(),
                apps: Vec::new(),
            },
            ..hw("game", &[], &[])
        };
        let looped = || {
            out(
                "A2",
                OutputKind::Hardware {
                    sink: "Hako_Game".into(),
                    description: String::new(),
                },
            )
        };
        let mut m = sample();
        m.apply(&MixerOp::PutOutput { output: looped() }).unwrap();
        let err = m.apply(&MixerOp::PutInput { input: game() }).unwrap_err();
        assert!(err.contains("feedback"), "{err}");

        let mut m = sample();
        m.apply(&MixerOp::PutInput { input: game() }).unwrap();
        let err = m
            .apply(&MixerOp::PutOutput { output: looped() })
            .unwrap_err();
        assert!(err.contains("feedback"), "{err}");
    }

    #[test]
    fn a_bad_single_source_trim_is_refused_not_folded() {
        let with_trim = |trim_db| {
            let mut input = hw("voice", &["alsa_input.usb-mic.mono-fallback"], &["B1"]);
            if let InputKind::Hardware { sources } = &mut input.kind {
                sources[0].trim_db = trim_db;
            }
            input
        };
        let mut m = sample();
        for trim_db in [f32::NAN, f32::INFINITY, TRIM_MAX_DB + 1.0] {
            let input = with_trim(trim_db);
            assert!(m.apply(&MixerOp::PutInput { input }).is_err(), "{trim_db}");
        }
        assert_eq!(m, sample());

        let root = std::env::temp_dir().join(format!("hako-mixer-badtrim-{}", std::process::id()));
        let paths = crate::ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.clone(),
        };
        paths.ensure().unwrap();
        let mut bad = sample();
        bad.live.inputs[0] = with_trim(f32::NAN);
        std::fs::write(mixer_path(&paths), toml::to_string_pretty(&bad).unwrap()).unwrap();
        assert!(load_mixer(&paths).is_err(), "a NaN trim is an error, not 0");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_peer_s_audio_cannot_route_straight_back_to_it() {
        let mut m = sample();
        let id = "0011223344556677";
        m.apply(&MixerOp::PutOutput {
            output: out(
                "A2",
                OutputKind::Hardware {
                    sink: crate::net::send_node(id),
                    description: "hako to Desktop".into(),
                },
            ),
        })
        .unwrap();
        let recv = crate::net::recv_node(id);
        m.apply(&MixerOp::PutInput {
            input: hw("desk", &[recv.as_str()], &["A1"]),
        })
        .unwrap();
        let err = m
            .apply(&MixerOp::SetRoute {
                input: "desk".into(),
                output: "A2".into(),
                on: true,
            })
            .unwrap_err();
        assert!(err.contains("echo"), "{err}");
        // Another peer's network rack is fine.
        m.apply(&MixerOp::PutOutput {
            output: out(
                "A3",
                OutputKind::Hardware {
                    sink: crate::net::send_node("8899aabbccddeeff"),
                    description: String::new(),
                },
            ),
        })
        .unwrap();
        m.apply(&MixerOp::SetRoute {
            input: "desk".into(),
            output: "A3".into(),
            on: true,
        })
        .unwrap();
    }

    #[test]
    fn an_app_is_captured_by_one_input_at_most() {
        let virt = |id: &str, node: &str, apps: &[&str]| Input {
            kind: InputKind::Virtual {
                node_name: node.into(),
                apps: apps.iter().map(|a| (*a).into()).collect(),
            },
            ..hw(id, &[], &[])
        };
        let mut m = sample();
        m.apply(&MixerOp::PutInput {
            input: virt("music", "Hako_Music", &["Spotify", "Brave"]),
        })
        .unwrap();
        let err = m
            .apply(&MixerOp::PutInput {
                input: virt("web", "Hako_Web", &["Brave"]),
            })
            .unwrap_err();
        assert!(err.contains("two inputs"), "{err}");
        for bad in ["", " ", "a\nb"] {
            let input = virt("x", "Hako_X", &[bad]);
            assert!(m.apply(&MixerOp::PutInput { input }).is_err(), "{bad:?}");
        }
        let text = toml::to_string_pretty(&m).unwrap();
        assert_eq!(toml::from_str::<MixerFile>(&text).unwrap(), m);
        // Without apps the file reads as before.
        assert_eq!(text.matches("apps =").count(), 1, "{text}");
    }

    #[test]
    fn ids_names_and_ranges_are_checked() {
        let mut bad = sample();
        bad.live.outputs[1].id = "A2".into();
        assert!(bad.validate().is_err(), "virtual output with an A id");
        let mut bad = sample();
        bad.live.inputs.push(hw("voice", &[], &[]));
        assert!(bad.validate().is_err(), "duplicate input id");
        let mut bad = sample();
        bad.live.inputs[0].id = "Voice".into();
        assert!(bad.validate().is_err(), "uppercase id");
        let mut bad = sample();
        bad.live.outputs[0].kind = OutputKind::Hardware {
            sink: "has space".into(),
            description: String::new(),
        };
        assert!(bad.validate().is_err(), "node name with a space");
        let mut bad = sample();
        bad.version = MIXER_VERSION + 1;
        assert!(bad.validate().is_err(), "newer file");
    }

    #[test]
    fn a_single_source_has_one_volume_the_fader() {
        let mut m = sample();
        m.live.inputs[0].chain.effects = vec![
            crate::ipc::AuxEffect {
                id: "gate".into(),
                enabled: true,
                kind: AuxEffectKind::default_gate(),
            },
            crate::ipc::AuxEffect {
                id: "comp".into(),
                enabled: true,
                kind: AuxEffectKind::default_compressor(),
            },
        ];
        // A lone source's level is set on the strip: a trim op would add
        // up with every repeat, and a mute could not be taken back.
        for op in [
            MixerOp::SetSourceTrim {
                input: "voice".into(),
                source: 0,
                trim_db: 4.0,
            },
            MixerOp::SetSourceMute {
                input: "voice".into(),
                source: 0,
                mute: true,
            },
        ] {
            assert!(m.apply(&op).is_err(), "{op:?}");
        }
        // One left from an older file or an unmerge folds in.
        let mut voice = m.live.inputs[0].clone();
        if let InputKind::Hardware { sources } = &mut voice.kind {
            sources[0].trim_db = 4.0;
            sources[0].mute = true;
        }
        let mut too_loud = voice.clone();
        m.apply(&MixerOp::PutInput { input: voice }).unwrap();
        let voice = &m.live.inputs[0];
        assert_eq!((voice.gain_db, voice.mute), (4.0, true));
        let InputKind::Hardware { sources } = &voice.kind else {
            unreachable!()
        };
        assert_eq!((sources[0].trim_db, sources[0].mute), (0.0, false));
        // The trim was ahead of the effects: thresholds follow it down.
        let thresholds: Vec<f32> = voice
            .chain
            .effects
            .iter()
            .map(|e| match e.kind {
                AuxEffectKind::NoiseGate { threshold_db, .. }
                | AuxEffectKind::Compressor { threshold_db, .. } => threshold_db,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(thresholds, [-49.0, -24.0]);

        // A trim the fader cannot take in full stays on the source.
        too_loud.gain_db = 10.0;
        m.apply(&MixerOp::PutInput { input: too_loud }).unwrap();
        let voice = &m.live.inputs[0];
        let InputKind::Hardware { sources } = &voice.kind else {
            unreachable!()
        };
        assert_eq!((voice.gain_db, voice.mute), (10.0, true));
        assert_eq!((sources[0].trim_db, sources[0].mute), (4.0, false));
        assert_eq!(voice.chain.effects[0].kind, AuxEffectKind::default_gate());
        // Clearing it is allowed.
        m.apply(&MixerOp::SetSourceTrim {
            input: "voice".into(),
            source: 0,
            trim_db: 0.0,
        })
        .unwrap();

        // Merged sources keep their own balance.
        let mut merged = hw("pair", &["alsa_input.a", "alsa_input.b"], &[]);
        if let InputKind::Hardware { sources } = &mut merged.kind {
            sources[1].trim_db = -3.0;
        }
        m.apply(&MixerOp::PutInput { input: merged }).unwrap();
        let InputKind::Hardware { sources } = &m.live.inputs[1].kind else {
            unreachable!()
        };
        assert_eq!(sources[1].trim_db, -3.0);
        assert_eq!(m.live.inputs[1].gain_db, 0.0);
    }

    #[test]
    fn a_normalised_file_is_written_back_once() {
        let root = std::env::temp_dir().join(format!("hako-mixer-norm-{}", std::process::id()));
        let paths = crate::ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.clone(),
        };
        paths.ensure().unwrap();
        assert!(!normalize_mixer_file(&paths).unwrap(), "no file: none made");
        assert!(!mixer_path(&paths).exists());

        let mut old = sample();
        if let InputKind::Hardware { sources } = &mut old.live.inputs[0].kind {
            sources[0].trim_db = 4.0;
        }
        let on_disk = || -> MixerFile {
            toml::from_str(&std::fs::read_to_string(mixer_path(&paths)).unwrap()).unwrap()
        };
        std::fs::write(mixer_path(&paths), toml::to_string_pretty(&old).unwrap()).unwrap();
        // Loading alone never writes.
        assert_eq!(load_mixer(&paths).unwrap().live.inputs[0].gain_db, 4.0);
        assert_eq!(on_disk(), old);

        assert!(normalize_mixer_file(&paths).unwrap());
        assert_eq!(on_disk(), load_mixer(&paths).unwrap());
        assert_eq!(on_disk().live.inputs[0].gain_db, 4.0);
        assert!(
            !normalize_mixer_file(&paths).unwrap(),
            "nothing left to fold"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scenes_save_apply_and_delete() {
        let mut m = sample();
        m.apply(&MixerOp::SaveScene {
            name: "Streaming".into(),
        })
        .unwrap();
        m.apply(&MixerOp::RemoveInput {
            input: "voice".into(),
        })
        .unwrap();
        assert!(m.live.inputs.is_empty());
        m.apply(&MixerOp::ApplyScene {
            name: "Streaming".into(),
        })
        .unwrap();
        assert_eq!(m.live.inputs.len(), 1);
        m.apply(&MixerOp::DeleteScene {
            name: "Streaming".into(),
        })
        .unwrap();
        assert!(m.scenes.is_empty());
    }

    #[test]
    fn ipc_payloads_round_trip_through_json() {
        let patch = PatchMixerParams {
            ops: vec![
                MixerOp::SetRoute {
                    input: "voice".into(),
                    output: "A1".into(),
                    on: true,
                },
                MixerOp::PutInput {
                    input: hw("desk", &["alsa_input.usb-desk.analog-stereo"], &["B1"]),
                },
            ],
        };
        let json = serde_json::to_string(&patch).unwrap();
        assert!(json.contains(r#""op":"set_route""#), "{json}");
        assert_eq!(
            serde_json::from_str::<PatchMixerParams>(&json).unwrap(),
            patch
        );
        let state = MixerState {
            file: sample(),
            status: MixerStatus {
                running: true,
                problems: vec![StripProblem {
                    strip: "voice".into(),
                    message: "device unplugged".into(),
                }],
                devices: vec![AudioDevice {
                    node: "alsa_input.usb-mic.mono-fallback".into(),
                    description: "USB mic".into(),
                    direction: DeviceDirection::Source,
                    channels: 1,
                    inputs: vec!["Rear Mic".into(), "Line".into()],
                    capture: true,
                    capture_now: None,
                }],
                apps: vec!["Spotify".into()],
            },
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<MixerState>(&json).unwrap(), state);
    }

    #[test]
    fn source_capture_settings_apply_and_are_checked() {
        let mut m = sample();
        let set = |alsa_input: Option<&str>, capture_percent| MixerOp::SetSourceCapture {
            input: "voice".into(),
            source: 0,
            alsa_input: alsa_input.map(str::to_string),
            capture_percent,
        };
        m.apply(&set(Some("Line"), Some(66))).unwrap();
        let InputKind::Hardware { sources } = &m.live.inputs[0].kind else {
            unreachable!()
        };
        assert_eq!(sources[0].alsa_input.as_deref(), Some("Line"));
        assert_eq!(sources[0].capture_percent, Some(66));
        let before = m.clone();
        for bad in [
            set(Some("Line"), Some(101)),
            set(Some(""), None),
            set(Some("Line\n"), None),
            set(Some("it's"), None),
            set(Some("Mic ☺"), None),
            set(Some(&"x".repeat(65)), None),
        ] {
            assert!(m.apply(&bad).is_err(), "{bad:?}");
        }
        // No such source, or a virtual input.
        let mut far = set(None, None);
        if let MixerOp::SetSourceCapture { source, .. } = &mut far {
            *source = 3;
        }
        assert!(m.apply(&far).is_err());
        assert_eq!(m, before);
        // Both cleared: the card is left alone again.
        m.apply(&set(None, None)).unwrap();
        let text = toml::to_string_pretty(&m).unwrap();
        assert!(
            !text.contains("alsa_input =") && !text.contains("capture_percent"),
            "{text}"
        );
    }

    #[test]
    fn the_file_round_trips_through_toml() {
        let mut m = sample();
        m.live.inputs[0].chatmix = Some(ChatMixSide::Chat);
        let text = toml::to_string_pretty(&m).unwrap();
        assert_eq!(toml::from_str::<MixerFile>(&text).unwrap(), m);
        // An old file without the newer fields still loads.
        let old: MixerFile = toml::from_str("version = 1\n").unwrap();
        assert!(!old.enabled && old.live.inputs.is_empty());
        // Sources and outputs from before capture settings and descriptions.
        let old: MixerFile = toml::from_str(
            "version = 1\n[[live.inputs]]\nid = \"l\"\nlabel = \"L\"\n[live.inputs.kind]\ntype = \"hardware\"\n[[live.inputs.kind.sources]]\nnode = \"alsa_input.x\"\n[[live.outputs]]\nid = \"A1\"\nlabel = \"A\"\n[live.outputs.kind]\ntype = \"hardware\"\nsink = \"alsa_output.x\"\n",
        )
        .unwrap();
        old.validate().unwrap();
        assert!(
            matches!(&old.live.inputs[0].kind, InputKind::Hardware { sources }
            if sources[0].alsa_input.is_none() && sources[0].capture_percent.is_none())
        );
        assert!(
            matches!(&old.live.outputs[0].kind, OutputKind::Hardware { description, .. }
            if description.is_empty())
        );
    }
}
