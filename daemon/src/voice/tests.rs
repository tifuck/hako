//! The state machine with a fake backend: no PipeWire, no worker process,
//! no uinput; time is passed in.

use super::*;
use hako_core::voice::worker::WorkerOut;
use tokio::sync::broadcast;

#[derive(Default)]
struct Log {
    to_worker: Vec<ToWorker>,
    spawned: usize,
    killed: usize,
    played: Vec<Sound>,
    jobs: Vec<Job>,
    captures: usize,
    open: usize,
    mute: Vec<bool>,
    active: Vec<bool>,
    jarvis_active: Vec<bool>,
    recording: Vec<bool>,
    runner: Vec<RunnerIn>,
    mic_muted: bool,
}

type Shared = Arc<Mutex<Log>>;

struct FakeWorker(Shared);

impl Worker for FakeWorker {
    fn send(&mut self, msg: ToWorker) {
        lock(&self.0).to_worker.push(msg);
    }
}

impl Drop for FakeWorker {
    fn drop(&mut self) {
        lock(&self.0).killed += 1;
    }
}

struct FakeSpawner {
    log: Shared,
    missing: Option<String>,
}

impl Spawner for FakeSpawner {
    fn available(&self) -> Result<(), String> {
        self.missing.clone().map_or(Ok(()), Err)
    }

    fn spawn(&mut self, _sink: engine::WorkerSink) -> Result<Box<dyn Worker>, String> {
        self.available()?;
        lock(&self.log).spawned += 1;
        Ok(Box::new(FakeWorker(self.log.clone())))
    }
}

struct FakeCapture(Shared);

impl Capture for FakeCapture {
    fn failed(&self) -> bool {
        false
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        lock(&self.0).open -= 1;
    }
}

struct Fake {
    log: Shared,
    spawner: FakeSpawner,
}

impl Backend for Fake {
    fn spawner(&mut self) -> &mut dyn Spawner {
        &mut self.spawner
    }
    fn open_capture(
        &mut self,
        _mic: Option<&str>,
        _sink: PcmSink,
    ) -> Result<Box<dyn Capture>, String> {
        let mut log = lock(&self.log);
        log.captures += 1;
        log.open += 1;
        Ok(Box::new(FakeCapture(self.log.clone())))
    }
    fn play(&mut self, sound: Sound, _settings: &SoundSettings) {
        lock(&self.log).played.push(sound);
    }
    fn cancel_start_ping(&mut self) {}
    fn output(&mut self, job: Job) {
        if job != Job::Warm {
            lock(&self.log).jobs.push(job);
        }
    }
    fn mute(&mut self, on: bool) {
        lock(&self.log).mute.push(on);
    }
    fn hotkeys_active(&mut self, on: bool, jarvis: bool) {
        let mut log = lock(&self.log);
        log.active.push(on);
        log.jarvis_active.push(jarvis);
    }
    fn jarvis(&mut self, msg: RunnerIn) {
        lock(&self.log).runner.push(msg);
    }
    fn mic_muted(&mut self) -> bool {
        lock(&self.log).mic_muted
    }
    fn recording(&mut self, on: bool) {
        lock(&self.log).recording.push(on);
    }
    fn detect_layout(&mut self) -> String {
        "us".into()
    }
    fn sources(&mut self) -> Vec<(String, String)> {
        vec![("mic".into(), "Mic".into())]
    }
    fn checks(&mut self, _out: &OutputSettings) -> Vec<OutputCheck> {
        Vec::new()
    }
}

struct T {
    m: Machine,
    log: Shared,
    t0: Instant,
    events: broadcast::Receiver<AdapterEvent>,
    _rx: mpsc::Receiver<Ev>,
}

const MODEL: &str = "test.gguf";

