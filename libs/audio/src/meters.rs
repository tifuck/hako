//! Live peak meters: passive PipeWire capture streams on their own thread
//! and connection, alive only while someone subscribes.
//!
//! The streams never sit in a signal path: `node.passive` keeps them from
//! waking a suspended device, no `node.latency` is requested (the graph
//! quantum stays as it is), and audioconvert hands over peaks
//! (`resample.peaks`) that the process callback folds into atomics.

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tracing::{debug, warn};

/// How often a missing target, a failed stream or a lost PipeWire
/// connection is retried while subscribed.
const RETRY: Duration = Duration::from_secs(1);

/// A node to meter, by `node.name`. `monitor_sink` meters what a sink
/// plays (its monitor ports) instead of capturing from a source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MeterTarget {
    pub node_name: String,
    pub monitor_sink: bool,
}

/// Highest |sample| per channel since the last `take`, as f32 bits.
/// Non-negative floats order like their bit patterns, so `fetch_max` works.
#[derive(Debug, Default)]
struct Peaks([AtomicU32; 2]);

impl Peaks {
    fn hold(&self, channel: usize, peak: f32) {
        // `> 0.0` also drops NaN.
        if peak > 0.0 {
            self.0[channel].fetch_max(peak.min(f32::MAX).to_bits(), Ordering::Relaxed);
        }
    }

    fn take(&self) -> [f32; 2] {
        self.0
            .each_ref()
            .map(|p| f32::from_bits(p.swap(0, Ordering::Relaxed)))
    }
}

enum Cmd {
    Add(MeterTarget, Arc<Peaks>),
    Remove(MeterTarget, Arc<Peaks>),
    Quit,
}

struct Worker {
    tx: pw::channel::Sender<Cmd>,
    subscriptions: usize,
}

/// Meter streams for any number of subscribers. Free until the first
/// `subscribe`: the PipeWire thread runs only while a subscription lives.
#[derive(Default)]
pub struct MeterService {
    worker: Arc<Mutex<Option<Worker>>>,
}

impl MeterService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start metering `targets`. Identical targets across subscriptions share
    /// one stream; a target that does not exist yet is metered once it appears.
    pub fn subscribe(&self, targets: Vec<MeterTarget>) -> MeterSubscription {
        let peaks: Vec<Arc<Peaks>> = targets.iter().map(|_| Arc::default()).collect();
        let mut worker = lock(&self.worker);
        let w = worker.get_or_insert_with(spawn_worker);
        w.subscriptions += 1;
        for (target, peaks) in targets.iter().zip(&peaks) {
            let _ = w.tx.send(Cmd::Add(target.clone(), peaks.clone()));
        }
        MeterSubscription {
            worker: self.worker.clone(),
            targets,
            peaks,
        }
    }
}

/// Dropping it removes its streams (the last one stops the thread).
pub struct MeterSubscription {
    worker: Arc<Mutex<Option<Worker>>>,
    targets: Vec<MeterTarget>,
    peaks: Vec<Arc<Peaks>>,
}

impl MeterSubscription {
    /// Per target, in subscription order: the highest peak (linear, 0..1
    /// for an unclipped signal; L/R) since the previous read. Silence and
    /// targets not metered right now read 0.
    pub fn read(&self) -> Vec<[f32; 2]> {
        self.peaks.iter().map(|p| p.take()).collect()
    }
}

impl Drop for MeterSubscription {
    fn drop(&mut self) {
        let mut worker = lock(&self.worker);
        let Some(w) = worker.as_mut() else {
            return;
        };
        for (target, peaks) in self.targets.drain(..).zip(self.peaks.drain(..)) {
            let _ = w.tx.send(Cmd::Remove(target, peaks));
        }
        w.subscriptions -= 1;
        if w.subscriptions == 0 {
            let _ = w.tx.send(Cmd::Quit);
            *worker = None;
        }
    }
}

