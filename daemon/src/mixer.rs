//! The mixer service: `mixer.toml` in memory plus an actor thread that owns
//! the PipeWire graph (`hako_audio::mixer::MixerGraph`), keeps it healed and
//! publishes its status. `GetMixer` and meters read what was published;
//! only `PatchMixer` waits, for its own change.
//!
//! While the mixer is on it owns ChatMix (`AudioService::hand_over`): the
//! legacy loopbacks stand down and the ChatMix dial drives the inputs on its
//! sides. Off, they come back. Line-in is only ever a mixer input: a line-in
//! that was on before the mixer moves into it once (`migrates`).
//!
//! Profiles carry the layout (`set_live`); scenes saved before that become
//! profiles at start (`scenes_to_profiles`).

use crate::drivers::EventTx;
use anyhow::Result;
use hako_audio::mixer::{
    apps, devices, starter_scene, Detected, MixerGraph, Naming, Side, StripKey,
};
use hako_audio::{AudioService, CHATMIX_GAME};
use hako_core::catalog::SettingKey;
use hako_core::config::{
    get_setting, list_profiles, load_app_config, load_device_settings, save_profile,
    validate_profile_name, ConfigPaths, ProfileFile,
};
use hako_core::mixer::{
    load_mixer, normalize_mixer_file, update_mixer, InputKind, MixerFile, MixerOp, MixerState,
    MixerStatus, OutputKind, Scene, Source, MAX_MIXER_OPS,
};
use hako_core::protocol::AdapterEvent;
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How often the graph is healed (crashed strips, devices that came or went).
const HEAL: Duration = Duration::from_millis(500);
/// Fader drags (~30 Hz) write `mixer.toml` at most this often.
const WRITE_DELAY: Duration = Duration::from_millis(500);
/// Longest a request waits for the graph (a `PatchMixer`, teardown).
const APPLY_WAIT: Duration = Duration::from_secs(15);

enum Cmd {
    Apply(MixerFile, Option<mpsc::Sender<()>>),
    Dial(f32, f32),
    Latency(u32),
    /// Kill-switch: tear down and stay down until the next `Apply`.
    Reset(mpsc::Sender<()>),
    Shutdown(mpsc::Sender<()>),
}

/// `mixer.toml` as the daemon has it, ahead of the disk while a fader
/// drag's write is pending (`dirty`: since when).
#[derive(Default)]
struct FileCache {
    file: Option<MixerFile>,
    dirty: Option<Instant>,
}

struct Shared {
    paths: ConfigPaths,
    cache: Mutex<FileCache>,
    status: Mutex<MixerStatus>,
    /// Post-fader node of each running strip, for meters.
    meters: Mutex<HashMap<StripKey, String>>,
    events: EventTx,
    /// The last `MixerUpdated` sent: events go out on change only.
    sent: Mutex<Option<MixerState>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    /// The current file: the cache while a write is pending, else the disk.
    fn file(&self) -> Result<MixerFile> {
        let mut cache = lock(&self.cache);
        if let (Some(_), Some(file)) = (cache.dirty, &cache.file) {
            return Ok(file.clone());
        }
        let file = load_mixer(&self.paths)?;
        cache.file = Some(file.clone());
        Ok(file)
    }

    /// Write a pending fader change once it is `WRITE_DELAY` old (`now`: at once).
    fn flush(&self, now: bool) {
        let mut cache = lock(&self.cache);
        let (Some(since), Some(file)) = (cache.dirty, cache.file.clone()) else {
            return;
        };
        if !now && since.elapsed() < WRITE_DELAY {
            return;
        }
        cache.dirty = None;
        if let Err(e) = update_mixer(&self.paths, |f| {
            *f = file;
            Ok(())
        }) {
            warn!("mixer.toml: {e:#}");
            let _ = self.events.send(AdapterEvent::Error {
                device: None,
                message: format!("Mixer: saving mixer.toml failed: {e:#}"),
            });
        }
    }
}

/// Why a `PatchMixer` failed: the request (`invalid_value`) or the file (`io`).
#[derive(Debug)]
pub enum PatchError {
    Invalid(String),
    Io(anyhow::Error),
}

pub struct MixerService {
    tx: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    audio: Arc<AudioService>,
}