fn setup(name: &str, edit: impl FnOnce(&mut VoiceFile), missing: Option<&str>) -> T {
    let root = std::env::temp_dir().join(format!("hako-voice-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = ConfigPaths {
        config_toml: root.join("config.toml"),
        devices_dir: root.join("devices"),
        profiles_dir: root.join("profiles"),
        remaps_dir: root.join("remaps"),
        root: root.clone(),
    };
    paths.ensure().unwrap();
    let dirs = Dirs {
        models: root.join("models"),
        history: root.join("voice"),
        runtime: root.join("run"),
    };
    std::fs::create_dir_all(&dirs.models).unwrap();
    std::fs::write(dirs.models.join(MODEL), b"gguf").unwrap();
    update_voice(&paths, |f| {
        f.enabled = true;
        f.model = Some(ModelChoice::Custom { file: MODEL.into() });
        edit(f);
        Ok(())
    })
    .unwrap();
    let log = Shared::default();
    let backend = Fake {
        log: log.clone(),
        spawner: FakeSpawner {
            log: log.clone(),
            missing: missing.map(str::to_string),
        },
    };
    let (events, rx_events) = broadcast::channel(256);
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(Mutex::new(placeholder_state()));
    let mut m = Machine::new(paths, dirs, events, Box::new(backend), tx, state);
    let t0 = Instant::now();
    m.startup(t0);
    T {
        m,
        log,
        t0,
        events: rx_events,
        _rx: rx,
    }
}

fn caps(streaming: bool) -> ModelCaps {
    ModelCaps {
        streaming,
        ..Default::default()
    }
}

impl T {
    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }
    fn trigger(&mut self, action: HotkeyAction, edge: HotkeyEdge, ms: u64) {
        let now = self.at(ms);
        self.m.handle(Ev::Trigger(action, edge), now);
    }
    fn start(&mut self, ms: u64) {
        self.trigger(HotkeyAction::VoiceTranscribe, HotkeyEdge::Start, ms);
    }
    fn stop(&mut self, ms: u64) {
        self.trigger(HotkeyAction::VoiceTranscribe, HotkeyEdge::Stop, ms);
    }
    fn worker(&mut self, out: WorkerOut, ms: u64) {
        let (gen, now) = (self.m.worker_gen, self.at(ms));
        self.m.handle(Ev::Worker(gen, Some(out)), now);
    }
    fn pcm(&mut self, n: usize, ms: u64) {
        let (gen, now) = (self.m.capture_gen, self.at(ms));
        self.m.handle(Ev::Pcm(gen, vec![0.25; n]), now);
    }
    fn take_worker(&self) -> Vec<ToWorker> {
        std::mem::take(&mut lock(&self.log).to_worker)
    }
    fn texts(&self) -> Vec<String> {
        lock(&self.log)
            .jobs
            .iter()
            .map(|j| match j {
                Job::Text(_, t) => t.clone(),
                Job::Erase(_, n) => format!("<{n}"),
                Job::Submit(..) => "<submit>".into(),
                Job::Copy(t) => format!("copy:{t}"),
                Job::Warm => "warm".into(),
                Job::Keys(_, k) => format!("keys:{k:?}"),
            })
            .collect()
    }
    fn engine(&self) -> EngineStatus {
        self.m.status()
    }
    fn partials(&mut self) -> Vec<VoicePartial> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            if let AdapterEvent::VoicePartial(p) = ev {
                out.push(p);
            }
        }
        out
    }
}

fn pcm_len(msgs: &[ToWorker]) -> Vec<usize> {
    msgs.iter()
        .filter_map(|m| match m {
            ToWorker::Pcm(p) => Some(p.len()),
            _ => None,
        })
        .collect()
}

