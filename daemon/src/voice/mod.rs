//! Voice: local dictation. `voice.toml` in memory plus an actor thread that
//! owns the state machine (hotkey / IPC triggers, the `hako_stt` capture
//! stream, the `hako-stt` worker, pings, output, history, downloads) and
//! publishes `VoiceState`. Design: `docs/superpowers/specs/2026-11-25-voice-stt-design.md`.
//!
//! Sessions: a trigger opens the capture and plays the start ping at once;
//! the model loads lazily (audio is buffered meanwhile) and is unloaded
//! after the idle timeout. A trigger while an earlier utterance is still
//! transcribing starts a new recording at once: the worker serves sessions
//! in order and one output thread types them in order.

mod download;
mod engine;
mod history;
mod output;
mod sounds;

use crate::drivers::EventTx;
use crate::hotkeys::{HotkeyEdge, HotkeyService, TriggerSink};
use crate::jarvis::{Ear, HubTx, JarvisOut, Listen, RealEnv, Runner, RunnerIn};
use crate::mixer::PatchError;
use anyhow::{Context, Result};
use download::{Download, Fetch};
use engine::{ChildSpawner, Spawner, ToWorker, Worker};
use hako_core::config::ConfigPaths;
use hako_core::hotkey::HotkeyAction;
use hako_core::protocol::AdapterEvent;
use hako_core::voice::text;
use hako_core::voice::worker::{WorkerIn, WorkerOut};
use hako_core::voice::{
    apply_voice_settings, load_voice, update_voice, valid_custom_file, wake_params, Accelerator,
    CommandOutcome, CustomModel, EngineStatus, HistoryEntry, InstalledModel, ModelCaps,
    ModelChoice, ModelStatus, OutputCheck, OutputMethod, OutputSettings, SoundSettings,
    VoiceAction, VoiceControlParams, VoiceFile, VoiceOp, VoicePartial, VoiceState, CATALOG,
    MAX_VOICE_OPS, SAMPLE_RATE,
};
use history::History;
pub(crate) use output::run_tool;
use output::{Ctx, Job, OutputQueue, Outputter};
use sounds::{SinkMute, Sound, Sounds};
use std::collections::{BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, error, info, warn};

/// Audio right after the start ping is its bleed into the mic: dropped.
const BLEED: Duration = Duration::from_millis(150);
/// PCM goes to the worker in blocks this long.
const CHUNK: usize = SAMPLE_RATE as usize * 80 / 1000;
/// Longest wait for a session's final text after its end.
const FINAL_WAIT: Duration = Duration::from_secs(120);
/// First wait before a crashed worker is started again; doubles per crash.
const RESTART_FIRST: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(30);
/// A worker that ran this long since its last crash starts over at `RESTART_FIRST`.
const RECOVERED_AFTER: Duration = Duration::from_secs(60);
/// Models on disk, sources and output methods are looked at this often.
const REFRESH: Duration = Duration::from_secs(2);
/// Download progress is published this often.
const PROGRESS: Duration = Duration::from_millis(250);
/// Longest a request waits for the actor.
const REPLY_WAIT: Duration = Duration::from_secs(15);
/// Jarvis's microphone is tried again this long after it failed to open.
const CAPTURE_RETRY: Duration = Duration::from_secs(5);

/// Where the service keeps its files (tests point them at temp dirs).
#[derive(Debug, Clone)]
pub struct Dirs {
    /// GGUF models (`~/.local/share/hako/models`).
    pub models: PathBuf,
    /// History (`~/.local/share/hako/voice`).
    pub history: PathBuf,
    /// Synthesised pings (`$XDG_RUNTIME_DIR`).
    pub runtime: PathBuf,
}

impl Dirs {
    pub fn user() -> Self {
        let data =
            hako_core::voice::data_dir().unwrap_or_else(|_| std::env::temp_dir().join("hako"));
        Self {
            models: hako_core::voice::models_dir().unwrap_or_else(|_| data.join("models")),
            history: data.join("voice"),
            runtime: std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
        }
    }
}

/// Temp file then rename: a crash never leaves a truncated file.
pub(crate) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("write {}", path.display()))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------
// Side effects, behind a trait so the state machine runs in tests.

/// An open capture stream; dropping it closes it.
pub trait Capture: Send {
    fn failed(&self) -> bool;
}

impl Capture for hako_audio::stt::SttCapture {
    fn failed(&self) -> bool {
        hako_audio::stt::SttCapture::failed(self)
    }
}

pub type PcmSink = Box<dyn FnMut(&[f32]) + Send>;

pub trait Backend: Send {
    fn spawner(&mut self) -> &mut dyn Spawner;
    fn open_capture(
        &mut self,
        mic: Option<&str>,
        sink: PcmSink,
    ) -> Result<Box<dyn Capture>, String>;
    fn play(&mut self, sound: Sound, settings: &SoundSettings);
    fn cancel_start_ping(&mut self);
    fn output(&mut self, job: Job);
    fn mute(&mut self, on: bool);
    /// The voice hotkeys may fire (Voice on with a usable model), and the
    /// push-to-command one (Jarvis on too).
    fn hotkeys_active(&mut self, on: bool, jarvis: bool);
    /// To the Jarvis command runner.
    fn jarvis(&mut self, msg: RunnerIn);
    /// A headset reports its microphone muted (wake words are ignored).
    fn mic_muted(&mut self) -> bool;
    fn recording(&mut self, on: bool);
    fn detect_layout(&mut self) -> String;
    fn sources(&mut self) -> Vec<(String, String)>;
    fn checks(&mut self, out: &OutputSettings) -> Vec<OutputCheck>;
}

struct RealBackend {
    hotkeys: Arc<HotkeyService>,
    spawner: ChildSpawner,
    sounds: Sounds,
    mute: SinkMute,
    queue: OutputQueue,
    runner: Runner,
    mic_muted: Arc<AtomicBool>,
}

impl Backend for RealBackend {
    fn spawner(&mut self) -> &mut dyn Spawner {
        &mut self.spawner
    }

    fn open_capture(
        &mut self,
        mic: Option<&str>,
        mut sink: PcmSink,
    ) -> Result<Box<dyn Capture>, String> {
        hako_audio::stt::SttCapture::open(mic, move |pcm| sink(pcm))
            .map(|c| Box::new(c) as Box<dyn Capture>)
            .map_err(|e| format!("{e:#}"))
    }

    fn play(&mut self, sound: Sound, settings: &SoundSettings) {
        self.sounds.play(sound, settings);
    }

    fn cancel_start_ping(&mut self) {
        self.sounds.cancel_start();
    }

    fn output(&mut self, job: Job) {
        self.queue.push(job);
    }

    fn mute(&mut self, on: bool) {
        if on {
            self.mute.mute();
        } else {
            self.mute.restore();
        }
    }

    fn hotkeys_active(&mut self, on: bool, jarvis: bool) {
        let mut actions: BTreeSet<HotkeyAction> = if on {
            [
                HotkeyAction::VoiceTranscribe,
                HotkeyAction::VoiceTranscribeAlt,
                HotkeyAction::VoiceCancel,
            ]
            .into()
        } else {
            BTreeSet::new()
        };
        if on && jarvis {
            actions.insert(HotkeyAction::JarvisListen);
        }
        self.hotkeys.set_active(actions);
    }

    fn jarvis(&mut self, msg: RunnerIn) {
        self.runner.send(msg);
    }

    fn mic_muted(&mut self) -> bool {
        self.mic_muted.load(Ordering::Relaxed)
    }

    fn recording(&mut self, on: bool) {
        self.hotkeys.set_recording(on);
    }

    fn detect_layout(&mut self) -> String {
        output::detect_layout()
    }

    fn sources(&mut self) -> Vec<(String, String)> {
        hako_audio::stt::sources().unwrap_or_else(|e| {
            warn!("voice: listing sources: {e:#}");
            Vec::new()
        })
    }

    fn checks(&mut self, out: &OutputSettings) -> Vec<OutputCheck> {
        output::checks(out)
    }
}

// ---------------------------------------------------------------------------
// The service.

enum Ev {
    Trigger(HotkeyAction, HotkeyEdge),
    Control(VoiceControlParams, mpsc::Sender<Result<VoiceState, String>>),
    Patch(Vec<VoiceOp>, mpsc::Sender<Result<VoiceState, PatchError>>),
    /// Capture generation, samples.
    Pcm(u64, Vec<f32>),
    /// Worker generation; `None`: it exited.
    Worker(u64, Option<WorkerOut>),
    /// Jarvis's capture generation, samples.
    JarvisPcm(u64, Vec<f32>),
    /// Jarvis's wake-word worker generation; `None`: it exited.
    JarvisWorker(u64, Option<WorkerOut>),
    Jarvis(JarvisOut),
    Download(u64, Result<bool, String>),
    OutputFailed(String),
    Shutdown(mpsc::Sender<()>),
}

pub struct VoiceService {
    tx: mpsc::Sender<Ev>,
    state: Arc<Mutex<VoiceState>>,
}

