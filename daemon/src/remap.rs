//! Host-side remapper: evdev listen + exclusive grab + uinput inject.
//!
//! One engine serves every attached mouse / keyboard. A device is grabbed only
//! while it has bindings and no capture/record session runs; everything not
//! bound (motion, MSC, SYN, autorepeat) is forwarded 1:1 through one shared
//! virtual device. Turbo, toggle and macros run on deadlines from
//! [`Mapper::tick`], never sleeps.

use anyhow::Result;
use evdev::{
    uinput::VirtualDevice, AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode,
    MiscCode, RelativeAxisCode,
};
use hako_core::config::{load_remap_bindings, remap_config_path, save_remap_bindings, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind};
use hako_core::protocol::AdapterEvent;
use hako_core::remap::{
    clamp_interval, emittable, CaptureRole, DeviceRemaps, EvdevKey, MacroRepeat, MacroStep,
    RemapBinding, RemapCaptured, RemapMode, MAX_MACRO_STEPS, MAX_STEP_DELAY_MS,
};
use std::collections::{BTreeSet, HashMap};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

const VIRTUAL_NAME: &str = "Hako Remap";
const KEY_ESC: u16 = 1;
/// Upper bound on one poll; also the command latency.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// Retry when a key is held at grab time.
const GRAB_RETRY: Duration = Duration::from_millis(250);
const OPEN_RETRY: Duration = Duration::from_secs(2);
/// A capture / record session that nobody ends is dropped after this.
const SESSION_LIMIT: Duration = Duration::from_secs(120);

/// Commands carry an index into `EngineInner::devices` (append-only), which the
/// worker mirrors in its own slot list.
#[derive(Debug)]
enum Cmd {
    Attach {
        idx: usize,
        id: DeviceId,
        kind: DeviceKind,
        pids: Vec<u16>,
        bindings: HashMap<u16, RemapBinding>,
    },
    SetBindings {
        idx: usize,
        bindings: HashMap<u16, RemapBinding>,
    },
    StartCapture {
        idx: usize,
    },
    CancelCapture {
        idx: usize,
    },
    Shutdown,
}

struct DevState {
    id: DeviceId,
    /// Every PID the driver model enumerates as; `pids[0]` names the config file.
    pids: Vec<u16>,
    bindings: HashMap<u16, RemapBinding>,
    capturing: Option<CaptureRole>,
    capture_source: Option<EvdevKey>,
    last_error: Option<String>,
    grabbed: bool,
    recorded: Vec<MacroStep>,
    /// Bindings this version rejects: kept on disk, never applied.
    invalid: Vec<RemapBinding>,
    /// The remap file could not be read: edits are refused so it is never
    /// overwritten.
    load_error: Option<String>,
}

impl DevState {
    fn snapshot(&self) -> DeviceRemaps {
        let mut bindings: Vec<_> = self.bindings.values().cloned().collect();
        bindings.sort_by_key(|b| b.source.0);
        DeviceRemaps {
            device_id: Some(self.id.clone()),
            bindings,
            capturing: self.capturing,
            capture_source: self.capture_source,
            last_error: self.last_error.clone(),
            grabbed: self.grabbed,
            recorded: self.recorded.clone(),
        }
    }
}

struct EngineInner {
    paths: ConfigPaths,
    devices: Vec<DevState>,
}

impl EngineInner {
    fn find(&self, id: &DeviceId) -> Result<usize> {
        self.devices
            .iter()
            .position(|d| d.id == *id)
            .or_else(|| {
                self.devices
                    .iter()
                    .position(|d| claims(d.id.vendor_id, &d.pids, id))
            })
            .ok_or_else(|| anyhow::anyhow!("no remappable device {}", id.key()))
    }
}

pub struct RemapEngine {
    inner: Arc<Mutex<EngineInner>>,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl RemapEngine {
    pub fn start(paths: ConfigPaths, events: broadcast::Sender<AdapterEvent>) -> Self {
        let inner = Arc::new(Mutex::new(EngineInner {
            paths,
            devices: Vec::new(),
        }));
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            inner: inner.clone(),
            events,
            slots: Vec::new(),
            virt: None,
            mapper: Mapper::default(),
            session: None,
        };
        std::thread::Builder::new()
            .name("hako-remap".into())
            .spawn(move || worker.run(rx))
            .expect("spawn remap thread");
        Self { inner, tx }
    }

    /// Register a mouse or keyboard (idempotent; a reconnect keeps its bindings).
    /// `pids` are the driver model's PIDs, so any enumeration path is one device.
    pub fn attach(&self, id: DeviceId, kind: DeviceKind, pids: Vec<u16>) {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let idx = match g
            .devices
            .iter()
            .position(|d| claims(d.id.vendor_id, &d.pids, &id))
        {
            Some(i) => {
                g.devices[i].id = id.clone();
                i
            }
            None => {
                let (bindings, invalid, load_error) =
                    match load_bindings(&g.paths, id.vendor_id, &pids) {
                        Ok((b, invalid)) => (b, invalid, None),
                        Err(e) => {
                            warn!("{e}");
                            (HashMap::new(), Vec::new(), Some(e))
                        }
                    };
                g.devices.push(DevState {
                    id: id.clone(),
                    bindings,
                    pids: pids.clone(),
                    capturing: None,
                    capture_source: None,
                    last_error: load_error.clone(),
                    grabbed: false,
                    recorded: Vec::new(),
                    invalid,
                    load_error,
                });
                g.devices.len() - 1
            }
        };
        let bindings = g.devices[idx].bindings.clone();
        let pids = g.devices[idx].pids.clone();
        // Sent under the lock so command order matches index order.
        let _ = self.tx.send(Cmd::Attach {
            idx,
            id,
            kind,
            pids,
            bindings,
        });
    }