impl MixerService {
    /// Load `mixer.toml` and bring its graph up on the actor thread.
    /// `latency`: `node.latency` in samples at 48 kHz.
    pub fn start(
        paths: ConfigPaths,
        audio: Arc<AudioService>,
        events: EventTx,
        latency: u32,
    ) -> Self {
        let shared = Arc::new(Shared {
            paths,
            cache: Mutex::default(),
            status: Mutex::default(),
            meters: Mutex::default(),
            events,
            sent: Mutex::default(),
        });
        let (tx, rx) = mpsc::channel();
        let actor = Actor {
            graph: MixerGraph::new(Naming::daemon(), latency),
            file: MixerFile::default(),
            dial: audio.chatmix_dial(),
            handed_over: false,
            suppressed: false,
            game: None,
            audio: audio.clone(),
            shared: shared.clone(),
        };
        if let Err(e) = std::thread::Builder::new()
            .name("hako-mixer".into())
            .spawn(move || actor.run(rx))
        {
            warn!("spawn mixer thread: {e}");
        }
        match shared.file() {
            Ok(file) => {
                // `file` is normalised already: the disk follows, once.
                if let Err(e) = normalize_mixer_file(&shared.paths) {
                    warn!("mixer.toml: {e:#}");
                }
                let file = migrate(&shared.paths, &audio, file);
                let file = scenes_to_profiles(&shared.paths, file);
                let _ = tx.send(Cmd::Apply(file, None));
            }
            Err(e) => {
                warn!("mixer.toml: {e:#}");
                let _ = shared.events.send(AdapterEvent::Error {
                    device: None,
                    message: format!("Mixer: {e:#}"),
                });
            }
        }
        Self { tx, shared, audio }
    }

    /// For `AudioService::on_dial`: the ChatMix dial, forwarded to the actor.
    pub fn dial_hook(&self) -> impl Fn(f32, f32) + Send + Sync + 'static {
        let tx = self.tx.clone();
        move |game, chat| {
            let _ = tx.send(Cmd::Dial(game, chat));
        }
    }

    pub fn set_latency(&self, samples: u32) {
        let _ = self.tx.send(Cmd::Latency(samples));
    }

    /// The file and the last published status (no waiting on PipeWire).
    pub fn state(&self) -> Result<MixerState> {
        Ok(MixerState {
            file: self.shared.file()?,
            status: lock(&self.shared.status).clone(),
        })
    }

    /// The post-fader node of a running strip.
    pub fn meter_node(&self, side: Side, id: &str) -> Option<String> {
        lock(&self.shared.meters)
            .get(&(side, id.to_string()))
            .cloned()
    }

    /// Apply `ops` in order, all or nothing, then wait for the graph to
    /// follow. Fader-only patches (drags) write `mixer.toml` later.
    /// Blocking.
    pub fn patch(&self, ops: &[MixerOp]) -> Result<MixerState, PatchError> {
        if ops.len() > MAX_MIXER_OPS {
            return Err(PatchError::Invalid(format!(
                "at most {MAX_MIXER_OPS} ops per patch"
            )));
        }
        self.edit(ops.iter().all(is_fader), |file| {
            ops.iter().try_for_each(|op| file.apply(op))
        })
    }

    /// Make `live` (a profile's layout) the live one, all or nothing, and
    /// wait for the graph to follow. On / off stays as it is: an off mixer
    /// keeps the layout for when it is turned on. Blocking.
    pub fn set_live(&self, mut live: Scene) -> Result<MixerState, PatchError> {
        live.normalize();
        live.validate().map_err(PatchError::Invalid)?;
        self.edit(false, |file| {
            file.live = live;
            Ok(())
        })
    }

    /// Change the file (`later`: written by the next flush, a fader drag),
    /// then wait for the graph to follow.
    fn edit(
        &self,
        later: bool,
        change: impl FnOnce(&mut MixerFile) -> Result<(), String>,
    ) -> Result<MixerState, PatchError> {
        let (reply, done) = mpsc::channel();
        let file = {
            let mut cache = lock(&self.shared.cache);
            let mut next = match (cache.dirty, &cache.file) {
                (Some(_), Some(file)) => file.clone(),
                _ => load_mixer(&self.shared.paths).map_err(PatchError::Io)?,
            };
            let was_on = next.enabled;
            change(&mut next).map_err(PatchError::Invalid)?;
            if next.enabled && !was_on && next.live == Scene::default() {
                let line_in = legacy_line_in(&self.shared.paths);
                if let Some(live) = starter(&self.shared.paths, &self.audio, line_in) {
                    next.live = live;
                }
            }
            if later {
                cache.dirty.get_or_insert_with(Instant::now);
            } else {
                next = update_mixer(&self.shared.paths, |f| {
                    *f = next.clone();
                    Ok(())
                })
                .map_err(PatchError::Io)?
                .0;
                cache.dirty = None;
            }
            cache.file = Some(next.clone());
            // Sent under the lock: the actor sees edits in file order.
            let _ = self.tx.send(Cmd::Apply(next.clone(), Some(reply)));
            next
        };
        let _ = done.recv_timeout(APPLY_WAIT);
        Ok(MixerState {
            file,
            status: lock(&self.shared.status).clone(),
        })
    }

    /// Kill-switch: the graph goes and stays down until the next patch.
    /// Blocking.
    pub fn reset(&self) {
        let (reply, done) = mpsc::channel();
        if self.tx.send(Cmd::Reset(reply)).is_ok() {
            let _ = done.recv_timeout(APPLY_WAIT);
        }
    }

    /// Daemon stop: save a pending fader change, tear the graph down (ChatMix
    /// is not brought back). Blocking.
    pub fn shutdown(&self) {
        let (reply, done) = mpsc::channel();
        if self.tx.send(Cmd::Shutdown(reply)).is_ok() {
            let _ = done.recv_timeout(APPLY_WAIT);
        }
        self.shared.flush(true);
    }
}

