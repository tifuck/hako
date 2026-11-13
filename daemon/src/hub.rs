//! Daemon hub: adapters, discovery, IPC dispatch.

use crate::drivers::{self, DeviceAdapter, DriverCtx, EventTx, Model, PlaceholderAdapter};
use crate::hid::HidDeviceInfo;
use crate::mixer::{MixerService, PatchError};
use crate::remap::RemapEngine;
use crate::worker::{DeviceHandle, Refused};
use anyhow::{Context, Result};
use hako_audio::meters::MeterService;
use hako_audio::AudioService;
use hako_core::catalog::{SettingKey, SettingMeta, SettingValue};
use hako_core::config::{
    delete_profile, list_profiles, load_app_config, load_profile, save_profile,
    settings_from_profile_device, update_app_config, validate_profile_name, AppConfig, ConfigPaths,
    ProfileFile, TomlSetting,
};
use hako_core::device::{AudioSummary, DaemonHealth, DeviceId, DeviceKind, HubState};
use hako_core::ipc::{
    audio_latency_samples, AppSettings, IpcRequest, IpcResponse, Method, ProfileNameParams,
    SetManyParams, SetParams, SnapshotParams, AUDIO_LATENCIES,
};
use hako_core::mixer::{MixerFile, PatchMixerParams, Scene};
use hako_core::protocol::{is_new_error, AdapterEvent};
use hako_core::remap::{CancelCaptureParams, ClearRemapParams, SetRemapParams, StartCaptureParams};
use hako_core::VERSION;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

