//! LAN audio nodes: per connected peer, a sink whose audio the daemon sends
//! away and a source playing what the peer sends, with a jitter buffer and
//! clock-drift correction. Networking and crypto are the daemon's; this
//! module only moves samples between PipeWire and the daemon's callbacks.
//! Design: `docs/superpowers/specs/2026-11-13-lan-audio-design.md`.
//!
//! Both nodes are `pw_stream`s on one thread and connection of their own
//! (like the meters), processing on the data thread (`RT_PROCESS`). Drift
//! between the two machines' clocks is corrected by PipeWire's adaptive
//! resampler: a slow controller on the main loop turns the receive buffer's
//! fill level into a ratio that the source's process callback writes into
//! the adapter's rate-match area (what `pw_stream_set_rate` does in
//! PipeWire 1.x; 0.3.48 has no such call, `pw_stream_set_control` cannot
//! send the resampler's Double `SPA_PROP_rate` there, and the resampler
//! ignores that prop while the adapter gives it a rate-match area anyway).

use hako_core::net::{JITTER_MAX_MS, JITTER_MIN_MS};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// The wire format: 48 kHz, interleaved stereo, i16.
pub const NET_RATE: u32 = 48_000;
pub const NET_CHANNELS: usize = 2;
/// Longest block handed to a `SendFn` (frames): one UDP packet, 5 ms.
pub const NET_MAX_FRAMES: usize = 240;

/// Gets each block played into a peer's sink (interleaved stereo i16 at
/// `NET_RATE`, at most `NET_MAX_FRAMES` frames) and the frame position of
/// its first frame. Runs on PipeWire's data thread: never block or allocate.
/// Blocks of digital silence are not handed over.
pub type SendFn = Box<dyn FnMut(u64, &[i16]) + Send>;

const FRAMES_PER_MS: usize = NET_RATE as usize / 1000;
/// Bytes per S16LE stereo frame.
const STRIDE: usize = NET_CHANNELS * 2;
/// A gap in the peer's positions up to this long is played as silence;
/// a longer one restarts the buffer.
const MAX_GAP: usize = 20 * FRAMES_PER_MS;
/// Audio further behind the buffer than this: the peer started counting
/// again, so restart instead of dropping it forever.
const MAX_BEHIND: u64 = NET_RATE as u64;
/// The largest target, its overflow allowance, a 21 ms quantum and a packet.
const CAPACITY: usize = (2 * JITTER_MAX_MS as usize + 50) * FRAMES_PER_MS;
/// How often a lost PipeWire connection or a failed stream is retried.
const RETRY: Duration = Duration::from_secs(1);
/// How often the drift controller runs.
const DRIFT_TICK: Duration = Duration::from_millis(250);
/// Resampler ratio limit: ±0.5 %.
const MAX_SKEW: f64 = 0.005;
/// Drift controller gains (ratio per second of fill error, and per second
/// of its integral): critically damped, settles in ~20 s; a 2000 ppm step
/// moves the fill by ~3 ms at most, real clocks (tens of ppm) by < 0.1 ms.
const KP: f64 = 0.5;
const KI: f64 = 0.0625;
/// A send callback this long after the previous one: the sink sat idle
/// (unlinked), so the position jumps by the time passed.
const IDLE: Duration = Duration::from_millis(200);
/// `receiving` / `sending` look this far back.
const RECENT: Duration = Duration::from_secs(1);
/// Tries for the receive buffer's lock on the data thread.
const LOCK_SPINS: usize = 200;

/// One port's live numbers (`hako_core::net::LinkStats` takes these).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PortStats {
    /// Audio waiting in the receive buffer.
    pub buffered_ms: f32,
    /// Times the receive buffer ran dry while audio was arriving.
    pub dropouts: u64,
    /// Audio arrived in the last second.
    pub receiving: bool,
    /// Audio was sent in the last second.
    pub sending: bool,
}

/// The receive buffer: blocks at peer frame positions in, a steady stream
/// out. `target` is what stays buffered after each pull.
struct Jitter {
    ring: Box<[[i16; 2]]>,
    start: usize,
    len: usize,
    /// Peer position just after the newest buffered frame.
    end: Option<u64>,
    target: usize,
    /// Frames the latest pull asked for (the receiver's quantum).
    pulled: usize,
    playing: bool,
    /// Ran dry while playing; a dropout once more audio follows on time
    /// (not when the peer had just stopped sending).
    starved: bool,
    dropouts: u64,
}

impl Jitter {
    fn new(jitter_ms: u32) -> Self {
        Self {
            ring: vec![[0; 2]; CAPACITY].into_boxed_slice(),
            start: 0,
            len: 0,
            end: None,
            target: target_frames(jitter_ms),
            pulled: 0,
            playing: false,
            starved: false,
            dropouts: 0,
        }
    }

    /// A smaller target drops the excess now; a larger one refills first.
    fn set_target(&mut self, jitter_ms: u32) {
        let target = target_frames(jitter_ms);
        if target == self.target {
            return;
        }
        if self.len > target + self.pulled {
            self.skip(self.len - target - self.pulled);
        } else if target > self.target {
            self.playing = false;
        }
        self.target = target;
    }

