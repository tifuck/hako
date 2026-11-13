//! `hakod --latency-test`: how much latency the AUX processing chain adds
//! over a direct link, measured in software on the live PipeWire graph.
//!
//! A generator stream plays short noise bursts into the default AUX chain
//! (gate, EQ, compressor; the same `generate_dsp_config` output in a
//! `pipewire -c` child, as AUX runs it) and straight into channel 1 of one
//! capture stream; the chain's output feeds channel 2. Both channels arrive
//! in the same buffers, so the offset between them is the added latency.
//!
//! No test audio reaches a device: every node is `hako-test-latency-<pid>-*`,
//! none autoconnects, links are this process's own (they die with it), and
//! the generator stays silent until the registry shows that only those links
//! touch the test nodes. A foreign link at any point aborts the run.

use crate::engine::{engine, Direction, Graph, LinkHandle};
use crate::{
    ensure_running, find_lsp_ladspa, generate_dsp_config, ladspa_dirs, meters, node_latency,
    stop_child, LSP_LADSPA, NODE_WAIT, PLAYBACK,
};
use anyhow::{anyhow, bail, Context, Result};
use hako_core::ipc::{AuxEffect, AuxEffectChain, AuxEffectKind};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::collections::HashSet;
use std::fmt;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Samples from one burst onset to the next (500 ms at 48 kHz).
const PERIOD: u64 = 24_000;
/// Burst length (10 ms at 48 kHz): white noise has a sharp onset and a single
/// correlation peak, where a tone's repeats every period and a lone click
/// would be eaten by the gate.
const BURST: usize = 480;
/// Peak level of the noise: -6 dBFS.
const AMPLITUDE: f32 = 0.5;
/// Widest offset searched: PipeWire's default `clock.quantum-limit`.
const MAX_LAG: usize = 8192;
/// A reference sample above this starts a burst.
const ONSET: f32 = 1e-4;
/// A burst whose best normalised correlation is lower was lost in the chain.
const MIN_CORRELATION: f32 = 0.5;
const MIN_BURSTS: usize = 3;
/// How long the unlinked test nodes are watched before any audio flows
/// (WirePlumber acts on new nodes within a few ms).
const SETTLE: Duration = Duration::from_millis(500);
const STRIDE: usize = 8;

#[derive(Debug, Clone, Copy)]
pub struct LatencyTestOptions {
    /// `node.latency` in samples at 48 kHz (an `audio_latency` preset).
    pub latency: u32,
    pub seconds: u32,
}

#[derive(Debug, Clone, Default)]
pub struct LatencyReport {
    pub requested: u32,
    /// Smallest and largest capture buffer seen while measuring, in frames.
    pub quantum: Option<(u32, u32)>,
    /// Graph clock rate.
    pub rate: Option<u32>,
    /// The driver the test nodes ran on (`node.name`).
    pub driver: Option<String>,
    /// Processed minus direct onset, per burst, in samples.
    pub offsets: Vec<i64>,
    /// Bursts recorded but not found in the processed channel.
    pub lost: usize,
    pub links: usize,
    pub async_links: usize,
    /// Links whose info never arrived, so `link.async` is unknown.
    pub unreported_links: usize,
}

impl LatencyReport {
    /// Quantum the added latency is measured against.
    pub fn quantum(&self) -> u32 {
        self.quantum.map_or(self.requested, |(_, max)| max)
    }

    pub fn median(&self) -> Option<i64> {
        let mut sorted = self.offsets.clone();
        sorted.sort_unstable();
        sorted.get(sorted.len().saturating_sub(1) / 2).copied()
    }

