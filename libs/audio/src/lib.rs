//! PipeWire ChatMix + AUX (line-in) on a native control plane (no PulseAudio,
//! `pw-link`, `pw-cli` or `wpctl`).
//!
//! The `engine` module keeps one PipeWire connection on its own thread with a registry
//! cache; node lookups, links, volumes, DSP parameters and the default sink go
//! through it. Audio itself runs in child processes for crash isolation:
//! `pw-loopback` (the ChatMix sinks) and `pipewire -c` (the AUX filter-chain).
//! The line-in mixer controls use `amixer`.

mod engine;
pub mod mixer;

use anyhow::{anyhow, bail, Context, Result};
use engine::{engine, Direction, LinkHandle};
use hako_core::{
    ipc::{AuxEffectChain, AuxEffectKind, EqBandKind},
    protocol::is_new_error,
    AuxConfig,
};
use regex::Regex;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub mod latency;
pub mod meters;

pub const CHATMIX_GAME: &str = "Hako_Game";
pub const CHATMIX_CHAT: &str = "Hako_Chat";

/// `node.latency` in samples until `set_latency` (the "balanced" setting).
const DEFAULT_LATENCY: u32 = 256;
const LSP_LADSPA: &str = "lsp-plugins-ladspa.so";
/// Searched after `$LADSPA_PATH`.
const LADSPA_DIRS: [&str; 4] = [
    "/usr/lib/ladspa",
    "/usr/lib64/ladspa",
    "/usr/lib/x86_64-linux-gnu/ladspa",
    "/usr/local/lib/ladspa",
];
const AUX_DSP_SINK: &str = "Hako_AUX_DSP";
/// How long a spawned node may take to show its ports.
const NODE_WAIT: Duration = Duration::from_secs(4);
const AUX_DSP_DESC: &str = "hako AUX Processing";
/// LSP's UI default. Unset, PipeWire falls back to the LADSPA minimum (0 ms),
/// which makes the gate/compressor detector follow raw peaks and chatter.
const SIDECHAIN_REACTIVITY_MS: f32 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultOwner {
    ChatMix,
    Aux,
}

/// Sinks are tracked by node name: PipeWire ids go stale (and get reused)
/// when a node is recreated.
#[derive(Debug, Default)]
struct DefaultState {
    session_original: Option<String>,
    claims: Vec<(DefaultOwner, String)>,
}

impl DefaultState {
    fn claim(&mut self, owner: DefaultOwner, sink: String, session_default: Option<String>) {
        if self.claims.is_empty() {
            self.session_original = session_default;
        }
        self.claims.retain(|(held, _)| *held != owner);
        self.claims.push((owner, sink));
    }

    fn release(&mut self, owner: DefaultOwner) -> Option<String> {
        let was_current = self.claims.last().is_some_and(|(held, _)| *held == owner);
        self.claims.retain(|(held, _)| *held != owner);
        if !was_current {
            return None;
        }
        self.claims
            .last()
            .map(|(_, sink)| sink.clone())
            .or_else(|| self.session_original.take())
    }

    /// The sink hako made the default, if any.
    fn held(&self) -> Option<&str> {
        self.claims.last().map(|(_, sink)| sink.as_str())
    }

    fn restore(&mut self) -> Option<String> {
        self.claims.clear();
        self.session_original.take()
    }
}

#[derive(Debug, Default)]
struct ChatMixState {
    game: Option<Child>,
    chat: Option<Child>,
    /// The headset sink both loopbacks play into.
    sink: Option<String>,
    /// Last volumes written, so repeated dial reports cost nothing.
    volumes: Option<(f32, f32)>,
}

impl ChatMixState {
    fn active(&self) -> bool {
        self.game.is_some() && self.chat.is_some()
    }
}

#[derive(Debug)]
struct AuxState {
    /// `("node:port", "node:port")` of `handles`, for `aux_graph_intact`.
    links: Vec<(String, String)>,
    handles: Vec<LinkHandle>,
    dsp: Option<Child>,
    /// Capture source node name.
    capture: String,
    sink: String,
    /// `None` marks a pending AUX that owns nothing yet; `heal_aux` builds it.
    alsa_card: Option<String>,
    previous_input_source: Option<String>,
    cfg: AuxConfig,
    chain: AuxEffectChain,
}

impl AuxState {
    fn pending(cfg: AuxConfig, chain: AuxEffectChain) -> Self {
        Self {
            links: Vec::new(),
            handles: Vec::new(),
            dsp: None,
            capture: String::new(),
            sink: String::new(),
            alsa_card: None,
            previous_input_source: None,
            cfg,
            chain,
        }
    }
}

#[derive(Debug)]
struct AudioState {
    chatmix: ChatMixState,
    aux: Option<AuxState>,
    defaults: DefaultState,
    /// Last ChatMix / AUX failure, kept apart so one part's success never
    /// hides the other's error.
    chatmix_error: Option<String>,
    aux_error: Option<String>,
    /// The errors above as last handed to `on_error` or returned to a caller.
    chatmix_reported: Option<String>,
    aux_reported: Option<String>,
    /// ChatMix takes the default sink when it comes up.
    chatmix_set_default: bool,
    /// Set by `reset_all` (the kill-switch): ChatMix stays down until
    /// `resume_chatmix`.
    chatmix_suppressed: bool,
    /// `node.latency` of hako's nodes, in samples at 48 kHz.
    latency: u32,
    /// The mixer owns ChatMix and the line-in (`hand_over`): neither is built
    /// while set; the ChatMix dial goes to the mixer.
    handed_over: bool,
    /// ChatMix's sink when the mixer took over, to bring it back on that sink.
    parked_sink: Option<String>,
    /// The last ChatMix dial position (game, chat), 0..1.
    dial: Option<(f32, f32)>,
    /// The mixer runs a `Hako_Game` sink (the default-sink preference's target).
    mixer_game: bool,
}

/// What status reads see: refreshed each time the audio lock is released,
/// so `GetState` never waits on audio work in progress.
#[derive(Debug, Clone, Default)]
struct Status {
    chatmix_active: bool,
    aux_active: bool,
    aux_processing_active: bool,
    chatmix_error: Option<String>,
    aux_error: Option<String>,
    aux_capture: Option<String>,
    aux_output: Option<String>,
}

impl Status {
    fn of(state: &mut AudioState) -> Self {
        reap_exited(&mut state.chatmix.game);
        reap_exited(&mut state.chatmix.chat);
        let (mut aux_processing_active, mut aux_capture, mut aux_output) = (false, None, None);
        if let Some(aux) = state.aux.as_mut() {
            reap_exited(&mut aux.dsp);
            aux_processing_active = aux.dsp.is_some();
            if aux.alsa_card.is_some() {
                aux_capture = Some(aux.capture.clone());
                aux_output = Some(if aux.dsp.is_some() {
                    AUX_DSP_SINK.to_string()
                } else {
                    aux.sink.clone()
                });
            }
        }
        Self {
            chatmix_active: state.chatmix.active(),
            aux_active: state.aux.is_some(),
            aux_processing_active,
            chatmix_error: state.chatmix_error.clone(),
            aux_error: state.aux_error.clone(),
            aux_capture,
            aux_output,
        }
    }
}

/// The audio lock; publishes a fresh `Status` when released.
struct StateGuard<'a> {
    state: MutexGuard<'a, AudioState>,
    status: &'a Mutex<Status>,
}

impl Deref for StateGuard<'_> {
    type Target = AudioState;
    fn deref(&self) -> &AudioState {
        &self.state
    }
}

impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut AudioState {
        &mut self.state
    }
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = Status::of(&mut self.state);
    }
}

type ErrorHook = Box<dyn Fn(String) + Send + Sync>;
type DialHook = Box<dyn Fn(f32, f32) + Send + Sync>;

pub struct AudioService {
    state: Mutex<AudioState>,
    status: Mutex<Status>,
    on_error: OnceLock<ErrorHook>,
    on_dial: OnceLock<DialHook>,
}