    pub fn snapshots(&self) -> Vec<DeviceRemaps> {
        let g = self.inner.lock().unwrap();
        g.devices.iter().map(DevState::snapshot).collect()
    }

    pub fn snapshot(&self, id: &DeviceId) -> Result<DeviceRemaps> {
        let g = self.inner.lock().unwrap();
        Ok(g.devices[g.find(id)?].snapshot())
    }

    pub fn set_binding(&self, device_id: DeviceId, binding: RemapBinding) -> Result<DeviceRemaps> {
        binding.validate().map_err(|e| anyhow::anyhow!(e))?;
        self.edit_bindings(&device_id, |b| {
            b.insert(binding.source.0, binding);
        })
    }

    pub fn clear_binding(&self, device_id: DeviceId, source: EvdevKey) -> Result<DeviceRemaps> {
        self.edit_bindings(&device_id, |b| {
            b.remove(&source.0);
        })
    }

    fn edit_bindings(
        &self,
        id: &DeviceId,
        edit: impl FnOnce(&mut HashMap<u16, RemapBinding>),
    ) -> Result<DeviceRemaps> {
        let mut guard = self.inner.lock().unwrap();
        let g = &mut *guard;
        let idx = g.find(id)?;
        let d = &mut g.devices[idx];
        let mut next = d.bindings.clone();
        edit(&mut next);
        persist(&g.paths, d, &next)?;
        d.bindings = next;
        if d.capturing.take().is_some() {
            d.capture_source = None;
            let _ = self.tx.send(Cmd::CancelCapture { idx });
        }
        let _ = self.tx.send(Cmd::SetBindings {
            idx,
            bindings: d.bindings.clone(),
        });
        Ok(d.snapshot())
    }

    pub fn start_capture(
        &self,
        device_id: DeviceId,
        role: CaptureRole,
        source: Option<EvdevKey>,
    ) -> Result<DeviceRemaps> {
        if role == CaptureRole::Target && source.is_none() {
            anyhow::bail!("target capture needs a source button");
        }
        let mut g = self.inner.lock().unwrap();
        let idx = g.find(&device_id)?;
        // One listening session at a time.
        for d in &mut g.devices {
            d.capturing = None;
            d.capture_source = None;
        }
        let d = &mut g.devices[idx];
        d.capturing = Some(role);
        d.capture_source = if role == CaptureRole::Target {
            source
        } else {
            None
        };
        d.last_error = None;
        if role == CaptureRole::Record {
            d.recorded.clear();
        }
        let _ = self.tx.send(Cmd::StartCapture { idx });
        Ok(d.snapshot())
    }

    /// Stop listening. For a record session `keep` stores the take in `recorded`.
    pub fn cancel_capture(&self, device_id: DeviceId, keep: bool) -> Result<DeviceRemaps> {
        let mut g = self.inner.lock().unwrap();
        let idx = g.find(&device_id)?;
        let d = &mut g.devices[idx];
        if d.capturing.take() == Some(CaptureRole::Record) {
            d.recorded = if keep {
                finish_recording(std::mem::take(&mut d.recorded), true)
            } else {
                Vec::new()
            };
        }
        d.capture_source = None;
        let _ = self.tx.send(Cmd::CancelCapture { idx });
        Ok(d.snapshot())
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

/// `id` is the same physical device as a model with these PIDs (instance and
/// receiver-vs-wired PID may differ across reconnects).
fn claims(vid: u16, pids: &[u16], id: &DeviceId) -> bool {
    id.vendor_id == vid && pids.contains(&id.product_id)
}

type Loaded = (HashMap<u16, RemapBinding>, Vec<RemapBinding>);

/// (valid bindings, rejected ones). First existing file wins, canonical PID
/// first (an emptied canonical file must not resurrect bindings from an older
/// sibling file). An unreadable file is an error, never "no bindings".
fn load_bindings(paths: &ConfigPaths, vid: u16, pids: &[u16]) -> Result<Loaded, String> {
    for &pid in pids {
        let path = remap_config_path(paths, vid, pid);
        if !path.exists() {
            continue;
        }
        let list = load_remap_bindings(paths, vid, pid).map_err(|e| {
            format!(
                "cannot read {}: {e:#}. Remaps for this device are read-only until it is fixed.",
                path.display()
            )
        })?;
        let (valid, invalid): (Vec<_>, Vec<_>) =
            list.into_iter().partition(|b| match b.validate() {
                Ok(()) => true,
                Err(e) => {
                    warn!("remap {} kept but not applied: {e}", b.source.label());
                    false
                }
            });
        return Ok((
            valid.into_iter().map(|b| (b.source.0, b)).collect(),
            invalid,
        ));
    }
    Ok(Default::default())
}

/// Write `bindings` plus the rejected ones they don't replace.
fn persist(paths: &ConfigPaths, d: &DevState, bindings: &HashMap<u16, RemapBinding>) -> Result<()> {
    if let Some(e) = &d.load_error {
        anyhow::bail!("{e}");
    }
    let mut list: Vec<_> = bindings
        .values()
        .chain(
            d.invalid
                .iter()
                .filter(|b| !bindings.contains_key(&b.source.0)),
        )
        .cloned()
        .collect();
    list.sort_by_key(|b| b.source.0);
    save_remap_bindings(paths, d.id.vendor_id, d.pids[0], &list)
}

struct Slot {
    id: DeviceId,
    kind: DeviceKind,
    pids: Vec<u16>,
    bindings: HashMap<u16, RemapBinding>,
    /// Open evdev nodes; non-empty exactly while the device is grabbed.
    nodes: Vec<Device>,
    /// Device paths of `nodes`.
    paths: Vec<PathBuf>,
    retry: Instant,
}

/// A capture / record session. Listening nodes stay open for its whole
/// duration so no edge is lost between reads.
struct Session {
    idx: usize,
    /// (node, belongs to the session's own device)
    nodes: Vec<(Device, bool)>,
    rec: Recorder,
    started: Instant,
}

impl Session {
    fn open(idx: usize, slot: &Slot) -> Self {
        let nodes = evdev::enumerate()
            .filter_map(|(_, dev)| {
                if dev.name() == Some(VIRTUAL_NAME) {
                    return None;
                }
                let own = node_matches(slot, &dev);
                let keyboard = dev
                    .supported_keys()
                    .is_some_and(|k| k.contains(KeyCode::KEY_A));
                if !own && !keyboard {
                    return None;
                }
                let _ = dev.set_nonblocking(true);
                Some((dev, own))
            })
            .collect();
        Self {
            idx,
            nodes,
            rec: Recorder::default(),
            started: Instant::now(),
        }
    }
}

struct Worker {
    inner: Arc<Mutex<EngineInner>>,
    events: broadcast::Sender<AdapterEvent>,
    slots: Vec<Slot>,
    virt: Option<VirtualDevice>,
    mapper: Mapper,
    session: Option<Session>,
}

impl Worker {
    fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    Cmd::Shutdown => {
                        self.session = None;
                        self.ungrab_all();
                        return;
                    }
                    Cmd::Attach {
                        idx,
                        id,
                        kind,
                        pids,
                        bindings,
                    } => {
                        if idx == self.slots.len() {
                            self.slots.push(Slot {
                                id,
                                kind,
                                pids,
                                bindings: HashMap::new(),
                                nodes: Vec::new(),
                                paths: Vec::new(),
                                retry: Instant::now(),
                            });
                        } else if self.slots[idx].id != id || self.slots[idx].kind != kind {
                            self.ungrab(idx);
                            self.slots[idx].id = id;
                            self.slots[idx].kind = kind;
                            self.slots[idx].retry = Instant::now();
                        }
                        self.set_bindings(idx, bindings);
                    }
                    Cmd::SetBindings { idx, bindings } => self.set_bindings(idx, bindings),
                    Cmd::StartCapture { idx } => {
                        // Never steal input while the user is choosing keys.
                        self.ungrab_all();
                        self.session = Some(Session::open(idx, &self.slots[idx]));
                    }
                    Cmd::CancelCapture { idx } => {
                        if self.session.as_ref().is_some_and(|s| s.idx == idx) {
                            self.session = None;
                        }
                    }
                }
            }