fn msgs(msgs: &[ToWorker]) -> Vec<WorkerIn> {
    msgs.iter()
        .filter_map(|m| match m {
            ToWorker::Msg(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn batch_session_loads_lazily_buffers_and_outputs_segments_then_the_rest() {
    let mut t = setup("batch", |_| {}, None);
    assert_eq!(t.engine(), EngineStatus::Idle);
    assert_eq!(lock(&t.log).active, [true]);
    assert_eq!(lock(&t.log).spawned, 0, "loaded lazily");

    t.start(0);
    {
        let log = lock(&t.log);
        assert_eq!(log.played, [Sound::Start]);
        assert_eq!((log.open, log.spawned), (1, 1));
        assert_eq!(log.recording, [true]);
    }
    assert_eq!(t.engine(), EngineStatus::Loading);
    let sent = msgs(&t.take_worker());
    assert!(
        matches!(&sent[..], [WorkerIn::Load { path, cpu_only: false, .. }] if path.ends_with(MODEL))
    );

    // 150 ms of ping bleed is dropped; the rest waits for the model.
    t.pcm(4000, 250);
    assert!(t.take_worker().is_empty());
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 300);
    assert_eq!(t.engine(), EngineStatus::Recording);
    let sent = t.take_worker();
    assert!(matches!(
        msgs(&sent)[..],
        [WorkerIn::Begin {
            session: 1,
            streaming: false,
            pause_ms: Some(500),
            ..
        }]
    ));
    assert_eq!(pcm_len(&sent), [CHUNK]);

    t.stop(1000);
    {
        let log = lock(&t.log);
        assert_eq!(log.played, [Sound::Start, Sound::Stop]);
        assert_eq!(log.open, 0, "capture closed on stop");
        assert_eq!(log.recording, [true, false]);
    }
    let sent = t.take_worker();
    assert_eq!(pcm_len(&sent), [4000 - 2400 - CHUNK]);
    assert_eq!(msgs(&sent), [WorkerIn::End { session: 1 }]);
    assert_eq!(t.engine(), EngineStatus::Transcribing);

    t.worker(
        WorkerOut::Segment {
            session: 1,
            text: "hello world".into(),
        },
        1100,
    );
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "Hello world. Um, done".into(),
            language: Some("en".into()),
        },
        1200,
    );
    assert_eq!(t.texts(), ["hello world", " done"]);
    assert_eq!(t.engine(), EngineStatus::Idle);
    assert_eq!(t.m.entries.len(), 1);
    assert_eq!(t.m.entries[0].text, "Hello world. done");
    assert_eq!(t.m.history.load(), t.m.entries);
}

#[test]
fn streaming_types_settled_words_live_and_the_rest_at_the_end() {
    let mut t = setup(
        "stream",
        |f| {
            f.output.trailing_space = true;
            f.output.auto_submit = Some(hako_core::voice::SubmitKey::Enter);
        },
        None,
    );
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(true) }, 10);
    assert!(matches!(
        msgs(&t.take_worker())[..],
        [
            _,
            WorkerIn::Begin {
                streaming: true,
                pause_ms: None,
                ..
            }
        ]
    ));
    let partial = |committed: &str| WorkerOut::Partial {
        session: 1,
        committed: committed.into(),
        tentative: "x".into(),
    };
    t.worker(partial("hello wor"), 100);
    t.worker(partial("hello world how"), 200);
    assert_eq!(t.texts(), ["hello", " world"]);
    let p = t.partials();
    assert_eq!(p.len(), 2);
    assert_eq!(
        (p[1].committed.as_str(), p[1].tentative.as_str()),
        ("hello world how", "x")
    );
    t.stop(300);
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "Hello, world, how are you?".into(),
            language: None,
        },
        400,
    );
    assert_eq!(
        t.texts(),
        ["hello", " world", " how are you?", " ", "<submit>"]
    );
}

#[test]
fn streaming_fix_up_rewrites_only_this_utterance() {
    let mut t = setup("fixup", |f| f.output.fix_up_at_end = true, None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(true) }, 10);
    t.worker(
        WorkerOut::Partial {
            session: 1,
            committed: "and so my ".into(),
            tentative: String::new(),
        },
        100,
    );
    t.stop(200);
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "and so, my friends".into(),
            language: None,
        },
        300,
    );
    // "and so my" -> "and so, my friends": 3 back, then ", my friends".
    assert_eq!(t.texts(), ["and so my", "<3", ", my friends"]);
}

