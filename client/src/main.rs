//! hako — thin UI client for hakod.

mod update;

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use hako_core::catalog::{ControlKind, SettingKey, SettingValue};
use hako_core::default_socket_path;
use hako_core::device::{DeviceSnapshot, HubState};
use hako_core::ipc::{
    AppSettings, EventKind, IpcEvent, IpcRequest, IpcResponse, MeterLevels, MeterTargetSpec,
    Method, ProfileNameParams, SetManyParams, SetParams, SubscribeMetersParams,
};
use hako_core::mixer::{MixerState, PatchMixerParams};
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

/// Longest one daemon request may take (a profile touches every device).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How often an open UI looks for a new release.
const UPDATE_EVERY: Duration = Duration::from_secs(6 * 3600);

/// A meter stream closes once no frame has asked for it this long (the
/// window is hidden and no longer drawn).
const METER_STALE: Duration = Duration::from_secs(1);

#[derive(Parser, Debug)]
#[command(name = "hako", version, about = "hako control panel")]
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
    /// `ClientState::mixer_rev` the UI has seen.
    mixer_rev: u64,
    rt: Handle,
    /// `(owner/repo, installed binary)` when this build can update itself.
    updater: Option<(&'static str, PathBuf)>,
    meters: Arc<Mutex<Meters>>,
}

/// Level meters, shared with the meter task.
#[derive(Default)]
struct Meters {
    /// What the page shows, as of `asked`.
    wanted: Vec<MeterTargetSpec>,
    asked: Option<Instant>,
    /// Targets the stream is open for, and their latest frame.
    streaming: Vec<MeterTargetSpec>,
    levels: Vec<[f32; 2]>,
}

impl Meters {
    fn wanted(&self) -> &[MeterTargetSpec] {
        match self.asked {
            Some(t) if t.elapsed() < METER_STALE => &self.wanted,
            _ => &[],
        }
    }
}

#[derive(Default)]
struct ClientState {
    hub: Option<HubState>,
    settings: Option<AppSettings>,
    error: Option<String>,
    /// Command outcomes waiting to be shown as toasts.
    notices: Vec<(Tone, String)>,
    /// Daemon calls sent and not yet answered (with the state after them).
    pending: usize,
    /// Newer release, downloaded and waiting for a restart.
    update: Option<update::Ready>,
    /// Looking for or downloading a release.
    checking: bool,
    /// Installing `update`.
    updating: bool,
    /// Latest mixer from `GetMixer` / `PatchMixer` / `MixerUpdated`, or why
    /// `GetMixer` failed; `mixer_rev` counts changes.
    mixer: Option<Result<MixerState, String>>,
    mixer_rev: u64,
    /// `PatchMixer` calls sent and not yet answered.
    mixer_pending: usize,
}

impl ClientState {
    fn set_mixer(&mut self, m: Result<MixerState, String>) {
        self.mixer = Some(m);
        self.mixer_rev += 1;
    }
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