            self.sync_grabs();
            self.wait();
            self.read_slots();
            self.read_session();
            self.expire_session();
            self.mapper.tick(Instant::now());
            self.flush();
        }
    }

    fn set_bindings(&mut self, idx: usize, bindings: HashMap<u16, RemapBinding>) {
        let slot = &mut self.slots[idx];
        for b in slot.bindings.values() {
            if b.mode == RemapMode::Tap {
                self.mapper.release(b.target.0);
            }
        }
        self.mapper.stop_device(idx);
        slot.bindings = bindings;
        if slot.bindings.is_empty() {
            self.ungrab(idx);
        }
        self.flush();
    }

    /// Listening must not outlive an abandoned UI: every grab is released and
    /// (Record) every keyboard is read while a session runs.
    fn expire_session(&mut self) {
        let Some(sess) = &self.session else { return };
        if sess.started.elapsed() < SESSION_LIMIT {
            return;
        }
        let idx = sess.idx;
        self.session = None;
        self.update(idx, |d| {
            if let Some(role) = d.capturing.take() {
                d.capture_source = None;
                if role == CaptureRole::Record {
                    d.recorded.clear();
                }
                d.last_error = Some("Listening timed out".into());
            }
        });
    }

    fn sync_grabs(&mut self) {
        let now = Instant::now();
        for idx in 0..self.slots.len() {
            let slot = &self.slots[idx];
            let want = !slot.bindings.is_empty() && self.session.is_none();
            let grabbed = !slot.nodes.is_empty();
            // The seat ACL goes away when the user switches session; open fds
            // would keep reading (and re-injecting) someone else's input.
            let lost = grabbed && !slot.paths.iter().all(|p| readable(p));
            if !want || lost {
                if lost {
                    info!("lost access to {}; releasing it", slot.id.key());
                }
                self.ungrab(idx);
                if lost {
                    self.slots[idx].retry = now + OPEN_RETRY;
                }
            } else if !grabbed && now >= slot.retry {
                self.try_grab(idx, now);
            }
        }
    }

    fn try_grab(&mut self, idx: usize, now: Instant) {
        if self.virt.is_none() {
            match build_virtual() {
                Ok(v) => self.virt = Some(v),
                Err(e) => {
                    warn!("uinput: {e}");
                    self.slots[idx].retry = now + OPEN_RETRY;
                    self.update(idx, |d| {
                        d.last_error = Some(format!(
                            "Cannot create virtual device ({e}). Need write access to /dev/uinput (group input)."
                        ))
                    });
                    return;
                }
            }
        }
        let slot = &mut self.slots[idx];
        let nodes: Vec<(PathBuf, Device)> = evdev::enumerate()
            .filter(|(_, dev)| node_matches(slot, dev))
            .collect();
        if nodes.is_empty() {
            slot.retry = now + OPEN_RETRY;
            // Present but unreadable (no udev rule for this device): say so.
            let blocked = unreadable_nodes(slot.id.vendor_id, &slot.pids);
            if let Some(first) = blocked.first() {
                let msg = format!(
                    "Cannot read {} ({} input node(s)): add your user to the `input` group, or install the Hako udev rules if this device is listed.",
                    first.display(),
                    blocked.len()
                );
                let changed = self.inner.lock().unwrap().devices[idx]
                    .last_error
                    .as_deref()
                    != Some(msg.as_str());
                if changed {
                    self.update(idx, |d| d.last_error = Some(msg));
                }
            }
            return;
        }
        // Grabbing mid-press would leave that key stuck down in the compositor.
        if nodes
            .iter()
            .any(|(_, d)| d.get_key_state().is_ok_and(|k| k.iter().next().is_some()))
        {
            slot.retry = now + GRAB_RETRY;
            return;
        }
        let mut err = None;
        for (path, mut dev) in nodes {
            match dev.grab() {
                Ok(()) => {
                    let _ = dev.set_nonblocking(true);
                    slot.nodes.push(dev);
                    slot.paths.push(path);
                }
                Err(e) => {
                    warn!("grab {}: {e}", slot.id.key());
                    err = Some(format!("grab: {e}"));
                }
            }
        }
        slot.retry = now + OPEN_RETRY;
        let grabbed = !slot.nodes.is_empty();
        if grabbed {
            info!(
                "grabbed {} evdev node(s) of {} for remaps",
                slot.nodes.len(),
                slot.id.key()
            );
        }
        self.update(idx, |d| {
            d.grabbed = grabbed;
            d.last_error = err;
        });
    }

    /// Release everything the virtual device holds, then let go of the device.
    fn ungrab(&mut self, idx: usize) {
        if self.slots[idx].nodes.is_empty() {
            return;
        }
        // ponytail: releases keys held via other grabbed devices too; track
        // per-device ownership if that glitch ever matters.
        self.mapper.release_all();
        self.flush();
        self.drop_nodes(idx);
        if self.slots.iter().all(|s| s.nodes.is_empty()) {
            self.virt = None;
        }
    }

    fn ungrab_all(&mut self) {
        for idx in 0..self.slots.len() {
            self.ungrab(idx);
        }
    }

    fn drop_nodes(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        if slot.nodes.is_empty() {
            return;
        }
        for dev in &mut slot.nodes {
            let _ = dev.ungrab();
        }
        slot.nodes.clear();
        slot.paths.clear();
        info!("released {} from remaps", slot.id.key());
        self.update(idx, |d| d.grabbed = false);
    }

    fn update(&self, idx: usize, f: impl FnOnce(&mut DevState)) {
        let snap = {
            let mut g = self.inner.lock().unwrap();
            f(&mut g.devices[idx]);
            g.devices[idx].snapshot()
        };
        let _ = self.events.send(AdapterEvent::RemapsUpdated(snap));
    }

    fn wait(&self) {
        let mut fds: Vec<libc::pollfd> = self
            .slots
            .iter()
            .flat_map(|s| s.nodes.iter())
            .chain(
                self.session
                    .iter()
                    .flat_map(|s| s.nodes.iter().map(|(d, _)| d)),
            )
            .map(|d| libc::pollfd {
                fd: d.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let timeout = self
            .mapper
            .next_deadline()
            .map_or(IDLE_WAIT, |t| t.saturating_duration_since(Instant::now()))
            .min(IDLE_WAIT);
        let ms = timeout.as_micros().div_ceil(1000) as i32;
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms);
        }
    }

    fn read_slots(&mut self) {
        let now = Instant::now();
        let mut dead = Vec::new();
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            for dev in &mut slot.nodes {
                match dev.fetch_events() {
                    Ok(iter) => {
                        for ev in iter {
                            let bind = match ev.destructure() {
                                EventSummary::Key(_, code, _) => slot.bindings.get(&code.code()),
                                _ => None,
                            };
                            match bind {
                                Some(b) => self.mapper.key(idx, b, ev.value(), now),
                                None => self.mapper.forward(ev),
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        warn!("evdev {}: {e}", slot.id.key());
                        dead.push(idx);
                        break;
                    }
                }
            }
        }
        for idx in dead {
            self.ungrab(idx);
            self.slots[idx].retry = now + OPEN_RETRY;
        }
    }

    fn read_session(&mut self) {
        let Some(sess) = self.session.as_mut() else {
            return;
        };
        let mut edges = Vec::new();
        sess.nodes
            .retain_mut(|(dev, own)| match dev.fetch_events() {
                Ok(iter) => {
                    for ev in iter {
                        if let EventSummary::Key(_, code, value @ (0 | 1)) = ev.destructure() {
                            edges.push((ev.timestamp(), code.code(), value == 1, *own));
                        }
                    }
                    true
                }
                Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
            });
        edges.sort_by_key(|e| e.0);
        for (at, code, down, own) in edges {
            if !self.session_edge(at, code, down, own) {
                self.session = None;
                return;
            }
        }
    }

    /// Feed one key edge to the session; false when the session is over.
    fn session_edge(&mut self, at: SystemTime, code: u16, down: bool, own: bool) -> bool {
        let Some(sess) = self.session.as_mut() else {
            return false;
        };
        let idx = sess.idx;
        let kind = self.slots[idx].kind;
        let key = EvdevKey(code);
        let inner = self.inner.clone();
        let mut guard = inner.lock().unwrap();
        let g = &mut *guard;
        let d = &mut g.devices[idx];
        // The API may have cancelled before our Cancel command arrives.
        let Some(role) = d.capturing else {
            return false;
        };
        let mut sent = Vec::new();
        let mut new_bindings = None;
        let more = if down && code == KEY_ESC {
            if role == CaptureRole::Record {
                d.recorded = finish_recording(std::mem::take(&mut d.recorded), false);
            }
            d.capturing = None;
            d.capture_source = None;
            false
        } else {
            match role {
                CaptureRole::Record => {
                    if d.recorded.len() < MAX_MACRO_STEPS {
                        let delay_ms = sess.rec.delay(at);
                        d.recorded.push(MacroStep {
                            key,
                            down,
                            delay_ms,
                        });
                    }
                    true
                }
                CaptureRole::Source => {
                    if down && own && source_allowed(kind, code) {
                        d.capturing = Some(CaptureRole::Target);
                        d.capture_source = Some(key);
                        sent.push(AdapterEvent::RemapCaptured(RemapCaptured {
                            device_id: d.id.clone(),
                            role,
                            key,
                            source: None,
                        }));
                    }
                    true
                }
                CaptureRole::Target => {
                    // Left click from elsewhere is the UI "listen" click, not a target.
                    if !down || (!own && key == EvdevKey::BTN_LEFT) {
                        return true;
                    }
                    let source = d.capture_source.take();
                    d.capturing = None;
                    if let Some(src) = source {
                        let binding = RemapBinding {
                            source: src,
                            target: key,
                            ..d.bindings
                                .get(&src.0)
                                .cloned()
                                .unwrap_or_else(|| RemapBinding::tap(src, key))
                        };
                        let mut next = d.bindings.clone();
                        next.insert(src.0, binding.clone());
                        match binding
                            .validate()
                            .map_err(anyhow::Error::msg)
                            .and_then(|()| persist(&g.paths, d, &next))
                        {
                            Ok(()) => {
                                d.bindings = next;
                                new_bindings = Some(d.bindings.clone());
                            }
                            Err(e) => d.last_error = Some(e.to_string()),
                        }
                    }
                    info!("captured target {} for {}", key.label(), d.id.key());
                    sent.push(AdapterEvent::RemapCaptured(RemapCaptured {
                        device_id: d.id.clone(),
                        role,
                        key,
                        source,
                    }));
                    false
                }
            }
        };
        sent.push(AdapterEvent::RemapsUpdated(d.snapshot()));
        drop(guard);
        for ev in sent {
            let _ = self.events.send(ev);
        }
        if let Some(b) = new_bindings {
            self.set_bindings(idx, b);
        }
        more
    }

    fn flush(&mut self) {
        let out = self.mapper.take();
        if out.is_empty() {
            return;
        }
        let Some(virt) = self.virt.as_mut() else {
            return;
        };
        if let Err(e) = virt.emit(&out) {
            // Fail safe: a grabbed device we cannot forward would eat all input.
            warn!("uinput emit: {e}; releasing all devices");
            self.virt = None;
            self.mapper = Mapper::default();
            for idx in 0..self.slots.len() {
                if !self.slots[idx].nodes.is_empty() {
                    self.drop_nodes(idx);
                    self.update(idx, |d| d.last_error = Some(format!("uinput: {e}")));
                }
            }
        }
    }
}