    /// `Err(reason)` unless every burst came through within one quantum
    /// and no link is async.
    pub fn verdict(&self) -> Result<(), String> {
        let quantum = i64::from(self.quantum());
        if self.offsets.len() < MIN_BURSTS {
            return Err(format!(
                "only {} of {} bursts came through the chain",
                self.offsets.len(),
                self.offsets.len() + self.lost
            ));
        }
        if self.async_links > 0 {
            return Err(format!("{} link(s) are async", self.async_links));
        }
        if self.unreported_links > 0 {
            return Err(format!(
                "no link info for {} link(s); link.async unknown",
                self.unreported_links
            ));
        }
        let (min, max) = (self.offsets.iter().min(), self.offsets.iter().max());
        if let (Some(&min), Some(&max)) = (min, max) {
            if min < 0 {
                return Err(format!(
                    "processed audio arrived {} samples before the direct path",
                    -min
                ));
            }
            if max > quantum {
                return Err(format!(
                    "added latency {max} samples exceeds one quantum ({quantum})"
                ));
            }
        }
        Ok(())
    }

    fn ms(&self, samples: i64) -> String {
        self.rate.map_or_else(String::new, |rate| {
            format!(" = {:.2} ms", samples as f64 * 1000.0 / f64::from(rate))
        })
    }
}

impl fmt::Display for LatencyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "hako latency test: AUX chain (gate, EQ, compressor) vs a direct link"
        )?;
        writeln!(
            f,
            "  requested  node.latency {}",
            node_latency(self.requested)
        )?;
        let rate = self
            .rate
            .map_or("unknown rate".into(), |r| format!("{r} Hz"));
        match self.quantum {
            Some((min, max)) if min != max => {
                writeln!(f, "  graph      quantum {min}..{max} samples at {rate}")?
            }
            Some((_, max)) => writeln!(f, "  graph      quantum {max} samples at {rate}")?,
            None => writeln!(f, "  graph      no buffers captured ({rate})")?,
        }
        if let Some(driver) = &self.driver {
            writeln!(f, "  driver     {driver}")?;
        }
        match (
            self.median(),
            self.offsets.iter().min(),
            self.offsets.iter().max(),
        ) {
            (Some(median), Some(&min), Some(&max)) => writeln!(
                f,
                "  added      {median} samples{} (median of {} bursts; min {min}, max {max}{})",
                self.ms(median),
                self.offsets.len(),
                if self.lost > 0 {
                    format!("; {} lost", self.lost)
                } else {
                    String::new()
                }
            )?,
            _ => writeln!(f, "  added      no burst came through ({} lost)", self.lost)?,
        }
        write!(
            f,
            "  links      {} created, {} async",
            self.links, self.async_links
        )?;
        if self.unreported_links > 0 {
            write!(f, " ({} sent no link info)", self.unreported_links)?;
        }
        writeln!(f)?;
        match self.verdict() {
            Ok(()) => writeln!(
                f,
                "  result     PASS: within one quantum ({})",
                self.quantum()
            ),
            Err(reason) => writeln!(f, "  result     FAIL: {reason}"),
        }
    }
}

/// The chain the UI offers by default, every stage enabled.
fn default_chain() -> AuxEffectChain {
    AuxEffectChain {
        effects: [
            ("gate", AuxEffectKind::default_gate()),
            ("eq", AuxEffectKind::default_equalizer()),
            ("compressor", AuxEffectKind::default_compressor()),
        ]
        .map(|(id, kind)| AuxEffect {
            id: id.into(),
            enabled: true,
            kind,
        })
        .into(),
    }
}

/// The filter-chain child; dropping it stops the process and removes its config.
struct Dsp {
    child: Child,
    conf: PathBuf,
}

impl Dsp {
    fn spawn(sink: &str, latency: u32) -> Result<Self> {
        let dirs = ladspa_dirs(std::env::var_os("LADSPA_PATH").as_deref());
        let lsp = find_lsp_ladspa(&dirs)
            .ok_or_else(|| anyhow!("LSP LADSPA plugins not installed ({LSP_LADSPA} not found)"))?;
        let config = generate_dsp_config(sink, None, &default_chain(), latency, Some(&lsp))?;
        let conf = hako_core::default_socket_path().with_file_name(format!("{sink}.conf"));
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&conf)
            .and_then(|mut f| f.write_all(config.as_bytes()))
            .with_context(|| format!("write {}", conf.display()))?;
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&conf)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => Ok(Self { child, conf }),
            Err(error) => {
                let _ = std::fs::remove_file(&conf);
                Err(error).context("spawn the test filter graph")
            }
        }
    }
}

