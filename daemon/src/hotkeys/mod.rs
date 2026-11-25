//! App-wide hotkeys: reads `hotkeys.toml`, matches chords on the remap
//! engine's evdev events and tells the owners of the actions (voice, ...).
//! Design: `docs/superpowers/specs/2026-11-25-voice-stt-design.md` (Hotkeys).
//!
//! The remap thread feeds [`Hotkeys`] (through a [`HotkeyLink`]) every key
//! event of every node it reads, as the physical key; the matcher
//! ([`matcher::Matcher`]) is pure state behind one mutex. Edges go to the
//! [`TriggerSink`] on a thread of their own, so a slow sink never stalls input.

mod capture;
mod desktop;
pub mod inputs;
pub mod matcher;

use crate::mixer::PatchError;
use capture::Capture;
use desktop::Shortcut;
use hako_core::config::ConfigPaths;
use hako_core::hotkey::{
    apply_hotkey_ops, load_hotkeys, side_neutral, update_hotkeys, HotkeyAction, HotkeyBinding,
    HotkeyCaptureState, HotkeyConflict, HotkeyOp, HotkeySource, HotkeyStatus, HotkeysFile,
    HotkeysState, InputDeviceInfo, PassThrough,
};
use hako_core::protocol::AdapterEvent;
use inputs::InputNode;
use matcher::{is_mouse_button, Matcher, Node, NodeInfo, Verdict};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{error, warn};

/// Name of the virtual keyboard the voice service types with; the matcher
/// never reads it (typed text must not trigger hotkeys).
pub const VOICE_KEYBOARD_NAME: &str = "hako voice keyboard";

/// How often logind's `LockedHint` is read while hotkeys listen.
const LOCK_POLL: Duration = Duration::from_millis(1500);
/// Desktop shortcuts are read again at most this often.
const DESKTOP_TTL: Duration = Duration::from_secs(30);

/// What a binding's mode turned a chord press / release into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEdge {
    /// Start recording (Hold / Toggle / Smart press).
    Start,
    /// Stop and output (release in Hold / long Smart press, second Toggle press).
    Stop,
    /// Throw the session away (too short a hold, a modifier-only chord
    /// followed by another key).
    Discard,
    /// A momentary action fired (cancel).
    Fire,
}

pub type TriggerSink = Box<dyn Fn(HotkeyAction, HotkeyEdge) + Send + Sync>;

/// A registered mouse / keyboard as the remap engine knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemapDevice {
    pub name: String,
    pub vendor_id: u16,
    /// Own PIDs first, then its receiver's ([`crate::remap::RemapPids`]).
    pub pids: Vec<u16>,
    pub own: usize,
    /// Keys with a remap binding.
    pub remapped: BTreeSet<u16>,
}

impl RemapDevice {
    fn own(&self) -> &[u16] {
        &self.pids[..self.own.min(self.pids.len())]
    }
}

struct Inner {
    file: HotkeysFile,
    load_error: Option<String>,
    matcher: Matcher,
    capture: Option<Capture>,
    /// Every keyboard / mouse node (the remap thread's last scan).
    inputs: Vec<InputNode>,
    devices: Vec<RemapDevice>,
    /// A remap capture / record session runs: nothing fires.
    remap_capture: bool,
    /// The state last pushed as `HotkeysUpdated`.
    published: Option<HotkeysState>,
}

/// Hotkey state shared by the service (IPC, voice) and the remap thread.
pub struct Hotkeys {
    paths: Option<ConfigPaths>,
    events: broadcast::Sender<AdapterEvent>,
    inner: Mutex<Inner>,
    fired: mpsc::Sender<(HotkeyAction, HotkeyEdge)>,
    sink: Arc<Mutex<Option<TriggerSink>>>,
    /// (read at, shortcuts)
    desktop: Mutex<Option<(Instant, Vec<Shortcut>)>>,
}

/// What the remap engine holds.
pub type HotkeyLink = Arc<Hotkeys>;

pub struct HotkeyService {
    shared: Arc<Hotkeys>,
}