/// Mouse: may use any button but left / right. Keyboard: any key but Esc
/// (Esc cancels listening).
fn source_allowed(kind: DeviceKind, code: u16) -> bool {
    let key = EvdevKey(code);
    code != KEY_ESC
        && match kind {
            DeviceKind::Mouse => key != EvdevKey::BTN_LEFT && key != EvdevKey::BTN_RIGHT,
            _ => true,
        }
}

/// `/dev/input/event*` nodes of `vendor` + one of `pids` this user cannot read.
fn unreadable_nodes(vendor: u16, pids: &[u16]) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/sys/class/input") else {
        return Vec::new();
    };
    let id = |p: &Path, f: &str| {
        std::fs::read_to_string(p.join("device/id").join(f))
            .ok()
            .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
    };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .filter(|e| {
            id(&e.path(), "vendor") == Some(vendor)
                && id(&e.path(), "product").is_some_and(|p| pids.contains(&p))
        })
        .map(|e| Path::new("/dev/input").join(e.file_name()))
        .filter(|dev| !readable(dev))
        .collect()
}

/// The calling user may still open `path` (seat ACL / group).
fn readable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        // SAFETY: valid NUL-terminated path; access() only reads it.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::R_OK) } == 0)
}

fn node_matches(slot: &Slot, dev: &Device) -> bool {
    if dev.name() == Some(VIRTUAL_NAME) {
        return false;
    }
    let Some(keys) = dev.supported_keys() else {
        return false;
    };
    let iid = dev.input_id();
    let same = iid.vendor() == slot.id.vendor_id && slot.pids.contains(&iid.product());
    match slot.kind {
        DeviceKind::Mouse => {
            same && (keys.contains(KeyCode::BTN_LEFT) || keys.contains(KeyCode::BTN_SIDE))
        }
        // Every interface that emits keyboard / consumer keys (the Huntsman
        // splits keys across several); skips its pointer-only interface.
        DeviceKind::Keyboard => same && keys.iter().any(|k| k.code() < 0x100 || k.code() >= 0x160),
        _ => false,
    }
}