    fn push(&mut self, pos: u64, samples: &[i16]) {
        let frames = samples.as_chunks::<2>().0;
        // A position from the network that would wrap: start over.
        let Some(new_end) = pos.checked_add(frames.len() as u64) else {
            self.len = 0;
            self.end = None;
            self.playing = false;
            self.starved = false;
            return;
        };
        match self.end {
            Some(end) if pos < end && end - pos <= MAX_BEHIND => return,
            Some(end) if pos >= end && pos - end <= MAX_GAP as u64 => {
                if std::mem::take(&mut self.starved) {
                    self.dropouts += 1;
                }
                for _ in end..pos {
                    self.put([0; 2]);
                }
            }
            _ => {
                self.len = 0;
                self.playing = false;
                self.starved = false;
            }
        }
        for frame in frames {
            self.put(*frame);
        }
        self.end = Some(new_end);
        if self.len > self.target + self.pulled + self.target.max(MAX_GAP) {
            self.skip(self.len - self.target - self.pulled);
        }
    }

    /// Fill `out` (S16LE stereo); silence until target + `out` is buffered.
    fn pull(&mut self, out: &mut [u8]) {
        let frames = out.as_chunks_mut::<STRIDE>().0;
        self.pulled = frames.len();
        if !self.playing && self.len >= self.target + frames.len() {
            self.playing = true;
        }
        for frame in frames {
            let [left, right] = if self.playing && self.len > 0 {
                let f = self.ring[self.start];
                self.skip(1);
                f
            } else {
                if self.playing {
                    self.playing = false;
                    self.starved = true;
                }
                [0; 2]
            };
            frame[..2].copy_from_slice(&left.to_le_bytes());
            frame[2..].copy_from_slice(&right.to_le_bytes());
        }
    }

    fn put(&mut self, frame: [i16; 2]) {
        if self.len == self.ring.len() {
            self.skip(1);
        }
        let at = (self.start + self.len) % self.ring.len();
        self.ring[at] = frame;
        self.len += 1;
    }

    fn skip(&mut self, frames: usize) {
        let frames = frames.min(self.len);
        self.start = (self.start + frames) % self.ring.len();
        self.len -= frames;
    }
}

fn target_frames(jitter_ms: u32) -> usize {
    jitter_ms.clamp(JITTER_MIN_MS, JITTER_MAX_MS) as usize * FRAMES_PER_MS
}

/// Hand `bytes` (S16LE stereo) to `send` in blocks of at most
/// `NET_MAX_FRAMES` frames, skipping digital silence; `pos` advances by
/// every frame. True when anything was sent.
fn send_blocks(pos: &mut u64, bytes: &[u8], mut send: impl FnMut(u64, &[i16])) -> bool {
    let mut sent = false;
    let mut block = [0i16; NET_MAX_FRAMES * NET_CHANNELS];
    for chunk in bytes.as_chunks::<STRIDE>().0.chunks(NET_MAX_FRAMES) {
        let samples = &mut block[..chunk.len() * NET_CHANNELS];
        for (sample, b) in samples
            .iter_mut()
            .zip(chunk.as_flattened().as_chunks::<2>().0)
        {
            *sample = i16::from_le_bytes(*b);
        }
        if samples.iter().any(|&s| s != 0) {
            send(*pos, samples);
            sent = true;
        }
        *pos += chunk.len() as u64;
    }
    sent
}

/// Slow PI controller from the receive buffer's fill error to the
/// resampler ratio of the receive stream.
#[derive(Default)]
struct Drift {
    integral: f64,
}

impl Drift {
    /// `error` = average fill minus target, in seconds. The resampler takes
    /// `NET_RATE / ratio` frames a second from the buffer, so a ratio below
    /// 1 drains it (checked by the live test).
    fn update(&mut self, error: f64, dt: f64) -> f64 {
        self.integral = (self.integral + error * dt).clamp(-MAX_SKEW / KI, MAX_SKEW / KI);
        1.0 - (KP * error + KI * self.integral).clamp(-MAX_SKEW, MAX_SKEW)
    }
}

struct Sender {
    send: SendFn,
    /// Survives stream rebuilds: the peer drops positions it has seen.
    pos: u64,
    last: Option<Instant>,
}

/// What a port's owner, the data thread and the drift timer share.
struct State {
    jitter: Mutex<Jitter>,
    sender: Mutex<Sender>,
    target_ms: AtomicU32,
    /// Receive fill in frames after the latest push or pull.
    fill: AtomicU32,
    /// Fill after each pull while playing, summed, and how many pulls.
    fill_sum: AtomicU64,
    fills: AtomicU64,
    /// Average fill over the last drift tick (frames), for `stats`.
    buffered: AtomicU32,
    dropouts: AtomicU64,
    /// Resampler ratio of the receive stream (f64 bits).
    ratio: AtomicU64,
    epoch: Instant,
    /// Milliseconds since `epoch`, plus one (0 = never), of the latest
    /// push / block sent.
    pushed: AtomicU64,
    sent: AtomicU64,
    /// Why its nodes are missing (set by the PipeWire thread).
    problem: Mutex<Option<String>>,
}

impl State {
    fn new(jitter_ms: u32, send: SendFn) -> Self {
        Self {
            jitter: Mutex::new(Jitter::new(jitter_ms)),
            sender: Mutex::new(Sender {
                send,
                pos: 0,
                last: None,
            }),
            target_ms: AtomicU32::new(jitter_ms),
            fill: AtomicU32::new(0),
            fill_sum: AtomicU64::new(0),
            fills: AtomicU64::new(0),
            buffered: AtomicU32::new(0),
            dropouts: AtomicU64::new(0),
            ratio: AtomicU64::new(1f64.to_bits()),
            epoch: Instant::now(),
            pushed: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            problem: Mutex::default(),
        }
    }