impl Drop for Dsp {
    fn drop(&mut self) {
        let _ = stop_child(&mut self.child);
        let _ = std::fs::remove_file(&self.conf);
    }
}

/// White-noise bursts, `BURST` samples every `PERIOD`, starting half a
/// period in. Deterministic (xorshift32).
struct Bursts {
    position: u64,
    rng: u32,
}

impl Default for Bursts {
    fn default() -> Self {
        Self {
            position: 0,
            rng: 0x9e37_79b9,
        }
    }
}

impl Bursts {
    fn next_sample(&mut self) -> f32 {
        let phase = (self.position % PERIOD) as usize;
        self.position += 1;
        if !(PERIOD as usize / 2..PERIOD as usize / 2 + BURST).contains(&phase) {
            return 0.0;
        }
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng as f32 / u32::MAX as f32 * 2.0 - 1.0) * AMPLITUDE
    }
}

#[derive(Default)]
struct Recording {
    reference: Vec<f32>,
    processed: Vec<f32>,
    quantum: Option<(u32, u32)>,
    rate: Option<u32>,
}

/// Fields drop in order: the listener is unhooked before the stream goes.
struct TestStream<D> {
    _listener: pw::stream::StreamListener<D>,
    _stream: pw::stream::StreamRc,
}

fn stream_props(name: &str, category: &str, latency: &str) -> pw::properties::PropertiesBox {
    properties! {
        "media.type" => "Audio",
        "media.category" => category,
        "node.name" => name,
        "node.description" => "hako latency test",
        "node.latency" => latency,
        // Linked by this test only; WirePlumber must route it nowhere.
        "node.autoconnect" => "false",
        "node.dont-reconnect" => "true",
        "node.dont-fallback" => "true",
        "state.restore-props" => "false",
        // Run on the idle software driver, never on the user's running
        // device (whose quantum `node.latency` would otherwise change).
        "node.group" => "pipewire.dummy",
    }
}

fn connect(stream: &pw::stream::StreamRc, direction: spa::utils::Direction) -> Result<()> {
    let pod = meters::format_pod();
    let mut params = [spa::pod::Pod::from_bytes(&pod).expect("format pod parses")];
    // No AUTOCONNECT flag either. RT_PROCESS keeps the node synchronous: a
    // stream processing on the main loop is `node.async` (PipeWire 1.x), and
    // so is every link it has.
    stream.connect(
        direction,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    Ok(())
}

/// Stereo F32 bursts (silence until `go`), whole buffers at a time. The
/// callbacks run on the data loop thread.
fn generator(
    core: &pw::core::CoreRc,
    name: &str,
    latency: &str,
    go: Arc<AtomicBool>,
) -> Result<TestStream<Bursts>> {
    let stream =
        pw::stream::StreamRc::new(core.clone(), name, stream_props(name, "Playback", latency))?;
    let listener = stream
        .add_local_listener_with_user_data(Bursts::default())
        .process(move |stream, bursts| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let mut frames = 0;
            if let Some(bytes) = data.data() {
                for frame in bytes.as_chunks_mut::<STRIDE>().0 {
                    let sample = if go.load(Ordering::Relaxed) {
                        bursts.next_sample()
                    } else {
                        *bursts = Bursts::default();
                        0.0
                    };
                    let sample = sample.to_le_bytes();
                    frame[..4].copy_from_slice(&sample);
                    frame[4..].copy_from_slice(&sample);
                    frames += 1;
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = STRIDE as _;
            *chunk.size_mut() = (frames * STRIDE) as _;
        })
        .register()?;
    connect(&stream, spa::utils::Direction::Output)?;
    Ok(TestStream {
        _listener: listener,
        _stream: stream,
    })
}

/// Records both channels (reference, processed) while `go`.
fn capture(
    core: &pw::core::CoreRc,
    name: &str,
    latency: &str,
    go: Arc<AtomicBool>,
    recording: Arc<Mutex<Recording>>,
) -> Result<TestStream<()>> {
    let stream =
        pw::stream::StreamRc::new(core.clone(), name, stream_props(name, "Capture", latency))?;
    let listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
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
            if !go.load(Ordering::Relaxed) {
                return;
            }
            let frames = bytes[..size.min(bytes.len())].as_chunks::<STRIDE>().0;
            // Uncontended: the main thread reads it once the run is over.
            let mut rec = recording.lock().unwrap_or_else(PoisonError::into_inner);
            if !frames.is_empty() {
                let n = frames.len() as u32;
                rec.quantum = Some(
                    rec.quantum
                        .map_or((n, n), |(lo, hi)| (lo.min(n), hi.max(n))),
                );
            }
            if let Ok(time) = stream.time() {
                let rate = time.rate();
                if rate.num == 1 && rate.denom > 0 {
                    rec.rate = Some(rate.denom);
                }
            }
            // ponytail: the whole run is kept in memory (~23 MB a minute,
            // reserved up front so the data loop rarely allocates); estimate
            // per burst as it arrives if runs get much longer.
            for frame in frames {
                let [left, right] = frame.as_chunks::<4>().0 else {
                    continue;
                };
                rec.reference.push(f32::from_le_bytes(*left));
                rec.processed.push(f32::from_le_bytes(*right));
            }
        })
        .register()?;
    connect(&stream, spa::utils::Direction::Input)?;
    Ok(TestStream {
        _listener: listener,
        _stream: stream,
    })
}

