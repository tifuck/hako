//! PipeWire ChatMix + AUX helpers (no PulseAudio / pactl required).

use anyhow::{anyhow, bail, Context, Result};
use hako_core::{
    ipc::{AuxEffectChain, AuxEffectKind, EqBandKind},
    protocol::is_new_error,
    AuxConfig,
};
use regex::Regex;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub const CHATMIX_GAME: &str = "Hako_Game";
pub const CHATMIX_CHAT: &str = "Hako_Chat";

const LSP_LADSPA: &str = "/usr/lib/ladspa/lsp-plugins-ladspa.so";
const AUX_DSP_SINK: &str = "Hako_AUX_DSP";
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
    /// (game, chat) node ids, valid while the children run.
    ids: Option<(u32, u32)>,
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
    links: Vec<(String, String)>,
    dsp: Option<Child>,
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
            dsp: None,
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
}

type ErrorHook = Box<dyn Fn(String) + Send + Sync>;

pub struct AudioService {
    state: Mutex<AudioState>,
    on_error: OnceLock<ErrorHook>,
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
            }),
            on_error: OnceLock::new(),
        }
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

    fn state(&self) -> MutexGuard<'_, AudioState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.chatmix_error = Some("audio state mutex was poisoned; recovered state".into());
            state
        })
    }

    pub fn chatmix_active(&self) -> bool {
        let mut state = self.state();
        reap_exited(&mut state.chatmix.game);
        reap_exited(&mut state.chatmix.chat);
        state.chatmix.active()
    }

    pub fn aux_active(&self) -> bool {
        self.state().aux.is_some()
    }

    pub fn aux_processing_active(&self) -> bool {
        let mut state = self.state();
        if let Some(aux) = state.aux.as_mut() {
            reap_exited(&mut aux.dsp);
            aux.dsp.is_some()
        } else {
            false
        }
    }

    pub fn aux_dsp_active(&self) -> bool {
        self.aux_processing_active()
    }

    pub fn last_error(&self) -> Option<String> {
        let state = self.state();
        match (&state.chatmix_error, &state.aux_error) {
            (Some(c), Some(a)) => Some(format!("ChatMix: {c}; AUX: {a}")),
            (c, a) => c.clone().or_else(|| a.clone()),
        }
    }

    pub fn healthy(&self) -> bool {
        let state = self.state();
        state.chatmix_error.is_none() && state.aux_error.is_none()
    }

    /// Bring ChatMix up on the headset's playback sink: the first node name
    /// matching `sink_regexes`, tried in order (most specific first), unless
    /// the kill-switch suppressed it. Already up (for any headset): no-op.
    pub fn ensure_chatmix(&self, sink_regexes: &[&str]) -> Result<()> {
        let mut state = self.state();
        if state.chatmix.active() || state.chatmix_suppressed {
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
        if !state.chatmix.active() {
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
        if !state.chatmix.active() || state.chatmix.volumes == Some((game, chat)) {
            return Ok(());
        }
        let set = |(g, c): (u32, u32)| {
            wpctl_set_volume(g, game)?;
            wpctl_set_volume(c, chat)
        };
        let result = match state.chatmix.ids {
            Some(ids) if set(ids).is_ok() => Ok(()),
            // Ids unknown or stale: look them up once more.
            _ => chatmix_ids().and_then(|ids| {
                state.chatmix.ids = Some(ids);
                set(ids)
            }),
        };
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

    /// PipeWire silently drops `pw-link` links when a node is recreated
    /// (suspend/resume, device replug, server restart), and the DSP process can
    /// exit. Rebuild the graph whenever any part of it is gone; on failure stay
    /// pending so the next call retries once the nodes are back.
    pub fn heal_aux(&self) {
        let mut state = self.state();
        let Some(aux) = state.aux.as_mut() else {
            return;
        };
        if aux_graph_intact(aux) {
            return;
        }
        let (cfg, chain) = (aux.cfg.clone(), aux.chain.clone());
        if aux.alsa_card.is_some() {
            warn!("AUX graph broken; rebuilding");
            let _ = aux_disable_locked(&mut state);
        }
        let result = aux_enable_locked(&mut state, &cfg, &chain);
        if result.is_err() {
            state.aux = Some(AuxState::pending(cfg, chain));
        }
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
        let aux = aux_disable_locked(&mut state);
        let mut chatmix = destroy_chatmix_locked(&mut state);
        if let Some(original) = state.defaults.restore() {
            chatmix = chatmix.and(set_default_sink(&original));
        }
        record_result(&mut state.aux_error, &aux);
        record_result(&mut state.chatmix_error, &chatmix);
        seen(&mut state);
        aux.and(chatmix)
    }

    /// After a crash the daemon no longer knows which AUX links it made, and
    /// `pw-link` links outlive their creator: remove capture -> sink links
    /// matching the AUX config. No-op while this service owns AUX, or when
    /// either node is absent (nothing can be linked).
    pub fn remove_orphan_aux_links(&self, cfg: &AuxConfig) -> Result<()> {
        let state = self.state();
        if state.aux.is_some() {
            return Ok(());
        }
        let (Ok(capture), Ok(sink)) = (
            find_source_node_name(&cfg.capture_name_regex),
            find_sink_node_name(&cfg.sink_name_regex),
        ) else {
            return Ok(());
        };
        let present = parse_links(&run_cmd("pw-link", &["-l"])?);
        for link in stereo_links(&capture, &sink) {
            if present.contains(&link) {
                unlink_nodes(&link.0, &link.1)?;
            }
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
        let _ = aux_disable_locked(state);
        let _ = destroy_chatmix_locked(state);
        if let Some(original) = state.defaults.restore() {
            let _ = set_default_sink(&original);
        }
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
    let arctis = found?;

    cleanup_chatmix_children(&mut state.chatmix);
    let mut game = spawn_loopback(CHATMIX_GAME, "hako Game", &arctis)?;
    let mut chat = match spawn_loopback(CHATMIX_CHAT, "hako Chat", &arctis) {
        Ok(child) => child,
        Err(error) => {
            let _ = stop_child(&mut game);
            return Err(error);
        }
    };

    let ready = wait_for_chatmix_nodes(&mut game, &mut chat)
        .and_then(|()| chatmix_ids())
        .and_then(|ids| {
            if state.chatmix_set_default {
                claim_default(&mut state.defaults, DefaultOwner::ChatMix, CHATMIX_GAME)?;
            }
            Ok(ids)
        });
    let ids = match ready {
        Ok(ids) => ids,
        Err(error) => {
            let _ = stop_child(&mut game);
            let _ = stop_child(&mut chat);
            return Err(error);
        }
    };

    state.chatmix = ChatMixState {
        game: Some(game),
        chat: Some(chat),
        ids: Some(ids),
        volumes: None,
    };
    info!("ChatMix sinks ready -> {arctis}");
    Ok(())
}

fn chatmix_ids() -> Result<(u32, u32)> {
    Ok((
        resolve_node_id(CHATMIX_GAME)?,
        resolve_node_id(CHATMIX_CHAT)?,
    ))
}

fn destroy_chatmix_locked(state: &mut AudioState) -> Result<()> {
    let mut first_error = release_default(&mut state.defaults, DefaultOwner::ChatMix).err();
    state.chatmix.ids = None;
    state.chatmix.volumes = None;
    if let Some(mut child) = state.chatmix.game.take() {
        if let Err(error) = stop_child(&mut child) {
            first_error.get_or_insert(error);
        }
    } else {
        kill_loopback_orphan(CHATMIX_GAME);
    }
    if let Some(mut child) = state.chatmix.chat.take() {
        if let Err(error) = stop_child(&mut child) {
            first_error.get_or_insert(error);
        }
    } else {
        kill_loopback_orphan(CHATMIX_CHAT);
    }
    info!("ChatMix destroyed");
    first_error.map_or(Ok(()), Err)
}

fn cleanup_chatmix_children(chatmix: &mut ChatMixState) {
    chatmix.ids = None;
    chatmix.volumes = None;
    if let Some(mut child) = chatmix.game.take() {
        let _ = stop_child(&mut child);
    } else {
        kill_loopback_orphan(CHATMIX_GAME);
    }
    if let Some(mut child) = chatmix.chat.take() {
        let _ = stop_child(&mut child);
    } else {
        kill_loopback_orphan(CHATMIX_CHAT);
    }
}

fn aux_enable_locked(
    state: &mut AudioState,
    cfg: &AuxConfig,
    chain: &AuxEffectChain,
) -> Result<()> {
    chain.validate().map_err(anyhow::Error::msg)?;
    let capture = find_source_node_name(&cfg.capture_name_regex)?;
    let sink = find_sink_node_name(&cfg.sink_name_regex)?;
    let capture_id = resolve_node_id(&capture)?;
    let alsa_card = inspect_alsa_card(capture_id)?;
    let previous_input_source = current_input_source(&alsa_card).ok();

    let has_effects = chain.effects.iter().any(|effect| effect.enabled);
    let needs_lsp = chain
        .effects
        .iter()
        .any(|effect| effect.enabled && !matches!(effect.kind, AuxEffectKind::Equalizer { .. }));
    let mut dsp = None;
    let destination = if has_effects {
        if needs_lsp && !Path::new(LSP_LADSPA).exists() {
            bail!("LSP LADSPA plugins not installed ({LSP_LADSPA})");
        }
        let config = generate_dsp_config(AUX_DSP_SINK, &sink, chain)?;
        let path = dsp_conf_path();
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(config.as_bytes()))
            .with_context(|| format!("write {}", path.display()))?;
        kill_aux_dsp_orphan();
        let mut child = Command::new("pipewire")
            .arg("-c")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn PipeWire AUX filter graph")?;
        if let Err(error) = wait_for_node(&mut child, AUX_DSP_SINK, PortDirection::Input) {
            let _ = stop_child(&mut child);
            return Err(error);
        }
        dsp = Some(child);
        AUX_DSP_SINK.to_string()
    } else {
        sink.clone()
    };

    let pairs = stereo_links(&capture, &destination);
    let mut linked = Vec::new();
    for (source, destination) in &pairs {
        if let Err(error) = link_nodes(source, destination) {
            cleanup_aux_parts(&mut linked, &mut dsp);
            return Err(error);
        }
        linked.push((source.clone(), destination.clone()));
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
        links: linked,
        dsp,
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
        for (source, destination) in aux.links.drain(..) {
            if let Err(error) = unlink_nodes(&source, &destination) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(mut child) = aux.dsp.take() {
            if let Err(error) = stop_child(&mut child) {
                first_error.get_or_insert(error);
            }
        } else {
            kill_aux_dsp_orphan();
        }
        if let (Some(card), Some(source)) = (aux.alsa_card.as_deref(), aux.previous_input_source) {
            if let Err(error) = set_input_source(card, &source) {
                first_error.get_or_insert(error);
            }
        }
    } else {
        kill_aux_dsp_orphan();
    }
    info!("AUX disabled");
    first_error.map_or(Ok(()), Err)
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
    let Ok(dump) = run_cmd("pw-link", &["-l"]) else {
        return true;
    };
    let present = parse_links(&dump);
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

/// `pw-link -l` lists every output port followed by `  |-> input` lines.
fn parse_links(dump: &str) -> HashSet<(String, String)> {
    let mut links = HashSet::new();
    let mut port = "";
    for line in dump.lines() {
        if let Some(input) = line.trim().strip_prefix("|-> ") {
            links.insert((port.to_string(), input.trim().to_string()));
        } else if !line.starts_with(char::is_whitespace) {
            port = line.trim();
        }
    }
    links
}

fn cleanup_aux_parts(links: &mut Vec<(String, String)>, dsp: &mut Option<Child>) {
    for (source, destination) in links.drain(..) {
        let _ = unlink_nodes(&source, &destination);
    }
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

/// `pw-loopback` capture / playback properties. Values are quoted: an
/// unquoted space (`node.description=hako Game`) is a syntax error and
/// pw-loopback exits 255.
fn loopback_props(name: &str, description: &str, playback_target: &str) -> (String, String) {
    (
        format!(
            "media.class=Audio/Sink node.name=\"{}\" node.description=\"{}\" audio.position=[FL FR]",
            pw_escape(name),
            pw_escape(description)
        ),
        format!("node.target=\"{}\"", pw_escape(playback_target)),
    )
}

fn spawn_loopback(name: &str, description: &str, playback_target: &str) -> Result<Child> {
    kill_loopback_orphan(name);
    let (capture_props, playback_props) = loopback_props(name, description, playback_target);
    Command::new("pw-loopback")
        .args(["-n", name, "-i", &capture_props, "-o", &playback_props])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn pw-loopback for {name}"))
}

fn wait_for_chatmix_nodes(game: &mut Child, chat: &mut Child) -> Result<()> {
    for _ in 0..40 {
        ensure_running(game, CHATMIX_GAME)?;
        ensure_running(chat, CHATMIX_CHAT)?;
        let ports = run_cmd("pw-link", &["-i"]).unwrap_or_default();
        if node_has_stereo_ports(&ports, CHATMIX_GAME, "playback_FL", "playback_FR")
            && node_has_stereo_ports(&ports, CHATMIX_CHAT, "playback_FL", "playback_FR")
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("ChatMix nodes did not appear")
}

#[derive(Clone, Copy)]
enum PortDirection {
    Input,
}

fn wait_for_node(child: &mut Child, node: &str, direction: PortDirection) -> Result<()> {
    let flag = match direction {
        PortDirection::Input => "-i",
    };
    for _ in 0..40 {
        ensure_running(child, node)?;
        let ports = run_cmd("pw-link", &[flag]).unwrap_or_default();
        if node_has_stereo_ports(&ports, node, "playback_FL", "playback_FR") {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("PipeWire node {node} did not appear")
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
    let _ = Command::new("pkill")
        .args(["-f", &format!("^pw-loopback -n {name} ")])
        .status();
}

fn kill_aux_dsp_orphan() {
    let _ = Command::new("pkill")
        .args(["-f", r"^pipewire -c .*/hako-aux-dsp\.conf$"])
        .status();
}

/// Next to the daemon socket: the per-user runtime dir, never a shared /tmp.
fn dsp_conf_path() -> PathBuf {
    hako_core::default_socket_path().with_file_name("hako-aux-dsp.conf")
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// One filter-chain node: LADSPA when `plugin` is set, else a PipeWire builtin.
struct DspNode {
    name: String,
    plugin: Option<&'static str>,
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
            plugin: Some(LSP_LADSPA),
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
                                plugin: None,
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

fn generate_dsp_config(sink: &str, target: &str, chain: &AuxEffectChain) -> Result<String> {
    chain.validate().map_err(anyhow::Error::msg)?;
    let stages = dsp_stages(chain);
    let (Some(first), Some(last)) = (stages.first(), stages.last()) else {
        bail!("cannot generate an empty DSP graph");
    };

    let mut nodes = String::new();
    for node in stages.iter().flat_map(|stage| &stage.nodes) {
        let kind = match node.plugin {
            Some(plugin) => format!(
                "type = ladspa name = \"{}\" plugin = \"{}\"",
                pw_escape(&node.name),
                pw_escape(plugin)
            ),
            None => format!("type = builtin name = \"{}\"", pw_escape(&node.name)),
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
    for pair in stages.windows(2) {
        for (output, input) in pair[0].outputs.iter().zip(&pair[1].inputs) {
            links.push_str(&format!(
                "          {{ output = \"{}\" input = \"{}\" }}\n",
                pw_escape(output),
                pw_escape(input),
            ));
        }
    }
    let [in_l, in_r] = first.inputs.each_ref().map(|port| pw_escape(port));
    let [out_l, out_r] = last.outputs.each_ref().map(|port| pw_escape(port));

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
      filter.graph = {{
        nodes = [
{nodes}        ]
        links = [
{links}        ]
        inputs = [ "{in_l}" "{in_r}" ]
        outputs = [ "{out_l}" "{out_r}" ]
      }}
      audio.channels = 2
      audio.position = [ FL FR ]
      capture.props = {{ node.name = "{sink}" media.class = Audio/Sink }}
      playback.props = {{ node.name = "{sink}_out" node.target = "{target}" node.passive = true }}
    }}
  }}
]
"#,
        description = pw_escape(AUX_DSP_DESC),
        sink = pw_escape(sink),
        target = pw_escape(target),
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

fn stereo_links(source: &str, sink: &str) -> Vec<(String, String)> {
    vec![
        (
            format!("{source}:capture_FL"),
            format!("{sink}:playback_FL"),
        ),
        (
            format!("{source}:capture_FR"),
            format!("{sink}:playback_FR"),
        ),
    ]
}

/// Longest a PipeWire / ALSA tool may run: a hung server must not hold the
/// audio lock (and with it GetState and daemon shutdown) forever.
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
    let inspect = run_cmd("wpctl", &["inspect", "@DEFAULT_AUDIO_SINK@"])?;
    inspect
        .lines()
        .find_map(|line| parse_quoted_property(line.trim(), "node.name"))
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("default sink has no node.name"))
}

fn set_default_sink(name: &str) -> Result<()> {
    let id = resolve_node_id(name)?;
    run_cmd("wpctl", &["set-default", &id.to_string()]).map(|_| ())
}

fn wpctl_set_volume(id: u32, linear: f32) -> Result<()> {
    run_cmd(
        "wpctl",
        &["set-volume", &id.to_string(), &format!("{linear:.3}")],
    )
    .map(|_| ())
}

fn resolve_node_id(exact_name: &str) -> Result<u32> {
    let dump = run_cmd("pw-cli", &["ls", "Node"])?;
    let mut id = None;
    for line in dump.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("id ") {
            id = rest
                .split(',')
                .next()
                .and_then(|value| value.trim().parse().ok());
        } else if parse_quoted_property(trimmed, "node.name").as_deref() == Some(exact_name) {
            return id.ok_or_else(|| anyhow!("node {exact_name} has no id"));
        }
    }
    bail!("node id not found for {exact_name}")
}

fn inspect_alsa_card(node_id: u32) -> Result<String> {
    let inspect = run_cmd("wpctl", &["inspect", &node_id.to_string()])?;
    parse_alsa_card(&inspect).ok_or_else(|| anyhow!("no ALSA card for PipeWire node {node_id}"))
}

fn parse_alsa_card(inspect: &str) -> Option<String> {
    for key in ["api.alsa.card", "alsa.card"] {
        for line in inspect.lines() {
            if let Some(value) = parse_quoted_property(line.trim(), key) {
                if !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()) {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn parse_quoted_property(line: &str, key: &str) -> Option<String> {
    let (_, value) = line.split_once('=')?;
    if line
        .split_once('=')?
        .0
        .trim()
        .trim_start_matches('*')
        .trim()
        != key
    {
        return None;
    }
    Some(value.trim().trim_matches('"').to_string())
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
    run_cmd(
        "pw-cli",
        &["s", &id.to_string(), "Props", &effect_params(chain)],
    )
    .map(|_| ())
}

fn effect_params(chain: &AuxEffectChain) -> String {
    let mut params = String::from("{ params = [");
    for node in dsp_stages(chain).iter().flat_map(|stage| &stage.nodes) {
        for (port, value) in &node.controls {
            params.push_str(&format!(
                " \"{}\" {value}",
                pw_escape(&format!("{}:{port}", node.name))
            ));
        }
    }
    params.push_str(" ] }");
    params
}

fn find_source_node_name(pattern: &str) -> Result<String> {
    find_node_name(pattern, "-o", "capture_FL", "capture_FR", "source")
}

fn find_sink_node_name(pattern: &str) -> Result<String> {
    find_node_name(pattern, "-i", "playback_FL", "playback_FR", "sink")
}

fn find_node_name(
    pattern: &str,
    pw_link_flag: &str,
    left: &str,
    right: &str,
    kind: &str,
) -> Result<String> {
    let regex = Regex::new(pattern).with_context(|| format!("bad {kind} regex"))?;
    let ports = run_cmd("pw-link", &[pw_link_flag])?;
    parse_stereo_node(&ports, &regex, left, right)
        .ok_or_else(|| anyhow!("no {kind} matching /{pattern}/ with {left}/{right} ports"))
}

fn parse_stereo_node(ports: &str, regex: &Regex, left: &str, right: &str) -> Option<String> {
    let mut candidates = Vec::new();
    let mut found = HashSet::new();
    for line in ports.lines() {
        let port = line.trim();
        let Some((node, channel)) = port.rsplit_once(':') else {
            continue;
        };
        if regex.is_match(node) && (channel == left || channel == right) {
            if !candidates.iter().any(|candidate| candidate == node) {
                candidates.push(node.to_string());
            }
            found.insert((node.to_string(), channel.to_string()));
        }
    }
    candidates.into_iter().find(|node| {
        found.contains(&(node.clone(), left.to_string()))
            && found.contains(&(node.clone(), right.to_string()))
    })
}

fn node_has_stereo_ports(ports: &str, node: &str, left: &str, right: &str) -> bool {
    let expected_left = format!("{node}:{left}");
    let expected_right = format!("{node}:{right}");
    let mut has_left = false;
    let mut has_right = false;
    for line in ports.lines().map(str::trim) {
        has_left |= line == expected_left;
        has_right |= line == expected_right;
    }
    has_left && has_right
}

fn link_nodes(source: &str, destination: &str) -> Result<()> {
    let output = output_within(
        Command::new("pw-link").args([source, destination]),
        CMD_TIMEOUT,
    )?;
    if output.status.success() {
        return Ok(());
    }
    let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if error.contains("exist") {
        return Ok(());
    }
    bail!("pw-link {source} -> {destination} failed: {}", error.trim())
}

fn unlink_nodes(source: &str, destination: &str) -> Result<()> {
    match run_cmd("pw-link", &["-d", source, destination]) {
        // Already gone (node recreated) is the state we wanted.
        Err(error) if error.to_string().contains("No such file or directory") => Ok(()),
        result => result.map(|_| ()),
    }
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
            "sink\"name",
            &chain(vec![
                effect("first-gate", true, AuxEffectKind::default_gate()),
                effect("disabled", false, AuxEffectKind::default_compressor()),
                effect("second-gate", true, AuxEffectKind::default_gate()),
            ]),
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
        assert!(!config.contains("node.latency"));
    }

    #[test]
    fn config_maps_all_gate_and_compressor_params() {
        let config = generate_dsp_config(
            "Test",
            "target",
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
        let config = generate_dsp_config(AUX_DSP_SINK, "target", &gate_eq_compressor()).unwrap();

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
        let config = generate_dsp_config(AUX_DSP_SINK, "target", &eq_only).unwrap();
        assert!(config.contains("inputs = [ \"fx0_b0_l:In\" \"fx0_b0_r:In\" ]"));
        assert!(config.contains("outputs = [ \"fx0_b0_l:Out\" \"fx0_b0_r:Out\" ]"));
        assert!(!config.contains("{ output = "));
    }

    #[test]
    fn config_rejects_invalid_or_empty_chain() {
        assert!(generate_dsp_config(AUX_DSP_SINK, "t", &AuxEffectChain::default()).is_err());
        let disabled = chain(vec![effect("gate", false, AuxEffectKind::default_gate())]);
        assert!(generate_dsp_config(AUX_DSP_SINK, "t", &disabled).is_err());
        let empty_eq = chain(vec![effect("eq", true, eq(&[]))]);
        assert!(generate_dsp_config(AUX_DSP_SINK, "t", &empty_eq).is_err());
    }

    #[test]
    fn live_params_cover_every_control() {
        let params = effect_params(&gate_eq_compressor());
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
    fn port_parser_requires_exact_stereo_ports() {
        let ports = "other:capture_FL\nline:capture_FL\nline:capture_FR\nline:capture_AUX\n";
        let regex = Regex::new("^line$").unwrap();
        assert_eq!(
            parse_stereo_node(ports, &regex, "capture_FL", "capture_FR"),
            Some("line".into())
        );
        assert!(parse_stereo_node(ports, &regex, "playback_FL", "playback_FR").is_none());
        assert!(node_has_stereo_ports(
            ports,
            "line",
            "capture_FL",
            "capture_FR"
        ));
    }

    #[test]
    fn link_parser_reads_pw_link_list() {
        let dump = "line:capture_FL\n  |-> Hako_AUX_DSP:playback_FL\nHako_AUX_DSP:playback_FL\n  |<- line:capture_FL\nline:capture_FR\n";
        let links = parse_links(dump);
        assert_eq!(links.len(), 1);
        assert!(links.contains(&("line:capture_FL".into(), "Hako_AUX_DSP:playback_FL".into())));
    }

    #[test]
    fn config_string_escaping() {
        assert_eq!(pw_escape("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn loopback_props_quote_values_with_spaces() {
        let (capture, playback) = loopback_props("Hako_Game", "hako Game", "alsa_output.x");
        assert!(
            capture.contains("node.description=\"hako Game\""),
            "{capture}"
        );
        assert!(capture.contains("node.name=\"Hako_Game\""), "{capture}");
        assert_eq!(playback, "node.target=\"alsa_output.x\"");
    }

    #[test]
    fn parses_selected_alsa_card_only() {
        let inspect = "  * api.alsa.card = \"3\"\n    device.id = \"42\"\n";
        assert_eq!(parse_alsa_card(inspect).as_deref(), Some("3"));
        assert_eq!(parse_alsa_card("device.id = \"42\"").as_deref(), None);
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
}