fn build_virtual() -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in (1u16..0x2c0).filter(|&c| emittable(c)) {
        keys.insert(KeyCode::new(code));
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    rel.insert(RelativeAxisCode::REL_X);
    rel.insert(RelativeAxisCode::REL_Y);
    rel.insert(RelativeAxisCode::REL_WHEEL);
    rel.insert(RelativeAxisCode::REL_HWHEEL);
    rel.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rel.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);
    let mut msc = AttributeSet::<MiscCode>::new();
    msc.insert(MiscCode::MSC_SCAN);

    Ok(VirtualDevice::builder()?
        .name(VIRTUAL_NAME)
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .with_msc(&msc)?
        .build()?)
}

/// A timed key sequence (turbo pulse or macro) driven by one bound source.
struct Runner {
    steps: Vec<MacroStep>,
    looping: bool,
    gap: Duration,
    pos: usize,
    next: Instant,
    /// Keys this runner pressed and has not released yet.
    held: BTreeSet<u16>,
}

impl Runner {
    fn new(steps: Vec<MacroStep>, looping: bool, gap_ms: u32, now: Instant) -> Option<Self> {
        let first = steps.first()?.delay_ms;
        Some(Self {
            steps,
            looping,
            gap: ms(clamp_interval(gap_ms)),
            pos: 0,
            next: now + ms(first),
            held: BTreeSet::new(),
        })
    }