#[test]
fn non_live_methods_output_per_segment_or_at_the_end() {
    let mut t = setup("paste", |f| f.output.method = OutputMethod::Paste, None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(true) }, 10);
    t.worker(
        WorkerOut::Partial {
            session: 1,
            committed: "hello there ".into(),
            tentative: String::new(),
        },
        100,
    );
    assert!(t.texts().is_empty(), "paste never outputs live partials");
    t.stop(200);
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "Hello there.".into(),
            language: None,
        },
        300,
    );
    assert_eq!(t.texts(), ["Hello there."]);
    let job = lock(&t.log).jobs[0].clone();
    assert!(matches!(
        job,
        Job::Text(
            Ctx {
                method: OutputMethod::Paste,
                ..
            },
            _
        )
    ));

    let mut t = setup(
        "clipboard",
        |f| f.output.method = OutputMethod::Clipboard,
        None,
    );
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    t.worker(
        WorkerOut::Segment {
            session: 1,
            text: "one".into(),
        },
        20,
    );
    t.stop(30);
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "one two".into(),
            language: None,
        },
        40,
    );
    assert_eq!(t.texts(), ["copy:one two"]);
}

#[test]
fn cancel_outputs_nothing_and_tells_the_worker() {
    let mut t = setup("cancel", |_| {}, None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    t.take_worker();
    t.trigger(HotkeyAction::VoiceCancel, HotkeyEdge::Fire, 500);
    assert_eq!(msgs(&t.take_worker()), [WorkerIn::Cancel { session: 1 }]);
    {
        let log = lock(&t.log);
        assert_eq!(log.played, [Sound::Start, Sound::Cancel]);
        assert_eq!(log.open, 0);
        assert_eq!(log.recording, [true, false]);
    }
    // A late answer for the cancelled session is ignored.
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "nope".into(),
            language: None,
        },
        600,
    );
    assert!(t.texts().is_empty());
    assert_eq!(t.engine(), EngineStatus::Idle);

    // A discarded hold is a cancel too.
    t.start(1000);
    t.trigger(HotkeyAction::VoiceTranscribe, HotkeyEdge::Discard, 1050);
    assert!(t.m.sessions.is_empty());
}

#[test]
fn the_length_cap_stops_with_the_stop_ping() {
    let mut t = setup("cap", |f| f.advanced.max_seconds = 5, None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    t.take_worker();
    assert!(t.m.tick(t.at(1000)) <= t.at(5000));
    t.m.tick(t.at(4999));
    assert_eq!(lock(&t.log).played, [Sound::Start]);
    t.m.tick(t.at(5000));
    assert_eq!(lock(&t.log).played, [Sound::Start, Sound::Stop]);
    assert_eq!(msgs(&t.take_worker()), [WorkerIn::End { session: 1 }]);
}

#[test]
fn a_new_recording_starts_while_the_last_transcribes_and_outputs_stay_in_order() {
    let mut t = setup("overlap", |_| {}, None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    t.stop(500);
    t.start(600);
    assert_eq!(t.engine(), EngineStatus::Recording);
    let sent = msgs(&t.take_worker());
    assert!(matches!(
        &sent[..],
        [
            WorkerIn::Load { .. },
            WorkerIn::Begin { session: 1, .. },
            WorkerIn::End { session: 1 },
            WorkerIn::Begin { session: 2, .. },
        ]
    ));
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "first".into(),
            language: None,
        },
        700,
    );
    t.stop(800);
    t.worker(
        WorkerOut::Final {
            session: 2,
            text: "second".into(),
            language: None,
        },
        900,
    );
    assert_eq!(t.texts(), ["first", "second"]);
    assert_eq!(lock(&t.log).spawned, 1);
}

