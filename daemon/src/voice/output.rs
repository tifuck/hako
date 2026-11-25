//! Getting text into the focused window: hakod's own uinput keyboard (Type,
//! the paste key), the clipboard tools (`wl-copy` / `xclip`), an external
//! typing tool or a user command. One thread runs the jobs in order and
//! never while a physical modifier is held (typed letters would become
//! shortcuts).

use crate::hotkeys::VOICE_KEYBOARD_NAME;
use anyhow::{anyhow, bail, Context, Result};
use evdev::{uinput::VirtualDevice, AttributeSet, EventType, InputEvent, KeyCode};
use hako_core::voice::{OutputCheck, OutputMethod, OutputSettings, PasteKey, SubmitKey};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{debug, warn};
use xkbcommon::xkb;

/// While a modifier is held, the queue looks again this often.
const HELD_POLL: Duration = Duration::from_millis(20);
/// A `Command` output method gets this long.
const COMMAND_WAIT: Duration = Duration::from_secs(30);
/// Clipboard tools get this long.
const CLIPBOARD_WAIT: Duration = Duration::from_secs(2);
/// A new uinput device takes a moment before the compositor reads it.
const DEVICE_SETTLE: Duration = Duration::from_millis(200);
/// Highest key code the virtual keyboard declares (`KEY_MICMUTE`): keyboard
/// keys only, so udev keeps it a keyboard.
const MAX_KEY: u16 = 248;

const SHIFT: u8 = 1;
const ALTGR: u8 = 2;

/// How text is output; every job carries the settings it was made with.
#[derive(Debug, Clone, PartialEq)]
pub struct Ctx {
    pub method: OutputMethod,
    pub out: OutputSettings,
    /// XKB layout typing maps characters through (`de`, `de(nodeadkeys)`).
    pub layout: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    /// A recording started: make the virtual keyboard now, so it is known
    /// to the compositor by the time text comes.
    Warm,
    Text(Ctx, String),
    /// Delete characters typed before (Type only).
    Erase(Ctx, usize),
    Submit(Ctx, SubmitKey),
    /// `Clipboard`: copy, no key.
    Copy(String),
    /// Press these keys (evdev codes) in order, release them in reverse.
    Keys(Ctx, Vec<u16>),
}

/// Runs jobs in order on its own thread.
pub struct OutputQueue {
    tx: Option<mpsc::Sender<Job>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl OutputQueue {
    /// `held`: a physical modifier is down (jobs wait until it is not).
    pub fn start(
        mut exec: impl FnMut(Job) -> Result<()> + Send + 'static,
        held: impl Fn() -> bool + Send + 'static,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("hako-voice-output".into())
            .spawn({
                let stop = stop.clone();
                move || {
                    for job in rx {
                        if job != Job::Warm {
                            while held() && !stop.load(Ordering::Relaxed) {
                                std::thread::sleep(HELD_POLL);
                            }
                        }
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Err(e) = exec(job) {
                            on_error(format!("{e:#}"));
                        }
                    }
                }
            })
            .map_err(|e| warn!("voice: cannot start the output thread: {e}"))
            .ok();
        Self {
            tx: Some(tx),
            stop,
            thread,
        }
    }

    pub fn push(&self, job: Job) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(job);
        }
    }
}