impl AudioService {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(AudioState {
                chatmix: ChatMixState::default(),
                aux: None,
                defaults: DefaultState::default(),
                chatmix_error: None,
                aux_error: None,
                chatmix_reported: None,
                aux_reported: None,
                chatmix_set_default: true,
                chatmix_suppressed: false,
                latency: DEFAULT_LATENCY,
                handed_over: false,
                parked_sink: None,
                dial: None,
                mixer_game: false,
            }),
            status: Mutex::new(Status::default()),
            on_error: OnceLock::new(),
            on_dial: OnceLock::new(),
        }
    }

    /// Called with each ChatMix dial position (game, chat) while the mixer
    /// owns ChatMix (under the audio lock: it must not block). Set once.
    pub fn on_dial(&self, hook: impl Fn(f32, f32) + Send + Sync + 'static) {
        let _ = self.on_dial.set(Box::new(hook));
    }

    /// The last ChatMix dial position (game, chat); centred until reported.
    pub fn chatmix_dial(&self) -> (f32, f32) {
        self.state().dial.unwrap_or((1.0, 1.0))
    }

    /// The headset sink ChatMix plays into (or did, before the mixer took over).
    pub fn chatmix_sink(&self) -> Option<String> {
        let state = self.state();
        state.chatmix.sink.clone().or(state.parked_sink.clone())
    }

    /// The mixer takes ChatMix and the line-in over (`on`) or gives them back.
    /// Taking over stops the ChatMix loopbacks and the line-in links / DSP;
    /// the default-sink claim and the line-in's input selection stay, so the
    /// mixer's `Hako_Game` and line-in strip pick up where they left off.
    /// Giving back rebuilds both as they were.
    pub fn hand_over(&self, on: bool) -> Result<()> {
        let mut state = self.state();
        if state.handed_over == on {
            return Ok(());
        }
        state.handed_over = on;
        if on {
            state.parked_sink = state.chatmix.sink.clone();
            cleanup_chatmix_children(&mut state.chatmix);
            if let Some(aux) = state.aux.as_mut() {
                cleanup_aux_parts(&mut aux.handles, &mut aux.dsp);
            }
            info!("ChatMix and AUX handed over to the mixer");
            seen(&mut state);
            return Ok(());
        }
        state.mixer_game = false;
        let mut result = Ok(());
        if let Some(sink) = state.parked_sink.take() {
            if !state.chatmix_suppressed && !state.chatmix.active() {
                let chatmix =
                    start_chatmix_locked(&mut state, &sink).and_then(|()| match state.dial {
                        Some((game, chat)) => set_chatmix_volumes_now(game, chat)
                            .map(|()| state.chatmix.volumes = Some((game, chat))),
                        None => Ok(()),
                    });
                record_result(&mut state.chatmix_error, &chatmix);
                result = chatmix;
            }
        }
        if state.aux.is_some() {
            let aux = rebuild_aux_locked(&mut state);
            record_result(&mut state.aux_error, &aux);
            result = result.and(aux);
        }
        info!("ChatMix and AUX back from the mixer");
        seen(&mut state);
        result
    }

    /// While the mixer owns ChatMix: whether it runs a `Hako_Game` sink. With
    /// the default-sink preference on, that sink is claimed like ChatMix's.
    pub fn mixer_default(&self, game: bool) -> Result<()> {
        let mut state = self.state();
        if !state.handed_over {
            return Ok(());
        }
        state.mixer_game = game;
        let held = state.defaults.held() == Some(CHATMIX_GAME);
        let result = if game && state.chatmix_set_default && !held {
            claim_default(&mut state.defaults, DefaultOwner::ChatMix, CHATMIX_GAME)
        } else if !game && held {
            release_default(&mut state.defaults, DefaultOwner::ChatMix)
        } else {
            Ok(())
        };
        record_result(&mut state.chatmix_error, &result);
        seen(&mut state);
        result
    }

    /// Called (under the audio lock: it must not block) with each new ChatMix /
    /// AUX failure of background work (retries, healing, the ChatMix dial).
    /// Failures of calls whose caller gets the error are not passed. Set once.
    pub fn on_error(&self, hook: impl Fn(String) + Send + Sync + 'static) {
        let _ = self.on_error.set(Box::new(hook));
    }

    /// After background work: hand each part's new error to `on_error`.
    fn report(&self, s: &mut AudioState) {
        for (part, error, reported) in [
            ("ChatMix", &s.chatmix_error, &mut s.chatmix_reported),
            ("AUX", &s.aux_error, &mut s.aux_reported),
        ] {
            if is_new_error(reported, error.as_deref()) {
                if let (Some(hook), Some(e)) = (self.on_error.get(), error) {
                    hook(format!("{part}: {e}"));
                }
            }
        }
    }

    fn state(&self) -> StateGuard<'_> {
        let state = self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.chatmix_error = Some("audio state mutex was poisoned; recovered state".into());
            state
        });
        StateGuard {
            state,
            status: &self.status,
        }
    }

    /// The last published status, refreshed first when no audio work runs
    /// (a child may have exited since).
    fn status(&self) -> Status {
        match self.state.try_lock() {
            Ok(state) => drop(StateGuard {
                state,
                status: &self.status,
            }),
            Err(TryLockError::Poisoned(_)) => drop(self.state()),
            Err(TryLockError::WouldBlock) => {}
        }
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn chatmix_active(&self) -> bool {
        self.status().chatmix_active
    }

    pub fn aux_active(&self) -> bool {
        self.status().aux_active
    }

    pub fn aux_processing_active(&self) -> bool {
        self.status().aux_processing_active
    }

    pub fn aux_dsp_active(&self) -> bool {
        self.aux_processing_active()
    }

    /// `node.name` of the source AUX captures, while AUX is up.
    pub fn aux_capture_node(&self) -> Option<String> {
        self.status().aux_capture
    }

    /// `node.name` AUX plays into while up: the DSP node when processing is
    /// on, else the sink.
    pub fn aux_output_node(&self) -> Option<String> {
        self.status().aux_output
    }

    pub fn last_error(&self) -> Option<String> {
        let status = self.status();
        match (status.chatmix_error, status.aux_error) {
            (Some(c), Some(a)) => Some(format!("ChatMix: {c}; AUX: {a}")),
            (c, a) => c.or(a),
        }
    }

    pub fn healthy(&self) -> bool {
        let status = self.status();
        status.chatmix_error.is_none() && status.aux_error.is_none()
    }

    /// `node.latency` (samples at 48 kHz) of the ChatMix loopbacks and the AUX
    /// filter-chain. A change rebuilds whichever is up (a brief dropout).
    pub fn set_latency(&self, samples: u32) -> Result<()> {
        let mut state = self.state();
        if state.latency == samples {
            return Ok(());
        }
        state.latency = samples;
        // Built with the new latency when the mixer gives them back.
        if state.handed_over {
            return Ok(());
        }
        let mut result = Ok(());
        if state.chatmix.active() {
            let chatmix = restart_chatmix_locked(&mut state);
            record_result(&mut state.chatmix_error, &chatmix);
            result = chatmix;
        }
        if state
            .aux
            .as_ref()
            .is_some_and(|aux| aux.alsa_card.is_some())
        {
            let aux = rebuild_aux_locked(&mut state);
            record_result(&mut state.aux_error, &aux);
            result = result.and(aux);
        }
        seen(&mut state);
        result
    }

    /// Bring ChatMix up on the headset's playback sink: the first node name
    /// matching `sink_regexes`, tried in order (most specific first), unless
    /// the kill-switch suppressed it. Already up (for any headset): no-op.
    pub fn ensure_chatmix(&self, sink_regexes: &[&str]) -> Result<()> {
        let mut state = self.state();
        if state.chatmix.active() || state.chatmix_suppressed || state.handed_over {
            return Ok(());
        }

        let result = ensure_chatmix_locked(&mut state, sink_regexes);
        record_result(&mut state.chatmix_error, &result);
        self.report(&mut state);
        result
    }

    /// Allow ChatMix again after `reset_all` (the headset (re)connected).
    pub fn resume_chatmix(&self) {
        self.state().chatmix_suppressed = false;
    }

    /// Whether ChatMix owns the default sink; applied live when it is up.
    pub fn set_chatmix_default(&self, on: bool) -> Result<()> {
        let mut state = self.state();
        if state.chatmix_set_default == on {
            return Ok(());
        }
        state.chatmix_set_default = on;
        if !state.chatmix.active() && !state.mixer_game {
            return Ok(());
        }
        let result = if on {
            claim_default(&mut state.defaults, DefaultOwner::ChatMix, CHATMIX_GAME)
        } else {
            release_default(&mut state.defaults, DefaultOwner::ChatMix)
        };
        record_result(&mut state.chatmix_error, &result);
        seen(&mut state);
        result
    }

    pub fn set_chatmix_volumes(&self, game: f32, chat: f32) -> Result<()> {
        let mut state = self.state();
        let (game, chat) = (game.clamp(0.0, 1.0), chat.clamp(0.0, 1.0));
        state.dial = Some((game, chat));
        if state.handed_over {
            if let Some(hook) = self.on_dial.get() {
                hook(game, chat);
            }
            return Ok(());
        }
        if !state.chatmix.active() || state.chatmix.volumes == Some((game, chat)) {
            return Ok(());
        }
        let result = set_chatmix_volumes_now(game, chat);
        state.chatmix.volumes = result.is_ok().then_some((game, chat));
        record_result(&mut state.chatmix_error, &result);
        self.report(&mut state);
        result
    }

    pub fn destroy_chatmix(&self) -> Result<()> {
        let mut state = self.state();
        let result = destroy_chatmix_locked(&mut state);
        record_result(&mut state.chatmix_error, &result);
        seen(&mut state);
        result
    }

    pub fn aux_enable(&self, cfg: &AuxConfig, chain: &AuxEffectChain) -> Result<()> {
        let mut state = self.state();
        if state.aux.is_some() {
            return Ok(());
        }
        // Requested only: built when the mixer gives the line-in back.
        if state.handed_over {
            state.aux = Some(AuxState::pending(cfg.clone(), chain.clone()));
            return Ok(());
        }
        let result = aux_enable_locked(&mut state, cfg, chain);
        record_result(&mut state.aux_error, &result);
        seen(&mut state);
        result
    }

    /// Keeps AUX requested after a failed `aux_enable` so `heal_aux` retries
    /// it. That enable was automatic, so its error goes to `on_error`.
    pub fn aux_keep_pending(&self, cfg: &AuxConfig, chain: &AuxEffectChain) {
        let mut state = self.state();
        if state.aux.is_none() {
            state.aux = Some(AuxState::pending(cfg.clone(), chain.clone()));
        }
        state.aux_reported = None;
        self.report(&mut state);
    }

    /// PipeWire silently drops links when a node is recreated
    /// (suspend/resume, device replug, server restart), and the DSP process can
    /// exit. Rebuild the graph whenever any part of it is gone; on failure stay
    /// pending so the next call retries once the nodes are back.
    pub fn heal_aux(&self) {
        let mut state = self.state();
        if state.handed_over {
            return;
        }
        let Some(aux) = state.aux.as_mut() else {
            return;
        };
        if aux_graph_intact(aux) {
            return;
        }
        if aux.alsa_card.is_some() {
            warn!("AUX graph broken; rebuilding");
        }
        let result = rebuild_aux_locked(&mut state);
        record_result(&mut state.aux_error, &result);
        self.report(&mut state);
    }

    pub fn aux_disable(&self) -> Result<()> {
        let mut state = self.state();
        let result = aux_disable_locked(&mut state);
        record_result(&mut state.aux_error, &result);
        seen(&mut state);
        result
    }

    /// Updates the selected capture card without rebuilding the PipeWire graph.
    pub fn set_aux_capture_percent(&self, percent: u8) -> Result<()> {
        let mut state = self.state();
        let result = match state.aux.as_mut() {
            Some(aux) => match aux.alsa_card.as_deref() {
                Some(card) => set_capture_percent(card, percent).map(|()| {
                    aux.cfg.capture_percent = percent.min(100);
                }),
                None => {
                    aux.cfg.capture_percent = percent.min(100);
                    Ok(())
                }
            },
            None => bail_result("AUX is not active"),
        };
        record_result(&mut state.aux_error, &result);
        seen(&mut state);
        result
    }

    pub fn reconfigure_aux(&self, cfg: &AuxConfig, chain: &AuxEffectChain) -> Result<()> {
        let mut state = self.state();
        // The live capture level belongs to the headset setting (see
        // set_aux_capture_percent), not config.toml; keep it across rebuilds.
        let mut cfg = cfg.clone();
        if let Some(aux) = state.aux.as_ref() {
            cfg.capture_percent = aux.cfg.capture_percent;
        }
        let cfg = &cfg;
        if state.handed_over {
            if let Some(aux) = state.aux.as_mut() {
                aux.cfg = cfg.clone();
                aux.chain = chain.clone();
            }
            return Ok(());
        }
        if let Some(aux) = state.aux.as_mut() {
            if aux.cfg.capture_name_regex == cfg.capture_name_regex
                && aux.cfg.sink_name_regex == cfg.sink_name_regex
                && aux.cfg.set_default_sink == cfg.set_default_sink
                && same_effect_topology(&aux.chain, chain)
            {
                let result = update_effect_parameters(aux, chain);
                if result.is_ok() {
                    aux.cfg = cfg.clone();
                    aux.chain = chain.clone();
                }
                record_result(&mut state.aux_error, &result);
                seen(&mut state);
                return result;
            }
        }
        let old = state
            .aux
            .as_ref()
            .map(|aux| (aux.cfg.clone(), aux.chain.clone()));

        let disable_error = aux_disable_locked(&mut state).err();
        let enable_result = if let Some(error) = disable_error {
            Err(error.context("disable AUX before reconfiguration"))
        } else {
            aux_enable_locked(&mut state, cfg, chain)
        };
        let result = match enable_result {
            Ok(()) => Ok(()),
            Err(new_error) => {
                if let Some((old_cfg, old_chain)) = &old {
                    match aux_enable_locked(&mut state, old_cfg, old_chain) {
                        Ok(()) => Err(new_error.context("new AUX configuration failed; restored previous configuration")),
                        Err(rollback_error) => Err(anyhow!(
                            "new AUX configuration failed: {new_error:#}; rollback failed: {rollback_error:#}"
                        )),
                    }
                } else {
                    Err(new_error)
                }
            }
        };
        // AUX stays requested: heal_aux retries once the nodes are back.
        if result.is_err() && state.aux.is_none() && old.is_some() {
            state.aux = Some(AuxState::pending(cfg.clone(), chain.clone()));
        }
        record_result(&mut state.aux_error, &result);
        seen(&mut state);
        result
    }

    /// Kill-switch: tear down AUX and ChatMix, restore the default sink, and
    /// keep ChatMix down until `resume_chatmix`.
    pub fn reset_all(&self) -> Result<()> {
        let mut state = self.state();
        state.chatmix_suppressed = true;
        state.mixer_game = false;
        let aux = aux_disable_locked(&mut state);
        let mut chatmix = destroy_chatmix_locked(&mut state);
        kill_orphans_locked(&state);
        if let Some(original) = state.defaults.restore() {
            chatmix = chatmix.and(set_default_sink(&original));
        }
        record_result(&mut state.aux_error, &aux);
        record_result(&mut state.chatmix_error, &chatmix);
        seen(&mut state);
        aux.and(chatmix)
    }

    /// Kill ChatMix / AUX processes a crashed hakod left behind (they match
    /// by name, so this also hits another running hakod's). No-op while this
    /// instance owns any. Startup and the kill-switch only.
    pub fn kill_orphans(&self) {
        kill_orphans_locked(&self.state());
    }

    /// Versions before the native engine made AUX links with `pw-link`, which
    /// outlive their creator: remove capture -> sink links matching the AUX
    /// config. No-op while this service owns AUX, or when either node is
    /// absent (nothing can be linked).
    pub fn remove_orphan_aux_links(&self, cfg: &AuxConfig) -> Result<()> {
        let state = self.state();
        if state.aux.is_some() {
            return Ok(());
        }
        let (Ok((capture, mono)), Ok(sink)) = (
            find_source_node(&cfg.capture_name_regex),
            find_sink_node_name(&cfg.sink_name_regex),
        ) else {
            return Ok(());
        };
        let engine = engine()?;
        let orphans: Vec<u32> = engine.with_graph(|graph| {
            capture_links(&capture, mono, &sink)
                .iter()
                .filter_map(|(output, input)| {
                    let (_, output) = graph_port(graph, output, Direction::Out)?;
                    let (_, input) = graph_port(graph, input, Direction::In)?;
                    Some(graph.links_between(output, input))
                })
                .flatten()
                .collect()
        });
        for id in orphans {
            engine.destroy(id)?;
        }
        Ok(())
    }
}

