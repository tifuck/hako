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
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue};
use hako_core::config::{load_app_config, update_app_config, ConfigPaths};
use hako_core::device::DeviceSnapshot;
use hako_core::protocol::AdapterEvent;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
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
        let gate = Gate::load(paths, adapter.id().config_stem());
        let mut adapter = adapter;
        gate.enforce(adapter.as_mut());
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
        rx.await.map_err(|_| anyhow!("device worker stopped"))?
    }

    /// Ask the worker to release the device; the receiver fires when done.
    pub fn shutdown(&self) -> mpsc::Receiver<()> {
        let (ack, done) = mpsc::channel();
        let _ = self.tx.send(Cmd::Shutdown(ack));
        done
    }
}

/// The device's onboard-memory opt-in.
struct Gate {
    paths: ConfigPaths,
    /// `vid_pid` the opt-in is stored under.
    stem: String,
    allowed: bool,
}

impl Gate {
    fn load(paths: ConfigPaths, stem: String) -> Self {
        let allowed = load_app_config(&paths).is_ok_and(|c| c.onboard_writes.contains(&stem));
        Self {
            paths,
            stem,
            allowed,
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
                warn!("{}: could not leave save-to-device mode: {e}", self.stem);
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
                if let Err(e) = adapter.tick() {
                    warn!("tick {}: {e}", adapter.id().key());
                }
                if let Some(snap) = publish(gate.view(adapter.as_ref())) {
                    let _ = events.send(AdapterEvent::Updated(snap));
                }
            }
            // Handle dropped (device replaced after a reconnect): just let go of
            // the device; daemon-level teardown belongs to Shutdown only.
            Err(RecvTimeoutError::Disconnected) => return,
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
    let meta = snap
        .controls
        .iter()
        .find(|c| c.key == key)
        .ok_or_else(|| anyhow!("{} has no {} control", snap.name, key.as_str()))?;
    meta.validate(&value).map_err(anyhow::Error::msg)?;
    if key == SettingKey::OnboardWrites {
        gate.set(value.as_bool().unwrap_or(false))?;
        gate.enforce(a);
        return Ok(());
    }
    if meta.persistent && !gate.allowed {
        bail!(
            "{} is saved to the {}'s onboard memory: turn on \"Allow saving to device memory\" \
             (Device tab) first",
            meta.label,
            snap.name
        );
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
        assert!(set_checked(&mut a, &mut gate, SettingKey::DpiStage1, int(400)).is_err());
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
}