/// Log levels `config.toml` may name.
pub const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
/// Discovery period; also the first retry delay for a model that failed to open.
pub const DISCOVERY_PERIOD: Duration = Duration::from_secs(3);
/// Longest wait between attempts to open a model that keeps failing.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Every hidraw node.
type Present = Vec<HidDeviceInfo>;
type Opened = Vec<(&'static Model, Box<dyn DeviceAdapter>)>;
/// Every model tried: (yielded a device, open error).
type Attempted = Vec<(&'static Model, bool, Option<String>)>;
/// (consecutive failures, next attempt, open error last pushed).
type Retry = (u32, Instant, Option<String>);

pub struct DaemonHub {
    pub paths: ConfigPaths,
    pub audio: Arc<AudioService>,
    /// Level meters; runs only while a `SubscribeMeters` connection is open.
    pub meters: MeterService,
    /// `mixer.toml` and its PipeWire graph (own thread).
    pub mixer: Arc<MixerService>,
    /// Never held across an await or device I/O (workers own the devices).
    devices: std::sync::Mutex<HashMap<String, DeviceHandle>>,
    events: EventTx,
    started: Instant,
    last_profile_applied: Mutex<bool>,
    remap: RemapEngine,
    /// Models whose last open failed, keyed by (vid, first pid).
    retry: std::sync::Mutex<HashMap<(u16, u16), Retry>>,
    /// Last rescan of each connected `multi` model (receivers).
    rescanned: std::sync::Mutex<HashMap<(u16, u16), Instant>>,
}

impl DaemonHub {
    pub fn new(paths: ConfigPaths) -> Self {
        let (events, _) = broadcast::channel(64);
        let remap = RemapEngine::start(paths.clone(), events.clone());
        let audio = AudioService::new();
        let audio_events = events.clone();
        audio.on_error(move |message| {
            let _ = audio_events.send(AdapterEvent::Error {
                device: None,
                message,
            });
        });
        let cfg = load_app_config(&paths);
        // ChatMix is not up yet, so these only record the preferences.
        let _ =
            audio.set_chatmix_default(cfg.as_ref().map_or(true, |c| c.chatmix_set_default_sink));
        let latency = latency_samples(cfg.as_ref().ok());
        let _ = audio.set_latency(latency);
        // Loopback / DSP / mixer processes of a crashed daemon (they would
        // clash by name).
        audio.kill_orphans();
        // Line-in links older versions made outlive them (pw-link links linger).
        if let Ok(cfg) = &cfg {
            if let Err(e) = audio.remove_orphan_aux_links(&cfg.aux) {
                warn!("orphan AUX links: {e:#}");
            }
        }
        let audio = Arc::new(audio);
        let mixer = MixerService::start(paths.clone(), audio.clone(), events.clone(), latency);
        audio.on_dial(mixer.dial_hook());
        Self {
            paths,
            audio,
            meters: MeterService::new(),
            mixer: Arc::new(mixer),
            devices: std::sync::Mutex::new(HashMap::new()),
            events,
            started: Instant::now(),
            last_profile_applied: Mutex::new(false),
            remap,
            retry: std::sync::Mutex::new(HashMap::new()),
            rescanned: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AdapterEvent> {
        self.events.subscribe()
    }

    pub async fn discover_once(&self) {
        let Ok(Ok(present)) = tokio::task::spawn_blocking(present_nodes).await else {
            warn!("hidraw enumeration failed; skipping discovery");
            return;
        };
        // Unplugged or replugged: none of the nodes the adapter was opened on
        // is left (a replug between two passes brings new ones). Shut it down
        // (its teardown must run before a replugged device is
        // reopened below) so the device is reopened fresh.
        let gone: Vec<DeviceHandle> = {
            let mut map = lock(&self.devices);
            let mut gone = Vec::new();
            map.retain(|_, h| {
                let alive = model_nodes(h.model, &present)
                    .iter()
                    .any(|sys| h.nodes.contains(sys));
                if !alive {
                    gone.push(h.clone());
                }
                alive
            });
            gone
        };
        stop_all(&gone).await;
        for h in gone {
            let id = h.snapshot().id;
            info!("{} unplugged", id.key());
            let _ = self.events.send(AdapterEvent::Disconnected(id));
        }

        // Skipped: models with a live adapter (receivers only until their
        // rescan is due), and failing ones still backing off.
        let now = Instant::now();
        let connected: Vec<(&'static Model, DeviceId)> = self
            .handles()
            .iter()
            .map(|h| (h.model, h.snapshot()))
            .filter(|(_, s)| s.connected)
            .map(|(m, s)| (m, s.id))
            .collect();
        let mut skip: Vec<&'static Model> = Vec::new();
        {
            let mut rescanned = lock(&self.rescanned);
            for (m, _) in &connected {
                let due = m.multi
                    && rescanned
                        .get(&retry_key(m))
                        .is_none_or(|&at| now >= at + drivers::RESCAN);
                if due {
                    rescanned.insert(retry_key(m), now);
                } else if !skip.iter().any(|s| s.same(m)) {
                    skip.push(m);
                }
            }
            rescanned.retain(|k, _| connected.iter().any(|(m, _)| retry_key(m) == *k));
            let retry = lock(&self.retry);
            skip.extend(
                all_models(&present)
                    .filter(|m| retry.get(&retry_key(m)).is_some_and(|&(_, at, _)| now < at)),
            );
        }
        let ctx = DriverCtx {
            paths: self.paths.clone(),
            audio: self.audio.clone(),
            open: Vec::new(),
            vendor_id: 0,
        };

        let snapshot = present.clone();
        let (discovered, attempted) = tokio::task::spawn_blocking(move || {
            discover_blocking(ctx, &skip, &connected, &snapshot)
        })
        .await
        .unwrap_or_else(|e| {
            warn!("discover join: {e}");
            Default::default()
        });

        {
            let mut retry = lock(&self.retry);
            for (model, ok, error) in attempted {
                if ok {
                    retry.remove(&retry_key(model));
                    continue;
                }
                let (n, at, reported) = retry.entry(retry_key(model)).or_insert((0, now, None));
                *n += 1;
                *at = now + backoff(*n);
                if is_new_error(reported, error.as_deref()) {
                    self.push_error(format!("{}: {}", model.name, error.unwrap_or_default()));
                }
            }
        }

        // Built before taking the map lock: spawning runs driver code (and
        // may write the device), which must not happen under it.
        let handles: Vec<DeviceHandle> = discovered
            .into_iter()
            .map(|(model, a)| {
                let snap = a.snapshot();
                if matches!(snap.kind, DeviceKind::Mouse | DeviceKind::Keyboard) && snap.connected {
                    self.remap.attach(
                        snap.id.clone(),
                        snap.kind,
                        model.remap_pids(&snap.id),
                        &snap.name,
                    );
                }
                let nodes = model_nodes(model, &present);
                DeviceHandle::spawn(model, a, self.events.clone(), nodes, self.paths.clone())
            })
            .collect();
        let mut map = lock(&self.devices);
        // Reopened models replace their stale placeholders / dead adapters
        // (once per model: a receiver may return several devices).
        for h in &handles {
            map.retain(|_, old| !old.model.same(h.model) || old.snapshot().connected);
        }
        for handle in handles {
            let snap = handle.snapshot();
            map.insert(snap.id.key(), handle);
            let _ = self.events.send(AdapterEvent::Connected(snap));
        }
    }

    /// A background failure no request is waiting for.
    fn push_error(&self, message: String) {
        let _ = self.events.send(AdapterEvent::Error {
            device: None,
            message,
        });
    }

    fn handles(&self) -> Vec<DeviceHandle> {
        lock(&self.devices).values().cloned().collect()
    }

    /// The connection that started a capture went away: stop listening.
    pub fn end_capture(&self, device_id: DeviceId) {
        if let Ok(snap) = self.remap.cancel_capture(device_id, false) {
            let _ = self.events.send(AdapterEvent::RemapsUpdated(snap));
        }
    }

    fn app_settings(&self, cfg: &AppConfig) -> AppSettings {
        AppSettings {
            log_level: if cfg.log_level.is_empty() {
                "info".into()
            } else {
                cfg.log_level.clone()
            },
            chatmix_set_default_sink: cfg.chatmix_set_default_sink,
            aux_set_default_sink: cfg.aux.set_default_sink,
            aux_auto_enable: cfg.aux.auto_enable,
            aux_effects: cfg.aux.effects.clone(),
            config_dir: self.paths.root.display().to_string(),
            theme: cfg.theme.clone(),
            check_updates: cfg.check_updates,
            audio_latency: cfg.audio_latency.clone(),
        }
    }

    /// Save first, then apply live (blocking: audio lock, child processes).
    /// The legacy line-in fields are stored as given (older UIs send them).
    fn set_config(&self, s: AppSettings) -> Result<AppSettings> {
        let (cfg, ()) = update_app_config(&self.paths, |cfg| {
            if !s.log_level.is_empty() {
                cfg.log_level = s.log_level.clone();
            }
            cfg.chatmix_set_default_sink = s.chatmix_set_default_sink;
            cfg.aux.set_default_sink = s.aux_set_default_sink;
            cfg.aux.auto_enable = s.aux_auto_enable;
            cfg.aux.effects = s.aux_effects.clone();
            cfg.theme = s.theme.clone();
            cfg.check_updates = s.check_updates;
            cfg.audio_latency = s.audio_latency.clone();
            Ok(())
        })?;
        self.audio
            .set_chatmix_default(cfg.chatmix_set_default_sink)
            .context("saved, but the default sink could not follow")?;
        self.mixer.set_latency(latency_samples(Some(&cfg)));
        self.audio
            .set_latency(latency_samples(Some(&cfg)))
            .context("saved, but audio could not apply the new latency")?;
        Ok(self.app_settings(&cfg))
    }

    pub async fn maybe_apply_last_profile(&self) {
        let mut applied = self.last_profile_applied.lock().await;
        if *applied {
            return;
        }
        let cfg = match load_app_config(&self.paths) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!("startup profile skipped: {e:#}");
                self.push_error(format!("Startup profile skipped: {e:#}"));
                AppConfig::default()
            }
        };
        if let Some(name) = cfg.last_profile {
            if let Err(error) = self.apply_profile(&name, true).await {
                warn!("apply last profile {name}: {error:#}");
                self.push_error(format!("Startup profile {name}: {error:#}"));
            }
        }
        *applied = true;
    }

    pub async fn hub_state(&self) -> HubState {
        let (cfg, config_error) = match load_app_config(&self.paths) {
            Ok(cfg) => (cfg, None),
            Err(e) => (AppConfig::default(), Some(format!("{e:#}"))),
        };
        let (profiles, profiles_error) = match list_profiles(&self.paths) {
            Ok(p) => (p, None),
            Err(e) => (Vec::new(), Some(format!("profiles: {e:#}"))),
        };
        let devices: Vec<_> = self.handles().iter().map(DeviceHandle::snapshot).collect();
        // Published status: never waits for audio work in progress.
        let audio = AudioSummary {
            chatmix_active: self.audio.chatmix_active(),
            last_error: self.audio.last_error(),
            ..AudioSummary::default()
        };
        HubState {
            health: DaemonHealth {
                version: VERSION.into(),
                uptime_secs: self.started.elapsed().as_secs(),
                adapter_count: devices.len(),
                last_error: config_error.or(profiles_error),
            },
            last_profile: cfg.last_profile,
            profiles,
            audio,
            remaps: self.remap.snapshots(),
            devices,
        }
    }

    pub async fn handle(&self, req: IpcRequest) -> IpcResponse {
        let id = req.id.clone();
        match req.method {
            Method::Ping => {
                IpcResponse::ok(id, serde_json::json!({"pong": true, "version": VERSION}))
            }
            Method::Status | Method::Health => {
                let st = self.hub_state().await;
                IpcResponse::ok(id, st.health)
            }
            Method::GetState => IpcResponse::ok(id, self.hub_state().await),
            Method::ListDevices => {
                let list: Vec<_> = self
                    .handles()
                    .iter()
                    .map(|h| {
                        let d = h.snapshot();
                        serde_json::json!({
                            "id": d.id,
                            "kind": d.kind,
                            "name": d.name,
                            "connected": d.connected,
                        })
                    })
                    .collect();
                IpcResponse::ok(id, list)
            }
            Method::GetSnapshot => {
                let want = if req.params.is_null() {
                    None
                } else {
                    match serde_json::from_value::<SnapshotParams>(req.params) {
                        Ok(p) => p.device_id,
                        Err(e) => return IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                    }
                };
                let handles = self.handles();
                let devices = handles.iter().map(DeviceHandle::snapshot);
                match want {
                    Some(want) => match devices.into_iter().find(|d| d.id == want) {
                        Some(d) => IpcResponse::ok(id, d),
                        None => IpcResponse::err(id, "not_found", "device not found"),
                    },
                    None => IpcResponse::ok(id, devices.collect::<Vec<_>>()),
                }
            }
            Method::Set => match serde_json::from_value::<SetParams>(req.params) {
                Ok(p) => match self.set(p.device_id, p.key, p.value).await {
                    Ok(snap) => {
                        let _ = self.events.send(AdapterEvent::Updated(snap.clone()));
                        IpcResponse::ok(id, snap)
                    }
                    Err(e) => IpcResponse::err(id, Refused::code(&e), format!("{e:#}")),
                },
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::SetMany => match serde_json::from_value::<SetManyParams>(req.params) {
                Ok(p) => match self.set_many(p.device_id, p.values).await {
                    Ok(snap) => {
                        let _ = self.events.send(AdapterEvent::Updated(snap.clone()));
                        IpcResponse::ok(id, snap)
                    }
                    Err(e) => IpcResponse::err(id, Refused::code(&e), format!("{e:#}")),
                },
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::ListProfiles => {
                match list_profiles(&self.paths)
                    .and_then(|names| Ok((names, load_app_config(&self.paths)?.last_profile)))
                {
                    Ok((names, last)) => IpcResponse::ok(
                        id,
                        serde_json::json!({"profiles": names, "last_profile": last}),
                    ),
                    Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                }
            }
            Method::ApplyProfile | Method::SaveProfile | Method::DeleteProfile
                if serde_json::from_value::<ProfileNameParams>(req.params.clone())
                    .is_ok_and(|p| validate_profile_name(&p.name).is_err()) =>
            {
                IpcResponse::err(id, "invalid_value", "invalid profile name")
            }
            Method::ApplyProfile => match serde_json::from_value::<ProfileNameParams>(req.params) {
                Ok(p) => match self.apply_profile(&p.name, false).await {
                    Ok(()) => IpcResponse::ok(id, serde_json::json!({"applied": p.name})),
                    Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                },
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::SaveProfile => match serde_json::from_value::<ProfileNameParams>(req.params) {
                Ok(p) => match self.save_profile(&p.name).await {
                    Ok(()) => IpcResponse::ok(id, serde_json::json!({"saved": p.name})),
                    Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                },
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::DeleteProfile => {
                match serde_json::from_value::<ProfileNameParams>(req.params) {
                    Ok(p) => match self.delete_profile(&p.name) {
                        Ok(()) => IpcResponse::ok(id, serde_json::json!({"deleted": p.name})),
                        Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                    },
                    Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                }
            }
            Method::ResetAudio => {
                // pactl / pw-* subprocesses: keep them off the async runtime.
                let (audio, mixer) = (self.audio.clone(), self.mixer.clone());
                let aux = load_app_config(&self.paths).map(|c| c.aux);
                let reset = tokio::task::spawn_blocking(move || {
                    // Its processes go first: the sweep below spares none.
                    mixer.reset();
                    audio.reset_all()?;
                    // Links a crashed daemon left are not in this one's state.
                    match aux {
                        Ok(aux) => audio.remove_orphan_aux_links(&aux),
                        Err(_) => Ok(()),
                    }
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("reset task: {e}")));
                match reset {
                    Ok(()) => IpcResponse::ok(id, serde_json::json!({"reset": true})),
                    Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                }
            }
            Method::GetConfig => match load_app_config(&self.paths) {
                Ok(cfg) => IpcResponse::ok(id, self.app_settings(&cfg)),
                Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
            },
            Method::SetConfig => match serde_json::from_value::<AppSettings>(req.params) {
                Ok(s) => {
                    if let Err(e) = validate_settings(&s) {
                        return IpcResponse::err(id, "invalid_value", e);
                    }
                    match tokio::task::block_in_place(|| self.set_config(s)) {
                        Ok(saved) => IpcResponse::ok(id, saved),
                        Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                    }
                }
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::GetRemaps => {
                match serde_json::from_value::<SnapshotParams>(req.params)
                    .ok()
                    .and_then(|p| p.device_id)
                {
                    Some(dev) => match self.remap.snapshot(&dev) {
                        Ok(snap) => IpcResponse::ok(id, snap),
                        Err(e) => IpcResponse::err(id, "not_found", format!("{e:#}")),
                    },
                    None => IpcResponse::ok(id, self.remap.snapshots()),
                }
            }
            Method::SetRemap => match serde_json::from_value::<SetRemapParams>(req.params) {
                Ok(p) => match self.remap.set_binding(p.device_id, p.binding) {
                    Ok(snap) => {
                        let _ = self.events.send(AdapterEvent::RemapsUpdated(snap.clone()));
                        IpcResponse::ok(id, snap)
                    }
                    Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                },
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::ClearRemap => match serde_json::from_value::<ClearRemapParams>(req.params) {
                Ok(p) => match self.remap.clear_binding(p.device_id, p.source) {
                    Ok(snap) => {
                        let _ = self.events.send(AdapterEvent::RemapsUpdated(snap.clone()));
                        IpcResponse::ok(id, snap)
                    }
                    Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
                },
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
            Method::StartCapture => {
                match serde_json::from_value::<StartCaptureParams>(req.params) {
                    Ok(p) => match self.remap.start_capture(p.device_id, p.role, p.source) {
                        Ok(snap) => {
                            let _ = self.events.send(AdapterEvent::RemapsUpdated(snap.clone()));
                            IpcResponse::ok(id, snap)
                        }
                        Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                    },
                    Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                }
            }
            Method::CancelCapture => {
                match serde_json::from_value::<CancelCaptureParams>(req.params) {
                    Ok(p) => match self.remap.cancel_capture(p.device_id, p.keep) {
                        Ok(snap) => {
                            let _ = self.events.send(AdapterEvent::RemapsUpdated(snap.clone()));
                            IpcResponse::ok(id, snap)
                        }
                        Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                    },
                    Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
                }
            }
            // The connection loop turns its connection into an event stream
            // before requests reach the hub.
            Method::SubscribeEvents | Method::SubscribeMeters => IpcResponse::err(
                id,
                "invalid_value",
                format!("{:?} is handled by the connection", req.method),
            ),
            Method::GetMixer => match self.mixer.state() {
                Ok(state) => IpcResponse::ok(id, state),
                Err(e) => IpcResponse::err(id, "io", format!("{e:#}")),
            },
            Method::PatchMixer => match serde_json::from_value::<PatchMixerParams>(req.params) {
                Ok(p) => {
                    // Waits for the graph (child processes): off the runtime.
                    let mixer = self.mixer.clone();
                    match tokio::task::spawn_blocking(move || mixer.patch(&p.ops)).await {
                        Ok(Ok(state)) => IpcResponse::ok(id, state),
                        Ok(Err(PatchError::Invalid(e))) => IpcResponse::err(id, "invalid_value", e),
                        Ok(Err(PatchError::Io(e))) => IpcResponse::err(id, "io", format!("{e:#}")),
                        Err(e) => IpcResponse::err(id, "io", format!("mixer task: {e}")),
                    }
                }
                Err(e) => IpcResponse::err(id, "invalid_value", format!("{e:#}")),
            },
        }
    }

    async fn set(
        &self,
        device_id: DeviceId,
        key: SettingKey,
        value: SettingValue,
    ) -> Result<hako_core::DeviceSnapshot> {
        let handle = lock(&self.devices).get(&device_id.key()).cloned();
        handle
            .ok_or_else(|| Refused::new("not_found", "device offline"))?
            .set(key, value)
            .await
    }

    async fn set_many(
        &self,
        device_id: DeviceId,
        values: Vec<(SettingKey, SettingValue)>,
    ) -> Result<hako_core::DeviceSnapshot> {
        let handle = lock(&self.devices).get(&device_id.key()).cloned();
        handle
            .ok_or_else(|| Refused::new("not_found", "device offline"))?
            .set_many(values)
            .await
    }

    /// `startup`: the automatic re-apply at daemon start, which skips devices
    /// in save-to-device mode (their stored state is already on the device;
    /// re-applying would write onboard memory at every boot) and the mixer
    /// layout (`mixer.toml` has the live one).
    async fn apply_profile(&self, name: &str, startup: bool) -> Result<()> {
        let profile = load_profile(&self.paths, name)?;
        let mut failures = Vec::new();
        for h in self.handles() {
            let snap = h.snapshot();
            let saving = snap
                .settings
                .get(SettingKey::SaveToDevice.as_str())
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if startup && saving {
                info!(
                    "{}: in save-to-device mode, startup profile skipped",
                    h.model.name
                );
                continue;
            }
            // The device's own stem, else any PID of its model (receiver vs. wired path).
            let m = h.model;
            let own = snap.id.config_stem();
            let Some(dev_map) = profile.devices.get(&own).or_else(|| {
                m.product_ids.iter().find_map(|pid| {
                    profile
                        .devices
                        .get(&format!("{:04x}_{pid:04x}", m.vendor_id))
                })
            }) else {
                continue;
            };
            // Offline (placeholder / unplugged) devices are skipped like
            // absent ones: the rest of the profile still applies.
            if !snap.connected {
                info!("{}: not connected, profile skipped", m.name);
                continue;
            }
            let controls = snap.controls;
            for (k, v) in settings_from_profile_device(dev_map) {
                let result = match SettingKey::parse(&k) {
                    Some(key) if profile_skips(key, &controls) => continue,
                    Some(key) => h.set(key, v).await.map(|_| ()),
                    None => Err(anyhow::anyhow!("unknown setting")),
                };
                if let Err(error) = result {
                    failures.push(format!("{} {k}: {error:#}", m.name));
                }
            }
            let _ = self.events.send(AdapterEvent::Updated(h.snapshot()));
        }
        if let Some(layout) = profile.mixer.filter(|_| !startup) {
            // Waits for the graph (child processes): off the runtime.
            let mixer = self.mixer.clone();
            match tokio::task::spawn_blocking(move || mixer.set_live(layout)).await {
                Ok(Ok(_)) => {}
                Ok(Err(PatchError::Invalid(e))) => failures.push(format!("mixer layout: {e}")),
                Ok(Err(PatchError::Io(e))) => failures.push(format!("mixer layout: {e:#}")),
                Err(e) => failures.push(format!("mixer task: {e}")),
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("profile partially applied: {}", failures.join("; "));
        }
        set_last_profile(&self.paths, name)?;
        info!("applied profile {name}");
        Ok(())
    }

    /// Delete a profile and forget it as the one applied at start.
    fn delete_profile(&self, name: &str) -> Result<()> {
        anyhow::ensure!(delete_profile(&self.paths, name)?, "no profile {name:?}");
        update_app_config(&self.paths, |cfg| {
            if cfg.last_profile.as_deref() == Some(name) {
                cfg.last_profile = None;
            }
            Ok(())
        })
        .map(drop)
    }

    async fn save_profile(&self, name: &str) -> Result<()> {
        let mut devices = std::collections::BTreeMap::new();
        for h in self.handles() {
            let snap = h.snapshot();
            // Only what ApplyProfile can write back (no read-only or stale keys).
            let m: std::collections::BTreeMap<_, _> = snap
                .settings
                .iter()
                .filter(|(k, _)| {
                    snap.controls.iter().any(|c| {
                        c.key.as_str() == k.as_str()
                            && !c.read_only
                            && !c.persistent
                            && c.blocked.is_none()
                            && c.key != SettingKey::OnboardWrites
                    })
                })
                .map(|(k, v)| (k.clone(), TomlSetting::from(v)))
                .collect();
            if !m.is_empty() {
                devices.insert(snap.id.config_stem(), m);
            }
        }
        let profile = ProfileFile {
            version: 1,
            name: name.to_string(),
            devices,
            // The file in memory: a fader drag not yet written counts.
            mixer: self
                .mixer
                .state()
                .ok()
                .and_then(|state| profile_layout(&state.file)),
        };
        save_profile(&self.paths, &profile)?;
        set_last_profile(&self.paths, name)
    }

    pub async fn shutdown(&self) {
        let handles: Vec<_> = lock(&self.devices).drain().map(|(_, h)| h).collect();
        stop_all(&handles).await;
        let mixer = self.mixer.clone();
        let _ = tokio::task::spawn_blocking(move || mixer.shutdown()).await;
        if let Err(e) = self.audio.reset_all() {
            warn!("audio teardown: {e:#}");
        }
        self.remap.shutdown();
        let _ = self.events.send(AdapterEvent::ShuttingDown);
    }
}

/// Shut the workers down and wait for them.
async fn stop_all(handles: &[DeviceHandle]) {
    if handles.is_empty() {
        return;
    }
    let acks: Vec<_> = handles.iter().map(DeviceHandle::shutdown).collect();
    tokio::task::spawn_blocking(move || {
        // ponytail: shared 2 s budget; a wedged device is abandoned, not joined.
        let deadline = Instant::now() + Duration::from_secs(2);
        for ack in acks {
            let _ = ack.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    })
    .await
    .ok();
}

/// A panic elsewhere must not take every later request down with it: the
/// guarded maps stay consistent between statements.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn set_last_profile(paths: &ConfigPaths, name: &str) -> Result<()> {
    update_app_config(paths, |cfg| {
        cfg.last_profile = Some(name.to_string());
        Ok(())
    })
    .map(drop)
}

/// The mixer layout a profile stores: the live one while the mixer is on,
/// without solo (a momentary state).
fn profile_layout(file: &MixerFile) -> Option<Scene> {
    let mut live = file.enabled.then(|| file.live.clone())?;
    for input in &mut live.inputs {
        input.solo = false;
    }
    Some(live)
}

/// Settings checks that are the caller's fault (`invalid_value`).
fn validate_settings(s: &AppSettings) -> Result<(), String> {
    s.aux_effects.validate()?;
    if !s.log_level.is_empty() && !LOG_LEVELS.contains(&s.log_level.as_str()) {
        return Err(format!(
            "log level must be one of {}",
            LOG_LEVELS.join(", ")
        ));
    }
    if audio_latency_samples(&s.audio_latency).is_none() {
        let presets: Vec<_> = AUDIO_LATENCIES.iter().map(|(name, _)| *name).collect();
        return Err(format!(
            "audio latency must be one of {}",
            presets.join(", ")
        ));
    }
    Ok(())
}

/// The configured latency preset in samples (an unknown one: the default).
fn latency_samples(cfg: Option<&AppConfig>) -> u32 {
    let balanced = AUDIO_LATENCIES[1].1;
    cfg.and_then(|cfg| audio_latency_samples(&cfg.audio_latency))
        .unwrap_or(balanced)
}

/// Wait after the `n`th consecutive failed open (n >= 1): one discovery
/// period, doubling, capped.
fn backoff(n: u32) -> Duration {
    DISCOVERY_PERIOD
        .saturating_mul(1 << (n.max(1) - 1).min(5))
        .min(MAX_BACKOFF)
}

/// Profile keys never applied: onboard memory writes (profiles re-apply at
/// every start: flash wear) and the opt-in that allows them, plus the old
/// headset line-in keys older profiles hold (line-in is a mixer input now).
fn profile_skips(key: SettingKey, controls: &[SettingMeta]) -> bool {
    matches!(
        key,
        SettingKey::OnboardWrites | SettingKey::AuxEnabled | SettingKey::AuxCapturePercent
    ) || controls.iter().any(|c| c.key == key && c.persistent)
}

fn retry_key(m: &Model) -> (u16, u16) {
    (m.vendor_id, m.product_ids[0])
}

/// sysfs paths of `model`'s nodes in `present`.
fn model_nodes(model: &Model, present: &Present) -> Vec<PathBuf> {
    present
        .iter()
        .filter(|d| d.vendor_id == model.vendor_id && model.product_ids.contains(&d.product_id))
        .map(|d| d.sys.clone())
        .collect()
}

fn present_nodes() -> Result<Present> {
    crate::hid::enumerate_hidraw()
}

/// Every table model, then a runtime-probed model for each present VID:PID
/// no table lists.
fn all_models(present: &Present) -> impl Iterator<Item = &'static Model> + '_ {
    let mut unlisted: Vec<(u16, u16)> = present
        .iter()
        .map(|d| (d.vendor_id, d.product_id))
        .filter(|&(v, p)| {
            !drivers::models().any(|m| m.vendor_id == v && m.product_ids.contains(&p))
        })
        .collect();
    unlisted.sort_unstable();
    unlisted.dedup();
    drivers::models().chain(unlisted.into_iter().filter_map(move |(v, p)| {
        let nodes: Vec<&HidDeviceInfo> = present
            .iter()
            .filter(|d| d.vendor_id == v && d.product_id == p)
            .collect();
        drivers::fallback_model(&nodes)
    }))
}

/// Blocking discovery body (called from spawn_blocking): open every present
/// model not in `skip`; failures become placeholders. Also returns every
/// model tried, whether it yielded a device, and its open error. `connected`
/// lists the open devices a rescanned `multi` model must not return again.
fn discover_blocking(
    mut ctx: DriverCtx,
    skip: &[&'static Model],
    connected: &[(&'static Model, DeviceId)],
    present: &Present,
) -> (Opened, Attempted) {
    let mut out = Vec::new();
    let mut attempted = Vec::new();
    for model in all_models(present) {
        if skip.iter().any(|m| m.same(model)) {
            continue;
        }
        let Some(pid) = model.product_ids.iter().copied().find(|&pid| {
            present
                .iter()
                .any(|d| d.vendor_id == model.vendor_id && d.product_id == pid)
        }) else {
            continue;
        };
        ctx.vendor_id = model.vendor_id;
        ctx.open = connected
            .iter()
            .filter(|(m, _)| m.same(model))
            .map(|(_, id)| id.clone())
            .collect();
        let rescan = !ctx.open.is_empty();
        // A driver bug fails only its own model (with backoff), not the pass.
        let opened = catch_unwind(AssertUnwindSafe(|| (model.open)(pid, &ctx)))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("driver crashed while opening")));
        match opened {
            Ok(adapters) => {
                // A receiver with nothing reachable backs off like a failure;
                // a rescan that finds nothing new is fine.
                attempted.push((model, rescan || !adapters.is_empty(), None));
                out.extend(adapters.into_iter().map(|a| (model, a)));
            }
            Err(e) if rescan => warn!("{} rescan: {e:#}", model.name),
            Err(e) => {
                warn!("{}: {e:#}", model.name);
                let placeholder = PlaceholderAdapter::permission_denied(
                    DeviceId {
                        vendor_id: model.vendor_id,
                        product_id: pid,
                        instance: "pending".into(),
                    },
                    model.kind,
                    model.name,
                    &e,
                );
                attempted.push((model, false, placeholder.snapshot().last_error));
                out.push((model, Box::new(placeholder) as Box<dyn DeviceAdapter>));
            }
        }
    }
    (out, attempted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_validation_rejects_unknown_log_levels() {
        let mut s = AppSettings::default();
        assert!(validate_settings(&s).is_ok());
        s.log_level = "loud".into();
        assert!(validate_settings(&s).is_err());
        s.log_level = String::new();
        assert!(validate_settings(&s).is_ok());
    }

    #[test]
    fn first_retry_waits_one_discovery_period() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [3, 6, 12, 24, 48, 60, 60, 60]);
    }

    #[test]
    fn profiles_skip_onboard_writes_and_the_old_line_in() {
        let controls = [SettingMeta::toggle(SettingKey::SaveToDevice, "Save").persistent()];
        assert!(profile_skips(SettingKey::SaveToDevice, &controls));
        assert!(profile_skips(SettingKey::OnboardWrites, &controls));
        assert!(profile_skips(SettingKey::AuxEnabled, &controls));
        assert!(profile_skips(SettingKey::AuxCapturePercent, &controls));
        assert!(!profile_skips(SettingKey::Dpi, &controls));
    }

    /// Built by hand: `new` would touch the session's PipeWire graph.
    fn hub(name: &str) -> DaemonHub {
        let root = std::env::temp_dir().join(format!("hako-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        };
        paths.ensure().unwrap();
        let (events, _) = broadcast::channel(8);
        let audio = Arc::new(AudioService::new());
        DaemonHub {
            // Off (no mixer.toml): never builds anything.
            mixer: Arc::new(MixerService::start(
                paths.clone(),
                audio.clone(),
                events.clone(),
                256,
            )),
            audio,
            meters: MeterService::new(),
            devices: Default::default(),
            remap: RemapEngine::start(paths.clone(), events.clone()),
            events,
            started: Instant::now(),
            last_profile_applied: Mutex::new(false),
            retry: Default::default(),
            rescanned: Default::default(),
            paths,
        }
    }

    fn close(hub: DaemonHub) {
        hub.remap.shutdown();
        hub.mixer.shutdown();
        let _ = std::fs::remove_dir_all(&hub.paths.root);
    }

    /// A layout of one virtual output (never built: the mixer stays off).
    fn layout(id: &str) -> Scene {
        Scene {
            inputs: Vec::new(),
            outputs: vec![hako_core::mixer::Output {
                id: id.into(),
                label: "Test".into(),
                kind: hako_core::mixer::OutputKind::Virtual {
                    node_name: "hako-test-never-built".into(),
                },
                chain: Default::default(),
                gain_db: 0.0,
                mute: false,
                mono: false,
            }],
        }
    }

    #[test]
    fn a_saved_profile_holds_the_layout_of_a_mixer_that_is_on_without_solo() {
        let mut file = MixerFile {
            live: layout("B1"),
            ..MixerFile::default()
        };
        file.live.inputs.push(hako_core::mixer::Input {
            id: "game".into(),
            label: "Game".into(),
            kind: hako_core::mixer::InputKind::Virtual {
                node_name: "hako-test-never-built-in".into(),
            },
            chain: Default::default(),
            gain_db: -3.0,
            mute: false,
            solo: true,
            routes: vec!["B1".into()],
            chatmix: None,
        });
        assert_eq!(profile_layout(&file), None, "mixer off");
        file.enabled = true;
        let mut saved = file.live.clone();
        saved.inputs[0].solo = false;
        assert_eq!(profile_layout(&file), Some(saved));
    }

    #[tokio::test]
    async fn profiles_save_and_apply_the_mixer_layout() {
        let hub = hub("profile-mixer");
        let paths = hub.paths.clone();
        let live = || hub.mixer.state().unwrap().file;
        hub.mixer.set_live(layout("B1")).unwrap();

        // Mixer off: the profile has no layout, and applying it changes none.
        hub.save_profile("plain").await.unwrap();
        assert_eq!(load_profile(&paths, "plain").unwrap().mixer, None);

        let mut profile = ProfileFile {
            version: 1,
            name: "mix".into(),
            devices: Default::default(),
            mixer: Some(layout("B2")),
        };
        save_profile(&paths, &profile).unwrap();
        // The daemon's own apply at start leaves the mixer alone.
        hub.apply_profile("mix", true).await.unwrap();
        assert_eq!(live().live, layout("B1"));

        let mut events = hub.subscribe();
        hub.apply_profile("mix", false).await.unwrap();
        assert_eq!(live().live, layout("B2"));
        assert!(!live().enabled, "a profile never turns the mixer on");
        assert_eq!(
            hako_core::mixer::load_mixer(&paths).unwrap().live,
            layout("B2")
        );
        let mut updated = false;
        while let Ok(ev) = events.try_recv() {
            updated |= matches!(ev, AdapterEvent::MixerUpdated(s) if s.file.live == layout("B2"));
        }
        assert!(updated, "no MixerUpdated");

        hub.apply_profile("plain", false).await.unwrap();
        assert_eq!(live().live, layout("B2"), "no layout in the profile");

        // A hand-edited layout fails the apply and changes nothing.
        profile.name = "bad".into();
        profile.mixer = Some(layout("X9"));
        save_profile(&paths, &profile).unwrap();
        let err = hub.apply_profile("bad", false).await.unwrap_err();
        assert!(format!("{err:#}").contains("mixer layout"), "{err:#}");
        assert_eq!(live().live, layout("B2"));
        assert_eq!(
            load_app_config(&paths).unwrap().last_profile.as_deref(),
            Some("plain")
        );
        close(hub);
    }

    #[tokio::test]
    async fn deleting_a_profile_forgets_it_as_the_startup_one() {
        let hub = hub("profile-delete");
        hub.save_profile("p").await.unwrap();
        hub.save_profile("q").await.unwrap();
        let last = || load_app_config(&hub.paths).unwrap().last_profile;
        hub.delete_profile("p").unwrap();
        assert_eq!(list_profiles(&hub.paths).unwrap(), ["q"]);
        assert_eq!(last().as_deref(), Some("q"), "another profile's stays");
        hub.delete_profile("q").unwrap();
        assert_eq!(last(), None);
        assert!(hub.delete_profile("q").is_err(), "no such profile");
        close(hub);
    }

    #[tokio::test]
    async fn offline_devices_do_not_fail_a_profile() {
        let hub = hub("offline");
        let paths = hub.paths.clone();
        let model = drivers::models().next().unwrap();
        let id = DeviceId {
            vendor_id: model.vendor_id,
            product_id: model.product_ids[0],
            instance: "pending".into(),
        };
        let profile = ProfileFile {
            version: 1,
            name: "p".into(),
            devices: [(
                id.config_stem(),
                [("dpi".to_string(), TomlSetting::Int(800))].into(),
            )]
            .into(),
            mixer: None,
        };
        save_profile(&paths, &profile).unwrap();
        let placeholder = PlaceholderAdapter::permission_denied(
            id.clone(),
            model.kind,
            model.name,
            &anyhow::anyhow!("gone"),
        );
        let h = DeviceHandle::spawn(
            model,
            Box::new(placeholder),
            hub.events.clone(),
            Vec::new(),
            paths.clone(),
        );
        lock(&hub.devices).insert(id.key(), h);

        hub.apply_profile("p", false).await.unwrap();
        assert_eq!(
            load_app_config(&paths).unwrap().last_profile.as_deref(),
            Some("p")
        );
        close(hub);
    }
}
