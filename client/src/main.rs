//! hako — thin UI client for hakod.

mod update;

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use hako_core::catalog::{SettingKey, SettingValue};
use hako_core::default_socket_path;
use hako_core::device::{DeviceSnapshot, HubState};
use hako_core::ipc::{
    AppSettings, EventKind, IpcEvent, IpcRequest, IpcResponse, Method, SetParams,
};
use hako_core::remap::{
    CancelCaptureParams, ClearRemapParams, DeviceRemaps, SetRemapParams, StartCaptureParams,
};
use hako_core::{read_json_frame, write_json_frame};
use hako_ui::widgets::{Tone, UpdateOffer};
use hako_ui::{HakoAppUi, UiCommand};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UnixStream;
use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc;

/// How often an open UI looks for a new release.
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Parser, Debug)]
#[command(name = "hako", version, about = "Hako control panel")]
struct Cli {
    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long)]
    socket: Option<std::path::PathBuf>,
}

struct App {
    ui: HakoAppUi,
    cmd_tx: mpsc::UnboundedSender<ClientCmd>,
    state: Arc<Mutex<ClientState>>,
    was_online: bool,
    /// The connection dropped after being up; the next connect is announced.
    lost: bool,
    rt: Handle,
    /// `(owner/repo, installed binary)` when this build can update itself.
    updater: Option<(&'static str, PathBuf)>,
}

#[derive(Default)]
struct ClientState {
    hub: Option<HubState>,
    settings: Option<AppSettings>,
    error: Option<String>,
    /// Command outcomes waiting to be shown as toasts.
    notices: Vec<(Tone, String)>,
    /// Newer release on offer.
    update: Option<update::Release>,
    /// Installing `update`.
    updating: bool,
}

/// What to tell the user when a command finishes.
struct Feedback {
    /// Success message; `None` = the change speaks for itself.
    done: Option<String>,
    /// Prefix for the daemon's error message.
    failed: String,
}

impl Feedback {
    fn quiet(failed: impl Into<String>) -> Self {
        Self {
            done: None,
            failed: failed.into(),
        }
    }

    fn loud(done: impl Into<String>, failed: impl Into<String>) -> Self {
        Self {
            done: Some(done.into()),
            failed: failed.into(),
        }
    }
}

enum ClientCmd {
    Call(Method, serde_json::Value, Feedback),
    OpenPath(String),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(cli.log_level)
        .init();

    // Set by the previous process when it restarted us after an update;
    // cleared before any thread starts so children don't inherit it.
    let mut notices = Vec::new();
    if std::env::var_os(update::UPDATED_ENV).is_some() {
        notices.push((
            Tone::Ok,
            format!("Updated to v{}", env!("CARGO_PKG_VERSION")),
        ));
        if let Ok(w) = std::env::var(update::WARN_ENV) {
            notices.push((Tone::Error, w));
        }
    }
    std::env::remove_var(update::UPDATED_ENV);
    std::env::remove_var(update::WARN_ENV);

    let socket = cli.socket.unwrap_or_else(default_socket_path);
    let rt = Runtime::new()?;
    let state = Arc::new(Mutex::new(ClientState {
        error: Some(format!("Connecting to {}…", socket.display())),
        notices,
        ..Default::default()
    }));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientCmd>();

    let updater = update::target();
    if let Some((repo, _)) = updater {
        rt.spawn(update_checks(repo, state.clone()));
    }