impl Drop for OutputQueue {
    /// Jobs not started yet are dropped (daemon stop).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Characters to keys (XKB reverse lookup).

/// Character -> (evdev key code, `SHIFT` / `ALTGR` bits).
pub type CharMap = HashMap<char, (u16, u8)>;

/// `de(nodeadkeys)` -> (`de`, `nodeadkeys`).
fn split_layout(layout: &str) -> (&str, &str) {
    match layout.split_once('(') {
        Some((l, v)) => (l, v.trim_end_matches(')')),
        None => (layout, ""),
    }
}

/// Every character the layout types with at most Shift and AltGr (level 3),
/// the fewest modifiers first. Keypad keys are left out (NumLock decides
/// what they type), and so are dead keys (nothing comes out of them alone).
pub fn char_map(layout: &str) -> Result<CharMap> {
    let (layout, variant) = split_layout(layout);
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        layout,
        variant,
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| anyhow!("unknown keyboard layout {layout:?}"))?;
    let index = |name: &str| {
        let i = keymap.mod_get_index(name);
        (i != xkb::MOD_INVALID).then(|| 1u32 << i)
    };
    let shift = index(xkb::MOD_NAME_SHIFT).context("the layout has no Shift")?;
    let mut combos = vec![(0, 0), (shift, SHIFT)];
    if let Some(l3) = index(xkb::MOD_NAME_ISO_LEVEL3_SHIFT) {
        combos.extend([(l3, ALTGR), (shift | l3, SHIFT | ALTGR)]);
    }
    let mut map = CharMap::new();
    for (mask, mods) in combos {
        let mut state = xkb::State::new(&keymap);
        state.update_mask(mask, 0, 0, 0, 0, 0);
        for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let kc = xkb::Keycode::new(raw);
            let Some(code) = raw.checked_sub(8).and_then(|c| u16::try_from(c).ok()) else {
                continue;
            };
            if code == 0
                || code > MAX_KEY
                || keymap.key_get_name(kc).is_some_and(|n| n.starts_with("KP"))
            {
                continue;
            }
            let Some(c) = char::from_u32(state.key_get_utf32(kc)) else {
                continue;
            };
            if c != '\0' && !c.is_control() {
                map.entry(c).or_insert((code, mods));
            }
        }
    }
    map.insert('\n', (KeyCode::KEY_ENTER.0, 0));
    map.insert('\t', (KeyCode::KEY_TAB.0, 0));
    Ok(map)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Key(u16, u8),
    /// Characters the layout cannot type: pasted, then typing goes on.
    Paste(String),
}

pub fn plan(text: &str, map: &CharMap) -> Vec<Piece> {
    let mut out = Vec::new();
    for c in text.chars() {
        match (map.get(&c), out.last_mut()) {
            (Some(&(code, mods)), _) => out.push(Piece::Key(code, mods)),
            (None, Some(Piece::Paste(run))) => run.push(c),
            (None, _) => out.push(Piece::Paste(c.to_string())),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Layout detection.

/// `[('xkb', 'de+nodeadkeys'), ('xkb', 'us')]` -> `de(nodeadkeys)`.
fn parse_gnome_sources(s: &str) -> Option<String> {
    let first = s.split("('xkb', '").nth(1)?;
    let id = first.split('\'').next()?;
    Some(match id.split_once('+') {
        Some((l, v)) => format!("{l}({v})"),
        None => id.to_string(),
    })
}

/// `localectl status`: `X11 Layout: de,us` + `X11 Variant: nodeadkeys,`.
fn parse_localectl(s: &str) -> Option<String> {
    let field = |key: &str| {
        s.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .and_then(|v| v.trim().split(',').next().map(str::trim))
            .filter(|v| !v.is_empty())
    };
    let layout = field("X11 Layout:")?;
    Some(match field("X11 Variant:") {
        Some(v) => format!("{layout}({v})"),
        None => layout.to_string(),
    })
}

fn valid_layout(l: &str) -> bool {
    !l.is_empty()
        && l.len() <= 32
        && l.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-()".contains(&b))
}

fn stdout_of(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    let out = run_timeout(&mut cmd, None, true, CLIPBOARD_WAIT).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The layout the desktop types with: GNOME's current input source, else
/// the system's X11 layout, else `us`.
pub fn detect_layout() -> String {
    let gnome = || {
        ["mru-sources", "sources"].iter().find_map(|key| {
            stdout_of(
                "gsettings",
                &["get", "org.gnome.desktop.input-sources", key],
            )
            .and_then(|s| parse_gnome_sources(&s))
        })
    };
    gnome()
        .or_else(|| stdout_of("localectl", &["status"]).and_then(|s| parse_localectl(&s)))
        .filter(|l| valid_layout(l))
        .unwrap_or_else(|| "us".into())
}

// ---------------------------------------------------------------------------
// Processes.

/// Run `cmd` with `input` on stdin, killed after `wait`. `capture`: read
/// stdout / stderr (off for tools that fork a server holding them, such as
/// `wl-copy`).
fn run_timeout(
    cmd: &mut Command,
    input: Option<&[u8]>,
    capture: bool,
    wait: Duration,
) -> Result<Output> {
    let pipe = || {
        if capture {
            Stdio::piped()
        } else {
            Stdio::null()
        }
    };
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(pipe())
        .stderr(pipe())
        .spawn()
        .with_context(|| format!("cannot run {:?}", cmd.get_program()))?;
    let pid = child.id() as libc::pid_t;
    let stdin = child.stdin.take();
    let input = input.map(<[u8]>::to_vec);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            let _ = stdin.write_all(&input);
        }
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(wait) {
        Ok(out) => Ok(out?),
        Err(_) => {
            // SAFETY: the child is not reaped before `wait_with_output`
            // returns, so `pid` is still ours.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            bail!("{:?} did not finish within {wait:?}", cmd.get_program())
        }
    }
}

/// Run `argv` (no shell) and wait up to `wait`: its stdout.
pub fn run_tool(argv: &[String], wait: Duration) -> Result<String> {
    let (program, args) = argv.split_first().context("nothing to run")?;
    let out = run_timeout(Command::new(program).args(args), None, true, wait)?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program)).filter(|p| executable(p));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| executable(p))
}

fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Clipboard.

#[derive(Debug, Clone, Copy, PartialEq)]
enum Clip {
    Wayland,
    X11,
}

/// The session's clipboard tools, or why there are none.
fn clipboard() -> Result<Clip, String> {
    if env_set("WAYLAND_DISPLAY") {
        if which("wl-copy").is_some() && which("wl-paste").is_some() {
            return Ok(Clip::Wayland);
        }
        return Err("wl-clipboard (wl-copy, wl-paste) is not installed".into());
    }
    if env_set("DISPLAY") {
        if which("xclip").is_some() {
            return Ok(Clip::X11);
        }
        return Err("xclip is not installed".into());
    }
    Err("hakod has no WAYLAND_DISPLAY or DISPLAY (run `dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY` at login)".into())
}

impl Clip {
    /// The selection's text, if it has any.
    fn get(self, primary: bool) -> Option<Vec<u8>> {
        let mut cmd = match self {
            Self::Wayland => {
                let mut c = Command::new("wl-paste");
                c.args(["--no-newline", "--type", "text"]);
                if primary {
                    c.arg("--primary");
                }
                c
            }
            Self::X11 => {
                let mut c = Command::new("xclip");
                c.args([
                    "-o",
                    "-selection",
                    if primary { "primary" } else { "clipboard" },
                ]);
                c
            }
        };
        let out = run_timeout(&mut cmd, None, true, CLIPBOARD_WAIT).ok()?;
        out.status.success().then_some(out.stdout)
    }