impl Default for AudioService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioService {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Only what this instance owns; process-wide sweeps are `kill_orphans`.
        let _ = aux_disable_locked(state);
        let _ = destroy_chatmix_locked(state);
        if let Some(original) = state.defaults.restore() {
            let _ = set_default_sink(&original);
        }
    }
}

fn kill_orphans_locked(state: &AudioState) {
    let owns_dsp = state.aux.as_ref().is_some_and(|aux| aux.dsp.is_some());
    if state.chatmix.game.is_none() && state.chatmix.chat.is_none() && !owns_dsp {
        kill_loopback_orphan(CHATMIX_GAME);
        kill_loopback_orphan(CHATMIX_CHAT);
        kill_aux_dsp_orphan();
        // The caller tore its own mixer down first (startup: none yet).
        mixer::kill_orphans();
    }
}

fn record_result(slot: &mut Option<String>, result: &Result<()>) {
    *slot = result.as_ref().err().map(|error| format!("{error:#}"));
}

/// After a call whose caller gets the error: nothing left to report.
fn seen(state: &mut AudioState) {
    state.chatmix_reported = state.chatmix_error.clone();
    state.aux_reported = state.aux_error.clone();
}

fn bail_result<T>(message: &str) -> Result<T> {
    Err(anyhow!(message.to_string()))
}

fn ensure_chatmix_locked(state: &mut AudioState, sink_regexes: &[&str]) -> Result<()> {
    let mut found = Err(anyhow!("no ChatMix sink pattern given"));
    for pattern in sink_regexes {
        found = find_sink_node_name(pattern);
        if found.is_ok() {
            break;
        }
    }
    start_chatmix_locked(state, &found?)
}

/// The loopbacks into `sink`, then the default-sink claim if wanted.
fn start_chatmix_locked(state: &mut AudioState, sink: &str) -> Result<()> {
    spawn_chatmix(state, sink)?;
    if state.chatmix_set_default {
        if let Err(error) = claim_default(&mut state.defaults, DefaultOwner::ChatMix, CHATMIX_GAME)
        {
            cleanup_chatmix_children(&mut state.chatmix);
            return Err(error);
        }
    }
    info!("ChatMix sinks ready -> {sink}");
    Ok(())
}

/// Both loopbacks into `sink`, up once their nodes show their ports.
fn spawn_chatmix(state: &mut AudioState, sink: &str) -> Result<()> {
    cleanup_chatmix_children(&mut state.chatmix);
    let mut game = spawn_loopback(CHATMIX_GAME, "hako Game", sink, state.latency)?;
    let mut chat = match spawn_loopback(CHATMIX_CHAT, "hako Chat", sink, state.latency) {
        Ok(child) => child,
        Err(error) => {
            let _ = stop_child(&mut game);
            return Err(error);
        }
    };
    if let Err(error) = wait_for_chatmix_nodes(&mut game, &mut chat) {
        let _ = stop_child(&mut game);
        let _ = stop_child(&mut chat);
        return Err(error);
    }
    state.chatmix = ChatMixState {
        game: Some(game),
        chat: Some(chat),
        sink: Some(sink.to_string()),
        volumes: None,
    };
    Ok(())
}

/// Respawn the loopbacks on the same sink (a new latency). The default-sink
/// claim stays; WirePlumber's configured default is `Hako_Game` all along.
fn restart_chatmix_locked(state: &mut AudioState) -> Result<()> {
    let (Some(sink), volumes) = (state.chatmix.sink.clone(), state.chatmix.volumes) else {
        return Ok(());
    };
    spawn_chatmix(state, &sink)?;
    if let Some((game, chat)) = volumes {
        set_chatmix_volumes_now(game, chat)?;
        state.chatmix.volumes = volumes;
    }
    if state.defaults.held() == Some(CHATMIX_GAME) {
        set_default_sink(CHATMIX_GAME)?;
    }
    info!(
        "ChatMix rebuilt for latency {}",
        node_latency(state.latency)
    );
    Ok(())
}

fn set_chatmix_volumes_now(game: f32, chat: f32) -> Result<()> {
    set_node_volume(resolve_node_id(CHATMIX_GAME)?, game)?;
    set_node_volume(resolve_node_id(CHATMIX_CHAT)?, chat)
}

fn destroy_chatmix_locked(state: &mut AudioState) -> Result<()> {
    let mut first_error = release_default(&mut state.defaults, DefaultOwner::ChatMix).err();
    state.chatmix.sink = None;
    state.chatmix.volumes = None;
    if let Some(mut child) = state.chatmix.game.take() {
        if let Err(error) = stop_child(&mut child) {
            first_error.get_or_insert(error);
        }
    }
    if let Some(mut child) = state.chatmix.chat.take() {
        if let Err(error) = stop_child(&mut child) {
            first_error.get_or_insert(error);
        }
    }
    info!("ChatMix destroyed");
    first_error.map_or(Ok(()), Err)
}

fn cleanup_chatmix_children(chatmix: &mut ChatMixState) {
    chatmix.sink = None;
    chatmix.volumes = None;
    for mut child in [chatmix.game.take(), chatmix.chat.take()]
        .into_iter()
        .flatten()
    {
        let _ = stop_child(&mut child);
    }
}

fn aux_enable_locked(
    state: &mut AudioState,
    cfg: &AuxConfig,
    chain: &AuxEffectChain,
) -> Result<()> {
    chain.validate().map_err(anyhow::Error::msg)?;
    let (capture, mono) = find_source_node(&cfg.capture_name_regex)?;
    let sink = find_sink_node_name(&cfg.sink_name_regex)?;
    let alsa_card = node_alsa_card(&capture)?;
    let previous_input_source = current_input_source(&alsa_card).ok();

    let has_effects = chain.effects.iter().any(|effect| effect.enabled);
    let mut dsp = None;
    let destination = if has_effects {
        let lsp = lsp_for(chain)?;
        let config = generate_dsp_config(
            AUX_DSP_SINK,
            Some(&sink),
            chain,
            state.latency,
            lsp.as_deref(),
        )?;
        let path = dsp_conf_path();
        write_conf(&path, &config)?;
        kill_aux_dsp_orphan();
        let mut child = spawn_pipewire(&path).context("spawn PipeWire AUX filter graph")?;
        if let Err(error) = wait_for_node(&mut child, AUX_DSP_SINK) {
            let _ = stop_child(&mut child);
            return Err(error);
        }
        dsp = Some(child);
        AUX_DSP_SINK.to_string()
    } else {
        sink.clone()
    };

    let pairs = capture_links(&capture, mono, &destination);
    let mut linked = Vec::new();
    for (source, destination) in &pairs {
        match link_ports(source, destination) {
            Ok(handle) => linked.push(handle),
            Err(error) => {
                cleanup_aux_parts(&mut linked, &mut dsp);
                return Err(error);
            }
        }
    }

    if let Err(error) = set_input_source(&alsa_card, "Line") {
        cleanup_aux_parts(&mut linked, &mut dsp);
        return Err(error);
    }
    if let Err(error) = set_capture_percent(&alsa_card, cfg.capture_percent) {
        if let Some(source) = previous_input_source.as_deref() {
            let _ = set_input_source(&alsa_card, source);
        }
        cleanup_aux_parts(&mut linked, &mut dsp);
        return Err(error);
    }

    if cfg.set_default_sink {
        if let Err(error) = claim_default(&mut state.defaults, DefaultOwner::Aux, &sink) {
            if let Some(source) = previous_input_source.as_deref() {
                let _ = set_input_source(&alsa_card, source);
            }
            cleanup_aux_parts(&mut linked, &mut dsp);
            return Err(error);
        }
    }

    info!("AUX enabled {capture} -> {destination}");
    state.aux = Some(AuxState {
        links: pairs,
        handles: linked,
        dsp,
        capture,
        sink,
        alsa_card: Some(alsa_card),
        previous_input_source,
        cfg: cfg.clone(),
        chain: chain.clone(),
    });
    Ok(())
}

