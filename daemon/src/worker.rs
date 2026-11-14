//! One OS thread per device, so blocking HID I/O never stalls IPC or another
//! device. The hub only touches a [`DeviceHandle`]: a cached snapshot plus a
//! command channel.
//!
//! The worker also owns the onboard-memory gate: controls a driver marks
//! `persistent` (they write the device's flash / EEPROM / profiles) are
//! refused until the owner turns on that device's `onboard_writes` opt-in,
//! which is stored in `config.toml`, never by the driver. A driver's
//! `save_to_device` mode (store every later change in onboard memory) is such a
//! control, and is switched off whenever the opt-in is not (or no longer) given.

use crate::drivers::{DeviceAdapter, EventTx, Model};
use anyhow::{anyhow, bail, Context, Result};
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue};
use hako_core::config::{load_app_config, load_device_settings, update_app_config, ConfigPaths};
use hako_core::device::DeviceSnapshot;
use hako_core::protocol::{is_new_error, AdapterEvent};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

/// Confirmation shown before a device may save settings to its own memory.
pub const ONBOARD_WARNING: &str =
    "Settings marked \"Saved to device\" are written to this device's \
onboard memory (flash / EEPROM / onboard profiles). They persist after unplugging and on other \
computers, every change wears the memory slightly, and a faulty write could leave the device \
misconfigured until it is reset with the maker's software. hako only offers writes whose \
memory layout several independent sources confirm, and only writes when you change a setting \
(never automatically). Continue?";

/// A request refused before it reached the device, with the IPC error code
/// that names whose fault it is.
#[derive(Debug)]
pub struct Refused {
    code: &'static str,
    message: String,
}

