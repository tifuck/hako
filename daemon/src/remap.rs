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
    MiscCode, RelativeAxisCode, UinputAbsSetup,
};
use hako_core::config::{load_remap_bindings, remap_config_path, save_remap_bindings, ConfigPaths};
use hako_core::device::{DeviceId, DeviceKind};
use hako_core::protocol::AdapterEvent;
use hako_core::remap::{
    clamp_interval, emittable, CaptureRole, DeviceRemaps, EvdevKey, MacroRepeat, MacroStep,
    RemapBinding, RemapCaptured, RemapMode, MAX_MACRO_STEPS, MAX_STEP_DELAY_MS,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::fd::AsRawFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

const VIRTUAL_NAME: &str = "hako remap";
const KEY_ESC: u16 = 1;
/// Upper bound on one poll; also the command latency.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// Retry when a key is held at grab time.
const GRAB_RETRY: Duration = Duration::from_millis(250);
/// Held-key retries before the wait is reported and slows to [`OPEN_RETRY`].
const HELD_REPORT_AFTER: u32 = 20;
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
    /// Product name, for error messages.
    name: String,
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

    /// An `Error` event when `last_error` is set and differs from `before`:
    /// each grab / open retry repeats the same error, which is pushed once.
    fn error_event(&self, before: Option<&str>) -> Option<AdapterEvent> {
        let error = self.last_error.as_deref().filter(|e| Some(*e) != before)?;
        Some(AdapterEvent::Error {
            device: Some(self.id.clone()),
            message: format!("{} remaps: {error}", self.name),
        })
    }
}

struct EngineInner {
    paths: ConfigPaths,
    devices: Vec<DevState>,
    /// Number of the last capture / record session started (one runs at a time).
    session: u64,
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
    /// Serialises binding edits, so a remap file is written outside `inner`
    /// (which the remap thread takes while forwarding). Taken before `inner`.
    edits: Arc<Mutex<()>>,
    tx: mpsc::UnboundedSender<Cmd>,
    events: broadcast::Sender<AdapterEvent>,
}

impl RemapEngine {
    pub fn start(paths: ConfigPaths, events: broadcast::Sender<AdapterEvent>) -> Self {
        let inner = Arc::new(Mutex::new(EngineInner {
            paths,
            devices: Vec::new(),
            session: 0,
        }));
        let edits = Arc::new(Mutex::new(()));
        let (tx, rx) = mpsc::unbounded_channel();
        let worker = Worker {
            inner: inner.clone(),
            edits: edits.clone(),
            events: events.clone(),
            slots: Vec::new(),
            virt: None,
            axes: Axes::default(),
            mapper: Mapper::default(),
            session: None,
        };
        // Without the thread `rx` is dropped: every request then fails with
        // "remap engine stopped" instead of the daemon going down.
        if let Err(e) = std::thread::Builder::new()
            .name("hako-remap".into())
            .spawn(move || worker.run(rx))
        {
            error!("cannot start the remap thread: {e}");
        }
        Self {
            inner,
            edits,
            tx,
            events,
        }
    }

    /// Register a mouse or keyboard (idempotent; a reconnect keeps its bindings).
    /// `pids` are the driver model's PIDs, so any enumeration path is one device.
    pub fn attach(&self, id: DeviceId, kind: DeviceKind, pids: Vec<u16>, name: &str) {
        let mut guard = lock(&self.inner);
        let g = &mut *guard;
        let idx = match g
            .devices
            .iter()
            .position(|d| claims(d.id.vendor_id, &d.pids, &id))
        {
            Some(i) => {
                let d = &mut g.devices[i];
                d.id = id.clone();
                if d.load_error.is_some() && retry_load(&g.paths, d) {
                    let _ = self.events.send(AdapterEvent::RemapsUpdated(d.snapshot()));
                }
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
                    name: name.into(),
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
                let d = g.devices.last().expect("just pushed");
                if let Some(ev) = d.error_event(None) {
                    let _ = self.events.send(ev);
                }
                g.devices.len() - 1
            }
        };
        let bindings = g.devices[idx].bindings.clone();
        let pids = g.devices[idx].pids.clone();
        // Sent under the lock so command order matches index order.
        if let Err(e) = self.send(Cmd::Attach {
            idx,
            id,
            kind,
            pids,
            bindings,
        }) {
            warn!("{e}");
        }
    }