impl Default for HotkeyService {
    /// Defaults, no config directory (edits are refused), no lock watch.
    fn default() -> Self {
        Self::build(None, broadcast::channel(1).0, HotkeysFile::default(), None)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl HotkeyService {
    pub fn start(paths: ConfigPaths, events: broadcast::Sender<AdapterEvent>) -> Self {
        let (file, load_error) = match load_hotkeys(&paths) {
            Ok(file) => (file, None),
            Err(e) => {
                warn!("{e:#}");
                (unreadable_file(), Some(unreadable_message(&e)))
            }
        };
        let svc = Self::build(Some(paths), events, file, load_error);
        let weak = Arc::downgrade(&svc.shared);
        if let Err(e) = std::thread::Builder::new()
            .name("hako-hotkeys-lock".into())
            .spawn(move || watch_lock(weak))
        {
            error!("cannot start the lock-screen watch: {e}");
        }
        svc
    }

    fn build(
        paths: Option<ConfigPaths>,
        events: broadcast::Sender<AdapterEvent>,
        file: HotkeysFile,
        load_error: Option<String>,
    ) -> Self {
        let (fired, rx) = mpsc::channel::<(HotkeyAction, HotkeyEdge)>();
        let sink: Arc<Mutex<Option<TriggerSink>>> = Arc::default();
        let to = sink.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("hako-hotkeys".into())
            .spawn(move || {
                for (action, edge) in rx {
                    if let Some(sink) = lock(&to).as_ref() {
                        sink(action, edge);
                    }
                }
            })
        {
            error!("cannot start the hotkey thread: {e}");
        }
        let mut matcher = Matcher::default();
        matcher.set_bindings(&file.bindings);
        Self {
            shared: Arc::new(Hotkeys {
                paths,
                events,
                inner: Mutex::new(Inner {
                    file,
                    load_error,
                    matcher,
                    capture: None,
                    inputs: Vec::new(),
                    devices: Vec::new(),
                    remap_capture: false,
                    published: None,
                }),
                fired,
                sink,
                desktop: Mutex::new(None),
            }),
        }
    }

    /// No config directory, these bindings.
    #[cfg(test)]
    pub fn with_bindings(bindings: Vec<HotkeyBinding>) -> Self {
        let file = HotkeysFile {
            bindings,
            ..HotkeysFile::default()
        };
        Self::build(None, broadcast::channel(1).0, file, None)
    }

    /// For the remap engine, which feeds the matcher.
    pub fn link(&self) -> HotkeyLink {
        self.shared.clone()
    }

    /// Where chord edges go (the hub wires the voice service here). Called
    /// on a hotkey thread of its own, in order.
    pub fn on_trigger(&self, sink: TriggerSink) {
        *lock(&self.shared.sink) = Some(sink);
    }

    /// Actions whose feature is on; others never fire and show as inactive.
    pub fn set_active(&self, actions: BTreeSet<HotkeyAction>) {
        let mut g = lock(&self.shared.inner);
        g.matcher.set_active(actions);
        self.shared.publish(&mut g);
    }

    /// The cancel action is only live while a recording runs.
    pub fn set_recording(&self, recording: bool) {
        lock(&self.shared.inner).matcher.set_recording(recording);
    }

    /// Modifier keys (evdev codes) apps see held right now: physically held
    /// on a node hotkeys read, minus those a Block chord released for them.
    pub fn held_modifiers(&self) -> Vec<u16> {
        lock(&self.shared.inner).matcher.held_modifiers()
    }

    /// `GetHotkeys`. Blocking: may read the desktop's shortcuts.
    pub fn state(&self) -> HotkeysState {
        let desktop = self.shared.desktop(true);
        let mut g = lock(&self.shared.inner);
        self.shared.retry_load(&mut g);
        let state = state_of(&g, &desktop);
        if g.published.as_ref() != Some(&state) {
            g.published = Some(state.clone());
            let _ =
                (self.shared.events).send(AdapterEvent::HotkeysUpdated(Box::new(state.clone())));
        }
        state
    }

    /// `PatchHotkeys`: all or nothing; refused while `hotkeys.toml` is unreadable.
    pub fn patch(&self, ops: &[HotkeyOp]) -> Result<HotkeysState, PatchError> {
        let Some(paths) = &self.shared.paths else {
            return Err(PatchError::Io(anyhow::anyhow!("no config directory")));
        };
        {
            let mut g = lock(&self.shared.inner);
            self.shared.retry_load(&mut g);
            if let Some(e) = &g.load_error {
                return Err(PatchError::Io(anyhow::anyhow!("{e}")));
            }
            apply_hotkey_ops(&g.file, ops).map_err(PatchError::Invalid)?;
        }
        let file = update_hotkeys(paths, |f| apply_hotkey_ops(f, ops)).map_err(PatchError::Io)?;
        {
            let mut g = lock(&self.shared.inner);
            g.matcher.set_bindings(&file.bindings);
            g.file = file;
            self.shared.flush(&mut g);
        }
        Ok(self.state())
    }

    /// `StartHotkeyCapture`: record the next chord on any keyboard / mouse
    /// (replaces a capture that still runs).
    pub fn start_capture(&self) -> HotkeyCaptureState {
        let mut g = lock(&self.shared.inner);
        let capture = Capture::new(Instant::now());
        let state = capture.state.clone();
        g.capture = Some(capture);
        g.matcher.set_capturing(true);
        let _ = (self.shared.events).send(AdapterEvent::HotkeyCapture(state.clone()));
        state
    }

    /// `CancelHotkeyCapture`.
    pub fn cancel_capture(&self) -> HotkeyCaptureState {
        let mut g = lock(&self.shared.inner);
        let mut capture = g
            .capture
            .take()
            .unwrap_or_else(|| Capture::new(Instant::now()));
        capture.cancel();
        self.shared.end_capture(&mut g, capture.state.clone());
        capture.state
    }
}

impl Hotkeys {
    /// Something to read for: a live binding or a capture.
    pub fn want_listen(&self) -> bool {
        let g = lock(&self.inner);
        g.capture.is_some() || live(&g).next().is_some()
    }

    /// The remap engine should grab this registered device so a chord can
    /// be held back: a Block chord with keyboard keys (keyboards), or a
    /// chord with a mouse button (mice).
    pub fn wants_grab(&self, vendor_id: u16, pids: &[u16], keyboard: bool) -> bool {
        let g = lock(&self.inner);
        let covered = |b: &&HotkeyBinding| pids.iter().any(|&p| b.scope.covers(vendor_id, p));
        let held_back = |b: &&HotkeyBinding| {
            b.keys.iter().any(|k| {
                if keyboard {
                    b.pass == PassThrough::Block && !is_mouse_button(k.0)
                } else {
                    is_mouse_button(k.0)
                }
            })
        };
        let wants = live(&g).filter(covered).any(|b| held_back(&b));
        wants
    }

    /// One physical key event of `node`; the verdict says what the remap
    /// engine forwards.
    pub fn key(&self, node: Node, code: u16, value: i32, now: Instant) -> Verdict {
        let mut g = lock(&self.inner);
        if let Some(c) = g.capture.as_mut() {
            if c.key(code, value) {
                let state = c.state.clone();
                if state.done {
                    g.capture = None;
                    self.end_capture(&mut g, state);
                } else {
                    let _ = self.events.send(AdapterEvent::HotkeyCapture(state));
                }
            }
        }
        let v = g.matcher.key(node, code, value, now);
        self.flush(&mut g);
        v
    }

    /// The nodes the matcher is fed from; keys of nodes gone are released.
    pub fn sync_nodes(&self, nodes: BTreeMap<Node, NodeInfo>, now: Instant) {
        let mut g = lock(&self.inner);
        g.matcher.sync_nodes(nodes, now);
        self.flush(&mut g);
    }

    /// `down`: the keys a node reports held (`EVIOCGKEY`) as it is opened.
    pub fn resync(&self, node: Node, down: &[u16], now: Instant) {
        let mut g = lock(&self.inner);
        g.matcher.resync(node, down, now);
        self.flush(&mut g);
    }

    /// Watchdog: `down` is what a node of a held chord reports now.
    pub fn release_missing(&self, node: Node, down: &[u16], now: Instant) {
        let mut g = lock(&self.inner);
        g.matcher.release_missing(node, down, now);
        self.flush(&mut g);
    }

    /// Nodes to re-read for the watchdog.
    pub fn watch_nodes(&self) -> BTreeSet<Node> {
        lock(&self.inner).matcher.watch_nodes()
    }

    /// Deadlines: a capture nobody finishes is cancelled.
    pub fn tick(&self, now: Instant) {
        let mut g = lock(&self.inner);
        let Some(c) = g.capture.as_mut() else { return };
        if !c.expire(now) {
            return;
        }
        let state = c.state.clone();
        g.capture = None;
        self.end_capture(&mut g, state);
    }

    pub fn set_inputs(&self, inputs: Vec<InputNode>) {
        let mut g = lock(&self.inner);
        g.inputs = inputs;
        self.publish(&mut g);
    }

    /// Registered devices and their remaps (on every remap change).
    pub fn set_devices(&self, devices: Vec<RemapDevice>) {
        let mut g = lock(&self.inner);
        g.devices = devices;
        self.publish(&mut g);
    }

    pub fn set_remap_capture(&self, on: bool) {
        let mut g = lock(&self.inner);
        if g.remap_capture != on {
            g.remap_capture = on;
            let capturing = on || g.capture.is_some();
            g.matcher.set_capturing(capturing);
        }
    }

    fn end_capture(&self, g: &mut Inner, state: HotkeyCaptureState) {
        let capturing = g.remap_capture || g.capture.is_some();
        g.matcher.set_capturing(capturing);
        let _ = self.events.send(AdapterEvent::HotkeyCapture(state));
    }

    /// Hand the matcher's edges to the sink thread.
    fn flush(&self, g: &mut Inner) {
        for edge in g.matcher.take_edges() {
            let _ = self.fired.send(edge);
        }
    }

    /// Push `HotkeysUpdated` when the state changed (desktop shortcuts as
    /// last read: this runs on the remap thread too).
    fn publish(&self, g: &mut Inner) {
        let state = state_of(g, &self.desktop(false));
        if g.published.as_ref() != Some(&state) {
            g.published = Some(state.clone());
            let _ = self
                .events
                .send(AdapterEvent::HotkeysUpdated(Box::new(state)));
        }
    }

    /// `hotkeys.toml` failed to load: try again (the user may have fixed it).
    fn retry_load(&self, g: &mut Inner) {
        let (Some(paths), Some(_)) = (&self.paths, &g.load_error) else {
            return;
        };
        match load_hotkeys(paths) {
            Ok(file) => {
                g.matcher.set_bindings(&file.bindings);
                g.file = file;
                g.load_error = None;
            }
            Err(e) => g.load_error = Some(unreadable_message(&e)),
        }
    }

    /// Desktop shortcuts; `refresh` reads them again when stale (blocking).
    fn desktop(&self, refresh: bool) -> Vec<Shortcut> {
        let cached = lock(&self.desktop).clone();
        match cached {
            Some((at, list)) if !refresh || at.elapsed() < DESKTOP_TTL => list,
            None if !refresh => Vec::new(),
            _ => {
                let list = desktop::load();
                *lock(&self.desktop) = Some((Instant::now(), list.clone()));
                list
            }
        }
    }
}

/// Bindings that can fire: enabled, and their feature is on.
fn live(g: &Inner) -> impl Iterator<Item = &HotkeyBinding> {
    (g.file.bindings.iter()).filter(|b| b.enabled && g.matcher.active().contains(&b.action))
}

/// What is used while `hotkeys.toml` cannot be read: nothing fires.
fn unreadable_file() -> HotkeysFile {
    HotkeysFile {
        bindings: Vec::new(),
        ..HotkeysFile::default()
    }
}

fn unreadable_message(e: &anyhow::Error) -> String {
    format!("{e:#}. Hotkeys are off and cannot be edited until the file is fixed.")
}

fn state_of(g: &Inner, desktop: &[Shortcut]) -> HotkeysState {
    HotkeysState {
        file: g.file.clone(),
        status: (g.file.bindings.iter())
            .map(|b| status(b, g.matcher.active(), &g.inputs, &g.devices, desktop))
            .collect(),
        inputs: (g.inputs.iter())
            .map(|i| InputDeviceInfo {
                name: i.name.clone(),
                node: i.path.display().to_string(),
                vendor_id: i.vendor_id,
                product_id: i.product_id,
                readable: i.readable,
            })
            .collect(),
        load_error: g.load_error.clone(),
    }
}

/// The registered device an input node belongs to.
fn owner<'a>(i: &InputNode, devices: &'a [RemapDevice]) -> Option<&'a RemapDevice> {
    (devices.iter()).find(|d| d.vendor_id == i.vendor_id && d.pids.contains(&i.product_id))
}