impl Refused {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// IPC code for `e`: the refusal's own, else `io` (the device failed).
    pub fn code(e: &anyhow::Error) -> &'static str {
        e.downcast_ref::<Self>().map_or("io", |r| r.code)
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

/// Longest an IPC `Set` waits for the device thread (it may be mid-tick).
const SET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// A device whose ticks failed counts as recovered (its next failure is
/// pushed again) only after this long without one: drivers poll on timers,
/// so the ticks in between succeed.
const RECOVERED_AFTER: Duration = Duration::from_secs(60);

type Reply = oneshot::Sender<Result<DeviceSnapshot>>;

enum Cmd {
    Set {
        key: SettingKey,
        value: SettingValue,
        reply: Reply,
    },
    SetMany {
        values: Vec<(SettingKey, SettingValue)>,
        reply: Reply,
    },
    Shutdown(mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct DeviceHandle {
    pub model: &'static Model,
    /// sysfs paths of the model's HID nodes when it was opened; a replug
    /// creates new ones, which is how the hub notices it.
    pub nodes: Vec<PathBuf>,
    snapshot: Arc<Mutex<DeviceSnapshot>>,
    tx: mpsc::Sender<Cmd>,
}

impl DeviceHandle {
    /// Start the device's worker. Its first writes (leaving an unallowed
    /// save-to-device mode, repairing saved values) run on the worker thread:
    /// blocking HID stays off the caller's, and a driver that panics there
    /// fails this open only. Blocking (up to `SET_TIMEOUT`).
    pub fn spawn(
        model: &'static Model,
        adapter: Box<dyn DeviceAdapter>,
        events: EventTx,
        nodes: Vec<PathBuf>,
        paths: ConfigPaths,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        std::thread::Builder::new()
            .name("hako-dev".into())
            .spawn(move || {
                let Ok((mut adapter, gate, shared)) =
                    catch_unwind(AssertUnwindSafe(|| start(adapter, paths, &events)))
                else {
                    error!("device worker panicked while starting");
                    return;
                };
                // The caller gave up waiting: nobody will drive this adapter.
                if ready.send(shared.clone()).is_err() {
                    adapter.shutdown();
                    return;
                }
                let fallback = shared.clone();
                let ev = events.clone();
                // A driver bug must not leave a handle that looks connected
                // forever: report the device gone so discovery reopens it.
                if catch_unwind(AssertUnwindSafe(|| run(adapter, gate, rx, shared, events)))
                    .is_err()
                {
                    error!("device worker panicked");
                    let mut snap = fallback.lock().unwrap_or_else(|p| p.into_inner());
                    snap.connected = false;
                    snap.last_error = Some("driver crashed; reopening".into());
                    let _ = ev.send(AdapterEvent::Updated(snap.clone()));
                    let mut crashed = CRASHED.lock().unwrap_or_else(|p| p.into_inner());
                    let (n, at) = crashed
                        .get_or_insert_with(HashMap::new)
                        .entry(snap.id.key())
                        .or_insert((0, Instant::now()));
                    *n += 1;
                    *at = Instant::now();
                    if *n == 1 {
                        let _ = ev.send(AdapterEvent::Error {
                            device: Some(snap.id.clone()),
                            message: format!("{}: driver crashed; reopening it", snap.name),
                        });
                    }
                }
            })
            .context("spawn device thread")?;
        let snapshot = match started.recv_timeout(SET_TIMEOUT) {
            Ok(snapshot) => snapshot,
            Err(RecvTimeoutError::Timeout) => bail!("the device did not answer while opening"),
            Err(RecvTimeoutError::Disconnected) => bail!("driver crashed while opening"),
        };
        Ok(Self {
            model,
            nodes,
            snapshot,
            tx,
        })
    }

    /// Last published state; never waits on the device.
    pub fn snapshot(&self) -> DeviceSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub async fn set(&self, key: SettingKey, value: SettingValue) -> Result<DeviceSnapshot> {
        self.call(|reply| Cmd::Set { key, value, reply }).await
    }

    /// Every value is checked before any is written; the driver writes them together.
    pub async fn set_many(
        &self,
        values: Vec<(SettingKey, SettingValue)>,
    ) -> Result<DeviceSnapshot> {
        self.call(|reply| Cmd::SetMany { values, reply }).await
    }

    async fn call(
        &self,
        cmd: impl FnOnce(oneshot::Sender<Result<DeviceSnapshot>>) -> Cmd,
    ) -> Result<DeviceSnapshot> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(cmd(reply))
            .map_err(|_| Refused::new("io", STOPPED))?;
        // A wedged device must not hang the IPC connection that asked.
        match tokio::time::timeout(SET_TIMEOUT, rx).await {
            Ok(reply) => reply.map_err(|_| Refused::new("io", STOPPED))?,
            Err(_) => Err(Refused::new("busy", "the device did not answer in time").into()),
        }
    }

    /// `e` (from `set`) says the worker did not answer at all (wedged or
    /// stopped), so later writes to it would fail the same way.
    pub fn unresponsive(e: &anyhow::Error) -> bool {
        e.downcast_ref::<Refused>()
            .is_some_and(|r| r.code == "busy" || r.message == STOPPED)
    }

    /// Ask the worker to release the device; the receiver fires when done.
    pub fn shutdown(&self) -> mpsc::Receiver<()> {
        let (ack, done) = mpsc::channel();
        let _ = self.tx.send(Cmd::Shutdown(ack));
        done
    }
}

const STOPPED: &str = "device worker stopped";

/// Driver crashes per device key (count, last one), until a reopened worker
/// has run for `RECOVERED_AFTER`: a driver that crashes soon after every
/// reopen is reported once, and reopened with backoff (`crashes`).
static CRASHED: Mutex<Option<HashMap<String, (u32, Instant)>>> = Mutex::new(None);

/// Consecutive driver crashes of the device with key `key`, and the last one.
pub fn crashes(key: &str) -> Option<(u32, Instant)> {
    CRASHED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()?
        .get(key)
        .copied()
}

/// The worker's state before it takes requests: the opt-in loaded and
/// enforced, saved values repaired, the first snapshot published.
fn start(
    mut adapter: Box<dyn DeviceAdapter>,
    paths: ConfigPaths,
    events: &EventTx,
) -> (Box<dyn DeviceAdapter>, Gate, Arc<Mutex<DeviceSnapshot>>) {
    let mut gate = Gate::load(paths, adapter.id().config_stem());
    if let Err(e) = gate.enforce(adapter.as_mut()) {
        warn!("{}: {e:#}", gate.stem);
        let snap = adapter.snapshot();
        let _ = events.send(AdapterEvent::Error {
            device: Some(snap.id),
            message: format!(
                "{}: {e:#}; its changes are refused until it does",
                snap.name
            ),
        });
    }
    repair_saved(adapter.as_mut(), &mut gate);
    let shared = Arc::new(Mutex::new(gate.view(adapter.as_ref())));
    (adapter, gate, shared)
}

/// Bumped whenever any worker changes an opt-in, so workers sharing a `vid_pid`
/// stem (identical devices) reload theirs.
static OPT_IN_EPOCH: AtomicUsize = AtomicUsize::new(0);

/// The device's onboard-memory opt-in.
struct Gate {
    paths: ConfigPaths,
    /// `vid_pid` the opt-in is stored under.
    stem: String,
    allowed: bool,
    /// `OPT_IN_EPOCH` when `allowed` was read.
    epoch: usize,
}

impl Gate {
    fn load(paths: ConfigPaths, stem: String) -> Self {
        let epoch = OPT_IN_EPOCH.load(Ordering::SeqCst);
        let allowed = load_app_config(&paths).is_ok_and(|c| c.onboard_writes.contains(&stem));
        Self {
            paths,
            stem,
            allowed,
            epoch,
        }
    }