#[test]
fn a_crashed_worker_fails_its_session_and_restarts_with_backoff() {
    let mut t = setup("crash", |_| {}, None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    let gen = t.m.worker_gen;
    t.m.handle(Ev::Worker(gen, None), t.at(100));
    assert!(t.m.sessions.is_empty());
    assert_eq!(lock(&t.log).played, [Sound::Start, Sound::Error]);
    assert!(matches!(t.engine(), EngineStatus::Error { .. }));
    // A trigger during the backoff records; the worker comes back after it.
    t.start(200);
    assert_eq!(lock(&t.log).spawned, 1);
    assert_eq!(t.engine(), EngineStatus::Recording);
    t.m.tick(t.at(1100));
    assert_eq!(lock(&t.log).spawned, 2);
    assert_eq!(t.engine(), EngineStatus::Loading);
}

#[test]
fn missing_engine_is_an_error_and_never_arms_hotkeys() {
    let mut t = setup("missing", |_| {}, Some("hako-stt is not installed"));
    assert_eq!(
        t.engine(),
        EngineStatus::Error {
            message: "hako-stt is not installed".into()
        }
    );
    assert_eq!(lock(&t.log).active, [false]);
    t.start(0);
    assert_eq!(lock(&t.log).played, [Sound::Error]);
    assert_eq!(lock(&t.log).open, 0);
}

#[test]
fn idle_unload_and_preload() {
    let mut t = setup("unload", |f| f.advanced.unload_after_s = Some(0), None);
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    t.stop(20);
    t.worker(
        WorkerOut::Final {
            session: 1,
            text: "x".into(),
            language: None,
        },
        30,
    );
    t.m.tick(t.at(30));
    assert_eq!(lock(&t.log).killed, 1);
    assert!(t.m.loaded.is_none());

    let t = setup("preload", |f| f.advanced.preload = true, None);
    assert_eq!(lock(&t.log).spawned, 1);
    assert!(matches!(
        msgs(&t.take_worker())[..],
        [WorkerIn::Load { .. }]
    ));
}

#[test]
fn mute_follows_the_recording_after_the_ping() {
    let mut t = setup("mute", |f| f.advanced.mute_while_recording = true, None);
    t.start(0);
    assert!(lock(&t.log).mute.is_empty(), "the start ping plays first");
    t.m.tick(t.at(150));
    assert_eq!(lock(&t.log).mute, [true]);
    t.stop(500);
    assert_eq!(lock(&t.log).mute, [true, false]);
}

#[test]
fn alt_profile_overrides_the_model_and_method() {
    let mut t = setup(
        "alt",
        |f| {
            f.alt.method = Some(OutputMethod::Paste);
            f.alt.language = Some("de".into());
        },
        None,
    );
    t.trigger(HotkeyAction::VoiceTranscribeAlt, HotkeyEdge::Start, 0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    assert!(matches!(
        &msgs(&t.take_worker())[..],
        [_, WorkerIn::Begin { language: Some(l), .. }] if l == "de"
    ));
    assert_eq!(t.m.sessions[0].cfg.output.method, OutputMethod::Paste);
}

#[test]
fn patch_saves_settings_then_runs_actions() {
    let mut t = setup("patch", |_| {}, None);
    // Invalid: nothing is saved.
    let bad = t.m.patch(
        &[
            VoiceOp::SetLanguage {
                language: Some("en".into()),
            },
            VoiceOp::Download {
                id: "nope".into(),
                quant: "Q8_0".into(),
            },
        ],
        t.at(0),
    );
    assert!(matches!(bad, Err(PatchError::Invalid(_))));
    assert_eq!(load_voice(&t.m.paths).unwrap().language, None);

    t.m.patch(
        &[VoiceOp::SetHistory {
            history: hako_core::voice::HistorySettings {
                keep: 0,
                keep_audio: false,
            },
        }],
        t.at(0),
    )
    .unwrap();
    assert_eq!(load_voice(&t.m.paths).unwrap().history.keep, 0);

    // The loaded custom model: unloaded, then deleted.
    t.start(0);
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 10);
    let in_use =
        t.m.patch(&[VoiceOp::DeleteCustom { file: MODEL.into() }], t.at(20));
    assert!(matches!(in_use, Err(PatchError::Invalid(_))));
    t.trigger(HotkeyAction::VoiceCancel, HotkeyEdge::Fire, 30);
    t.m.patch(&[VoiceOp::DeleteCustom { file: MODEL.into() }], t.at(40))
        .unwrap();
    assert!(t.m.loaded.is_none());
    assert!(!t.m.dirs.models.join(MODEL).exists());
    t.m.rescan();
    assert_eq!(t.engine(), EngineStatus::NoModel);
    t.m.publish();
    assert_eq!(lock(&t.log).active.last(), Some(&false));

    // Off: the worker goes.
    t.m.patch(&[VoiceOp::SetEnabled { on: false }], t.at(50))
        .unwrap();
    assert_eq!(t.engine(), EngineStatus::Off);
}

#[test]
fn custom_and_catalog_files_are_listed() {
    let mut t = setup("scan", |_| {}, None);
    let m = &CATALOG.models[0];
    let f = &m.files[0];
    std::fs::write(t.m.dirs.models.join(&f.file), b"short").unwrap();
    t.m.rescan();
    assert_eq!(
        t.m.scan.custom,
        [CustomModel {
            file: MODEL.into(),
            size: 4
        }]
    );
    assert!(matches!(
        &t.m.scan.installed[..],
        [InstalledModel {
            status: ModelStatus::Corrupt { .. },
            ..
        }]
    ));
    assert_eq!(t.m.scan.sources, [("mic".to_string(), "Mic".to_string())]);
}

#[test]
fn hand_edits_are_followed_and_a_broken_file_turns_voice_off() {
    let mut t = setup("reload", |_| {}, None);
    let path = hako_core::voice::voice_path(&t.m.paths);
    let good = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, "version = \"x\"").unwrap();
    t.m.tick(t.at(2500));
    assert_eq!(t.engine(), EngineStatus::Off);
    assert!(t.m.load_error.is_some());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version = \"x\"");
    std::fs::write(
        &path,
        good.replace("enabled = true", "enabled = true\nlanguage = \"fr\""),
    )
    .unwrap();
    t.m.tick(t.at(5000));
    assert_eq!(t.engine(), EngineStatus::Idle);
    assert_eq!(t.m.file.language.as_deref(), Some("fr"));
}