    /// Record why the nodes are missing (`None`: they are up); the first
    /// failure is a warning, repeats of it are not.
    fn set_problem(&self, label: &str, problem: Option<String>) {
        let mut slot = self.problem.lock().unwrap_or_else(PoisonError::into_inner);
        match &problem {
            Some(p) if slot.is_none() => warn!("net audio: nodes for {label}: {p}"),
            Some(p) => debug!("net audio: nodes for {label}: {p}"),
            None => {}
        }
        *slot = problem;
    }

    fn publish(&self, jitter: &Jitter) {
        self.fill.store(jitter.len as u32, Ordering::Relaxed);
        self.dropouts.store(jitter.dropouts, Ordering::Relaxed);
    }

    fn stamp(&self, at: &AtomicU64, now: Instant) {
        at.store((now - self.epoch).as_millis() as u64 + 1, Ordering::Relaxed);
    }

    fn recent(&self, at: &AtomicU64) -> bool {
        let t = at.load(Ordering::Relaxed);
        t != 0
            && (self.epoch.elapsed().as_millis() as u64 + 1).saturating_sub(t)
                <= RECENT.as_millis() as u64
    }

    /// The source's process callback: pull a buffer's worth, or silence
    /// when the push side holds the lock.
    fn play(&self, out: &mut [u8]) {
        // ponytail: bounded spin on the push side's few-µs critical section,
        // then a silent cycle; a lock-free ring if that shows up as dropouts.
        for _ in 0..LOCK_SPINS {
            let mut jitter = match self.jitter.try_lock() {
                Ok(j) => j,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    std::hint::spin_loop();
                    continue;
                }
            };
            jitter.pull(out);
            self.publish(&jitter);
            if jitter.playing {
                self.fill_sum
                    .fetch_add(jitter.len as u64, Ordering::Relaxed);
                self.fills.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        out.fill(0);
    }

    /// The sink's process callback.
    fn capture(&self, bytes: &[u8]) {
        let Ok(mut sender) = self.sender.try_lock() else {
            return;
        };
        let now = Instant::now();
        if let Some(idle) = sender.last.replace(now).map(|last| now - last) {
            if idle > IDLE {
                sender.pos += (idle.as_secs_f64() * f64::from(NET_RATE)) as u64;
            }
        }
        let Sender { send, pos, .. } = &mut *sender;
        if send_blocks(pos, bytes, send) {
            self.stamp(&self.sent, now);
        }
    }
}

enum Cmd {
    Open(u64, Spec),
    Close(u64),
    Quit,
}

struct Spec {
    send_node: String,
    recv_node: String,
    label: String,
    state: Arc<State>,
}

struct Worker {
    tx: pw::channel::Sender<Cmd>,
    thread: std::thread::JoinHandle<()>,
    /// Tells this thread's ports from those of one that stopped.
    generation: u64,
    ports: usize,
    next_id: u64,
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The PipeWire side of every peer: its own thread and connection, started
/// with the first `open`, healed after a PipeWire restart.
#[derive(Default)]
pub struct NetAudio {
    worker: Arc<Mutex<Option<Worker>>>,
}

impl NetAudio {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create `send_node` (Audio/Sink, described `hako to <label>`: what
    /// plays into it goes to `send`) and `recv_node` (Audio/Source, `hako
    /// from <label>`: what `NetPort::push` delivers, after a `jitter_ms`
    /// buffer). Neither autoconnects nor becomes a default device. They
    /// exist until the port is dropped. An error: the PipeWire thread
    /// could not be set up (tried again on the next `open`).
    pub fn open(
        &self,
        send_node: &str,
        recv_node: &str,
        label: &str,
        jitter_ms: u32,
        send: SendFn,
    ) -> Result<NetPort, String> {
        let state = Arc::new(State::new(jitter_ms, send));
        let mut worker = lock(&self.worker);
        // A thread that stopped is replaced; its ports have no nodes any
        // more (`NetPort::problem`).
        let w = match worker.take() {
            Some(w) if !w.thread.is_finished() => w,
            stopped => {
                if stopped.is_some() {
                    warn!("net audio: the PipeWire thread stopped; starting another");
                }
                spawn_worker()?
            }
        };
        let w = worker.insert(w);
        w.ports += 1;
        w.next_id += 1;
        let id = w.next_id;
        let _ = w.tx.send(Cmd::Open(
            id,
            Spec {
                send_node: send_node.into(),
                recv_node: recv_node.into(),
                label: label.into(),
                state: state.clone(),
            },
        ));
        Ok(NetPort {
            worker: self.worker.clone(),
            generation: w.generation,
            id,
            state,
        })
    }
}

/// A peer's nodes; dropping it removes them. `Send + Sync`.
pub struct NetPort {
    worker: Arc<Mutex<Option<Worker>>>,
    generation: u64,
    id: u64,
    state: Arc<State>,
}

impl NetPort {
    /// Audio from the peer: interleaved stereo i16 at `NET_RATE`, `pos` =
    /// sender frame position of `samples[0]` (gaps are lost packets or
    /// silence). Called from the daemon's UDP thread; never blocks for long.
    pub fn push(&self, pos: u64, samples: &[i16]) {
        let s = &self.state;
        let mut jitter = s.jitter.lock().unwrap_or_else(PoisonError::into_inner);
        jitter.push(pos, samples);
        s.publish(&jitter);
        drop(jitter);
        s.stamp(&s.pushed, Instant::now());
    }

    /// New receive buffer target, live.
    pub fn set_jitter(&self, ms: u32) {
        let s = &self.state;
        s.jitter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set_target(ms);
        s.target_ms.store(ms, Ordering::Relaxed);
    }