    /// Hand `cmd` to the worker thread; an error when it is gone (it would
    /// otherwise look like success while nothing is applied).
    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx.send(cmd).map_err(|_| stopped())
    }

    /// Checked before a request changes state, so state and reply agree.
    fn alive(&self) -> Result<()> {
        if self.tx.is_closed() {
            return Err(stopped());
        }
        Ok(())
    }

    pub fn snapshots(&self) -> Vec<DeviceRemaps> {
        let g = lock(&self.inner);
        g.devices.iter().map(DevState::snapshot).collect()
    }

    pub fn snapshot(&self, id: &DeviceId) -> Result<DeviceRemaps> {
        let g = lock(&self.inner);
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
        self.alive()?;
        let _edits = lock_edits(&self.edits);
        let (idx, next, list, paths, vid, pid) = {
            let g = lock(&self.inner);
            let idx = g.find(id)?;
            let d = &g.devices[idx];
            let mut next = d.bindings.clone();
            edit(&mut next);
            let list = saved_list(d, &next)?;
            (idx, next, list, g.paths.clone(), d.id.vendor_id, d.pids[0])
        };
        save_remap_bindings(&paths, vid, pid, &list)?;
        let mut guard = lock(&self.inner);
        let d = &mut guard.devices[idx];
        d.bindings = next;
        if let Some(role) = d.capturing.take() {
            d.capture_source = None;
            if role == CaptureRole::Record {
                d.recorded.clear();
            }
            self.send(Cmd::CancelCapture { idx })?;
        }
        self.send(Cmd::SetBindings {
            idx,
            bindings: d.bindings.clone(),
        })?;
        Ok(d.snapshot())
    }

    /// Start listening; also returns the session's number (for
    /// [`Self::cancel_session`]).
    pub fn start_capture(
        &self,
        device_id: DeviceId,
        role: CaptureRole,
        source: Option<EvdevKey>,
    ) -> Result<(DeviceRemaps, u64)> {
        if role == CaptureRole::Target && source.is_none() {
            anyhow::bail!("target capture needs a source button");
        }
        self.alive()?;
        let mut g = lock(&self.inner);
        let idx = g.find(&device_id)?;
        // Listening to a device whose input nodes are all unreadable would
        // only ever time out.
        let nodes = event_nodes(g.devices[idx].id.vendor_id, &g.devices[idx].pids);
        let blocked: Vec<PathBuf> = nodes.iter().filter(|p| !readable(p)).cloned().collect();
        if blocked.len() == nodes.len() {
            if let Some(msg) = unreadable_message(&blocked) {
                anyhow::bail!(msg);
            }
        }
        // One listening session at a time: others stop (and say so).
        for (i, d) in g.devices.iter_mut().enumerate() {
            if i != idx && d.capturing.take().is_some() {
                d.capture_source = None;
                let _ = self.events.send(AdapterEvent::RemapsUpdated(d.snapshot()));
            }
        }
        g.session += 1;
        let session = g.session;
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
        let snap = d.snapshot();
        self.send(Cmd::StartCapture { idx })?;
        Ok((snap, session))
    }

    /// Stop listening. For a record session `keep` stores the take in `recorded`.
    pub fn cancel_capture(&self, device_id: DeviceId, keep: bool) -> Result<DeviceRemaps> {
        let mut g = lock(&self.inner);
        let idx = g.find(&device_id)?;
        self.stop_capture(&mut g.devices[idx], idx, keep)
    }

    /// Stop session `session` (from [`Self::start_capture`]) if it still
    /// runs, dropping its take; None when it has ended or another started.
    pub fn cancel_session(
        &self,
        device_id: DeviceId,
        session: u64,
    ) -> Result<Option<DeviceRemaps>> {
        let mut g = lock(&self.inner);
        let idx = g.find(&device_id)?;
        if g.session != session || g.devices[idx].capturing.is_none() {
            return Ok(None);
        }
        self.stop_capture(&mut g.devices[idx], idx, false).map(Some)
    }

    fn stop_capture(&self, d: &mut DevState, idx: usize, keep: bool) -> Result<DeviceRemaps> {
        if d.capturing.take() == Some(CaptureRole::Record) {
            d.recorded = if keep {
                finish_recording(std::mem::take(&mut d.recorded), true)
            } else {
                Vec::new()
            };
        }
        d.capture_source = None;
        let snap = d.snapshot();
        self.send(Cmd::CancelCapture { idx })?;
        Ok(snap)
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

/// The engine state, also after a panic while it was held (the remap thread
/// survives panics, and `GetState` reads it on every poll).
fn lock(inner: &Mutex<EngineInner>) -> MutexGuard<'_, EngineInner> {
    inner.lock().unwrap_or_else(|p| p.into_inner())
}

fn lock_edits(edits: &Mutex<()>) -> MutexGuard<'_, ()> {
    edits.lock().unwrap_or_else(|p| p.into_inner())
}