// ---------------------------------------------------------------------------
// Jarvis.

/// Voice usable, Jarvis on, the wake-word model on disk.
fn jarvis(name: &str, edit: impl FnOnce(&mut VoiceFile)) -> T {
    let mut t = setup(
        name,
        |f| {
            f.jarvis.enabled = true;
            edit(f);
        },
        None,
    );
    let k = &CATALOG.kws;
    let dir = t.m.kws_dir();
    std::fs::create_dir_all(&dir).unwrap();
    for f in [&k.encoder, &k.decoder, &k.joiner, &k.tokens, &k.pieces] {
        std::fs::write(dir.join(f), b"x").unwrap();
    }
    t.m.rescan();
    t.m.wake_sync(t.t0);
    t.m.publish();
    t
}

impl T {
    fn wake_worker(&mut self, out: WorkerOut, ms: u64) {
        let (gen, now) = (self.m.wake.worker_gen, self.at(ms));
        self.m.handle(Ev::JarvisWorker(gen, Some(out)), now);
    }
    fn wake_pcm(&mut self, n: usize, ms: u64) {
        let (gen, now) = (self.m.wake.capture_gen, self.at(ms));
        self.m.handle(Ev::JarvisPcm(gen, vec![0.5; n]), now);
    }
    fn speech(&mut self, active: bool, ms: u64) {
        self.wake_worker(WorkerOut::Speech { session: 1, active }, ms);
    }
    fn runner(&self) -> Vec<String> {
        lock(&self.log)
            .runner
            .iter()
            .map(|r| match r {
                RunnerIn::Heard { kind, text, .. } => format!("{kind:?}: {text}"),
                RunnerIn::Failed { kind, message } => format!("{kind:?} failed: {message}"),
                RunnerIn::Listening(on) => format!("listening {on}"),
            })
            .collect()
    }
}