/// One binding as the daemon sees it: who can fire it, what it clashes with.
fn status(
    b: &HotkeyBinding,
    active: &BTreeSet<HotkeyAction>,
    inputs: &[InputNode],
    devices: &[RemapDevice],
    desktop: &[Shortcut],
) -> HotkeyStatus {
    let covered: Vec<(&InputNode, String)> = (inputs.iter())
        .filter_map(|i| {
            let dev = owner(i, devices);
            let by_device =
                dev.is_some_and(|d| d.own().iter().any(|&p| b.scope.covers(d.vendor_id, p)));
            (b.scope.covers(i.vendor_id, i.product_id) || by_device)
                .then(|| (i, dev.map_or(i.name.clone(), |d| d.name.clone())))
        })
        .collect();
    let mut readable = Vec::new();
    let mut blocked = Vec::new();
    for (i, name) in &covered {
        let list = if i.readable {
            &mut readable
        } else {
            &mut blocked
        };
        if !list.contains(name) {
            list.push(name.clone());
        }
    }
    let mut problems = Vec::new();
    if covered.is_empty() {
        problems.push("No keyboard or mouse it listens to is connected.".into());
    } else if !blocked.is_empty() {
        problems.push(format!(
            "Cannot read {}: install the hako udev rules (they give the logged-in user read access to keyboards and mice), then log in again.",
            blocked.join(", ")
        ));
    }
    let same = |a: u16, k: u16| a == k || (!b.exact_side && side_neutral(a) == side_neutral(k));
    let mut conflicts = Vec::new();
    if !b.with_remaps {
        for d in devices
            .iter()
            .filter(|d| d.own().iter().any(|&p| b.scope.covers(d.vendor_id, p)))
        {
            for k in b
                .keys
                .iter()
                .filter(|k| d.remapped.iter().any(|&r| same(r, k.0)))
            {
                conflicts.push(HotkeyConflict::Remap {
                    device: d.name.clone(),
                    vendor_id: d.vendor_id,
                    product_id: d.pids[0],
                    key: *k,
                });
            }
        }
    }
    let chord: BTreeSet<u16> = b.keys.iter().map(|k| side_neutral(k.0)).collect();
    for s in desktop.iter().filter(|s| s.keys == chord) {
        let c = HotkeyConflict::Desktop {
            shortcut: s.name.clone(),
        };
        if !conflicts.contains(&c) {
            conflicts.push(c);
        }
    }
    HotkeyStatus {
        id: b.id.clone(),
        source: if readable.is_empty() {
            HotkeySource::Cli
        } else {
            HotkeySource::Evdev
        },
        active: active.contains(&b.action),
        devices: readable,
        conflicts,
        problems,
    }
}