fn stopped() -> anyhow::Error {
    anyhow::anyhow!("remap engine stopped; restart hakod")
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

/// Read a remap file that failed to load again (on reconnect); true when it
/// loaded, which makes the device's remaps editable again.
fn retry_load(paths: &ConfigPaths, d: &mut DevState) -> bool {
    let Ok((bindings, invalid)) = load_bindings(paths, d.id.vendor_id, &d.pids) else {
        return false;
    };
    info!("remap file of {} is readable again", d.id.key());
    if d.last_error == d.load_error {
        d.last_error = None;
    }
    d.bindings = bindings;
    d.invalid = invalid;
    d.load_error = None;
    true
}

/// Write `bindings` plus the rejected ones they don't replace.
fn persist(paths: &ConfigPaths, d: &DevState, bindings: &HashMap<u16, RemapBinding>) -> Result<()> {
    save_remap_bindings(paths, d.id.vendor_id, d.pids[0], &saved_list(d, bindings)?)
}

/// What [`persist`] writes; an error while the file could not be read.
fn saved_list(d: &DevState, bindings: &HashMap<u16, RemapBinding>) -> Result<Vec<RemapBinding>> {
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
    Ok(list)
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
    /// Grab attempts in a row that found a key held down.
    held_retries: u32,
}

impl Slot {
    /// A key was down at grab time: retry soon, then (a key that never comes
    /// up) every [`OPEN_RETRY`]. True once the wait should be reported.
    fn key_held(&mut self, now: Instant) -> bool {
        self.held_retries = self.held_retries.saturating_add(1);
        let report = self.held_retries >= HELD_REPORT_AFTER;
        self.retry = now + if report { OPEN_RETRY } else { GRAB_RETRY };
        report
    }
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
    /// An error when none of the device's own nodes could be opened because
    /// this user cannot read them.
    fn open(idx: usize, slot: &Slot) -> Result<Self, String> {
        let nodes: Vec<(Device, bool)> = evdev::enumerate()
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
        if !nodes.iter().any(|(_, own)| *own) {
            if let Some(msg) = unreadable_message(&unreadable_nodes(slot.id.vendor_id, &slot.pids))
            {
                return Err(msg);
            }
        }
        Ok(Self {
            idx,
            nodes,
            rec: Recorder::default(),
            started: Instant::now(),
        })
    }
}

struct Worker {
    inner: Arc<Mutex<EngineInner>>,
    edits: Arc<Mutex<()>>,
    events: broadcast::Sender<AdapterEvent>,
    slots: Vec<Slot>,
    virt: Option<VirtualDevice>,
    /// Axes `virt` declares beyond the fixed pointer set.
    axes: Axes,
    mapper: Mapper,
    session: Option<Session>,
}

impl Worker {
    fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        // A panic in every pass is pushed once, not every retry.
        let mut reported = false;
        loop {
            match catch_unwind(AssertUnwindSafe(|| self.step(&mut rx))) {
                Ok(true) => reported = false,
                Ok(false) => return,
                // A bug in one pass must not end remapping until the daemon
                // restarts: let go of input and the session, then carry on.
                Err(_) => {
                    error!("remap thread panicked; releasing grabs");
                    // A session's own error event says what stopped.
                    if let Some(sess) = self.session.take() {
                        self.update(sess.idx, |d| {
                            d.capturing = None;
                            d.capture_source = None;
                            d.last_error = Some("Listening stopped after an internal error".into());
                        });
                    } else if !reported {
                        let _ = self.events.send(AdapterEvent::Error {
                            device: None,
                            message: "Remapping hit an internal error: held keys were released \
                                      and remaps resume in a moment."
                                .into(),
                        });
                    }
                    reported = true;
                    self.ungrab_all();
                    // ponytail: fixed pause so a panic that repeats every pass
                    // cannot spin; nothing is lost, commands queue meanwhile.
                    std::thread::sleep(OPEN_RETRY);
                }
            }
        }
    }

    /// One pass: queued commands, then input. False after `Shutdown`.
    fn step(&mut self, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> bool {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Cmd::Shutdown => {
                    self.session = None;
                    self.ungrab_all();
                    return false;
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
                            held_retries: 0,
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
                    self.session = None;
                    match Session::open(idx, &self.slots[idx]) {
                        Ok(sess) => self.session = Some(sess),
                        Err(e) => self.update(idx, |d| {
                            d.capturing = None;
                            d.capture_source = None;
                            d.last_error = Some(e);
                        }),
                    }
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
        true
    }

    fn set_bindings(&mut self, idx: usize, bindings: HashMap<u16, RemapBinding>) {
        let slot = &mut self.slots[idx];
        self.mapper.stop_device(idx);
        self.mapper
            .release_forwarded(idx, |code| bindings.contains_key(&code));
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
        let mut excluded = 0;
        let nodes: Vec<(PathBuf, Device)> = evdev::enumerate()
            .filter(|(_, dev)| {
                let own = own_node(&self.slots[idx], dev);
                let ok = own && forwards_all(dev);
                excluded += usize::from(own && !ok);
                ok
            })
            .collect();
        if nodes.is_empty() {
            let slot = &mut self.slots[idx];
            slot.retry = now + OPEN_RETRY;
            // Present but unreadable (no udev rule for this device), or
            // never grabbed: say so, or the bindings silently do nothing.
            let msg = unreadable_message(&unreadable_nodes(slot.id.vendor_id, &slot.pids)).or_else(
                || {
                    (excluded > 0).then(|| {
                        "Remaps cannot apply: this device's input also carries joystick, \
                         gamepad or tablet codes the virtual device cannot forward, so it \
                         is never grabbed."
                            .to_string()
                    })
                },
            );
            if let Some(msg) = msg {
                self.report(idx, msg);
            }
            return;
        }
        // Grabbing mid-press would leave that key stuck down in the compositor.
        if let Some(code) = held_key(nodes.iter().map(|(_, d)| d)) {
            self.wait_for_release(idx, code, now);
            return;
        }
        if !self.ensure_virtual(idx, &nodes) {
            self.slots[idx].retry = now + OPEN_RETRY;
            return;
        }
        let slot = &mut self.slots[idx];
        slot.retry = now + OPEN_RETRY;
        let mut grabbed = Vec::new();
        let mut err = None;
        for (path, mut dev) in nodes {
            match dev.grab() {
                Ok(()) => grabbed.push((path, dev)),
                Err(e) => {
                    warn!("grab {} ({}): {e}", slot.id.key(), path.display());
                    err = Some(format!("grab {}: {e}", path.display()));
                }
            }
        }
        // All or nothing: a node left out would bypass the remaps, and a
        // device that counts as grabbed is never retried. A key pressed
        // while the virtual device was built reached the compositor as down:
        // its release must too.
        let held = held_key(grabbed.iter().map(|(_, d)| d));
        if err.is_some() || held.is_some() {
            for (_, dev) in &mut grabbed {
                let _ = dev.ungrab();
            }
            match (err, held) {
                (Some(e), _) => self.report(idx, e),
                (None, Some(code)) => self.wait_for_release(idx, code, now),
                (None, None) => {}
            }
            return;
        }
        for (path, mut dev) in grabbed {
            let _ = dev.set_nonblocking(true);
            // Queued since the node was opened, so the compositor saw it.
            for _ in 0..64 {
                if !dev.fetch_events().is_ok_and(|evs| evs.count() > 0) {
                    break;
                }
            }
            slot.nodes.push(dev);
            slot.paths.push(path);
        }
        slot.held_retries = 0;
        info!(
            "grabbed {} evdev node(s) of {} for remaps",
            slot.nodes.len(),
            slot.id.key()
        );
        self.update(idx, |d| {
            d.grabbed = true;
            d.last_error = None;
        });
    }

    /// A key is down on the device: grab later (see [`Slot::key_held`]).
    fn wait_for_release(&mut self, idx: usize, code: u16, now: Instant) {
        if self.slots[idx].key_held(now) {
            let msg = format!(
                "Waiting for {} to be released before remapping (it reports held down).",
                EvdevKey(code).label()
            );
            self.report(idx, msg);
        }
    }

    /// Set `last_error` unless it already says `msg` (each retry repeats it).
    fn report(&self, idx: usize, msg: String) {
        let changed = lock(&self.inner).devices[idx].last_error.as_deref() != Some(msg.as_str());
        if changed {
            self.update(idx, |d| d.last_error = Some(msg));
        }
    }

    /// A virtual device that declares every axis of `nodes` (uinput drops
    /// events it does not declare), rebuilt when they add one. False (error
    /// reported on `idx`) when it cannot be created.
    fn ensure_virtual(&mut self, idx: usize, nodes: &[(PathBuf, Device)]) -> bool {
        let mut axes = self.axes.clone();
        let grew = nodes
            .iter()
            .fold(false, |grew, (_, dev)| axes.merge(Axes::of(dev)) || grew);
        if self.virt.is_some() && !grew {
            return true;
        }
        if self.virt.is_some() {
            // ponytail: keys held through other grabbed devices are released
            // by the swap, like `ungrab`.
            self.mapper.release_all();
            self.flush();
            self.virt = None;
        }
        match build_virtual(&axes) {
            Ok(v) => {
                self.virt = Some(v);
                self.axes = axes;
                true
            }
            Err(e) => {
                warn!("uinput: {e}");
                self.axes = Axes::default();
                // The old device is gone: so is every grab forwarding into it.
                for i in 0..self.slots.len() {
                    self.drop_nodes(i);
                    self.slots[i].retry = Instant::now() + OPEN_RETRY;
                }
                self.update(idx, |d| {
                    d.last_error = Some(format!(
                        "Cannot create virtual device ({e}). Need write access to /dev/uinput: install the hako udev rules (udev/70-hako.rules gives the logged-in user access)."
                    ))
                });
                false
            }
        }
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
            self.axes = Axes::default();
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
        let (snap, error) = {
            let mut g = lock(&self.inner);
            let d = &mut g.devices[idx];
            let before = d.last_error.clone();
            f(d);
            (d.snapshot(), d.error_event(before.as_deref()))
        };
        let _ = self.events.send(AdapterEvent::RemapsUpdated(snap));
        if let Some(ev) = error {
            let _ = self.events.send(ev);
        }
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
                                None => self.mapper.forward(idx, ev),
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
        let (inner, edits) = (self.inner.clone(), self.edits.clone());
        // A captured target is saved here; no input is grabbed meanwhile.
        let _edits = lock_edits(&edits);
        let mut guard = lock(&inner);
        let g = &mut *guard;
        let d = &mut g.devices[idx];
        // The API may have cancelled before our Cancel command arrives.
        let Some(role) = d.capturing else {
            return false;
        };
        let before = d.last_error.clone();
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
                CaptureRole::Record => record_step(d, &mut sess.rec, at, key, down),
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
        sent.extend(d.error_event(before.as_deref()));
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
            self.axes = Axes::default();
            self.mapper = Mapper::default();
            for idx in 0..self.slots.len() {
                if !self.slots[idx].nodes.is_empty() {
                    self.drop_nodes(idx);
                    self.slots[idx].retry = Instant::now() + OPEN_RETRY;
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
    event_nodes(vendor, pids)
        .into_iter()
        .filter(|dev| !readable(dev))
        .collect()
}

/// What to tell the user about `blocked` input nodes (none: `None`).
fn unreadable_message(blocked: &[PathBuf]) -> Option<String> {
    let first = blocked.first()?;
    Some(format!(
        "Cannot read {} ({} input node(s)): add your user to the `input` group, or install the hako udev rules if this device is listed.",
        first.display(),
        blocked.len()
    ))
}

/// `/dev/input/event*` nodes of `vendor` + one of `pids` (sysfs).
fn event_nodes(vendor: u16, pids: &[u16]) -> Vec<PathBuf> {
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
        .collect()
}

/// The calling user may still open `path` (seat ACL / group).
fn readable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        // SAFETY: valid NUL-terminated path; access() only reads it.
        .is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::R_OK) } == 0)
}

/// A key or button `nodes` report held down.
fn held_key<'a>(nodes: impl IntoIterator<Item = &'a Device>) -> Option<u16> {
    nodes
        .into_iter()
        .find_map(|d| d.get_key_state().ok()?.iter().next().map(|k| k.code()))
}