#[test]
fn jarvis_listens_with_its_own_capture_and_worker() {
    let mut t = jarvis("j-listen", |f| f.jarvis.wake_word = "Hey Computer".into());
    {
        let log = lock(&t.log);
        assert_eq!(
            (log.open, log.spawned),
            (1, 1),
            "one capture, one wake worker"
        );
        assert_eq!(
            log.jarvis_active.last(),
            Some(&true),
            "push-to-command is live"
        );
    }
    let (threshold, boost) = hako_core::voice::wake_params(0.5);
    assert_eq!(
        msgs(&t.take_worker()),
        [WorkerIn::Wake {
            session: t.m.wake.worker_gen,
            model_dir: t.m.kws_dir().to_string_lossy().into_owned(),
            keywords: vec!["hey computer".into()],
            threshold,
            boost,
        }]
    );
    let state = t.m.state.lock().unwrap().clone();
    assert!(state.jarvis_listening);
    assert_eq!(state.wake_model, Some(ModelStatus::Ready));
    assert_eq!(t.runner(), ["listening true"]);
    // Audio reaches the spotter; nothing loads the dictation model.
    t.wake_pcm(800, 10);
    assert_eq!(pcm_len(&t.take_worker()), [800]);
    assert_eq!(t.engine(), EngineStatus::Idle);

    // Dictation pauses it (no audio to the spotter), then it resumes.
    t.start(100);
    t.wake_pcm(800, 150);
    assert!(pcm_len(&t.take_worker()).is_empty());
    assert!(!t.m.state.lock().unwrap().jarvis_listening);
    t.trigger(HotkeyAction::VoiceCancel, HotkeyEdge::Start, 200);
    assert!(t.m.state.lock().unwrap().jarvis_listening);

    // A crash: started again after the backoff.
    let gen = t.m.wake.worker_gen;
    t.m.handle(Ev::JarvisWorker(gen, None), t.at(300));
    assert!(t.m.wake.worker.is_none());
    assert!(t.m.problems().iter().any(|p| p.starts_with("Jarvis:")));
    t.m.tick(t.at(1400));
    assert!(t.m.wake.worker.is_some());

    // Jarvis off: the ear closes; push-to-command stays (no wake word, so
    // no always-on microphone).
    update_voice(&t.m.paths, |f| {
        f.jarvis.enabled = false;
        Ok(())
    })
    .unwrap();
    t.m.reload(t.at(2000));
    t.m.wake_sync(t.at(2000));
    t.m.publish();
    let log = lock(&t.log);
    assert_eq!(log.open, 0);
    assert_eq!(log.jarvis_active.last(), Some(&true));
    assert!(t.m.wake.worker.is_none());
}

#[test]
fn a_wake_take_is_transcribed_for_the_runner_and_never_typed() {
    let mut t = jarvis("j-take", |_| {});
    t.take_worker();
    t.wake_pcm(16_000, 0);
    t.wake_worker(
        WorkerOut::Wake {
            session: 1,
            keyword: "jarvis".into(),
        },
        1000,
    );
    assert_eq!(lock(&t.log).played, [Sound::Start]);
    t.wake_pcm(8000, 1500);
    t.speech(true, 1200);
    t.speech(false, 2500);
    // The take goes to the dictation model as a batch session.
    let sent = msgs(&t.take_worker());
    assert!(matches!(&sent[..], [WorkerIn::Load { .. }]), "{sent:?}");
    t.worker(WorkerOut::Loaded { caps: caps(true) }, 2600);
    let sent = t.take_worker();
    let session = match &msgs(&sent)[..] {
        [WorkerIn::Begin {
            session,
            streaming: false,
            pause_ms: None,
            ..
        }, WorkerIn::End { .. }] => *session,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        pcm_len(&sent).iter().sum::<usize>(),
        24_000,
        "ring + command"
    );
    t.worker(
        WorkerOut::Partial {
            session,
            committed: "jarvis open".into(),
            tentative: String::new(),
        },
        2700,
    );
    t.worker(
        WorkerOut::Final {
            session,
            text: " Jarvis, open Firefox. ".into(),
            language: None,
        },
        2800,
    );
    assert_eq!(
        t.runner(),
        ["listening true", "Wake: Jarvis, open Firefox."]
    );
    assert!(t.texts().is_empty(), "never typed");
    assert!(t.m.entries.is_empty(), "never in the history");
    assert!(t.partials().is_empty(), "never previewed");

    // The outcome: shown and pinged.
    let outcome = CommandOutcome {
        at: 1,
        heard: "open Firefox.".into(),
        action: Some("open Firefox".into()),
        ok: true,
        message: Some("Opened Firefox".into()),
    };
    t.m.handle(Ev::Jarvis(JarvisOut::Done(outcome.clone())), t.at(3000));
    assert_eq!(t.m.state.lock().unwrap().last_command, Some(outcome));
    assert_eq!(lock(&t.log).played.last(), Some(&Sound::Stop));
}