    /// Pick up an opt-in another worker changed, leaving save mode if revoked.
    fn refresh(&mut self, a: &mut dyn DeviceAdapter) {
        if self.epoch != OPT_IN_EPOCH.load(Ordering::SeqCst) {
            *self = Self::load(self.paths.clone(), std::mem::take(&mut self.stem));
            // A failure is retried (and refuses the write) in `check`.
            if let Err(e) = self.enforce(a) {
                warn!("{}: {e:#}", self.stem);
            }
        }
    }

    /// The adapter's snapshot, plus the opt-in toggle when any control it
    /// offers writes onboard memory.
    fn view(&self, a: &dyn DeviceAdapter) -> DeviceSnapshot {
        let mut s = a.snapshot();
        if s.controls
            .iter()
            .any(|c| c.persistent && c.blocked.is_none())
        {
            s.controls.push(
                SettingMeta::toggle(SettingKey::OnboardWrites, "Allow saving to device memory")
                    .warning(ONBOARD_WARNING),
            );
            s.settings.insert(
                SettingKey::OnboardWrites.as_str().into(),
                SettingValue::Bool(self.allowed),
            );
        }
        s
    }

    /// The device saves every change to its onboard memory without the opt-in.
    fn unallowed_saving(&self, a: &dyn DeviceAdapter) -> bool {
        !self.allowed
            && a.snapshot()
                .settings
                .get(SettingKey::SaveToDevice.as_str())
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    }

    /// Without the opt-in, a driver's `save_to_device` mode must be off.
    fn enforce(&self, a: &mut dyn DeviceAdapter) -> Result<()> {
        if self.unallowed_saving(a) {
            a.set(SettingKey::SaveToDevice, SettingValue::Bool(false))
                .context("could not leave save-to-device mode")?;
        }
        Ok(())
    }