/// One of `slot`'s nodes that may be grabbed.
fn node_matches(slot: &Slot, dev: &Device) -> bool {
    own_node(slot, dev) && forwards_all(dev)
}

/// A node with a key the virtual device cannot repeat (touchpad, joystick,
/// gamepad) is never grabbed: those events would be lost.
fn forwards_all(dev: &Device) -> bool {
    dev.supported_keys()
        .is_some_and(|keys| keys.iter().all(|k| forwardable(k.code())))
}

/// A node of `slot`'s device that carries its buttons / keys.
fn own_node(slot: &Slot, dev: &Device) -> bool {
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

/// Relative / absolute axes of grabbed nodes, so the virtual device can
/// declare them: uinput silently drops events of undeclared codes.
#[derive(Clone, Default)]
struct Axes {
    rel: BTreeSet<u16>,
    abs: Vec<UinputAbsSetup>,
}

impl Axes {
    fn of(dev: &Device) -> Self {
        Self {
            rel: dev
                .supported_relative_axes()
                .into_iter()
                .flat_map(|axes| axes.iter())
                .map(|axis| axis.0)
                .collect(),
            abs: dev
                .get_absinfo()
                .map(|axes| {
                    axes.map(|(code, info)| UinputAbsSetup::new(code, info))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Add `other`'s axes; true when any was new.
    fn merge(&mut self, other: Axes) -> bool {
        let mut grew = false;
        for code in other.rel {
            grew |= self.rel.insert(code);
        }
        for setup in other.abs {
            if !self.abs.iter().any(|a| a.code() == setup.code()) {
                self.abs.push(setup);
                grew = true;
            }
        }
        grew
    }
}

/// Keys the virtual device declares: every remap target plus the `BTN_MISC`
/// block (`BTN_0`..), which some mice use for extra buttons. Not the
/// joystick / gamepad / tablet ranges, so udev keeps it a keyboard + mouse.
fn forwardable(code: u16) -> bool {
    emittable(code) || (0x100..=0x10f).contains(&code)
}

fn build_virtual(axes: &Axes) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in (1u16..0x2c0).filter(|&c| forwardable(c)) {
        keys.insert(KeyCode::new(code));
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    rel.insert(RelativeAxisCode::REL_X);
    rel.insert(RelativeAxisCode::REL_Y);
    rel.insert(RelativeAxisCode::REL_WHEEL);
    rel.insert(RelativeAxisCode::REL_HWHEEL);
    rel.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rel.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);
    for &code in &axes.rel {
        rel.insert(RelativeAxisCode(code));
    }
    let mut msc = AttributeSet::<MiscCode>::new();
    msc.insert(MiscCode::MSC_SCAN);

    let mut builder = VirtualDevice::builder()?
        .name(VIRTUAL_NAME)
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .with_msc(&msc)?;
    for setup in &axes.abs {
        builder = builder.with_absolute_axis(setup)?;
    }
    Ok(builder.build()?)
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
    /// Tap sources held down: (slot index, source code) -> target.
    taps: HashMap<(usize, u16), u16>,
    /// Keys the virtual device holds, with how many owners (a physical key,
    /// a tap, a runner) hold each.
    held: BTreeMap<u16, u32>,
    /// Unbound physical keys forwarded down: (slot index, code) -> presses.
    forwarded: BTreeMap<(usize, u16), u32>,
    out: Vec<InputEvent>,
}

/// Count one owner's key edge; true when the virtual device must see it
/// (first press, last release, autorepeat).
fn hold<K: Ord>(held: &mut BTreeMap<K, u32>, code: K, value: i32) -> bool {
    match value {
        1 => {
            let n = held.entry(code).or_insert(0);
            *n += 1;
            *n == 1
        }
        0 => match held.get_mut(&code) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            _ => {
                held.remove(&code);
                true
            }
        },
        _ => true,
    }
}

fn push_key(out: &mut Vec<InputEvent>, held: &mut BTreeMap<u16, u32>, code: u16, value: i32) {
    if hold(held, code, value) {
        out.push(InputEvent::new(EventType::KEY.0, code, value));
    }
}

impl Mapper {
    /// Pass an unbound event of slot `dev` through unchanged.
    fn forward(&mut self, dev: usize, ev: InputEvent) {
        if ev.event_type() == EventType::KEY {
            hold(&mut self.forwarded, (dev, ev.code()), ev.value());
            if !hold(&mut self.held, ev.code(), ev.value()) {
                return;
            }
        }
        self.out.push(ev);
    }

    /// Release the keys slot `dev` forwarded down that are `bound` now:
    /// their physical release goes to the binding, which never pressed them.
    fn release_forwarded(&mut self, dev: usize, bound: impl Fn(u16) -> bool) {
        let ids: Vec<_> = self
            .forwarded
            .keys()
            .filter(|k| k.0 == dev && bound(k.1))
            .copied()
            .collect();
        for id in ids {
            for _ in 0..self.forwarded.remove(&id).unwrap_or(0) {
                push_key(&mut self.out, &mut self.held, id.1, 0);
            }
        }
    }

    /// A tap source's edge, sent as its target's.
    fn tap(&mut self, id: (usize, u16), target: u16, value: i32) {
        let target = match value {
            1 => self.taps.insert(id, target).is_none().then_some(target),
            0 => self.taps.remove(&id),
            _ => self.taps.get(&id).copied(),
        };
        if let Some(t) = target {
            push_key(&mut self.out, &mut self.held, t, value);
        }
    }

    /// A bound source key changed (`value` 0 up, 1 down, 2 autorepeat).
    fn key(&mut self, dev: usize, b: &RemapBinding, value: i32, now: Instant) {
        let id = (dev, b.source.0);
        let running = self.runners.contains_key(&id);
        match (b.mode, value) {
            (RemapMode::Tap, _) => self.tap(id, b.target.0, value),
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

    /// Stop the runners and release the taps of slot `dev`.
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
        let taps: Vec<_> = self.taps.keys().filter(|k| k.0 == dev).copied().collect();
        for id in taps {
            self.tap(id, 0, 0);
        }
    }

    /// Stop every runner and release every key the virtual device holds.
    fn release_all(&mut self) {
        self.runners.clear();
        self.taps.clear();
        self.forwarded.clear();
        for k in std::mem::take(&mut self.held).into_keys() {
            self.out.push(InputEvent::new(EventType::KEY.0, k, 0));
        }
    }

    /// Emit every step that is due at `now`.
    fn tick(&mut self, now: Instant) {
        let mut done = Vec::new();
        for (id, r) in self.runners.iter_mut() {
            while now >= r.next {
                let s = r.steps[r.pos];
                // A runner owns each key once: repeated edges are no-ops.
                let owned = if s.down {
                    r.held.insert(s.key.0)
                } else {
                    r.held.remove(&s.key.0)
                };
                if owned {
                    push_key(&mut self.out, &mut self.held, s.key.0, i32::from(s.down));
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

/// Add one edge to a record take; false (the take is finished and the limit
/// reported) once it holds [`MAX_MACRO_STEPS`].
fn record_step(
    d: &mut DevState,
    rec: &mut Recorder,
    at: SystemTime,
    key: EvdevKey,
    down: bool,
) -> bool {
    let delay_ms = rec.delay(at);
    d.recorded.push(MacroStep {
        key,
        down,
        delay_ms,
    });
    if d.recorded.len() < MAX_MACRO_STEPS {
        return true;
    }
    d.recorded = finish_recording(std::mem::take(&mut d.recorded), false);
    d.capturing = None;
    d.capture_source = None;
    d.last_error = Some(format!(
        "Recording stopped: a macro holds at most {MAX_MACRO_STEPS} steps"
    ));
    false
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
        m.forward(0, InputEvent::new(EventType::KEY.0, A.0, 1));
        m.key(0, &RemapBinding::tap(EvdevKey::BTN_SIDE, B), 1, t0);
        let mac = macro_binding(
            vec![step(C, true, 0), step(C, false, 500)],
            MacroRepeat::Once,
            50,
        );
        m.key(1, &mac, 1, t0);
        m.take();
        assert_eq!(m.held.keys().copied().collect::<Vec<_>>(), vec![30, 46, 48]);
        m.release_all();
        assert_eq!(keys(&mut m), vec![(30, 0), (46, 0), (48, 0)]);
        assert!(m.held.is_empty() && m.runners.is_empty());
    }

    #[test]
    fn stop_device_only_touches_its_keys() {
        let t0 = Instant::now();
        let mut m = Mapper::default();
        m.forward(0, InputEvent::new(EventType::KEY.0, A.0, 1));
        m.key(0, &RemapBinding::tap(EvdevKey::BTN_SIDE, B), 1, t0);
        let mac = macro_binding(
            vec![step(C, true, 0), step(C, false, 500)],
            MacroRepeat::Once,
            50,
        );
        m.key(1, &mac, 1, t0);
        m.take();
        m.stop_device(1);
        assert_eq!(keys(&mut m), vec![(46, 0)]);
        assert_eq!(m.held.keys().copied().collect::<Vec<_>>(), vec![30, 48]);
        // Slot 0's tap is released with its slot, the physical key stays.
        m.stop_device(0);
        assert_eq!(keys(&mut m), vec![(48, 0)]);
        assert_eq!(m.held.keys().copied().collect::<Vec<_>>(), vec![30]);
    }

    #[test]
    fn binding_a_held_key_releases_it() {
        let t0 = Instant::now();
        let mut m = Mapper::default();
        let down = |k: EvdevKey| InputEvent::new(EventType::KEY.0, k.0, 1);
        m.forward(0, down(A));
        m.forward(1, down(B));
        m.take();
        // SetRemap binds A and B on slot 0: only slot 0's A was forwarded.
        m.release_forwarded(0, |c| c == A.0 || c == B.0);
        assert_eq!(keys(&mut m), vec![(30, 0)]);
        // A's physical release now reaches the binding, which holds nothing.
        m.key(0, &RemapBinding::tap(A, C), 0, t0);
        assert!(keys(&mut m).is_empty());
        assert_eq!(m.held.keys().copied().collect::<Vec<_>>(), vec![48]);
        assert_eq!(
            m.forwarded.keys().copied().collect::<Vec<_>>(),
            vec![(1, 48)]
        );
    }

    fn dev_state() -> DevState {
        DevState {
            id: DeviceId {
                vendor_id: 0x046d,
                product_id: 0x40a8,
                instance: "x".into(),
            },
            name: "Mouse".into(),
            pids: vec![0x40a8],
            bindings: HashMap::new(),
            capturing: None,
            capture_source: None,
            last_error: None,
            grabbed: false,
            recorded: Vec::new(),
            invalid: Vec::new(),
            load_error: None,
        }
    }

    #[test]
    fn a_key_that_stays_down_is_reported_and_retried_slower() {
        let t0 = Instant::now();
        let mut slot = Slot {
            id: dev_state().id,
            kind: DeviceKind::Keyboard,
            pids: vec![0x40a8],
            bindings: HashMap::new(),
            nodes: Vec::new(),
            paths: Vec::new(),
            retry: t0,
            held_retries: 0,
        };
        for _ in 1..HELD_REPORT_AFTER {
            assert!(!slot.key_held(t0));
            assert_eq!(slot.retry, t0 + GRAB_RETRY);
        }
        assert!(slot.key_held(t0));
        assert_eq!(slot.retry, t0 + OPEN_RETRY);
    }

    #[test]
    fn recording_stops_at_the_step_limit() {
        let mut d = dev_state();
        d.capturing = Some(CaptureRole::Record);
        let mut rec = Recorder::default();
        let t0 = SystemTime::UNIX_EPOCH;
        for i in 0..MAX_MACRO_STEPS - 1 {
            assert!(record_step(&mut d, &mut rec, t0, A, i % 2 == 0));
        }
        assert!(!record_step(&mut d, &mut rec, t0, A, false));
        assert_eq!(d.recorded.len(), MAX_MACRO_STEPS);
        assert!(d.capturing.is_none());
        assert!(d.last_error.as_deref().is_some_and(|e| e.contains("256")));
    }

    #[test]
    fn unreadable_remap_file_is_reloaded_on_reattach() {
        let root = std::env::temp_dir().join(format!("hako-remap-reload-{}", std::process::id()));
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
        let mut d = dev_state();
        d.load_error = Some(err.clone());
        d.last_error = Some(err);
        assert!(!retry_load(&paths, &mut d));
        assert!(d.load_error.is_some() && d.last_error.is_some());

        std::fs::write(
            &file,
            "version = 1\n[[bindings]]\nsource = 276\ntarget = 30\nmode = \"tap\"\n",
        )
        .unwrap();
        assert!(retry_load(&paths, &mut d));
        assert!(d.load_error.is_none() && d.last_error.is_none());
        assert!(d.bindings.contains_key(&276));
        persist(&paths, &d, &d.bindings).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shared_key_goes_up_with_its_last_holder() {
        let t0 = Instant::now();
        let mut m = Mapper::default();
        let ctrl = EvdevKey(29);
        let side = RemapBinding::tap(EvdevKey::BTN_SIDE, ctrl);
        let extra = RemapBinding::tap(EvdevKey::BTN_EXTRA, ctrl);
        m.key(0, &side, 1, t0);
        m.key(0, &extra, 1, t0);
        m.key(0, &side, 0, t0);
        assert_eq!(
            keys(&mut m),
            vec![(29, 1)],
            "still held by the other button"
        );
        m.key(0, &extra, 0, t0);
        assert_eq!(keys(&mut m), vec![(29, 0)]);

        // A macro pressing a physically held key does not release it.
        m.forward(0, InputEvent::new(EventType::KEY.0, 29, 1));
        let mac = macro_binding(
            vec![step(ctrl, true, 0), step(ctrl, false, 10)],
            MacroRepeat::Once,
            50,
        );
        m.key(1, &mac, 1, t0);
        m.tick(at(t0, 10));
        assert_eq!(keys(&mut m), vec![(29, 1)]);
        m.forward(0, InputEvent::new(EventType::KEY.0, 29, 0));
        assert_eq!(keys(&mut m), vec![(29, 0)]);
        assert!(m.held.is_empty());
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
            name: "Mouse".into(),
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

        // A grab error is pushed when it appears or changes, not on retries.
        d.last_error = Some("grab: busy".into());
        assert!(matches!(
            d.error_event(None),
            Some(AdapterEvent::Error { message, .. }) if message == "Mouse remaps: grab: busy"
        ));
        assert!(d.error_event(Some("grab: busy")).is_none());
        assert!(d.error_event(Some("uinput: gone")).is_some());
        d.last_error = None;
        assert!(d.error_event(Some("grab: busy")).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// An engine with one attached keyboard no input node belongs to.
    fn engine(name: &str) -> (RemapEngine, ConfigPaths, DeviceId) {
        let root = std::env::temp_dir().join(format!("hako-remap-{name}-{}", std::process::id()));
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        };
        paths.ensure().unwrap();
        let engine = RemapEngine::start(paths.clone(), broadcast::channel(64).0);
        let id = DeviceId {
            vendor_id: 0x1234,
            product_id: 0x5678,
            instance: "x".into(),
        };
        engine.attach(id.clone(), DeviceKind::Keyboard, vec![0x5678], "Keyboard");
        (engine, paths, id)
    }

    #[test]
    fn a_closed_connection_stops_only_its_own_capture() {
        let (engine, paths, id) = engine("session");
        let (_, first) = engine
            .start_capture(id.clone(), CaptureRole::Source, None)
            .unwrap();
        // It ended on its own (captured / Esc / timeout), then another started.
        lock(&engine.inner).devices[0].capturing = None;
        let (_, second) = engine
            .start_capture(id.clone(), CaptureRole::Source, None)
            .unwrap();
        assert!(engine.cancel_session(id.clone(), first).unwrap().is_none());
        assert!(engine.snapshot(&id).unwrap().capturing.is_some());
        let snap = engine.cancel_session(id.clone(), second).unwrap().unwrap();
        assert!(snap.capturing.is_none());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn editing_bindings_mid_recording_drops_the_take() {
        let (engine, paths, id) = engine("edit-record");
        engine
            .start_capture(id.clone(), CaptureRole::Record, None)
            .unwrap();
        lock(&engine.inner).devices[0].recorded = vec![step(A, false, 0), step(A, true, 40)];
        let snap = engine
            .set_binding(id.clone(), RemapBinding::tap(EvdevKey(30), B))
            .unwrap();
        assert!(snap.capturing.is_none() && snap.recorded.is_empty());
        engine.shutdown();
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn axes_merge_reports_only_new_axes() {
        use evdev::{AbsInfo, AbsoluteAxisCode};
        let abs =
            |code| UinputAbsSetup::new(AbsoluteAxisCode(code), AbsInfo::new(0, 0, 100, 0, 0, 0));
        let volume = || Axes {
            rel: BTreeSet::from([0x07]), // REL_DIAL
            abs: vec![abs(0x20)],        // ABS_VOLUME
        };
        let mut a = Axes::default();
        assert!(a.merge(volume()));
        assert!(!a.merge(volume()), "nothing new: no rebuild");
        assert!(a.merge(Axes {
            rel: BTreeSet::from([0x08]),
            abs: vec![abs(0x20)],
        }));
        assert_eq!((a.rel.len(), a.abs.len()), (2, 1));
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