    let meters = Arc::new(Mutex::new(Meters::default()));
    let meter_socket = socket.clone();
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
                    fetch_mixer(&mut stream, &state_bg).await;
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
                                let mixer_call = matches!(method, Method::GetMixer | Method::PatchMixer);
                                let alive = match call(&mut stream, method, params).await {
                                    Some(resp) if mixer_call => {
                                        // A refused patch: show why, then reload what the daemon has.
                                        let refused = method == Method::PatchMixer && !resp.ok;
                                        if refused {
                                            let why = resp.error.map_or_else(|| "no reason given".into(), |e| e.message);
                                            state_bg.lock().unwrap().notices.push((Tone::Error, format!("{}: {why}", feedback.failed)));
                                            fetch_mixer(&mut stream, &state_bg).await
                                        } else {
                                            state_bg.lock().unwrap().set_mixer(mixer_result(resp));
                                            true
                                        }
                                    }
                                    Some(resp) => {
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
                                        // A profile carries a mixer layout: show it even if
                                        // the `MixerUpdated` event is missed.
                                        refresh(&mut stream, &state_bg).await
                                            && (method != Method::ApplyProfile
                                                || fetch_mixer(&mut stream, &state_bg).await)
                                    }
                                    None => false,
                                };
                                {
                                    let mut s = state_bg.lock().unwrap();
                                    s.pending -= 1;
                                    if method == Method::PatchMixer {
                                        s.mixer_pending -= 1;
                                    }
                                }
                                if !alive {
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
        mixer_rev: 0,
        rt: rt.handle().clone(),
        updater,
        meters: meters.clone(),
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 780.0])
        .with_min_inner_size([900.0, 600.0])
        .with_title("hako")
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

    let handle = rt.handle().clone();
    eframe::run_native(
        "hako",
        native,
        Box::new(move |cc| {
            handle.spawn(stream_meters(meter_socket, meters, cc.egui_ctx.clone()));
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (notices, online) = {
            let mut st = self.state.lock().unwrap();
            self.ui.state = st.hub.clone();
            self.ui.daemon_error = st.error.clone();
            self.ui.pending = st.pending > 0;
            self.ui.mixer.busy = st.mixer_pending > 0;
            if st.mixer_rev != self.mixer_rev {
                self.mixer_rev = st.mixer_rev;
                match &st.mixer {
                    Some(Ok(m)) => self.ui.mixer.receive(m.clone()),
                    Some(Err(e)) => self.ui.mixer.unavailable(e.clone()),
                    None => {}
                }
            }
            // Adopt server settings until the user starts editing, so a late
            // GetConfig doesn't clobber an in-progress edit.
            if self.ui.settings.is_none() || st.error.is_some() {
                self.ui.settings = st.settings.clone();
            }
            {
                // Levels only for the meters the page asked for last frame.
                let m = self.meters.lock().unwrap();
                self.ui.meter_levels = if m.streaming == self.ui.meter_targets {
                    m.levels.clone()
                } else {
                    Vec::new()
                };
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
        {
            let mut m = self.meters.lock().unwrap();
            m.wanted.clone_from(&self.ui.meter_targets);
            m.asked = Some(Instant::now());
        }
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
                UiCommand::SetMany { device_id, values } => {
                    let feedback = values.first().map_or_else(
                        || Feedback::quiet("Couldn't change settings"),
                        |(key, value)| {
                            set_feedback(self.ui.state.as_ref(), &device_id, *key, value)
                        },
                    );
                    ClientCmd::Call(
                        Method::SetMany,
                        to(serde_json::to_value(SetManyParams { device_id, values })),
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
                UiCommand::DeleteProfile(name) => ClientCmd::Call(
                    Method::DeleteProfile,
                    to(serde_json::to_value(ProfileNameParams {
                        name: name.clone(),
                    })),
                    Feedback::loud(
                        format!("Deleted profile “{name}”"),
                        format!("Couldn't delete “{name}”"),
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
                UiCommand::GetMixer => ClientCmd::Call(
                    Method::GetMixer,
                    serde_json::Value::Null,
                    Feedback::quiet("Couldn't load the mixer"),
                ),
                UiCommand::PatchMixer(ops) => ClientCmd::Call(
                    Method::PatchMixer,
                    to(serde_json::to_value(PatchMixerParams { ops })),
                    Feedback::quiet("Couldn't change the mixer"),
                ),
            };
            if let ClientCmd::Call(method, ..) = &cmd {
                let mut s = self.state.lock().unwrap();
                s.pending += 1;
                if *method == Method::PatchMixer {
                    s.mixer_pending += 1;
                }
            }
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
            let res = fetch_update(repo, &state, true);
            let mut s = state.lock().unwrap();
            match res {
                Ok(true) => {}
                Ok(false) => s.notices.push((
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
            let Some(rel) = s.update.clone().filter(|_| !s.updating && !s.checking) else {
                return;
            };
            s.updating = true;
            rel
        };
        let state = self.state.clone();
        self.rt.spawn_blocking(move || {
            let msg = match update::install(&rel) {
                Ok(warn) => format!(
                    "Updated, but couldn't restart ({}). Reopen hako.",
                    update::restart(&bin, warn)
                ),
                Err(e) => format!("Update failed: {e:#}"),
            };
            let mut s = state.lock().unwrap();
            s.updating = false;
            if !rel.on_disk() {
                s.update = None;
            }
            s.notices.push((Tone::Error, msg));
        });
    }
}

/// Look for a newer release and download it, so the offer only has to
/// restart. `Ok(false)` = this build is the latest. `loud` announces the
/// download (the user clicked "Check now").
fn fetch_update(repo: &str, state: &Mutex<ClientState>, loud: bool) -> anyhow::Result<bool> {
    {
        let mut s = state.lock().unwrap();
        if s.checking || s.updating {
            anyhow::bail!("an update is already in progress");
        }
        s.checking = true;
    }
    let res = (|| {
        let Some(rel) = update::check(repo)? else {
            return Ok(false);
        };
        let have = state
            .lock()
            .unwrap()
            .update
            .as_ref()
            .map(|r| r.version.clone());
        if have.as_deref() != Some(rel.version.as_str()) {
            {
                // The download replaces any older one on disk.
                let mut s = state.lock().unwrap();
                s.update = None;
                if loud {
                    s.notices
                        .push((Tone::Ok, format!("Downloading hako v{}…", rel.version)));
                }
            }
            let ready = update::download(&rel)?;
            state.lock().unwrap().update = Some(ready);
        }
        Ok(true)
    })();
    state.lock().unwrap().checking = false;
    res
}

/// Background release checks while the UI is open, if the user allows them;
/// a newer release is downloaded, then offered. "Later" only hides the
/// offer, so the next check brings it back.
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
        let state = state.clone();
        let _ = tokio::task::spawn_blocking(move || fetch_update(repo, &state, false)).await;
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
    let label = match meta {
        // One picker sets all three channels.
        Some(m) if m.kind == ControlKind::ColorChannel => "color".into(),
        Some(m) => m.label.clone(),
        None => key.as_str().to_string(),
    };
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

/// One request/response round trip. `None` means the connection is gone (or
/// the daemon stopped answering: the caller reconnects, so a late reply can
/// never be read as the answer to a later request).
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
    let round_trip = async {
        write_json_frame(stream, &req).await.ok()?;
        read_json_frame(stream).await.ok()
    };
    let v = tokio::time::timeout(CALL_TIMEOUT, round_trip)
        .await
        .ok()??;
    serde_json::from_value(v).ok()
}

/// Pull the full hub state. Returns false when the connection dropped.
async fn refresh(stream: &mut UnixStream, state: &Arc<Mutex<ClientState>>) -> bool {
    let Some(resp) = call(stream, Method::GetState, serde_json::Value::Null).await else {
        return false;
    };
    let hub = match (resp.result, resp.error) {
        (Some(r), _) => serde_json::from_value::<HubState>(r)
            .map_err(|e| format!("hakod sent a state this UI cannot read ({e}); update both")),
        (None, e) => Err(format!(
            "hakod could not report its state: {}",
            e.map_or_else(|| "no reason given".into(), |e| e.message)
        )),
    };
    let mut s = state.lock().unwrap();
    match hub {
        Ok(hub) => {
            s.hub = Some(hub);
            s.error = None;
        }
        Err(e) => s.error = Some(e),
    }
    true
}

/// A `GetMixer` / `PatchMixer` answer: the mixer, or why there is none
/// (an older hakod answers `unsupported`).
fn mixer_result(resp: IpcResponse) -> Result<MixerState, String> {
    match (resp.result, resp.error) {
        (Some(r), _) if resp.ok => serde_json::from_value(r)
            .map_err(|e| format!("hakod sent a mixer this UI cannot read ({e}); update both")),
        (_, Some(e)) if e.code == "unsupported" => {
            Err("This hakod has no mixer yet. Update hako to use it.".into())
        }
        (_, e) => Err(e.map_or_else(|| "no reason given".into(), |e| e.message)),
    }
}

/// Reload the mixer. Returns false when the connection dropped.
async fn fetch_mixer(stream: &mut UnixStream, state: &Arc<Mutex<ClientState>>) -> bool {
    let Some(resp) = call(stream, Method::GetMixer, serde_json::Value::Null).await else {
        return false;
    };
    state.lock().unwrap().set_mixer(mixer_result(resp));
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
        // ponytail: events and replies travel on two connections, so an event
        // for an older edit can land after a newer reply; the UI shows it
        // until the next event. Sequence numbers if that flicker matters.
        if ev.kind == EventKind::MixerUpdated {
            if let Ok(m) = serde_json::from_value::<MixerState>(ev.payload) {
                s.set_mixer(Ok(m));
            }
            continue;
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
            EventKind::Error => {
                if let Some(msg) = ev.payload.get("message").and_then(|m| m.as_str()) {
                    s.notices.push((Tone::Error, msg.to_string()));
                }
            }
            EventKind::RemapCaptured
            | EventKind::DaemonShuttingDown
            | EventKind::Meters
            | EventKind::MixerUpdated => {}
        }
    }
}

/// Meter frames on their own connection, open only while the page shows
/// meters and the window is drawn. Each frame repaints the UI.
async fn stream_meters(socket: PathBuf, meters: Arc<Mutex<Meters>>, ctx: egui::Context) {
    loop {
        let wanted = meters.lock().unwrap().wanted().to_vec();
        if wanted.is_empty() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        let params = serde_json::to_value(SubscribeMetersParams {
            targets: wanted.clone(),
        })
        .unwrap_or_default();
        let mut stream = match UnixStream::connect(&socket).await {
            Ok(stream) => stream,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let subscribed = call(&mut stream, Method::SubscribeMeters, params).await;
        if !subscribed.is_some_and(|r| r.ok) {
            // A daemon without meters: the page stays silent.
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        // Frames come at ~30 Hz; a silent daemon is a dead one.
        while let Ok(Ok(v)) =
            tokio::time::timeout(Duration::from_secs(2), read_json_frame(&mut stream)).await
        {
            let Some(frame) = serde_json::from_value::<IpcEvent>(v)
                .ok()
                .filter(|ev| ev.kind == EventKind::Meters)
                .and_then(|ev| serde_json::from_value::<MeterLevels>(ev.payload).ok())
            else {
                continue;
            };
            {
                let mut m = meters.lock().unwrap();
                if m.wanted() != wanted.as_slice() {
                    break;
                }
                m.streaming.clone_from(&wanted);
                m.levels = frame.levels;
            }
            ctx.request_repaint();
        }
        let mut m = meters.lock().unwrap();
        m.streaming.clear();
        m.levels.clear();
    }
}