/// The first layout, from the devices, ChatMix's sink and the old line-in
/// (`line_in`: it was on, its capture level).
fn starter(
    paths: &ConfigPaths,
    audio: &AudioService,
    line_in: (bool, Option<u8>),
) -> Option<Scene> {
    let (devices, default_sink) = devices(&[]).unwrap_or_default();
    let aux = load_app_config(paths).map(|c| c.aux).unwrap_or_default();
    let headset = audio.chatmix_sink();
    let live = starter_scene(&Detected {
        devices: &devices,
        headset_sink: headset.as_deref(),
        default_sink: default_sink.as_deref(),
        aux: &aux,
        aux_enabled: line_in.0,
        capture_percent: Some(line_in.1.unwrap_or(aux.capture_percent)),
    });
    match live.validate() {
        Ok(()) => Some(live),
        Err(e) => {
            warn!("mixer starter layout: {e}");
            None
        }
    }
}

/// The headset's pre-mixer line-in, from its device file: whether it was on
/// (`aux_enabled`) and its capture level.
fn legacy_line_in(paths: &ConfigPaths) -> (bool, Option<u8>) {
    let files = std::fs::read_dir(&paths.devices_dir)
        .into_iter()
        .flatten()
        .flatten();
    for file in files {
        let name = file.file_name();
        let Some((vid, pid)) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".toml"))
            .and_then(|stem| stem.split_once('_'))
            .and_then(|(v, p)| {
                Some((
                    u16::from_str_radix(v, 16).ok()?,
                    u16::from_str_radix(p, 16).ok()?,
                ))
            })
        else {
            continue;
        };
        let Ok(settings) = load_device_settings(paths, vid, pid) else {
            continue;
        };
        if let Some(on) = get_setting(&settings, SettingKey::AuxEnabled).and_then(|v| v.as_bool()) {
            let percent = get_setting(&settings, SettingKey::AuxCapturePercent)
                .and_then(|v| v.as_int())
                .map(|p| p.clamp(0, 100) as u8);
            return (on, percent);
        }
    }
    (false, None)
}

/// A line-in that was on moves into the mixer, once: only while the mixer
/// was never set up (off and empty). Off: nothing changes.
fn migrates(line_in_on: bool, file: &MixerFile) -> bool {
    line_in_on && !file.line_in_moved && !file.enabled && file.live == Scene::default()
}