impl VoiceService {
    /// `hub`: where Jarvis's commands reach the hub.
    pub fn start(
        paths: ConfigPaths,
        events: EventTx,
        hotkeys: Arc<HotkeyService>,
        hub: HubTx,
    ) -> Self {
        Self::start_in(paths, Dirs::user(), events, hotkeys, hub)
    }

    pub fn start_in(
        paths: ConfigPaths,
        dirs: Dirs,
        events: EventTx,
        hotkeys: Arc<HotkeyService>,
        hub: HubTx,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let queue = OutputQueue::start(
            {
                let mut o = Outputter::default();
                move |job| o.run(job)
            },
            {
                let hotkeys = hotkeys.clone();
                move || !hotkeys.held_modifiers().is_empty()
            },
            {
                let tx = tx.clone();
                move |e| {
                    let _ = tx.send(Ev::OutputFailed(e));
                }
            },
        );
        let mic_muted = Arc::new(AtomicBool::new(false));
        let env = RealEnv {
            hub,
            voice: Box::new({
                let tx = tx.clone();
                move |out| {
                    let _ = tx.send(Ev::Jarvis(out));
                }
            }),
        };
        let backend = RealBackend {
            hotkeys,
            spawner: ChildSpawner,
            sounds: Sounds::new(dirs.runtime.clone()),
            mute: SinkMute::default(),
            queue,
            runner: Runner::start(Box::new(env), mic_muted.clone()),
            mic_muted,
        };
        Self::with_backend(paths, dirs, events, Box::new(backend), tx, rx)
    }

    fn with_backend(
        paths: ConfigPaths,
        dirs: Dirs,
        events: EventTx,
        backend: Box<dyn Backend>,
        tx: mpsc::Sender<Ev>,
        rx: mpsc::Receiver<Ev>,
    ) -> Self {
        let state = Arc::new(Mutex::new(placeholder_state()));
        let mut machine = Machine::new(paths, dirs, events, backend, tx.clone(), state.clone());
        machine.startup(Instant::now());
        if let Err(e) = std::thread::Builder::new()
            .name("hako-voice".into())
            .spawn(move || run(machine, rx))
        {
            warn!("voice: cannot start its thread: {e}");
        }
        Self { tx, state }
    }

    /// For `HotkeyService::on_trigger`.
    pub fn trigger_sink(&self) -> TriggerSink {
        let tx = self.tx.clone();
        Box::new(move |action, edge| {
            let _ = tx.send(Ev::Trigger(action, edge));
        })
    }

    /// The last published state (no waiting).
    pub fn state(&self) -> VoiceState {
        lock(&self.state).clone()
    }

    /// Apply `ops`: settings all or nothing, then the actions. Blocking.
    pub fn patch(&self, ops: Vec<VoiceOp>) -> Result<VoiceState, PatchError> {
        let (reply, done) = mpsc::channel();
        self.tx
            .send(Ev::Patch(ops, reply))
            .map_err(|_| PatchError::Io(anyhow::anyhow!("the voice service is not running")))?;
        done.recv_timeout(REPLY_WAIT)
            .map_err(|_| PatchError::Io(anyhow::anyhow!("the voice service did not answer")))?
    }

    /// `VoiceControl`. Blocking (briefly).
    pub fn control(&self, params: VoiceControlParams) -> Result<VoiceState, String> {
        let (reply, done) = mpsc::channel();
        self.tx
            .send(Ev::Control(params, reply))
            .map_err(|_| "the voice service is not running".to_string())?;
        done.recv_timeout(REPLY_WAIT)
            .map_err(|_| "the voice service did not answer".to_string())?
    }

    /// Daemon stop: capture closed, worker ended, the sink unmuted. Blocking.
    pub fn shutdown(&self) {
        let (reply, done) = mpsc::channel();
        if self.tx.send(Ev::Shutdown(reply)).is_ok() {
            let _ = done.recv_timeout(REPLY_WAIT);
        }
    }
}

fn placeholder_state() -> VoiceState {
    VoiceState {
        file: VoiceFile::default(),
        engine: EngineStatus::Off,
        loaded: None,
        installed: Vec::new(),
        custom: Vec::new(),
        vad_ready: false,
        history: Vec::new(),
        outputs: Vec::new(),
        accelerators: Vec::new(),
        layout: None,
        sources: Vec::new(),
        wake_model: None,
        jarvis_listening: false,
        last_command: None,
        problems: Vec::new(),
        load_error: None,
    }
}