fn aux_disable_locked(state: &mut AudioState) -> Result<()> {
    let mut first_error = release_default(&mut state.defaults, DefaultOwner::Aux).err();
    if let Some(mut aux) = state.aux.take() {
        aux.handles.clear();
        if let Some(mut child) = aux.dsp.take() {
            if let Err(error) = stop_child(&mut child) {
                first_error.get_or_insert(error);
            }
        }
        if let (Some(card), Some(source)) = (aux.alsa_card.as_deref(), aux.previous_input_source) {
            if let Err(error) = set_input_source(card, &source) {
                first_error.get_or_insert(error);
            }
        }
    }
    info!("AUX disabled");
    first_error.map_or(Ok(()), Err)
}

/// Tear AUX down and build it again from its own config; on failure it stays
/// pending so `heal_aux` retries.
fn rebuild_aux_locked(state: &mut AudioState) -> Result<()> {
    let Some(aux) = state.aux.as_ref() else {
        return Ok(());
    };
    let (cfg, chain) = (aux.cfg.clone(), aux.chain.clone());
    if aux.alsa_card.is_some() {
        let _ = aux_disable_locked(state);
    }
    let result = aux_enable_locked(state, &cfg, &chain);
    if result.is_err() {
        state.aux = Some(AuxState::pending(cfg, chain));
    }
    result
}

fn aux_graph_intact(aux: &mut AuxState) -> bool {
    let Some(card) = aux.alsa_card.as_deref() else {
        return false;
    };
    if let Some(dsp) = aux.dsp.as_mut() {
        if !matches!(dsp.try_wait(), Ok(None)) {
            return false;
        }
    }
    // Can't see the graph right now; don't thrash it.
    let Some(present) = engine()
        .ok()
        .and_then(|engine| engine.with_graph(|graph| graph.connected.then(|| graph.link_names())))
    else {
        return true;
    };
    let mut expected = aux.links.clone();
    if aux.dsp.is_some() {
        for channel in ["FL", "FR"] {
            expected.push((
                format!("{AUX_DSP_SINK}_out:output_{channel}"),
                format!("{}:playback_{channel}", aux.sink),
            ));
        }
    }
    expected.iter().all(|link| present.contains(link))
        && current_input_source(card).is_ok_and(|source| source == "Line")
}

fn cleanup_aux_parts(links: &mut Vec<LinkHandle>, dsp: &mut Option<Child>) {
    links.clear();
    if let Some(mut child) = dsp.take() {
        let _ = stop_child(&mut child);
    }
}

/// Make `sink` the default for `owner`, remembering the session's original.
fn claim_default(defaults: &mut DefaultState, owner: DefaultOwner, sink: &str) -> Result<()> {
    let original = if defaults.claims.is_empty() {
        current_default_sink().ok()
    } else {
        None
    };
    set_default_sink(sink)?;
    defaults.claim(owner, sink.to_string(), original);
    Ok(())
}

fn release_default(defaults: &mut DefaultState, owner: DefaultOwner) -> Result<()> {
    let ours = defaults.held().map(str::to_string);
    if let Some(sink) = defaults.release(owner) {
        let moved = ours.as_deref().is_some_and(|ours| {
            user_moved_default(
                ours,
                resolve_node_id(ours).is_ok(),
                current_default_sink().ok().as_deref(),
            )
        });
        if moved {
            info!("default output changed by the user; not restoring {sink}");
        } else {
            set_default_sink(&sink)?;
        }
    }
    Ok(())
}

/// The user picked another output while `ours` was the default. When `ours`
/// no longer exists the server chose the fallback itself, so restoring is right.
fn user_moved_default(ours: &str, ours_exists: bool, current: Option<&str>) -> bool {
    ours_exists && current.is_some_and(|current| current != ours)
}

/// `node.latency` for `samples` at 48 kHz.
fn node_latency(samples: u32) -> String {
    format!("{samples}/48000")
}

/// `pw-loopback` capture / playback properties. Values are quoted: an
/// unquoted space (`node.description=hako Game`) is a syntax error and
/// pw-loopback exits 255. WirePlumber links the playback side to its
/// `node.target` (and again after the headset sink is recreated).
fn loopback_props(
    name: &str,
    description: &str,
    playback_target: &str,
    latency: u32,
) -> (String, String) {
    let latency = node_latency(latency);
    (
        format!(
            "media.class=Audio/Sink node.name=\"{}\" node.description=\"{}\" audio.position=[FL FR] node.latency=\"{latency}\"",
            pw_escape(name),
            pw_escape(description)
        ),
        format!(
            "node.target=\"{}\" node.latency=\"{latency}\"",
            pw_escape(playback_target)
        ),
    )
}

fn spawn_loopback(
    name: &str,
    description: &str,
    playback_target: &str,
    latency: u32,
) -> Result<Child> {
    kill_loopback_orphan(name);
    let (capture_props, playback_props) =
        loopback_props(name, description, playback_target, latency);
    Command::new("pw-loopback")
        .args(["-n", name, "-i", &capture_props, "-o", &playback_props])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn pw-loopback for {name}"))
}

const PLAYBACK: [&str; 2] = ["playback_FL", "playback_FR"];

fn wait_for_chatmix_nodes(game: &mut Child, chat: &mut Child) -> Result<()> {
    let ready = engine()?.wait_for(NODE_WAIT, |graph| {
        ensure_running(game, CHATMIX_GAME)?;
        ensure_running(chat, CHATMIX_CHAT)?;
        Ok([CHATMIX_GAME, CHATMIX_CHAT]
            .iter()
            .all(|node| graph.has_ports(node, Direction::In, &PLAYBACK)))
    })?;
    if !ready {
        bail!("ChatMix nodes did not appear");
    }
    Ok(())
}

/// Wait for a sink `child` creates (its playback ports).
fn wait_for_node(child: &mut Child, node: &str) -> Result<()> {
    let ready = engine()?.wait_for(NODE_WAIT, |graph| {
        ensure_running(child, node)?;
        Ok(graph.has_ports(node, Direction::In, &PLAYBACK))
    })?;
    if !ready {
        bail!("PipeWire node {node} did not appear");
    }
    Ok(())
}

fn ensure_running(child: &mut Child, name: &str) -> Result<()> {
    if let Some(status) = child.try_wait().with_context(|| format!("poll {name}"))? {
        bail!("{name} exited early with {status}");
    }
    Ok(())
}

fn stop_child(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("poll child before stopping")?
        .is_none()
    {
        child.kill().context("kill owned audio child")?;
    }
    child.wait().context("wait for owned audio child")?;
    Ok(())
}

fn reap_exited(child: &mut Option<Child>) {
    // `try_wait()` returns Ok(None) while the child is still running — that is
    // not an exit. Dropping it here used to make ChatMix look dead every tick
    // and respawn pw-loopback in a tight loop.
    let exited = match child.as_mut() {
        Some(process) => matches!(process.try_wait(), Ok(Some(_)) | Err(_)),
        None => false,
    };
    if exited {
        child.take();
    }
}

fn kill_loopback_orphan(name: &str) {
    pkill(&format!("^pw-loopback -n {name} "));
}

fn kill_aux_dsp_orphan() {
    pkill(r"^pipewire -c .*/hako-aux-dsp\.conf$");
}

/// Signals every matching process of the user, a live hakod's children too.
#[cfg(not(test))]
fn pkill(pattern: &str) {
    let _ = Command::new("pkill").args(["-f", pattern]).status();
}

/// Unit tests run next to the user's hakod: count instead of signalling.
#[cfg(test)]
fn pkill(_pattern: &str) {
    tests::PKILLS.with(|count| count.set(count.get() + 1));
}