    pub fn stats(&self) -> PortStats {
        let s = &self.state;
        PortStats {
            buffered_ms: s.buffered.load(Ordering::Relaxed) as f32 / FRAMES_PER_MS as f32,
            dropouts: s.dropouts.load(Ordering::Relaxed),
            receiving: s.recent(&s.pushed),
            sending: s.recent(&s.sent),
        }
    }

    /// Why its nodes are missing (PipeWire unreachable, nodes refused, the
    /// PipeWire thread stopped); `None` while they are up or coming up.
    pub fn problem(&self) -> Option<String> {
        let running = lock(&self.worker)
            .as_ref()
            .is_some_and(|w| w.generation == self.generation && !w.thread.is_finished());
        if !running {
            return Some("the network audio thread stopped".into());
        }
        self.state
            .problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for NetPort {
    fn drop(&mut self) {
        let mut worker = lock(&self.worker);
        let Some(w) = worker.as_mut().filter(|w| w.generation == self.generation) else {
            return;
        };
        let _ = w.tx.send(Cmd::Close(self.id));
        w.ports -= 1;
        if w.ports == 0 {
            let _ = w.tx.send(Cmd::Quit);
            *worker = None;
        }
    }
}

fn lock(worker: &Mutex<Option<Worker>>) -> MutexGuard<'_, Option<Worker>> {
    worker.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Start the PipeWire thread and wait until its loop is set up.
fn spawn_worker() -> Result<Worker, String> {
    let (tx, rx) = pw::channel::channel();
    let (ready, started) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("hako-net".into())
        .spawn(move || {
            if let Err(e) = run(rx, &ready) {
                let _ = ready.send(Err(e.to_string()));
            }
        })
        .map_err(|e| format!("cannot start the network audio thread: {e}"))?;
    started
        .recv()
        .unwrap_or_else(|_| Err("the thread ended".into()))
        .map_err(|e| format!("network audio: {e}"))?;
    Ok(Worker {
        tx,
        thread,
        generation: GENERATION.fetch_add(1, Ordering::Relaxed),
        ports: 0,
        next_id: 0,
    })
}

/// Fields drop in order: the listener is unhooked before the stream goes.
struct LiveStream {
    _listener: pw::stream::StreamListener<()>,
    _stream: pw::stream::StreamRc,
}

struct Live {
    _send: LiveStream,
    _recv: LiveStream,
    failed: Rc<Cell<bool>>,
}

struct Port {
    spec: Spec,
    live: Option<Live>,
    drift: Drift,
}

struct Conn {
    _listener: pw::core::Listener,
    core: pw::core::CoreRc,
    /// Set by the core error event (PipeWire went away).
    dead: Rc<Cell<bool>>,
}

#[derive(Default)]
struct Engine {
    conn: Option<Conn>,
    ports: HashMap<u64, Port>,
}

type EngineRc = Rc<RefCell<Engine>>;

fn run(
    rx: pw::channel::Receiver<Cmd>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let engine: EngineRc = Rc::default();

    let _rx = rx.attach(mainloop.loop_(), {
        let (engine, context, mainloop) = (engine.clone(), context.clone(), mainloop.clone());
        move |cmd| {
            match cmd {
                Cmd::Open(id, spec) => {
                    engine.borrow_mut().ports.insert(
                        id,
                        Port {
                            spec,
                            live: None,
                            drift: Drift::default(),
                        },
                    );
                }
                Cmd::Close(id) => {
                    engine.borrow_mut().ports.remove(&id);
                }
                Cmd::Quit => {
                    mainloop.quit();
                    return;
                }
            }
            tick(&engine, &context);
        }
    });
    let retry = mainloop.loop_().add_timer({
        let (engine, context) = (engine.clone(), context.clone());
        move |_| tick(&engine, &context)
    });
    retry
        .update_timer(Some(RETRY), Some(RETRY))
        .into_sync_result()?;
    let drift = mainloop.loop_().add_timer({
        let engine = engine.clone();
        move |_| correct_drift(&engine)
    });
    drift
        .update_timer(Some(DRIFT_TICK), Some(DRIFT_TICK))
        .into_sync_result()?;

    let _ = ready.send(Ok(()));
    mainloop.run();
    // Streams and the connection go before the context and loop.
    let mut e = engine.borrow_mut();
    e.ports.clear();
    e.conn = None;
    Ok(())
}

/// Bring the connection and streams in line with the open ports.
fn tick(engine: &EngineRc, context: &pw::context::ContextRc) {
    let mut e = engine.borrow_mut();
    if e.conn.as_ref().is_some_and(|c| c.dead.get()) {
        debug!("net audio: PipeWire connection lost; reconnecting");
        for port in e.ports.values_mut() {
            port.live = None;
        }
        e.conn = None;
    }
    if e.ports.is_empty() {
        return;
    }
    if e.conn.is_none() {
        match connect(context) {
            Ok(conn) => e.conn = Some(conn),
            Err(err) => {
                for port in e.ports.values() {
                    let problem = format!("cannot connect to PipeWire: {err}");
                    port.spec.state.set_problem(&port.spec.label, Some(problem));
                }
                return;
            }
        }
    }
    let Engine { conn, ports } = &mut *e;
    let Some(conn) = conn.as_ref() else {
        return;
    };
    for port in ports.values_mut() {
        if port.live.as_ref().is_some_and(|l| l.failed.get()) {
            port.live = None;
        }
        if port.live.is_none() {
            let state = &port.spec.state;
            match open_port(&conn.core, &port.spec) {
                Ok(live) => {
                    port.live = Some(live);
                    state.set_problem(&port.spec.label, None);
                }
                Err(err) => state.set_problem(&port.spec.label, Some(err.to_string())),
            }
        }
    }
}

/// Every `DRIFT_TICK`: the average fill since the last tick sets the
/// receive stream's resampler ratio.
fn correct_drift(engine: &EngineRc) {
    for port in engine.borrow_mut().ports.values_mut() {
        let s = &port.spec.state;
        let n = s.fills.swap(0, Ordering::Relaxed);
        let sum = s.fill_sum.swap(0, Ordering::Relaxed);
        if n == 0 {
            s.buffered
                .store(s.fill.load(Ordering::Relaxed), Ordering::Relaxed);
            continue;
        }
        let fill = sum as f64 / n as f64;
        s.buffered.store(fill.round() as u32, Ordering::Relaxed);
        let target = target_frames(s.target_ms.load(Ordering::Relaxed)) as f64;
        let error = (fill - target) / f64::from(NET_RATE);
        let ratio = port.drift.update(error, DRIFT_TICK.as_secs_f64());
        s.ratio.store(ratio.to_bits(), Ordering::Relaxed);
    }
}

fn connect(context: &pw::context::ContextRc) -> Result<Conn, pw::Error> {
    let core = context.connect_rc(None)?;
    let dead = Rc::new(Cell::new(false));
    let listener = core
        .add_listener_local()
        .error({
            let dead = dead.clone();
            move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    debug!("net audio: core error {res}: {message}");
                    dead.set(true);
                }
            }
        })
        .register();
    Ok(Conn {
        _listener: listener,
        core,
        dead,
    })
}

fn stream_props(node: &str, description: &str, class: &str) -> pw::properties::PropertiesBox {
    properties! {
        "media.type" => "Audio",
        "media.class" => class,
        "node.name" => node,
        "node.description" => description,
        "node.virtual" => "true",
        "node.network" => "true",
        "node.autoconnect" => "false",
        "node.dont-reconnect" => "true",
        "node.dont-fallback" => "true",
        "state.restore-props" => "false",
        // ALSA devices sit at 600-2100: never picked as a default device.
        "priority.session" => "1",
        "priority.driver" => "1",
        // Drift correction needs the resampler even when the rates match.
        "resample.disable" => "false",
    }
}

/// S16LE interleaved stereo at `NET_RATE`; the adapter converts from and to
/// the graph.
fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(NET_RATE);
    info.set_channels(NET_CHANNELS as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("format pod serializes")
    .0
    .into_inner()
}

fn connect_stream(
    stream: &pw::stream::StreamRc,
    direction: spa::utils::Direction,
) -> Result<(), pw::Error> {
    let pod = format_pod();
    let mut params = [spa::pod::Pod::from_bytes(&pod).expect("format pod parses")];
    // RT_PROCESS: a stream processing on the main loop is `node.async`
    // (PipeWire 1.x), and so is every link it has.
    stream.connect(
        direction,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )
}

fn open_port(core: &pw::core::CoreRc, spec: &Spec) -> Result<Live, pw::Error> {
    let failed = Rc::new(Cell::new(false));
    let on_state = |node: &str| {
        let (failed, node) = (failed.clone(), node.to_string());
        move |_: &pw::stream::Stream,
              _: &mut (),
              _: pw::stream::StreamState,
              state: pw::stream::StreamState| {
            if let pw::stream::StreamState::Error(err) = state {
                debug!("net audio: {node} failed: {err}");
                failed.set(true);
            }
        }
    };

    let send = pw::stream::StreamRc::new(
        core.clone(),
        &spec.send_node,
        stream_props(
            &spec.send_node,
            &format!("hako to {}", spec.label),
            "Audio/Sink",
        ),
    )?;
    let send_listener = send
        .add_local_listener_with_user_data(())
        .state_changed(on_state(&spec.send_node))
        .process({
            let state = spec.state.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, size) = (data.chunk().offset(), data.chunk().size());
                let Some(bytes) = data.data() else {
                    return;
                };
                let (offset, size) = (offset as usize, size as usize);
                state.capture(bytes.get(offset..offset + size).unwrap_or_default());
            }
        })
        .register()?;
    connect_stream(&send, spa::utils::Direction::Input)?;