    fn set(self, primary: bool, text: &[u8]) -> Result<()> {
        let mut cmd = match self {
            Self::Wayland => {
                let mut c = Command::new("wl-copy");
                if primary {
                    c.arg("--primary");
                }
                c
            }
            Self::X11 => {
                let mut c = Command::new("xclip");
                c.args([
                    "-i",
                    "-selection",
                    if primary { "primary" } else { "clipboard" },
                ]);
                c
            }
        };
        let out = run_timeout(&mut cmd, Some(text), false, CLIPBOARD_WAIT)?;
        if !out.status.success() {
            bail!("{:?} failed ({})", cmd.get_program(), out.status);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The virtual keyboard and the jobs.

struct Keyboard {
    dev: VirtualDevice,
}

impl Keyboard {
    fn new() -> Result<Self> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=MAX_KEY {
            keys.insert(KeyCode::new(code));
        }
        let dev = VirtualDevice::builder()
            .context("cannot open /dev/uinput")?
            .name(VOICE_KEYBOARD_NAME)
            .with_keys(&keys)?
            .build()
            .context("cannot create the voice keyboard")?;
        Ok(Self { dev })
    }

    fn key(&mut self, code: u16, value: i32, delay: Duration) -> Result<()> {
        self.dev
            .emit(&[InputEvent::new(EventType::KEY.0, code, value)])
            .context("uinput write")?;
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        Ok(())
    }

    /// Press `code` with modifier keys held.
    fn chord(&mut self, mods: &[u16], code: u16, delay: Duration) -> Result<()> {
        for &m in mods {
            self.key(m, 1, delay)?;
        }
        self.key(code, 1, delay)?;
        self.key(code, 0, delay)?;
        for &m in mods.iter().rev() {
            self.key(m, 0, delay)?;
        }
        Ok(())
    }
}

fn mod_keys(mods: u8) -> Vec<u16> {
    let mut keys = Vec::new();
    if mods & SHIFT != 0 {
        keys.push(KeyCode::KEY_LEFTSHIFT.0);
    }
    if mods & ALTGR != 0 {
        keys.push(KeyCode::KEY_RIGHTALT.0);
    }
    keys
}

/// The real job runner: owns the virtual keyboard and the layout's map.
#[derive(Default)]
pub struct Outputter {
    keyboard: Option<Keyboard>,
    map: Option<(String, CharMap)>,
}

impl Outputter {
    pub fn run(&mut self, job: Job) -> Result<()> {
        match job {
            Job::Warm => {
                self.keyboard()?;
                Ok(())
            }
            Job::Text(ctx, text) => self.text(&ctx, &text),
            Job::Erase(ctx, n) => {
                let delay = key_delay(&ctx);
                let kb = self.keyboard()?;
                for _ in 0..n {
                    kb.chord(&[], KeyCode::KEY_BACKSPACE.0, delay)?;
                }
                Ok(())
            }
            Job::Submit(ctx, key) => {
                let delay = key_delay(&ctx);
                let mods: &[u16] = match key {
                    SubmitKey::Enter => &[],
                    SubmitKey::CtrlEnter => &[KeyCode::KEY_LEFTCTRL.0],
                    SubmitKey::ShiftEnter => &[KeyCode::KEY_LEFTSHIFT.0],
                };
                self.keyboard()?.chord(mods, KeyCode::KEY_ENTER.0, delay)
            }
            Job::Copy(text) => {
                let clip = clipboard().map_err(anyhow::Error::msg)?;
                clip.set(false, text.as_bytes())
            }
            Job::Keys(ctx, keys) => {
                if keys.is_empty() || keys.iter().any(|&k| k == 0 || k > MAX_KEY) {
                    bail!("only keyboard keys can be pressed");
                }
                let delay = key_delay(&ctx);
                let kb = self.keyboard()?;
                for &k in &keys {
                    kb.key(k, 1, delay)?;
                }
                for &k in keys.iter().rev() {
                    kb.key(k, 0, delay)?;
                }
                Ok(())
            }
        }
    }

    fn keyboard(&mut self) -> Result<&mut Keyboard> {
        if self.keyboard.is_none() {
            self.keyboard = Some(Keyboard::new()?);
            debug!("voice: virtual keyboard created");
            std::thread::sleep(DEVICE_SETTLE);
        }
        Ok(self.keyboard.as_mut().expect("just made"))
    }

    fn map(&mut self, layout: &str) -> Result<&CharMap> {
        if self.map.as_ref().is_none_or(|(l, _)| l != layout) {
            self.map = Some((layout.to_string(), char_map(layout)?));
        }
        Ok(&self.map.as_ref().expect("just made").1)
    }

    fn text(&mut self, ctx: &Ctx, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        match ctx.method {
            OutputMethod::Type => self.type_text(ctx, text),
            OutputMethod::Paste => self.paste(ctx, text),
            OutputMethod::Clipboard => clipboard()
                .map_err(anyhow::Error::msg)?
                .set(false, text.as_bytes()),
            OutputMethod::External => external(ctx, text),
            OutputMethod::Command => {
                let typed = command(ctx, text).unwrap_or_else(|e| {
                    warn!("voice: output command: {e:#}");
                    text.to_string()
                });
                self.type_text(ctx, &typed)
            }
        }
    }

    fn type_text(&mut self, ctx: &Ctx, text: &str) -> Result<()> {
        let pieces = plan(text, self.map(&ctx.layout)?);
        let delay = key_delay(ctx);
        for piece in pieces {
            match piece {
                Piece::Key(code, mods) => self.keyboard()?.chord(&mod_keys(mods), code, delay)?,
                Piece::Paste(run) => self.paste(ctx, &run)?,
            }
        }
        Ok(())
    }

    /// Clipboard and primary selection = `text`, the paste key, then both
    /// come back after `restore_ms`.
    fn paste(&mut self, ctx: &Ctx, text: &str) -> Result<()> {
        let clip = clipboard().map_err(|e| anyhow!("cannot paste: {e}"))?;
        // shortcut: only text selections come back (an image on the
        // clipboard is replaced); upgrade with per-MIME-type copies if asked.
        let old = (clip.get(false), clip.get(true));
        clip.set(false, text.as_bytes())?;
        clip.set(true, text.as_bytes())?;
        let delay = key_delay(ctx);
        let (mods, key) = match ctx.out.paste_key {
            PasteKey::ShiftInsert => (vec![KeyCode::KEY_LEFTSHIFT.0], KeyCode::KEY_INSERT.0),
            PasteKey::CtrlV => (vec![KeyCode::KEY_LEFTCTRL.0], self.letter('v', ctx)),
            PasteKey::CtrlShiftV => (
                vec![KeyCode::KEY_LEFTCTRL.0, KeyCode::KEY_LEFTSHIFT.0],
                self.letter('v', ctx),
            ),
        };
        self.keyboard()?.chord(&mods, key, delay)?;
        std::thread::sleep(Duration::from_millis(ctx.out.restore_ms.into()));
        for (primary, old) in [(false, old.0), (true, old.1)] {
            if let Some(old) = old {
                if let Err(e) = clip.set(primary, &old) {
                    warn!("voice: restoring the clipboard: {e:#}");
                }
            }
        }
        Ok(())
    }

    /// The key that types `c` in the layout (`Ctrl+V` on Dvorak is not `KEY_V`).
    fn letter(&mut self, c: char, ctx: &Ctx) -> u16 {
        self.map(&ctx.layout)
            .ok()
            .and_then(|m| m.get(&c))
            .map_or(KeyCode::KEY_V.0, |&(code, _)| code)
    }
}

fn key_delay(ctx: &Ctx) -> Duration {
    Duration::from_millis(ctx.out.key_delay_ms.into())
}

/// `wtype` / `xdotool` / `ydotool` with the text as an argument, `dotool`
/// with it on stdin; never through a shell.
fn external_command(tool: &str, text: &str, delay_ms: u32) -> (Command, Option<Vec<u8>>) {
    let mut cmd = Command::new(tool);
    let name = Path::new(tool)
        .file_name()
        .map_or(tool.into(), |n| n.to_string_lossy());
    let mut input = None;
    match name.as_ref() {
        "wtype" => {
            cmd.arg("--").arg(text);
        }
        "xdotool" => {
            cmd.args(["type", "--delay"])
                .arg(delay_ms.to_string())
                .arg("--")
                .arg(text);
        }
        "ydotool" => {
            cmd.args(["type", "--"]).arg(text);
        }
        "dotool" => {
            let mut script = String::new();
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    script.push_str("key enter\n");
                }
                if !line.is_empty() {
                    script.push_str(&format!("type {line}\n"));
                }
            }
            input = Some(script.into_bytes());
        }
        _ => {
            cmd.arg(text);
        }
    }
    (cmd, input)
}