/// Keep the matcher's `locked` in step with logind's `LockedHint` while
/// hotkeys listen (evdev ignores lock screens; we must not).
fn watch_lock(shared: Weak<Hotkeys>) {
    let mut session = None;
    loop {
        // ponytail: a poll; logind's PropertiesChanged would need a D-Bus client.
        std::thread::sleep(LOCK_POLL);
        let Some(hk) = shared.upgrade() else { return };
        if !hk.want_listen() {
            continue;
        }
        let locked = session_locked(&mut session).unwrap_or(false);
        lock(&hk.inner).matcher.set_locked(locked);
    }
}

/// `loginctl` output, or None (no logind, failed, hung past 2 s).
fn loginctl(args: &[&str]) -> Option<String> {
    let out = Command::new("timeout")
        .args(["2", "loginctl"])
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The user's graphical session is locked; `session` caches its id.
fn session_locked(session: &mut Option<String>) -> Option<bool> {
    if session.is_none() {
        // SAFETY: getuid() cannot fail and touches no memory.
        let uid = unsafe { libc::getuid() }.to_string();
        // The graphical session first: hakod runs as a user service (no
        // session of its own) or from a terminal (maybe an SSH one).
        *session = loginctl(&["show-user", &uid, "-p", "Display", "--value"])
            .or_else(|| std::env::var("XDG_SESSION_ID").ok())
            .filter(|s| !s.is_empty());
    }
    let hint = loginctl(&[
        "show-session",
        session.as_deref()?,
        "-p",
        "LockedHint",
        "--value",
    ]);
    match hint.as_deref() {
        Some("yes") => Some(true),
        Some("no") => Some(false),
        _ => {
            *session = None;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::hotkey::{HotkeyScope, KEY_LEFTCTRL, KEY_SPACE};
    use hako_core::remap::EvdevKey;

    fn input(node: Node, name: &str, pid: u16, readable: bool) -> InputNode {
        InputNode {
            path: format!("/dev/input/event{node}").into(),
            node,
            name: name.into(),
            vendor_id: 0x1532,
            product_id: pid,
            readable,
        }
    }

    #[test]
    fn status_reports_source_devices_and_conflicts() {
        let dictate = HotkeyBinding::new(
            "dictate",
            HotkeyAction::VoiceTranscribe,
            &[KEY_LEFTCTRL, KEY_SPACE],
        );
        let active = BTreeSet::from([HotkeyAction::VoiceTranscribe]);
        let inputs = [
            input(3, "Razer Huntsman V2 Tenkeyless", 0x026b, true),
            input(4, "AT Translated Set 2 keyboard", 0x0001, false),
        ];
        let huntsman = RemapDevice {
            name: "Huntsman V2 TKL".into(),
            vendor_id: 0x1532,
            pids: vec![0x026b],
            own: 1,
            remapped: BTreeSet::from([97]), // Right Ctrl
        };
        let desktop = [Shortcut {
            name: "Toggle Overview (KDE)".into(),
            keys: BTreeSet::from([KEY_LEFTCTRL, KEY_SPACE]),
        }];
        let s = status(
            &dictate,
            &active,
            &inputs,
            std::slice::from_ref(&huntsman),
            &desktop,
        );
        assert_eq!(s.source, HotkeySource::Evdev);
        assert!(s.active);
        assert_eq!(s.devices, ["Huntsman V2 TKL"]);
        assert!(
            s.problems[0].contains("AT Translated Set 2 keyboard"),
            "{:?}",
            s.problems
        );
        assert_eq!(
            s.conflicts,
            [
                HotkeyConflict::Remap {
                    device: "Huntsman V2 TKL".into(),
                    vendor_id: 0x1532,
                    product_id: 0x026b,
                    key: EvdevKey(KEY_LEFTCTRL),
                },
                HotkeyConflict::Desktop {
                    shortcut: "Toggle Overview (KDE)".into()
                }
            ]
        );

        // "Both": the remap is not a conflict; exact Left Ctrl is not Right Ctrl.
        for b in [
            HotkeyBinding {
                with_remaps: true,
                ..dictate.clone()
            },
            HotkeyBinding {
                exact_side: true,
                ..dictate.clone()
            },
        ] {
            let s = status(&b, &active, &inputs, std::slice::from_ref(&huntsman), &[]);
            assert!(s.conflicts.is_empty(), "{b:?}");
        }

        // Only the unreadable keyboard is in scope: CLI only.
        let laptop = HotkeyBinding {
            scope: HotkeyScope::Device {
                vendor_id: 0x1532,
                product_id: 0x0001,
            },
            ..dictate.clone()
        };
        let s = status(&laptop, &BTreeSet::new(), &inputs, &[huntsman], &[]);
        assert_eq!(s.source, HotkeySource::Cli);
        assert!(!s.active && s.devices.is_empty() && s.conflicts.is_empty());
        let s = status(&laptop, &active, &[], &[], &[]);
        assert!(s.problems[0].contains("connected"));
    }

    fn paths(name: &str) -> ConfigPaths {
        let root = std::env::temp_dir().join(format!("hako-hotkeys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        }
    }

    #[test]
    fn patches_are_validated_saved_and_refused_while_the_file_is_unreadable() {
        let paths = paths("patch");
        std::fs::create_dir_all(&paths.root).unwrap();
        let (events, mut rx) = broadcast::channel(16);
        let svc = HotkeyService::start(paths.clone(), events);
        let mut b = svc.state().file.bindings[0].clone();
        b.mode = hako_core::hotkey::HotkeyMode::Hold;
        let state = svc.patch(&[HotkeyOp::Set { binding: b }]).unwrap();
        assert_eq!(
            state.file.bindings[0].mode,
            hako_core::hotkey::HotkeyMode::Hold
        );
        assert!(std::fs::read_to_string(paths.root.join("hotkeys.toml"))
            .unwrap()
            .contains("hold"));
        assert!(std::iter::from_fn(|| rx.try_recv().ok())
            .any(|e| matches!(e, AdapterEvent::HotkeysUpdated(s) if *s == state)));
        assert!(matches!(
            svc.patch(&[HotkeyOp::Remove { id: "nope".into() }]),
            Err(PatchError::Invalid(_))
        ));

        std::fs::write(paths.root.join("hotkeys.toml"), "version = [").unwrap();
        let broken = HotkeyService::start(paths.clone(), broadcast::channel(4).0);
        let state = broken.state();
        assert!(state.load_error.is_some() && state.file.bindings.is_empty());
        assert!(matches!(
            broken.patch(&[HotkeyOp::Remove {
                id: "cancel".into()
            }]),
            Err(PatchError::Io(_))
        ));
        assert_eq!(
            std::fs::read_to_string(paths.root.join("hotkeys.toml")).unwrap(),
            "version = ["
        );
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn capture_and_edges_flow_through_the_link() {
        let (events, mut rx) = broadcast::channel(32);
        let svc = HotkeyService::build(None, events, HotkeysFile::default(), None);
        let (tx, edges) = mpsc::channel();
        svc.on_trigger(Box::new(move |a, e| {
            let _ = tx.send((a, e));
        }));
        let hk = svc.link();
        assert!(!hk.want_listen(), "voice is off");
        svc.set_active(BTreeSet::from([HotkeyAction::VoiceTranscribe]));
        assert!(hk.want_listen());
        assert!(
            !hk.wants_grab(0x1532, &[0x026b], true),
            "Listen grabs nothing"
        );
        let info = NodeInfo {
            vendor_id: 0x1532,
            product_ids: vec![0x026b],
            ..NodeInfo::default()
        };
        let t0 = Instant::now();
        hk.sync_nodes(BTreeMap::from([(3, info)]), t0);

        // A capture swallows presses: nothing fires.
        svc.start_capture();
        hk.key(3, KEY_LEFTCTRL, 1, t0);
        hk.key(3, KEY_SPACE, 1, t0);
        hk.key(3, KEY_SPACE, 0, t0);
        hk.key(3, KEY_LEFTCTRL, 0, t0);
        let last = std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                AdapterEvent::HotkeyCapture(s) => Some(s),
                _ => None,
            })
            .last()
            .unwrap();
        assert!(last.done && !last.cancelled);
        assert_eq!(last.chord, [EvdevKey(KEY_LEFTCTRL), EvdevKey(KEY_SPACE)]);

        // Then the chord fires again.
        hk.key(3, KEY_LEFTCTRL, 1, t0);
        hk.key(3, KEY_SPACE, 1, t0);
        assert_eq!(
            edges.recv_timeout(Duration::from_secs(5)).unwrap(),
            (HotkeyAction::VoiceTranscribe, HotkeyEdge::Start)
        );
        assert_eq!(svc.held_modifiers(), [KEY_LEFTCTRL]);
        assert!(edges.try_recv().is_err());

        // A capture nobody finishes is cancelled.
        svc.start_capture();
        hk.tick(t0 + capture::CAPTURE_LIMIT + Duration::from_secs(1));
        assert!(svc.cancel_capture().cancelled);
    }
}