    /// Turn the opt-in on or off (saved first, so a failed save changes nothing).
    fn set(&mut self, on: bool) -> Result<()> {
        let stem = self.stem.clone();
        update_app_config(&self.paths, |cfg| {
            cfg.onboard_writes.retain(|s| *s != stem);
            if on {
                cfg.onboard_writes.push(stem);
            }
            Ok(())
        })?;
        info!(
            "onboard memory writes {} for {}",
            if on { "allowed" } else { "disallowed" },
            self.stem
        );
        self.allowed = on;
        self.epoch = OPT_IN_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(())
    }
}

fn run(
    mut adapter: Box<dyn DeviceAdapter>,
    mut gate: Gate,
    rx: mpsc::Receiver<Cmd>,
    shared: Arc<Mutex<DeviceSnapshot>>,
    events: EventTx,
) {
    // Returns the new snapshot when it differs from the published one.
    let publish = |snap: DeviceSnapshot| {
        let mut cur = shared.lock().unwrap_or_else(|p| p.into_inner());
        (*cur != snap).then(|| {
            *cur = snap.clone();
            snap
        })
    };
    // The hub announces a successful write; a failed one may still have
    // changed the snapshot (e.g. the driver lost its device).
    let settle = |result: Result<()>, snap: DeviceSnapshot, reply: Reply| {
        if let (Some(snap), true) = (publish(snap.clone()), result.is_err()) {
            let _ = events.send(AdapterEvent::Updated(snap));
        }
        let _ = reply.send(result.map(|()| snap));
    };
    let mut next_tick = Instant::now() + adapter.tick_interval();
    // Tick error last pushed as an `Error` event, and when a tick last failed.
    let mut reported = None;
    let mut failed = Instant::now();
    let started = Instant::now();
    let mut crash_cleared = false;
    loop {
        match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            // The caller timed out and was told the write failed: it must
            // not land on the device later.
            Ok(Cmd::Set { key, reply, .. }) if reply.is_closed() => {
                info!(
                    "{}: dropped {} nobody waits for",
                    adapter.id().key(),
                    key.as_str()
                );
            }
            Ok(Cmd::Set { key, value, reply }) => {
                let result = set_checked(adapter.as_mut(), &mut gate, key, value);
                settle(result, gate.view(adapter.as_ref()), reply);
            }
            Ok(Cmd::SetMany { reply, .. }) if reply.is_closed() => {
                info!("{}: dropped a batch nobody waits for", adapter.id().key());
            }
            Ok(Cmd::SetMany { values, reply }) => {
                let result = set_many_checked(adapter.as_mut(), &mut gate, &values);
                settle(result, gate.view(adapter.as_ref()), reply);
            }
            Ok(Cmd::Shutdown(ack)) => {
                adapter.shutdown();
                publish(gate.view(adapter.as_ref()));
                let _ = ack.send(());
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                next_tick = Instant::now() + adapter.tick_interval();
                gate.refresh(adapter.as_mut());
                let ticked = adapter.tick();
                let snap = gate.view(adapter.as_ref());
                if !crash_cleared && started.elapsed() >= RECOVERED_AFTER {
                    crash_cleared = true;
                    if let Some(crashed) =
                        CRASHED.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
                    {
                        crashed.remove(&snap.id.key());
                    }
                }
                match ticked {
                    Err(e) => {
                        warn!("tick {}: {e:#}", adapter.id().key());
                        failed = Instant::now();
                        let error = format!("{e:#}");
                        if is_new_error(&mut reported, Some(&error)) {
                            let _ = events.send(AdapterEvent::Error {
                                device: Some(snap.id.clone()),
                                message: format!("{}: {error}", snap.name),
                            });
                        }
                    }
                    Ok(()) if snap.last_error.is_none() && failed.elapsed() >= RECOVERED_AFTER => {
                        is_new_error(&mut reported, None);
                    }
                    Ok(()) => {}
                }
                if let Some(snap) = publish(snap) {
                    let _ = events.send(AdapterEvent::Updated(snap));
                }
            }
            // Handle dropped without Shutdown (replaced by a reopened adapter
            // that now owns the device and its audio): just let go.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A device file may hold a number its control no longer accepts (hand edits,
/// an older version's range): the driver already sent it while opening, so
/// write the nearest valid value back. Only sliders, only values that came
/// from the file, and never in save-to-device mode (no automatic onboard write).
/// ponytail: the first write at open is not prevented, only corrected.
fn repair_saved(a: &mut dyn DeviceAdapter, gate: &mut Gate) {
    let snap = gate.view(a);
    let saving =
        snap.settings.get(SettingKey::SaveToDevice.as_str()) == Some(&SettingValue::Bool(true));
    if !snap.connected || saving {
        return;
    }
    let file = load_device_settings(&gate.paths, snap.id.vendor_id, snap.id.product_id)
        .unwrap_or_default();
    for meta in &snap.controls {
        let key = meta.key.as_str();
        let Some(value) = snap.settings.get(key).filter(|v| file.get(key) == Some(*v)) else {
            continue;
        };
        let Some(fixed) = meta.nearest_valid(value) else {
            continue;
        };
        warn!(
            "{}: saved {key} {value:?} is invalid; setting {fixed:?}",
            snap.name
        );
        if let Err(e) = set_checked(a, gate, meta.key, fixed) {
            warn!("{}: could not repair {key}: {e:#}", snap.name);
        }
    }
}

/// Every write is checked against the control the driver declares for it, so
/// drivers only see values inside their own advertised ranges and choices,
/// and never a write to onboard memory the owner has not allowed.
fn set_checked(
    a: &mut dyn DeviceAdapter,
    gate: &mut Gate,
    key: SettingKey,
    value: SettingValue,
) -> Result<()> {
    let values = [(key, value)];
    check(a, gate, &values)?;
    let [(key, value)] = values;
    if key == SettingKey::OnboardWrites {
        gate.set(value.as_bool().unwrap_or(false))?;
        return gate.enforce(a).map_err(|e| {
            anyhow!(
                "saved, but the device is still in save-to-device mode ({e:#}); its changes are \
                 refused until it leaves it"
            )
        });
    }
    a.set(key, value)
}

/// [`set_checked`] for several values: all are checked before the driver
/// writes any. The opt-in is a decision of its own, never part of a batch.
fn set_many_checked(
    a: &mut dyn DeviceAdapter,
    gate: &mut Gate,
    values: &[(SettingKey, SettingValue)],
) -> Result<()> {
    for (i, (key, _)) in values.iter().enumerate() {
        if *key == SettingKey::OnboardWrites {
            return Err(Refused::new("invalid_value", "onboard_writes is set on its own").into());
        }
        if values[..i].iter().any(|(k, _)| k == key) {
            let e = format!("{} is given twice", key.as_str());
            return Err(Refused::new("invalid_value", e).into());
        }
    }
    check(a, gate, values)?;
    a.set_many(values)
}

/// The device is connected and offers a control for every value, which
/// accepts it and may be written now.
fn check(
    a: &mut dyn DeviceAdapter,
    gate: &mut Gate,
    values: &[(SettingKey, SettingValue)],
) -> Result<()> {
    gate.refresh(a);
    let mut snap = gate.view(a);
    if !snap.connected {
        bail!(
            "{} is not connected{}",
            snap.name,
            snap.last_error
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        );
    }
    // Every write would land in onboard memory without the owner's consent:
    // only leaving that mode (or deciding the opt-in) is let through.
    if gate.unallowed_saving(a) {
        let exempt = matches!(
            values,
            [(SettingKey::SaveToDevice, SettingValue::Bool(false))]
                | [(SettingKey::OnboardWrites, _)]
        );
        if let Err(e) = gate.enforce(a) {
            if !exempt {
                bail!(
                    "{} is in save-to-device mode without \"Allow saving to device memory\" and \
                     {e:#}: changes are refused until it leaves it",
                    snap.name
                );
            }
        }
        snap = gate.view(a);
    }
    for (key, value) in values {
        let meta = snap
            .controls
            .iter()
            .find(|c| c.key == *key)
            .ok_or_else(|| {
                Refused::new(
                    "invalid_value",
                    format!("{} has no {} control", snap.name, key.as_str()),
                )
            })?;
        meta.validate(value)
            .map_err(|e| Refused::new("invalid_value", e))?;
        let leaving_save_mode =
            *key == SettingKey::SaveToDevice && *value == SettingValue::Bool(false);
        if meta.persistent && !gate.allowed && !leaving_save_mode {
            return Err(Refused::new(
                "invalid_value",
                format!(
                    "{} is saved to the {}'s onboard memory: turn on \"Allow saving to device \
                     memory\" (Device tab) first",
                    meta.label, snap.name
                ),
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::device::{DeviceId, DeviceKind};
    use tokio::sync::broadcast;

    /// A device with one ordinary and one onboard-memory control.
    struct Fake {
        id: DeviceId,
        writes: Vec<SettingKey>,
        /// Keys of each `set_many`, shared so a test can watch the worker.
        batches: Arc<Mutex<Vec<Vec<SettingKey>>>>,
        saving: bool,
        /// Fails to leave save-to-device mode.
        stuck: bool,
    }

    impl DeviceAdapter for Fake {
        fn id(&self) -> &DeviceId {
            &self.id
        }
        fn snapshot(&self) -> DeviceSnapshot {
            DeviceSnapshot {
                id: self.id.clone(),
                kind: DeviceKind::Mouse,
                name: "Fake".into(),
                connected: true,
                batteries: vec![],
                settings: [(
                    SettingKey::SaveToDevice.as_str().to_string(),
                    SettingValue::Bool(self.saving),
                )]
                .into(),
                controls: vec![
                    SettingMeta::toggle(SettingKey::SaveToDevice, "Save to device").persistent(),
                    SettingMeta::slider(SettingKey::Dpi, "DPI", 100.0, 1000.0, 1.0),
                    SettingMeta::slider(SettingKey::DpiStage1, "Stage 1", 100.0, 1000.0, 1.0)
                        .persistent(),
                    SettingMeta::slider(SettingKey::DpiStage2, "Stage 2", 100.0, 1000.0, 1.0)
                        .persistent()
                        .blocked("layout unconfirmed"),
                ],
                last_error: None,
            }
        }
        fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
            if key == SettingKey::SaveToDevice {
                if self.stuck && value == SettingValue::Bool(false) {
                    bail!("no answer");
                }
                self.saving = value.as_bool().unwrap();
            }
            self.writes.push(key);
            Ok(())
        }
        fn set_many(&mut self, values: &[(SettingKey, SettingValue)]) -> Result<()> {
            let keys = values.iter().map(|(k, _)| *k).collect();
            self.batches.lock().unwrap().push(keys);
            Ok(())
        }
        fn tick(&mut self) -> Result<()> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    fn temp_paths(tag: &str) -> ConfigPaths {
        let root = std::env::temp_dir().join(format!("hako-{tag}-{}", std::process::id()));
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

    fn fake() -> Fake {
        Fake {
            id: DeviceId {
                vendor_id: 0x1234,
                product_id: 0x5678,
                instance: "x".into(),
            },
            writes: vec![],
            batches: Arc::default(),
            saving: false,
            stuck: false,
        }
    }

    #[test]
    fn a_device_stuck_in_save_mode_takes_no_changes_without_the_opt_in() {
        use SettingKey::{Dpi, OnboardWrites, SaveToDevice};
        let paths = temp_paths("stuck");
        let mut a = fake();
        let stem = a.id.config_stem();
        let mut gate = Gate::load(paths.clone(), stem.clone());
        let (on, off) = (SettingValue::Bool(true), SettingValue::Bool(false));
        set_checked(&mut a, &mut gate, OnboardWrites, on.clone()).unwrap();
        set_checked(&mut a, &mut gate, SaveToDevice, on).unwrap();
        a.stuck = true;
        a.writes.clear();

        // The revocation is saved, but the reply says the device kept saving.
        let e = set_checked(&mut a, &mut gate, OnboardWrites, off.clone()).unwrap_err();
        assert!(format!("{e:#}").contains("saved, but"), "{e:#}");
        assert!(!Gate::load(paths.clone(), stem).allowed);
        assert!(a.saving);

        // So nothing more is written (to onboard memory) without consent.
        assert!(set_checked(&mut a, &mut gate, Dpi, SettingValue::Int(400)).is_err());
        assert!(set_many_checked(&mut a, &mut gate, &[(Dpi, SettingValue::Int(400))]).is_err());
        assert!(a.writes.is_empty() && a.batches.lock().unwrap().is_empty());
        // Leaving the mode is let through (the device's own error comes back).
        let e = set_checked(&mut a, &mut gate, SaveToDevice, off).unwrap_err();
        assert!(format!("{e:#}").contains("no answer"), "{e:#}");

        // Once the device answers, the next write leaves the mode first.
        a.stuck = false;
        set_checked(&mut a, &mut gate, Dpi, SettingValue::Int(400)).unwrap();
        assert!(!a.saving);
        assert_eq!(a.writes, [SaveToDevice, Dpi]);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    /// Panics in `snapshot` (while opening) or in every `tick`.
    struct Crashy {
        fake: Fake,
        at_open: bool,
    }

    impl DeviceAdapter for Crashy {
        fn id(&self) -> &DeviceId {
            self.fake.id()
        }
        fn snapshot(&self) -> DeviceSnapshot {
            assert!(!self.at_open, "driver bug");
            self.fake.snapshot()
        }
        fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
            self.fake.set(key, value)
        }
        fn tick(&mut self) -> Result<()> {
            panic!("driver bug")
        }
        fn shutdown(&mut self) {}
        fn tick_interval(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[test]
    fn a_driver_crash_fails_its_open_or_is_counted_for_backoff() {
        let paths = temp_paths("crash");
        let model = crate::drivers::models().next().unwrap();
        let crashy = |instance: &str, at_open| {
            let mut fake = fake();
            fake.id.instance = instance.into();
            Box::new(Crashy { fake, at_open })
        };
        let events = broadcast::channel(16).0;
        let spawn = |a| DeviceHandle::spawn(model, a, events.clone(), vec![], paths.clone());

        let e = spawn(crashy("crash-open", true)).err().unwrap();
        assert!(format!("{e:#}").contains("crashed"), "{e:#}");

        let h = spawn(crashy("crash-tick", false)).unwrap();
        let key = h.snapshot().id.key();
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.snapshot().connected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!h.snapshot().connected);
        assert_eq!(crashes(&key).map(|(n, _)| n), Some(1));
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn onboard_writes_need_the_owners_opt_in() {
        let paths = temp_paths("gate");
        let root = paths.root.clone();
        let mut a = fake();
        let mut gate = Gate::load(paths.clone(), a.id.config_stem());
        let int = |v| SettingValue::Int(v);

        // The opt-in toggle is offered (with its warning) and starts off.
        let view = gate.view(&a);
        let toggle = view
            .controls
            .iter()
            .find(|c| c.key == SettingKey::OnboardWrites)
            .unwrap();
        assert!(toggle.warning.is_some());
        assert_eq!(
            view.settings.get("onboard_writes"),
            Some(&SettingValue::Bool(false))
        );

        // Ordinary writes pass; onboard writes are refused until allowed.
        set_checked(&mut a, &mut gate, SettingKey::Dpi, int(400)).unwrap();
        let refused = set_checked(&mut a, &mut gate, SettingKey::DpiStage1, int(400)).unwrap_err();
        assert_eq!(Refused::code(&refused), "invalid_value");
        let out_of_range = set_checked(&mut a, &mut gate, SettingKey::Dpi, int(5)).unwrap_err();
        assert_eq!(Refused::code(&out_of_range), "invalid_value");
        set_checked(
            &mut a,
            &mut gate,
            SettingKey::OnboardWrites,
            SettingValue::Bool(true),
        )
        .unwrap();
        set_checked(&mut a, &mut gate, SettingKey::DpiStage1, int(400)).unwrap();
        // Blocked stays refused even when allowed.
        assert!(set_checked(&mut a, &mut gate, SettingKey::DpiStage2, int(400)).is_err());
        assert_eq!(a.writes, [SettingKey::Dpi, SettingKey::DpiStage1]);
        a.writes.clear();

        // Save-to-device mode needs the opt-in; revoking it switches the mode off.
        set_checked(
            &mut a,
            &mut gate,
            SettingKey::SaveToDevice,
            SettingValue::Bool(true),
        )
        .unwrap();
        assert!(a.saving);

        // The opt-in survives a restart (config.toml), and turns off again.
        assert!(Gate::load(paths.clone(), a.id.config_stem()).allowed);
        set_checked(
            &mut a,
            &mut gate,
            SettingKey::OnboardWrites,
            SettingValue::Bool(false),
        )
        .unwrap();
        assert!(!a.saving, "revoking the opt-in leaves save mode");
        assert!(!Gate::load(paths, a.id.config_stem()).allowed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_set_whose_caller_gave_up_is_not_written() {
        let paths = temp_paths("stale-set");
        let a = fake();
        let stem = a.id.config_stem();
        let gate = Gate::load(paths.clone(), stem.clone());
        let shared = Arc::new(Mutex::new(gate.view(&a)));
        let (tx, rx) = mpsc::channel();
        let (reply, gave_up) = oneshot::channel();
        drop(gave_up);
        // The opt-in is the one write visible from outside the worker.
        tx.send(Cmd::Set {
            key: SettingKey::OnboardWrites,
            value: SettingValue::Bool(true),
            reply,
        })
        .unwrap();
        let (ack, done) = mpsc::channel();
        tx.send(Cmd::Shutdown(ack)).unwrap();
        run(Box::new(a), gate, rx, shared, broadcast::channel(4).0);
        done.recv().unwrap();
        assert!(!Gate::load(paths.clone(), stem).allowed);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    /// Loses its device on the first write.
    struct Unplugged(Fake, bool);

    impl DeviceAdapter for Unplugged {
        fn id(&self) -> &DeviceId {
            self.0.id()
        }
        fn snapshot(&self) -> DeviceSnapshot {
            DeviceSnapshot {
                connected: self.1,
                ..self.0.snapshot()
            }
        }
        fn set(&mut self, _: SettingKey, _: SettingValue) -> Result<()> {
            self.1 = false;
            bail!("unplugged")
        }
        fn tick(&mut self) -> Result<()> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    #[test]
    fn a_failed_set_that_changes_the_device_is_announced() {
        let paths = temp_paths("failed-set");
        let a = Unplugged(fake(), true);
        let gate = Gate::load(paths.clone(), a.id().config_stem());
        let shared = Arc::new(Mutex::new(gate.view(&a)));
        let (tx, rx) = mpsc::channel();
        let (reply, answer) = oneshot::channel();
        let (key, value) = (SettingKey::Dpi, SettingValue::Int(400));
        tx.send(Cmd::Set { key, value, reply }).unwrap();
        tx.send(Cmd::Shutdown(mpsc::channel().0)).unwrap();
        let (events, mut sub) = broadcast::channel(16);
        run(Box::new(a), gate, rx, shared, events);
        assert!(answer.blocking_recv().unwrap().is_err());
        let mut updated = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            if let AdapterEvent::Updated(s) = ev {
                updated.push(s.connected);
            }
        }
        assert_eq!(updated, [false]);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    /// Fails its ticks with `errors` in order, then shuts its worker down.
    struct Flaky {
        fake: Fake,
        errors: std::collections::VecDeque<&'static str>,
        worker: mpsc::Sender<Cmd>,
    }

    impl DeviceAdapter for Flaky {
        fn id(&self) -> &DeviceId {
            self.fake.id()
        }
        fn snapshot(&self) -> DeviceSnapshot {
            self.fake.snapshot()
        }
        fn set(&mut self, key: SettingKey, value: SettingValue) -> Result<()> {
            self.fake.set(key, value)
        }
        fn tick(&mut self) -> Result<()> {
            match self.errors.pop_front() {
                Some(e) => bail!("{e}"),
                None => {
                    let _ = self.worker.send(Cmd::Shutdown(mpsc::channel().0));
                    Ok(())
                }
            }
        }
        fn shutdown(&mut self) {}
        fn tick_interval(&self) -> Duration {
            Duration::ZERO
        }
    }

    #[test]
    fn a_tick_error_is_pushed_once_until_it_changes() {
        let paths = temp_paths("tick-errors");
        let (tx, rx) = mpsc::channel();
        let a = Flaky {
            fake: fake(),
            errors: ["timeout", "timeout", "unplugged"].into(),
            worker: tx,
        };
        let gate = Gate::load(paths.clone(), a.id().config_stem());
        let shared = Arc::new(Mutex::new(gate.view(&a)));
        let (events, mut sub) = broadcast::channel(16);
        run(Box::new(a), gate, rx, shared, events);
        let mut errors = Vec::new();
        while let Ok(ev) = sub.try_recv() {
            if let AdapterEvent::Error { device, message } = ev {
                assert!(device.is_some());
                errors.push(message);
            }
        }
        assert_eq!(errors, ["Fake: timeout", "Fake: unplugged"]);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn a_batch_is_checked_whole_then_written_once() {
        use SettingKey::{Brightness, Dpi, DpiStage1, OnboardWrites};
        let paths = temp_paths("batch");
        let mut a = fake();
        let batches = a.batches.clone();
        let mut gate = Gate::load(paths.clone(), a.id.config_stem());
        let int = SettingValue::Int;
        // One value the worker refuses stops the batch before any write.
        for bad in [
            vec![(Dpi, int(400)), (DpiStage1, int(400))],
            vec![(Dpi, int(400)), (Dpi, int(500))],
            vec![(Dpi, int(400)), (OnboardWrites, SettingValue::Bool(true))],
            vec![(Dpi, int(400)), (Brightness, int(50))],
            vec![(Dpi, int(5))],
        ] {
            let e = set_many_checked(&mut a, &mut gate, &bad).unwrap_err();
            assert_eq!(Refused::code(&e), "invalid_value", "{bad:?}");
        }
        assert!(batches.lock().unwrap().is_empty() && a.writes.is_empty());
        assert!(!gate.allowed);
        set_many_checked(&mut a, &mut gate, &[(Dpi, int(400))]).unwrap();
        assert_eq!(*batches.lock().unwrap(), [vec![Dpi]]);

        // Nor is a batch written whose caller gave up.
        let shared = Arc::new(Mutex::new(gate.view(&a)));
        let (tx, rx) = mpsc::channel();
        let (reply, gave_up) = oneshot::channel();
        drop(gave_up);
        let values = vec![(Dpi, int(500))];
        tx.send(Cmd::SetMany { values, reply }).unwrap();
        let (ack, done) = mpsc::channel();
        tx.send(Cmd::Shutdown(ack)).unwrap();
        run(Box::new(a), gate, rx, shared, broadcast::channel(4).0);
        done.recv().unwrap();
        assert_eq!(batches.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn invalid_saved_slider_values_get_the_nearest_valid_one() {
        let pct = SettingMeta::slider(SettingKey::Brightness, "B", 0.0, 100.0, 1.0);
        assert_eq!(
            pct.nearest_valid(&SettingValue::Int(300)),
            Some(SettingValue::Int(100))
        );
        assert_eq!(
            pct.nearest_valid(&SettingValue::Float(40.4)),
            Some(SettingValue::Int(40))
        );
        assert_eq!(pct.nearest_valid(&SettingValue::Int(50)), None);
        let mix = SettingMeta::slider(SettingKey::Volume, "V", 0.0, 1.0, 0.01);
        assert_eq!(
            mix.nearest_valid(&SettingValue::Float(1.5)),
            Some(SettingValue::Float(1.0))
        );
        let toggle = SettingMeta::toggle(SettingKey::GameMode, "G");
        assert_eq!(toggle.nearest_valid(&SettingValue::Int(3)), None);
        let empty = SettingMeta::slider(SettingKey::Volume, "V", 10.0, 0.0, 1.0);
        assert_eq!(empty.nearest_valid(&SettingValue::Int(50)), None);
    }
}