/// Next to the daemon socket: the per-user runtime dir, never a shared /tmp.
fn dsp_conf_path() -> PathBuf {
    hako_core::default_socket_path().with_file_name("hako-aux-dsp.conf")
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// `$LADSPA_PATH` entries first, then the distributions' usual directories.
fn ladspa_dirs(ladspa_path: Option<&OsStr>) -> Vec<PathBuf> {
    ladspa_path
        .map(|path| std::env::split_paths(path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .chain(LADSPA_DIRS.iter().map(PathBuf::from))
        .collect()
}

fn find_lsp_ladspa(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(LSP_LADSPA))
        .find(|path| path.is_file())
}

/// The LSP LADSPA library if `chain` needs it (an enabled gate or compressor).
fn lsp_for(chain: &AuxEffectChain) -> Result<Option<PathBuf>> {
    let needs_lsp = chain
        .effects
        .iter()
        .any(|effect| effect.enabled && !matches!(effect.kind, AuxEffectKind::Equalizer { .. }));
    if !needs_lsp {
        return Ok(None);
    }
    let dirs = ladspa_dirs(std::env::var_os("LADSPA_PATH").as_deref());
    let Some(lsp) = find_lsp_ladspa(&dirs) else {
        let searched: Vec<_> = dirs.iter().map(|dir| dir.display().to_string()).collect();
        bail!(
            "LSP LADSPA plugins not installed ({LSP_LADSPA} not in {})",
            searched.join(", ")
        );
    };
    Ok(Some(lsp))
}

/// One filter-chain node: the LSP LADSPA plugin when `lsp`, else a PipeWire builtin.
struct DspNode {
    name: String,
    lsp: bool,
    label: &'static str,
    controls: Vec<(&'static str, f32)>,
}

/// A stereo link unit in the linear graph: its nodes plus L/R entry and exit ports.
struct DspStage {
    nodes: Vec<DspNode>,
    inputs: [String; 2],
    outputs: [String; 2],
}

fn lsp_stage(name: String, label: &'static str, controls: Vec<(&'static str, f32)>) -> DspStage {
    DspStage {
        inputs: [format!("{name}:Input L"), format!("{name}:Input R")],
        outputs: [format!("{name}:Output L"), format!("{name}:Output R")],
        nodes: vec![DspNode {
            name,
            lsp: true,
            label,
            controls,
        }],
    }
}

/// Enabled effects in order; EQ bands become one mono biquad per channel.
/// Node names are positional, so `update_effect_parameters` addresses the same nodes.
fn dsp_stages(chain: &AuxEffectChain) -> Vec<DspStage> {
    let mut stages = Vec::new();
    for (index, effect) in chain
        .effects
        .iter()
        .filter(|effect| effect.enabled)
        .enumerate()
    {
        match &effect.kind {
            AuxEffectKind::NoiseGate {
                threshold_db,
                reduction_db,
                attack_ms,
                hold_ms,
                release_ms,
                detect_hpf_hz,
            } => stages.push(lsp_stage(
                format!("fx{index}"),
                "http://lsp-plug.in/plugins/ladspa/gate_stereo",
                vec![
                    ("Curve threshold (G)", db_to_gain(*threshold_db)),
                    ("Reduction (G)", db_to_gain(*reduction_db)),
                    ("Attack (ms)", *attack_ms),
                    ("Hold time (ms)", *hold_ms),
                    ("Release (ms)", *release_ms),
                    ("Sidechain reactivity (ms)", SIDECHAIN_REACTIVITY_MS),
                    // Sidechain HPF: mode 2 = 24 dB/oct, 0 = off.
                    (
                        "High-pass filter mode",
                        if *detect_hpf_hz >= 10.0 { 2.0 } else { 0.0 },
                    ),
                    ("High-pass filter frequency (Hz)", detect_hpf_hz.max(10.0)),
                ],
            )),
            AuxEffectKind::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                knee_db,
                makeup_db,
            } => stages.push(lsp_stage(
                format!("fx{index}"),
                "http://lsp-plug.in/plugins/ladspa/compressor_stereo",
                vec![
                    ("Attack threshold (G)", db_to_gain(*threshold_db)),
                    ("Ratio", *ratio),
                    ("Attack time (ms)", *attack_ms),
                    ("Release time (ms)", *release_ms),
                    ("Knee (G)", db_to_gain(*knee_db)),
                    ("Makeup gain (G)", db_to_gain(*makeup_db)),
                    ("Sidechain reactivity (ms)", SIDECHAIN_REACTIVITY_MS),
                ],
            )),
            AuxEffectKind::Equalizer { bands } => {
                for (band_index, band) in bands.iter().enumerate() {
                    let label = match band.kind {
                        EqBandKind::Peaking => "bq_peaking",
                        EqBandKind::LowShelf => "bq_lowshelf",
                        EqBandKind::HighShelf => "bq_highshelf",
                        EqBandKind::HighPass => "bq_highpass",
                        EqBandKind::LowPass => "bq_lowpass",
                    };
                    let [left, right] =
                        ["l", "r"].map(|channel| format!("fx{index}_b{band_index}_{channel}"));
                    stages.push(DspStage {
                        inputs: [format!("{left}:In"), format!("{right}:In")],
                        outputs: [format!("{left}:Out"), format!("{right}:Out")],
                        nodes: [left, right]
                            .map(|name| DspNode {
                                name,
                                lsp: false,
                                label,
                                controls: vec![
                                    ("Freq", band.freq_hz),
                                    ("Q", band.q),
                                    ("Gain", band.gain_db),
                                ],
                            })
                            .into(),
                    });
                }
            }
        }
    }
    stages
}

/// A `filter.graph`: its nodes, the links between them, and the ports the
/// capture stream's channels enter / the playback stream's channels leave by.
struct FilterGraph {
    nodes: Vec<DspNode>,
    links: Vec<(String, String)>,
    inputs: Vec<String>,
    outputs: Vec<String>,
}

impl FilterGraph {
    /// `stages` one after the other.
    fn chain(stages: Vec<DspStage>) -> Self {
        let mut links = Vec::new();
        for pair in stages.windows(2) {
            links.extend(pair[0].outputs.iter().cloned().zip(pair[1].inputs.clone()));
        }
        Self {
            inputs: stages.first().map_or_else(Vec::new, |s| s.inputs.to_vec()),
            outputs: stages.last().map_or_else(Vec::new, |s| s.outputs.to_vec()),
            nodes: stages.into_iter().flat_map(|stage| stage.nodes).collect(),
            links,
        }
    }
}

/// Write a generated config owner-only (it names the user's devices).
fn write_conf(path: &Path, config: &str) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(config.as_bytes()))
        .with_context(|| format!("write {}", path.display()))
}

/// `pipewire -c conf`: a filter-chain in its own process.
fn spawn_pipewire(conf: &Path) -> Result<Child> {
    Command::new("pipewire")
        .arg("-c")
        .arg(conf)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn pipewire -c {}", conf.display()))
}

/// `lsp`: the LSP LADSPA library, needed when the chain has a gate or compressor.
/// Without a `target` the playback side is never autoconnected (its owner links it).
fn generate_dsp_config(
    sink: &str,
    target: Option<&str>,
    chain: &AuxEffectChain,
    latency: u32,
    lsp: Option<&Path>,
) -> Result<String> {
    chain.validate().map_err(anyhow::Error::msg)?;
    let graph = FilterGraph::chain(dsp_stages(chain));
    if graph.nodes.is_empty() {
        bail!("cannot generate an empty DSP graph");
    }
    let latency = node_latency(latency);
    let sink = pw_escape(sink);
    let route = match target {
        Some(target) => format!("node.target = \"{}\"", pw_escape(target)),
        None => "node.autoconnect = false node.dont-reconnect = true node.dont-fallback = true"
            .to_string(),
    };
    filter_chain_config(
        AUX_DSP_DESC,
        &graph,
        "",
        &format!("node.name = \"{sink}\" media.class = Audio/Sink node.latency = \"{latency}\""),
        &format!(
            "node.name = \"{sink}_out\" {route} node.passive = true node.latency = \"{latency}\""
        ),
        lsp,
    )
}

/// A `pipewire -c` config hosting `graph` in a filter-chain. `capture` and
/// `playback` are the two streams' props (stereo unless they say otherwise),
/// `args` extra module arguments.
fn filter_chain_config(
    description: &str,
    graph: &FilterGraph,
    args: &str,
    capture: &str,
    playback: &str,
    lsp: Option<&Path>,
) -> Result<String> {
    let mut nodes = String::new();
    for node in &graph.nodes {
        let kind = if node.lsp {
            let lsp = lsp.ok_or_else(|| anyhow!("LSP LADSPA plugins not installed"))?;
            format!(
                "type = ladspa name = \"{}\" plugin = \"{}\"",
                pw_escape(&node.name),
                pw_escape(&lsp.to_string_lossy())
            )
        } else {
            format!("type = builtin name = \"{}\"", pw_escape(&node.name))
        };
        let controls: String = node
            .controls
            .iter()
            .map(|(port, value)| format!(" \"{}\" = {value}", pw_escape(port)))
            .collect();
        nodes.push_str(&format!(
            "          {{ {kind} label = \"{}\" control = {{{controls} }} }}\n",
            pw_escape(node.label),
        ));
    }

    let mut links = String::new();
    for (output, input) in &graph.links {
        links.push_str(&format!(
            "          {{ output = \"{}\" input = \"{}\" }}\n",
            pw_escape(output),
            pw_escape(input),
        ));
    }
    let ports = |ports: &[String]| -> String {
        ports
            .iter()
            .map(|port| format!("\"{}\"", pw_escape(port)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let args = if args.is_empty() {
        String::new()
    } else {
        format!("      {args}\n")
    };

    Ok(format!(
        r#"context.properties = {{ log.level = 1 }}
context.spa-libs = {{ audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }}
context.modules = [
  {{ name = libpipewire-module-rt flags = [ ifexists nofail ] }}
  {{ name = libpipewire-module-protocol-native }}
  {{ name = libpipewire-module-client-node }}
  {{ name = libpipewire-module-adapter }}
  {{ name = libpipewire-module-filter-chain
    args = {{
      node.description = "{description}"
      media.name = "{description}"
{args}      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
        inputs = [ {inputs} ]
        outputs = [ {outputs} ]
      }}
      audio.channels = 2
      audio.position = [ FL FR ]
      capture.props = {{ {capture} }}
      playback.props = {{ {playback} }}
    }}
  }}
]
"#,
        description = pw_escape(description),
        inputs = ports(&graph.inputs),
        outputs = ports(&graph.outputs),
    ))
}

fn pw_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            ch if ch.is_control() => Vec::new(),
            ch => vec![ch],
        })
        .collect()
}

/// Capture -> sink port pairs; a mono source feeds both channels.
fn capture_links(source: &str, mono: bool, sink: &str) -> Vec<(String, String)> {
    ["FL", "FR"]
        .map(|channel| {
            let capture = if mono { "MONO" } else { channel };
            (
                format!("{source}:capture_{capture}"),
                format!("{sink}:playback_{channel}"),
            )
        })
        .into()
}