fn lock(worker: &Mutex<Option<Worker>>) -> MutexGuard<'_, Option<Worker>> {
    worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn spawn_worker() -> Worker {
    let (tx, rx) = pw::channel::channel();
    let spawned = std::thread::Builder::new()
        .name("hako-meters".into())
        .spawn(move || {
            if let Err(e) = run(rx) {
                warn!("meters: {e}");
            }
        });
    if let Err(e) = spawned {
        warn!("meters: cannot start thread: {e}");
    }
    Worker {
        tx,
        subscriptions: 0,
    }
}

/// One stream and everyone reading it.
#[derive(Default)]
struct Meter {
    readers: Rc<RefCell<Vec<Arc<Peaks>>>>,
    live: Option<Live>,
}

/// Fields drop in order: the listener is unhooked before the stream goes.
struct Live {
    _listener: pw::stream::StreamListener<()>,
    _stream: pw::stream::StreamRc,
    failed: Rc<Cell<bool>>,
}

struct Conn {
    _core_listener: pw::core::Listener,
    _registry_listener: pw::registry::Listener,
    _registry: pw::registry::RegistryRc,
    core: pw::core::CoreRc,
    /// Set by the core error event (PipeWire went away).
    dead: Rc<Cell<bool>>,
}

#[derive(Default)]
struct Engine {
    conn: Option<Conn>,
    /// Node id → `node.name`, from the registry.
    nodes: HashMap<u32, String>,
    meters: HashMap<MeterTarget, Meter>,
}

type Shared = Rc<RefCell<Engine>>;

fn run(rx: pw::channel::Receiver<Cmd>) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let engine: Shared = Rc::default();

    let _rx = rx.attach(mainloop.loop_(), {
        let (engine, context, mainloop) = (engine.clone(), context.clone(), mainloop.clone());
        move |cmd| {
            {
                let mut e = engine.borrow_mut();
                match cmd {
                    Cmd::Add(target, peaks) => {
                        e.meters
                            .entry(target)
                            .or_default()
                            .readers
                            .borrow_mut()
                            .push(peaks);
                    }
                    Cmd::Remove(target, peaks) => {
                        if let Some(meter) = e.meters.get_mut(&target) {
                            meter
                                .readers
                                .borrow_mut()
                                .retain(|p| !Arc::ptr_eq(p, &peaks));
                            if meter.readers.borrow().is_empty() {
                                e.meters.remove(&target);
                            }
                        }
                    }
                    Cmd::Quit => {
                        mainloop.quit();
                        return;
                    }
                }
            }
            tick(&engine, &context);
        }
    });
    let timer = mainloop.loop_().add_timer({
        let (engine, context) = (engine.clone(), context.clone());
        move |_| tick(&engine, &context)
    });
    timer
        .update_timer(Some(RETRY), Some(RETRY))
        .into_sync_result()?;

    mainloop.run();
    // Streams and the connection go before the context and loop.
    let mut e = engine.borrow_mut();
    e.meters.clear();
    e.conn = None;
    Ok(())
}

/// Bring the connection and streams in line with what is wanted.
fn tick(engine: &Shared, context: &pw::context::ContextRc) {
    let mut e = engine.borrow_mut();
    if e.conn.as_ref().is_some_and(|c| c.dead.get()) {
        debug!("meters: PipeWire connection lost; reconnecting");
        disconnect(&mut e);
    }
    if e.meters.is_empty() {
        return;
    }
    if e.conn.is_none() {
        match connect(context, Rc::downgrade(engine)) {
            Ok(conn) => e.conn = Some(conn),
            Err(err) => {
                debug!("meters: cannot connect to PipeWire: {err}");
                return;
            }
        }
    }
    for meter in e.meters.values_mut() {
        if meter.live.as_ref().is_some_and(|l| l.failed.get()) {
            meter.live = None;
        }
    }
    sync(&mut e);
}

fn disconnect(e: &mut Engine) {
    for meter in e.meters.values_mut() {
        meter.live = None;
    }
    e.nodes.clear();
    e.conn = None;
}