#[test]
fn wakes_are_ignored_while_the_mic_is_muted_or_a_take_runs() {
    let mut t = jarvis("j-guard", |_| {});
    let wake = WorkerOut::Wake {
        session: 1,
        keyword: "jarvis".into(),
    };
    lock(&t.log).mic_muted = true;
    t.wake_worker(wake.clone(), 100);
    assert!(t.m.wake.ear.taking().is_none());
    lock(&t.log).mic_muted = false;
    t.wake_worker(wake.clone(), 200);
    assert_eq!(t.m.wake.ear.taking(), Some(Listen::Wake));
    t.wake_worker(wake, 300);
    assert_eq!(lock(&t.log).played, [Sound::Start], "one take at a time");
    // Nothing said after the word: the take ends at its deadline.
    t.speech(false, 500);
    t.m.tick(t.at(4200));
    assert!(t.m.wake.ear.taking().is_none());
    assert_eq!(t.m.sessions.len(), 1);
}

#[test]
fn push_to_command_and_what_the_runner_asks_for() {
    // No wake-word model: push-to-command still works.
    let mut t = setup("j-push", |f| f.jarvis.enabled = true, None);
    assert_eq!(lock(&t.log).jarvis_active.last(), Some(&true));
    assert_eq!(lock(&t.log).open, 0, "nothing listens without the model");
    t.trigger(HotkeyAction::JarvisListen, HotkeyEdge::Start, 0);
    assert_eq!(lock(&t.log).open, 1);
    t.wake_pcm(SAMPLE_RATE as usize, 500);
    t.trigger(HotkeyAction::JarvisListen, HotkeyEdge::Stop, 1000);
    assert_eq!(lock(&t.log).open, 0, "closed again after the take");
    t.worker(WorkerOut::Loaded { caps: caps(false) }, 1100);
    let sent = t.take_worker();
    assert_eq!(
        pcm_len(&sent).iter().sum::<usize>(),
        16_000 - 2400,
        "ping bleed dropped"
    );
    let session = t.m.next_session - 1;
    t.worker(
        WorkerOut::Final {
            session,
            text: "volume 30".into(),
            language: None,
        },
        1200,
    );
    assert_eq!(t.runner().last().unwrap(), "Push: volume 30");

    // A spoken "type", keys, a "dictate" the dictation hotkey ends.
    t.m.handle(Ev::Jarvis(JarvisOut::Type("hi".into())), t.at(2000));
    t.m.handle(Ev::Jarvis(JarvisOut::Keys(vec![29, 46])), t.at(2000));
    assert_eq!(t.texts(), ["hi", "keys:[29, 46]"]);
    t.m.handle(Ev::Jarvis(JarvisOut::Dictate), t.at(2100));
    assert_eq!(t.engine(), EngineStatus::Recording);
    t.start(3000);
    assert!(t.m.recording().is_none(), "the hotkey stopped it");

    // A confirmation listens again without the wake word.
    t.m.handle(Ev::Jarvis(JarvisOut::Confirm), t.at(4000));
    assert_eq!(t.m.wake.ear.taking(), Some(Listen::Confirm));
}

#[test]
fn the_wake_model_status_and_its_download_slot() {
    let mut t = setup("j-model", |_| {}, None);
    assert_eq!(t.m.wake_status(), None);
    std::fs::create_dir_all(t.m.kws_dir()).unwrap();
    t.m.rescan();
    assert!(matches!(
        t.m.wake_status(),
        Some(ModelStatus::Corrupt { .. })
    ));
    // One download at a time (a local file that is not there: no network).
    let fetch = Fetch {
        url: "file:///nonexistent/hako-test".into(),
        size: 1,
        sha256: String::new(),
        dest: t.m.dirs.models.join("other.gguf"),
    };
    t.m.download = Some((
        1,
        Download::start("other".into(), "Q8_0".into(), vec![fetch], |_| {}),
    ));
    assert!(t.m.check_action(&VoiceOp::DownloadWake).is_err());
    assert!(!is_setting(&VoiceOp::DownloadWake));
    if let Some((_, d)) = t.m.download.take() {
        d.cancel();
    }
}
