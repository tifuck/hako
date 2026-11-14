//! The mixer's PipeWire graph: `mixer.toml`'s live scene as one filter-chain
//! child per strip, native links between them, faders as node `Props`.
//! Design: `docs/superpowers/specs/2026-11-13-mixer-design.md`.
//!
//! Input: a `pipewire -c` child whose capture side is a sink apps play into
//! (virtual) or a `2×N`-channel stream the daemon links the sources to
//! (hardware; builtin `mixer` nodes sum them with each source's trim), then
//! the effect chain; its playback side `hako_in_<id>` carries the fader.
//! Output: a child capturing on `hako_out_<id>` (the fader), the chain, an
//! optional mono fold (always for a one-channel sink, which gets one side),
//! and a playback side linked to the device sink
//! (hardware) or shown to apps as a microphone (virtual). Routes are links
//! from input playback to output capture ports; PipeWire sums them.
//!
//! [`MixerGraph::reconcile`] diffs the wanted scene against what runs:
//! fader, mute, solo, trim, route and effect-parameter edits are applied
//! live; a strip whose topology changed is rebuilt alone; the rest keep
//! running. Called again (healing) it restarts crashed strips with backoff and
//! links devices that appeared; a missing device is a [`StripProblem`].
//!
//! A hardware source may also set its ALSA card's input selection and
//! capture level (a line-in jack); the card's previous input comes back when
//! the source goes or the graph is torn down.
//!
//! A virtual input's apps: their playback streams are pointed at its sink
//! (`target.object` metadata, as pavucontrol moves a stream) while the strip
//! runs, and handed back to WirePlumber when the app, the strip or the graph
//! goes.

use crate::engine::{engine, fader_pod, params_pod, Direction, Engine, Graph, LinkHandle, Node};
use crate::{
    alsa_card, capture_level, card_capture_controls, current_input_source, db_to_gain, dsp_stages,
    effect_params, filter_chain_config, lsp_for, node_latency, pkill, pw_escape,
    same_effect_topology, set_capture_percent, set_input_source, spawn_pipewire, stop_child,
    wpctl_volume, write_conf, DspNode, DspStage, FilterGraph, NODE_WAIT,
};
use anyhow::{anyhow, Context, Result};
use hako_core::mixer::{
    AudioDevice, ChatMixSide, DeviceDirection, EffectChain, Input, InputKind, Output, OutputKind,
    Scene, Source, StripProblem,
};
use hako_core::AuxConfig;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long the nodes of a stopped strip may take to leave the registry.
const NAME_WAIT: Duration = Duration::from_secs(2);
/// Longest wait before restarting a strip that keeps failing.
const MAX_RETRY: Duration = Duration::from_secs(30);
/// A strip up this long counts as stable: its next crash restarts at once.
const STABLE: Duration = Duration::from_secs(30);
/// A link this young may not be in the registry cache yet.
const LINK_GRACE: Duration = Duration::from_secs(1);
/// Builtin `mixer` controls, one per summed source (`MAX_SOURCES`).
const GAINS: [&str; 8] = [
    "Gain 1", "Gain 2", "Gain 3", "Gain 4", "Gain 5", "Gain 6", "Gain 7", "Gain 8",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    Input,
    Output,
}

/// A strip: input or output, and its id.
pub type StripKey = (Side, String);

/// How a graph names what it makes. The daemon's nodes are `hako_in_<id>` /
/// `hako_out_<id>` with configs `hako-mixer-{in,out}-<id>.conf`; tests use
/// their own `hako-test-` prefixes.
#[derive(Debug, Clone)]
pub struct Naming {
    /// Prefix of the strips' own node names.
    pub nodes: String,
    /// Prefix of their config files in the runtime dir.
    pub files: String,
    /// Extra filter-chain module arguments (tests: `node.group`).
    pub args: String,
}

impl Naming {
    pub fn daemon() -> Self {
        Self {
            nodes: "hako_".into(),
            files: "hako-".into(),
            args: String::new(),
        }
    }
}

/// Kill strips a crashed hakod left behind (they match by config path, so
/// this also hits another running hakod's). Startup and the kill-switch only.
pub(crate) fn kill_orphans() {
    pkill(r"^pipewire -c .*/hako-mixer-(in|out)-[^/]*\.conf$");
    // Unit tests run next to the user's hakod: its runtime dir is not theirs.
    if !cfg!(test) {
        if let Some(dir) = hako_core::default_socket_path().parent() {
            remove_orphan_confs(dir);
        }
    }
}

/// The configs those strips ran from (`hako-mixer-{in,out}-<id>.conf`).
fn remove_orphan_confs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let strip = ["hako-mixer-in-", "hako-mixer-out-"]
            .iter()
            .any(|prefix| name.starts_with(prefix));
        if strip && name.ends_with(".conf") {
            if let Err(e) = std::fs::remove_file(entry.path()) {
                warn!("mixer: removing {}: {e}", entry.path().display());
            }
        }
    }
}

/// Everything a strip's child is built from; [`same_topology`] tells which
/// differences need a rebuild. Source and sink devices are links, not here.
#[derive(Debug, Clone, PartialEq)]
struct StripDef {
    /// `node.name` of the capture side (what feeds the strip) and the
    /// playback side (what it plays).
    capture: String,
    playback: String,
    label: String,
    /// Capture side is a sink apps play into (virtual input).
    sink: bool,
    /// Playback side is a source apps record from (virtual output).
    source: bool,
    /// Hardware input: linear trim per source (0 when muted), summed.
    trims: Vec<f32>,
    chain: EffectChain,
    mono: bool,
    latency: u32,
}

impl StripDef {
    /// Capture channels its sources link to (`AUX<n>`), or stereo.
    fn capture_channels(&self) -> Vec<String> {
        if self.trims.is_empty() {
            return vec!["FL".into(), "FR".into()];
        }
        (0..2 * self.trims.len())
            .map(|c| format!("AUX{c}"))
            .collect()
    }

    /// Where the fader sits: after the chain for inputs, before it for
    /// outputs (so a limiter there still catches what the fader adds).
    fn fader_node(&self, side: Side) -> &str {
        match side {
            Side::Input => &self.playback,
            Side::Output => &self.capture,
        }
    }
}

fn same_topology(a: &StripDef, b: &StripDef) -> bool {
    a.capture == b.capture
        && a.playback == b.playback
        && a.label == b.label
        && a.sink == b.sink
        && a.source == b.source
        && a.trims.len() == b.trims.len()
        && a.mono == b.mono
        && a.latency == b.latency
        && same_effect_topology(&a.chain, &b.chain)
}

fn source_trim(source: &Source) -> f32 {
    if source.mute {
        0.0
    } else {
        db_to_gain(source.trim_db)
    }
}

/// The strips `scene` wants, plus problems of strips that cannot exist.
/// A hardware output whose sink is `mono_sink` folds to mono: only one of
/// its channels is linked there (`wanted_links`).
fn strip_defs(
    scene: &Scene,
    naming: &Naming,
    latency: u32,
    mono_sink: impl Fn(&str) -> bool,
) -> (Vec<(StripKey, StripDef)>, Vec<StripProblem>) {
    let mut defs = Vec::new();
    let mut problems = Vec::new();
    for input in &scene.inputs {
        let playback = format!("{}in_{}", naming.nodes, input.id);
        let (capture, sink, trims) = match &input.kind {
            InputKind::Virtual { node_name, .. } => (node_name.clone(), true, Vec::new()),
            InputKind::Hardware { sources } if sources.is_empty() => {
                problems.push(problem(&input.id, "no source selected"));
                continue;
            }
            InputKind::Hardware { sources } => (
                format!("{playback}.capture"),
                false,
                sources.iter().map(source_trim).collect(),
            ),
        };
        defs.push((
            (Side::Input, input.id.clone()),
            StripDef {
                capture,
                playback,
                label: input.label.clone(),
                sink,
                source: false,
                trims,
                chain: input.chain.clone(),
                mono: false,
                latency,
            },
        ));
    }
    for output in &scene.outputs {
        let capture = format!("{}out_{}", naming.nodes, output.id);
        let (playback, source, mono) = match &output.kind {
            OutputKind::Hardware { sink, .. } => (
                format!("{capture}.play"),
                false,
                output.mono || mono_sink(sink),
            ),
            OutputKind::Virtual { node_name } => (node_name.clone(), true, output.mono),
        };
        defs.push((
            (Side::Output, output.id.clone()),
            StripDef {
                capture,
                playback,
                label: output.label.clone(),
                sink: false,
                source,
                trims: Vec::new(),
                chain: output.chain.clone(),
                mono,
                latency,
            },
        ));
    }
    (defs, problems)
}

fn problem(strip: &str, message: impl Into<String>) -> StripProblem {
    StripProblem {
        strip: strip.to_string(),
        message: message.into(),
    }
}

/// Builtin passthrough for a strip with nothing else in its graph.
fn copy_stage() -> DspStage {
    let [left, right] = ["copy_l", "copy_r"].map(String::from);
    DspStage {
        inputs: [format!("{left}:In"), format!("{right}:In")],
        outputs: [format!("{left}:Out"), format!("{right}:Out")],
        nodes: [left, right]
            .map(|name| DspNode {
                name,
                lsp: false,
                label: "copy",
                controls: Vec::new(),
            })
            .into(),
    }
}

/// A builtin `mixer` summing its inputs with `gains`.
fn mixer_node(name: &str, gains: &[f32]) -> DspNode {
    DspNode {
        name: name.to_string(),
        lsp: false,
        label: "mixer",
        controls: GAINS.iter().copied().zip(gains.iter().copied()).collect(),
    }
}

/// The filter graph: summed sources (hardware input), the chain, mono fold.
fn strip_graph(def: &StripDef) -> FilterGraph {
    let mut stages = dsp_stages(&def.chain);
    if stages.is_empty() && (def.trims.is_empty() || def.mono) {
        stages.push(copy_stage());
    }
    let mut graph = FilterGraph::chain(stages);
    if !def.trims.is_empty() {
        let n = def.trims.len();
        let ports: Vec<String> = (1..=n)
            .flat_map(|i| [format!("mix_l:In {i}"), format!("mix_r:In {i}")])
            .collect();
        let sum = ["mix_l:Out", "mix_r:Out"].map(String::from);
        if graph.nodes.is_empty() {
            graph.outputs = sum.to_vec();
        } else {
            graph
                .links
                .splice(0..0, sum.into_iter().zip(graph.inputs.clone()));
        }
        graph.inputs = ports;
        graph.nodes.splice(
            0..0,
            ["mix_l", "mix_r"].map(|name| mixer_node(name, &def.trims)),
        );
    }
    if def.mono {
        let (left, right) = (graph.outputs[0].clone(), graph.outputs[1].clone());
        for name in ["mono_l", "mono_r"] {
            graph.nodes.push(mixer_node(name, &[0.5, 0.5]));
            graph.links.push((left.clone(), format!("{name}:In 1")));
            graph.links.push((right.clone(), format!("{name}:In 2")));
        }
        graph.outputs = vec!["mono_l:Out".into(), "mono_r:Out".into()];
    }
    graph
}

/// What apps and PipeWire tools call a strip's nodes: `hako <label>`, unless
/// the label already starts with "hako".
fn describe(label: &str) -> String {
    if label
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("hako"))
    {
        label.to_string()
    } else {
        format!("hako {label}")
    }
}

/// The `pipewire -c` config of a strip. No side autoconnects (the daemon
/// makes every link) and none restores a stored volume (the fader owns it).
fn strip_config(def: &StripDef, naming: &Naming, lsp: Option<&Path>) -> Result<String> {
    def.chain.validate().map_err(anyhow::Error::msg)?;
    let common = format!(
        "node.latency = \"{}\" state.restore-props = false",
        node_latency(def.latency)
    );
    let unlinked = "node.autoconnect = false node.dont-reconnect = true node.dont-fallback = true";
    let description = describe(&def.label);
    let label = pw_escape(&description);
    let capture = if def.sink {
        format!(
            "node.name = \"{}\" node.description = \"{label}\" media.class = Audio/Sink {common}",
            pw_escape(&def.capture)
        )
    } else {
        let channels = def.capture_channels();
        format!(
            "node.name = \"{}\" node.description = \"{label}\" {unlinked} audio.channels = {} audio.position = [ {} ] {common}",
            pw_escape(&def.capture),
            channels.len(),
            channels.join(" ")
        )
    };
    let playback = if def.source {
        format!(
            "node.name = \"{}\" node.description = \"{label}\" media.class = Audio/Source {unlinked} {common}",
            pw_escape(&def.playback)
        )
    } else {
        format!(
            "node.name = \"{}\" node.description = \"{label}\" {unlinked} {common}",
            pw_escape(&def.playback)
        )
    };
    filter_chain_config(
        &description,
        &strip_graph(def),
        &naming.args,
        &capture,
        &playback,
        lsp,
    )
}

