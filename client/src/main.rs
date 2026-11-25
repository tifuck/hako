//! hako — thin UI client for hakod.

mod update;

use anyhow::Result;
use clap::Parser;
use eframe::egui;
use hako_core::catalog::{ControlKind, SettingKey, SettingValue};
use hako_core::default_socket_path;
use hako_core::device::{DeviceSnapshot, HubState};
use hako_core::hotkey::{HotkeyCaptureState, HotkeysState, PatchHotkeysParams};
use hako_core::ipc::{
    AppSettings, EventKind, IpcEvent, IpcRequest, IpcResponse, MeterLevels, MeterTargetSpec,
    Method, ProfileNameParams, SetManyParams, SetParams, SubscribeMetersParams,
};
use hako_core::mixer::{MixerState, PatchMixerParams};
use hako_core::net::{NetState, PatchNetParams};
use hako_core::remap::{
    CancelCaptureParams, ClearRemapParams, DeviceRemaps, SetRemapParams, StartCaptureParams,
};
use hako_core::voice::{
    PatchVoiceParams, VoiceAction, VoiceControlParams, VoicePartial, VoiceState,
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

/// First retry after a failed background update check; it doubles up to
/// `UPDATE_EVERY`.
const UPDATE_RETRY: Duration = Duration::from_secs(10 * 60);

/// Wait before reconnecting after a lost connection; it doubles up to
/// `RECONNECT_MAX` until a connection answers again.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(16);

/// Told when a change is not sent because hakod is unavailable.
const OFFLINE_NOTICE: &str = "hakod is unavailable; the change was not applied";

/// A meter stream closes once no frame has asked for it this long (the
/// window is hidden and no longer drawn).
const METER_STALE: Duration = Duration::from_secs(1);

/// Toasts waiting for the next frame (a hidden window draws none).
const MAX_NOTICES: usize = 16;

/// How long a closed window waits for update checks and other blocking work.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

#[derive(Parser, Debug)]
#[command(name = "hako", version, about = "hako control panel")]
struct Cli {
    #[arg(long, default_value = "info", global = true)]
    log_level: String,

    #[arg(long, global = true)]
    socket: Option<std::path::PathBuf>,

    /// Without a command the control panel opens.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug, PartialEq)]
enum Command {
    /// Control dictation (for scripts and desktop shortcuts).
    Voice {
        #[arg(value_enum)]
        action: VoiceVerb,
        /// Use the alternate profile.
        #[arg(long)]
        alt: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq)]
enum VoiceVerb {
    Start,
    Stop,
    Toggle,
    Cancel,
}

impl From<VoiceVerb> for VoiceAction {
    fn from(v: VoiceVerb) -> Self {
        match v {
            VoiceVerb::Start => Self::Start,
            VoiceVerb::Stop => Self::Stop,
            VoiceVerb::Toggle => Self::Toggle,
            VoiceVerb::Cancel => Self::Cancel,
        }
    }
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
    /// `ClientState::net_rev` the UI has seen.
    net_rev: u64,
    /// `ClientState::hotkeys_rev` / `voice_rev` / `capture_rev` /
    /// `partial_rev` the UI has seen.
    hotkeys_rev: u64,
    voice_rev: u64,
    capture_rev: u64,
    partial_rev: u64,
    /// `ClientState::settings_rev` the UI has adopted.
    settings_rev: u64,
    rt: Handle,
    /// `(owner/repo, installed binary)` when this build can update itself.
    updater: Option<(&'static str, PathBuf, update::Stamp)>,
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
    /// Settings as hakod last reported them; `settings_rev` counts every
    /// `GetConfig` / `SetConfig` answer (a refused one too).
    settings: Option<AppSettings>,
    settings_rev: u64,
    /// Why the last `GetConfig` brought no settings.
    settings_error: Option<String>,
    error: Option<String>,
    /// Command outcomes waiting to be shown as toasts.
    notices: Notices,
    /// Daemon calls sent and not yet answered (with the state after them).
    pending: usize,
    /// Newer release, downloaded and waiting for a restart.
    update: Option<update::Ready>,
    /// "Later" was picked: `update` stays downloaded but is not offered
    /// until the next check.
    update_dismissed: bool,
    /// Looking for or downloading a release.
    checking: bool,
    /// Installing `update`.
    updating: bool,
    /// The install task, so a closed window lets it finish.
    install: Option<tokio::task::JoinHandle<()>>,
    /// The window was closed: a finished install must not reopen it.
    closed: bool,
    /// Latest mixer from `GetMixer` / `PatchMixer` / `MixerUpdated`, or why
    /// `GetMixer` failed; `mixer_rev` counts changes.
    mixer: Option<Result<MixerState, String>>,
    mixer_rev: u64,
    /// `PatchMixer` and `ApplyProfile` calls (a profile replaces the layout)
    /// sent and not yet answered.
    mixer_pending: usize,
    /// `ApplyProfile` calls sent and not yet answered, with the mixer after them.
    applying: usize,
    /// Latest network state from `GetNet` / `PatchNet` / `NetUpdated`, or
    /// why `GetNet` failed; `net_rev` counts changes.
    net: Option<Result<NetState, String>>,
    net_rev: u64,
    /// `PatchNet` calls sent and not yet answered.
    net_pending: usize,
    /// Latest hotkeys from `GetHotkeys` / `PatchHotkeys` / `HotkeysUpdated`,
    /// or why `GetHotkeys` failed; `hotkeys_rev` counts changes.
    hotkeys: Option<Result<HotkeysState, String>>,
    hotkeys_rev: u64,
    /// `PatchHotkeys` calls sent and not yet answered.
    hotkeys_pending: usize,
    /// Latest chord capture state; `capture_rev` counts changes.
    capture: Option<HotkeyCaptureState>,
    capture_rev: u64,
    /// Latest Voice state, like `hotkeys`.
    voice: Option<Result<VoiceState, String>>,
    voice_rev: u64,
    /// `PatchVoice` calls sent and not yet answered.
    voice_pending: usize,
    /// Latest live transcript; `partial_rev` counts changes.
    partial: Option<VoicePartial>,
    partial_rev: u64,
}

impl ClientState {
    fn set_mixer(&mut self, m: Result<MixerState, String>) {
        self.mixer = Some(m);
        self.mixer_rev += 1;
    }

    fn set_net(&mut self, n: Result<NetState, String>) {
        self.net = Some(n);
        self.net_rev += 1;
    }

    fn set_hotkeys(&mut self, h: Result<HotkeysState, String>) {
        self.hotkeys = Some(h);
        self.hotkeys_rev += 1;
    }

    fn set_capture(&mut self, c: HotkeyCaptureState) {
        self.capture = Some(c);
        self.capture_rev += 1;
    }

    fn set_voice(&mut self, v: Result<VoiceState, String>) {
        self.voice = Some(v);
        self.voice_rev += 1;
    }

    /// A call for `method` is on its way.
    fn start(&mut self, method: Method) {
        self.pending += 1;
        if matches!(method, Method::PatchMixer | Method::ApplyProfile) {
            self.mixer_pending += 1;
        }
        if method == Method::ApplyProfile {
            self.applying += 1;
        }
        if method == Method::PatchNet {
            self.net_pending += 1;
        }
        if method == Method::PatchHotkeys {
            self.hotkeys_pending += 1;
        }
        if method == Method::PatchVoice {
            self.voice_pending += 1;
        }
    }

    /// A call for `method` was answered, failed or dropped.
    fn finish(&mut self, method: Method) {
        self.pending -= 1;
        if matches!(method, Method::PatchMixer | Method::ApplyProfile) {
            self.mixer_pending -= 1;
        }
        if method == Method::ApplyProfile {
            self.applying -= 1;
        }
        if method == Method::PatchNet {
            self.net_pending -= 1;
        }
        if method == Method::PatchHotkeys {
            self.hotkeys_pending -= 1;
        }
        if method == Method::PatchVoice {
            self.voice_pending -= 1;
        }
    }

    /// A call for `method` must not be sent now: mixer edits address sources
    /// by index, so one made on the layout a profile is replacing would land
    /// on the new layout.
    fn mixer_locked(&self, method: Method) -> bool {
        method == Method::PatchMixer && self.applying > 0
    }
}

/// Toasts to show, at most `MAX_NOTICES`.
#[derive(Default)]
struct Notices(Vec<(Tone, String)>);

impl Notices {
    /// Queue a toast, dropping the oldest non-error ones first past
    /// `MAX_NOTICES` (as `trim_toasts` does on screen).
    fn push(&mut self, notice: (Tone, String)) {
        self.0.push(notice);
        while self.0.len() > MAX_NOTICES {
            let i = self
                .0
                .iter()
                .position(|(t, _)| *t != Tone::Error)
                .unwrap_or(0);
            self.0.remove(i);
        }
    }
}

/// Calls that only read: losing one changes nothing worth telling.
fn read_only(method: Method) -> bool {
    matches!(
        method,
        Method::GetState
            | Method::GetMixer
            | Method::GetNet
            | Method::GetHotkeys
            | Method::GetVoice
    )
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(cli.log_level)
        .init();
    let socket = cli.socket.unwrap_or_else(default_socket_path);
    if let Some(Command::Voice { action, alt }) = cli.command {
        let params = VoiceControlParams {
            action: action.into(),
            alt,
        };
        let done = Runtime::new()?.block_on(voice_control(&socket, params));
        if let Err(e) = done {
            eprintln!("hako voice: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Set by the previous process when it restarted us after an update;
    // cleared before any thread starts so children don't inherit it.
    let mut notices = Notices::default();
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

    let rt = Runtime::new()?;
    let state = Arc::new(Mutex::new(ClientState {
        error: Some(format!("Connecting to {}…", socket.display())),
        notices,
        ..Default::default()
    }));
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientCmd>();

    let updater = update::target();
    if let Some((repo, ..)) = updater {
        rt.spawn(update_checks(repo, state.clone()));
    }

    let meters = Arc::new(Mutex::new(Meters::default()));
    let meter_socket = socket.clone();
    let state_bg = state.clone();
    rt.spawn(async move {
        let mut backoff = RECONNECT_MIN;
        loop {
            match UnixStream::connect(&socket).await {
                Ok(mut stream) => {
                    let mut answered = false;
                    let lost =
                        session(&mut stream, &socket, &state_bg, &mut cmd_rx, &mut answered).await;
                    let Err(e) = lost else { return };
                    if answered {
                        backoff = RECONNECT_MIN;
                    }
                    go_offline(&state_bg, &mut cmd_rx, format!("{e}. Reconnecting…"));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_MAX);
                }
                Err(e) => {
                    let why = format!("Cannot connect to hakod ({e}).");
                    go_offline(&state_bg, &mut cmd_rx, why);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });

    let app = App {
        ui: HakoAppUi::default(),
        cmd_tx,
        state: state.clone(),
        was_online: false,
        lost: false,
        mixer_rev: 0,
        net_rev: 0,
        hotkeys_rev: 0,
        voice_rev: 0,
        capture_rev: 0,
        partial_rev: 0,
        settings_rev: 0,
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
    let ran = eframe::run_native(
        "hako",
        native,
        Box::new(move |cc| {
            handle.spawn(stream_meters(meter_socket, meters, cc.egui_ctx.clone()));
            Ok(Box::new(app))
        }),
    );
    let install = {
        let mut s = state.lock().unwrap();
        s.closed = true;
        s.install.take()
    };
    // An install runs to its end rather than leaving hako half replaced;
    // checks, downloads and xdg-open are abandoned (their lock goes with us).
    if let Some(install) = install {
        let _ = rt.block_on(install);
    }
    rt.shutdown_timeout(SHUTDOWN_GRACE);
    ran.map_err(|e| anyhow::anyhow!("{e}"))
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
            self.ui.net.busy = st.net_pending > 0;
            if st.net_rev != self.net_rev {
                self.net_rev = st.net_rev;
                match &st.net {
                    Some(Ok(n)) => self.ui.net.receive(n.clone()),
                    Some(Err(e)) => self.ui.net.unavailable(e.clone()),
                    None => {}
                }
            }
            self.ui.hotkeys.busy = st.hotkeys_pending > 0;
            if st.hotkeys_rev != self.hotkeys_rev {
                self.hotkeys_rev = st.hotkeys_rev;
                match &st.hotkeys {
                    Some(Ok(h)) => self.ui.hotkeys.receive(h.clone()),
                    Some(Err(e)) => self.ui.hotkeys.unavailable(e.clone()),
                    None => {}
                }
            }
            if st.capture_rev != self.capture_rev {
                self.capture_rev = st.capture_rev;
                if let Some(c) = &st.capture {
                    self.ui.hotkeys.capture_update(c.clone());
                }
            }
            self.ui.voice.busy = st.voice_pending > 0;
            if st.voice_rev != self.voice_rev {
                self.voice_rev = st.voice_rev;
                match &st.voice {
                    Some(Ok(v)) => self.ui.voice.receive(v.clone()),
                    Some(Err(e)) => self.ui.voice.unavailable(e.clone()),
                    None => {}
                }
            }
            if st.partial_rev != self.partial_rev {
                self.partial_rev = st.partial_rev;
                if let Some(p) = &st.partial {
                    self.ui.voice.partial(p.clone());
                }
            }
            // Adopt server settings once every call is answered, so a
            // refused edit flips back but a reply doesn't clobber a newer edit.
            let answered = st.settings_rev != self.settings_rev && st.pending == 0;
            if self.ui.settings.is_none() || st.error.is_some() || answered {
                self.settings_rev = st.settings_rev;
                self.ui.settings = st.settings.clone();
            }
            self.ui.settings_error = st.settings_error.clone();
            {
                // Levels only for the meters the page asked for last frame.
                let m = self.meters.lock().unwrap();
                self.ui.meter_levels = if m.streaming == self.ui.meter_targets {
                    m.levels.clone()
                } else {
                    Vec::new()
                };
            }
            self.ui.update = st
                .update
                .as_ref()
                .filter(|_| !st.update_dismissed)
                .map(|r| UpdateOffer {
                    version: r.version.clone(),
                    busy: st.updating,
                });
            (
                std::mem::take(&mut st.notices.0),
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
                UiCommand::OpenPath(p) => {
                    self.open_path(p);
                    continue;
                }
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
                    self.state.lock().unwrap().update_dismissed = true;
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
                UiCommand::GetNet => ClientCmd::Call(
                    Method::GetNet,
                    serde_json::Value::Null,
                    Feedback::quiet("Couldn't load network audio"),
                ),
                UiCommand::PatchNet(ops) => ClientCmd::Call(
                    Method::PatchNet,
                    to(serde_json::to_value(PatchNetParams { ops })),
                    Feedback::quiet("Couldn't change network audio"),
                ),
                UiCommand::GetHotkeys => ClientCmd::Call(
                    Method::GetHotkeys,
                    serde_json::Value::Null,
                    Feedback::quiet("Couldn't load the hotkeys"),
                ),
                UiCommand::PatchHotkeys(ops) => ClientCmd::Call(
                    Method::PatchHotkeys,
                    to(serde_json::to_value(PatchHotkeysParams { ops })),
                    Feedback::quiet("Couldn't change the hotkey"),
                ),
                UiCommand::StartHotkeyCapture => ClientCmd::Call(
                    Method::StartHotkeyCapture,
                    serde_json::Value::Null,
                    Feedback::quiet("Couldn't start listening for keys"),
                ),
                UiCommand::CancelHotkeyCapture => ClientCmd::Call(
                    Method::CancelHotkeyCapture,
                    serde_json::Value::Null,
                    Feedback::quiet("Couldn't stop listening for keys"),
                ),
                UiCommand::GetVoice => ClientCmd::Call(
                    Method::GetVoice,
                    serde_json::Value::Null,
                    Feedback::quiet("Couldn't load Voice"),
                ),
                UiCommand::PatchVoice(ops) => ClientCmd::Call(
                    Method::PatchVoice,
                    to(serde_json::to_value(PatchVoiceParams { ops })),
                    Feedback::quiet("Couldn't change Voice"),
                ),
                UiCommand::VoiceControl(params) => ClientCmd::Call(
                    Method::VoiceControl,
                    to(serde_json::to_value(params)),
                    Feedback::quiet("Couldn't control dictation"),
                ),
            };
            let ClientCmd::Call(method, ..) = &cmd;
            // Sent under the lock `go_offline` drains in, so a command never
            // waits in the queue for the next connection.
            let mut s = self.state.lock().unwrap();
            if s.error.is_some() {
                if !read_only(*method) {
                    s.notices.push((Tone::Warn, OFFLINE_NOTICE.into()));
                }
                continue;
            }
            if s.mixer_locked(*method) {
                s.notices.push((
                    Tone::Warn,
                    "A profile is being applied; the mixer change was not sent".into(),
                ));
                continue;
            }
            s.start(*method);
            let _ = self.cmd_tx.send(cmd);
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl App {
    fn check_updates(&self) {
        let Some((repo, ..)) = self.updater else {
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
        let Some((_, bin, started)) = self.updater.clone() else {
            return;
        };
        let rel = {
            let mut s = self.state.lock().unwrap();
            let Some(rel) = s.update.clone() else {
                return;
            };
            if s.updating || s.checking {
                let busy = if s.updating {
                    "The update is already being installed"
                } else {
                    "Still checking for updates; try again in a moment"
                };
                s.notices.push((Tone::Warn, busy.into()));
                return;
            }
            s.updating = true;
            rel
        };
        let state = self.state.clone();
        let install = self.rt.spawn_blocking(move || {
            let res = update::install(&rel, &bin, &started);
            // Held through the restart, so a window closed meanwhile stays closed.
            let mut s = state.lock().unwrap();
            let msg = match res {
                Ok(_) if s.closed => return,
                Ok(warn) => {
                    let err = update::restart(&bin, warn.clone());
                    let msg = format!("Updated, but couldn't restart ({err}). Reopen hako.");
                    match warn {
                        Some(w) => format!("{msg} {w}"),
                        None => msg,
                    }
                }
                Err(e) => format!("Update failed: {e:#}"),
            };
            s.updating = false;
            if !rel.on_disk() {
                s.update = None;
            }
            s.notices.push((Tone::Error, msg));
        });
        self.state.lock().unwrap().install = Some(install);
    }

    /// Open `path` in the file manager. Waited for on a blocking thread, so
    /// the child is reaped and a failure is reported.
    fn open_path(&self, path: String) {
        let state = self.state.clone();
        self.rt.spawn_blocking(move || {
            let why = match std::process::Command::new("xdg-open").arg(&path).status() {
                Ok(s) if s.success() => return,
                Ok(s) => format!("xdg-open {s}"),
                Err(e) => format!("cannot run xdg-open: {e}"),
            };
            state
                .lock()
                .unwrap()
                .notices
                .push((Tone::Error, format!("Couldn't open {path}: {why}")));
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
            update::discard_stale();
            return Ok(false);
        };
        let have = state
            .lock()
            .unwrap()
            .update
            .as_ref()
            .filter(|r| r.on_disk())
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
        state.lock().unwrap().update_dismissed = false;
        Ok(true)
    })();
    state.lock().unwrap().checking = false;
    res
}

/// Background release checks while the UI is open, if the user allows them;
/// a newer release is downloaded, then offered. "Later" only hides the
/// offer, so the next check brings it back.
async fn update_checks(repo: &'static str, state: Arc<Mutex<ClientState>>) {
    let mut next = Instant::now();
    let mut failures = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Settings come from the daemon; until it answers, don't assume consent.
        let allowed = state
            .lock()
            .unwrap()
            .settings
            .as_ref()
            .map(|s| s.check_updates);
        if allowed != Some(true) || Instant::now() < next {
            continue;
        }
        let bg = state.clone();
        let res = tokio::task::spawn_blocking(move || fetch_update(repo, &bg, false))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("update check crashed: {e}")));
        next = Instant::now()
            + match res {
                Ok(_) => {
                    failures = 0;
                    UPDATE_EVERY
                }
                // A release that fails verification won't pass on a retry.
                Err(e) if update::rejected(&e) => {
                    eprintln!("hako: update rejected: {e:#}");
                    state
                        .lock()
                        .unwrap()
                        .notices
                        .push((Tone::Error, format!("Update not installed: {e:#}")));
                    UPDATE_EVERY
                }
                Err(e) => {
                    eprintln!("hako: update check failed: {e:#}");
                    failures += 1;
                    retry_after(failures)
                }
            };
    }
}

/// Wait after `failures` failed update checks in a row: `UPDATE_RETRY`,
/// doubling, at most `UPDATE_EVERY`.
fn retry_after(failures: u32) -> Duration {
    UPDATE_RETRY
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(UPDATE_EVERY)
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

/// Serve one daemon connection: the UI's commands, and a slow poll that keeps
/// health, audio, profile, mixer and network state fresh between events (and
/// covers events missed while the event stream was down). Returns `Ok` when
/// the UI is gone, else why the connection was lost. `answered` is set once
/// hakod answered on it.
async fn session(
    stream: &mut UnixStream,
    socket: &std::path::Path,
    state: &Arc<Mutex<ClientState>>,
    cmd_rx: &mut mpsc::UnboundedReceiver<ClientCmd>,
    answered: &mut bool,
) -> Result<(), String> {
    refresh(stream, state).await?;
    *answered = true;
    fetch_config(stream, state).await?;
    fetch_mixer(stream, state).await?;
    fetch_net(stream, state).await?;
    fetch_hotkeys(stream, state).await?;
    fetch_voice(stream, state).await?;
    let mut events = tokio::spawn(subscribe(socket.to_path_buf(), state.clone()));
    let lost = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(ClientCmd::Call(method, params, feedback)) = cmd else { break Ok(()) };
                let done = command(stream, state, method, params, feedback).await;
                state.lock().unwrap().finish(method);
                if let Err(e) = done {
                    break Err(e);
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                let polled = async {
                    refresh(stream, state).await?;
                    let unknown = state.lock().unwrap().settings.is_none();
                    if unknown {
                        fetch_config(stream, state).await?;
                    }
                    fetch_mixer(stream, state).await?;
                    fetch_net(stream, state).await?;
                    fetch_hotkeys(stream, state).await?;
                    fetch_voice(stream, state).await
                };
                if let Err(e) = polled.await {
                    break Err(e);
                }
                if events.is_finished() {
                    events = tokio::spawn(subscribe(socket.to_path_buf(), state.clone()));
                }
            }
        }
    };
    events.abort();
    lost
}

/// Send one UI command and show its outcome. Fails when the connection is
/// unusable; then the change may or may not have been applied, and the user
/// is told so.
async fn command(
    stream: &mut UnixStream,
    state: &Arc<Mutex<ClientState>>,
    method: Method,
    params: serde_json::Value,
    feedback: Feedback,
) -> Result<(), String> {
    let resp = match call(stream, method, params).await {
        Ok(resp) => resp,
        Err(e) => {
            if !read_only(method) {
                state.lock().unwrap().notices.push((
                    Tone::Error,
                    format!(
                        "{}: {e}. The change may not have been applied.",
                        feedback.failed
                    ),
                ));
            }
            return Err(e);
        }
    };
    let why = |resp: IpcResponse| {
        resp.error
            .map_or_else(|| "no reason given".into(), |e| e.message)
    };
    match method {
        // A refused patch: show why, then reload what the daemon has.
        Method::PatchMixer if !resp.ok => {
            let msg = format!("{}: {}", feedback.failed, why(resp));
            state.lock().unwrap().notices.push((Tone::Error, msg));
            fetch_mixer(stream, state).await
        }
        Method::GetMixer | Method::PatchMixer => {
            state.lock().unwrap().set_mixer(mixer_result(resp));
            Ok(())
        }
        Method::PatchNet if !resp.ok => {
            let msg = format!("{}: {}", feedback.failed, why(resp));
            state.lock().unwrap().notices.push((Tone::Error, msg));
            fetch_net(stream, state).await
        }
        Method::GetNet | Method::PatchNet => {
            state.lock().unwrap().set_net(net_result(resp));
            Ok(())
        }
        Method::PatchHotkeys if !resp.ok => {
            let msg = format!("{}: {}", feedback.failed, why(resp));
            state.lock().unwrap().notices.push((Tone::Error, msg));
            fetch_hotkeys(stream, state).await
        }
        Method::GetHotkeys | Method::PatchHotkeys => {
            state.lock().unwrap().set_hotkeys(hotkeys_result(resp));
            Ok(())
        }
        Method::PatchVoice if !resp.ok => {
            let msg = format!("{}: {}", feedback.failed, why(resp));
            state.lock().unwrap().notices.push((Tone::Error, msg));
            fetch_voice(stream, state).await
        }
        Method::GetVoice | Method::PatchVoice => {
            state.lock().unwrap().set_voice(voice_result(resp));
            Ok(())
        }
        // The capture runs on: its progress comes as events. Refused, it
        // ends at once so the page stops waiting.
        Method::StartHotkeyCapture => {
            let mut s = state.lock().unwrap();
            let started = resp
                .result
                .clone()
                .filter(|_| resp.ok)
                .and_then(|r| serde_json::from_value::<HotkeyCaptureState>(r).ok());
            match started {
                Some(c) => s.set_capture(c),
                None => {
                    s.set_capture(HotkeyCaptureState {
                        done: true,
                        cancelled: true,
                        ..Default::default()
                    });
                    let msg = format!("{}: {}", feedback.failed, why(resp));
                    s.notices.push((Tone::Error, msg));
                }
            }
            Ok(())
        }
        Method::CancelHotkeyCapture | Method::VoiceControl => {
            if !resp.ok {
                let msg = format!("{}: {}", feedback.failed, why(resp));
                state.lock().unwrap().notices.push((Tone::Error, msg));
            }
            Ok(())
        }
        _ => {
            {
                let mut s = state.lock().unwrap();
                if method == Method::SetConfig {
                    // A refused draft is replaced by what the daemon kept.
                    s.settings_rev += 1;
                }
                if resp.ok {
                    if let Some(done) = feedback.done {
                        s.notices.push((Tone::Ok, done));
                    }
                    if method == Method::SetConfig {
                        if let Some(cfg) = resp.result.and_then(|r| serde_json::from_value(r).ok())
                        {
                            s.settings = Some(cfg);
                        }
                    }
                } else {
                    let msg = format!("{}: {}", feedback.failed, why(resp));
                    s.notices.push((Tone::Error, msg));
                }
            }
            refresh(stream, state).await?;
            // A profile carries a mixer layout: show it even if the
            // `MixerUpdated` event is missed.
            if method == Method::ApplyProfile {
                fetch_mixer(stream, state).await?;
            }
            Ok(())
        }
    }
}

/// The connection is gone: show `why`, and drop commands still queued (with
/// one notice), so nothing clicked before the UI noticed runs when hakod is
/// back. New commands are not queued while `error` is set.
fn go_offline(
    state: &Mutex<ClientState>,
    cmd_rx: &mut mpsc::UnboundedReceiver<ClientCmd>,
    why: String,
) {
    let mut s = state.lock().unwrap();
    s.error = Some(why);
    let mut dropped = 0;
    while let Ok(ClientCmd::Call(method, ..)) = cmd_rx.try_recv() {
        s.finish(method);
        dropped += usize::from(!read_only(method));
    }
    if dropped > 0 {
        s.notices.push((
            Tone::Warn,
            format!("hakod is unavailable; {dropped} queued change(s) were not applied"),
        ));
    }
}

/// One request/response round trip. An error means the connection is
/// unusable (gone, silent, or answering in a form this UI cannot read): the
/// caller reconnects, so a late reply can never be read as the answer to a
/// later request.
async fn call(
    stream: &mut UnixStream,
    method: Method,
    params: serde_json::Value,
) -> Result<IpcResponse, String> {
    let req = IpcRequest {
        id: serde_json::to_value(method).map_err(|e| e.to_string())?,
        method,
        params,
    };
    let round_trip = async {
        write_json_frame(stream, &req).await?;
        read_json_frame(stream).await
    };
    let v = tokio::time::timeout(CALL_TIMEOUT, round_trip)
        .await
        .map_err(|_| format!("hakod did not answer within {}s", CALL_TIMEOUT.as_secs()))?
        .map_err(|e| format!("Disconnected from hakod ({e:#})"))?;
    serde_json::from_value(v)
        .map_err(|e| format!("hakod sent a reply this UI cannot read ({e}); update both"))
}

/// Pull the full hub state. Fails when the connection is unusable.
async fn refresh(stream: &mut UnixStream, state: &Arc<Mutex<ClientState>>) -> Result<(), String> {
    let resp = call(stream, Method::GetState, serde_json::Value::Null).await?;
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
        // hakod answers, but the top bar only says it is offline: say why
        // (e.g. it runs another version), once.
        Err(e) => {
            if s.error.as_deref() != Some(e.as_str()) {
                s.notices.push((Tone::Error, e.clone()));
            }
            s.error = Some(e);
        }
    }
    Ok(())
}

/// Load the app settings, or keep why hakod could not give them (e.g. an
/// unparsable `config.toml`). Fails when the connection is unusable.
async fn fetch_config(
    stream: &mut UnixStream,
    state: &Arc<Mutex<ClientState>>,
) -> Result<(), String> {
    let resp = call(stream, Method::GetConfig, serde_json::Value::Null).await?;
    let cfg = match (resp.result, resp.error) {
        (Some(r), _) if resp.ok => serde_json::from_value(r)
            .map_err(|e| format!("hakod sent settings this UI cannot read ({e}); update both")),
        (_, e) => Err(e.map_or_else(|| "no reason given".into(), |e| e.message)),
    };
    let mut s = state.lock().unwrap();
    match cfg {
        Ok(cfg) => {
            s.settings = Some(cfg);
            s.settings_error = None;
            s.settings_rev += 1;
        }
        Err(e) => s.settings_error = Some(e),
    }
    Ok(())
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

/// Reload the mixer. Fails when the connection is unusable.
async fn fetch_mixer(
    stream: &mut UnixStream,
    state: &Arc<Mutex<ClientState>>,
) -> Result<(), String> {
    let resp = call(stream, Method::GetMixer, serde_json::Value::Null).await?;
    state.lock().unwrap().set_mixer(mixer_result(resp));
    Ok(())
}

/// A `GetNet` / `PatchNet` answer: the state, or why there is none (an
/// older hakod answers `unsupported`).
fn net_result(resp: IpcResponse) -> Result<NetState, String> {
    match (resp.result, resp.error) {
        (Some(r), _) if resp.ok => serde_json::from_value(r).map_err(|e| {
            format!("hakod sent a network state this UI cannot read ({e}); update both")
        }),
        (_, Some(e)) if e.code == "unsupported" => {
            Err("This hakod has no network audio yet. Update hako to use it.".into())
        }
        (_, e) => Err(e.map_or_else(|| "no reason given".into(), |e| e.message)),
    }
}

/// Reload network audio. Fails when the connection is unusable.
async fn fetch_net(stream: &mut UnixStream, state: &Arc<Mutex<ClientState>>) -> Result<(), String> {
    let resp = call(stream, Method::GetNet, serde_json::Value::Null).await?;
    state.lock().unwrap().set_net(net_result(resp));
    Ok(())
}

/// A hakod from before Voice: `unsupported`, or a method it cannot parse.
fn too_old(e: &hako_core::ipc::IpcErrorBody) -> bool {
    e.code == "unsupported" || (e.code == "invalid_value" && e.message.contains("unknown variant"))
}

/// A `GetHotkeys` / `PatchHotkeys` answer: the state, or why there is none.
fn hotkeys_result(resp: IpcResponse) -> Result<HotkeysState, String> {
    match (resp.result, resp.error) {
        (Some(r), _) if resp.ok => serde_json::from_value(r)
            .map_err(|e| format!("hakod sent hotkeys this UI cannot read ({e}); update both")),
        (_, Some(e)) if too_old(&e) => Err("Hotkeys need a newer hakod.".into()),
        (_, e) => Err(e.map_or_else(|| "no reason given".into(), |e| e.message)),
    }
}

/// Reload the hotkeys. Fails when the connection is unusable.
async fn fetch_hotkeys(
    stream: &mut UnixStream,
    state: &Arc<Mutex<ClientState>>,
) -> Result<(), String> {
    let resp = call(stream, Method::GetHotkeys, serde_json::Value::Null).await?;
    state.lock().unwrap().set_hotkeys(hotkeys_result(resp));
    Ok(())
}

/// A `GetVoice` / `PatchVoice` answer: the state, or why there is none.
fn voice_result(resp: IpcResponse) -> Result<VoiceState, String> {
    match (resp.result, resp.error) {
        (Some(r), _) if resp.ok => serde_json::from_value(r).map_err(|e| {
            format!("hakod sent a Voice state this UI cannot read ({e}); update both")
        }),
        (_, Some(e)) if too_old(&e) => Err("Voice needs a newer hakod.".into()),
        (_, e) => Err(e.map_or_else(|| "no reason given".into(), |e| e.message)),
    }
}

/// Reload Voice. Fails when the connection is unusable.
async fn fetch_voice(
    stream: &mut UnixStream,
    state: &Arc<Mutex<ClientState>>,
) -> Result<(), String> {
    let resp = call(stream, Method::GetVoice, serde_json::Value::Null).await?;
    state.lock().unwrap().set_voice(voice_result(resp));
    Ok(())
}

/// `hako voice ...`: one `VoiceControl` request; the daemon's refusal is the error.
async fn voice_control(socket: &std::path::Path, params: VoiceControlParams) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("cannot connect to hakod at {} ({e})", socket.display()))?;
    let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
    let resp = call(&mut stream, Method::VoiceControl, params).await?;
    match resp.error {
        _ if resp.ok => Ok(()),
        Some(e) if too_old(&e) => Err("this hakod has no Voice yet; update it".into()),
        e => Err(e.map_or_else(|| "refused".into(), |e| e.message)),
    }
}

/// Event stream on its own connection; snapshots are applied in place.
/// Events missed around it (before it subscribed, while it was down, or
/// dropped by a lagging daemon) are caught up by the session's poll.
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
    .is_err()
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
        if ev.kind == EventKind::NetUpdated {
            if let Ok(n) = serde_json::from_value::<NetState>(ev.payload) {
                s.set_net(Ok(n));
            }
            continue;
        }
        match ev.kind {
            EventKind::HotkeysUpdated => {
                if let Ok(h) = serde_json::from_value::<HotkeysState>(ev.payload) {
                    s.set_hotkeys(Ok(h));
                }
                continue;
            }
            EventKind::HotkeyCapture => {
                if let Ok(c) = serde_json::from_value::<HotkeyCaptureState>(ev.payload) {
                    s.set_capture(c);
                }
                continue;
            }
            EventKind::VoiceUpdated => {
                if let Ok(v) = serde_json::from_value::<VoiceState>(ev.payload) {
                    s.set_voice(Ok(v));
                }
                continue;
            }
            EventKind::VoicePartial => {
                if let Ok(p) = serde_json::from_value::<VoicePartial>(ev.payload) {
                    s.partial = Some(p);
                    s.partial_rev += 1;
                }
                continue;
            }
            _ => {}
        }
        if ev.kind == EventKind::Error {
            if let Some(msg) = ev.payload.get("message").and_then(|m| m.as_str()) {
                s.notices.push((Tone::Error, msg.to_string()));
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
            EventKind::RemapCaptured
            | EventKind::Error
            | EventKind::DaemonShuttingDown
            | EventKind::Meters
            | EventKind::MixerUpdated
            | EventKind::NetUpdated
            | EventKind::HotkeysUpdated
            | EventKind::HotkeyCapture
            | EventKind::VoiceUpdated
            | EventKind::VoicePartial => {}
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
        if !subscribed.is_ok_and(|r| r.ok) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn going_offline_drops_queued_commands_with_one_notice() {
        let state = Mutex::new(ClientState::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        for method in [Method::ResetAudio, Method::PatchMixer, Method::GetMixer] {
            state.lock().unwrap().start(method);
            let feedback = Feedback::quiet("failed");
            tx.send(ClientCmd::Call(method, serde_json::Value::Null, feedback))
                .unwrap();
        }
        go_offline(&state, &mut rx, "gone".into());
        let s = state.lock().unwrap();
        assert!(rx.try_recv().is_err(), "nothing left to replay");
        assert_eq!((s.pending, s.mixer_pending), (0, 0));
        assert_eq!(s.error.as_deref(), Some("gone"));
        assert_eq!(s.notices.0.len(), 1);
        assert!(s.notices.0[0].1.contains("2 queued"), "{:?}", s.notices.0);
    }

    #[tokio::test]
    async fn a_state_the_ui_cannot_read_is_told_once() {
        let (mut ui, mut daemon) = UnixStream::pair().unwrap();
        let state = Arc::new(Mutex::new(ClientState::default()));
        for _ in 0..2 {
            let answer = async {
                let req = read_json_frame(&mut daemon).await.unwrap();
                let resp = IpcResponse::ok(req["id"].clone(), serde_json::json!({ "devices": 1 }));
                write_json_frame(&mut daemon, &resp).await.unwrap();
            };
            let (asked, ()) = tokio::join!(refresh(&mut ui, &state), answer);
            asked.unwrap();
        }
        let s = state.lock().unwrap();
        let error = s.error.clone().unwrap_or_default();
        assert!(error.contains("cannot read"), "{error}");
        assert_eq!(s.notices.0.len(), 1, "{:?}", s.notices.0);
        assert_eq!(s.notices.0[0].1, error);
    }

    #[test]
    fn queued_notices_are_capped_keeping_errors() {
        let mut n = Notices::default();
        n.push((Tone::Error, "failed".into()));
        for i in 0..MAX_NOTICES * 2 {
            n.push((Tone::Ok, format!("ok {i}")));
        }
        assert_eq!(n.0.len(), MAX_NOTICES);
        assert_eq!(n.0[0].1, "failed");
        assert_eq!(
            n.0[MAX_NOTICES - 1].1,
            format!("ok {}", MAX_NOTICES * 2 - 1)
        );
        for i in 0..MAX_NOTICES + 1 {
            n.push((Tone::Error, format!("e{i}")));
        }
        assert_eq!(n.0.len(), MAX_NOTICES);
        assert_eq!(n.0[0].1, "e1", "only errors left: the oldest goes");
    }

    #[test]
    fn mixer_edits_wait_for_an_applied_profile_layout() {
        let mut s = ClientState::default();
        s.start(Method::ApplyProfile);
        assert_eq!(s.mixer_pending, 1, "the mixer page holds its state");
        assert!(s.mixer_locked(Method::PatchMixer));
        assert!(!s.mixer_locked(Method::GetMixer));
        s.finish(Method::ApplyProfile);
        assert_eq!(s.mixer_pending, 0);
        assert!(!s.mixer_locked(Method::PatchMixer));
    }

    #[test]
    fn voice_commands_parse_and_no_command_opens_the_window() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.command);
        assert_eq!(parse(&["hako"]).unwrap(), None);
        assert_eq!(
            parse(&["hako", "voice", "toggle"]).unwrap(),
            Some(Command::Voice {
                action: VoiceVerb::Toggle,
                alt: false
            })
        );
        assert_eq!(
            parse(&["hako", "voice", "start", "--alt", "--socket", "/tmp/x"]).unwrap(),
            Some(Command::Voice {
                action: VoiceVerb::Start,
                alt: true
            })
        );
        assert!(parse(&["hako", "voice"]).is_err());
        assert!(parse(&["hako", "voice", "pause"]).is_err());
        let cli = Cli::try_parse_from(["hako", "--socket", "/tmp/s", "voice", "cancel"]).unwrap();
        assert_eq!(cli.socket.as_deref(), Some(std::path::Path::new("/tmp/s")));
        assert_eq!(VoiceAction::from(VoiceVerb::Stop), VoiceAction::Stop);
    }

    #[tokio::test]
    async fn voice_control_reports_the_daemons_answer() {
        let dir = std::env::temp_dir().join(format!("hako-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sock");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let answers = [
            IpcResponse::ok(serde_json::json!("VoiceControl"), serde_json::json!({})),
            IpcResponse::err(
                serde_json::json!(null),
                "invalid_value",
                "no model is ready",
            ),
            IpcResponse::err(
                serde_json::json!(null),
                "unsupported",
                "voice is not available",
            ),
        ];
        let daemon = tokio::spawn(async move {
            for resp in answers {
                let (mut s, _) = listener.accept().await.unwrap();
                let req = read_json_frame(&mut s).await.unwrap();
                assert_eq!(req["method"], "VoiceControl");
                assert_eq!(req["params"]["action"], "toggle");
                write_json_frame(&mut s, &resp).await.unwrap();
            }
        });
        let params = || VoiceControlParams {
            action: VoiceAction::Toggle,
            alt: false,
        };
        assert_eq!(voice_control(&path, params()).await, Ok(()));
        assert_eq!(
            voice_control(&path, params()).await,
            Err("no model is ready".into())
        );
        let old = voice_control(&path, params()).await.unwrap_err();
        assert!(old.contains("no Voice"), "{old}");
        daemon.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_hakod_without_voice_is_explained() {
        let unknown = IpcResponse::err(
            serde_json::json!(null),
            "invalid_value",
            "unknown variant `GetVoice`, expected one of ...",
        );
        assert_eq!(
            voice_result(unknown).unwrap_err(),
            "Voice needs a newer hakod."
        );
        let stub = IpcResponse::err(serde_json::json!(1), "unsupported", "not yet");
        assert_eq!(
            hotkeys_result(stub).unwrap_err(),
            "Hotkeys need a newer hakod."
        );
        let other = IpcResponse::err(serde_json::json!(1), "io", "disk full");
        assert_eq!(voice_result(other).unwrap_err(), "disk full");
    }

    #[test]
    fn failed_update_checks_retry_sooner_with_backoff() {
        assert_eq!(retry_after(1), UPDATE_RETRY);
        assert_eq!(retry_after(2), UPDATE_RETRY * 2);
        assert_eq!(retry_after(7), UPDATE_EVERY);
        assert_eq!(retry_after(u32::MAX), UPDATE_EVERY);
    }
}