/// A layout made before sources had capture settings: give the line-in's
/// source (same device as the starter's) the old jack and capture level,
/// where none is set, in the live layout and every saved scene.
fn fill_line_in_capture(file: &mut MixerFile, line_in: &Source) -> bool {
    let mut changed = false;
    let scenes =
        std::iter::once(&mut file.live).chain(file.scenes.iter_mut().map(|s| &mut s.scene));
    for scene in scenes {
        for input in &mut scene.inputs {
            let InputKind::Hardware { sources } = &mut input.kind else {
                continue;
            };
            for s in sources {
                if s.node == line_in.node && s.alsa_input.is_none() && s.capture_percent.is_none() {
                    s.alsa_input = line_in.alsa_input.clone();
                    s.capture_percent = line_in.capture_percent;
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Turn the mixer on with the starter layout when the line-in was on before
/// it (see `migrates`); `file` as it is otherwise.
fn migrate(paths: &ConfigPaths, audio: &AudioService, file: MixerFile) -> MixerFile {
    let line_in = legacy_line_in(paths);
    if !line_in.0 || file.line_in_moved {
        return file;
    }
    let fresh = migrates(line_in.0, &file);
    // ponytail: no line-in device yet (PipeWire still starting) retries at
    // the next daemon start; watch for it if that turns out to happen.
    let Some((live, source)) = starter(paths, audio, line_in).and_then(|scene| {
        let source = scene
            .inputs
            .iter()
            .find(|i| i.id == "line-in")
            .and_then(|i| match &i.kind {
                InputKind::Hardware { sources } => sources.first().cloned(),
                InputKind::Virtual { .. } => None,
            })?;
        Some((scene, source))
    }) else {
        warn!("mixer: line-in was on, but its device is not there; not moved into the mixer yet");
        return file;
    };
    match update_mixer(paths, |f| {
        if fresh {
            f.enabled = true;
            f.live = live;
        } else {
            fill_line_in_capture(f, &source);
        }
        f.line_in_moved = true;
        Ok(())
    }) {
        Ok((file, ())) => {
            if fresh {
                info!("mixer: line-in was on: turned the mixer on with the starter layout (line-in moved into it)");
            } else {
                info!("mixer: line-in was on: its jack and capture level moved onto the mixer's line-in source");
            }
            file
        }
        Err(e) => {
            warn!("mixer: moving line-in into the mixer: {e:#}");
            file
        }
    }
}

/// Why a saved scene cannot become a profile (`None`: it can).
fn scene_stays(name: &str, profiles: &[String]) -> Option<&'static str> {
    if validate_profile_name(name).is_err() {
        Some("its name is not a profile name")
    } else if profiles.iter().any(|p| p == name) {
        Some("a profile of that name exists")
    } else {
        None
    }
}

/// Scenes saved before profiles carried the mixer layout become profiles
/// (the layout only, no device settings) and leave `mixer.toml`; the ones
/// that cannot (`scene_stays`) are kept.
fn scenes_to_profiles(paths: &ConfigPaths, file: MixerFile) -> MixerFile {
    if file.scenes.is_empty() {
        return file;
    }
    let profiles = match list_profiles(paths) {
        Ok(profiles) => profiles,
        Err(e) => {
            warn!("mixer: saved scenes not moved into profiles: {e:#}");
            return file;
        }
    };
    let mut moved = Vec::new();
    for s in &file.scenes {
        let saved = match scene_stays(&s.name, &profiles) {
            Some(why) => Err(anyhow::anyhow!(why)),
            None => save_profile(
                paths,
                &ProfileFile {
                    version: 1,
                    name: s.name.clone(),
                    devices: Default::default(),
                    mixer: Some(s.scene.clone()),
                },
            ),
        };
        match saved {
            Ok(()) => moved.push(s.name.clone()),
            Err(e) => warn!("mixer: scene {:?} stays in mixer.toml: {e:#}", s.name),
        }
    }
    if moved.is_empty() {
        return file;
    }
    // ponytail: a failed write here leaves the scenes next to their new
    // profiles ("a profile of that name exists" at every start); compare
    // layouts and drop the copies if that ever shows up.
    match update_mixer(paths, |f| {
        f.scenes.retain(|s| !moved.contains(&s.name));
        Ok(())
    }) {
        Ok((file, ())) => {
            info!("mixer: saved scenes are profiles now: {}", moved.join(", "));
            file
        }
        Err(e) => {
            warn!("mixer: removing the scenes moved into profiles: {e:#}");
            file
        }
    }
}

fn is_fader(op: &MixerOp) -> bool {
    matches!(
        op,
        MixerOp::SetInputGain { .. } | MixerOp::SetOutputGain { .. }
    )
}

/// Owns the graph; every PipeWire change happens on its thread.
struct Actor {
    graph: MixerGraph,
    file: MixerFile,
    dial: (f32, f32),
    /// ChatMix is the mixer's (`AudioService::hand_over`).
    handed_over: bool,
    /// Torn down by the kill-switch.
    suppressed: bool,
    /// Last told `AudioService::mixer_default` (a `Hako_Game` input is up).
    game: Option<bool>,
    audio: Arc<AudioService>,
    shared: Arc<Shared>,
}

impl Actor {
    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(HEAL) {
                Ok(Cmd::Apply(file, reply)) => {
                    self.file = file;
                    self.suppressed = false;
                    self.sync();
                    self.publish();
                    if let Some(reply) = reply {
                        let _ = reply.send(());
                    }
                }
                Ok(Cmd::Dial(game, chat)) => {
                    self.dial = (game, chat);
                    self.sync();
                }
                Ok(Cmd::Latency(samples)) => {
                    self.graph.set_latency(samples);
                    self.sync();
                }
                Ok(Cmd::Reset(reply)) => {
                    self.graph.teardown();
                    self.suppressed = true;
                    // The kill-switch drops the default-sink claim too.
                    self.game = None;
                    self.publish();
                    let _ = reply.send(());
                }
                Ok(Cmd::Shutdown(reply)) => {
                    self.graph.teardown();
                    let _ = reply.send(());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => self.sync(),
                Err(RecvTimeoutError::Disconnected) => return self.graph.teardown(),
            }
            self.shared.flush(false);
            self.publish();
        }
    }

    /// Build / heal the graph while on; tear it down and hand ChatMix back
    /// while off.
    fn sync(&mut self) {
        if self.suppressed {
            return;
        }
        if !self.file.enabled {
            if !self.graph.is_empty() || self.graph.running() {
                self.graph.teardown();
            }
            if self.handed_over {
                self.handed_over = false;
                self.game = None;
                if let Err(e) = self.audio.hand_over(false) {
                    warn!("mixer off: {e:#}");
                }
            }
            return;
        }
        if !self.handed_over {
            self.handed_over = true;
            if let Err(e) = self.audio.hand_over(true) {
                warn!("mixer on: {e:#}");
            }
        }
        let live = &self.file.live;
        self.graph.reconcile(live, self.dial);
        let game = live.inputs.iter().any(|i| {
            matches!(&i.kind, InputKind::Virtual { node_name, .. } if node_name == CHATMIX_GAME)
                && self
                    .graph
                    .meter_node(&(Side::Input, i.id.clone()))
                    .is_some()
        });
        if self.game != Some(game) {
            self.game = Some(game);
            if let Err(e) = self.audio.mixer_default(game) {
                warn!("mixer default sink: {e:#}");
            }
        }
    }

    /// Publish status and meter nodes; send `MixerUpdated` if anything changed.
    fn publish(&self) {
        let live = &self.file.live;
        let mut created: Vec<&str> = self.graph.node_names();
        created.extend(live.inputs.iter().filter_map(|i| match &i.kind {
            InputKind::Virtual { node_name, .. } => Some(node_name.as_str()),
            InputKind::Hardware { .. } => None,
        }));
        created.extend(live.outputs.iter().filter_map(|o| match &o.kind {
            OutputKind::Virtual { node_name } => Some(node_name.as_str()),
            OutputKind::Hardware { .. } => None,
        }));
        let status = MixerStatus {
            running: self.graph.running(),
            problems: self.graph.problems().to_vec(),
            devices: devices(&created)
                .map(|(list, _)| list.into_iter().map(|d| d.device).collect())
                .unwrap_or_default(),
            apps: apps().unwrap_or_default(),
        };
        *lock(&self.shared.meters) = live
            .inputs
            .iter()
            .map(|i| (Side::Input, i.id.clone()))
            .chain(live.outputs.iter().map(|o| (Side::Output, o.id.clone())))
            .filter_map(|key| Some((key.clone(), self.graph.meter_node(&key)?)))
            .collect();
        *lock(&self.shared.status) = status.clone();
        let state = MixerState {
            file: self.file.clone(),
            status,
        };
        let mut sent = lock(&self.shared.sent);
        if sent.as_ref() != Some(&state) {
            *sent = Some(state.clone());
            let _ = self
                .shared
                .events
                .send(AdapterEvent::MixerUpdated(Box::new(state)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::mixer::{mixer_path, Output, OutputKind};
    use tokio::sync::broadcast;

    fn paths(name: &str) -> ConfigPaths {
        let root = std::env::temp_dir().join(format!("hako-mixer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        };
        paths.ensure().unwrap();
        paths
    }

    /// A mixer that stays off: its actor never touches PipeWire's graph.
    fn service(paths: &ConfigPaths) -> (MixerService, broadcast::Receiver<AdapterEvent>) {
        let (events, rx) = broadcast::channel(64);
        let mixer = MixerService::start(paths.clone(), Arc::new(AudioService::new()), events, 256);
        (mixer, rx)
    }

    fn b1() -> MixerOp {
        MixerOp::PutOutput {
            output: Output {
                id: "B1".into(),
                label: "Test".into(),
                kind: OutputKind::Virtual {
                    node_name: "hako-test-never-built".into(),
                },
                chain: Default::default(),
                gain_db: 0.0,
                mute: false,
                mono: false,
            },
        }
    }

    #[test]
    fn patches_are_all_or_nothing_and_fader_drags_write_later() {
        let paths = paths("patch");
        let (mixer, mut events) = service(&paths);
        let on_disk = || load_mixer(&paths).unwrap();

        let ops = vec![b1(); MAX_MIXER_OPS + 1];
        assert!(matches!(mixer.patch(&ops), Err(PatchError::Invalid(_))));
        // The second op fails: the first is not applied either.
        let bad = [
            b1(),
            MixerOp::SetOutputGain {
                output: "B7".into(),
                gain_db: 0.0,
            },
        ];
        assert!(matches!(mixer.patch(&bad), Err(PatchError::Invalid(_))));
        assert!(!mixer_path(&paths).exists());

        // Structural: written at once.
        let state = mixer.patch(&[b1()]).unwrap();
        assert_eq!(state.file.live.outputs.len(), 1);
        assert_eq!(on_disk().live.outputs.len(), 1);
        assert!(!state.status.running, "off: nothing built");

        // A drag: the reply and GetMixer have it at once, the disk later.
        let drag = |db| MixerOp::SetOutputGain {
            output: "B1".into(),
            gain_db: db,
        };
        for db in [-1.0, -2.0, -3.0] {
            mixer.patch(&[drag(db)]).unwrap();
        }
        assert_eq!(mixer.state().unwrap().file.live.outputs[0].gain_db, -3.0);
        assert_eq!(on_disk().live.outputs[0].gain_db, 0.0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while on_disk().live.outputs[0].gain_db != -3.0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(on_disk().live.outputs[0].gain_db, -3.0);

        // A structural patch during a drag keeps the drag's value.
        mixer.patch(&[drag(-4.0)]).unwrap();
        mixer
            .patch(&[MixerOp::SetOutputMute {
                output: "B1".into(),
                mute: true,
            }])
            .unwrap();
        let file = on_disk();
        assert_eq!(file.live.outputs[0].gain_db, -4.0);
        assert!(file.live.outputs[0].mute);

        // Subscribers heard about it.
        let mut updates = 0;
        while let Ok(ev) = events.try_recv() {
            if let AdapterEvent::MixerUpdated(state) = ev {
                updates += 1;
                assert!(!state.file.enabled);
            }
        }
        assert!(updates >= 2, "{updates} MixerUpdated events");
        mixer.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn line_in_that_was_on_moves_into_a_mixer_never_set_up() {
        let mut file = MixerFile::default();
        assert!(migrates(true, &file), "no mixer.toml yet");
        assert!(!migrates(false, &file), "line-in was off: nothing changes");
        file.enabled = true;
        assert!(!migrates(true, &file), "on, even if empty: set up");
        file.enabled = false;
        if let MixerOp::PutOutput { output } = b1() {
            file.live.outputs.push(output);
        }
        assert!(!migrates(true, &file), "a layout exists (mixer turned off)");
        let moved = MixerFile {
            line_in_moved: true,
            ..MixerFile::default()
        };
        assert!(!migrates(true, &moved), "moved once already");

        // The headset's device file says whether line-in was on.
        let paths = paths("legacy");
        assert_eq!(legacy_line_in(&paths), (false, None));
        let save = |on: bool| {
            let settings = [
                (SettingKey::AuxEnabled, hako_core::SettingValue::Bool(on)),
                (
                    SettingKey::AuxCapturePercent,
                    hako_core::SettingValue::Int(140),
                ),
            ]
            .map(|(k, v)| (k.as_str().to_string(), v))
            .into();
            hako_core::config::save_device_settings(&paths, 0x1038, 0x12e5, &settings).unwrap();
        };
        std::fs::write(paths.devices_dir.join("notes.txt"), "x").unwrap();
        save(true);
        assert_eq!(legacy_line_in(&paths), (true, Some(100)));
        save(false);
        assert_eq!(legacy_line_in(&paths), (false, Some(100)));
        // Off: the mixer file is left as it is (nothing written).
        let (events, _) = broadcast::channel(8);
        let mixer = MixerService::start(paths.clone(), Arc::new(AudioService::new()), events, 256);
        assert!(!mixer_path(&paths).exists());
        mixer.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn an_existing_layout_gets_the_old_line_in_jack_once() {
        let source = |node: &str, input: Option<&str>| Source {
            node: node.into(),
            description: String::new(),
            trim_db: 0.0,
            mute: false,
            alsa_input: input.map(Into::into),
            capture_percent: input.map(|_| 47),
        };
        let line_in = source("alsa_input.pci-line", Some("Line"));
        let strip = |id: &str, sources: Vec<Source>| hako_core::mixer::Input {
            id: id.into(),
            label: id.into(),
            kind: InputKind::Hardware { sources },
            chain: Default::default(),
            gain_db: 0.0,
            mute: false,
            solo: false,
            routes: Vec::new(),
            chatmix: None,
        };
        let mut file = MixerFile::default();
        file.live.inputs = vec![
            strip("line-in", vec![source("alsa_input.pci-line", None)]),
            strip("mic", vec![source("alsa_input.usb-mic", None)]),
        ];
        file.scenes.push(hako_core::mixer::NamedScene {
            name: "Saved".into(),
            scene: file.live.clone(),
        });
        assert!(fill_line_in_capture(&mut file, &line_in));
        let first = |scene: &Scene, i: usize| match &scene.inputs[i].kind {
            InputKind::Hardware { sources } => sources[0].clone(),
            InputKind::Virtual { .. } => unreachable!(),
        };
        for scene in [&file.live, &file.scenes[0].scene] {
            assert_eq!(first(scene, 0).alsa_input.as_deref(), Some("Line"));
            assert_eq!(first(scene, 0).capture_percent, Some(47));
            assert_eq!(first(scene, 1).alsa_input, None, "other devices untouched");
        }
        // A choice the user made later ("Leave as is" or another jack) stays.
        if let InputKind::Hardware { sources } = &mut file.live.inputs[0].kind {
            sources[0].alsa_input = Some("Rear Mic".into());
        }
        assert!(
            !fill_line_in_capture(&mut file, &line_in),
            "nothing left to fill"
        );
        assert_eq!(first(&file.live, 0).alsa_input.as_deref(), Some("Rear Mic"));
    }

    #[test]
    fn a_profile_layout_replaces_the_live_one_only() {
        let paths = paths("set-live");
        let (mixer, _events) = service(&paths);
        mixer.patch(&[b1()]).unwrap();
        // A drag whose write is still pending.
        mixer
            .patch(&[MixerOp::SetOutputGain {
                output: "B1".into(),
                gain_db: -9.0,
            }])
            .unwrap();
        let MixerOp::PutOutput { output: mut b2 } = b1() else {
            unreachable!()
        };
        b2.id = "B2".into();
        let layout = Scene {
            inputs: Vec::new(),
            outputs: vec![b2],
        };

        let mut bad = layout.clone();
        bad.outputs[0].id = "X9".into();
        assert!(matches!(mixer.set_live(bad), Err(PatchError::Invalid(_))));
        assert_eq!(mixer.state().unwrap().file.live.outputs[0].gain_db, -9.0);

        let state = mixer.set_live(layout.clone()).unwrap();
        assert_eq!(state.file.live, layout);
        assert!(!state.file.enabled && !state.status.running, "still off");
        assert_eq!(load_mixer(&paths).unwrap().live, layout, "written at once");
        // The drag's late write does not bring the old layout back.
        mixer.shutdown();
        assert_eq!(load_mixer(&paths).unwrap().live, layout);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn saved_scenes_become_profiles_once() {
        assert_eq!(scene_stays("Streaming", &["Night".into()]), None);
        assert!(scene_stays("Night", &["Night".into()]).is_some());
        assert!(scene_stays("Late: night", &[]).is_some());

        let paths = paths("scenes");
        let MixerOp::PutOutput { output } = b1() else {
            unreachable!()
        };
        let scene = Scene {
            inputs: Vec::new(),
            outputs: vec![output],
        };
        let taken = ProfileFile {
            version: 1,
            name: "Night".into(),
            devices: Default::default(),
            mixer: None,
        };
        save_profile(&paths, &taken).unwrap();
        update_mixer(&paths, |f| {
            f.scenes = ["Streaming", "Night", "Late: night"]
                .map(|name| hako_core::mixer::NamedScene {
                    name: name.into(),
                    scene: scene.clone(),
                })
                .into();
            Ok(())
        })
        .unwrap();

        let names = |file: &MixerFile| -> Vec<String> {
            file.scenes.iter().map(|s| s.name.clone()).collect()
        };
        let file = scenes_to_profiles(&paths, load_mixer(&paths).unwrap());
        assert_eq!(names(&file), ["Night", "Late: night"]);
        assert_eq!(names(&load_mixer(&paths).unwrap()), names(&file));
        let load = |name| hako_core::config::load_profile(&paths, name).unwrap();
        let profile = load("Streaming");
        assert_eq!(profile.mixer, Some(scene));
        assert!(profile.devices.is_empty());
        assert_eq!(load("Night").mixer, None, "an existing profile is kept");
        assert_eq!(list_profiles(&paths).unwrap(), ["Night", "Streaming"]);
        // Nothing left to move: the file is not touched again.
        let again = scenes_to_profiles(&paths, file.clone());
        assert_eq!(again, file);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn an_old_single_source_trim_is_folded_on_disk_at_start() {
        let paths = paths("normalise");
        std::fs::write(
            mixer_path(&paths),
            "version = 1\n[[live.inputs]]\nid = \"l\"\nlabel = \"L\"\n[live.inputs.kind]\ntype = \"hardware\"\n[[live.inputs.kind.sources]]\nnode = \"alsa_input.x\"\ntrim_db = 4.0\n",
        )
        .unwrap();
        let (mixer, _events) = service(&paths);
        let text = std::fs::read_to_string(mixer_path(&paths)).unwrap();
        assert!(
            text.contains("gain_db = 4.0") && text.contains("trim_db = 0.0"),
            "{text}"
        );
        mixer.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn a_pending_drag_is_saved_at_shutdown() {
        let paths = paths("shutdown");
        let (mixer, _events) = service(&paths);
        mixer.patch(&[b1()]).unwrap();
        mixer
            .patch(&[MixerOp::SetOutputGain {
                output: "B1".into(),
                gain_db: -9.0,
            }])
            .unwrap();
        mixer.shutdown();
        assert_eq!(load_mixer(&paths).unwrap().live.outputs[0].gain_db, -9.0);
        let _ = std::fs::remove_dir_all(&paths.root);
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_replaced() {
        let paths = paths("broken");
        std::fs::write(mixer_path(&paths), "version = \"x\"").unwrap();
        let (mixer, _events) = service(&paths);
        assert!(mixer.state().is_err());
        assert!(matches!(mixer.patch(&[b1()]), Err(PatchError::Io(_))));
        assert_eq!(
            std::fs::read_to_string(mixer_path(&paths)).unwrap(),
            "version = \"x\""
        );
        mixer.shutdown();
        let _ = std::fs::remove_dir_all(&paths.root);
    }
}