/// Every control a live update sets: the chain's, then the source trims.
fn strip_params(def: &StripDef) -> Vec<(String, f32)> {
    let mut params = effect_params(&def.chain);
    for (gain, trim) in GAINS.iter().zip(&def.trims) {
        for mix in ["mix_l", "mix_r"] {
            params.push((format!("{mix}:{gain}"), *trim));
        }
    }
    params
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Nothing changed.
    Keep,
    /// Parameters only (chain values, trims): set live.
    Update,
    /// New, or its topology changed: (re)built alone.
    Build,
}

/// What `reconcile` does: the running strips to remove, and a step for every
/// wanted strip.
fn plan(
    running: &BTreeMap<StripKey, StripDef>,
    wanted: &[(StripKey, StripDef)],
) -> (Vec<StripKey>, Vec<(StripKey, Step)>) {
    let remove = running
        .keys()
        .filter(|key| !wanted.iter().any(|(k, _)| k == *key))
        .cloned()
        .collect();
    let steps = wanted
        .iter()
        .map(|(key, def)| {
            let step = match running.get(key) {
                None => Step::Build,
                Some(old) if !same_topology(old, def) => Step::Build,
                Some(old) if old != def => Step::Update,
                Some(_) => Step::Keep,
            };
            (key.clone(), step)
        })
        .collect();
    (remove, steps)
}

/// Linear gain and mute of every fader. Inputs on a ChatMix side are scaled
/// by the dial with the curve legacy ChatMix used (so the dial feels the
/// same); while any input is soloed, the others are muted.
fn faders(scene: &Scene, dial: (f32, f32)) -> Vec<(StripKey, f32, bool)> {
    let solo = scene.inputs.iter().any(|i| i.solo);
    let inputs = scene.inputs.iter().map(|i: &Input| {
        let side = match i.chatmix {
            Some(ChatMixSide::Game) => wpctl_volume(dial.0),
            Some(ChatMixSide::Chat) => wpctl_volume(dial.1),
            None => 1.0,
        };
        (
            (Side::Input, i.id.clone()),
            db_to_gain(i.gain_db) * side,
            i.mute || (solo && !i.solo),
        )
    });
    let outputs = scene
        .outputs
        .iter()
        .map(|o: &Output| ((Side::Output, o.id.clone()), db_to_gain(o.gain_db), o.mute));
    inputs.chain(outputs).collect()
}

/// Left and right of a device's channels: FL/FR, else its mono channel for
/// both, else the first two (or only) channels it has.
fn pair(channels: &[&str]) -> Option<(String, String)> {
    let has = |c: &str| channels.contains(&c);
    match channels {
        _ if has("FL") && has("FR") => Some(("FL".into(), "FR".into())),
        _ if has("MONO") => Some(("MONO".into(), "MONO".into())),
        [] => None,
        [only] => Some((only.to_string(), only.to_string())),
        [left, right, ..] => Some((left.to_string(), right.to_string())),
    }
}

/// A sink with one channel for both sides (its strip folds to mono).
fn mono_sink(graph: &Graph, sink: &str) -> bool {
    let channels: Vec<&str> = graph
        .channels(sink, Direction::In)
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    pair(&channels).is_some_and(|(left, right)| left == right)
}

/// `(node, channel)`.
type End = (String, String);

fn end(node: &str, channel: &str) -> End {
    (node.to_string(), channel.to_string())
}

/// Links `scene` wants between running strips (`up`) and its devices, each
/// with the strip it belongs to; plus a problem per absent device.
fn wanted_links(
    scene: &Scene,
    up: &BTreeMap<StripKey, StripDef>,
    graph: &Graph,
) -> (Vec<(End, End, StripKey)>, Vec<StripProblem>) {
    let mut links = Vec::new();
    let mut problems = Vec::new();
    let stereo = |from: &str, to: &str, key: &StripKey, links: &mut Vec<_>| {
        for channel in ["FL", "FR"] {
            links.push((end(from, channel), end(to, channel), key.clone()));
        }
    };
    for input in &scene.inputs {
        let key = (Side::Input, input.id.clone());
        let Some(def) = up.get(&key) else {
            continue;
        };
        if let InputKind::Hardware { sources } = &input.kind {
            for (i, source) in sources.iter().enumerate() {
                let channels: Vec<&str> = graph
                    .channels(&source.node, Direction::Out)
                    .into_iter()
                    .map(|(_, c)| c)
                    .collect();
                let Some((left, right)) = pair(&channels) else {
                    let name = if source.description.is_empty() {
                        &source.node
                    } else {
                        &source.description
                    };
                    problems.push(problem(&input.id, format!("{name} is not connected")));
                    continue;
                };
                for (channel, aux) in [(left, 2 * i), (right, 2 * i + 1)] {
                    links.push((
                        end(&source.node, &channel),
                        end(&def.capture, &format!("AUX{aux}")),
                        key.clone(),
                    ));
                }
            }
        }
        for route in &input.routes {
            if let Some(out) = up.get(&(Side::Output, route.clone())) {
                stereo(&def.playback, &out.capture, &key, &mut links);
            }
        }
    }
    for output in &scene.outputs {
        let key = (Side::Output, output.id.clone());
        let (Some(def), OutputKind::Hardware { sink, .. }) = (up.get(&key), &output.kind) else {
            continue;
        };
        let channels: Vec<&str> = graph
            .channels(sink, Direction::In)
            .into_iter()
            .map(|(_, c)| c)
            .collect();
        match pair(&channels) {
            // One channel: PipeWire would sum both sides (+6 dB); the strip
            // folds to mono instead (`mono_sink`), one side is enough.
            Some((left, right)) if left == right => {
                links.push((end(&def.playback, "FL"), end(sink, &left), key.clone()));
            }
            Some((left, right)) => {
                for (from, to) in [("FL", left), ("FR", right)] {
                    links.push((end(&def.playback, from), end(sink, &to), key.clone()));
                }
            }
            None => problems.push(problem(&output.id, format!("{sink} is not connected"))),
        }
    }
    (links, problems)
}

/// A capture device or sink an input / output can use, with the card it
/// belongs to (`device.id`), so a headset's mic can be told from others.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub device: AudioDevice,
    pub card: Option<String>,
    /// Its ALSA card number, if it is an ALSA device.
    pub alsa_card: Option<String>,
}

/// Node names hako makes (its own prefixes) are never offered as devices,
/// except network peers' nodes (`hako_net.`): they are the network racks.
pub(crate) fn is_hako_node(name: &str) -> bool {
    !name.starts_with(hako_core::net::NODE_PREFIX)
        && ["hako_", "Hako_", "hako-"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Sources and sinks in `graph`, without hako's own nodes and `created`
/// (the scene's user-named virtual nodes).
pub fn device_list(graph: &Graph, created: &[&str]) -> Vec<DeviceInfo> {
    graph
        .nodes
        .values()
        .filter(|node| !is_hako_node(&node.name) && !created.contains(&node.name.as_str()))
        .filter_map(|node| {
            let (direction, ports) = match node.media_class.as_str() {
                "Audio/Source" => (DeviceDirection::Source, Direction::Out),
                "Audio/Sink" => (DeviceDirection::Sink, Direction::In),
                _ => return None,
            };
            let channels = node
                .props
                .get("audio.channels")
                .and_then(|c| c.parse().ok())
                .unwrap_or_else(|| graph.channels(&node.name, ports).len().min(255) as u8);
            let description = ["node.description", "node.nick"]
                .iter()
                .find_map(|key| node.props.get(*key))
                .unwrap_or(&node.name)
                .clone();
            Some(DeviceInfo {
                device: AudioDevice {
                    node: node.name.clone(),
                    description,
                    direction,
                    channels,
                    inputs: Vec::new(),
                    capture: false,
                    capture_now: None,
                },
                card: node.props.get("device.id").cloned(),
                alsa_card: alsa_card(&node.props),
            })
        })
        .collect()
}

/// The app a playback stream belongs to, as [`MixerStatus::apps`] lists it
/// (not hako's own streams, nor ones that asked not to be moved).
///
/// [`MixerStatus::apps`]: hako_core::mixer::MixerStatus::apps
fn app_name(node: &Node) -> Option<&str> {
    if node.media_class != "Stream/Output/Audio"
        || is_hako_node(&node.name)
        || node
            .props
            .get("node.dont-move")
            .is_some_and(|v| v == "true")
    {
        return None;
    }
    ["application.name", "application.process.binary"]
        .iter()
        .find_map(|key| node.props.get(*key))
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty())
}

/// Apps with a playback stream in `graph`, sorted, once each.
pub fn app_list(graph: &Graph) -> Vec<String> {
    let apps: BTreeSet<&str> = graph.nodes.values().filter_map(app_name).collect();
    apps.into_iter().map(str::to_string).collect()
}

/// [`app_list`] of the live graph.
pub fn apps() -> Result<Vec<String>> {
    Ok(engine()?.with_graph(app_list))
}

/// The live graph's devices (see [`device_list`]; capture devices with the
/// input selection and capture level of their ALSA card) and its default sink.
pub fn devices(created: &[&str]) -> Result<(Vec<DeviceInfo>, Option<String>)> {
    let (mut list, default_sink) =
        engine()?.with_graph(|graph| (device_list(graph, created), graph.default_sink.clone()));
    for info in &mut list {
        if let (DeviceDirection::Source, Some(card)) = (info.device.direction, &info.alsa_card) {
            (info.device.inputs, info.device.capture) = capture_controls(&info.device.node, card);
            if info.device.capture {
                info.device.capture_now = capture_level(&info.device.node, card);
            }
        }
    }
    Ok((list, default_sink))
}

/// A card's "Input Source" items and whether it has a capture level.
type CardControls = (Vec<String>, bool);

/// How long a card whose controls could not be read shows none before it is
/// asked again (`amixer` may take seconds to fail; status reads wait on it).
const CARD_RETRY: Duration = Duration::from_secs(30);

/// Controls read, or none until the instant after a failed read.
type CachedControls = (CardControls, Option<Instant>);

/// Asked once per (node, card); after a failure, again once `CARD_RETRY`
/// has passed.
/// ponytail: never evicted (a few entries per device ever plugged in).
static CARD_CONTROLS: Mutex<BTreeMap<(String, String), CachedControls>> =
    Mutex::new(BTreeMap::new());

fn capture_controls(node: &str, card: &str) -> CardControls {
    let mut cache = CARD_CONTROLS.lock().unwrap_or_else(|p| p.into_inner());
    let key = (node.to_string(), card.to_string());
    if let Some((found, retry)) = cache.get(&key) {
        if retry.is_none_or(|at| Instant::now() < at) {
            return found.clone();
        }
    }
    let (found, retry) = match card_capture_controls(card) {
        Ok(found) => (found, None),
        Err(e) => {
            warn!("mixer: ALSA card {card} ({node}): {e:#}");
            ((Vec::new(), false), Some(Instant::now() + CARD_RETRY))
        }
    };
    cache.insert(key, (found.clone(), retry));
    found
}

/// What the first-run layout is made from.
pub struct Detected<'a> {
    pub devices: &'a [DeviceInfo],
    /// The headset sink ChatMix plays into today (else the sink the old
    /// line-in played on, `aux.sink_name_regex`, is the headset).
    pub headset_sink: Option<&'a str>,
    pub default_sink: Option<&'a str>,
    /// The old line-in settings (capture regex, chain).
    pub aux: &'a AuxConfig,
    /// The old line-in was on.
    pub aux_enabled: bool,
    /// Its capture level (the headset's saved `aux_capture_percent`).
    pub capture_percent: Option<u8>,
}