/// Streams for targets that exist, none for targets that are gone.
fn sync(e: &mut Engine) {
    let Engine {
        conn,
        nodes,
        meters,
    } = e;
    let Some(conn) = conn.as_ref() else {
        return;
    };
    for (target, meter) in meters.iter_mut() {
        let present = nodes.values().any(|n| *n == target.node_name);
        if !present {
            meter.live = None;
        } else if meter.live.is_none() {
            match open_stream(&conn.core, target, &meter.readers) {
                Ok(live) => meter.live = Some(live),
                Err(err) => warn!("meters: stream for {}: {err}", target.node_name),
            }
        }
    }
}

fn connect(
    context: &pw::context::ContextRc,
    engine: Weak<RefCell<Engine>>,
) -> Result<Conn, pw::Error> {
    let core = context.connect_rc(None)?;
    let dead = Rc::new(Cell::new(false));
    let core_listener = core
        .add_listener_local()
        .error({
            let dead = dead.clone();
            move |id, _seq, res, message| {
                if id == pw::core::PW_ID_CORE {
                    debug!("meters: core error {res}: {message}");
                    dead.set(true);
                }
            }
        })
        .register();
    let registry = core.get_registry_rc()?;
    let registry_listener = registry
        .add_listener_local()
        .global({
            let engine = engine.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(name) = global.props.and_then(|p| p.get("node.name")) else {
                    return;
                };
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                let mut e = engine.borrow_mut();
                e.nodes.insert(global.id, name.to_string());
                if e.meters.keys().any(|t| t.node_name == name) {
                    sync(&mut e);
                }
            }
        })
        .global_remove(move |id| {
            let Some(engine) = engine.upgrade() else {
                return;
            };
            let mut e = engine.borrow_mut();
            if e.nodes.remove(&id).is_some() {
                sync(&mut e);
            }
        })
        .register();
    Ok(Conn {
        _core_listener: core_listener,
        _registry_listener: registry_listener,
        _registry: registry,
        core,
        dead,
    })
}

fn stream_props(target: &MeterTarget) -> pw::properties::PropertiesBox {
    let mut props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "node.name" => "hako-meter",
        "node.description" => "hako meter",
        "stream.monitor" => "true",
        "resample.peaks" => "true",
        "node.passive" => "true",
        "node.dont-reconnect" => "true",
        // Only opened once the target exists; never meter the default instead.
        "node.dont-fallback" => "true",
        "target.object" => target.node_name.as_str(),
        // WirePlumber 0.4 (Ubuntu 22.04) reads the older key.
        "node.target" => target.node_name.as_str(),
    };
    if target.monitor_sink {
        props.insert("stream.capture.sink", "true");
    }
    props
}

/// F32 interleaved stereo at the graph rate; a mono node is upmixed.
pub(crate) fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_channels(2);
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

fn open_stream(
    core: &pw::core::CoreRc,
    target: &MeterTarget,
    readers: &Rc<RefCell<Vec<Arc<Peaks>>>>,
) -> Result<Live, pw::Error> {
    let stream = pw::stream::StreamRc::new(core.clone(), "hako-meter", stream_props(target))?;
    let failed = Rc::new(Cell::new(false));
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let (failed, name) = (failed.clone(), target.node_name.clone());
            move |_, _, _, state| {
                if let pw::stream::StreamState::Error(err) = state {
                    debug!("meters: stream for {name} failed: {err}");
                    failed.set(true);
                }
            }
        })
        .process({
            let readers = readers.clone();
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
                let peak = stereo_peak(&bytes[..size.min(bytes.len())]);
                for r in readers.borrow().iter() {
                    r.hold(0, peak[0]);
                    r.hold(1, peak[1]);
                }
            }
        })
        .register()?;
    let pod = format_pod();
    let mut params = [spa::pod::Pod::from_bytes(&pod).expect("format pod parses")];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    debug!("meters: metering {}", target.node_name);
    Ok(Live {
        _listener: listener,
        _stream: stream,
        failed,
    })
}

