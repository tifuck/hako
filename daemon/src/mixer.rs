//! The mixer service: `mixer.toml` in memory plus an actor thread that owns
//! the PipeWire graph (`hako_audio::mixer::MixerGraph`), keeps it healed and
//! publishes its status. `GetMixer` and meters read what was published;
//! only `PatchMixer` waits, for its own change.
//!
//! While the mixer is on it owns ChatMix and the line-in
//! (`AudioService::hand_over`): the legacy loopbacks and AUX stand down and
//! the ChatMix dial drives the inputs on its sides. Off, they come back.

use crate::drivers::EventTx;
use anyhow::Result;
use hako_audio::mixer::{devices, starter_scene, Detected, MixerGraph, Naming, Side, StripKey};
use hako_audio::{AudioService, CHATMIX_GAME};
use hako_core::config::{load_app_config, ConfigPaths};
use hako_core::mixer::{
    load_mixer, update_mixer, InputKind, MixerFile, MixerOp, MixerState, MixerStatus, OutputKind,
    Scene, MAX_MIXER_OPS,
};
use hako_core::protocol::AdapterEvent;
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tracing::warn;

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
        let (reply, done) = mpsc::channel();
        let file = {
            let mut cache = lock(&self.shared.cache);
            let mut next = match (cache.dirty, &cache.file) {
                (Some(_), Some(file)) => file.clone(),
                _ => load_mixer(&self.shared.paths).map_err(PatchError::Io)?,
            };
            let was_on = next.enabled;
            for op in ops {
                next.apply(op).map_err(PatchError::Invalid)?;
            }
            if next.enabled && !was_on && next.live == Scene::default() {
                self.add_starter(&mut next);
            }
            if ops.iter().all(is_fader) {
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
            // Sent under the lock: the actor sees patches in file order.
            let _ = self.tx.send(Cmd::Apply(next.clone(), Some(reply)));
            next
        };
        let _ = done.recv_timeout(APPLY_WAIT);
        Ok(MixerState {
            file,
            status: lock(&self.shared.status).clone(),
        })
    }

    /// The first layout, from the devices and today's ChatMix / line-in.
    fn add_starter(&self, file: &mut MixerFile) {
        let (devices, default_sink) = devices(&[]).unwrap_or_default();
        let aux = load_app_config(&self.shared.paths)
            .map(|c| c.aux)
            .unwrap_or_default();
        let headset = self.audio.chatmix_sink();
        let live = starter_scene(&Detected {
            devices: &devices,
            headset_sink: headset.as_deref(),
            default_sink: default_sink.as_deref(),
            aux: &aux,
            aux_enabled: self.audio.aux_active(),
        });
        match live.validate() {
            Ok(()) => file.live = live,
            Err(e) => warn!("mixer starter layout: {e}"),
        }
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
    /// and the line-in are not brought back). Blocking.
    pub fn shutdown(&self) {
        let (reply, done) = mpsc::channel();
        if self.tx.send(Cmd::Shutdown(reply)).is_ok() {
            let _ = done.recv_timeout(APPLY_WAIT);
        }
        self.shared.flush(true);
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
    /// ChatMix and the line-in are the mixer's (`AudioService::hand_over`).
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

    /// Build / heal the graph while on; tear it down and hand ChatMix and
    /// the line-in back while off.
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
            matches!(&i.kind, InputKind::Virtual { node_name } if node_name == CHATMIX_GAME)
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
            InputKind::Virtual { node_name } => Some(node_name.as_str()),
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