    /// Turbo / toggle: down, wait interval, up, wait interval, repeat.
    fn pulse(b: &RemapBinding, now: Instant) -> Option<Self> {
        let interval = clamp_interval(b.interval_ms);
        let steps = vec![
            MacroStep {
                key: b.target,
                down: true,
                delay_ms: 0,
            },
            MacroStep {
                key: b.target,
                down: false,
                delay_ms: interval,
            },
        ];
        Self::new(steps, true, interval, now)
    }
}

fn ms(v: u32) -> Duration {
    Duration::from_millis(u64::from(v))
}

/// Pure remap state: turns physical events into what the virtual device must
/// emit (`out`) and tracks which keys it holds down. No I/O.
#[derive(Default)]
struct Mapper {
    /// Keyed by (slot index, source code).
    runners: HashMap<(usize, u16), Runner>,
    held: BTreeSet<u16>,
    out: Vec<InputEvent>,
}

fn push_key(out: &mut Vec<InputEvent>, held: &mut BTreeSet<u16>, code: u16, value: i32) {
    match value {
        0 => {
            held.remove(&code);
        }
        1 => {
            held.insert(code);
        }
        _ => {}
    }
    out.push(InputEvent::new(EventType::KEY.0, code, value));
}

impl Mapper {
    /// Pass an unbound event through unchanged.
    fn forward(&mut self, ev: InputEvent) {
        if ev.event_type() == EventType::KEY {
            match ev.value() {
                0 => {
                    self.held.remove(&ev.code());
                }
                1 => {
                    self.held.insert(ev.code());
                }
                _ => {}
            }
        }
        self.out.push(ev);
    }

    /// A bound source key changed (`value` 0 up, 1 down, 2 autorepeat).
    fn key(&mut self, dev: usize, b: &RemapBinding, value: i32, now: Instant) {
        let id = (dev, b.source.0);
        let running = self.runners.contains_key(&id);
        match (b.mode, value) {
            (RemapMode::Tap, _) => push_key(&mut self.out, &mut self.held, b.target.0, value),
            (RemapMode::Disabled, _) | (_, 2) => {}
            (RemapMode::Turbo, 1) => self.start(id, Runner::pulse(b, now), now),
            (RemapMode::Turbo, _) => self.stop(id),
            (RemapMode::Toggle, 1) if running => self.stop(id),
            (RemapMode::Toggle, 1) => self.start(id, Runner::pulse(b, now), now),
            (RemapMode::Macro, 1) => match b.repeat {
                MacroRepeat::Toggle if running => self.stop(id),
                MacroRepeat::Once if running => {}
                repeat => self.start(
                    id,
                    Runner::new(
                        b.steps.clone(),
                        repeat != MacroRepeat::Once,
                        b.interval_ms,
                        now,
                    ),
                    now,
                ),
            },
            (RemapMode::Macro, _) if b.repeat == MacroRepeat::WhileHeld => self.stop(id),
            _ => {}
        }
    }

    fn start(&mut self, id: (usize, u16), runner: Option<Runner>, now: Instant) {
        self.stop(id);
        if let Some(r) = runner {
            self.runners.insert(id, r);
            self.tick(now);
        }
    }

    fn stop(&mut self, id: (usize, u16)) {
        if let Some(r) = self.runners.remove(&id) {
            for k in r.held {
                push_key(&mut self.out, &mut self.held, k, 0);
            }
        }
    }

    fn stop_device(&mut self, dev: usize) {
        let ids: Vec<_> = self
            .runners
            .keys()
            .filter(|k| k.0 == dev)
            .copied()
            .collect();
        for id in ids {
            self.stop(id);
        }
    }

    fn release(&mut self, code: u16) {
        if self.held.contains(&code) {
            push_key(&mut self.out, &mut self.held, code, 0);
        }
    }

    /// Stop every runner and release every key the virtual device holds.
    fn release_all(&mut self) {
        self.runners.clear();
        for k in std::mem::take(&mut self.held) {
            self.out.push(InputEvent::new(EventType::KEY.0, k, 0));
        }
    }

    /// Emit every step that is due at `now`.
    fn tick(&mut self, now: Instant) {
        let mut done = Vec::new();
        for (id, r) in self.runners.iter_mut() {
            while now >= r.next {
                let s = r.steps[r.pos];
                push_key(&mut self.out, &mut self.held, s.key.0, i32::from(s.down));
                if s.down {
                    r.held.insert(s.key.0);
                } else {
                    r.held.remove(&s.key.0);
                }
                r.pos += 1;
                if let Some(next) = r.steps.get(r.pos) {
                    r.next += ms(next.delay_ms);
                    continue;
                }
                // End of one pass: never leave keys down between iterations.
                for k in std::mem::take(&mut r.held) {
                    push_key(&mut self.out, &mut self.held, k, 0);
                }
                if !r.looping {
                    done.push(*id);
                    break;
                }
                r.pos = 0;
                r.next = now + r.gap + ms(r.steps[0].delay_ms);
            }
        }
        for id in done {
            self.runners.remove(&id);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.runners.values().map(|r| r.next).min()
    }

    fn take(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.out)
    }
}

/// Delay bookkeeping for a record session.
#[derive(Default)]
struct Recorder {
    last: Option<SystemTime>,
}

impl Recorder {
    /// Wait before an edge at `at`: time since the previous edge (0 for the
    /// first), clamped to [`MAX_STEP_DELAY_MS`].
    fn delay(&mut self, at: SystemTime) -> u32 {
        let delay = self
            .last
            .and_then(|last| at.duration_since(last).ok())
            .map_or(0, |d| {
                d.as_millis().min(u128::from(MAX_STEP_DELAY_MS)) as u32
            });
        self.last = Some(at);
        delay
    }
}

