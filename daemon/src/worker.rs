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
use anyhow::{anyhow, bail, Result};
use hako_core::catalog::{ControlKind, SettingKey, SettingMeta, SettingValue};
use hako_core::config::{load_app_config, load_device_settings, update_app_config, ConfigPaths};
use hako_core::device::DeviceSnapshot;
use hako_core::protocol::AdapterEvent;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Instant;
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

enum Cmd {
    Set {
        key: SettingKey,
        value: SettingValue,
        reply: oneshot::Sender<Result<DeviceSnapshot>>,
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
    pub fn spawn(
        model: &'static Model,
        adapter: Box<dyn DeviceAdapter>,
        events: EventTx,
        nodes: Vec<PathBuf>,
        paths: ConfigPaths,
    ) -> Self {
        let mut gate = Gate::load(paths, adapter.id().config_stem());
        let mut adapter = adapter;
        gate.enforce(adapter.as_mut());
        repair_saved(adapter.as_mut(), &mut gate);
        let snapshot = Arc::new(Mutex::new(gate.view(adapter.as_ref())));
        let (tx, rx) = mpsc::channel();
        let shared = snapshot.clone();
        std::thread::Builder::new()
            .name("hako-dev".into())
            .spawn(move || {
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
                }
            })
            .expect("spawn device thread");
        Self {
            model,
            nodes,
            snapshot,
            tx,
        }
    }

    /// Last published state; never waits on the device.
    pub fn snapshot(&self) -> DeviceSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub async fn set(&self, key: SettingKey, value: SettingValue) -> Result<DeviceSnapshot> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Set { key, value, reply })
            .map_err(|_| anyhow!("device worker stopped"))?;
        // A wedged device must not hang the IPC connection that asked.
        match tokio::time::timeout(SET_TIMEOUT, rx).await {
            Ok(reply) => reply.map_err(|_| anyhow!("device worker stopped"))?,
            Err(_) => Err(Refused::new("busy", "the device did not answer in time").into()),
        }
    }

    /// Ask the worker to release the device; the receiver fires when done.
    pub fn shutdown(&self) -> mpsc::Receiver<()> {
        let (ack, done) = mpsc::channel();
        let _ = self.tx.send(Cmd::Shutdown(ack));
        done
    }
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
            self.enforce(a);
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

    /// Without the opt-in, a driver's `save_to_device` mode must be off.
    fn enforce(&self, a: &mut dyn DeviceAdapter) {
        let saving = a
            .snapshot()
            .settings
            .get(SettingKey::SaveToDevice.as_str())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if saving && !self.allowed {
            if let Err(e) = a.set(SettingKey::SaveToDevice, SettingValue::Bool(false)) {
                warn!("{}: could not leave save-to-device mode: {e:#}", self.stem);
            }
        }
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
    let mut next_tick = Instant::now() + adapter.tick_interval();
    loop {
        match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(Cmd::Set { key, value, reply }) => {
                let result = set_checked(adapter.as_mut(), &mut gate, key, value);
                publish(gate.view(adapter.as_ref()));
                let _ = reply.send(result.map(|()| gate.view(adapter.as_ref())));
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
                if let Err(e) = adapter.tick() {
                    warn!("tick {}: {e:#}", adapter.id().key());
                }
                if let Some(snap) = publish(gate.view(adapter.as_ref())) {
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
        let Some(fixed) = nearest_valid(meta, value) else {
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

/// The in-range whole/fractional number closest to `value` for an invalid
/// slider value; `None` when it is valid or not a slider.
fn nearest_valid(meta: &SettingMeta, value: &SettingValue) -> Option<SettingValue> {
    if meta.kind != ControlKind::Slider && meta.kind != ControlKind::ColorChannel
        || meta.validate(value).is_ok()
    {
        return None;
    }
    let n = value.as_float().filter(|f| f.is_finite())?;
    let n = n.clamp(meta.min.unwrap_or(f64::MIN), meta.max.unwrap_or(f64::MAX));
    let fixed = if [meta.min, meta.max, meta.step]
        .iter()
        .all(|v| v.is_none_or(|v| v.fract() == 0.0))
    {
        SettingValue::Int(n.round() as i64)
    } else {
        SettingValue::Float(n)
    };
    meta.validate(&fixed).is_ok().then_some(fixed)
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
    gate.refresh(a);
    let snap = gate.view(a);
    if !snap.connected {
        bail!(
            "{} is not connected{}",
            snap.name,
            snap.last_error
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        );
    }
    let meta = snap.controls.iter().find(|c| c.key == key).ok_or_else(|| {
        Refused::new(
            "invalid_value",
            format!("{} has no {} control", snap.name, key.as_str()),
        )
    })?;
    meta.validate(&value)
        .map_err(|e| Refused::new("invalid_value", e))?;
    if key == SettingKey::OnboardWrites {
        gate.set(value.as_bool().unwrap_or(false))?;
        gate.enforce(a);
        return Ok(());
    }
    if meta.persistent && !gate.allowed {
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
    a.set(key, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::device::{DeviceId, DeviceKind};

    /// A device with one ordinary and one onboard-memory control.
    struct Fake {
        id: DeviceId,
        writes: Vec<SettingKey>,
        saving: bool,
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
                self.saving = value.as_bool().unwrap();
            }
            self.writes.push(key);
            Ok(())
        }
        fn tick(&mut self) -> Result<()> {
            Ok(())
        }
        fn shutdown(&mut self) {}
    }

    #[test]
    fn onboard_writes_need_the_owners_opt_in() {
        let root = std::env::temp_dir().join(format!("hako-gate-{}", std::process::id()));
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root: root.clone(),
        };
        paths.ensure().unwrap();
        let mut a = Fake {
            id: DeviceId {
                vendor_id: 0x1234,
                product_id: 0x5678,
                instance: "x".into(),
            },
            writes: vec![],
            saving: false,
        };
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
    fn invalid_saved_slider_values_get_the_nearest_valid_one() {
        let pct = SettingMeta::slider(SettingKey::Brightness, "B", 0.0, 100.0, 1.0);
        assert_eq!(
            nearest_valid(&pct, &SettingValue::Int(300)),
            Some(SettingValue::Int(100))
        );
        assert_eq!(
            nearest_valid(&pct, &SettingValue::Float(40.4)),
            Some(SettingValue::Int(40))
        );
        assert_eq!(nearest_valid(&pct, &SettingValue::Int(50)), None);
        let mix = SettingMeta::slider(SettingKey::Volume, "V", 0.0, 1.0, 0.01);
        assert_eq!(
            nearest_valid(&mix, &SettingValue::Float(1.5)),
            Some(SettingValue::Float(1.0))
        );
        let toggle = SettingMeta::toggle(SettingKey::GameMode, "G");
        assert_eq!(nearest_valid(&toggle, &SettingValue::Int(3)), None);
    }
}