/// The first layout, matching what ran without the mixer: Game and Chat as
/// `Hako_Game` / `Hako_Chat` on the ChatMix dial (apps routed there keep
/// working), the line-in with its chain (muted unless it was on), the
/// headset mic, all into the headset (A1); the mic also feeds `hako Mic` (B1).
/// A line-in that was on also selects its card's `Line` jack and capture
/// level; one that was off leaves the card alone (that jack may be a mic).
pub fn starter_scene(d: &Detected) -> Scene {
    let find = |direction, node: &str| {
        d.devices
            .iter()
            .find(|i| i.device.direction == direction && i.device.node == node)
    };
    let sinks = || {
        d.devices
            .iter()
            .filter(|i| i.device.direction == DeviceDirection::Sink)
    };
    let headset = d
        .headset_sink
        .and_then(|s| find(DeviceDirection::Sink, s))
        .or_else(|| {
            let re = Regex::new(&d.aux.sink_name_regex).ok()?;
            sinks().find(|i| re.is_match(&i.device.node))
        });
    let a1 = headset
        .or_else(|| d.default_sink.and_then(|s| find(DeviceDirection::Sink, s)))
        .or_else(|| sinks().next());
    let line_in = Regex::new(&d.aux.capture_name_regex).ok().and_then(|re| {
        d.devices
            .iter()
            .find(|i| i.device.direction == DeviceDirection::Source && re.is_match(&i.device.node))
    });
    let mic = a1.filter(|a1| a1.card.is_some()).and_then(|a1| {
        d.devices.iter().find(|i| {
            i.device.direction == DeviceDirection::Source
                && i.card == a1.card
                && Some(&i.device.node) != line_in.map(|l| &l.device.node)
        })
    });

    let mut outputs = Vec::new();
    if let Some(a1) = a1 {
        let label = if headset.is_some() {
            "Headset".to_string()
        } else {
            a1.device.description.chars().take(32).collect::<String>()
        };
        outputs.push(output(
            "A1",
            label.trim(),
            OutputKind::Hardware {
                sink: a1.device.node.clone(),
                description: a1.device.description.clone(),
            },
        ));
    }
    outputs.push(output(
        "B1",
        "hako Mic",
        OutputKind::Virtual {
            node_name: "Hako_Mic".into(),
        },
    ));
    let a1 = |routes: &[&str]| -> Vec<String> {
        routes
            .iter()
            .filter(|r| outputs.iter().any(|o| o.id == **r))
            .map(|r| r.to_string())
            .collect()
    };
    let virtual_input = |id: &str, label: &str, node: &str, side| Input {
        chatmix: Some(side),
        ..input(
            id,
            label,
            InputKind::Virtual {
                node_name: node.into(),
                apps: Vec::new(),
            },
            a1(&["A1"]),
        )
    };
    let mut inputs = vec![
        virtual_input("game", "Game", crate::CHATMIX_GAME, ChatMixSide::Game),
        virtual_input("chat", "Chat", crate::CHATMIX_CHAT, ChatMixSide::Chat),
    ];
    let source = |info: &DeviceInfo| Source {
        node: info.device.node.clone(),
        description: info.device.description.clone(),
        trim_db: 0.0,
        mute: false,
        alsa_input: None,
        capture_percent: None,
    };
    if let Some(line_in) = line_in {
        let (dev, on) = (&line_in.device, d.aux_enabled);
        let jack = Source {
            alsa_input: (on && dev.inputs.iter().any(|i| i == "Line")).then(|| "Line".into()),
            capture_percent: d.capture_percent.filter(|_| on && dev.capture),
            ..source(line_in)
        };
        let kind = InputKind::Hardware {
            sources: vec![jack],
        };
        inputs.push(Input {
            chain: d.aux.effects.clone(),
            mute: !on,
            ..input("line-in", "Line-in", kind, a1(&["A1"]))
        });
    }
    if let Some(mic) = mic {
        let kind = InputKind::Hardware {
            sources: vec![source(mic)],
        };
        inputs.push(input("mic", "Mic", kind, a1(&["B1"])));
    }
    Scene { inputs, outputs }
}

fn input(id: &str, label: &str, kind: InputKind, routes: Vec<String>) -> Input {
    Input {
        id: id.into(),
        label: label.into(),
        kind,
        chain: EffectChain::default(),
        gain_db: 0.0,
        mute: false,
        solo: false,
        routes,
        chatmix: None,
    }
}

fn output(id: &str, label: &str, kind: OutputKind) -> Output {
    Output {
        id: id.into(),
        label: label.into(),
        kind,
        chain: EffectChain::default(),
        gain_db: 0.0,
        mute: false,
        mono: false,
    }
}

/// A strip as it runs (or waits to be restarted).
struct Strip {
    def: StripDef,
    /// `None` while failed: restarted at `retry_at`.
    child: Option<Child>,
    conf: PathBuf,
    /// Fader last written; written again after every (re)build.
    fader: Option<(f32, bool)>,
    started: Instant,
    failures: u32,
    retry_at: Instant,
    error: Option<String>,
}

impl Strip {
    fn up(&self) -> bool {
        self.child.is_some()
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = stop_child(&mut child);
        }
        let _ = std::fs::remove_file(&self.conf);
    }

    fn fail(&mut self, error: String) {
        self.stop();
        self.failures += 1;
        let wait = Duration::from_secs(1 << (self.failures - 1).min(5)).min(MAX_RETRY);
        self.retry_at = Instant::now() + wait;
        warn!(
            "mixer strip {}: {error}; retrying in {wait:?}",
            self.def.playback
        );
        self.error = Some(error);
    }
}

struct OwnedLink {
    handle: LinkHandle,
    ports: (u32, u32),
    made: Instant,
}

/// An app's stream pointed at a virtual input's sink.
#[derive(Debug, Clone, PartialEq)]
struct Moved {
    /// `object.serial`: ids are reused, serials are not.
    serial: Option<String>,
    sink: String,
}

fn serial(node: &Node) -> Option<String> {
    node.props.get("object.serial").cloned()
}

/// The streams `scene`'s running virtual inputs (`up`: their ids) take:
/// stream id -> (its target, the input).
fn wanted_apps(scene: &Scene, up: &[&str], graph: &Graph) -> BTreeMap<u32, (Moved, String)> {
    let mut sinks = HashMap::new();
    for input in scene.inputs.iter().filter(|i| up.contains(&i.id.as_str())) {
        if let InputKind::Virtual { node_name, apps } = &input.kind {
            for app in apps {
                sinks.insert(app.as_str(), (node_name, &input.id));
            }
        }
    }
    graph
        .nodes
        .iter()
        .filter_map(|(&id, node)| {
            let (sink, input) = sinks.get(app_name(node)?)?;
            let moved = Moved {
                serial: serial(node),
                sink: sink.to_string(),
            };
            Some((id, (moved, input.to_string())))
        })
        .collect()
}

/// `(Source::alsa_input, Source::capture_percent)`.
type CardWant = (Option<String>, Option<u8>);

/// What a source wants of its capture card: `(node, input id, settings)`,
/// the first source of each node with any (two inputs can't both set one).
fn wanted_cards(scene: &Scene) -> Vec<(String, String, CardWant)> {
    let mut wanted: Vec<(String, String, CardWant)> = Vec::new();
    for input in &scene.inputs {
        let InputKind::Hardware { sources } = &input.kind else {
            continue;
        };
        for s in sources {
            let want = (s.alsa_input.clone(), s.capture_percent);
            if want != (None, None) && !wanted.iter().any(|(node, ..)| *node == s.node) {
                wanted.push((s.node.clone(), input.id.clone(), want));
            }
        }
    }
    wanted
}

/// A capture card a source's settings were applied to.
#[derive(Debug)]
struct CardClaim {
    /// The input the source belongs to (its problems show there).
    input: String,
    card: String,
    /// The node's PipeWire id then: a new one (a replug, the card reset, a
    /// PipeWire restart) gets the settings again.
    node_id: u32,
    applied: CardWant,
    /// "Input Source" before hako changed it; restored when let go. Kept
    /// while the node is on the same card (ALSA keeps hako's selection
    /// across a PipeWire restart).
    previous: Option<String>,
    error: Option<String>,
}

/// Put the card's input selection back, unless the node is gone or on
/// another card (its card number may be another device's by now).
fn release_card(node: &str, claim: &CardClaim, present: bool) {
    let (Some(previous), true) = (&claim.previous, present) else {
        return;
    };
    match set_input_source(&claim.card, previous) {
        Ok(()) => info!(
            "mixer: {node}: card {} input source back to {previous}",
            claim.card
        ),
        Err(e) => warn!("mixer: {node}: restoring card {} input: {e:#}", claim.card),
    }
}

/// The mixer as it runs. Dropping it stops every strip.
pub struct MixerGraph {
    naming: Naming,
    latency: u32,
    strips: BTreeMap<StripKey, Strip>,
    links: HashMap<(End, End), OwnedLink>,
    /// Capture cards set for sources, by node name.
    cards: BTreeMap<String, CardClaim>,
    /// App streams moved into virtual inputs, by stream id.
    moved: BTreeMap<u32, Moved>,
    problems: Vec<StripProblem>,
    running: bool,
}

impl MixerGraph {
    /// `latency`: `node.latency` of every node, in samples at 48 kHz.
    pub fn new(naming: Naming, latency: u32) -> Self {
        Self {
            naming,
            latency,
            strips: BTreeMap::new(),
            links: HashMap::new(),
            cards: BTreeMap::new(),
            moved: BTreeMap::new(),
            problems: Vec::new(),
            running: false,
        }
    }

    /// Takes effect at the next `reconcile` (every strip is rebuilt).
    pub fn set_latency(&mut self, samples: u32) {
        self.latency = samples;
    }

    /// The last `reconcile` built the scene (PipeWire was reachable).
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn problems(&self) -> &[StripProblem] {
        &self.problems
    }

    /// No strip, and no app stream left to hand back.
    pub fn is_empty(&self) -> bool {
        self.strips.is_empty() && self.moved.is_empty()
    }

    /// The post-fader node of a running strip (a meter captures its output).
    pub fn meter_node(&self, key: &StripKey) -> Option<String> {
        self.strips
            .get(key)
            .filter(|s| s.up())
            .map(|s| s.def.playback.clone())
    }

    /// Node names the strips make (running or not).
    pub fn node_names(&self) -> Vec<&str> {
        self.strips
            .values()
            .flat_map(|s| [s.def.capture.as_str(), s.def.playback.as_str()])
            .collect()
    }

    /// Ids of the links the graph made.
    pub fn link_ids(&self) -> Vec<u32> {
        self.links.values().map(|l| l.handle.id).collect()
    }

    /// Fails if a strip's process has exited.
    pub fn ensure_running(&mut self) -> Result<()> {
        for strip in self.strips.values_mut() {
            if let Some(child) = strip.child.as_mut() {
                crate::ensure_running(child, &strip.def.playback)?;
            }
        }
        Ok(())
    }

    /// Bring the graph in line with `scene` (healing it when called again
    /// with the same scene); `dial` is the ChatMix dial (game, chat).
    pub fn reconcile(&mut self, scene: &Scene, dial: (f32, f32)) {
        let engine = engine();
        let (wanted, mut problems) = match &engine {
            Ok(engine) => engine.with_graph(|graph| {
                strip_defs(scene, &self.naming, self.latency, |sink| {
                    mono_sink(graph, sink)
                })
            }),
            Err(_) => strip_defs(scene, &self.naming, self.latency, |_| false),
        };
        let engine = match engine {
            Ok(engine) => engine,
            Err(error) => {
                self.running = false;
                self.problems = wanted
                    .iter()
                    .map(|((_, id), _)| problem(id, format!("{error:#}")))
                    .chain(problems)
                    .collect();
                return;
            }
        };
        let now = Instant::now();
        for strip in self.strips.values_mut() {
            let exited = strip
                .child
                .as_mut()
                .is_some_and(|child| !matches!(child.try_wait(), Ok(None)));
            if exited {
                if strip.started.elapsed() >= STABLE {
                    strip.failures = 0;
                }
                strip.fail("stopped unexpectedly".into());
            }
        }

        let running = self
            .strips
            .iter()
            .map(|(key, strip)| (key.clone(), strip.def.clone()))
            .collect();
        let (remove, steps) = plan(&running, &wanted);
        for key in remove {
            if let Some(mut strip) = self.strips.remove(&key) {
                info!("mixer: removing {}", strip.def.playback);
                strip.stop();
            }
        }
        let mut build = Vec::new();
        for ((key, step), (_, def)) in steps.into_iter().zip(wanted) {
            let strip = self.strips.get_mut(&key);
            match (step, strip) {
                (Step::Build, _) | (_, None) => build.push((key, def)),
                (_, Some(strip)) if !strip.up() => {
                    strip.def = def;
                    if now >= strip.retry_at {
                        let def = strip.def.clone();
                        build.push((key, def));
                    }
                }
                (Step::Update, Some(strip)) => {
                    let params = strip_params(&def);
                    let result = engine
                        .with_graph(|g| g.node_id(&def.capture))
                        .ok_or_else(|| anyhow::anyhow!("{} is gone", def.capture))
                        .and_then(|id| engine.set_props(id, params_pod(&params)?));
                    // Kept as it was on failure: the next heal tries again.
                    match result {
                        Ok(()) => strip.def = def,
                        Err(error) => {
                            problems.push(problem(&key.1, format!("live update: {error:#}")))
                        }
                    }
                }
                (Step::Keep, Some(_)) => {}
            }
        }
        self.build(engine, build);

        for (key, gain, mute) in faders(scene, dial) {
            let Some(strip) = self.strips.get_mut(&key).filter(|s| s.up()) else {
                continue;
            };
            if strip.fader == Some((gain, mute)) {
                continue;
            }
            let node = strip.def.fader_node(key.0);
            let result = engine
                .with_graph(|g| g.node_id(node))
                .ok_or_else(|| anyhow::anyhow!("{node} is gone"))
                .and_then(|id| engine.set_props(id, fader_pod(gain, mute, 2)?));
            match result {
                Ok(()) => strip.fader = Some((gain, mute)),
                Err(error) => problems.push(problem(&key.1, format!("fader: {error:#}"))),
            }
        }

        self.sync_links(engine, scene, &mut problems);
        self.sync_apps(engine, scene, &mut problems);
        let wanted = wanted_cards(scene);
        let present: HashMap<String, (u32, String)> = engine.with_graph(|graph| {
            wanted
                .iter()
                .map(|(node, ..)| node)
                .chain(self.cards.keys())
                .filter_map(|node| {
                    let id = graph.node_id(node)?;
                    Some((node.clone(), (id, alsa_card(&graph.nodes[&id].props)?)))
                })
                .collect()
        });
        problems.extend(self.sync_cards(&wanted, &present));
        problems.extend(self.strips.iter().filter_map(|((_, id), strip)| {
            strip.error.as_ref().map(|error| problem(id, error.clone()))
        }));
        self.problems = problems;
        self.running = true;
    }