fn external(ctx: &Ctx, text: &str) -> Result<()> {
    let tool = ctx
        .out
        .external_tool
        .as_deref()
        .context("no external tool chosen")?;
    let (mut cmd, input) = external_command(tool, text, ctx.out.key_delay_ms);
    let wait = Duration::from_secs(10) + Duration::from_millis(text.len() as u64 * 20);
    let out = run_timeout(&mut cmd, input.as_deref(), true, wait)?;
    if !out.status.success() {
        bail!(
            "{tool} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `output.command` with `text` on stdin: its stdout.
fn command(ctx: &Ctx, text: &str) -> Result<String> {
    let path = ctx.out.command.as_deref().context("no command chosen")?;
    let out = run_timeout(
        &mut Command::new(path),
        Some(text.as_bytes()),
        true,
        COMMAND_WAIT,
    )?;
    if !out.status.success() {
        bail!("{path} failed ({})", out.status);
    }
    let s = String::from_utf8(out.stdout).context("its output is not UTF-8")?;
    Ok(s.trim_end_matches('\n').to_string())
}

// ---------------------------------------------------------------------------
// What works in this session.

fn uinput_writable() -> Result<(), String> {
    // SAFETY: a NUL-terminated path; access() only reads it.
    let ok = unsafe { libc::access(c"/dev/uinput".as_ptr(), libc::W_OK) } == 0;
    ok.then_some(())
        .ok_or_else(|| "/dev/uinput is not writable (install hako's udev rules)".into())
}

fn check(method: OutputMethod, out: &OutputSettings) -> Result<(), String> {
    match method {
        OutputMethod::Type => uinput_writable(),
        OutputMethod::Paste => uinput_writable().and_then(|()| clipboard().map(|_| ())),
        OutputMethod::Clipboard => clipboard().map(|_| ()),
        OutputMethod::External => {
            let tool = out
                .external_tool
                .as_deref()
                .ok_or("pick an external tool")?;
            which(tool).ok_or(format!("{tool} is not installed"))?;
            match tool {
                "wtype" if !env_set("WAYLAND_DISPLAY") => {
                    Err("wtype needs WAYLAND_DISPLAY in hakod's environment".into())
                }
                "xdotool" if !env_set("DISPLAY") => {
                    Err("xdotool needs DISPLAY in hakod's environment".into())
                }
                _ => Ok(()),
            }
        }
        OutputMethod::Command => {
            let path = out.command.as_deref().ok_or("pick a command")?;
            executable(Path::new(path))
                .then_some(())
                .ok_or(format!("{path} is not an executable file"))
        }
    }
}

/// Every method, whether it works here and why not.
pub fn checks(out: &OutputSettings) -> Vec<OutputCheck> {
    OutputMethod::ALL
        .iter()
        .map(|&method| {
            let r = check(method, out);
            OutputCheck {
                method,
                ok: r.is_ok(),
                reason: r.err(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    #[test]
    fn us_layout_maps_characters_to_keys() {
        let map = char_map("us").unwrap();
        assert_eq!(map[&'a'], (KeyCode::KEY_A.0, 0));
        assert_eq!(map[&'A'], (KeyCode::KEY_A.0, SHIFT));
        assert_eq!(map[&'!'], (KeyCode::KEY_1.0, SHIFT));
        assert_eq!(map[&' '], (KeyCode::KEY_SPACE.0, 0));
        // The main row, not the keypad.
        assert_eq!(map[&'+'], (KeyCode::KEY_EQUAL.0, SHIFT));
        assert_eq!(map[&'\n'], (KeyCode::KEY_ENTER.0, 0));
        assert!(!map.contains_key(&'é'));
        assert_eq!(
            plan("Hi 😀🙂!", &map),
            [
                Piece::Key(KeyCode::KEY_H.0, SHIFT),
                Piece::Key(KeyCode::KEY_I.0, 0),
                Piece::Key(KeyCode::KEY_SPACE.0, 0),
                Piece::Paste("😀🙂".into()),
                Piece::Key(KeyCode::KEY_1.0, SHIFT),
            ]
        );
    }

    #[test]
    fn altgr_layouts_use_level_three() {
        let map = char_map("de(nodeadkeys)").unwrap();
        assert_eq!(map[&'z'], (KeyCode::KEY_Y.0, 0));
        assert_eq!(map[&'@'], (KeyCode::KEY_Q.0, ALTGR));
        assert_eq!(map[&'ä'], (KeyCode::KEY_APOSTROPHE.0, 0));
        assert!(char_map("no-such-layout").is_err());
    }

    #[test]
    fn layouts_are_parsed_from_gnome_and_localectl() {
        assert_eq!(
            parse_gnome_sources("[('xkb', 'de+nodeadkeys'), ('xkb', 'us')]\n").as_deref(),
            Some("de(nodeadkeys)")
        );
        assert_eq!(
            parse_gnome_sources("[('ibus', 'anthy'), ('xkb', 'fr')]").as_deref(),
            Some("fr")
        );
        assert_eq!(parse_gnome_sources("@a(ss) []"), None);
        let status = "   System Locale: LANG=de_DE.UTF-8\n       X11 Layout: de,us\n      X11 Variant: nodeadkeys,\n";
        assert_eq!(parse_localectl(status).as_deref(), Some("de(nodeadkeys)"));
        assert_eq!(parse_localectl("X11 Layout: us\n").as_deref(), Some("us"));
        assert_eq!(parse_localectl("VC Keymap: n/a\n"), None);
        assert!(valid_layout("de(nodeadkeys)") && !valid_layout("de;rm"));
    }

    #[test]
    fn external_tools_get_the_text_as_an_argument() {
        let args = |tool, text| {
            let (cmd, input) = external_command(tool, text, 2);
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            (args, input.map(|i| String::from_utf8(i).unwrap()))
        };
        assert_eq!(
            args("wtype", "-rf; x"),
            (vec!["--".into(), "-rf; x".into()], None)
        );
        assert_eq!(
            args("xdotool", "hi").0,
            ["type", "--delay", "2", "--", "hi"]
        );
        assert_eq!(
            args("dotool", "a\nb"),
            (vec![], Some("type a\nkey enter\ntype b\n".into()))
        );
    }

    #[test]
    fn command_output_and_timeout() {
        let ctx = |cmd: &str| Ctx {
            method: OutputMethod::Command,
            out: OutputSettings {
                command: Some(cmd.into()),
                ..Default::default()
            },
            layout: "us".into(),
        };
        assert_eq!(command(&ctx("/usr/bin/rev"), "abc").unwrap(), "cba");
        assert!(command(&ctx("/usr/bin/false"), "abc").is_err());
        let start = Instant::now();
        assert!(run_timeout(
            Command::new("/usr/bin/sleep").arg("5"),
            None,
            true,
            Duration::from_millis(100)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn queue_waits_for_held_modifiers_and_keeps_order() {
        let done = Arc::new(Mutex::new(Vec::new()));
        let held = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();
        let q = OutputQueue::start(
            {
                let done = done.clone();
                move |job| {
                    done.lock().unwrap().push(job);
                    let _ = tx.send(());
                    Ok(())
                }
            },
            {
                let held = held.clone();
                move || held.load(Ordering::Relaxed)
            },
            |_| {},
        );
        let ctx = Ctx {
            method: OutputMethod::Type,
            out: OutputSettings::default(),
            layout: "us".into(),
        };
        q.push(Job::Warm);
        q.push(Job::Text(ctx.clone(), "one".into()));
        q.push(Job::Text(ctx.clone(), "two".into()));
        // Warm does not wait; text does while Ctrl is held.
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
        assert_eq!(*done.lock().unwrap(), [Job::Warm]);
        held.store(false, Ordering::Relaxed);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            *done.lock().unwrap(),
            [
                Job::Warm,
                Job::Text(ctx.clone(), "one".into()),
                Job::Text(ctx, "two".into())
            ]
        );
    }

    #[test]
    fn checks_cover_every_method() {
        let c = checks(&OutputSettings::default());
        assert_eq!(c.len(), OutputMethod::ALL.len());
        let ext = c
            .iter()
            .find(|c| c.method == OutputMethod::External)
            .unwrap();
        assert!(!ext.ok && ext.reason.is_some());
    }

    /// Types into whatever has focus: run by hand with an empty editor focused.
    #[test]
    #[ignore = "types through a real uinput keyboard"]
    fn live_uinput_typing() {
        let mut o = Outputter::default();
        o.run(Job::Warm).unwrap();
        let ctx = Ctx {
            method: OutputMethod::Type,
            out: OutputSettings::default(),
            layout: detect_layout(),
        };
        o.run(Job::Text(ctx, "hako voice test".into())).unwrap();
    }
}