    let state_bg = state.clone();
    rt.spawn(async move {
        loop {
            match UnixStream::connect(&socket).await {
                Ok(mut stream) => {
                    refresh(&mut stream, &state_bg).await;
                    if let Some(resp) = call(&mut stream, Method::GetConfig, serde_json::Value::Null).await {
                        if let Some(cfg) = resp.result.and_then(|r| serde_json::from_value(r).ok()) {
                            state_bg.lock().unwrap().settings = Some(cfg);
                        }
                    }
                    let mut events = tokio::spawn(subscribe(socket.clone(), state_bg.clone()));

                    // Commands on this connection; a slow poll keeps health,
                    // audio and profile info fresh between events.
                    loop {
                        tokio::select! {
                            cmd = cmd_rx.recv() => {
                                let Some(cmd) = cmd else { return };
                                let (method, params, feedback) = match cmd {
                                    ClientCmd::OpenPath(path) => {
                                        let _ = std::process::Command::new("xdg-open").arg(&path).spawn();
                                        continue;
                                    }
                                    ClientCmd::Call(m, p, f) => (m, p, f),
                                };
                                let Some(resp) = call(&mut stream, method, params).await else { break };
                                {
                                    let mut s = state_bg.lock().unwrap();
                                    if resp.ok {
                                        if let Some(done) = feedback.done {
                                            s.notices.push((Tone::Ok, done));
                                        }
                                        if method == Method::SetConfig {
                                            if let Some(cfg) = resp.result.and_then(|r| serde_json::from_value(r).ok()) {
                                                s.settings = Some(cfg);
                                            }
                                        }
                                    } else {
                                        let why = resp.error.map_or_else(|| "no reason given".into(), |e| e.message);
                                        s.notices.push((Tone::Error, format!("{}: {why}", feedback.failed)));
                                    }
                                }
                                if !refresh(&mut stream, &state_bg).await {
                                    break;
                                }
                            }
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                                if !refresh(&mut stream, &state_bg).await {
                                    break;
                                }
                                if events.is_finished() {
                                    events = tokio::spawn(subscribe(socket.clone(), state_bg.clone()));
                                }
                            }
                        }
                    }
                    events.abort();
                    state_bg.lock().unwrap().error =
                        Some("Disconnected from hakod. Reconnecting…".into());
                }
                Err(e) => {
                    state_bg.lock().unwrap().error =
                        Some(format!("Cannot connect to hakod ({e})."));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });

    let app = App {
        ui: HakoAppUi::default(),
        cmd_tx,
        state,
        was_online: false,
        lost: false,
        rt: rt.handle().clone(),
        updater,
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 780.0])
        .with_min_inner_size([900.0, 600.0])
        .with_title("Hako")
        // app_id / WM_CLASS: lets the compositor match hako.desktop for its icon.
        .with_app_id("hako");

    // Embedded window icon for taskbars that read the window's own icon.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }

    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native("Hako", native, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (notices, online) = {
            let mut st = self.state.lock().unwrap();
            self.ui.state = st.hub.clone();
            self.ui.daemon_error = st.error.clone();
            // Adopt server settings until the user starts editing, so a late
            // GetConfig doesn't clobber an in-progress edit.
            if self.ui.settings.is_none() || st.error.is_some() {
                self.ui.settings = st.settings.clone();
            }
            self.ui.update = st.update.as_ref().map(|r| UpdateOffer {
                version: r.version.clone(),
                busy: st.updating,
            });
            (
                std::mem::take(&mut st.notices),
                st.error.is_none() && st.hub.is_some(),
            )
        };
        for (tone, text) in notices {
            self.ui.notify(tone, text);
        }
        // Connection changes are announced once; the top bar pill keeps the state.
        if self.was_online && !online {
            self.ui
                .notify(Tone::Warn, "Lost connection to hakod. Reconnecting…");
            self.lost = true;
        } else if online && self.lost {
            self.ui.notify(Tone::Ok, "Reconnected to hakod");
            self.lost = false;
        }
        self.was_online = online;
        let mut cmds = Vec::new();
        self.ui.ui(ctx, &mut cmds);
        for c in cmds {
            let to = |v: Result<serde_json::Value, serde_json::Error>| v.unwrap_or_default();
            let cmd = match c {
                UiCommand::Set {
                    device_id,
                    key,
                    value,
                } => {
                    let feedback = set_feedback(self.ui.state.as_ref(), &device_id, key, &value);
                    ClientCmd::Call(
                        Method::Set,
                        to(serde_json::to_value(SetParams {
                            device_id,
                            key,
                            value,
                        })),
                        feedback,
                    )
                }
                UiCommand::ApplyProfile(name) => ClientCmd::Call(
                    Method::ApplyProfile,
                    serde_json::json!({ "name": name }),
                    Feedback::loud(
                        format!("Applied profile “{name}”"),
                        format!("Couldn't apply “{name}”"),
                    ),
                ),
                UiCommand::SaveProfile(name) => ClientCmd::Call(
                    Method::SaveProfile,
                    serde_json::json!({ "name": name }),
                    Feedback::loud(
                        format!("Saved profile “{name}”"),
                        format!("Couldn't save “{name}”"),
                    ),
                ),
                UiCommand::ResetAudio => ClientCmd::Call(
                    Method::ResetAudio,
                    serde_json::Value::Null,
                    Feedback::loud("Audio graph reset", "Couldn't reset audio"),
                ),
                UiCommand::Refresh => ClientCmd::Call(
                    Method::GetState,
                    serde_json::Value::Null,
                    Feedback::loud("Device state reloaded", "Couldn't reload devices"),
                ),
                UiCommand::SetConfig(cfg) => ClientCmd::Call(
                    Method::SetConfig,
                    to(serde_json::to_value(cfg)),
                    Feedback::quiet("Couldn't save settings"),
                ),
                UiCommand::OpenPath(p) => ClientCmd::OpenPath(p),
                UiCommand::StartCapture {
                    device_id,
                    role,
                    source,
                } => ClientCmd::Call(
                    Method::StartCapture,
                    to(serde_json::to_value(StartCaptureParams {
                        device_id,
                        role,
                        source,
                    })),
                    Feedback::quiet("Couldn't start listening"),
                ),
                UiCommand::CancelCapture { device_id, keep } => ClientCmd::Call(
                    Method::CancelCapture,
                    to(serde_json::to_value(CancelCaptureParams {
                        device_id,
                        keep,
                    })),
                    Feedback::quiet("Couldn't stop listening"),
                ),
                UiCommand::SetRemap { device_id, binding } => ClientCmd::Call(
                    Method::SetRemap,
                    to(serde_json::to_value(SetRemapParams { device_id, binding })),
                    Feedback::quiet("Couldn't save the assignment"),
                ),
                UiCommand::ClearRemap { device_id, source } => ClientCmd::Call(
                    Method::ClearRemap,
                    to(serde_json::to_value(ClearRemapParams { device_id, source })),
                    Feedback::quiet("Couldn't reset the key"),
                ),
                UiCommand::CheckUpdates => {
                    self.check_updates();
                    continue;
                }
                UiCommand::InstallUpdate => {
                    self.install_update();
                    continue;
                }
                UiCommand::DismissUpdate => {
                    self.state.lock().unwrap().update = None;
                    continue;
                }
            };
            let _ = self.cmd_tx.send(cmd);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl App {
    fn check_updates(&self) {
        let Some((repo, _)) = self.updater else {
            self.state.lock().unwrap().notices.push((
                Tone::Warn,
                "Updates are only available for installs from a GitHub release (.deb or tarball)"
                    .into(),
            ));
            return;
        };
        let state = self.state.clone();
        self.rt.spawn_blocking(move || {
            let res = update::check(repo);
            let mut s = state.lock().unwrap();
            match res {
                Ok(Some(rel)) => s.update = Some(rel),
                Ok(None) => s.notices.push((
                    Tone::Ok,
                    format!("v{} is the latest version", env!("CARGO_PKG_VERSION")),
                )),
                Err(e) => s
                    .notices
                    .push((Tone::Error, format!("Couldn't check for updates: {e:#}"))),
            }
        });
    }

    fn install_update(&self) {
        let Some((_, bin)) = self.updater.clone() else {
            return;
        };
        let rel = {
            let mut s = self.state.lock().unwrap();
            let Some(rel) = s.update.clone().filter(|_| !s.updating) else {
                return;
            };
            s.updating = true;
            rel
        };
        let state = self.state.clone();
        self.rt.spawn_blocking(move || {
            let msg = match update::install(&rel) {
                Ok(warn) => format!(
                    "Updated, but couldn't restart ({}). Reopen Hako.",
                    update::restart(&bin, warn)
                ),
                Err(e) => format!("Update failed: {e:#}"),
            };
            let mut s = state.lock().unwrap();
            s.updating = false;
            s.notices.push((Tone::Error, msg));
        });
    }
}

/// Background release checks while the UI is open, if the user allows them.
/// "Later" only hides the offer, so the next check brings it back.
async fn update_checks(repo: &'static str, state: Arc<Mutex<ClientState>>) {
    let mut last: Option<Instant> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Settings come from the daemon; until it answers, don't assume consent.
        let allowed = state
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .map(|s| s.check_updates);
        if allowed != Some(true) || last.is_some_and(|t| t.elapsed() < UPDATE_EVERY) {
            continue;
        }
        last = Some(Instant::now());
        if let Ok(Ok(Some(rel))) = tokio::task::spawn_blocking(move || update::check(repo)).await {
            let mut s = state.lock().unwrap();
            if !s.updating {
                s.update = Some(rel);
            }
        }
    }
}

/// Writes to onboard memory are announced; ordinary changes speak for themselves.
fn set_feedback(
    hub: Option<&HubState>,
    id: &hako_core::DeviceId,
    key: SettingKey,
    value: &SettingValue,
) -> Feedback {
    let meta = hub
        .and_then(|h| h.devices.iter().find(|d| &d.id == id))
        .and_then(|d| d.controls.iter().find(|m| m.key == key));
    let label = meta.map_or_else(|| key.as_str().to_string(), |m| m.label.clone());
    let failed = format!("Couldn't change {label}");
    match meta {
        _ if key == SettingKey::OnboardWrites => Feedback::loud(
            if value.as_bool() == Some(true) {
                "Saving to device memory allowed"
            } else {
                "Saving to device memory turned off"
            },
            failed,
        ),
        Some(m) if m.persistent => {
            Feedback::loud(format!("{label} saved to device memory"), failed)
        }
        _ => Feedback::quiet(failed),
    }
}

/// One request/response round trip. `None` means the connection is gone.
async fn call(
    stream: &mut UnixStream,
    method: Method,
    params: serde_json::Value,
) -> Option<IpcResponse> {
    let req = IpcRequest {
        id: serde_json::to_value(method).ok()?,
        method,
        params,
    };
    write_json_frame(stream, &req).await.ok()?;
    let v = read_json_frame(stream).await.ok()?;
    serde_json::from_value(v).ok()
}

/// Pull the full hub state. Returns false when the connection dropped.
async fn refresh(stream: &mut UnixStream, state: &Arc<Mutex<ClientState>>) -> bool {
    let Some(resp) = call(stream, Method::GetState, serde_json::Value::Null).await else {
        return false;
    };
    if let Some(hub) = resp
        .result
        .and_then(|r| serde_json::from_value::<HubState>(r).ok())
    {
        let mut s = state.lock().unwrap();
        s.hub = Some(hub);
        s.error = None;
    }
    true
}

/// Event stream on its own connection; snapshots are applied in place.
async fn subscribe(socket: std::path::PathBuf, state: Arc<Mutex<ClientState>>) {
    let Ok(mut stream) = UnixStream::connect(&socket).await else {
        return;
    };
    if call(
        &mut stream,
        Method::SubscribeEvents,
        serde_json::Value::Null,
    )
    .await
    .is_none()
    {
        return;
    }
    while let Ok(v) = read_json_frame(&mut stream).await {
        let Ok(ev) = serde_json::from_value::<IpcEvent>(v) else {
            continue;
        };
        let mut s = state.lock().unwrap();
        if ev.kind == EventKind::DaemonShuttingDown {
            s.error = Some("Daemon stopped".into());
            break;
        }
        let Some(hub) = s.hub.as_mut() else { continue };
        match ev.kind {
            EventKind::DeviceUpdated | EventKind::DeviceConnected => {
                if let Ok(snap) = serde_json::from_value::<DeviceSnapshot>(ev.payload) {
                    match hub.devices.iter_mut().find(|d| d.id == snap.id) {
                        Some(slot) => *slot = snap,
                        None => hub.devices.push(snap),
                    }
                }
            }
            EventKind::DeviceDisconnected => {
                if let Ok(id) = serde_json::from_value::<hako_core::DeviceId>(ev.payload) {
                    hub.devices.retain(|d| d.id != id);
                }
            }
            EventKind::RemapsUpdated => {
                if let Ok(r) = serde_json::from_value::<DeviceRemaps>(ev.payload) {
                    match hub.remaps.iter_mut().find(|x| x.device_id == r.device_id) {
                        Some(slot) => *slot = r,
                        None => hub.remaps.push(r),
                    }
                }
            }
            EventKind::RemapCaptured | EventKind::Error | EventKind::DaemonShuttingDown => {}
        }
    }
}