    /// Stop the strips in `build` that run, then start them all with their
    /// new definitions; each failure is that strip's alone.
    fn build(&mut self, engine: &Engine, build: Vec<(StripKey, StripDef)>) {
        if build.is_empty() {
            return;
        }
        let mut failures = HashMap::new();
        for (key, _) in &build {
            if let Some(mut strip) = self.strips.remove(key) {
                strip.stop();
                failures.insert(key.clone(), (strip.failures, strip.started));
            }
        }
        // Two nodes of one name break routing: what a stopped strip, legacy
        // ChatMix or a crashed daemon made must be gone first.
        let names: Vec<&str> = build
            .iter()
            .flat_map(|(_, def)| [def.capture.as_str(), def.playback.as_str()])
            .collect();
        let taken = |graph: &Graph| -> Vec<String> {
            names
                .iter()
                .filter(|name| graph.node_id(name).is_some())
                .map(|name| name.to_string())
                .collect()
        };
        let _ = engine.wait_for(NAME_WAIT, |graph| Ok(taken(graph).is_empty()));
        let taken = engine.with_graph(taken);

        let mut started = Vec::new();
        for (key, def) in build {
            let side = match key.0 {
                Side::Input => "in",
                Side::Output => "out",
            };
            let conf = hako_core::default_socket_path()
                .with_file_name(format!("{}mixer-{side}-{}.conf", self.naming.files, key.1));
            let (failures, since) = failures.remove(&key).unwrap_or((0, Instant::now()));
            let mut strip = Strip {
                def,
                child: None,
                conf,
                fader: None,
                started: since,
                failures,
                retry_at: Instant::now(),
                error: None,
            };
            let clash = [&strip.def.capture, &strip.def.playback]
                .into_iter()
                .find(|name| taken.contains(name));
            let spawned = match clash {
                Some(name) => Err(anyhow::anyhow!(
                    "PipeWire node {name} already exists (another program's?)"
                )),
                None => lsp_for(&strip.def.chain)
                    .and_then(|lsp| strip_config(&strip.def, &self.naming, lsp.as_deref()))
                    .and_then(|config| write_conf(&strip.conf, &config))
                    .and_then(|()| {
                        // A crashed daemon's child with this config.
                        pkill(&format!(
                            "^pipewire -c {}$",
                            regex::escape(&strip.conf.to_string_lossy())
                        ));
                        spawn_pipewire(&strip.conf)
                    }),
            };
            match spawned {
                Ok(child) => {
                    strip.child = Some(child);
                    strip.started = Instant::now();
                    started.push(key.clone());
                }
                Err(error) => strip.fail(format!("{error:#}")),
            }
            self.strips.insert(key, strip);
        }

        let strips = &mut self.strips;
        let mut ready = Vec::new();
        let _ = engine.wait_for(NODE_WAIT, |graph| {
            ready.clear();
            let mut pending = false;
            for key in &started {
                let strip = strips.get_mut(key).expect("started strip");
                let exited = strip
                    .child
                    .as_mut()
                    .is_none_or(|c| !matches!(c.try_wait(), Ok(None)));
                if exited {
                    continue;
                }
                let inputs = graph.channels(&strip.def.capture, Direction::In).len();
                let outputs = graph.channels(&strip.def.playback, Direction::Out).len();
                if inputs >= strip.def.capture_channels().len() && outputs >= 2 {
                    ready.push(key.clone());
                } else {
                    pending = true;
                }
            }
            Ok(!pending)
        });
        for key in started {
            let strip = strips.get_mut(&key).expect("started strip");
            if ready.contains(&key) {
                info!("mixer: {} up", strip.def.playback);
                continue;
            }
            let exited = strip
                .child
                .as_mut()
                .is_none_or(|c| !matches!(c.try_wait(), Ok(None)));
            strip.fail(if exited {
                "its PipeWire process exited at start".into()
            } else {
                format!("{} did not appear", strip.def.playback)
            });
        }
    }

    /// Make the links the scene wants that are missing (a device appeared, a
    /// strip was rebuilt) and drop the rest.
    fn sync_links(&mut self, engine: &Engine, scene: &Scene, problems: &mut Vec<StripProblem>) {
        let up = self.linkable();
        let (wanted, missing, alive) = engine.with_graph(|graph| {
            let (links, missing) = wanted_links(scene, &up, graph);
            let resolved: Vec<_> = links
                .into_iter()
                .map(|(from, to, key)| {
                    let ports = graph
                        .channel_port(&from.0, &from.1, Direction::Out)
                        .zip(graph.channel_port(&to.0, &to.1, Direction::In));
                    ((from, to), key, ports)
                })
                .collect();
            let alive: Vec<u32> = self
                .links
                .values()
                .filter(|l| {
                    graph
                        .links
                        .get(&l.handle.id)
                        .is_some_and(|g| (g.output_port, g.input_port) == l.ports)
                })
                .map(|l| l.handle.id)
                .collect();
            (resolved, missing, alive)
        });
        problems.extend(missing);
        self.links.retain(|ends, link| {
            let still = wanted.iter().any(|(e, _, ports)| {
                e == ends && ports.is_some_and(|(o, i)| (o.1, i.1) == link.ports)
            });
            still && (alive.contains(&link.handle.id) || link.made.elapsed() < LINK_GRACE)
        });
        for (ends, key, ports) in wanted {
            let Some((output, input)) = ports else {
                continue;
            };
            if self.links.contains_key(&ends) {
                continue;
            }
            match engine.link(output, input) {
                Ok(handle) => {
                    self.links.insert(
                        ends,
                        OwnedLink {
                            handle,
                            ports: (output.1, input.1),
                            made: Instant::now(),
                        },
                    );
                }
                Err(error) => problems.push(problem(&key.1, format!("link: {error:#}"))),
            }
        }
    }

    /// Strips that may be linked: running, with their fader written (a
    /// fresh one plays at unity until then, whatever its fader says).
    fn linkable(&self) -> BTreeMap<StripKey, StripDef> {
        self.strips
            .iter()
            .filter(|(_, s)| s.up() && s.fader.is_some())
            .map(|(k, s)| (k.clone(), s.def.clone()))
            .collect()
    }

    /// Point the streams of the running virtual inputs' apps at their sinks
    /// (new streams, a strip that came back); hand back the ones no input
    /// takes any more.
    fn sync_apps(&mut self, engine: &Engine, scene: &Scene, problems: &mut Vec<StripProblem>) {
        let up: Vec<&str> = self
            .strips
            .iter()
            .filter(|((side, _), s)| *side == Side::Input && s.up())
            .map(|((_, id), _)| id.as_str())
            .collect();
        let (wanted, release) = engine.with_graph(|graph| {
            let wanted = wanted_apps(scene, &up, graph);
            let release: Vec<u32> = self
                .moved
                .iter()
                .filter(|(id, m)| wanted.get(id).is_none_or(|(w, _)| w != *m))
                .filter(|(id, m)| graph.nodes.get(id).is_some_and(|n| serial(n) == m.serial))
                .map(|(&id, _)| id)
                .collect();
            (wanted, release)
        });
        let mut failed = Vec::new();
        for id in release {
            if let Err(e) = engine.set_target(id, None) {
                warn!("mixer: handing back stream {id}: {e:#}");
                failed.push(id);
            }
        }
        // A stream no input takes that could not be handed back is tried again.
        self.moved.retain(|id, m| match wanted.get(id) {
            Some((w, _)) => w == m,
            None => failed.contains(id),
        });
        for (id, (moved, input)) in wanted {
            if self.moved.contains_key(&id) {
                continue;
            }
            match engine.set_target(id, Some(&moved.sink)) {
                Ok(()) => {
                    self.moved.insert(id, moved);
                }
                Err(e) => problems.push(problem(&input, format!("moving an app here: {e:#}"))),
            }
        }
    }

    /// Give every moved stream back to WirePlumber. The ones that could not
    /// be are kept (`is_empty` is false), for the next teardown.
    fn release_apps(&mut self) {
        if self.moved.is_empty() {
            return;
        }
        let Ok(engine) = engine() else {
            return;
        };
        let moved = std::mem::take(&mut self.moved);
        for (id, m) in moved {
            let there =
                engine.with_graph(|g| g.nodes.get(&id).is_some_and(|n| serial(n) == m.serial));
            if !there {
                continue;
            }
            if let Err(e) = engine.set_target(id, None) {
                warn!("mixer: handing back stream {id}: {e:#}");
                self.moved.insert(id, m);
            }
        }
    }

    /// Apply the capture card settings sources want (`present`: node ->
    /// (PipeWire id, ALSA card) of the nodes there), give back the cards no
    /// source wants any more. Each change runs `amixer` once; a failure is
    /// the input's problem until the settings or the node change.
    fn sync_cards(
        &mut self,
        wanted: &[(String, String, CardWant)],
        present: &HashMap<String, (u32, String)>,
    ) -> Vec<StripProblem> {
        let gone: Vec<String> = self
            .cards
            .keys()
            .filter(|node| !wanted.iter().any(|(n, ..)| n == *node))
            .cloned()
            .collect();
        for node in gone {
            if let Some(claim) = self.cards.remove(&node) {
                let there = present
                    .get(&node)
                    .is_some_and(|(_, card)| *card == claim.card);
                release_card(&node, &claim, there);
            }
        }
        let mut problems = Vec::new();
        for (node, input, want) in wanted {
            // Absent, or not an ALSA device: applied once it is there.
            let Some((id, card)) = present.get(node) else {
                continue;
            };
            // The settings are the card's: one node holds them (a second
            // would take the first one's input for the card's own).
            if let Some((other, claim)) = self.cards.iter().find(|(other, claim)| {
                *other != node
                    && claim.card == *card
                    && present.get(*other).is_some_and(|(_, c)| c == card)
            }) {
                if claim.applied != *want {
                    problems.push(problem(input, format!("its card is set for {other}")));
                }
                continue;
            }
            let current = self.cards.get(node);
            if let Some(c) = current.filter(|c| c.node_id == *id && c.applied == *want) {
                problems.extend(c.error.as_ref().map(|e| problem(&c.input, e.clone())));
                continue;
            }
            // A new node id may be a reset card: everything is applied again.
            // The card's own input is still the one from before hako's (a
            // PipeWire restart leaves hako's selection on the card).
            let (applied, mut previous) = match self.cards.remove(node) {
                Some(c) if c.card == *card && c.node_id == *id => (c.applied, c.previous),
                Some(c) if c.card == *card => ((None, None), c.previous),
                _ => ((None, None), None),
            };
            let mut errors = Vec::new();
            if applied.0 != want.0 || (want.0.is_none() && previous.is_some()) {
                let result = match &want.0 {
                    // Never switched without knowing what to switch back to.
                    Some(item) => match previous.clone() {
                        Some(original) => Ok(original),
                        None => current_input_source(card).context("reading its current input"),
                    }
                    .and_then(|original| {
                        previous = Some(original);
                        if capture_controls(node, card).0.contains(item) {
                            set_input_source(card, item)
                        } else {
                            Err(anyhow!("the card has no input source {item:?}"))
                        }
                    }),
                    None => match previous.clone() {
                        Some(back) => set_input_source(card, &back).map(|()| previous = None),
                        None => Ok(()),
                    },
                };
                if let Err(e) = result {
                    errors.push(format!("input source: {e:#}"));
                }
            }
            if applied.1 != want.1 {
                if let Some(Err(e)) = want.1.map(|p| set_capture_percent(card, p)) {
                    errors.push(format!("capture level: {e:#}"));
                }
            }
            let error = (!errors.is_empty()).then(|| errors.join("; "));
            match &error {
                Some(e) => {
                    warn!("mixer: {node}: card {card}: {e}");
                    problems.push(problem(input, e.clone()));
                }
                None => info!(
                    "mixer: {node}: card {card} input {:?}, capture {:?}",
                    want.0, want.1
                ),
            }
            self.cards.insert(
                node.clone(),
                CardClaim {
                    input: input.clone(),
                    card: card.clone(),
                    node_id: *id,
                    applied: want.clone(),
                    previous,
                    error,
                },
            );
        }
        problems
    }

    /// Give every claimed card its input selection back: by card number
    /// (`amixer`), while its node is still on that card or PipeWire can't
    /// say (the card is most likely the same one).
    fn release_cards(&mut self) {
        if self.cards.is_empty() {
            return;
        }
        let cards: Option<HashMap<String, String>> = engine().ok().map(|engine| {
            engine.with_graph(|graph| {
                self.cards
                    .keys()
                    .filter_map(|node| {
                        let id = graph.node_id(node)?;
                        Some((node.clone(), alsa_card(&graph.nodes.get(&id)?.props)?))
                    })
                    .collect()
            })
        });
        for (node, claim) in std::mem::take(&mut self.cards) {
            let there = cards
                .as_ref()
                .is_none_or(|cards| cards.get(&node) == Some(&claim.card));
            release_card(&node, &claim, there);
        }
    }