/// Highest |sample| of each channel in interleaved F32LE stereo.
fn stereo_peak(bytes: &[u8]) -> [f32; 2] {
    let mut peak = [0f32; 2];
    for frame in bytes.as_chunks::<8>().0 {
        for (channel, sample) in frame.as_chunks::<4>().0.iter().enumerate() {
            peak[channel] = peak[channel].max(f32::from_le_bytes(*sample).abs());
        }
    }
    peak
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_hold_the_maximum_until_read() {
        let p = Peaks::default();
        for v in [0.25, 0.75, 0.5] {
            p.hold(0, v);
        }
        p.hold(1, f32::NAN);
        p.hold(1, 0.1);
        assert_eq!(p.take(), [0.75, 0.1]);
        assert_eq!(p.take(), [0.0, 0.0]);
    }

    #[test]
    fn stereo_peak_reads_interleaved_channels() {
        let bytes: Vec<u8> = [0.1f32, -0.9, -0.4, 0.2]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(stereo_peak(&bytes), [0.4, 0.9]);
        // A torn trailing frame is ignored.
        assert_eq!(stereo_peak(&bytes[..12]), [0.1, 0.9]);
    }

    fn meter_nodes() -> usize {
        let dump = std::process::Command::new("pw-dump").output().unwrap();
        String::from_utf8_lossy(&dump.stdout)
            .matches(r#""node.name": "hako-meter""#)
            .count()
    }

    /// Highest reading over `secs` seconds of 30 Hz reads.
    fn loudest(sub: &MeterSubscription, secs: f32) -> [f32; 2] {
        let mut max = [0f32; 2];
        for _ in 0..(secs * 30.0) as usize {
            std::thread::sleep(Duration::from_millis(33));
            let [l, r] = sub.read()[0];
            max = [max[0].max(l), max[1].max(r)];
        }
        max
    }

    /// Subscribes before the target exists, then creates a null sink and
    /// plays a -6 dBFS tone into it: the monitor meter must read ~0.5, and
    /// no meter node may outlive the subscription.
    #[test]
    #[ignore = "needs a live PipeWire session (pw-cli, pw-play, pw-dump)"]
    fn meters_a_sink_that_appears_later() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let sink = format!("hako-test-sink-{}", std::process::id());
        let service = MeterService::new();
        let sub = service.subscribe(vec![MeterTarget {
            node_name: sink.clone(),
            monitor_sink: true,
        }]);
        assert_eq!(loudest(&sub, 1.0), [0.0, 0.0]);
        assert_eq!(meter_nodes(), 0, "no stream while the target is missing");

        let mut cli = Command::new("pw-cli")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        writeln!(
            cli.stdin.as_mut().unwrap(),
            "create-node adapter {{ factory.name=support.null-audio-sink node.name={sink} \
             media.class=Audio/Sink audio.position=[FL FR] object.linger=false }}"
        )
        .unwrap();

        let wav = std::env::temp_dir().join(format!("{sink}.wav"));
        let rate = 48_000u32;
        let samples: Vec<i16> = (0..rate * 3)
            .flat_map(|n| {
                let s = (n as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin();
                let s = (s * 0.5 * i16::MAX as f32) as i16;
                [s, s]
            })
            .collect();
        let mut file = Vec::new();
        let data_len = samples.len() as u32 * 2;
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(36 + data_len).to_le_bytes());
        file.extend_from_slice(b"WAVEfmt ");
        for v in [16u32, 1 | (2 << 16), rate, rate * 4, 4 | (16 << 16)] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(b"data");
        file.extend_from_slice(&data_len.to_le_bytes());
        file.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        std::fs::write(&wav, file).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let mut tone = Command::new("pw-play")
            .args([
                "--target",
                &sink,
                "--volume",
                "1",
                "-P",
                "{ node.name=hako-test-tone state.restore-props=false }",
            ])
            .arg(&wav)
            .spawn()
            .unwrap();

        let peak = loudest(&sub, 2.0);
        let nodes = meter_nodes();
        drop(sub);
        std::thread::sleep(Duration::from_millis(500));
        let after = meter_nodes();
        let _ = tone.kill();
        let _ = tone.wait();
        let _ = cli.kill();
        let _ = cli.wait();
        let _ = std::fs::remove_file(&wav);

        assert_eq!(nodes, 1, "one meter stream while subscribed");
        for p in peak {
            assert!((0.45..0.55).contains(&p), "peak {peak:?}");
        }
        assert_eq!(after, 0, "meter stream outlived its subscription");
    }
}