/// Links touching a node named `prefix*` other than `ours`, as
/// `"node -> node"` (by node id from the link's own props, so a link is
/// caught even before its ports are cached).
fn foreign_links(graph: &Graph, prefix: &str, ours: &[u32]) -> Vec<String> {
    let name = |id: Option<&String>| {
        id.and_then(|id| id.parse::<u32>().ok())
            .and_then(|id| graph.nodes.get(&id))
            .map_or("?", |node| node.name.as_str())
    };
    graph
        .links
        .iter()
        .filter(|(id, _)| !ours.contains(id))
        .filter_map(|(_, link)| {
            let ends = [
                name(link.props.get("link.output.node")),
                name(link.props.get("link.input.node")),
            ];
            ends.iter()
                .any(|end| end.starts_with(prefix))
                .then(|| format!("{} -> {}", ends[0], ends[1]))
        })
        .collect()
}

/// The test's PipeWire loop plus what every wait must keep checking.
struct Session<'a> {
    mainloop: pw::main_loop::MainLoopRc,
    dsp: Dsp,
    stop: &'a AtomicBool,
}

impl Session<'_> {
    /// Run the loop until `ready` holds for the registry cache or `timeout`
    /// passes; fails at once on Ctrl-C / SIGTERM or when the DSP child exits.
    fn pump(
        &mut self,
        timeout: Duration,
        mut ready: impl FnMut(&Graph) -> Result<bool>,
    ) -> Result<bool> {
        let engine = engine()?;
        let deadline = Instant::now() + timeout;
        loop {
            self.mainloop
                .loop_()
                .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
            if self.stop.load(Ordering::Relaxed) {
                bail!("interrupted");
            }
            ensure_running(&mut self.dsp.child, "test filter graph")?;
            if engine.with_graph(|graph| ready(graph))? {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }
}

/// Measure, then tear everything down (on every path: streams and links drop
/// with this frame, the DSP child with `Session`). `stop` aborts the run.
pub fn run_latency_test(opts: &LatencyTestOptions, stop: &AtomicBool) -> Result<LatencyReport> {
    let engine = engine()?;
    let prefix = format!("hako-test-latency-{}-", std::process::id());
    let [gen, cap, dsp] = ["gen", "capture", "dsp"].map(|part| format!("{prefix}{part}"));
    let dsp_out = format!("{dsp}_out");
    let latency = node_latency(opts.latency);

    pw::init();
    let mut session = Session {
        mainloop: pw::main_loop::MainLoopRc::new(None)?,
        dsp: Dsp::spawn(&dsp, opts.latency)?,
        stop,
    };
    let context = pw::context::ContextRc::new(&session.mainloop, None)?;
    let core = context.connect_rc(None)?;
    let go = Arc::new(AtomicBool::new(false));
    let samples = opts.seconds as usize * 48_000;
    let recording = Arc::new(Mutex::new(Recording {
        reference: Vec::with_capacity(samples),
        processed: Vec::with_capacity(samples),
        ..Recording::default()
    }));
    let _generator = generator(&core, &gen, &latency, go.clone())?;
    let _capture = capture(&core, &cap, &latency, go.clone(), recording.clone())?;

    const OUTPUTS: [&str; 2] = ["output_FL", "output_FR"];
    let ready = session.pump(NODE_WAIT, |graph| {
        Ok(graph.has_ports(&gen, Direction::Out, &OUTPUTS)
            && graph.has_ports(&cap, Direction::In, &["input_FL", "input_FR"])
            && graph.has_ports(&dsp, Direction::In, &PLAYBACK)
            && graph.has_ports(&dsp_out, Direction::Out, &OUTPUTS))
    })?;
    if !ready {
        bail!("the test nodes did not appear");
    }
    let autoconnecting: Vec<String> = engine.with_graph(|graph| {
        [&gen, &cap, &dsp_out]
            .into_iter()
            .filter(|name| {
                graph.node_id(name).is_none_or(|id| {
                    graph.nodes[&id]
                        .props
                        .get("node.autoconnect")
                        .map(String::as_str)
                        != Some("false")
                })
            })
            .cloned()
            .collect()
    });
    if !autoconnecting.is_empty() {
        bail!("not node.autoconnect=false: {}", autoconnecting.join(", "));
    }
    let isolated = |ours: &[u32]| {
        let ours = ours.to_vec();
        let prefix = prefix.clone();
        move |graph: &Graph| -> Result<bool> {
            let foreign = foreign_links(graph, &prefix, &ours);
            if !foreign.is_empty() {
                bail!(
                    "aborted: links the test did not make: {}",
                    foreign.join(", ")
                );
            }
            Ok(false)
        }
    };
    session.pump(SETTLE, isolated(&[]))?;

    let pairs = [
        ((&gen, "output_FL"), (&dsp, "playback_FL")),
        ((&gen, "output_FR"), (&dsp, "playback_FR")),
        ((&gen, "output_FL"), (&cap, "input_FL")),
        ((&dsp_out, "output_FL"), (&cap, "input_FR")),
    ];
    let mut links: Vec<LinkHandle> = Vec::new();
    for ((out_node, out_port), (in_node, in_port)) in pairs {
        let ports = engine.with_graph(|graph| {
            graph
                .port(out_node, out_port, Direction::Out)
                .zip(graph.port(in_node, in_port, Direction::In))
        });
        let (output, input) =
            ports.ok_or_else(|| anyhow!("{out_node}:{out_port} or {in_node}:{in_port} is gone"))?;
        links.push(
            engine
                .link(output, input)
                .with_context(|| format!("link {out_node}:{out_port} -> {in_node}:{in_port}"))?,
        );
    }
    let ids: Vec<u32> = links.iter().map(|link| link.id).collect();
    session.pump(Duration::ZERO, isolated(&ids))?;
    go.store(true, Ordering::Relaxed);
    let check = isolated(&ids);
    let ours: HashSet<u32> = ids.iter().copied().collect();
    session.pump(Duration::from_secs(opts.seconds.into()), |graph| {
        if !ours.iter().all(|id| graph.links.contains_key(id)) {
            bail!("a test link disappeared");
        }
        check(graph)
    })?;
    go.store(false, Ordering::Relaxed);
    let driver = engine.with_graph(|graph| {
        let id = graph.node_id(&cap)?;
        let driver = graph.nodes[&id].props.get("node.driver-id")?.parse().ok()?;
        Some(graph.nodes.get(&driver)?.name.clone())
    });
    // Read once the links have run: `link.async` is in their info only when
    // true (PipeWire 1.6), and absent before async links existed (0.3.x).
    let (async_links, unreported_links) = engine.with_graph(|graph| {
        let info: Vec<_> = ids.iter().filter_map(|id| graph.links.get(id)).collect();
        (
            info.iter()
                .filter(|link| link.props.get("link.async").is_some_and(|v| v == "true"))
                .count(),
            ids.len()
                - info
                    .iter()
                    .filter(|link| link.props.contains_key("object.id"))
                    .count(),
        )
    });
    drop(links);

    let rec = std::mem::take(&mut *recording.lock().unwrap_or_else(PoisonError::into_inner));
    let found = burst_offsets(&rec.reference, &rec.processed, MAX_LAG);
    Ok(LatencyReport {
        requested: opts.latency,
        quantum: rec.quantum,
        rate: rec.rate,
        driver,
        lost: found.iter().filter(|offset| offset.is_none()).count(),
        offsets: found.into_iter().flatten().collect(),
        links: ids.len(),
        async_links,
        unreported_links,
    })
}

/// Per burst in `reference`: the lag (within ±`max_lag`) at which
/// `processed` correlates best with it, or `None` when nothing correlates
/// (the burst was lost). Bursts too close to either end are skipped.
fn burst_offsets(reference: &[f32], processed: &[f32], max_lag: usize) -> Vec<Option<i64>> {
    let len = reference.len().min(processed.len());
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let mut offsets = Vec::new();
    let mut at = max_lag;
    while let Some(onset) = reference
        .get(at..len)
        .and_then(|rest| rest.iter().position(|s| s.abs() > ONSET))
        .map(|i| at + i)
    {
        let end = onset + BURST;
        if end + max_lag > len {
            break;
        }
        let window = &reference[onset..end];
        // ponytail: direct O(BURST * lags) correlation, ~8 M multiply-adds
        // per burst; an FFT if the window or lag range grows a lot.
        let (mut best, mut best_lag) = (0.0, None);
        for lag in 0..=2 * max_lag {
            let start = onset + lag - max_lag;
            let corr = dot(window, &processed[start..start + BURST]);
            if corr > best {
                (best, best_lag) = (corr, Some(lag));
            }
        }
        offsets.push(
            best_lag
                .filter(|&lag| {
                    let start = onset + lag - max_lag;
                    let shifted = &processed[start..start + BURST];
                    let energy = dot(window, window) * dot(shifted, shifted);
                    best / energy.sqrt() >= MIN_CORRELATION
                })
                .map(|lag| lag as i64 - max_lag as i64),
        );
        at = end;
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAG: usize = 1024;

    /// The generator's own signal for `len` samples.
    fn signal(len: usize) -> Vec<f32> {
        let mut bursts = Bursts::default();
        (0..len).map(|_| bursts.next_sample()).collect()
    }

    fn delayed(signal: &[f32], by: usize) -> Vec<f32> {
        let mut out = vec![0.0; by];
        out.extend_from_slice(&signal[..signal.len() - by]);
        out
    }

    #[test]
    fn bursts_are_short_loud_and_periodic() {
        let s = signal(2 * PERIOD as usize);
        let half = PERIOD as usize / 2;
        assert!(s[..half].iter().all(|v| *v == 0.0));
        assert!(s[half..half + BURST].iter().any(|v| v.abs() > 0.4));
        assert!(s[half..half + BURST].iter().all(|v| v.abs() <= AMPLITUDE));
        assert!(s[half + BURST..half + PERIOD as usize]
            .iter()
            .all(|v| *v == 0.0));
        assert!(s[half + PERIOD as usize..].iter().any(|v| *v != 0.0));
    }

    #[test]
    fn finds_known_offsets() {
        let reference = signal(4 * PERIOD as usize);
        for by in [0, 1, 37, 256, 1000] {
            let found = burst_offsets(&reference, &delayed(&reference, by), LAG);
            assert_eq!(found.len(), 4, "offset {by}");
            assert!(
                found.iter().all(|f| *f == Some(by as i64)),
                "{by}: {found:?}"
            );
        }
    }

    #[test]
    fn a_gated_onset_and_noise_do_not_move_the_peak() {
        let reference = signal(4 * PERIOD as usize);
        let mut processed = delayed(&reference, 128);
        // Gate opening: -72 dB rising to unity over the first 2 ms of each
        // burst, then a 3:1-ish level drop; plus a -50 dBFS noise floor.
        let mut rng = 1u32;
        let mut since_onset = usize::MAX;
        for sample in processed.iter_mut() {
            since_onset = if *sample == 0.0 {
                usize::MAX
            } else {
                since_onset.wrapping_add(1)
            };
            let ramp = (since_onset.min(96) as f32 / 96.0).max(0.000_25);
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            *sample = *sample * ramp * 0.6 + (rng as f32 / u32::MAX as f32 - 0.5) * 0.006;
        }
        let found = burst_offsets(&reference, &processed, LAG);
        assert_eq!(found, vec![Some(128); 4]);
    }

    #[test]
    fn a_lost_burst_is_reported_as_lost() {
        let reference = signal(3 * PERIOD as usize);
        let mut processed = reference.clone();
        // The second burst never makes it through.
        processed[PERIOD as usize..2 * PERIOD as usize].fill(0.0);
        assert_eq!(
            burst_offsets(&reference, &processed, LAG),
            [Some(0), None, Some(0)]
        );
        // Processed audio that comes early shows as a negative offset.
        assert_eq!(
            burst_offsets(&delayed(&reference, 50), &reference, LAG)[1],
            Some(-50)
        );
    }

    #[test]
    fn verdict_is_one_quantum_and_no_async_link() {
        let report = |offsets: Vec<i64>, async_links| LatencyReport {
            requested: 256,
            quantum: Some((256, 256)),
            rate: Some(48_000),
            offsets,
            links: 4,
            async_links,
            ..LatencyReport::default()
        };
        assert!(report(vec![0, 0, 1], 0).verdict().is_ok());
        assert!(report(vec![256; 3], 0).verdict().is_ok());
        assert!(report(vec![0, 0, 257], 0).verdict().is_err());
        assert!(report(vec![0, 0, -1], 0).verdict().is_err());
        assert!(report(vec![0; 3], 1).verdict().is_err());
        assert!(report(vec![0; 2], 0).verdict().is_err());
        let unknown = LatencyReport {
            unreported_links: 1,
            ..report(vec![0; 3], 0)
        };
        assert!(unknown.verdict().is_err());
        assert_eq!(report(vec![3, 1, 2, 9], 0).median(), Some(2));
        let text = report(vec![0; 3], 0).to_string();
        assert!(text.contains("PASS"), "{text}");
        assert!(text.contains("0 samples = 0.00 ms"), "{text}");
    }

    fn test_objects() -> usize {
        let dump = Command::new("pw-dump").output().unwrap();
        String::from_utf8_lossy(&dump.stdout)
            .matches("hako-test-latency-")
            .count()
    }

    #[test]
    #[ignore = "needs a live PipeWire session and LSP plugins"]
    fn live_balanced_chain_adds_at_most_one_quantum() {
        let opts = LatencyTestOptions {
            latency: hako_core::ipc::audio_latency_samples("balanced").unwrap(),
            seconds: 5,
        };
        let report = run_latency_test(&opts, &AtomicBool::new(false)).unwrap();
        println!("{report}");
        let deadline = Instant::now() + Duration::from_secs(2);
        while test_objects() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(test_objects(), 0, "test nodes outlived the run");
        report.verdict().unwrap();
    }
}