    /// Stop every strip and wait (briefly) until their nodes are gone, so
    /// whatever comes next may reuse the names. Claimed cards get their
    /// input selection back.
    pub fn teardown(&mut self) {
        self.release_cards();
        self.release_apps();
        self.links.clear();
        let names: Vec<String> = self.node_names().iter().map(|n| n.to_string()).collect();
        for strip in self.strips.values_mut() {
            strip.stop();
        }
        self.strips.clear();
        self.problems.clear();
        self.running = false;
        if let Ok(engine) = engine() {
            let _ = engine.wait_for(NAME_WAIT, |graph| {
                Ok(names.iter().all(|name| graph.node_id(name).is_none()))
            });
        }
    }
}

impl Drop for MixerGraph {
    fn drop(&mut self) {
        self.release_cards();
        self.release_apps();
        self.links.clear();
        for strip in self.strips.values_mut() {
            strip.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Node, Port};
    use hako_core::ipc::{AuxEffect, AuxEffectKind};

    fn test_naming() -> Naming {
        Naming {
            nodes: "hako-test-x-".into(),
            files: "hako-test-x-".into(),
            args: String::new(),
        }
    }

    fn source(node: &str) -> Source {
        Source {
            node: node.into(),
            description: String::new(),
            trim_db: 0.0,
            mute: false,
            alsa_input: None,
            capture_percent: None,
        }
    }

    fn scene() -> Scene {
        Scene {
            inputs: vec![
                Input {
                    chatmix: Some(ChatMixSide::Game),
                    ..input(
                        "game",
                        "Game",
                        InputKind::Virtual {
                            node_name: "hako-test-game".into(),
                            apps: Vec::new(),
                        },
                        vec!["A1".into()],
                    )
                },
                input(
                    "desk",
                    "Desk \"mic\"",
                    InputKind::Hardware {
                        sources: vec![source("mic-a"), source("mic-b")],
                    },
                    vec!["A1".into(), "B1".into()],
                ),
            ],
            outputs: vec![
                output(
                    "A1",
                    "Headset",
                    OutputKind::Hardware {
                        sink: "headset".into(),
                        description: String::new(),
                    },
                ),
                output(
                    "B1",
                    "hako Mic",
                    OutputKind::Virtual {
                        node_name: "hako-test-mic".into(),
                    },
                ),
            ],
        }
    }

    fn defs(scene: &Scene) -> Vec<(StripKey, StripDef)> {
        strip_defs(scene, &test_naming(), 256, |_| false).0
    }

    fn def(scene: &Scene, side: Side, id: &str) -> StripDef {
        defs(scene)
            .into_iter()
            .find(|(k, _)| *k == (side, id.to_string()))
            .unwrap()
            .1
    }

    fn gate() -> EffectChain {
        EffectChain {
            effects: vec![AuxEffect {
                id: "gate".into(),
                enabled: true,
                kind: AuxEffectKind::default_gate(),
            }],
        }
    }

    fn lsp() -> Option<&'static Path> {
        Some(Path::new("/usr/lib/ladspa/lsp-plugins-ladspa.so"))
    }

    #[test]
    fn virtual_input_is_a_sink_apps_pick_by_label() {
        let config =
            strip_config(&def(&scene(), Side::Input, "game"), &test_naming(), None).unwrap();
        assert!(config.contains(
            "capture.props = { node.name = \"hako-test-game\" node.description = \"hako Game\" media.class = Audio/Sink node.latency = \"256/48000\" state.restore-props = false }"
        ), "{config}");
        assert!(config.contains(
            "playback.props = { node.name = \"hako-test-x-in_game\" node.description = \"hako Game\" node.autoconnect = false"
        ), "{config}");
        // No chain: a builtin passthrough.
        assert!(config.contains("inputs = [ \"copy_l:In\" \"copy_r:In\" ]"));
        assert!(config.contains("outputs = [ \"copy_l:Out\" \"copy_r:Out\" ]"));
    }

    #[test]
    fn hardware_input_sums_its_sources_with_their_trims() {
        let mut scene = scene();
        let InputKind::Hardware { sources } = &mut scene.inputs[1].kind else {
            unreachable!()
        };
        sources[0].trim_db = -20.0;
        sources[1].mute = true;
        scene.inputs[1].chain = gate();
        let desk = def(&scene, Side::Input, "desk");
        assert_eq!(desk.capture, "hako-test-x-in_desk.capture");
        assert_eq!(desk.capture_channels(), ["AUX0", "AUX1", "AUX2", "AUX3"]);
        let config = strip_config(&desk, &test_naming(), lsp()).unwrap();
        assert!(config.contains(
            "{ type = builtin name = \"mix_l\" label = \"mixer\" control = { \"Gain 1\" = 0.1 \"Gain 2\" = 0 } }"
        ), "{config}");
        assert!(config
            .contains("inputs = [ \"mix_l:In 1\" \"mix_r:In 1\" \"mix_l:In 2\" \"mix_r:In 2\" ]"));
        assert!(config.contains("{ output = \"mix_l:Out\" input = \"fx0:Input L\" }"));
        assert!(config.contains("{ output = \"mix_r:Out\" input = \"fx0:Input R\" }"));
        assert!(config.contains("outputs = [ \"fx0:Output L\" \"fx0:Output R\" ]"));
        assert!(config.contains("node.autoconnect = false node.dont-reconnect = true node.dont-fallback = true audio.channels = 4 audio.position = [ AUX0 AUX1 AUX2 AUX3 ]"));
        // The description is escaped, the label shown as `hako <label>`.
        assert!(config.contains("node.description = \"hako Desk \\\"mic\\\"\""));
        assert!(!config.contains("copy_l"), "the mixer needs no passthrough");
        // Live parameters reach the same controls.
        let params = strip_params(&desk);
        assert!(params.contains(&("mix_r:Gain 2".into(), 0.0)));
        assert!(params
            .iter()
            .any(|(k, _)| k == "fx0:Ratio" || k == "fx0:Attack (ms)"));
    }

    #[test]
    fn outputs_capture_privately_and_play_to_a_device_or_as_a_mic() {
        let scene = scene();
        let a1 = strip_config(&def(&scene, Side::Output, "A1"), &test_naming(), None).unwrap();
        assert!(a1.contains("capture.props = { node.name = \"hako-test-x-out_A1\" node.description = \"hako Headset\" node.autoconnect = false"), "{a1}");
        assert!(a1.contains("playback.props = { node.name = \"hako-test-x-out_A1.play\""));
        assert!(!a1.contains("media.class"));
        let b1 = strip_config(&def(&scene, Side::Output, "B1"), &test_naming(), None).unwrap();
        assert!(b1.contains(
            "playback.props = { node.name = \"hako-test-mic\" node.description = \"hako Mic\" media.class = Audio/Source"
        ), "{b1}");
        // Every side of every strip requests the latency setting.
        assert_eq!(b1.matches("node.latency = \"256/48000\"").count(), 2);
        // A label that says hako already is not prefixed again.
        assert!(
            b1.contains("node.description = \"hako Mic\" node.autoconnect"),
            "{b1}"
        );
        assert!(!b1.contains("hako hako"), "{b1}");
    }

    #[test]
    fn node_descriptions_say_hako_once() {
        assert_eq!(describe("Game"), "hako Game");
        assert_eq!(describe("hako Mic"), "hako Mic");
        assert_eq!(describe("HAKO Stream"), "HAKO Stream");
        assert_eq!(describe("Hakone mic"), "Hakone mic", "starts with hako");
        assert_eq!(describe("hak"), "hako hak");
        assert_eq!(describe("hä"), "hako hä", "no char boundary at 4");
    }