fn run(mut machine: Machine, rx: mpsc::Receiver<Ev>) {
    let ran = catch_unwind(AssertUnwindSafe(|| loop {
        let now = Instant::now();
        let next = machine.tick(now);
        let ev = match rx.recv_timeout(next.saturating_duration_since(now)) {
            Ok(ev) => ev,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if !machine.handle(ev, Instant::now()) {
            return;
        }
    }));
    if ran.is_err() {
        error!("voice: the service crashed; dictation is off until hakod restarts");
        let _ = catch_unwind(AssertUnwindSafe(|| machine.stop_all()));
        let _ = machine.events.send(AdapterEvent::Error {
            device: None,
            message: "Voice: the service crashed; restart hakod".into(),
        });
    }
}

// ---------------------------------------------------------------------------
// The state machine.

struct Session {
    id: u64,
    model: ModelChoice,
    /// `voice.toml` with the alt profile applied, as at the start.
    cfg: VoiceFile,
    layout: String,
    recording: bool,
    started: Instant,
    /// Stopped: `End` goes once it has begun.
    stopped: bool,
    /// `Begin` sent; `live`: text is typed as it streams.
    begun: bool,
    live: bool,
    ended: Option<Instant>,
    /// Samples still to drop (ping bleed).
    bleed: usize,
    pcm: Vec<f32>,
    sent: usize,
    /// Processed text output so far.
    typed: String,
    /// A Jarvis take: transcribed for the command runner, never output.
    clip: Option<Listen>,
    /// Started by a spoken "dictate": the dictation hotkey stops it.
    by_voice: bool,
}

impl Session {
    fn ctx(&self) -> Ctx {
        Ctx {
            method: self.cfg.output.method,
            out: self.cfg.output.clone(),
            layout: self.layout.clone(),
        }
    }

    fn model_name(&self) -> String {
        match &self.model {
            ModelChoice::Catalog { id, .. } => id.clone(),
            ModelChoice::Custom { file } => file.clone(),
        }
    }
}

/// What disk, PipeWire and the session offer (refreshed every `REFRESH`).
#[derive(Default)]
struct Scan {
    installed: Vec<InstalledModel>,
    custom: Vec<CustomModel>,
    vad_ready: bool,
    outputs: Vec<OutputCheck>,
    sources: Vec<(String, String)>,
    engine_missing: Option<String>,
    /// The wake-word model is extracted whole / only partly.
    kws_ready: bool,
    kws_partial: bool,
}

/// Jarvis's ear: its own capture stream and wake-word worker, and the take
/// in progress.
struct Wake {
    ear: Ear,
    capture: Option<Box<dyn Capture>>,
    capture_gen: u64,
    capture_mic: Option<String>,
    /// A capture that could not be opened is tried again from then.
    capture_retry: Option<Instant>,
    worker: Option<Box<dyn Worker>>,
    worker_gen: u64,
    /// (wake word, sensitivity) the worker listens with.
    listens_for: Option<(String, f32)>,
    spawned_at: Instant,
    crashes: u32,
    restart_at: Option<Instant>,
    error: Option<String>,
    /// Last told to the runner.
    listening: Option<bool>,
    last: Option<CommandOutcome>,
}

impl Wake {
    fn new(now: Instant) -> Self {
        Self {
            ear: Ear::default(),
            capture: None,
            capture_gen: 0,
            capture_mic: None,
            capture_retry: None,
            worker: None,
            worker_gen: 0,
            listens_for: None,
            spawned_at: now,
            crashes: 0,
            restart_at: None,
            error: None,
            // The runner starts out not listening.
            listening: Some(false),
            last: None,
        }
    }
}

struct Machine {
    paths: ConfigPaths,
    dirs: Dirs,
    events: EventTx,
    backend: Box<dyn Backend>,
    tx: mpsc::Sender<Ev>,
    state: Arc<Mutex<VoiceState>>,
    file: VoiceFile,
    load_error: Option<String>,

    worker: Option<Box<dyn Worker>>,
    worker_gen: u64,
    spawned_at: Instant,
    accelerators: Vec<String>,
    loaded: Option<(ModelChoice, ModelCaps)>,
    loading: Option<ModelChoice>,
    crashes: u32,
    restart_at: Option<Instant>,
    preload: bool,
    idle_since: Instant,

    capture: Option<Box<dyn Capture>>,
    capture_gen: u64,
    capture_mic: Option<String>,

    sessions: VecDeque<Session>,
    next_session: u64,
    mute_at: Option<Instant>,
    muted: bool,

    /// Shown as `EngineStatus::Error` while idle, until something works.
    error: Option<String>,
    output_error: Option<String>,
    download: Option<(u64, Download)>,
    download_gen: u64,
    download_error: Option<String>,
    history: History,
    entries: Vec<HistoryEntry>,
    layout: Option<String>,
    hotkeys_on: Option<(bool, bool)>,
    scan: Scan,
    wake: Wake,
    scanned: Option<Instant>,
    sent: Option<VoiceState>,
}

fn model_file_name(choice: &ModelChoice) -> Option<String> {
    match choice {
        ModelChoice::Catalog { id, quant } => {
            CATALOG.model(id)?.file(quant).map(|f| f.file.clone())
        }
        ModelChoice::Custom { file } => Some(file.clone()),
    }
}

/// `file` with the alt profile's overrides.
fn effective(file: &VoiceFile, alt: bool) -> VoiceFile {
    let mut f = file.clone();
    if alt {
        if let Some(m) = &file.alt.model {
            f.model = Some(m.clone());
        }
        if let Some(l) = &file.alt.language {
            f.language = Some(l.clone());
        }
        if let Some(t) = file.alt.translate {
            f.translate = t;
        }
        if let Some(m) = file.alt.method {
            f.output.method = m;
        }
    }
    f
}

/// When a crashed worker may start again (the wait doubles per crash,
/// and starts over once it ran `RECOVERED_AFTER`).
fn restart_at(crashes: &mut u32, spawned_at: Instant, now: Instant) -> Instant {
    if now.duration_since(spawned_at) >= RECOVERED_AFTER {
        *crashes = 0;
    }
    let wait = RESTART_FIRST
        .saturating_mul(1 << (*crashes).min(5))
        .min(RESTART_MAX);
    *crashes += 1;
    now + wait
}

/// `piece` appended to `typed` with one space between words.
fn join(typed: &str, piece: &str) -> String {
    if piece.is_empty() || typed.is_empty() || typed.ends_with(char::is_whitespace) {
        piece.to_string()
    } else {
        format!(" {piece}")
    }
}

impl Machine {
    fn new(
        paths: ConfigPaths,
        dirs: Dirs,
        events: EventTx,
        backend: Box<dyn Backend>,
        tx: mpsc::Sender<Ev>,
        state: Arc<Mutex<VoiceState>>,
    ) -> Self {
        let (file, load_error) = match load_voice(&paths) {
            Ok(f) => (f, None),
            Err(e) => {
                warn!("voice.toml: {e:#}");
                (VoiceFile::default(), Some(format!("{e:#}")))
            }
        };
        let history = History::new(dirs.history.clone());
        let now = Instant::now();
        Self {
            entries: history.load(),
            history,
            paths,
            dirs,
            events,
            backend,
            tx,
            state,
            file,
            load_error,
            worker: None,
            worker_gen: 0,
            spawned_at: now,
            accelerators: Vec::new(),
            loaded: None,
            loading: None,
            crashes: 0,
            restart_at: None,
            preload: false,
            idle_since: now,
            capture: None,
            capture_gen: 0,
            capture_mic: None,
            sessions: VecDeque::new(),
            next_session: 1,
            mute_at: None,
            muted: false,
            error: None,
            output_error: None,
            download: None,
            download_gen: 0,
            download_error: None,
            layout: None,
            hotkeys_on: None,
            scan: Scan::default(),
            wake: Wake::new(now),
            scanned: None,
            sent: None,
        }
    }

    fn startup(&mut self, now: Instant) {
        self.preload = self.file.advanced.preload;
        if self.file.enabled {
            self.layout = Some(self.backend.detect_layout());
        }
        self.rescan();
        self.pump(now);
        self.wake_sync(now);
        self.publish();
    }

    // -- inputs --------------------------------------------------------------

    /// `false`: stop the actor.
    fn handle(&mut self, ev: Ev, now: Instant) -> bool {
        match ev {
            Ev::Trigger(action, edge) => self.trigger(action, edge, now),
            Ev::Control(p, reply) => {
                let r = self.control(&p, now).map(|()| {
                    self.publish();
                    lock(&self.state).clone()
                });
                let _ = reply.send(r);
            }
            Ev::Patch(ops, reply) => {
                let r = self.patch(&ops, now).map(|()| {
                    self.rescan();
                    self.publish();
                    lock(&self.state).clone()
                });
                let _ = reply.send(r);
            }
            Ev::Pcm(gen, pcm) => {
                if gen == self.capture_gen {
                    self.pcm(&pcm);
                }
                return true;
            }
            Ev::Worker(gen, msg) => {
                if gen == self.worker_gen {
                    match msg {
                        Some(m) => self.worker_says(m, now),
                        None => self.worker_exited(now),
                    }
                }
            }
            Ev::JarvisPcm(gen, pcm) => {
                if gen == self.wake.capture_gen {
                    self.wake.ear.pcm(&pcm);
                    // Paused while dictation records.
                    if self.recording().is_none() {
                        if let Some(w) = &mut self.wake.worker {
                            w.send(ToWorker::Pcm(pcm));
                        }
                    }
                }
                return true;
            }
            Ev::JarvisWorker(gen, msg) => {
                if gen == self.wake.worker_gen {
                    self.wake_worker_says(msg, now);
                }
            }
            Ev::Jarvis(out) => self.jarvis_says(out, now),
            Ev::Download(gen, result) => {
                if self.download.as_ref().is_some_and(|(g, _)| *g == gen) {
                    let (_, dl) = self.download.take().expect("checked");
                    match result {
                        Ok(true) => {
                            info!("voice: {} {} downloaded", dl.id, dl.quant);
                            self.download_error = None;
                            self.preload = self.file.advanced.preload;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warn!("voice: download of {} failed: {e}", dl.id);
                            self.download_error = Some(format!("Download of {}: {e}", dl.id));
                        }
                    }
                    self.rescan();
                    self.pump(now);
                }
            }
            Ev::OutputFailed(e) => {
                // One ping per new failure, not per piece of text.
                if self.output_error.as_ref() != Some(&e) {
                    warn!("voice output: {e}");
                    self.backend.play(Sound::Error, &self.file.sounds);
                    self.output_error = Some(e);
                }
            }
            Ev::Shutdown(reply) => {
                self.stop_all();
                let _ = reply.send(());
                return false;
            }
        }
        self.wake_sync(now);
        self.publish();
        true
    }

    fn trigger(&mut self, action: HotkeyAction, edge: HotkeyEdge, now: Instant) {
        let alt = match action {
            HotkeyAction::VoiceTranscribe => false,
            HotkeyAction::VoiceTranscribeAlt => true,
            HotkeyAction::VoiceCancel => {
                if edge == HotkeyEdge::Fire || edge == HotkeyEdge::Start {
                    self.cancel(now);
                }
                return;
            }
            HotkeyAction::JarvisListen => {
                self.push_to_command(edge, now);
                return;
            }
        };
        match edge {
            HotkeyEdge::Start | HotkeyEdge::Fire => {
                // A dictation "dictate" started has no hotkey of its own.
                if self.recording().is_some_and(|i| self.sessions[i].by_voice) {
                    self.stop(now);
                    return;
                }
                if let Err(e) = self.start(alt, now) {
                    info!("voice: {e}");
                }
            }
            HotkeyEdge::Stop => self.stop(now),
            HotkeyEdge::Discard => self.cancel(now),
        }
    }

    fn control(&mut self, p: &VoiceControlParams, now: Instant) -> Result<(), String> {
        match p.action {
            VoiceAction::Start => self.start(p.alt, now),
            VoiceAction::Stop => {
                self.stop(now);
                Ok(())
            }
            VoiceAction::Toggle => {
                if self.recording().is_some() {
                    self.stop(now);
                    Ok(())
                } else {
                    self.start(p.alt, now)
                }
            }
            VoiceAction::Cancel => {
                self.cancel(now);
                Ok(())
            }
        }
    }

    // -- sessions ------------------------------------------------------------

    fn recording(&self) -> Option<usize> {
        self.sessions.iter().position(|s| s.recording)
    }

    fn session(&mut self, id: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    /// The model file if it is on disk whole.
    fn model_path(&self, choice: &ModelChoice) -> Option<PathBuf> {
        let path = self.dirs.models.join(model_file_name(choice)?);
        let size = std::fs::metadata(&path).ok()?.len();
        match choice {
            ModelChoice::Catalog { id, quant } => {
                (CATALOG.model(id)?.file(quant)?.size == size).then_some(path)
            }
            ModelChoice::Custom { .. } => Some(path),
        }
    }

    fn vad_path(&self) -> Option<PathBuf> {
        let f = CATALOG.vad.files.first()?;
        let path = self.dirs.models.join(&f.file);
        std::fs::metadata(&path)
            .is_ok_and(|m| m.len() == f.size)
            .then_some(path)
    }

    fn usable(&self) -> bool {
        self.file.enabled
            && self.load_error.is_none()
            && self.scan.engine_missing.is_none()
            && self
                .file
                .model
                .as_ref()
                .is_some_and(|m| self.model_path(m).is_some())
    }

    fn start(&mut self, alt: bool, now: Instant) -> Result<(), String> {
        if self.recording().is_some() {
            return Ok(());
        }
        if !self.file.enabled {
            return Err("Voice is off".into());
        }
        if let Some(e) = &self.load_error {
            return Err(e.clone());
        }
        let cfg = effective(&self.file, alt);
        let fail = |m: &mut Self, e: String| {
            m.backend.play(Sound::Error, &m.file.sounds);
            m.error = Some(e.clone());
            Err(e)
        };
        let Some(model) = cfg.model.clone() else {
            return fail(self, "pick a model first".into());
        };
        if self.model_path(&model).is_none() {
            return fail(self, "the model is not downloaded".into());
        }
        if let Err(e) = self.backend.spawner().available() {
            return fail(self, e);
        }
        if let Err(e) = self.open_capture() {
            return fail(self, format!("the microphone could not be opened: {e}"));
        }
        if self.wake.ear.cancel().is_some() {
            debug!("jarvis: the take gave way to dictation");
        }
        self.backend.play(Sound::Start, &cfg.sounds);
        self.backend.recording(true);
        if cfg.advanced.mute_while_recording {
            // After the ping, which plays on the same sink.
            self.mute_at = Some(now + BLEED);
        }
        let layout = match &cfg.output.layout {
            Some(l) => l.clone(),
            None => {
                let l = self.backend.detect_layout();
                self.layout = Some(l.clone());
                l
            }
        };
        if matches!(
            cfg.output.method,
            OutputMethod::Type | OutputMethod::Paste | OutputMethod::Command
        ) {
            self.backend.output(Job::Warm);
        }
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.push_back(Session {
            id,
            model,
            cfg,
            layout,
            recording: true,
            started: now,
            stopped: false,
            begun: false,
            live: false,
            ended: None,
            bleed: (SAMPLE_RATE as u128 * BLEED.as_millis() / 1000) as usize,
            pcm: Vec::new(),
            sent: 0,
            typed: String::new(),
            clip: None,
            by_voice: false,
        });
        self.error = None;
        info!(
            "voice: recording (session {id}{})",
            if alt { ", alt" } else { "" }
        );
        self.pump(now);
        Ok(())
    }

    /// Recording ends: capture closed (unless always on), stop ping, `End`.
    fn stop(&mut self, now: Instant) {
        let Some(i) = self.recording() else {
            return;
        };
        self.end_recording(i);
        let s = &mut self.sessions[i];
        s.stopped = true;
        let sounds = s.cfg.sounds.clone();
        self.backend.play(Sound::Stop, &sounds);
        self.pump(now);
    }

    fn end_recording(&mut self, i: usize) {
        self.sessions[i].recording = false;
        if !self.always_on() {
            self.close_capture();
        }
        self.unmute();
        self.backend.recording(false);
    }

    fn unmute(&mut self) {
        self.mute_at = None;
        if std::mem::take(&mut self.muted) {
            self.backend.mute(false);
        }
    }

    /// The recording (else the newest session) goes: nothing is output.
    fn cancel(&mut self, now: Instant) {
        let newest = self.sessions.iter().rposition(|s| s.clip.is_none());
        let Some(i) = self.recording().or(newest) else {
            return;
        };
        if self.sessions[i].recording {
            self.end_recording(i);
        }
        let s = self.sessions.remove(i).expect("index is valid");
        if s.begun {
            if let Some(w) = &mut self.worker {
                w.send(ToWorker::Msg(WorkerIn::Cancel { session: s.id }));
            }
        }
        self.backend.cancel_start_ping();
        self.backend.play(Sound::Cancel, &s.cfg.sounds);
        info!("voice: session {} cancelled", s.id);
        self.went_idle(now);
    }

    fn fail(&mut self, i: usize, message: String, now: Instant) {
        if self.sessions[i].recording {
            self.end_recording(i);
        }
        let s = self.sessions.remove(i).expect("index is valid");
        warn!("voice: session {}: {message}", s.id);
        match s.clip {
            // The runner reports it (and pings).
            Some(kind) => self.backend.jarvis(RunnerIn::Failed {
                kind,
                message: message.clone(),
            }),
            None => self.backend.play(Sound::Error, &s.cfg.sounds),
        }
        self.error = Some(message);
        self.went_idle(now);
    }

    fn went_idle(&mut self, now: Instant) {
        if self.sessions.is_empty() {
            self.idle_since = now;
        }
    }

    // -- capture -------------------------------------------------------------

    fn always_on(&self) -> bool {
        self.file.advanced.always_on_mic && self.usable()
    }

    fn open_capture(&mut self) -> Result<(), String> {
        if self.capture.is_some() && self.capture_mic == self.file.mic {
            return Ok(());
        }
        self.close_capture();
        self.capture_gen += 1;
        let (tx, gen) = (self.tx.clone(), self.capture_gen);
        let capture = self.backend.open_capture(
            self.file.mic.as_deref(),
            Box::new(move |pcm| {
                let _ = tx.send(Ev::Pcm(gen, pcm.to_vec()));
            }),
        )?;
        self.capture = Some(capture);
        self.capture_mic = self.file.mic.clone();
        Ok(())
    }

    fn close_capture(&mut self) {
        if self.capture.take().is_some() {
            self.capture_gen += 1;
        }
    }

    fn pcm(&mut self, pcm: &[f32]) {
        let Some(i) = self.recording() else {
            return;
        };
        let s = &mut self.sessions[i];
        let skip = s.bleed.min(pcm.len());
        s.bleed -= skip;
        s.pcm.extend_from_slice(&pcm[skip..]);
        if s.begun {
            self.feed(i, false);
        }
    }

    /// Send buffered audio in `CHUNK`s (`all`: the rest too).
    fn feed(&mut self, i: usize, all: bool) {
        let Some(w) = &mut self.worker else {
            return;
        };
        let s = &mut self.sessions[i];
        while s.pcm.len() - s.sent >= CHUNK || (all && s.sent < s.pcm.len()) {
            let end = (s.sent + CHUNK).min(s.pcm.len());
            w.send(ToWorker::Pcm(s.pcm[s.sent..end].to_vec()));
            s.sent = end;
        }
    }

    // -- the worker ----------------------------------------------------------

    fn load_msg(&self, model: &ModelChoice) -> Option<WorkerIn> {
        let path = self.model_path(model)?;
        let (device, cpu_only) = match &self.file.advanced.accelerator {
            Accelerator::Auto => (None, false),
            Accelerator::Cpu => (None, true),
            Accelerator::Device { name } => (Some(name.clone()), false),
        };
        Some(WorkerIn::Load {
            path: path.to_string_lossy().into_owned(),
            vad: self
                .vad_path()
                .filter(|_| self.file.advanced.vad)
                .map(|p| p.to_string_lossy().into_owned()),
            device,
            cpu_only,
        })
    }

    fn ensure_worker(&mut self, now: Instant) -> bool {
        if self.worker.is_some() {
            return true;
        }
        if self.restart_at.is_some_and(|at| now < at) {
            return false;
        }
        self.worker_gen += 1;
        let (tx, gen) = (self.tx.clone(), self.worker_gen);
        match self.backend.spawner().spawn(Box::new(move |m| {
            let _ = tx.send(Ev::Worker(gen, m));
        })) {
            Ok(w) => {
                self.worker = Some(w);
                self.spawned_at = now;
                self.restart_at = None;
                true
            }
            Err(e) => {
                self.back_off(now);
                while let Some(i) = self.sessions.iter().position(|s| !s.begun) {
                    self.fail(i, e.clone(), now);
                }
                self.error = Some(e);
                false
            }
        }
    }

    fn back_off(&mut self, now: Instant) {
        self.restart_at = Some(restart_at(&mut self.crashes, self.spawned_at, now));
    }

    /// End the worker on purpose (its exit is not a crash).
    fn unload(&mut self) {
        if self.worker.take().is_some() {
            info!("voice: model unloaded");
        }
        self.worker_gen += 1;
        self.loaded = None;
        self.loading = None;
    }

    /// Move sessions along: `End` for stopped ones, `Load` / `Begin` for
    /// the first that has not begun, preload when idle.
    fn pump(&mut self, now: Instant) {
        for i in 0..self.sessions.len() {
            let s = &self.sessions[i];
            if s.begun && s.stopped && s.ended.is_none() {
                self.feed(i, true);
                let id = self.sessions[i].id;
                if let Some(w) = &mut self.worker {
                    w.send(ToWorker::Msg(WorkerIn::End { session: id }));
                }
                self.sessions[i].ended = Some(now);
            }
        }
        while let Some(i) = self.sessions.iter().position(|s| !s.begun) {
            if !self.ensure_worker(now) {
                return;
            }
            let model = self.sessions[i].model.clone();
            match &self.loaded {
                Some((m, caps)) if *m == model => {
                    let caps = caps.clone();
                    self.begin(i, &caps, now);
                }
                _ => {
                    if self.loading.is_none() {
                        self.send_load(model, now);
                    }
                    return;
                }
            }
        }
        if self.preload && self.usable() && self.sessions.is_empty() {
            self.preload = false;
            let model = self.file.model.clone().expect("usable");
            if self.loaded.as_ref().map(|(m, _)| m) != Some(&model)
                && self.loading.is_none()
                && self.ensure_worker(now)
            {
                self.send_load(model, now);
            }
        }
    }

    fn send_load(&mut self, model: ModelChoice, now: Instant) {
        let Some(msg) = self.load_msg(&model) else {
            while let Some(i) = self
                .sessions
                .iter()
                .position(|s| !s.begun && s.model == model)
            {
                self.fail(i, "the model is not downloaded".into(), now);
            }
            return;
        };
        info!("voice: loading {model:?}");
        if let Some(w) = &mut self.worker {
            w.send(ToWorker::Msg(msg));
        }
        self.loaded = None;
        self.loading = Some(model);
    }

    fn begin(&mut self, i: usize, caps: &ModelCaps, now: Instant) {
        let s = &mut self.sessions[i];
        let cfg = &s.cfg;
        // Jarvis takes come out once, at the end.
        let at_end = s.clip.is_some();
        s.live = caps.streaming && cfg.output.method.is_live() && !at_end;
        let msg = WorkerIn::Begin {
            session: s.id,
            streaming: caps.streaming && s.clip.is_none(),
            pause_ms: (!caps.streaming && cfg.streaming.transcribe_at_pauses && !at_end)
                .then_some(cfg.streaming.pause_ms),
            vad: cfg.advanced.vad,
            language: cfg.language.clone(),
            translate: cfg.translate,
            vocabulary: cfg.text.custom_words.clone(),
            punctuation: cfg.text.punctuation,
        };
        s.begun = true;
        if let Some(w) = &mut self.worker {
            w.send(ToWorker::Msg(msg));
        }
        self.feed(i, false);
        if self.sessions[i].stopped {
            self.pump(now);
        }
    }

    fn worker_says(&mut self, msg: WorkerOut, now: Instant) {
        match msg {
            WorkerOut::Ready { devices, .. } => self.accelerators = devices,
            WorkerOut::Loaded { caps } => {
                let Some(model) = self.loading.take() else {
                    return;
                };
                info!("voice: model loaded ({} on {})", caps.arch, caps.backend);
                self.loaded = Some((model, caps));
                self.crashes = 0;
                self.error = None;
                self.pump(now);
            }
            WorkerOut::Partial {
                session,
                committed,
                tentative,
            } => {
                let Some(i) = self
                    .session(session)
                    .filter(|&i| self.sessions[i].clip.is_none())
                else {
                    return;
                };
                let _ = self.events.send(AdapterEvent::VoicePartial(VoicePartial {
                    session,
                    committed: committed.clone(),
                    tentative,
                }));
                let s = &mut self.sessions[i];
                if s.live {
                    let settled = text::process(text::settled(&committed), &s.cfg.text, None);
                    let piece = text::remaining(&s.typed, &settled);
                    self.emit(i, piece);
                }
            }
            WorkerOut::Segment { session, text: t } => {
                let Some(i) = self.session(session) else {
                    return;
                };
                let s = &self.sessions[i];
                if s.clip.is_some() {
                    return;
                }
                let processed = text::process(&t, &s.cfg.text, s.cfg.language.as_deref());
                if s.cfg.output.method != OutputMethod::Clipboard {
                    let piece = join(&s.typed, &processed);
                    self.emit(i, piece);
                }
                let s = &self.sessions[i];
                let _ = self.events.send(AdapterEvent::VoicePartial(VoicePartial {
                    session,
                    committed: s.typed.clone(),
                    tentative: String::new(),
                }));
            }
            WorkerOut::Final {
                session,
                text: t,
                language,
            } => {
                if let Some(i) = self.session(session) {
                    self.finish(i, &t, language.as_deref(), now);
                }
            }
            // Wake words come from Jarvis's own worker, never this one.
            WorkerOut::Speech { .. } | WorkerOut::Wake { .. } => {}
            WorkerOut::Error { session, message } => {
                match session.and_then(|id| self.session(id)) {
                    Some(i) => self.fail(i, message, now),
                    None if self.loading.is_some() => {
                        let model = self.loading.take().expect("checked");
                        while let Some(i) = self
                            .sessions
                            .iter()
                            .position(|s| !s.begun && s.model == model)
                        {
                            self.fail(i, format!("the model could not be loaded: {message}"), now);
                        }
                        self.error = Some(format!("the model could not be loaded: {message}"));
                    }
                    None => {
                        warn!("hako-stt: {message}");
                        self.error = Some(message);
                    }
                }
            }
        }
    }

    /// Output `piece` for session `i` (in order behind earlier output).
    fn emit(&mut self, i: usize, piece: String) {
        if piece.is_empty() {
            return;
        }
        let s = &mut self.sessions[i];
        s.typed.push_str(&piece);
        let ctx = s.ctx();
        self.backend.output(Job::Text(ctx, piece));
    }

    fn finish(&mut self, i: usize, raw: &str, language: Option<&str>, now: Instant) {
        if let Some(kind) = self.sessions[i].clip {
            let s = self.sessions.remove(i).expect("index is valid");
            debug!("jarvis: take {} transcribed", s.id);
            self.backend.jarvis(RunnerIn::Heard {
                kind,
                text: raw.trim().to_string(),
                file: Box::new(self.file.clone()),
            });
            self.went_idle(now);
            return;
        }
        let s = &self.sessions[i];
        let lang = language.or(s.cfg.language.as_deref());
        let fin = text::process(raw, &s.cfg.text, lang);
        let ctx = s.ctx();
        if s.live && s.cfg.output.fix_up_at_end {
            let (erase, add) = text::fix_up(&s.typed, &fin);
            if erase > 0 {
                self.backend.output(Job::Erase(ctx.clone(), erase));
            }
            let s = &mut self.sessions[i];
            s.typed = fin.clone();
            if !add.is_empty() {
                self.backend.output(Job::Text(ctx.clone(), add));
            }
        } else if s.cfg.output.method == OutputMethod::Clipboard {
            if !fin.is_empty() {
                self.backend.output(Job::Copy(fin.clone()));
            }
            self.sessions[i].typed = fin.clone();
        } else {
            let piece = text::remaining(&s.typed, &fin);
            self.emit(i, piece);
        }
        let s = self.sessions.remove(i).expect("index is valid");
        if !s.typed.is_empty() && s.cfg.output.method != OutputMethod::Clipboard {
            if s.cfg.output.trailing_space {
                self.backend.output(Job::Text(ctx.clone(), " ".into()));
            }
            if let Some(key) = s.cfg.output.auto_submit {
                self.backend.output(Job::Submit(ctx, key));
            }
        }
        let _ = self.events.send(AdapterEvent::VoicePartial(VoicePartial {
            session: s.id,
            committed: fin.clone(),
            tentative: String::new(),
        }));
        info!(
            "voice: session {} done ({} characters)",
            s.id,
            fin.chars().count()
        );
        if !fin.is_empty() && s.cfg.history.keep > 0 {
            let at = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let entry = HistoryEntry {
                at,
                text: fin,
                model: s.model_name(),
                audio: None,
            };
            let audio = s.cfg.history.keep_audio.then_some((&s.pcm[..], s.id));
            match self.history.add(entry, audio, s.cfg.history.keep) {
                Ok(entries) => self.entries = entries,
                Err(e) => warn!("voice history: {e:#}"),
            }
        }
        self.went_idle(now);
    }

    fn worker_exited(&mut self, now: Instant) {
        warn!("voice: the speech engine stopped unexpectedly");
        self.worker = None;
        self.worker_gen += 1;
        let was_loading = self.loading.take().is_some();
        self.loaded = None;
        self.back_off(now);
        let message = "the speech engine stopped unexpectedly".to_string();
        while let Some(i) = self.sessions.iter().position(|s| s.begun || was_loading) {
            self.fail(i, message.clone(), now);
        }
        self.error = Some(message);
    }

    // -- time ----------------------------------------------------------------

    /// Deadlines: the delayed mute, the length cap, a hung worker, the idle
    /// unload, a worker restart, the periodic refresh. Returns the next one.
    fn tick(&mut self, now: Instant) -> Instant {
        let mut next = now + REFRESH;
        let mut changed = false;
        if let Some(at) = self.mute_at {
            if now >= at {
                self.mute_at = None;
                if self.recording().is_some() {
                    self.backend.mute(true);
                    self.muted = true;
                }
            } else {
                next = next.min(at);
            }
        }
        if let Some(i) = self.recording() {
            let s = &self.sessions[i];
            let cap = s.started + Duration::from_secs(s.cfg.advanced.max_seconds.into());
            if now >= cap {
                info!("voice: session {} reached the maximum length", s.id);
                self.stop(now);
                changed = true;
            } else if self.capture.as_ref().is_some_and(|c| c.failed()) {
                self.fail(i, "the microphone stopped".into(), now);
                self.close_capture();
                changed = true;
            } else {
                next = next.min(cap);
            }
        }
        if let Some(i) = self
            .sessions
            .iter()
            .position(|s| s.ended.is_some_and(|at| now >= at + FINAL_WAIT))
        {
            self.unload();
            self.fail(i, "the speech engine did not answer".into(), now);
            while let Some(i) = self.sessions.iter().position(|s| s.begun) {
                self.fail(i, "the speech engine did not answer".into(), now);
            }
            changed = true;
        }
        if let Some(at) = self.sessions.iter().filter_map(|s| s.ended).min() {
            next = next.min(at + FINAL_WAIT);
        }
        if self.worker.is_some() && self.sessions.is_empty() && self.loading.is_none() {
            let off = !self.file.enabled;
            match self.file.advanced.unload_after_s {
                _ if off => {
                    self.unload();
                    changed = true;
                }
                Some(s) => {
                    let at = self.idle_since + Duration::from_secs(s.into());
                    if now >= at {
                        self.unload();
                        changed = true;
                    } else {
                        next = next.min(at);
                    }
                }
                None => {}
            }
        }
        if let Some(at) = self.restart_at {
            if now >= at {
                self.restart_at = None;
                self.pump(now);
                changed = true;
            } else {
                next = next.min(at);
            }
        }
        let (done, at) = self.wake.ear.tick(now);
        if let Some(done) = done {
            self.take_done(done, now);
            changed = true;
        }
        if let Some(at) = at {
            next = next.min(at);
        }
        if let Some(at) = self.wake.restart_at {
            if now >= at {
                self.wake.restart_at = None;
                changed = true;
            } else {
                next = next.min(at);
            }
        }
        if self.wake.capture.as_ref().is_some_and(|c| c.failed()) {
            warn!("jarvis: the microphone stopped");
            self.close_wake_capture();
            self.wake.error = Some("the microphone stopped".into());
            changed = true;
        }
        // Always-on mic follows the settings.
        if self.always_on() && self.capture.is_none() && self.recording().is_none() {
            if let Err(e) = self.open_capture() {
                warn!("voice: always-on microphone: {e}");
            }
        } else if !self.always_on() && self.recording().is_none() && self.capture.is_some() {
            self.close_capture();
        }
        let every = if self.download.is_some() {
            PROGRESS
        } else {
            REFRESH
        };
        match self.scanned {
            Some(at) if now < at + every => next = next.min(at + every),
            _ => {
                self.reload(now);
                self.rescan();
                changed = true;
                next = next.min(now + every);
            }
        }
        self.wake_sync(now);
        if changed {
            self.publish();
        }
        next
    }

    /// Follow hand edits of `voice.toml`; an unreadable file turns Voice off
    /// until it is fixed (it is never overwritten).
    fn reload(&mut self, now: Instant) {
        match load_voice(&self.paths) {
            Ok(file) => {
                self.load_error = None;
                if file != self.file {
                    self.settings_changed(file, now);
                }
            }
            Err(e) => {
                let e = format!("{e:#}");
                if self.load_error.as_ref() != Some(&e) {
                    warn!("voice.toml: {e}");
                    self.load_error = Some(e);
                }
            }
        }
    }

    // -- patch ---------------------------------------------------------------

    fn patch(&mut self, ops: &[VoiceOp], now: Instant) -> Result<(), PatchError> {
        if ops.len() > MAX_VOICE_OPS {
            return Err(PatchError::Invalid(format!(
                "at most {MAX_VOICE_OPS} ops per patch"
            )));
        }
        let settings: Vec<&VoiceOp> = ops.iter().filter(|op| is_setting(op)).collect();
        for op in ops.iter().filter(|op| !is_setting(op)) {
            self.check_action(op).map_err(PatchError::Invalid)?;
        }
        if !settings.is_empty() {
            let mut next = load_voice(&self.paths).map_err(PatchError::Io)?;
            settings
                .iter()
                .for_each(|op| apply_voice_settings(&mut next, op));
            next.validate().map_err(PatchError::Invalid)?;
            let (file, ()) = update_voice(&self.paths, |f| {
                settings.iter().for_each(|op| apply_voice_settings(f, op));
                Ok(())
            })
            .map_err(PatchError::Io)?;
            self.load_error = None;
            self.settings_changed(file, now);
        }
        for op in ops.iter().filter(|op| !is_setting(op)) {
            self.action(op).map_err(PatchError::Io)?;
        }
        Ok(())
    }

    fn check_action(&self, op: &VoiceOp) -> Result<(), String> {
        match op {
            VoiceOp::Download { id, quant } => {
                CATALOG
                    .model(id)
                    .and_then(|m| m.file(quant))
                    .ok_or_else(|| format!("no catalog file {id} {quant}"))?;
                match &self.download {
                    Some((_, d)) if d.id != *id || d.quant != *quant => {
                        Err(format!("{} is downloading; one download at a time", d.id))
                    }
                    _ => Ok(()),
                }
            }
            VoiceOp::DeleteModel { id, quant } => {
                CATALOG
                    .model(id)
                    .and_then(|m| m.file(quant))
                    .ok_or_else(|| format!("no catalog file {id} {quant}"))?;
                self.not_in_use(&ModelChoice::Catalog {
                    id: id.clone(),
                    quant: quant.clone(),
                })
            }
            VoiceOp::DeleteCustom { file } => {
                if !valid_custom_file(file) {
                    return Err(format!("invalid model file name {file:?}"));
                }
                self.not_in_use(&ModelChoice::Custom { file: file.clone() })
            }
            VoiceOp::DownloadWake => match &self.download {
                Some((_, d)) if d.id != CATALOG.kws.id => {
                    Err(format!("{} is downloading; one download at a time", d.id))
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    fn not_in_use(&self, model: &ModelChoice) -> Result<(), String> {
        if self.sessions.iter().any(|s| s.model == *model) {
            return Err("the model is in use; stop dictating first".into());
        }
        Ok(())
    }

    fn action(&mut self, op: &VoiceOp) -> Result<()> {
        match op {
            VoiceOp::Download { id, quant } => {
                if self
                    .download
                    .as_ref()
                    .is_some_and(|(_, d)| d.id == *id && d.quant == *quant)
                {
                    return Ok(());
                }
                let file = CATALOG
                    .model(id)
                    .and_then(|m| m.file(quant))
                    .context("no such catalog file")?;
                let mut files = Vec::new();
                if self.vad_path().is_none() {
                    files.extend(
                        CATALOG
                            .vad
                            .files
                            .first()
                            .map(|f| Fetch::new(f, &self.dirs.models)),
                    );
                }
                files.push(Fetch::new(file, &self.dirs.models));
                self.download_gen += 1;
                let (tx, gen) = (self.tx.clone(), self.download_gen);
                let dl = Download::start(id.clone(), quant.clone(), files, move |r| {
                    let _ = tx.send(Ev::Download(gen, r));
                });
                self.download = Some((gen, dl));
                self.download_error = None;
            }
            VoiceOp::DownloadWake => {
                if self
                    .download
                    .as_ref()
                    .is_some_and(|(_, d)| d.id == CATALOG.kws.id)
                {
                    return Ok(());
                }
                let k = &CATALOG.kws;
                let into = self.dirs.models.join("kws");
                let name = k.url.rsplit('/').next().unwrap_or("kws.tar.bz2");
                let fetch = Fetch {
                    url: k.url.clone(),
                    size: k.size,
                    sha256: k.sha256.clone(),
                    dest: into.join(name),
                };
                let archive = fetch.dest.clone();
                self.download_gen += 1;
                let (tx, gen) = (self.tx.clone(), self.download_gen);
                let dl = Download::start(k.id.clone(), String::new(), vec![fetch], move |r| {
                    let r = match r {
                        Ok(true) => download::extract_kws(&archive, &into)
                            .map(|()| true)
                            .map_err(|e| format!("{e:#}")),
                        other => other,
                    };
                    let _ = tx.send(Ev::Download(gen, r));
                });
                self.download = Some((gen, dl));
                self.download_error = None;
            }
            VoiceOp::CancelDownload => {
                if let Some((_, dl)) = self.download.take() {
                    dl.cancel();
                }
            }
            VoiceOp::DeleteModel { id, quant } => {
                let choice = ModelChoice::Catalog {
                    id: id.clone(),
                    quant: quant.clone(),
                };
                self.delete_model(&choice)?;
            }
            VoiceOp::DeleteCustom { file } => {
                self.delete_model(&ModelChoice::Custom { file: file.clone() })?;
            }
            VoiceOp::ClearHistory => {
                self.history.clear()?;
                self.entries.clear();
            }
            _ => {}
        }
        Ok(())
    }

    /// Unload it if it is loaded (and idle: checked before), then delete it.
    fn delete_model(&mut self, model: &ModelChoice) -> Result<()> {
        if self.loaded.as_ref().map(|(m, _)| m) == Some(model)
            || self.loading.as_ref() == Some(model)
        {
            self.unload();
        }
        let name = model_file_name(model).context("no such model")?;
        let path = self.dirs.models.join(&name);
        for p in [download::part_path(&path), path] {
            match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("delete {}", p.display()));
                }
                _ => {}
            }
        }
        info!("voice: deleted {name}");
        Ok(())
    }

    fn settings_changed(&mut self, file: VoiceFile, now: Instant) {
        let old = std::mem::replace(&mut self.file, file);
        let new = self.file.clone();
        if !new.enabled {
            if let Some(i) = self.recording() {
                self.end_recording(i);
                self.sessions.remove(i);
            }
            while let Some(s) = self.sessions.pop_front() {
                if let Some(w) = &mut self.worker {
                    w.send(ToWorker::Msg(WorkerIn::Cancel { session: s.id }));
                }
            }
            self.close_capture();
            self.unload();
        } else if self.sessions.is_empty() {
            // A model no profile uses any more, or another device: unloaded.
            let keep = |m: &ModelChoice| {
                new.model.as_ref() == Some(m) || new.alt.model.as_ref() == Some(m)
            };
            if self.loaded.as_ref().is_some_and(|(m, _)| !keep(m))
                || new.advanced.accelerator != old.advanced.accelerator
                || new.advanced.vad != old.advanced.vad
            {
                self.unload();
            }
        }
        if new.enabled
            && new.advanced.preload
            && (!old.enabled || !old.advanced.preload || new.model != old.model)
        {
            self.preload = true;
        }
        if new.enabled && new.output.layout.is_none() && (self.layout.is_none() || !old.enabled) {
            self.layout = Some(self.backend.detect_layout());
        }
        if new.history.keep != old.history.keep {
            match self.history.trim(new.history.keep) {
                Ok(entries) => self.entries = entries,
                Err(e) => warn!("voice history: {e:#}"),
            }
        }
        if new.mic != old.mic && self.capture.is_some() && self.recording().is_none() {
            self.close_capture();
        }
        self.error = None;
        self.output_error = None;
        self.rescan();
        self.pump(now);
    }

    // -- state ---------------------------------------------------------------

    fn rescan(&mut self) {
        let models = &self.dirs.models;
        let mut installed = Vec::new();
        let mut known = BTreeSet::new();
        for m in &CATALOG.models {
            for f in &m.files {
                known.insert(f.file.as_str());
                let downloading = self
                    .download
                    .as_ref()
                    .filter(|(_, d)| d.id == m.id && d.quant == f.quant);
                let status = if let Some((_, d)) = downloading {
                    ModelStatus::Downloading {
                        bytes: d.progress(),
                        total: f.size,
                    }
                } else {
                    match std::fs::metadata(models.join(&f.file)) {
                        Ok(meta) if meta.len() == f.size => ModelStatus::Ready,
                        Ok(meta) => ModelStatus::Corrupt {
                            reason: format!("{} of {} bytes", meta.len(), f.size),
                        },
                        Err(_) => continue,
                    }
                };
                installed.push(InstalledModel {
                    id: m.id.clone(),
                    quant: f.quant.clone(),
                    file: f.file.clone(),
                    size: f.size,
                    status,
                });
            }
        }
        for f in &CATALOG.vad.files {
            known.insert(f.file.as_str());
        }
        let mut custom: Vec<CustomModel> = std::fs::read_dir(models)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().to_str()?.to_string();
                let meta = e.metadata().ok()?;
                (valid_custom_file(&file) && meta.is_file() && !known.contains(file.as_str()))
                    .then_some(CustomModel {
                        file,
                        size: meta.len(),
                    })
            })
            .collect();
        custom.sort_by(|a, b| a.file.cmp(&b.file));
        let enabled = self.file.enabled;
        let kws = self.kws_dir();
        let k = &CATALOG.kws;
        let kws_files = [&k.encoder, &k.decoder, &k.joiner, &k.tokens, &k.pieces];
        self.scan = Scan {
            kws_ready: kws_files.iter().all(|f| kws.join(f).is_file()),
            kws_partial: kws.is_dir(),
            installed,
            custom,
            vad_ready: self.vad_path().is_some(),
            outputs: self.backend.checks(&self.file.output),
            sources: if enabled {
                self.backend.sources()
            } else {
                Vec::new()
            },
            engine_missing: self.backend.spawner().available().err(),
        };
        self.scanned = Some(Instant::now());
    }

    fn status(&self) -> EngineStatus {
        if !self.file.enabled || self.load_error.is_some() {
            return EngineStatus::Off;
        }
        if let Some(e) = &self.scan.engine_missing {
            return EngineStatus::Error { message: e.clone() };
        }
        if self.recording().is_none()
            && self.sessions.is_empty()
            && !self
                .file
                .model
                .as_ref()
                .is_some_and(|m| self.model_path(m).is_some())
        {
            return EngineStatus::NoModel;
        }
        if self.loading.is_some() && self.sessions.iter().any(|s| !s.begun) {
            EngineStatus::Loading
        } else if self.recording().is_some() {
            EngineStatus::Recording
        } else if !self.sessions.is_empty() {
            EngineStatus::Transcribing
        } else if self.loading.is_some() {
            EngineStatus::Loading
        } else if let Some(e) = &self.error {
            EngineStatus::Error { message: e.clone() }
        } else {
            EngineStatus::Idle
        }
    }

    fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if !self.file.enabled {
            return p;
        }
        if let Some(mic) = &self.file.mic {
            if !self.scan.sources.iter().any(|(n, _)| n == mic) {
                p.push(format!(
                    "Microphone {mic} is not connected (the default source is used)"
                ));
            }
        }
        if let Some(c) = self
            .scan
            .outputs
            .iter()
            .find(|c| c.method == self.file.output.method && !c.ok)
        {
            p.push(format!(
                "{}: {}",
                c.method.label(),
                c.reason.as_deref().unwrap_or("not available")
            ));
        }
        p.extend(self.output_error.iter().map(|e| format!("Output: {e}")));
        if self.file.jarvis.enabled {
            p.extend(self.wake.error.iter().map(|e| format!("Jarvis: {e}")));
        }
        p.extend(self.download_error.iter().cloned());
        p
    }

    /// Publish the state; subscribers hear about changes only.
    fn publish(&mut self) {
        let usable = self.usable();
        // Push-to-command needs no wake word, so no always-on microphone.
        let hotkeys = (usable, usable);
        if self.hotkeys_on != Some(hotkeys) {
            self.backend.hotkeys_active(hotkeys.0, hotkeys.1);
            self.hotkeys_on = Some(hotkeys);
        }
        let state = VoiceState {
            file: self.file.clone(),
            engine: self.status(),
            loaded: self.loaded.as_ref().map(|(_, c)| c.clone()),
            installed: self.scan.installed.clone(),
            custom: self.scan.custom.clone(),
            vad_ready: self.scan.vad_ready,
            history: self.entries.clone(),
            outputs: self.scan.outputs.clone(),
            accelerators: self.accelerators.clone(),
            layout: self
                .file
                .output
                .layout
                .clone()
                .or_else(|| self.layout.clone()),
            sources: self.scan.sources.clone(),
            wake_model: self.wake_status(),
            jarvis_listening: self.wake.listening == Some(true),
            last_command: self.wake.last.clone(),
            problems: self.problems(),
            load_error: self.load_error.clone(),
        };
        *lock(&self.state) = state.clone();
        if self.sent.as_ref() != Some(&state) {
            let _ = self
                .events
                .send(AdapterEvent::VoiceUpdated(Box::new(state.clone())));
            self.sent = Some(state);
        }
    }

    /// Daemon stop (or a crash): nothing records, nothing runs, the sink
    /// is unmuted. Downloads keep their `.part`.
    fn stop_all(&mut self) {
        if let Some(i) = self.recording() {
            self.end_recording(i);
        }
        self.sessions.clear();
        self.close_capture();
        self.unload();
        self.unmute();
        self.download = None;
        self.wake.ear.cancel();
        self.close_wake_capture();
        self.stop_wake_worker();
        self.tell_listening(false);
    }

    // -- Jarvis --------------------------------------------------------------

    fn kws_dir(&self) -> PathBuf {
        self.dirs.models.join("kws").join(&CATALOG.kws.dir)
    }

    fn wake_status(&self) -> Option<ModelStatus> {
        if let Some((_, d)) = self
            .download
            .as_ref()
            .filter(|(_, d)| d.id == CATALOG.kws.id)
        {
            // Downloaded whole: being extracted.
            let bytes = if d.main.dest.is_file() {
                CATALOG.kws.size
            } else {
                d.progress()
            };
            return Some(ModelStatus::Downloading {
                bytes,
                total: CATALOG.kws.size,
            });
        }
        if self.scan.kws_ready {
            Some(ModelStatus::Ready)
        } else if self.scan.kws_partial {
            Some(ModelStatus::Corrupt {
                reason: "files are missing; download it again".into(),
            })
        } else {
            None
        }
    }

    /// Jarvis may take commands (Voice usable and Jarvis on).
    fn jarvis_on(&self) -> bool {
        self.usable() && self.file.jarvis.enabled
    }

    /// Open or close the ear (capture, wake-word worker) to match the
    /// settings; tell the runner whether it listens.
    fn wake_sync(&mut self, now: Instant) {
        let wake = self.jarvis_on() && self.scan.kws_ready;
        if wake || (self.usable() && self.wake.ear.taking().is_some()) {
            let stale = self.wake.capture.is_none() || self.wake.capture_mic != self.file.mic;
            if stale && self.wake.capture_retry.is_none_or(|at| now >= at) {
                match self.open_wake_capture() {
                    Ok(()) => self.wake.capture_retry = None,
                    Err(e) => {
                        warn!("jarvis: microphone: {e}");
                        self.wake.error = Some(format!("the microphone could not be opened: {e}"));
                        self.wake.capture_retry = Some(now + CAPTURE_RETRY);
                    }
                }
            }
            if self.wake.capture.is_none() {
                self.wake.ear.cancel();
            }
        } else {
            self.wake.ear.cancel();
            self.close_wake_capture();
        }
        let listens_for = (
            self.file.jarvis.wake_word.trim().to_lowercase(),
            self.file.jarvis.sensitivity,
        );
        if self.wake.worker.is_some()
            && (!wake || self.wake.listens_for.as_ref() != Some(&listens_for))
        {
            self.stop_wake_worker();
        }
        if wake && self.wake.worker.is_none() && self.wake.restart_at.is_none() {
            self.start_wake_worker(listens_for, now);
        }
        let listening =
            self.wake.worker.is_some() && self.wake.capture.is_some() && self.recording().is_none();
        self.tell_listening(listening);
    }

    fn tell_listening(&mut self, on: bool) {
        if self.wake.listening != Some(on) {
            if on {
                info!("jarvis: listening for \"{}\"", self.file.jarvis.wake_word);
            }
            self.wake.listening = Some(on);
            self.backend.jarvis(RunnerIn::Listening(on));
        }
    }

    fn open_wake_capture(&mut self) -> Result<(), String> {
        self.close_wake_capture();
        self.wake.capture_gen += 1;
        let (tx, gen) = (self.tx.clone(), self.wake.capture_gen);
        let capture = self.backend.open_capture(
            self.file.mic.as_deref(),
            Box::new(move |pcm| {
                let _ = tx.send(Ev::JarvisPcm(gen, pcm.to_vec()));
            }),
        )?;
        self.wake.capture = Some(capture);
        self.wake.capture_mic = self.file.mic.clone();
        Ok(())
    }

    fn close_wake_capture(&mut self) {
        if self.wake.capture.take().is_some() {
            self.wake.capture_gen += 1;
        }
    }

    fn start_wake_worker(&mut self, (word, sensitivity): (String, f32), now: Instant) {
        self.wake.worker_gen += 1;
        let (tx, gen) = (self.tx.clone(), self.wake.worker_gen);
        match self.backend.spawner().spawn(Box::new(move |m| {
            let _ = tx.send(Ev::JarvisWorker(gen, m));
        })) {
            Ok(mut w) => {
                let (threshold, boost) = wake_params(sensitivity);
                w.send(ToWorker::Msg(WorkerIn::Wake {
                    session: gen,
                    model_dir: self.kws_dir().to_string_lossy().into_owned(),
                    keywords: vec![word.clone()],
                    threshold,
                    boost,
                }));
                self.wake.worker = Some(w);
                self.wake.listens_for = Some((word, sensitivity));
                self.wake.spawned_at = now;
                self.wake.error = None;
            }
            Err(e) => {
                warn!("jarvis: {e}");
                self.wake.error = Some(e);
                self.wake.restart_at = Some(restart_at(
                    &mut self.wake.crashes,
                    self.wake.spawned_at,
                    now,
                ));
            }
        }
    }

    fn stop_wake_worker(&mut self) {
        if self.wake.worker.take().is_some() {
            self.wake.worker_gen += 1;
        }
        self.wake.listens_for = None;
    }

    fn wake_worker_says(&mut self, msg: Option<WorkerOut>, now: Instant) {
        match msg {
            Some(WorkerOut::Wake { .. }) => self.wake_heard(now),
            Some(WorkerOut::Speech { active, .. }) => {
                if let Some(done) = self.wake.ear.speech(active, now) {
                    self.take_done(done, now);
                }
            }
            Some(WorkerOut::Error { message, .. }) => {
                // Its listening session ended: started again after a wait.
                warn!("jarvis: wake-word listener: {message}");
                self.stop_wake_worker();
                self.wake.error = Some(message);
                self.wake.restart_at = Some(restart_at(
                    &mut self.wake.crashes,
                    self.wake.spawned_at,
                    now,
                ));
            }
            Some(_) => {}
            None => {
                warn!("jarvis: the wake-word listener stopped unexpectedly");
                self.stop_wake_worker();
                self.wake.error = Some("the wake-word listener stopped unexpectedly".into());
                self.wake.restart_at = Some(restart_at(
                    &mut self.wake.crashes,
                    self.wake.spawned_at,
                    now,
                ));
            }
        }
    }

    /// The spotter heard the wake word: the take starts (with the audio
    /// before it) unless something else has the mic.
    fn wake_heard(&mut self, now: Instant) {
        let why_not = if self.recording().is_some() {
            Some("dictation is recording")
        } else if self.wake.ear.taking().is_some() {
            Some("a command is being heard")
        } else if self.backend.mic_muted() {
            Some("the microphone is muted")
        } else if self.wake.listening != Some(true) {
            Some("not listening")
        } else {
            None
        };
        if let Some(why) = why_not {
            debug!("jarvis: wake word ignored: {why}");
            return;
        }
        info!("jarvis: wake word heard");
        self.wake.ear.begin(Listen::Wake, now);
        self.backend.play(Sound::Start, &self.file.sounds);
    }

    /// The push-to-command hotkey.
    fn push_to_command(&mut self, edge: HotkeyEdge, now: Instant) {
        let taking = self.wake.ear.taking() == Some(Listen::Push);
        match edge {
            HotkeyEdge::Start | HotkeyEdge::Fire if !taking => {
                if !self.usable() || self.recording().is_some() {
                    return;
                }
                self.wake.ear.cancel();
                self.wake.ear.begin(Listen::Push, now);
                self.wake_sync(now);
                if self.wake.capture.is_none() {
                    self.backend.play(Sound::Error, &self.file.sounds);
                    return;
                }
                self.backend.play(Sound::Start, &self.file.sounds);
            }
            HotkeyEdge::Start | HotkeyEdge::Fire | HotkeyEdge::Stop => {
                if let Some(done) = self.wake.ear.release() {
                    self.take_done(done, now);
                }
            }
            HotkeyEdge::Discard => {
                if taking {
                    self.wake.ear.cancel();
                    self.backend.play(Sound::Cancel, &self.file.sounds);
                }
            }
        }
    }

    /// A take is complete: transcribed by the dictation model (in order
    /// behind any dictation), then the runner gets the text.
    fn take_done(&mut self, (kind, pcm): (Listen, Vec<f32>), now: Instant) {
        let Some(model) = self.file.model.clone().filter(|_| self.usable()) else {
            return;
        };
        let id = self.next_session;
        self.next_session += 1;
        debug!(
            "jarvis: take {id} ({} ms)",
            pcm.len() as u64 * 1000 / u64::from(SAMPLE_RATE)
        );
        let layout = self.layout.clone().unwrap_or_default();
        self.sessions.push_back(Session {
            id,
            model,
            cfg: effective(&self.file, false),
            layout,
            recording: false,
            started: now,
            stopped: true,
            begun: false,
            live: false,
            ended: None,
            bleed: 0,
            pcm,
            sent: 0,
            typed: String::new(),
            clip: Some(kind),
            by_voice: false,
        });
        self.pump(now);
    }

    /// What the runner asks for.
    fn jarvis_says(&mut self, out: JarvisOut, now: Instant) {
        let ctx = || Ctx {
            method: self.file.output.method,
            out: self.file.output.clone(),
            layout: self
                .file
                .output
                .layout
                .clone()
                .or_else(|| self.layout.clone())
                .unwrap_or_else(|| "us".into()),
        };
        match out {
            JarvisOut::Done(outcome) => {
                let sound = if outcome.ok {
                    Sound::Stop
                } else {
                    Sound::Error
                };
                self.backend.play(sound, &self.file.sounds);
                self.wake.last = Some(outcome);
            }
            JarvisOut::Confirm => {
                if self.usable() && self.wake.ear.taking().is_none() {
                    self.wake.ear.begin(Listen::Confirm, now);
                    self.wake_sync(now);
                    self.backend.play(Sound::Start, &self.file.sounds);
                }
            }
            JarvisOut::Type(text) => {
                let job = Job::Text(ctx(), text);
                self.backend.output(job);
            }
            JarvisOut::Keys(keys) => {
                let job = Job::Keys(ctx(), keys);
                self.backend.output(job);
            }
            JarvisOut::Dictate => match self.start(false, now) {
                Ok(()) => {
                    if let Some(i) = self.recording() {
                        self.sessions[i].by_voice = true;
                    }
                }
                Err(e) => info!("voice: {e}"),
            },
            JarvisOut::Ping => self.backend.play(Sound::Start, &self.file.sounds),
        }
    }
}

fn is_setting(op: &VoiceOp) -> bool {
    !matches!(
        op,
        VoiceOp::Download { .. }
            | VoiceOp::CancelDownload
            | VoiceOp::DeleteModel { .. }
            | VoiceOp::DeleteCustom { .. }
            | VoiceOp::ClearHistory
            | VoiceOp::DownloadWake
    )
}

#[cfg(test)]
mod tests;