/// Clean a raw take: drop releases of keys held before recording began
/// (their wait moves to the next step) and, when stopped from the UI, the
/// trailing left click that pressed "Stop". The first step waits 0 ms.
fn finish_recording(mut steps: Vec<MacroStep>, drop_stop_click: bool) -> Vec<MacroStep> {
    if drop_stop_click {
        if let Some(i) = steps
            .iter()
            .rposition(|s| s.key == EvdevKey::BTN_LEFT && s.down)
        {
            if steps[i..].iter().all(|s| s.key == EvdevKey::BTN_LEFT) {
                steps.truncate(i);
            }
        }
    }
    let mut down = BTreeSet::new();
    let mut carry = 0u32;
    let mut out = Vec::with_capacity(steps.len());
    for s in steps {
        if !s.down && !down.remove(&s.key.0) {
            carry = carry.saturating_add(s.delay_ms);
            continue;
        }
        if s.down {
            down.insert(s.key.0);
        }
        out.push(MacroStep {
            delay_ms: s.delay_ms.saturating_add(carry).min(MAX_STEP_DELAY_MS),
            ..s
        });
        carry = 0;
    }
    if let Some(first) = out.first_mut() {
        first.delay_ms = 0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: EvdevKey = EvdevKey(30);
    const B: EvdevKey = EvdevKey(48);
    const C: EvdevKey = EvdevKey(46);

    fn keys(m: &mut Mapper) -> Vec<(u16, i32)> {
        m.take().iter().map(|e| (e.code(), e.value())).collect()
    }

    fn step(key: EvdevKey, down: bool, delay_ms: u32) -> MacroStep {
        MacroStep {
            key,
            down,
            delay_ms,
        }
    }

    fn macro_binding(steps: Vec<MacroStep>, repeat: MacroRepeat, interval_ms: u32) -> RemapBinding {
        RemapBinding {
            mode: RemapMode::Macro,
            steps,
            repeat,
            interval_ms,
            ..RemapBinding::tap(EvdevKey::BTN_SIDE, EvdevKey(0))
        }
    }

    fn at(t0: Instant, ms_: u64) -> Instant {
        t0 + Duration::from_millis(ms_)
    }

    #[test]
    fn macro_once_honours_delays_and_order() {
        let b = macro_binding(
            vec![
                step(A, true, 0),
                step(A, false, 30),
                step(B, true, 20),
                step(B, false, 0),
            ],
            MacroRepeat::Once,
            50,
        );
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.key(0, &b, 1, t0);
        assert_eq!(keys(&mut m), vec![(30, 1)]);
        assert_eq!(m.next_deadline(), Some(at(t0, 30)));
        m.key(0, &b, 0, at(t0, 5)); // release doesn't stop a Once macro
        m.tick(at(t0, 29));
        assert!(keys(&mut m).is_empty());
        m.tick(at(t0, 30));
        assert_eq!(keys(&mut m), vec![(30, 0)]);
        m.tick(at(t0, 50));
        assert_eq!(keys(&mut m), vec![(48, 1), (48, 0)]);
        assert_eq!(m.next_deadline(), None);
        assert!(m.held.is_empty());
    }

    #[test]
    fn macro_while_held_loops_and_releases_on_up() {
        let b = macro_binding(
            vec![step(A, true, 0), step(A, false, 10)],
            MacroRepeat::WhileHeld,
            100,
        );
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.key(0, &b, 1, t0);
        m.tick(at(t0, 10));
        assert_eq!(keys(&mut m), vec![(30, 1), (30, 0)]);
        assert_eq!(m.next_deadline(), Some(at(t0, 110)));
        m.tick(at(t0, 110));
        assert_eq!(keys(&mut m), vec![(30, 1)]);
        m.key(0, &b, 0, at(t0, 115));
        assert_eq!(keys(&mut m), vec![(30, 0)]);
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn unfinished_macro_pass_releases_its_keys() {
        let b = macro_binding(
            vec![step(A, true, 0), step(B, true, 5)],
            MacroRepeat::Once,
            50,
        );
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.key(0, &b, 1, t0);
        m.tick(at(t0, 5));
        assert_eq!(keys(&mut m), vec![(30, 1), (48, 1), (30, 0), (48, 0)]);
        assert!(m.held.is_empty());
    }

    #[test]
    fn toggle_press_starts_and_next_press_stops() {
        let b = RemapBinding {
            mode: RemapMode::Toggle,
            interval_ms: 50,
            ..RemapBinding::tap(EvdevKey::BTN_SIDE, A)
        };
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.key(0, &b, 1, t0);
        m.key(0, &b, 0, at(t0, 5));
        assert_eq!(keys(&mut m), vec![(30, 1)]);
        m.tick(at(t0, 50));
        m.tick(at(t0, 100));
        assert_eq!(keys(&mut m), vec![(30, 0), (30, 1)]);
        m.key(0, &b, 1, at(t0, 120));
        assert_eq!(keys(&mut m), vec![(30, 0)]);
        assert_eq!(m.next_deadline(), None);
        m.key(0, &b, 0, at(t0, 125));
        assert!(keys(&mut m).is_empty());
    }

    #[test]
    fn turbo_pulses_while_held() {
        let b = RemapBinding {
            mode: RemapMode::Turbo,
            interval_ms: 20,
            ..RemapBinding::tap(EvdevKey::BTN_SIDE, A)
        };
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.key(0, &b, 1, t0);
        m.tick(at(t0, 20));
        m.tick(at(t0, 40));
        m.key(0, &b, 0, at(t0, 45));
        assert_eq!(keys(&mut m), vec![(30, 1), (30, 0), (30, 1), (30, 0)]);
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn disabled_swallows_and_tap_follows() {
        let mut m = Mapper::default();
        let t0 = Instant::now();
        let off = RemapBinding {
            mode: RemapMode::Disabled,
            ..RemapBinding::tap(EvdevKey::BTN_SIDE, A)
        };
        m.key(0, &off, 1, t0);
        m.key(0, &off, 0, t0);
        assert!(keys(&mut m).is_empty());
        let tap = RemapBinding::tap(EvdevKey::BTN_SIDE, B);
        m.key(0, &tap, 1, t0);
        m.key(0, &tap, 2, t0);
        m.key(0, &tap, 0, t0);
        assert_eq!(keys(&mut m), vec![(48, 1), (48, 2), (48, 0)]);
    }

    #[test]
    fn release_all_frees_every_held_key() {
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.forward(InputEvent::new(EventType::KEY.0, A.0, 1));
        m.key(0, &RemapBinding::tap(EvdevKey::BTN_SIDE, B), 1, t0);
        let mac = macro_binding(
            vec![step(C, true, 0), step(C, false, 500)],
            MacroRepeat::Once,
            50,
        );
        m.key(1, &mac, 1, t0);
        m.take();
        assert_eq!(m.held.iter().copied().collect::<Vec<_>>(), vec![30, 46, 48]);
        m.release_all();
        assert_eq!(keys(&mut m), vec![(30, 0), (46, 0), (48, 0)]);
        assert!(m.held.is_empty() && m.runners.is_empty());
    }

    #[test]
    fn stop_device_and_release_only_touch_their_keys() {
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.forward(InputEvent::new(EventType::KEY.0, A.0, 1));
        let mac = macro_binding(
            vec![step(C, true, 0), step(C, false, 500)],
            MacroRepeat::Once,
            50,
        );
        m.key(1, &mac, 1, t0);
        m.take();
        m.stop_device(1);
        m.release(B.0); // not held: nothing to emit
        assert_eq!(keys(&mut m), vec![(46, 0)]);
        assert_eq!(m.held.iter().copied().collect::<Vec<_>>(), vec![30]);
    }

    #[test]
    fn recorder_delays_are_relative_and_clamped() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut r = Recorder::default();
        assert_eq!(r.delay(t0), 0);
        assert_eq!(r.delay(t0 + Duration::from_millis(37)), 37);
        assert_eq!(r.delay(t0 + Duration::from_secs(60)), MAX_STEP_DELAY_MS);
        assert_eq!(r.delay(t0), 0); // out of order
    }

    #[test]
    fn finish_recording_drops_orphans_and_stop_click() {
        let raw = vec![
            step(EvdevKey(28), false, 0), // Enter release from before recording
            step(A, true, 40),
            step(A, false, 30),
            step(EvdevKey::BTN_LEFT, true, 500),
            step(EvdevKey::BTN_LEFT, false, 80),
        ];
        let kept = finish_recording(raw.clone(), true);
        assert_eq!(kept, vec![step(A, true, 0), step(A, false, 30)]);
        // Esc stop keeps clicks; orphan release's wait carries to the next step.
        let esc = finish_recording(raw[..3].to_vec(), false);
        assert_eq!(esc, vec![step(A, true, 0), step(A, false, 30)]);
        let carried = finish_recording(
            vec![step(A, true, 0), step(B, false, 15), step(A, false, 10)],
            false,
        );
        assert_eq!(carried, vec![step(A, true, 0), step(A, false, 25)]);
    }

    #[test]
    fn unreadable_or_rejected_remaps_are_never_overwritten() {
        let root = std::env::temp_dir().join(format!("hako-remap-{}", std::process::id()));
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.clone(),
        };
        paths.ensure().unwrap();
        let file = remap_config_path(&paths, 0x046d, 0x40a8);

        std::fs::write(&file, "version = [").unwrap();
        let err = load_bindings(&paths, 0x046d, &[0x40a8]).unwrap_err();
        let mut d = DevState {
            id: DeviceId {
                vendor_id: 0x046d,
                product_id: 0x40a8,
                instance: "x".into(),
            },
            pids: vec![0x40a8],
            bindings: HashMap::new(),
            capturing: None,
            capture_source: None,
            last_error: None,
            grabbed: false,
            recorded: Vec::new(),
            invalid: Vec::new(),
            load_error: Some(err),
        };
        let edit = HashMap::from([(276, RemapBinding::tap(EvdevKey(276), A))]);
        assert!(persist(&paths, &d, &edit).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "version = [");

        // A binding this version rejects (joystick target) stays on disk.
        std::fs::write(
            &file,
            "version = 1\n[[bindings]]\nsource = 275\ntarget = 288\nmode = \"tap\"\n",
        )
        .unwrap();
        let (valid, invalid) = load_bindings(&paths, 0x046d, &[0x40a8]).unwrap();
        assert!(valid.is_empty() && invalid.len() == 1);
        d.invalid = invalid;
        d.load_error = None;
        persist(&paths, &d, &edit).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("source = 275") && text.contains("source = 276"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn capture_source_rules() {
        assert!(!source_allowed(DeviceKind::Mouse, EvdevKey::BTN_LEFT.0));
        assert!(!source_allowed(DeviceKind::Mouse, EvdevKey::BTN_RIGHT.0));
        assert!(source_allowed(DeviceKind::Mouse, EvdevKey::BTN_MIDDLE.0));
        assert!(source_allowed(DeviceKind::Keyboard, A.0));
        assert!(!source_allowed(DeviceKind::Keyboard, KEY_ESC));
    }

    #[test]
    fn any_model_pid_is_the_same_device() {
        let id = |pid| DeviceId {
            vendor_id: 0x046d,
            product_id: pid,
            instance: "x".into(),
        };
        let pids = &[0x40a8, 0xc543];
        assert!(claims(0x046d, pids, &id(0xc543)));
        assert!(claims(0x046d, pids, &id(0x40a8)));
        assert!(!claims(0x046d, pids, &id(0xc52b)));
        assert!(!claims(0x1532, pids, &id(0x40a8)));
    }
}