    /// Card settings run `amixer` once per change (the fake one, card 7:
    /// Rear Mic / Front Mic / Line, now on Rear Mic), come back after a
    /// replug, and the old input returns when the source lets go.
    #[test]
    fn capture_cards_are_set_once_and_given_back() {
        use crate::tests::amixer_calls;
        let node = "hako-test-cards-line".to_string();
        let mut graph = MixerGraph::new(test_naming(), 256);
        let want = |item: Option<&str>, percent| {
            vec![(
                node.clone(),
                "line-in".to_string(),
                (item.map(str::to_string), percent),
            )]
        };
        let at = |id| HashMap::from([(node.clone(), (id, "7".to_string()))]);
        amixer_calls();

        assert!(graph
            .sync_cards(&want(Some("Line"), Some(66)), &at(5))
            .is_empty());
        let calls = amixer_calls();
        assert_eq!(
            calls.first().map(String::as_str),
            Some("7 sget Input Source")
        );
        assert!(calls.ends_with(&[
            "7 sset Input Source Line".to_string(),
            "7 sset Capture 66%".to_string()
        ]));
        // Nothing changed: nothing runs.
        graph.sync_cards(&want(Some("Line"), Some(66)), &at(5));
        assert!(amixer_calls().is_empty());
        // A level change touches the level only.
        graph.sync_cards(&want(Some("Line"), Some(50)), &at(5));
        assert_eq!(amixer_calls(), ["7 sset Capture 50%"]);
        // An item the card doesn't have is the input's problem, not a write.
        let problems = graph.sync_cards(&want(Some("Aux"), Some(50)), &at(5));
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].strip, "line-in");
        assert!(!amixer_calls().iter().any(|c| c.contains("sset")));
        // Still reported while nothing changes, without running amixer.
        assert_eq!(
            graph.sync_cards(&want(Some("Aux"), Some(50)), &at(5)).len(),
            1
        );
        assert!(amixer_calls().is_empty());
        graph.sync_cards(&want(Some("Line"), Some(50)), &at(5));
        assert_eq!(amixer_calls(), ["7 sset Input Source Line"]);
        // A replug (new node id) resets the card: applied again. The card's
        // own input is not read again (after a PipeWire restart it reads
        // hako's "Line").
        graph.sync_cards(&want(Some("Line"), Some(50)), &at(6));
        let calls = amixer_calls();
        assert!(
            calls.contains(&"7 sset Input Source Line".to_string()),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| c.contains("sget")), "{calls:?}");
        assert!(
            calls.contains(&"7 sset Capture 50%".to_string()),
            "{calls:?}"
        );
        // The source lets go of the input: the card's own comes back.
        graph.sync_cards(&want(None, Some(50)), &at(6));
        assert_eq!(amixer_calls(), ["7 sset Input Source Rear Mic"]);
        graph.sync_cards(&want(Some("Line"), Some(50)), &at(6));
        amixer_calls();
        // Removed while there: restored, under a new node id too (same
        // card); removed while gone: untouched.
        graph.sync_cards(&[], &at(8));
        assert_eq!(amixer_calls(), ["7 sset Input Source Rear Mic"]);
        graph.sync_cards(&want(Some("Line"), None), &at(6));
        amixer_calls();
        graph.sync_cards(&[], &HashMap::new());
        assert!(amixer_calls().is_empty());
        assert!(graph.cards.is_empty());
        // Not there yet: applied once it appears.
        graph.sync_cards(&want(Some("Line"), None), &HashMap::new());
        assert!(amixer_calls().is_empty() && graph.cards.is_empty());

        // A card whose input can't be read is not switched (nothing to
        // switch back to): the input's problem.
        // Card 5 offers "Line" (cached) but fails every read.
        CARD_CONTROLS.lock().unwrap().insert(
            (node.clone(), "5".to_string()),
            ((vec!["Line".into()], false), None),
        );
        let unreadable = HashMap::from([(node.clone(), (5, "5".to_string()))]);
        let problems = graph.sync_cards(&want(Some("Line"), None), &unreadable);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].message.contains("current input"),
            "{problems:?}"
        );
        assert_eq!(amixer_calls(), ["5 sget Input Source"]);
        graph.sync_cards(&[], &unreadable);
        assert!(amixer_calls().is_empty(), "nothing to give back");
    }

    /// Two capture nodes of one card: the first to claim it sets it, the
    /// other leaves it alone (it would take the first one's input for the
    /// card's own and give that back last).
    #[test]
    fn one_node_sets_a_card() {
        use crate::tests::amixer_calls;
        let (a, b) = (
            "hako-test-card-a".to_string(),
            "hako-test-card-b".to_string(),
        );
        let mut graph = MixerGraph::new(test_naming(), 256);
        let line = |node: &String, item: &str| {
            let want = (Some(item.to_string()), None);
            (node.clone(), format!("in-{node}"), want)
        };
        let both = HashMap::from([
            (a.clone(), (5, "7".to_string())),
            (b.clone(), (6, "7".to_string())),
        ]);
        graph.sync_cards(&[line(&a, "Line")], &both);
        amixer_calls();
        let problems = graph.sync_cards(&[line(&a, "Line"), line(&b, "Line")], &both);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(amixer_calls().is_empty());
        let problems = graph.sync_cards(&[line(&a, "Line"), line(&b, "Front Mic")], &both);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].strip, format!("in-{b}"));
        assert!(amixer_calls().is_empty());
        // The holder goes: the card's own input comes back, then the other
        // node takes the card from there.
        graph.sync_cards(&[line(&b, "Line")], &both);
        let calls = amixer_calls();
        assert_eq!(calls.first().unwrap(), "7 sset Input Source Rear Mic");
        assert_eq!(calls.last().unwrap(), "7 sset Input Source Line");
        graph.sync_cards(&[], &both);
        assert_eq!(amixer_calls(), ["7 sset Input Source Rear Mic"]);
    }

    #[test]
    fn a_fresh_strip_is_linked_once_its_fader_is_written() {
        let mut graph = MixerGraph::new(test_naming(), 256);
        let key = (Side::Input, "game".to_string());
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let strip = Strip {
            def: def(&scene(), Side::Input, "game"),
            child: Some(child),
            conf: std::env::temp_dir().join(format!("hako-test-x-{}.conf", std::process::id())),
            fader: None,
            started: Instant::now(),
            failures: 0,
            retry_at: Instant::now(),
            error: None,
        };
        graph.strips.insert(key.clone(), strip);
        assert!(graph.linkable().is_empty(), "unity until the fader is set");
        graph.strips.get_mut(&key).unwrap().fader = Some((0.0, true));
        assert!(graph.linkable().contains_key(&key));
        // An app stream still to hand back keeps a stopped graph busy.
        graph.strips.get_mut(&key).unwrap().stop();
        graph.strips.clear();
        assert!(graph.is_empty());
        let moved = Moved {
            serial: None,
            sink: "x".into(),
        };
        graph.moved.insert(1, moved);
        assert!(!graph.is_empty());
        graph.moved.clear();
    }

    #[test]
    fn orphan_strip_configs_are_removed() {
        let dir = std::env::temp_dir().join(format!("hako-test-confs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let names = [
            "hako-mixer-in-game.conf",
            "hako-mixer-out-A1.conf",
            "hako-mixer-in-game.conf.bak",
            "hako-aux-dsp.conf",
            "hakod.sock",
        ];
        for name in names {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        remove_orphan_confs(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            left,
            [
                "hako-aux-dsp.conf",
                "hako-mixer-in-game.conf.bak",
                "hakod.sock"
            ]
        );
    }

    #[test]
    fn unreadable_card_controls_are_asked_again_later() {
        use crate::tests::amixer_calls;
        let node = "hako-test-controls-retry";
        amixer_calls();
        assert_eq!(capture_controls(node, "9"), (Vec::new(), false));
        assert_eq!(amixer_calls(), ["9 scontrols"]);
        // Not every status read: once the retry time has come.
        capture_controls(node, "9");
        assert!(amixer_calls().is_empty());
        let key = (node.to_string(), "9".to_string());
        let mut cache = CARD_CONTROLS.lock().unwrap();
        cache.get_mut(&key).unwrap().1 = Some(Instant::now());
        drop(cache);
        capture_controls(node, "9");
        assert_eq!(amixer_calls(), ["9 scontrols"]);
    }

    #[test]
    fn mono_folds_both_channels_into_each() {
        let mut scene = scene();
        scene.outputs[0].mono = true;
        let config = strip_config(&def(&scene, Side::Output, "A1"), &test_naming(), None).unwrap();
        for link in [
            "{ output = \"copy_l:Out\" input = \"mono_l:In 1\" }",
            "{ output = \"copy_r:Out\" input = \"mono_l:In 2\" }",
            "{ output = \"copy_l:Out\" input = \"mono_r:In 1\" }",
            "{ output = \"copy_r:Out\" input = \"mono_r:In 2\" }",
        ] {
            assert!(config.contains(link), "missing {link} in {config}");
        }
        assert!(config.contains("outputs = [ \"mono_l:Out\" \"mono_r:Out\" ]"));
        assert!(config.contains("control = { \"Gain 1\" = 0.5 \"Gain 2\" = 0.5 }"));
    }

    #[test]
    fn a_hardware_input_without_sources_is_a_problem_not_a_strip() {
        let mut scene = scene();
        scene.inputs[1].kind = InputKind::Hardware { sources: vec![] };
        let (defs, problems) = strip_defs(&scene, &test_naming(), 256, |_| false);
        assert!(!defs.iter().any(|(k, _)| k.1 == "desk"));
        assert_eq!(problems, [problem("desk", "no source selected")]);
    }

    #[test]
    fn the_plan_rebuilds_only_changed_topology() {
        let before = scene();
        let running: BTreeMap<_, _> = defs(&before).into_iter().collect();
        let step = |after: &Scene, id: &str| {
            let (remove, steps) = plan(&running, &defs(after));
            assert!(remove.is_empty());
            let mut changed: Vec<_> = steps
                .into_iter()
                .filter(|(_, s)| *s != Step::Keep)
                .collect();
            assert!(changed.len() <= 1, "{changed:?}");
            let only = changed.pop();
            if let Some((key, _)) = &only {
                assert_eq!(key.1, id);
            }
            only.map(|(_, s)| s)
        };

        // Faders, mute, solo, routes, the device a source / output uses:
        // nothing to rebuild (props and links).
        let mut after = before.clone();
        after.inputs[0].gain_db = -12.0;
        after.inputs[0].mute = true;
        after.inputs[1].solo = true;
        after.inputs[1].routes.clear();
        after.outputs[0].gain_db = 3.0;
        after.outputs[0].kind = OutputKind::Hardware {
            sink: "speakers".into(),
            description: "Speakers".into(),
        };
        // So are a source's capture card settings.
        let InputKind::Hardware { sources } = &mut after.inputs[1].kind else {
            unreachable!()
        };
        sources[1].alsa_input = Some("Line".into());
        sources[1].capture_percent = Some(50);
        let InputKind::Hardware { sources } = &mut after.inputs[1].kind else {
            unreachable!()
        };
        sources[0].node = "mic-c".into();
        assert_eq!(step(&after, ""), None);

        // Trims and effect values: live parameters.
        let mut after = before.clone();
        let InputKind::Hardware { sources } = &mut after.inputs[1].kind else {
            unreachable!()
        };
        sources[1].trim_db = 6.0;
        assert_eq!(step(&after, "desk"), Some(Step::Update));
        let mut with_gate = before.clone();
        with_gate.inputs[1].chain = gate();
        let running_gate: BTreeMap<_, _> = defs(&with_gate).into_iter().collect();
        let mut tweaked = with_gate.clone();
        if let AuxEffectKind::NoiseGate { threshold_db, .. } =
            &mut tweaked.inputs[1].chain.effects[0].kind
        {
            *threshold_db = -30.0;
        }
        let (_, steps) = plan(&running_gate, &defs(&tweaked));
        assert!(steps.contains(&((Side::Input, "desk".into()), Step::Update)));

        // Topology: a source added, the chain's stages, kind, mono, label.
        let mut after = before.clone();
        let InputKind::Hardware { sources } = &mut after.inputs[1].kind else {
            unreachable!()
        };
        sources.push(source("mic-c"));
        assert_eq!(step(&after, "desk"), Some(Step::Build));
        assert_eq!(step(&with_gate, "desk"), Some(Step::Build));
        let mut after = before.clone();
        after.outputs[1].mono = true;
        assert_eq!(step(&after, "B1"), Some(Step::Build));
        let mut after = before.clone();
        after.inputs[0].kind = InputKind::Virtual {
            node_name: "hako-test-other".into(),
            apps: Vec::new(),
        };
        assert_eq!(step(&after, "game"), Some(Step::Build));
        let mut after = before.clone();
        after.outputs[0].label = "Speakers".into();
        assert_eq!(step(&after, "A1"), Some(Step::Build));

        // Removed strips go; the others are untouched.
        let mut after = before.clone();
        after.inputs.remove(0);
        let (remove, steps) = plan(&running, &defs(&after));
        assert_eq!(remove, [(Side::Input, "game".to_string())]);
        assert!(steps.iter().all(|(_, s)| *s == Step::Keep));

        // A new latency rebuilds everything.
        let relatency: Vec<_> = strip_defs(&before, &test_naming(), 128, |_| false).0;
        let (_, steps) = plan(&running, &relatency);
        assert!(steps.iter().all(|(_, s)| *s == Step::Build));
    }

    #[test]
    fn fader_gain_is_db_times_the_chatmix_cube_and_solo_mutes_the_rest() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        let mut scene = scene();
        scene.inputs[0].gain_db = -6.0;
        let f = faders(&scene, (0.5, 1.0));
        // -6 dB * 0.5^3.
        assert!(close(f[0].1, 0.501_187 * 0.125), "{f:?}");
        assert!(close(f[1].1, 1.0) && !f[1].2);
        assert!(close(f[2].1, 1.0));
        assert!(close(db_to_gain(12.0), 3.981_072));
        // The chat side follows the dial's second value.
        scene.inputs[0].chatmix = Some(ChatMixSide::Chat);
        assert!(close(faders(&scene, (0.5, 0.0))[0].1, 0.0));

        scene.inputs[1].solo = true;
        let f = faders(&scene, (1.0, 1.0));
        assert!(f[0].2, "not soloed: muted");
        assert!(!f[1].2, "soloed: plays");
        assert!(!f[2].2 && !f[3].2, "outputs are never soloed away");
        scene.inputs[1].mute = true;
        assert!(faders(&scene, (1.0, 1.0))[1].2, "mute wins over solo");
    }

    #[test]
    fn devices_pair_stereo_mono_and_odd_channels() {
        assert_eq!(pair(&["FL", "FR"]), Some(("FL".into(), "FR".into())));
        assert_eq!(pair(&["FR", "FL", "LFE"]), Some(("FL".into(), "FR".into())));
        assert_eq!(pair(&["MONO"]), Some(("MONO".into(), "MONO".into())));
        assert_eq!(
            pair(&["AUX0", "AUX1"]),
            Some(("AUX0".into(), "AUX1".into()))
        );
        assert_eq!(pair(&["AUX3"]), Some(("AUX3".into(), "AUX3".into())));
        assert_eq!(pair(&[]), None);
    }

    /// `(node.name, media.class, other props)`.
    type TestNode<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    fn graph_with(nodes: &[TestNode]) -> Graph {
        let mut graph = Graph::default();
        let mut port_id = 1000;
        for (i, (name, class, extra)) in nodes.iter().enumerate() {
            let id = i as u32 + 1;
            let mut props: HashMap<String, String> = extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            props.insert("node.name".into(), name.to_string());
            graph.nodes.insert(
                id,
                Node {
                    name: name.to_string(),
                    media_class: class.to_string(),
                    props,
                },
            );
            let (direction, prefix) = if class.ends_with("Sink") {
                (Direction::In, "playback")
            } else {
                (Direction::Out, "capture")
            };
            for channel in ["FL", "FR"] {
                port_id += 1;
                graph.ports.insert(
                    port_id,
                    Port {
                        node: id,
                        name: format!("{prefix}_{channel}"),
                        direction,
                        channel: channel.into(),
                    },
                );
            }
        }
        graph
    }

    /// Apps are playback streams by app name (else binary), once each; not
    /// hako's own, nor ones that must not move. Only running inputs take them.
    #[test]
    fn apps_are_named_streams_and_go_to_running_inputs() {
        let graph = graph_with(&[
            (
                "spotify",
                "Stream/Output/Audio",
                &[("application.name", "Spotify")],
            ),
            (
                "spotify",
                "Stream/Output/Audio",
                &[("application.name", "Spotify")],
            ),
            (
                "Brave",
                "Stream/Output/Audio",
                &[("application.process.binary", "brave")],
            ),
            (
                "hako_in_game",
                "Stream/Output/Audio",
                &[("application.name", "hako")],
            ),
            (
                "pinned",
                "Stream/Output/Audio",
                &[("application.name", "Pinned"), ("node.dont-move", "true")],
            ),
            ("obs", "Stream/Input/Audio", &[("application.name", "OBS")]),
        ]);
        assert_eq!(app_list(&graph), ["Spotify", "brave"]);

        let music = input(
            "music",
            "Music",
            InputKind::Virtual {
                node_name: "Hako_Music".into(),
                apps: vec!["Spotify".into(), "Pinned".into()],
            },
            Vec::new(),
        );
        let scene = Scene {
            inputs: vec![music],
            outputs: Vec::new(),
        };
        let wanted = wanted_apps(&scene, &["music"], &graph);
        assert_eq!(wanted.keys().copied().collect::<Vec<_>>(), [1, 2]);
        assert!(wanted
            .values()
            .all(|(m, i)| m.sink == "Hako_Music" && i == "music"));
        assert!(
            wanted_apps(&scene, &[], &graph).is_empty(),
            "strip down: left alone"
        );
    }

    #[test]
    fn device_list_skips_hakos_own_nodes() {
        let graph = graph_with(&[
            (
                "alsa_input.usb-headset.mono-fallback",
                "Audio/Source",
                &[
                    ("node.description", "Headset Mono"),
                    ("audio.channels", "1"),
                    ("device.id", "70"),
                    ("alsa.card", "4"),
                ],
            ),
            (
                "alsa_output.usb-headset.analog-stereo",
                "Audio/Sink",
                &[("device.id", "70")],
            ),
            ("Hako_Game", "Audio/Sink", &[]),
            ("hako_in_game", "Stream/Output/Audio", &[]),
            ("hako-meter", "Stream/Input/Audio", &[]),
            ("My_Stream_Mic", "Audio/Source", &[]),
            ("spotify", "Stream/Output/Audio", &[]),
        ]);
        let list = device_list(&graph, &["My_Stream_Mic"]);
        assert_eq!(list.len(), 2, "{list:?}");
        assert_eq!(list[0].device.description, "Headset Mono");
        assert_eq!(list[0].device.channels, 1);
        assert_eq!(list[0].device.direction, DeviceDirection::Source);
        assert_eq!(list[0].card.as_deref(), Some("70"));
        // No description: the node name; channels from its ports.
        assert_eq!(
            list[1].device.description,
            "alsa_output.usb-headset.analog-stereo"
        );
        assert_eq!(list[1].device.channels, 2);
        assert_eq!(list[1].device.direction, DeviceDirection::Sink);
        assert_eq!(list[0].alsa_card.as_deref(), Some("4"));
        assert_eq!(list[1].alsa_card, None);
        // A network peer's nodes are offered: they are the network racks.
        let net = graph_with(&[
            ("hako_net.0011223344556677.recv", "Audio/Source", &[]),
            ("hako_net.0011223344556677.send", "Audio/Sink", &[]),
        ]);
        assert_eq!(device_list(&net, &[]).len(), 2);
    }

    fn info(node: &str, direction: DeviceDirection, card: &str) -> DeviceInfo {
        DeviceInfo {
            device: AudioDevice {
                node: node.into(),
                description: format!("{node} description"),
                direction,
                channels: 2,
                inputs: Vec::new(),
                capture: false,
                capture_now: None,
            },
            card: Some(card.into()),
            alsa_card: None,
        }
    }

    #[test]
    fn starter_layout_preserves_chatmix_and_the_line_in() {
        use DeviceDirection::{Sink, Source as Src};
        let mut line = info("alsa_input.pci-0000_10_00.6.analog-stereo", Src, "202");
        line.device.inputs = vec!["Rear Mic".into(), "Front Mic".into(), "Line".into()];
        line.device.capture = true;
        let devices = [
            line,
            info("alsa_output.hdmi", Sink, "60"),
            info("alsa_output.usb-Arctis.analog-stereo", Sink, "70"),
            info("alsa_input.usb-Arctis.mono-fallback", Src, "70"),
        ];
        let aux = AuxConfig {
            effects: gate(),
            sink_name_regex: r"usb-Arctis.*analog".into(),
            ..AuxConfig::default()
        };
        let detected = |aux_enabled| Detected {
            devices: &devices,
            headset_sink: Some("alsa_output.usb-Arctis.analog-stereo"),
            default_sink: Some("alsa_output.hdmi"),
            aux: &aux,
            aux_enabled,
            capture_percent: Some(40),
        };
        let scene = starter_scene(&detected(false));
        scene.validate().unwrap();
        let ids: Vec<_> = scene.inputs.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["game", "chat", "line-in", "mic"]);
        assert_eq!(
            scene.inputs[0].kind,
            InputKind::Virtual {
                node_name: "Hako_Game".into(),
                apps: Vec::new(),
            }
        );
        assert_eq!(scene.inputs[0].chatmix, Some(ChatMixSide::Game));
        assert_eq!(scene.inputs[1].chatmix, Some(ChatMixSide::Chat));
        assert_eq!(scene.inputs[0].routes, ["A1"]);
        let line_in = &scene.inputs[2];
        assert!(line_in.mute, "AUX was off");
        assert_eq!(line_in.chain, gate());
        // Off: the card's jack may be a mic; it is left alone.
        assert!(matches!(&line_in.kind, InputKind::Hardware { sources }
            if sources[0].node == "alsa_input.pci-0000_10_00.6.analog-stereo"
                && sources[0].alsa_input.is_none() && sources[0].capture_percent.is_none()));
        let mic = &scene.inputs[3];
        assert!(matches!(&mic.kind, InputKind::Hardware { sources }
            if sources[0].node == "alsa_input.usb-Arctis.mono-fallback"));
        assert_eq!(mic.routes, ["B1"]);
        assert_eq!(
            scene.outputs[0].kind,
            OutputKind::Hardware {
                sink: "alsa_output.usb-Arctis.analog-stereo".into(),
                description: "alsa_output.usb-Arctis.analog-stereo description".into(),
            }
        );
        assert_eq!(scene.outputs[0].label, "Headset");
        assert_eq!(
            scene.outputs[1].kind,
            OutputKind::Virtual {
                node_name: "Hako_Mic".into()
            }
        );
        assert_eq!(scene.outputs[1].label, "hako Mic");
        // On: unmuted, its card on the line jack at the saved level.
        let on = starter_scene(&detected(true));
        on.validate().unwrap();
        assert!(!on.inputs[2].mute, "AUX was on");
        assert_eq!(on.inputs[2].routes, ["A1"]);
        assert!(matches!(&on.inputs[2].kind, InputKind::Hardware { sources }
            if sources[0].alsa_input.as_deref() == Some("Line")
                && sources[0].capture_percent == Some(40)));
        // A card without a Line item or a capture level: nothing to set.
        let mut plain = devices.clone();
        plain[0].device.inputs = vec!["Mic".into()];
        plain[0].device.capture = false;
        let scene = starter_scene(&Detected {
            devices: &plain,
            ..detected(true)
        });
        assert!(
            matches!(&scene.inputs[2].kind, InputKind::Hardware { sources }
            if sources[0].alsa_input.is_none() && sources[0].capture_percent.is_none())
        );

        // ChatMix not up yet (daemon start): the old line-in's sink.
        let scene = starter_scene(&Detected {
            headset_sink: None,
            ..detected(true)
        });
        assert_eq!(scene.outputs[0].label, "Headset");
        assert!(
            matches!(&scene.outputs[0].kind, OutputKind::Hardware { sink, .. }
            if sink == "alsa_output.usb-Arctis.analog-stereo")
        );

        // No headset, no line-in: Game / Chat on the default sink, no mic.
        let bare = [info("alsa_output.hdmi", Sink, "60")];
        let scene = starter_scene(&Detected {
            devices: &bare,
            headset_sink: None,
            ..detected(false)
        });
        scene.validate().unwrap();
        let ids: Vec<_> = scene.inputs.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["game", "chat"]);
        assert!(scene.outputs[0].label.len() <= 32);
        // Nothing at all: only the virtual mic, nothing routes nowhere.
        let scene = starter_scene(&Detected {
            devices: &[],
            headset_sink: None,
            default_sink: None,
            ..detected(false)
        });
        scene.validate().unwrap();
        assert!(scene.inputs.iter().all(|i| i.routes.is_empty()));
    }

    #[test]
    fn links_follow_routes_sources_and_sinks() {
        let scene = scene();
        let up: BTreeMap<_, _> = defs(&scene).into_iter().collect();
        let mut graph = graph_with(&[
            ("mic-a", "Audio/Source", &[]),
            ("headset", "Audio/Sink", &[]),
        ]);
        // mic-b is mono.
        graph.nodes.insert(
            50,
            Node {
                name: "mic-b".into(),
                media_class: "Audio/Source".into(),
                props: HashMap::new(),
            },
        );
        graph.ports.insert(
            51,
            Port {
                node: 50,
                name: "capture_MONO".into(),
                direction: Direction::Out,
                channel: "MONO".into(),
            },
        );
        let (links, problems) = wanted_links(&scene, &up, &graph);
        assert!(problems.is_empty(), "{problems:?}");
        let has = |from: (&str, &str), to: (&str, &str)| {
            links
                .iter()
                .any(|(f, t, _)| *f == end(from.0, from.1) && *t == end(to.0, to.1))
        };
        assert!(has(
            ("mic-a", "FL"),
            ("hako-test-x-in_desk.capture", "AUX0")
        ));
        assert!(has(
            ("mic-a", "FR"),
            ("hako-test-x-in_desk.capture", "AUX1")
        ));
        assert!(has(
            ("mic-b", "MONO"),
            ("hako-test-x-in_desk.capture", "AUX2")
        ));
        assert!(has(
            ("mic-b", "MONO"),
            ("hako-test-x-in_desk.capture", "AUX3")
        ));
        assert!(has(
            ("hako-test-x-in_game", "FL"),
            ("hako-test-x-out_A1", "FL")
        ));
        assert!(has(
            ("hako-test-x-in_desk", "FR"),
            ("hako-test-x-out_B1", "FR")
        ));
        assert!(has(("hako-test-x-out_A1.play", "FR"), ("headset", "FR")));
        assert_eq!(links.len(), 4 + 2 + 4 + 2);

        // An unplugged device is a problem of its strip; routes to an output
        // that is not up are skipped.
        let mut down = up.clone();
        down.remove(&(Side::Output, "B1".into()));
        let graph = graph_with(&[("mic-a", "Audio/Source", &[])]);
        let (links, problems) = wanted_links(&scene, &down, &graph);
        assert_eq!(
            problems,
            [
                problem("desk", "mic-b is not connected"),
                problem("A1", "headset is not connected")
            ]
        );
        assert!(!links.iter().any(|(_, t, _)| t.0 == "hako-test-x-out_B1"));
    }

    /// A one-channel sink (Bluetooth HFP, a speakerphone) gets the strip's
    /// 0.5 / 0.5 fold on one link, not both sides summed into its port.
    #[test]
    fn a_mono_sink_gets_a_folded_downmix_on_one_link() {
        let scene = scene();
        let mut graph = graph_with(&[("mic-a", "Audio/Source", &[])]);
        graph.nodes.insert(
            50,
            Node {
                name: "headset".into(),
                media_class: "Audio/Sink".into(),
                props: HashMap::new(),
            },
        );
        graph.ports.insert(
            51,
            Port {
                node: 50,
                name: "playback_MONO".into(),
                direction: Direction::In,
                channel: "MONO".into(),
            },
        );
        assert!(mono_sink(&graph, "headset"));
        assert!(!mono_sink(
            &graph_with(&[("headset", "Audio/Sink", &[])]),
            "headset"
        ));
        let (defs, _) = strip_defs(&scene, &test_naming(), 256, |s| mono_sink(&graph, s));
        let up: BTreeMap<_, _> = defs.into_iter().collect();
        let a1 = &up[&(Side::Output, "A1".to_string())];
        assert!(a1.mono, "folds although the output's mono is off");
        assert!(!up[&(Side::Output, "B1".to_string())].mono);
        let config = strip_config(a1, &test_naming(), None).unwrap();
        assert!(config.contains("control = { \"Gain 1\" = 0.5 \"Gain 2\" = 0.5 }"));
        let (links, _) = wanted_links(&scene, &up, &graph);
        let to_sink: Vec<_> = links.iter().filter(|(_, t, _)| t.0 == "headset").collect();
        assert_eq!(to_sink.len(), 1, "{to_sink:?}");
        assert_eq!(to_sink[0].0, end("hako-test-x-out_A1.play", "FL"));
        assert_eq!(to_sink[0].1, end("headset", "MONO"));
    }

    // ── live ─────────────────────────────────────────────────────────

    use crate::engine::engine;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    /// `pw-cli` holding null devices named `hako-test-mix-<pid>-*` (gone with
    /// it), or another test process; killed on drop.
    struct TestDevices(Child);

    impl TestDevices {
        fn spawn(devices: &[(&str, &str)]) -> Self {
            let mut cli = Command::new("pw-cli")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            for (name, class) in devices {
                writeln!(
                    cli.stdin.as_mut().unwrap(),
                    "create-node adapter {{ factory.name=support.null-audio-sink node.name={name} \
                     media.class={class} audio.position=[FL FR] object.linger=false \
                     node.group=pipewire.dummy }}"
                )
                .unwrap();
            }
            let names: Vec<String> = devices.iter().map(|(n, _)| n.to_string()).collect();
            let ready = engine()
                .unwrap()
                .wait_for(Duration::from_secs(4), |g| {
                    Ok(names.iter().all(|n| g.node_id(n).is_some()))
                })
                .unwrap();
            assert!(ready, "test devices did not appear");
            Self(cli)
        }
    }

    impl Drop for TestDevices {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn pids(graph: &MixerGraph) -> BTreeMap<StripKey, u32> {
        graph
            .strips
            .iter()
            .filter_map(|(k, s)| Some((k.clone(), s.child.as_ref()?.id())))
            .collect()
    }

    /// `channelVolumes` / `mute` of `node` from `pw-dump` once they read `want`.
    fn fader_becomes(node: &str, want: (f64, bool)) -> Option<(f64, bool)> {
        let mut got = None;
        for _ in 0..30 {
            got = crate::tests::dumped_props(node).iter().find_map(|p| {
                let volume = p.get("channelVolumes")?.as_array()?.first()?.as_f64()?;
                Some((volume, p.get("mute")?.as_bool()?))
            });
            if got.is_some_and(|(v, m)| (v - want.0).abs() < 1e-4 && m == want.1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        got
    }

    /// A -6 dBFS tone played into a virtual input reads -6 dB lower on a
    /// meter of the strip's post-fader node with the fader at -6 dB.
    #[test]
    #[ignore = "needs a running PipeWire session (pw-play, pw-dump)"]
    fn live_meter_reads_the_input_after_its_fader() {
        use crate::meters::{tests::loudest, tests::write_tone, MeterService, MeterTarget};
        let prefix = format!("hako-test-meter-{}-", std::process::id());
        let sink = format!("{prefix}va");
        let mut scene = Scene {
            inputs: vec![input(
                "a",
                "Test A",
                InputKind::Virtual {
                    node_name: sink.clone(),
                    apps: Vec::new(),
                },
                Vec::new(),
            )],
            outputs: Vec::new(),
        };
        scene.inputs[0].gain_db = -6.0;
        let mut graph = MixerGraph::new(
            Naming {
                nodes: prefix.clone(),
                files: prefix.clone(),
                args: "node.group = \"pipewire.dummy\"".into(),
            },
            256,
        );
        graph.reconcile(&scene, (1.0, 1.0));
        assert!(graph.problems().is_empty(), "{:?}", graph.problems());
        let node = graph.meter_node(&(Side::Input, "a".into())).unwrap();
        assert_eq!(node, format!("{prefix}in_a"));

        let meters = MeterService::new();
        let sub = meters.subscribe(vec![MeterTarget {
            node_name: node,
            monitor_sink: false,
        }]);
        let wav = std::env::temp_dir().join(format!("{prefix}tone.wav"));
        write_tone(&wav);
        let mut tone = Command::new("pw-play")
            .args([
                "--target",
                &sink,
                "--volume",
                "1",
                "-P",
                &format!(
                    "{{ node.name={prefix}tone state.restore-props=false node.group=pipewire.dummy }}"
                ),
            ])
            .arg(&wav)
            .spawn()
            .unwrap();
        let peak = loudest(&sub, 2.0);
        let _ = tone.kill();
        let _ = tone.wait();
        drop(sub);
        let _ = std::fs::remove_file(&wav);
        graph.teardown();
        // 0.5 at -6 dB: 0.25.
        for p in peak {
            assert!((0.22..0.28).contains(&p), "peak {peak:?}");
        }
    }

    /// Whether the `default` metadata points stream `id` somewhere.
    fn has_target(id: u32) -> bool {
        let out = Command::new("pw-metadata")
            .args(["-n", "default"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("id:{id} key:'target.object'"))
    }

    /// A virtual input takes its app's stream (from wherever it played)
    /// while it runs, again after a rebuild, and hands it back when the app
    /// is dropped and at teardown.
    #[test]
    #[ignore = "needs a running PipeWire session with WirePlumber (pw-cat, pw-metadata)"]
    fn live_virtual_input_takes_an_app_s_stream() {
        let prefix = format!("hako-test-app-{}-", std::process::id());
        // Not hako-named: hako's own streams are never apps.
        let (sink, app) = (
            format!("{prefix}va"),
            format!("test-player-{}", std::process::id()),
        );
        let virt = |apps: Vec<String>| InputKind::Virtual {
            node_name: sink.clone(),
            apps,
        };
        let mut scene = Scene {
            inputs: vec![input("a", "Test A", virt(vec![app.clone()]), Vec::new())],
            outputs: Vec::new(),
        };
        let mut graph = MixerGraph::new(
            Naming {
                nodes: prefix.clone(),
                files: prefix.clone(),
                args: "node.group = \"pipewire.dummy\"".into(),
            },
            256,
        );
        // Silence: harmless wherever WirePlumber plays it first. Killed on drop.
        let _player = TestDevices(
            Command::new("pw-cat")
                .args([
                    "-p",
                    "--raw",
                    "--format",
                    "s16",
                    "--rate",
                    "48000",
                    "--channels",
                    "2",
                ])
                .arg("-P")
                .arg(format!(
                    "{{ application.name={app} node.name={app} state.restore-props=false }}"
                ))
                .arg("-")
                .stdin(std::fs::File::open("/dev/zero").unwrap())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let engine = engine().unwrap();
        let stream = || engine.with_graph(|g| g.node_id(&app));
        engine
            .wait_for(Duration::from_secs(4), |g| Ok(g.node_id(&app).is_some()))
            .unwrap();
        assert!(apps().unwrap().contains(&app));
        let into_ours = |want: bool| {
            let link = (format!("{app}:output_FL"), format!("{sink}:playback_FL"));
            engine
                .wait_for(Duration::from_secs(4), |g| {
                    Ok(g.link_names().contains(&link) == want)
                })
                .unwrap()
        };

        graph.reconcile(&scene, (1.0, 1.0));
        assert!(graph.problems().is_empty(), "{:?}", graph.problems());
        assert!(into_ours(true), "moved into the input");
        let id = stream().unwrap();
        assert!(has_target(id));

        // A rebuilt strip (new label) gets it back.
        scene.inputs[0].label = "Test B".into();
        graph.reconcile(&scene, (1.0, 1.0));
        assert!(into_ours(true), "back after a rebuild");

        // Dropped from the input: handed back to WirePlumber.
        scene.inputs[0].kind = virt(Vec::new());
        graph.reconcile(&scene, (1.0, 1.0));
        assert!(into_ours(false), "handed back");
        assert!(!has_target(id));

        scene.inputs[0].kind = virt(vec![app.clone()]);
        graph.reconcile(&scene, (1.0, 1.0));
        assert!(into_ours(true));
        graph.teardown();
        assert!(!has_target(id), "teardown hands it back");
        assert!(stream().is_some(), "the app kept playing");
    }

    fn test_objects(prefix: &str) -> usize {
        let dump = Command::new("pw-dump").output().unwrap();
        String::from_utf8_lossy(&dump.stdout)
            .matches(prefix)
            .count()
    }

    /// Two inputs (virtual; hardware on a test source) into two outputs
    /// (a test sink; a virtual mic), all `hako-test-mix-<pid>-*` on the
    /// dummy driver: built, edited live, restructured, torn down.
    #[test]
    #[ignore = "needs a running PipeWire session"]
    fn live_mixer_builds_edits_and_tears_down() {
        let prefix = format!("hako-test-mix-{}-", std::process::id());
        let name = |part: &str| format!("{prefix}{part}");
        let _devices = TestDevices::spawn(&[
            (&name("src"), "Audio/Source/Virtual"),
            (&name("sink"), "Audio/Sink"),
        ]);
        let mut scene = Scene {
            inputs: vec![
                input(
                    "a",
                    "Test A",
                    InputKind::Virtual {
                        node_name: name("va"),
                        apps: Vec::new(),
                    },
                    vec!["A1".into(), "B1".into()],
                ),
                input(
                    "b",
                    "Test B",
                    InputKind::Hardware {
                        sources: vec![source(&name("src"))],
                    },
                    vec!["A1".into()],
                ),
            ],
            outputs: vec![
                output(
                    "A1",
                    "Test A1",
                    OutputKind::Hardware {
                        sink: name("sink"),
                        description: String::new(),
                    },
                ),
                output(
                    "B1",
                    "Test B1",
                    OutputKind::Virtual {
                        node_name: name("vb"),
                    },
                ),
            ],
        };
        scene.validate().unwrap();
        let mut graph = MixerGraph::new(
            Naming {
                nodes: prefix.clone(),
                files: prefix.clone(),
                args: "node.group = \"pipewire.dummy\"".into(),
            },
            256,
        );
        let engine = engine().unwrap();
        let linked = |graph: &MixerGraph, from: (&str, &str), to: (&str, &str)| {
            let ids = graph.link_ids();
            engine
                .wait_for(Duration::from_secs(2), |g| {
                    let ports = g
                        .channel_port(from.0, from.1, Direction::Out)
                        .zip(g.channel_port(to.0, to.1, Direction::In));
                    Ok(ports.is_some_and(|(o, i)| {
                        g.links_between(o.1, i.1).iter().any(|id| ids.contains(id))
                    }))
                })
                .unwrap()
        };

        graph.reconcile(&scene, (1.0, 1.0));
        assert!(graph.running());
        assert!(graph.problems().is_empty(), "{:?}", graph.problems());
        let built = pids(&graph);
        assert_eq!(built.len(), 4);
        let (in_a, in_b) = (name("in_a"), name("in_b"));
        let (out_a1, out_b1) = (name("out_A1"), name("out_B1"));
        assert!(linked(
            &graph,
            (&name("src"), "FL"),
            (&format!("{in_b}.capture"), "AUX0")
        ));
        assert!(linked(
            &graph,
            (&name("src"), "FR"),
            (&format!("{in_b}.capture"), "AUX1")
        ));
        assert!(linked(&graph, (&in_a, "FL"), (&out_a1, "FL")));
        assert!(linked(&graph, (&in_a, "FR"), (&out_b1, "FR")));
        assert!(linked(&graph, (&in_b, "FL"), (&out_a1, "FL")));
        assert!(linked(
            &graph,
            (&format!("{out_a1}.play"), "FR"),
            (&name("sink"), "FR")
        ));
        assert_eq!(graph.link_ids().len(), 2 + 4 + 2 + 2);
        // Synchronous: no hako link may add a quantum.
        std::thread::sleep(Duration::from_millis(300));
        engine.with_graph(|g| {
            for id in graph.link_ids() {
                assert_ne!(
                    g.links[&id].props.get("link.async").map(String::as_str),
                    Some("true")
                );
            }
        });
        assert_eq!(fader_becomes(&in_a, (1.0, false)), Some((1.0, false)));

        // A fader and a mute: Props only, no process restarted.
        scene.inputs[0].gain_db = -6.0;
        scene.outputs[0].mute = true;
        graph.reconcile(&scene, (1.0, 1.0));
        assert_eq!(pids(&graph), built);
        let want = f64::from(db_to_gain(-6.0));
        assert!(fader_becomes(&in_a, (want, false)).is_some_and(|(v, _)| (v - want).abs() < 1e-4));
        assert_eq!(fader_becomes(&out_a1, (1.0, true)), Some((1.0, true)));

        // A new route: one more pair of links, nothing restarted.
        scene.inputs[1].routes.push("B1".into());
        graph.reconcile(&scene, (1.0, 1.0));
        assert_eq!(pids(&graph), built);
        assert!(linked(&graph, (&in_b, "FL"), (&out_b1, "FL")));
        assert!(linked(&graph, (&in_b, "FR"), (&out_b1, "FR")));

        // A trim: a live parameter, nothing restarted.
        let InputKind::Hardware { sources } = &mut scene.inputs[1].kind else {
            unreachable!()
        };
        sources[0].trim_db = -12.0;
        graph.reconcile(&scene, (1.0, 1.0));
        assert_eq!(pids(&graph), built);
        assert!(graph.problems().is_empty(), "{:?}", graph.problems());

        // Removing an input stops it alone.
        scene.inputs.remove(0);
        graph.reconcile(&scene, (1.0, 1.0));
        let mut rest = built.clone();
        rest.remove(&(Side::Input, "a".into()));
        assert_eq!(pids(&graph), rest);
        let gone = engine
            .wait_for(Duration::from_secs(2), |g| {
                Ok(g.node_id(&in_a).is_none() && g.node_id(&name("va")).is_none())
            })
            .unwrap();
        assert!(gone, "removed strip's nodes outlived it");
        assert!(linked(&graph, (&in_b, "FL"), (&out_a1, "FL")));

        // A crashed strip comes back.
        let key = (Side::Output, "B1".to_string());
        let child = graph.strips.get_mut(&key).unwrap().child.as_mut().unwrap();
        let pid = child.id();
        let _ = Command::new("kill").arg(pid.to_string()).status();
        let _ = child.wait();
        graph.reconcile(&scene, (1.0, 1.0));
        assert!(
            graph
                .problems()
                .contains(&problem("B1", "stopped unexpectedly")),
            "{:?}",
            graph.problems()
        );
        std::thread::sleep(Duration::from_millis(1100));
        graph.reconcile(&scene, (1.0, 1.0));
        assert!(graph.problems().is_empty(), "{:?}", graph.problems());
        assert_ne!(pids(&graph)[&key], pid);
        assert!(linked(&graph, (&in_b, "FR"), (&out_b1, "FR")));

        graph.teardown();
        assert!(graph.is_empty() && !graph.running());
        drop(_devices);
        let deadline = Instant::now() + Duration::from_secs(2);
        while test_objects(&prefix) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(test_objects(&prefix), 0, "test objects outlived the mixer");
    }
}