    let recv = pw::stream::StreamRc::new(
        core.clone(),
        &spec.recv_node,
        stream_props(
            &spec.recv_node,
            &format!("hako from {}", spec.label),
            "Audio/Source",
        ),
    )?;
    // The adapter's rate-match area: the resampler's ratio and how many
    // frames it wants next.
    let rate_match = Arc::new(AtomicPtr::<spa::sys::spa_io_rate_match>::default());
    let recv_listener = recv
        .add_local_listener_with_user_data(())
        .state_changed(on_state(&spec.recv_node))
        .io_changed({
            let rate_match = rate_match.clone();
            move |_, _, id, area, size| {
                if id == spa::sys::SPA_IO_RateMatch {
                    let fits = size as usize >= size_of::<spa::sys::spa_io_rate_match>();
                    let area = if fits {
                        area.cast()
                    } else {
                        std::ptr::null_mut()
                    };
                    rate_match.store(area, Ordering::Release);
                }
            }
        })
        .process({
            let state = spec.state.clone();
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let io = rate_match.load(Ordering::Acquire);
                // SAFETY: `io` is the adapter's rate-match area, which lives
                // as long as the stream's node; only this data-thread
                // callback writes it on our side.
                let wanted = (!io.is_null()).then(|| unsafe {
                    (*io).rate = f64::from_bits(state.ratio.load(Ordering::Relaxed));
                    (*io).flags |= spa::sys::SPA_IO_RATE_MATCH_FLAG_ACTIVE;
                    (*io).size as usize
                });
                let mut frames = 0;
                if let Some(bytes) = data.data() {
                    let capacity = bytes.len() / STRIDE;
                    frames = wanted
                        .filter(|&n| n > 0)
                        .map_or(capacity, |n| n.min(capacity));
                    state.play(&mut bytes[..frames * STRIDE]);
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = STRIDE as _;
                *chunk.size_mut() = (frames * STRIDE) as _;
            }
        })
        .register()?;
    connect_stream(&recv, spa::utils::Direction::Output)?;
    debug!("net audio: nodes for {} up", spec.label);
    Ok(Live {
        _send: LiveStream {
            _listener: send_listener,
            _stream: send,
        },
        _recv: LiveStream {
            _listener: recv_listener,
            _stream: recv,
        },
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: usize = FRAMES_PER_MS;

    /// `frames` stereo frames of `value`.
    fn block(frames: usize, value: i16) -> Vec<i16> {
        vec![value; frames * 2]
    }

    /// Pull `frames`; the left channel of each frame.
    fn pull(j: &mut Jitter, frames: usize) -> Vec<i16> {
        let mut out = vec![0u8; frames * STRIDE];
        j.pull(&mut out);
        out.as_chunks::<STRIDE>()
            .0
            .iter()
            .map(|f| i16::from_le_bytes([f[0], f[1]]))
            .collect()
    }

    #[test]
    fn port_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<NetPort>();
        check::<NetAudio>();
    }

    #[test]
    fn plays_silence_until_target_plus_a_pull_is_buffered() {
        let mut j = Jitter::new(10);
        j.push(0, &block(5 * MS, 1));
        assert!(pull(&mut j, 5 * MS).iter().all(|&s| s == 0));
        j.push(5 * MS as u64, &block(10 * MS, 2));
        // 15 ms buffered, 10 ms target + a 5 ms pull: plays, oldest first.
        let out = pull(&mut j, 5 * MS);
        assert!(out.iter().all(|&s| s == 1), "{out:?}");
        assert_eq!(j.len, 10 * MS);
    }

    #[test]
    fn running_dry_while_audio_arrives_is_a_dropout() {
        let mut j = Jitter::new(2);
        j.push(0, &block(4 * MS, 1));
        pull(&mut j, 2 * MS);
        let out = pull(&mut j, 4 * MS);
        assert_eq!(out.iter().filter(|&&s| s == 1).count(), 2 * MS);
        assert!(!j.playing);
        assert_eq!(j.dropouts, 0, "counted once audio follows");
        j.push(4 * MS as u64, &block(MS, 1));
        assert_eq!(j.dropouts, 1);
        // Running dry because the peer stopped (a long gap) is no dropout.
        j.push(4 * MS as u64 + 100 * MS as u64, &block(6 * MS, 1));
        pull(&mut j, 2 * MS);
        pull(&mut j, 8 * MS);
        j.push(1_000 * MS as u64, &block(MS, 1));
        assert_eq!(j.dropouts, 1);
    }

    #[test]
    fn a_small_gap_is_filled_with_silence() {
        let mut j = Jitter::new(2);
        j.push(0, &block(MS, 1));
        j.push(11 * MS as u64, &block(MS, 2));
        assert_eq!(j.len, 12 * MS);
        let out = pull(&mut j, 10 * MS);
        assert!(out[..MS].iter().all(|&s| s == 1));
        assert!(out[MS..].iter().all(|&s| s == 0));
        assert_eq!(pull(&mut j, MS), vec![0; MS]);
        assert_eq!(j.ring[j.start], [2, 2]);
    }

    #[test]
    fn a_big_gap_restarts_at_the_new_audio() {
        let mut j = Jitter::new(2);
        j.push(0, &block(10 * MS, 1));
        pull(&mut j, MS);
        assert!(j.playing);
        j.push(10 * MS as u64 + MAX_GAP as u64 + 1, &block(3 * MS, 2));
        assert_eq!(j.len, 3 * MS);
        assert!(!j.playing);
        assert_eq!(pull(&mut j, MS), vec![2; MS]);
    }

    #[test]
    fn audio_behind_the_buffer_is_dropped() {
        let mut j = Jitter::new(2);
        j.push(100_000, &block(MS, 1));
        j.push(100_000, &block(MS, 2));
        j.push(100_000 + MS as u64 - 1, &block(MS, 3));
        assert_eq!(j.len, MS);
        assert_eq!(j.end, Some(100_000 + MS as u64));
        assert_eq!(j.ring[j.start], [1, 1]);
        // Far behind: the peer started counting again.
        j.push(0, &block(MS, 4));
        assert_eq!((j.len, j.ring[j.start]), (MS, [4, 4]));
        // A position that would wrap restarts the buffer, no overflow.
        j.push(u64::MAX - 10, &block(MS, 5));
        assert_eq!((j.len, j.end, j.playing), (0, None, false));
        j.push(0, &block(MS, 6));
        assert_eq!((j.len, j.ring[j.start]), (MS, [6, 6]));
    }

    #[test]
    fn overflow_drops_the_oldest_down_to_target() {
        let mut j = Jitter::new(10);
        pull(&mut j, 5 * MS);
        for i in 0..7 {
            j.push(i * 5 * MS as u64, &block(5 * MS, i as i16));
        }
        // Limit: 10 target + 5 pull + 20 allowance = 35 ms.
        assert_eq!(j.len, 35 * MS);
        j.push(35 * MS as u64, &block(5 * MS, 7));
        assert_eq!(j.len, 15 * MS);
        assert_eq!(j.ring[j.start], [5, 5]);
    }

    #[test]
    fn a_new_target_applies_at_once() {
        let mut j = Jitter::new(20);
        j.push(0, &block(30 * MS, 1));
        pull(&mut j, 5 * MS);
        j.set_target(10);
        assert_eq!(j.len, 15 * MS);
        assert!(j.playing);
        j.set_target(50);
        assert!(!j.playing);
    }

    #[test]
    fn send_splits_blocks_and_skips_silence() {
        let frames: Vec<i16> = (0..600)
            .flat_map(|n| {
                let v = if (240..480).contains(&n) {
                    0
                } else {
                    n as i16 + 1
                };
                [v, -v]
            })
            .collect();
        let bytes: Vec<u8> = frames.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut pos = 1_000;
        let mut got = Vec::new();
        assert!(send_blocks(&mut pos, &bytes, |p, b| got.push((p, b.to_vec()))));
        assert_eq!(pos, 1_600);
        let starts: Vec<_> = got.iter().map(|(p, b)| (*p, b.len() / 2)).collect();
        assert_eq!(starts, [(1_000, 240), (1_480, 120)]);
        assert_eq!(&got[0].1[..4], [1, -1, 2, -2]);
        assert_eq!(got[1].1[0], 481);
        assert!(!send_blocks(&mut pos, &[0; 64], |_, _| panic!(
            "silence sent"
        )));
        assert_eq!(pos, 1_616);
    }

    /// Sender 2000 ppm fast; the resampler takes `NET_RATE / ratio`.
    #[test]
    fn drift_controller_holds_the_fill_at_target() {
        let (mut drift, mut error, mut ratio) = (Drift::default(), 0.0, 1.0);
        let dt = DRIFT_TICK.as_secs_f64();
        let mut worst = 0f64;
        for _ in 0..(60.0 / dt) as usize {
            error += dt * (1.002 - 1.0 / ratio);
            worst = worst.max(error.abs());
            ratio = drift.update(error, dt);
        }
        assert!(error.abs() < 0.000_2, "{error}");
        assert!(worst < 0.004, "{worst}");
        assert!((ratio - 1.0 / 1.002).abs() < 1e-4, "{ratio}");
        // Clamped to ±0.5 %.
        assert_eq!(Drift::default().update(1.0, dt), 1.0 - MAX_SKEW);
    }

    mod live {
        use super::*;
        use crate::engine::{engine, Direction, Graph};
        use std::sync::atomic::AtomicUsize;

        fn test_objects(prefix: &str) -> usize {
            let dump = std::process::Command::new("pw-dump").output().unwrap();
            String::from_utf8_lossy(&dump.stdout)
                .matches(prefix)
                .count()
        }

        fn props(name: &str, category: &str) -> pw::properties::PropertiesBox {
            properties! {
                "media.type" => "Audio",
                "media.category" => category,
                "node.name" => name,
                "node.latency" => "256/48000",
                "node.autoconnect" => "false",
                "node.dont-reconnect" => "true",
                "state.restore-props" => "false",
                // The idle software driver, never the user's devices.
                "node.group" => "pipewire.dummy",
            }
        }

        fn connect(stream: &pw::stream::StreamRc, direction: spa::utils::Direction) {
            let pod = crate::meters::format_pod();
            let mut params = [spa::pod::Pod::from_bytes(&pod).unwrap()];
            stream
                .connect(
                    direction,
                    None,
                    pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
                    &mut params,
                )
                .unwrap();
        }

        /// 440 Hz tone at -6 dBFS (peak 0.5).
        fn tone(n: u64) -> f32 {
            (n as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5
        }

        /// Opens a port with `node.group=pipewire.dummy` test streams on
        /// both nodes: a tone played into the send node reaches the
        /// `SendFn`; a tone pushed at a 2000 ppm fast clock comes out of
        /// the receive node, and the drift controller keeps the buffer at
        /// its target (without it the fill grows 2 ms a second).
        #[test]
        #[ignore = "needs a live PipeWire session (pw-dump)"]
        fn moves_audio_both_ways_and_corrects_drift() {
            let prefix = format!("hako-test-net-{}", std::process::id());
            let (send_node, recv_node) = (format!("{prefix}.send"), format!("{prefix}.recv"));
            let (gen, cap) = (format!("{prefix}-gen"), format!("{prefix}-cap"));
            let engine = engine().unwrap();
            let sent = Arc::new(AtomicUsize::new(0));
            let audio = NetAudio::new();
            let port = audio
                .open(
                    &send_node,
                    &recv_node,
                    "test",
                    12,
                    Box::new({
                        let sent = sent.clone();
                        move |_, block| {
                            if block.iter().any(|&s| s.unsigned_abs() > 8_000) {
                                sent.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }),
                )
                .unwrap();

            pw::init();
            let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
            let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
            let core = context.connect_rc(None).unwrap();
            let generator =
                pw::stream::StreamRc::new(core.clone(), &gen, props(&gen, "Playback")).unwrap();
            let _gen_listener = generator
                .add_local_listener_with_user_data(0u64)
                .process(|stream, n| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let Some(data) = buffer.datas_mut().first_mut() else {
                        return;
                    };
                    let frames = data.data().map_or(0, |bytes| {
                        let frames = bytes.as_chunks_mut::<8>().0;
                        let len = frames.len().min(256);
                        for frame in &mut frames[..len] {
                            let s = tone(*n).to_le_bytes();
                            frame[..4].copy_from_slice(&s);
                            frame[4..].copy_from_slice(&s);
                            *n += 1;
                        }
                        len
                    });
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = 8;
                    *chunk.size_mut() = (frames * 8) as _;
                })
                .register()
                .unwrap();
            connect(&generator, spa::utils::Direction::Output);
            let peak = Arc::new(AtomicU32::new(0));
            let capture =
                pw::stream::StreamRc::new(core.clone(), &cap, props(&cap, "Capture")).unwrap();
            let _cap_listener = capture
                .add_local_listener_with_user_data(())
                .process({
                    let peak = peak.clone();
                    move |stream, _| {
                        let Some(mut buffer) = stream.dequeue_buffer() else {
                            return;
                        };
                        let Some(data) = buffer.datas_mut().first_mut() else {
                            return;
                        };
                        let size = data.chunk().size() as usize;
                        let Some(bytes) = data.data() else {
                            return;
                        };
                        for s in bytes[..size.min(bytes.len())].as_chunks::<4>().0 {
                            let v = f32::from_le_bytes(*s).abs().min(f32::MAX);
                            peak.fetch_max(v.to_bits(), Ordering::Relaxed);
                        }
                    }
                })
                .register()
                .unwrap();
            connect(&capture, spa::utils::Direction::Input);

            let pump = |secs: f64, mut each: Box<dyn FnMut() + '_>| {
                let deadline = Instant::now() + Duration::from_secs_f64(secs);
                while Instant::now() < deadline {
                    mainloop
                        .loop_()
                        .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(2)));
                    each();
                }
            };
            let stereo = |graph: &Graph, node: &str, direction| {
                ["FL", "FR"]
                    .iter()
                    .all(|c| graph.channel_port(node, c, direction).is_some())
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !engine.with_graph(|g| {
                stereo(g, &gen, Direction::Out)
                    && stereo(g, &send_node, Direction::In)
                    && stereo(g, &recv_node, Direction::Out)
                    && stereo(g, &cap, Direction::In)
            }) {
                assert!(Instant::now() < deadline, "test nodes did not appear");
                pump(0.05, Box::new(|| ()));
            }
            let mut links = Vec::new();
            for (out_node, in_node) in [(&gen, &send_node), (&recv_node, &cap)] {
                for channel in ["FL", "FR"] {
                    let (output, input) = engine
                        .with_graph(|g| {
                            g.channel_port(out_node, channel, Direction::Out)
                                .zip(g.channel_port(in_node, channel, Direction::In))
                        })
                        .unwrap();
                    links.push(engine.link(output, input).unwrap());
                }
            }

            // Push in 5 ms packets, paced by the wall clock times `speed`
            // from `base` (time, position).
            let pushed = Cell::new(0u64);
            let push_due = |base: (Instant, u64), speed: f64| {
                let due = base.1 + (base.0.elapsed().as_secs_f64() * 48_000.0 * speed) as u64;
                while pushed.get() + NET_MAX_FRAMES as u64 <= due {
                    let at = pushed.get();
                    let packet: Vec<i16> = (at..at + NET_MAX_FRAMES as u64)
                        .flat_map(|n| {
                            let s = (tone(n) * f32::from(i16::MAX)) as i16;
                            [s, s]
                        })
                        .collect();
                    port.push(at, &packet);
                    pushed.set(at + NET_MAX_FRAMES as u64);
                }
            };
            let base = (Instant::now(), 0);
            pump(3.0, Box::new(|| push_due(base, 1.0)));
            let played = f32::from_bits(peak.swap(0, Ordering::Relaxed));
            let blocks = sent.load(Ordering::Relaxed);
            let stats = port.stats();
            println!("1.0: peak {played}, {blocks} blocks sent, {stats:?}");

            let base = (Instant::now(), pushed.get());
            let mut fills = Vec::new();
            let mut last = Instant::now();
            pump(
                40.0,
                Box::new(|| {
                    push_due(base, 1.002);
                    if last.elapsed() >= Duration::from_secs(1) {
                        last = Instant::now();
                        let ratio = f64::from_bits(port.state.ratio.load(Ordering::Relaxed));
                        let stats = port.stats();
                        println!("1.002: {:.2} ms, ratio {ratio:.5}", stats.buffered_ms);
                        fills.push(stats.buffered_ms);
                    }
                }),
            );
            let ratio = f64::from_bits(port.state.ratio.load(Ordering::Relaxed));
            let drift_peak = f32::from_bits(peak.load(Ordering::Relaxed));
            let dropouts = port.stats().dropouts;

            drop(links);
            drop(port);
            drop(audio);
            let deadline = Instant::now() + Duration::from_secs(3);
            while test_objects(&format!("{prefix}.")) > 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            let left = test_objects(&format!("{prefix}."));

            assert!((0.4..0.6).contains(&played), "received peak {played}");
            assert!(blocks > 100, "send blocks {blocks}");
            assert!(stats.receiving && stats.sending, "{stats:?}");
            let tail = &fills[fills.len() - 10..];
            assert!(
                tail.iter().all(|f| (f - 12.0).abs() < 1.5),
                "fill not held at target: {tail:?}"
            );
            assert!(ratio < 1.0 && ratio > 0.997, "ratio {ratio}");
            assert!(drift_peak > 0.4, "drift phase peak {drift_peak}");
            assert_eq!(dropouts, 0);
            assert_eq!(left, 0, "net nodes outlived their port");
        }
    }
}