/// Longest `amixer` may run: a hung tool must not hold the audio lock (and
/// with it daemon shutdown) forever.
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// `Command::output` that kills the child after `timeout`.
fn output_within(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let bin = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {bin}"))?;
    let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("wait for {bin}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{bin} did not finish within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Read a child pipe to the end on its own thread (a full pipe would stall the child).
fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

fn run_cmd(bin: &str, args: &[&str]) -> Result<String> {
    let output = output_within(Command::new(bin).args(args), CMD_TIMEOUT)?;
    if !output.status.success() {
        bail!(
            "{bin} {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn current_default_sink() -> Result<String> {
    engine()?
        .with_graph(|graph| graph.default_sink.clone())
        .ok_or_else(|| anyhow!("PipeWire reports no default sink"))
}

fn set_default_sink(name: &str) -> Result<()> {
    resolve_node_id(name)?;
    engine()?.set_default_sink(name)
}

/// `wpctl set-volume <id> <linear>`: WirePlumber's cubic scale, applied to
/// the `{:.3}` argument hako passed. Measured on a `hako-test-` node with
/// pw-dump: 0.5 -> 0.125, 0.25 -> 0.015625, 0.8 -> 0.512, 0.123 -> 0.001861.
fn wpctl_volume(linear: f32) -> f32 {
    let linear = (linear.clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
    linear.powi(3)
}

/// `channelVolumes` of a ChatMix sink (stereo); mute is left alone.
fn set_node_volume(id: u32, linear: f32) -> Result<()> {
    engine()?.set_props(id, engine::volume_pod(wpctl_volume(linear), 2)?)
}

fn resolve_node_id(exact_name: &str) -> Result<u32> {
    engine()?
        .with_graph(|graph| graph.node_id(exact_name))
        .ok_or_else(|| anyhow!("node id not found for {exact_name}"))
}

/// The ALSA card behind a PipeWire node. Its info props (where the card is)
/// may trail the node's appearance briefly.
fn node_alsa_card(node: &str) -> Result<String> {
    let mut card = None;
    engine()?.wait_for(Duration::from_secs(1), |graph| {
        card = graph
            .node_id(node)
            .and_then(|id| alsa_card(&graph.nodes[&id].props));
        Ok(card.is_some())
    })?;
    card.ok_or_else(|| anyhow!("no ALSA card for PipeWire node {node}"))
}

fn alsa_card(props: &HashMap<String, String>) -> Option<String> {
    ["api.alsa.card", "alsa.card"]
        .iter()
        .filter_map(|key| props.get(*key))
        .find(|value| !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()))
        .cloned()
}

fn set_capture_percent(card: &str, percent: u8) -> Result<()> {
    if card.is_empty() || !card.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("invalid ALSA card '{card}'");
    }
    run_cmd(
        "amixer",
        &[
            "-c",
            card,
            "sset",
            "Capture",
            &format!("{}%", percent.min(100)),
        ],
    )
    .map(|_| ())
}

fn current_input_source(card: &str) -> Result<String> {
    let output = run_cmd("amixer", &["-c", card, "sget", "Input Source"])?;
    output
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Item0:")
                .map(|value| value.trim().trim_matches('\'').to_string())
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("ALSA card {card} did not report its input source"))
}

fn set_input_source(card: &str, source: &str) -> Result<()> {
    run_cmd("amixer", &["-c", card, "sset", "Input Source", source]).map(|_| ())
}

fn same_effect_topology(current: &AuxEffectChain, proposed: &AuxEffectChain) -> bool {
    current.effects.len() == proposed.effects.len()
        && current
            .effects
            .iter()
            .zip(&proposed.effects)
            .all(|(left, right)| {
                left.id == right.id
                    && left.enabled == right.enabled
                    && match (&left.kind, &right.kind) {
                        (
                            AuxEffectKind::Equalizer { bands: left },
                            AuxEffectKind::Equalizer { bands: right },
                        ) => {
                            left.len() == right.len()
                                && left.iter().zip(right).all(|(l, r)| l.kind == r.kind)
                        }
                        (left, right) => {
                            std::mem::discriminant(left) == std::mem::discriminant(right)
                        }
                    }
            })
}

fn update_effect_parameters(aux: &mut AuxState, chain: &AuxEffectChain) -> Result<()> {
    chain.validate().map_err(anyhow::Error::msg)?;
    if aux.dsp.is_none() {
        return Ok(());
    }
    let id = resolve_node_id(AUX_DSP_SINK)?;
    engine()?.set_props(id, engine::params_pod(&effect_params(chain))?)
}

/// Every control as filter-chain `params`: `("node:port", value)`.
fn effect_params(chain: &AuxEffectChain) -> Vec<(String, f32)> {
    dsp_stages(chain)
        .iter()
        .flat_map(|stage| &stage.nodes)
        .flat_map(|node| {
            node.controls
                .iter()
                .map(|(port, value)| (format!("{}:{port}", node.name), *value))
        })
        .collect()
}

/// A source (output `capture_FL` + `capture_FR`, or a mono `capture_MONO`)
/// matching `pattern`: `(node name, mono)`.
fn find_source_node(pattern: &str) -> Result<(String, bool)> {
    let regex = Regex::new(pattern).context("bad source regex")?;
    engine()?
        .with_graph(|graph| {
            pick_node(
                &graph.port_names(Direction::Out),
                &regex,
                ["capture_FL", "capture_FR"],
                Some("capture_MONO"),
            )
        })
        .ok_or_else(|| {
            anyhow!("no source matching /{pattern}/ with capture_FL/FR or capture_MONO ports")
        })
}

fn find_sink_node_name(pattern: &str) -> Result<String> {
    let regex = Regex::new(pattern).context("bad sink regex")?;
    engine()?
        .with_graph(|graph| pick_node(&graph.port_names(Direction::In), &regex, PLAYBACK, None))
        .map(|(node, _)| node)
        .ok_or_else(|| anyhow!("no sink matching /{pattern}/ with playback_FL/playback_FR ports"))
}

/// The first node matching `regex` (in port order) with both `stereo`
/// ports, or else its `mono` port: `(node, is_mono)`.
fn pick_node(
    ports: &[(&str, &str)],
    regex: &Regex,
    stereo: [&str; 2],
    mono: Option<&str>,
) -> Option<(String, bool)> {
    let mut candidates: Vec<&str> = Vec::new();
    for (node, _) in ports {
        if regex.is_match(node) && !candidates.contains(node) {
            candidates.push(node);
        }
    }
    let has = |node: &str, port: &str| ports.contains(&(node, port));
    candidates.into_iter().find_map(|node| {
        if stereo.iter().all(|port| has(node, port)) {
            Some((node.to_string(), false))
        } else if mono.is_some_and(|port| has(node, port)) {
            Some((node.to_string(), true))
        } else {
            None
        }
    })
}

/// `node:port` in the cache: `(node id, port id)`.
fn graph_port(graph: &engine::Graph, name: &str, direction: Direction) -> Option<(u32, u32)> {
    let (node, port) = name.rsplit_once(':')?;
    graph.port(node, port, direction)
}

/// Link `output` to `input` (`node:port` names) for as long as the handle
/// lives. A link someone else left there (an older hako's lingering
/// `pw-link` link) is replaced by one this process owns.
fn link_ports(output: &str, input: &str) -> Result<LinkHandle> {
    let engine = engine()?;
    let (ports, existing) = engine.with_graph(|graph| {
        let ports =
            graph_port(graph, output, Direction::Out).zip(graph_port(graph, input, Direction::In));
        let existing = ports.map_or_else(Vec::new, |((_, out), (_, inp))| {
            graph.links_between(out, inp)
        });
        (ports, existing)
    });
    let Some((out, inp)) = ports else {
        bail!("link {output} -> {input}: port not found");
    };
    for id in existing {
        let _ = engine.destroy(id);
    }
    engine
        .link(out, inp)
        .with_context(|| format!("link {output} -> {input}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::ipc::{AuxEffect, AuxEffectKind, EqBand};

    fn chain(effects: Vec<AuxEffect>) -> AuxEffectChain {
        AuxEffectChain { effects }
    }

    fn effect(id: &str, enabled: bool, kind: AuxEffectKind) -> AuxEffect {
        AuxEffect {
            id: id.into(),
            enabled,
            kind,
        }
    }

    fn eq(bands: &[(EqBandKind, f32)]) -> AuxEffectKind {
        AuxEffectKind::Equalizer {
            bands: bands
                .iter()
                .map(|&(kind, freq_hz)| EqBand {
                    kind,
                    freq_hz,
                    gain_db: 3.0,
                    q: 0.7,
                })
                .collect(),
        }
    }

    fn lsp() -> Option<&'static Path> {
        Some(Path::new("/usr/lib/ladspa/lsp-plugins-ladspa.so"))
    }

    fn gate_eq_compressor() -> AuxEffectChain {
        chain(vec![
            effect("gate", true, AuxEffectKind::default_gate()),
            effect(
                "eq",
                true,
                eq(&[(EqBandKind::HighPass, 80.0), (EqBandKind::Peaking, 1000.0)]),
            ),
            effect("compressor", true, AuxEffectKind::default_compressor()),
        ])
    }

    #[test]
    fn db_conversion() {
        assert!((db_to_gain(0.0) - 1.0).abs() < f32::EPSILON);
        assert!((db_to_gain(-20.0) - 0.1).abs() < 0.00001);
    }

    #[test]
    fn config_preserves_enabled_order_and_repeated_effects() {
        let config = generate_dsp_config(
            AUX_DSP_SINK,
            Some("sink\"name"),
            &chain(vec![
                effect("first-gate", true, AuxEffectKind::default_gate()),
                effect("disabled", false, AuxEffectKind::default_compressor()),
                effect("second-gate", true, AuxEffectKind::default_gate()),
            ]),
            256,
            lsp(),
        )
        .unwrap();

        assert_eq!(config.matches("gate_stereo").count(), 2);
        assert!(!config.contains("compressor_stereo"));
        assert!(config.contains("output = \"fx0:Output L\" input = \"fx1:Input L\""));
        assert!(config.contains("output = \"fx0:Output R\" input = \"fx1:Input R\""));
        assert!(config.contains("inputs = [ \"fx0:Input L\" \"fx0:Input R\" ]"));
        assert!(config.contains("outputs = [ \"fx1:Output L\" \"fx1:Output R\" ]"));
        assert!(config.contains("node.name = \"Hako_AUX_DSP\" media.class"));
        assert!(config.contains("node.target = \"sink\\\"name\""));
        // Both sides of the filter-chain request the latency setting.
        assert_eq!(config.matches("node.latency = \"256/48000\"").count(), 2);
        assert!(config.contains("plugin = \"/usr/lib/ladspa/lsp-plugins-ladspa.so\""));
        // Gate and compressor need the LSP library.
        let gate = chain(vec![effect("gate", true, AuxEffectKind::default_gate())]);
        assert!(generate_dsp_config(AUX_DSP_SINK, Some("t"), &gate, 256, None).is_err());
        assert!(!config.contains("node.autoconnect"));

        // Untargeted (the latency test): nothing routes the playback side.
        let config = generate_dsp_config("x", None, &gate, 256, lsp()).unwrap();
        assert!(!config.contains("node.target"));
        assert!(config.contains(
            "node.name = \"x_out\" node.autoconnect = false node.dont-reconnect = true node.dont-fallback = true node.passive = true"
        ));
    }

    #[test]
    fn config_maps_all_gate_and_compressor_params() {
        let config = generate_dsp_config(
            "Test",
            Some("target"),
            &chain(vec![
                effect(
                    "gate",
                    true,
                    AuxEffectKind::NoiseGate {
                        threshold_db: -20.0,
                        reduction_db: -40.0,
                        attack_ms: 2.0,
                        hold_ms: 30.0,
                        release_ms: 250.0,
                        detect_hpf_hz: 0.0,
                    },
                ),
                effect(
                    "compressor",
                    true,
                    AuxEffectKind::Compressor {
                        threshold_db: 0.0,
                        ratio: 4.0,
                        attack_ms: 10.0,
                        release_ms: 120.0,
                        knee_db: -20.0,
                        makeup_db: 20.0,
                    },
                ),
            ]),
            256,
            lsp(),
        )
        .unwrap();

        assert!(config.contains(
            "control = { \"Curve threshold (G)\" = 0.1 \"Reduction (G)\" = 0.01 \"Attack (ms)\" = 2 \"Hold time (ms)\" = 30 \"Release (ms)\" = 250 \"Sidechain reactivity (ms)\" = 10 \"High-pass filter mode\" = 0 \"High-pass filter frequency (Hz)\" = 10 }"
        ));
        assert!(config.contains(
            "control = { \"Attack threshold (G)\" = 1 \"Ratio\" = 4 \"Attack time (ms)\" = 10 \"Release time (ms)\" = 120 \"Knee (G)\" = 0.1 \"Makeup gain (G)\" = 10 \"Sidechain reactivity (ms)\" = 10 }"
        ));
    }

    #[test]
    fn config_builds_stereo_biquad_chain_for_eq() {
        let config = generate_dsp_config(
            AUX_DSP_SINK,
            Some("target"),
            &gate_eq_compressor(),
            256,
            lsp(),
        )
        .unwrap();

        let order = [
            "gate_stereo",
            "name = \"fx1_b0_l\" label = \"bq_highpass\"",
            "name = \"fx1_b0_r\" label = \"bq_highpass\"",
            "name = \"fx1_b1_l\" label = \"bq_peaking\"",
            "name = \"fx1_b1_r\" label = \"bq_peaking\"",
            "compressor_stereo",
        ]
        .map(|needle| {
            config
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        });
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(config.contains(
            "type = builtin name = \"fx1_b1_r\" label = \"bq_peaking\" control = { \"Freq\" = 1000 \"Q\" = 0.7 \"Gain\" = 3 }"
        ));
        for link in [
            "output = \"fx0:Output L\" input = \"fx1_b0_l:In\"",
            "output = \"fx0:Output R\" input = \"fx1_b0_r:In\"",
            "output = \"fx1_b0_l:Out\" input = \"fx1_b1_l:In\"",
            "output = \"fx1_b0_r:Out\" input = \"fx1_b1_r:In\"",
            "output = \"fx1_b1_l:Out\" input = \"fx2:Input L\"",
            "output = \"fx1_b1_r:Out\" input = \"fx2:Input R\"",
        ] {
            assert!(config.contains(link), "missing link {link}");
        }
        assert_eq!(config.matches("{ output = ").count(), 6);

        let eq_only = chain(vec![effect(
            "eq",
            true,
            eq(&[(EqBandKind::LowShelf, 100.0)]),
        )]);
        // EQ alone needs no LADSPA library.
        let config =
            generate_dsp_config(AUX_DSP_SINK, Some("target"), &eq_only, 128, None).unwrap();
        assert!(config.contains("node.latency = \"128/48000\""));
        assert!(config.contains("inputs = [ \"fx0_b0_l:In\" \"fx0_b0_r:In\" ]"));
        assert!(config.contains("outputs = [ \"fx0_b0_l:Out\" \"fx0_b0_r:Out\" ]"));
        assert!(!config.contains("{ output = "));
    }

    #[test]
    fn config_rejects_invalid_or_empty_chain() {
        let config = |chain: &AuxEffectChain| {
            generate_dsp_config(AUX_DSP_SINK, Some("t"), chain, 256, lsp())
        };
        assert!(config(&AuxEffectChain::default()).is_err());
        let disabled = chain(vec![effect("gate", false, AuxEffectKind::default_gate())]);
        assert!(config(&disabled).is_err());
        let empty_eq = chain(vec![effect("eq", true, eq(&[]))]);
        assert!(config(&empty_eq).is_err());
    }

    #[test]
    fn live_params_cover_every_control() {
        // The pairs as `pw-cli s <id> Props` used to get them.
        let pairs: String = effect_params(&gate_eq_compressor())
            .iter()
            .map(|(key, value)| format!(" \"{key}\" {value}"))
            .collect();
        let params = format!("{{ params = [{pairs} ] }}");
        assert!(params.starts_with("{ params = [ \"fx0:Curve threshold (G)\" "));
        assert!(params.ends_with(" ] }"));
        for key in [
            "\"fx0:Reduction (G)\" ",
            "\"fx0:Attack (ms)\" 1 ",
            "\"fx0:Hold time (ms)\" 5 ",
            "\"fx0:Release (ms)\" 150 ",
            "\"fx1_b0_l:Freq\" 80 ",
            "\"fx1_b0_r:Freq\" 80 ",
            "\"fx1_b1_l:Q\" 0.7 ",
            "\"fx1_b1_r:Gain\" 3 ",
            "\"fx2:Attack threshold (G)\" ",
            "\"fx2:Ratio\" 3 ",
            "\"fx2:Attack time (ms)\" 5 ",
            "\"fx2:Release time (ms)\" 150 ",
            "\"fx2:Knee (G)\" ",
            "\"fx2:Makeup gain (G)\" 1 ",
        ] {
            assert!(params.contains(key), "missing {key} in {params}");
        }
        // 8 gate + 2 bands * 2 channels * 3 + 7 compressor (incl. detector ports)
        assert_eq!(params.matches("\" ").count(), 27);
    }

    #[test]
    fn node_picker_requires_exact_stereo_or_mono_ports() {
        let ports = [
            ("other", "capture_FL"),
            ("line", "capture_FL"),
            ("line", "capture_FR"),
            ("line", "capture_AUX"),
            ("mic", "capture_MONO"),
        ];
        let pick =
            |pattern, stereo, mono| pick_node(&ports, &Regex::new(pattern).unwrap(), stereo, mono);
        let capture = ["capture_FL", "capture_FR"];
        assert_eq!(pick("^line$", capture, None), Some(("line".into(), false)));
        assert_eq!(pick("^line$", PLAYBACK, None), None);
        // A half-stereo node is skipped for the next candidate.
        assert_eq!(
            pick("^(other|line)$", capture, None),
            Some(("line".into(), false))
        );
        // Mono sources only where the caller accepts them (AUX capture).
        assert_eq!(pick("^mic$", capture, None), None);
        assert_eq!(
            pick("^mic$", capture, Some("capture_MONO")),
            Some(("mic".into(), true))
        );
        assert_eq!(
            pick("^(line|mic)$", capture, Some("capture_MONO")),
            Some(("line".into(), false))
        );
    }

    #[test]
    fn mono_capture_feeds_both_channels() {
        assert_eq!(
            capture_links("mic", true, "sink"),
            [
                (
                    "mic:capture_MONO".to_string(),
                    "sink:playback_FL".to_string()
                ),
                (
                    "mic:capture_MONO".to_string(),
                    "sink:playback_FR".to_string()
                ),
            ]
        );
        assert_eq!(
            capture_links("line", false, "sink"),
            [
                (
                    "line:capture_FL".to_string(),
                    "sink:playback_FL".to_string()
                ),
                (
                    "line:capture_FR".to_string(),
                    "sink:playback_FR".to_string()
                ),
            ]
        );
    }

    #[test]
    fn config_string_escaping() {
        assert_eq!(pw_escape("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn loopback_props_quote_values_with_spaces() {
        let (capture, playback) = loopback_props("Hako_Game", "hako Game", "alsa_output.x", 256);
        assert!(
            capture.contains("node.description=\"hako Game\""),
            "{capture}"
        );
        assert!(capture.contains("node.name=\"Hako_Game\""), "{capture}");
        assert!(capture.contains("node.latency=\"256/48000\""), "{capture}");
        assert_eq!(
            playback,
            "node.target=\"alsa_output.x\" node.latency=\"256/48000\""
        );
    }

    #[test]
    fn parses_selected_alsa_card_only() {
        let props = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>()
        };
        let node = props(&[("api.alsa.card", "3"), ("device.id", "42")]);
        assert_eq!(alsa_card(&node).as_deref(), Some("3"));
        assert_eq!(alsa_card(&props(&[("device.id", "42")])), None);
        assert_eq!(alsa_card(&props(&[("api.alsa.card", "hw:3")])), None);
        assert_eq!(
            alsa_card(&props(&[("alsa.card", "1")])).as_deref(),
            Some("1")
        );
    }

    #[test]
    fn effect_topology_ignores_parameter_only_changes() {
        let current = gate_eq_compressor();
        let mut changed = current.clone();
        changed.effects[0].kind = AuxEffectKind::NoiseGate {
            threshold_db: -30.0,
            reduction_db: -20.0,
            attack_ms: 3.0,
            hold_ms: 100.0,
            release_ms: 400.0,
            detect_hpf_hz: 150.0,
        };
        if let AuxEffectKind::Equalizer { bands } = &mut changed.effects[1].kind {
            bands[0].freq_hz = 120.0;
            bands[1].gain_db = -6.0;
            bands[1].q = 2.0;
        }
        assert!(same_effect_topology(&current, &changed));

        let mut band_kind = current.clone();
        if let AuxEffectKind::Equalizer { bands } = &mut band_kind.effects[1].kind {
            bands[1].kind = EqBandKind::LowShelf;
        }
        assert!(!same_effect_topology(&current, &band_kind));

        let mut band_count = current.clone();
        if let AuxEffectKind::Equalizer { bands } = &mut band_count.effects[1].kind {
            bands.pop();
        }
        assert!(!same_effect_topology(&current, &band_count));

        changed.effects.swap(0, 1);
        assert!(!same_effect_topology(&current, &changed));
    }

    #[test]
    fn reap_exited_keeps_running_child() {
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let mut slot = Some(child);
        reap_exited(&mut slot);
        assert!(slot.is_some(), "running child must not be dropped");
        if let Some(ref mut c) = slot {
            let _ = c.kill();
            let _ = c.wait();
        }
        reap_exited(&mut slot);
        assert!(slot.is_none(), "exited child must be reaped");
    }

    #[test]
    fn commands_are_killed_at_the_timeout() {
        let t0 = std::time::Instant::now();
        let err =
            output_within(Command::new("sleep").arg("5"), Duration::from_millis(100)).unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(2));
        let out = output_within(Command::new("echo").arg("hi"), Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hi\n");
    }

    #[test]
    fn a_default_the_user_picked_is_kept() {
        assert!(user_moved_default("Hako_Game", true, Some("hdmi")));
        assert!(!user_moved_default("Hako_Game", true, Some("Hako_Game")));
        assert!(!user_moved_default("Hako_Game", false, Some("hdmi")));
        assert!(!user_moved_default("Hako_Game", true, None));
    }

    #[test]
    fn default_owners_release_non_lifo() {
        let s = |n: &str| Some(n.to_string());
        let mut defaults = DefaultState::default();
        defaults.claim(DefaultOwner::ChatMix, "game".into(), s("speakers"));
        defaults.claim(DefaultOwner::Aux, "dac".into(), None);
        assert_eq!(defaults.release(DefaultOwner::ChatMix), None);
        assert_eq!(defaults.release(DefaultOwner::Aux), s("speakers"));

        defaults.claim(DefaultOwner::ChatMix, "game".into(), s("speakers"));
        defaults.claim(DefaultOwner::Aux, "dac".into(), None);
        assert_eq!(defaults.release(DefaultOwner::Aux), s("game"));
        assert_eq!(defaults.release(DefaultOwner::ChatMix), s("speakers"));
    }

    #[test]
    fn only_new_background_errors_reach_the_hook() {
        let audio = AudioService::new();
        let got = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        audio.on_error(move |m| sink.lock().unwrap().push(m));
        let background = |chatmix: Option<&str>| {
            let mut state = audio.state();
            state.chatmix_error = chatmix.map(str::to_owned);
            audio.report(&mut state);
        };

        // The caller of a request gets its error; the hook does not.
        assert!(audio.set_aux_capture_percent(50).is_err());
        background(Some("no sink"));
        background(Some("no sink"));
        background(None);
        background(Some("no sink"));
        assert_eq!(
            *got.lock().unwrap(),
            ["ChatMix: no sink", "ChatMix: no sink"]
        );
    }

    thread_local! {
        pub(super) static PKILLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn only_the_kill_switch_sweeps_other_processes() {
        let pkills = || PKILLS.with(std::cell::Cell::get);
        let audio = AudioService::new();
        audio.aux_keep_pending(&AuxConfig::default(), &AuxEffectChain::default());
        let _ = audio.aux_disable();
        let _ = audio.destroy_chatmix();
        let _ = audio.set_aux_capture_percent(10);
        drop(audio);
        assert_eq!(
            pkills(),
            0,
            "teardown of an instance signalled other processes"
        );

        let audio = AudioService::new();
        // ChatMix's two loopbacks, the AUX DSP, the mixer's strips.
        audio.kill_orphans();
        assert_eq!(pkills(), 4);
        // The kill-switch needs PipeWire only to restore a default it holds.
        let _ = audio.reset_all();
        assert_eq!(pkills(), 8);
        drop(audio);
        assert_eq!(pkills(), 8);
    }

    #[test]
    fn the_mixer_takes_the_dial_and_holds_chatmix_and_aux_back() {
        let audio = AudioService::new();
        let dial = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = dial.clone();
        audio.on_dial(move |game, chat| seen.lock().unwrap().push((game, chat)));
        // Not handed over: the dial is only remembered (no ChatMix is up).
        audio.set_chatmix_volumes(0.3, 1.0).unwrap();
        assert!(dial.lock().unwrap().is_empty());
        assert_eq!(audio.chatmix_dial(), (0.3, 1.0));

        audio.hand_over(true).unwrap();
        // Neither ChatMix nor AUX is built (no PipeWire lookup happens).
        assert!(audio.ensure_chatmix(&["no such sink"]).is_ok());
        assert!(!audio.chatmix_active() && audio.healthy());
        audio
            .aux_enable(&AuxConfig::default(), &AuxEffectChain::default())
            .unwrap();
        assert!(audio.aux_active(), "requested, built on hand-back");
        assert_eq!(audio.aux_capture_node(), None);
        audio.set_chatmix_volumes(0.5, 2.0).unwrap();
        assert_eq!(*dial.lock().unwrap(), [(0.5, 1.0)]);
        assert_eq!(audio.chatmix_dial(), (0.5, 1.0));

        audio.aux_disable().unwrap();
        audio.hand_over(false).unwrap();
        audio.set_chatmix_volumes(0.7, 0.7).unwrap();
        assert_eq!(dial.lock().unwrap().len(), 1, "the dial is ChatMix's again");
    }

    #[test]
    fn status_reads_never_wait_for_audio_work() {
        let audio = AudioService::new();
        audio.aux_keep_pending(&AuxConfig::default(), &AuxEffectChain::default());
        let busy = audio.state();
        assert!(audio.aux_active());
        assert!(!audio.chatmix_active());
        assert_eq!(audio.aux_capture_node(), None, "pending AUX owns no nodes");
        drop(busy);
        // Nothing is up, so a new latency rebuilds nothing.
        assert!(audio.set_latency(128).is_ok());
    }

    #[test]
    fn volumes_follow_the_wpctl_curve() {
        for (linear, expected) in [
            (0.5, 0.125),
            (0.25, 0.015625),
            (0.8, 0.512),
            (1.0, 1.0),
            (0.0, 0.0),
            (0.123, 0.001860867),
            // Rounded to the 3 decimals hako passed to wpctl.
            (0.12345, 0.001860867),
            (1.5, 1.0),
            (-1.0, 0.0),
        ] {
            let got = wpctl_volume(linear);
            assert!(
                (got - expected).abs() < 1e-6,
                "{linear}: {got} != {expected}"
            );
        }
    }

    #[test]
    fn latency_is_samples_at_48k() {
        assert_eq!(node_latency(128), "128/48000");
        assert_eq!(node_latency(512), "512/48000");
    }

    #[test]
    fn lsp_is_found_on_ladspa_path_first() {
        let root = std::env::temp_dir().join(format!("hako-ladspa-test-{}", std::process::id()));
        let (empty, with_lsp) = (root.join("empty"), root.join("lsp"));
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&with_lsp).unwrap();
        std::fs::write(with_lsp.join(LSP_LADSPA), b"").unwrap();

        let path = std::env::join_paths([&empty, &with_lsp]).unwrap();
        let dirs = ladspa_dirs(Some(&path));
        assert_eq!(dirs[..2], [empty.clone(), with_lsp.clone()]);
        assert_eq!(dirs[2..], LADSPA_DIRS.map(PathBuf::from));
        assert_eq!(find_lsp_ladspa(&dirs), Some(with_lsp.join(LSP_LADSPA)));
        assert_eq!(find_lsp_ladspa(std::slice::from_ref(&empty)), None);
        // Empty entries (`::`) are no directory.
        assert_eq!(ladspa_dirs(Some(OsStr::new("::"))).len(), LADSPA_DIRS.len());
        assert_eq!(ladspa_dirs(None).len(), LADSPA_DIRS.len());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A `pw-loopback` sink named `name` whose playback side links nowhere.
    struct TestSink(Child);

    impl TestSink {
        fn spawn(name: &str) -> Self {
            let child = Command::new("pw-loopback")
                .args([
                    "-n",
                    name,
                    "-i",
                    &format!("media.class=Audio/Sink node.name=\"{name}\""),
                    "-o",
                    &format!("node.name=\"{name}-out\" node.autoconnect=false"),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn pw-loopback");
            let mut sink = Self(child);
            let ready = engine()
                .unwrap()
                .wait_for(NODE_WAIT, |graph| {
                    ensure_running(&mut sink.0, name)?;
                    Ok(graph.has_ports(name, Direction::In, &PLAYBACK))
                })
                .unwrap();
            assert!(ready, "{name} did not appear");
            sink
        }
    }

    impl Drop for TestSink {
        fn drop(&mut self) {
            let _ = stop_child(&mut self.0);
        }
    }

    /// `info.params.Props` of the node named `name`, from `pw-dump`.
    pub(crate) fn dumped_props(name: &str) -> Vec<serde_json::Value> {
        let dump = run_cmd("pw-dump", &[]).unwrap();
        let objects: Vec<serde_json::Value> = serde_json::from_str(&dump).unwrap();
        let node = objects
            .iter()
            .find(|o| o["info"]["props"]["node.name"] == name)
            .unwrap_or_else(|| panic!("{name} not in pw-dump"));
        node["info"]["params"]["Props"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    #[test]
    #[ignore = "needs a running PipeWire session"]
    fn live_engine_sees_the_graph() {
        let engine = engine().unwrap();
        engine.with_graph(|graph| {
            assert!(graph.connected);
            assert!(graph.nodes.values().any(|n| n.media_class == "Audio/Sink"));
            assert!(!graph.ports.is_empty());
            assert!(graph.default_sink.is_some(), "default metadata not bound");
        });
        let sink = current_default_sink().unwrap();
        assert!(resolve_node_id(&sink).is_ok());
    }

    #[test]
    #[ignore = "needs a running PipeWire session"]
    fn live_links_are_owned_synchronous_and_replace_lingering_ones() {
        let (_a, _b) = (
            TestSink::spawn("hako-test-a"),
            TestSink::spawn("hako-test-b"),
        );
        let engine = engine().unwrap();
        let pair = ("hako-test-a:monitor_FL", "hako-test-b:playback_FL");
        let present = |linked: bool| {
            engine
                .wait_for(Duration::from_secs(2), |graph| {
                    let names = graph.link_names();
                    Ok(names.contains(&(pair.0.into(), pair.1.into())) == linked)
                })
                .unwrap()
        };

        // A lingering link, as older hako versions made them.
        run_cmd("pw-link", &[pair.0, pair.1]).unwrap();
        assert!(present(true));
        // Ids are reused at once; serials are not.
        let serials = |graph: &engine::Graph| -> Vec<String> {
            let out = graph_port(graph, pair.0, Direction::Out).unwrap().1;
            let inp = graph_port(graph, pair.1, Direction::In).unwrap().1;
            graph
                .links_between(out, inp)
                .iter()
                .map(|id| graph.links[id].props["object.serial"].clone())
                .collect()
        };
        let old = engine.with_graph(serials);
        assert_eq!(old.len(), 1);

        let link = link_ports(pair.0, pair.1).unwrap();
        let id = link.id;
        let synced = engine
            .wait_for(Duration::from_secs(2), |graph| {
                let now = serials(graph);
                Ok(now.len() == 1
                    && now != old
                    && graph
                        .links
                        .get(&id)
                        .is_some_and(|l| l.props.contains_key("object.id")))
            })
            .unwrap();
        assert!(synced, "lingering link not replaced, or no link info");
        engine.with_graph(|graph| {
            assert_ne!(
                graph.links[&id].props.get("link.async").map(String::as_str),
                Some("true")
            );
        });

        drop(link);
        assert!(present(false), "link outlived its handle");
    }

    #[test]
    #[ignore = "needs a running PipeWire session"]
    fn live_volume_matches_wpctl() {
        let _sink = TestSink::spawn("hako-test-volume");
        let id = resolve_node_id("hako-test-volume").unwrap();
        let mut volumes = None;
        for _ in 0..20 {
            // Re-applied: WirePlumber restores a stored volume onto a node
            // that just appeared, racing the first write.
            set_node_volume(id, 0.5).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            volumes = dumped_props("hako-test-volume")
                .iter()
                .find_map(|p| p.get("channelVolumes").cloned());
            if volumes == Some(serde_json::json!([0.125, 0.125])) {
                break;
            }
        }
        assert_eq!(volumes, Some(serde_json::json!([0.125, 0.125])));
    }

    #[test]
    #[ignore = "needs a running PipeWire session"]
    fn live_dsp_parameters_update() {
        const SINK: &str = "hako-test-dsp";
        let chain = |gain_db| {
            let mut chain = chain(vec![effect(
                "eq",
                true,
                eq(&[(EqBandKind::Peaking, 1000.0)]),
            )]);
            if let AuxEffectKind::Equalizer { bands } = &mut chain.effects[0].kind {
                bands[0].gain_db = gain_db;
            }
            chain
        };
        // Unlinked and suspended, filter-chain reports every control as 0:
        // always-process keeps it running (on the dummy driver).
        let config = generate_dsp_config(SINK, Some("hako-test-nowhere"), &chain(3.0), 256, None)
            .unwrap()
            .replace("node.passive = true", "node.autoconnect = false")
            .replace("node.latency", "node.always-process = true node.latency");
        let path = std::env::temp_dir().join(format!("hako-test-dsp-{}.conf", std::process::id()));
        std::fs::write(&path, config).unwrap();
        let mut child = Command::new("pipewire")
            .arg("-c")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let gain_becomes = |want: f64| {
            let mut gain = None;
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(100));
                gain = dumped_props(SINK).iter().find_map(|p| {
                    let params = p.get("params")?.as_array()?;
                    let at = params.iter().position(|v| v == "fx0_b0_l:Gain")?;
                    params.get(at + 1)?.as_f64()
                });
                if gain == Some(want) {
                    break;
                }
            }
            gain
        };
        let configured = wait_for_node(&mut child, SINK).map(|()| gain_becomes(3.0));
        let updated = configured.is_ok().then(|| {
            let id = resolve_node_id(SINK).unwrap();
            let pod = engine::params_pod(&effect_params(&chain(-4.5))).unwrap();
            engine().unwrap().set_props(id, pod).unwrap();
            gain_becomes(-4.5)
        });
        let _ = stop_child(&mut child);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            configured.unwrap(),
            Some(3.0),
            "configured gain not reported"
        );
        assert_eq!(updated.flatten(), Some(-4.5));
    }
}
