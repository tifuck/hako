//! Jarvis: spoken commands. The voice service owns the ear (the always-on
//! capture, its own `hako-stt` wake-word worker and the take after a wake,
//! [`Ear`]) and transcribes each take with the user's dictation model; the
//! transcript comes here, to the runner thread: wake word verified and
//! stripped, intent matched ([`intent`]), action run ([`actions`]), and the
//! outcome sent back. Design: `docs/superpowers/specs/2026-11-25-voice-stt-design.md`.

pub mod actions;
pub mod apps;
pub mod intent;

use actions::{Actions, Env};
use hako_core::ipc::Method;
use hako_core::jarvis::{duration_words, Action};
use hako_core::voice::{CommandOutcome, VoiceFile, SAMPLE_RATE};
use serde_json::Value;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// The hub, as Jarvis reaches it.

/// A request the hub runs for Jarvis.
#[derive(Debug, Clone, PartialEq)]
pub enum HubReq {
    /// Through `DaemonHub::handle`, like a UI request.
    Ipc(Method, Value),
    /// The ChatMix dial (the mixer's while it owns ChatMix).
    ChatMix { game: f32, chat: f32 },
}

pub type HubCall = (HubReq, mpsc::Sender<Result<Value, String>>);
pub type HubTx = tokio::sync::mpsc::UnboundedSender<HubCall>;
pub type HubRx = tokio::sync::mpsc::UnboundedReceiver<HubCall>;

/// Longest a hub request may take (a profile apply writes every device).
const HUB_WAIT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// The ear: what audio makes up a command.

/// Audio kept from before a wake word (the word itself is in it).
pub const RING: usize = SAMPLE_RATE as usize * 2;
/// Longest command after a wake word.
const WAKE_MAX: Duration = Duration::from_secs(8);
/// A pause this soon after the wake word is the user waiting for the ping...
const EARLY_PAUSE: Duration = Duration::from_millis(900);
/// ...who then has until this long after the wake to start speaking.
const RESUME_WAIT: Duration = Duration::from_secs(4);
/// Longest push-to-command take.
const PUSH_MAX: Duration = Duration::from_secs(30);
/// Longest wait for a spoken "yes".
const CONFIRM_MAX: Duration = Duration::from_secs(6);
/// Audio right after a ping is its bleed into the mic: dropped.
const BLEED: usize = SAMPLE_RATE as usize * 150 / 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listen {
    /// After the wake word: ends at a pause.
    Wake,
    /// The push-to-command hotkey: ends when it is released.
    Push,
    /// A "yes" for a command that asks first: ends at a pause.
    Confirm,
}

struct Take {
    kind: Listen,
    started: Instant,
    pcm: Vec<f32>,
    bleed: usize,
    /// Speech started after the take began.
    spoke: bool,
    /// Wake: the pause right after the word; waiting for speech.
    waiting: bool,
}

/// The ring of recent audio and the take in progress. Time is passed in.
#[derive(Default)]
pub struct Ear {
    ring: VecDeque<f32>,
    take: Option<Take>,
}

pub type Done = (Listen, Vec<f32>);

impl Ear {
    pub fn pcm(&mut self, pcm: &[f32]) {
        match &mut self.take {
            Some(t) => {
                let skip = t.bleed.min(pcm.len());
                t.bleed -= skip;
                t.pcm.extend_from_slice(&pcm[skip..]);
            }
            None => {
                self.ring.extend(pcm);
                let over = self.ring.len().saturating_sub(RING);
                self.ring.drain(..over);
            }
        }
    }

    /// A take starts; after a wake it begins with the ring (the word).
    pub fn begin(&mut self, kind: Listen, now: Instant) {
        let pcm = if kind == Listen::Wake {
            self.ring.drain(..).collect()
        } else {
            self.ring.clear();
            Vec::new()
        };
        self.take = Some(Take {
            kind,
            started: now,
            pcm,
            bleed: if kind == Listen::Wake { 0 } else { BLEED },
            spoke: false,
            waiting: false,
        });
    }

    pub fn taking(&self) -> Option<Listen> {
        self.take.as_ref().map(|t| t.kind)
    }

    fn end(&mut self) -> Option<Done> {
        self.take.take().map(|t| (t.kind, t.pcm))
    }

    /// Speech started (`true`) or a pause of 700 ms (`false`).
    pub fn speech(&mut self, active: bool, now: Instant) -> Option<Done> {
        let t = self.take.as_mut()?;
        match t.kind {
            Listen::Push => None,
            _ if active => {
                t.spoke = true;
                t.waiting = false;
                None
            }
            Listen::Confirm if !t.spoke => None,
            Listen::Wake if !t.spoke && now < t.started + EARLY_PAUSE => {
                t.waiting = true;
                None
            }
            _ => self.end(),
        }
    }

    /// The push-to-command key is up.
    pub fn release(&mut self) -> Option<Done> {
        (self.taking() == Some(Listen::Push))
            .then(|| self.end())
            .flatten()
    }

    pub fn cancel(&mut self) -> Option<Listen> {
        self.take.take().map(|t| t.kind)
    }

    /// A take that ran out of time ends; the next deadline.
    pub fn tick(&mut self, now: Instant) -> (Option<Done>, Option<Instant>) {
        let Some(t) = &self.take else {
            return (None, None);
        };
        let cap = t.started
            + match t.kind {
                Listen::Wake => WAKE_MAX,
                Listen::Push => PUSH_MAX,
                Listen::Confirm => CONFIRM_MAX,
            };
        let resume = t.waiting.then_some(t.started + RESUME_WAIT);
        let at = resume.map_or(cap, |r| r.min(cap));
        if now >= at {
            (self.end(), None)
        } else {
            (None, Some(at))
        }
    }
}

// ---------------------------------------------------------------------------
// Wake word verification.

/// Lowercase words with their byte spans in `text`.
fn words(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let part = c.is_alphanumeric() || c == '\'';
        match (part, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let w: String = text[s..i]
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .flat_map(char::to_lowercase)
                    .collect();
                if !w.is_empty() {
                    out.push((w, s, i));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// American Soundex: words that sound alike share it ("jarvis", "jervis").
pub fn soundex(word: &str) -> String {
    let code = |c: char| match c {
        'b' | 'f' | 'p' | 'v' => '1',
        'c' | 'g' | 'j' | 'k' | 'q' | 's' | 'x' | 'z' => '2',
        'd' | 't' => '3',
        'l' => '4',
        'm' | 'n' => '5',
        'r' => '6',
        'h' | 'w' => '-',
        _ => '0',
    };
    let mut chars = word
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase());
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::from(first.to_ascii_uppercase());
    let mut last = code(first);
    for c in chars {
        let d = code(c);
        if d == '-' {
            continue;
        }
        if d != '0' && d != last {
            out.push(d);
        }
        last = d;
        if out.len() == 4 {
            break;
        }
    }
    while out.len() < 4 {
        out.push('0');
    }
    out
}

/// `heard` (words squashed together) is the wake word: the same, one or two
/// letters off, or the same sound.
fn sounds_like(heard: &str, wake: &str) -> bool {
    heard == wake
        || intent::levenshtein(heard, wake) <= (wake.len() / 4).max(1)
        || (soundex(heard) == soundex(wake) && heard.len().abs_diff(wake.len()) <= 2)
}

/// The command after the wake word, when the transcript starts with it
/// (within its first three words: "hey jarvis", "ok, jarvis"); `None` is a
/// false wake. The wake word and the punctuation after it are stripped.
pub fn verify(text: &str, wake_word: &str) -> Option<String> {
    let wake: String = intent::squash(wake_word);
    let wake_words = wake_word.split_whitespace().count().max(1);
    let w = words(text);
    for start in 0..w.len().min(3) {
        // The model may split or join the word ("jar vis").
        for len in 1..=(wake_words + 1).min(w.len() - start) {
            let heard: String = w[start..start + len]
                .iter()
                .map(|(s, ..)| s.as_str())
                .collect();
            if sounds_like(&heard, &wake) {
                let end = w[start + len - 1].2;
                let rest = text[end..]
                    .trim_start_matches(|c: char| c.is_whitespace() || ",.:;!?-".contains(c));
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

/// A spoken confirmation.
pub fn is_yes(text: &str) -> bool {
    let w: Vec<String> = words(text).into_iter().map(|(s, ..)| s).collect();
    let no = w.iter().any(|s| {
        matches!(
            s.as_str(),
            "no" | "nope" | "dont" | "don't" | "cancel" | "stop"
        )
    });
    let yes = w.iter().any(|s| {
        matches!(
            s.as_str(),
            "yes" | "yeah" | "yep" | "yup" | "confirm" | "confirmed" | "sure"
        )
    }) || w.windows(2).any(|p| p[0] == "do" && p[1] == "it");
    yes && !no
}

// ---------------------------------------------------------------------------
// The runner.

/// To the runner.
#[derive(Debug)]
pub enum RunnerIn {
    /// A take's transcript.
    Heard {
        kind: Listen,
        text: String,
        file: Box<VoiceFile>,
    },
    /// A take could not be transcribed.
    Failed { kind: Listen, message: String },
    /// The ear is open for wake words (the runner then watches mic mute).
    Listening(bool),
}

/// To the voice service.
#[derive(Debug, Clone, PartialEq)]
pub enum JarvisOut {
    /// A command finished (also failures and "didn't catch that").
    Done(CommandOutcome),
    /// Listen once more for a "yes" (no wake word needed).
    Confirm,
    Type(String),
    /// Press this chord (evdev codes, in order) on the voice keyboard.
    Keys(Vec<u16>),
    /// Start dictating, as if the hotkey was pressed.
    Dictate,
    /// A timer ended: a ping.
    Ping,
}

/// How long a confirmation is waited for (the take plus transcription).
const CONFIRM_WAIT: Duration = Duration::from_secs(30);
/// Mic mute is read from the devices this often while the ear is open.
const MIC_POLL: Duration = Duration::from_secs(3);

struct Timer {
    at: Instant,
    seconds: u32,
    label: Option<String>,
}

struct Pending {
    action: Action,
    heard: String,
    file: Box<VoiceFile>,
    until: Instant,
}

/// The command logic, side effects through `env`.
pub struct Brain {
    env: Box<dyn Env>,
    actions: Actions,
    timers: Vec<Timer>,
    pending: Option<Pending>,
    false_wakes: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The `action` tag (what is logged: never the slot or text).
fn action_name(a: &Action) -> String {
    serde_json::to_value(a)
        .ok()
        .and_then(|v| v["action"].as_str().map(str::to_string))
        .unwrap_or_default()
}

impl Brain {
    pub fn new(env: Box<dyn Env>) -> Self {
        Self {
            env,
            actions: Actions::default(),
            timers: Vec::new(),
            pending: None,
            false_wakes: 0,
        }
    }

    fn done(&mut self, heard: &str, action: Option<&Action>, result: Result<String, String>) {
        let ok = result.is_ok();
        let message = match result {
            Ok(m) | Err(m) => m,
        };
        if !ok {
            let body = if heard.is_empty() {
                message.clone()
            } else {
                format!("{message}\n\u{201c}{heard}\u{201d}")
            };
            self.env.notify("Jarvis", &body);
        }
        self.env.voice(JarvisOut::Done(CommandOutcome {
            at: unix_now(),
            heard: heard.to_string(),
            action: action.map(Action::describe),
            ok,
            message: Some(message),
        }));
    }

    pub fn heard(&mut self, kind: Listen, text: &str, file: &VoiceFile, now: Instant) {
        match kind {
            Listen::Confirm => {
                let Some(p) = self.pending.take() else {
                    return;
                };
                if is_yes(text) {
                    info!("jarvis: confirmed {}", action_name(&p.action));
                    let r = self.actions.run(&mut *self.env, &p.action, &p.file);
                    self.done(&p.heard, Some(&p.action), r);
                } else {
                    self.done(&p.heard, Some(&p.action), Err("Not confirmed".into()));
                }
            }
            Listen::Wake => match verify(text, &file.jarvis.wake_word) {
                Some(rest) => self.command(&rest, file, now),
                None => {
                    self.false_wakes += 1;
                    debug!("jarvis: false wake dropped ({} so far)", self.false_wakes);
                }
            },
            Listen::Push => self.command(text.trim(), file, now),
        }
    }

    pub fn failed(&mut self, kind: Listen, message: &str) {
        warn!("jarvis: transcription failed: {message}");
        if kind == Listen::Confirm {
            if let Some(p) = self.pending.take() {
                self.done(&p.heard, Some(&p.action), Err("Not confirmed".into()));
            }
            return;
        }
        self.done("", None, Err(format!("Didn't catch that ({message})")));
    }

    fn context(&mut self) -> (Vec<String>, Vec<String>) {
        let profiles = self
            .env
            .hub(HubReq::Ipc(Method::ListProfiles, Value::Null))
            .ok()
            .and_then(|v| serde_json::from_value(v["profiles"].clone()).ok())
            .unwrap_or_default();
        let strips = self
            .env
            .hub(HubReq::Ipc(Method::GetMixer, Value::Null))
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .map(|s| actions::strip_labels(&s))
            .unwrap_or_default();
        (profiles, strips)
    }

    fn command(&mut self, heard: &str, file: &VoiceFile, now: Instant) {
        if heard.is_empty() {
            self.done(heard, None, Err("Didn't catch that".into()));
            return;
        }
        let (profiles, strips) = self.context();
        let ctx = intent::Context {
            profiles: &profiles,
            strips: &strips,
            commands: &file.jarvis.commands,
        };
        let action = intent::parse(heard, &ctx);
        let Some(action) = action else {
            self.done(heard, None, Err("Didn't catch that".into()));
            return;
        };
        info!("jarvis: {}", action_name(&action));
        if let Action::Timer { seconds, label } = &action {
            self.timers.push(Timer {
                at: now + Duration::from_secs((*seconds).into()),
                seconds: *seconds,
                label: label.clone(),
            });
            let what = format!("Timer {}", duration_words(*seconds));
            self.done(heard, Some(&action), Ok(what));
            return;
        }
        if let Action::User { index, .. } = &action {
            if file.jarvis.commands.get(*index).is_some_and(|c| c.confirm) {
                let say = file.jarvis.commands[*index]
                    .say
                    .first()
                    .cloned()
                    .unwrap_or_default();
                self.env.notify(
                    "Jarvis",
                    &format!("Say \u{201c}yes\u{201d} to run \u{201c}{say}\u{201d}"),
                );
                self.pending = Some(Pending {
                    action,
                    heard: heard.to_string(),
                    file: Box::new(file.clone()),
                    until: now + CONFIRM_WAIT,
                });
                self.env.voice(JarvisOut::Confirm);
                return;
            }
        }
        let r = self.actions.run(&mut *self.env, &action, file);
        if let Err(e) = &r {
            // The message may quote what was said: not at info.
            debug!("jarvis: {} failed: {e}", action_name(&action));
        }
        self.done(heard, Some(&action), r);
    }

    /// Timers that ended, a confirmation not given; the next deadline.
    pub fn tick(&mut self, now: Instant) -> Option<Instant> {
        while let Some(i) = self.timers.iter().position(|t| now >= t.at) {
            let t = self.timers.remove(i);
            self.env.voice(JarvisOut::Ping);
            let what = t.label.map_or_else(
                || duration_words(t.seconds),
                |l| format!("{l} ({})", duration_words(t.seconds)),
            );
            self.env.notify("Timer done", &what);
        }
        if self.pending.as_ref().is_some_and(|p| now >= p.until) {
            let p = self.pending.take().expect("checked");
            self.done(&p.heard, Some(&p.action), Err("Not confirmed".into()));
        }
        self.timers
            .iter()
            .map(|t| t.at)
            .chain(self.pending.as_ref().map(|p| p.until))
            .min()
    }

    fn mic_muted(&mut self) -> bool {
        actions::devices(&mut *self.env).is_ok_and(|d| actions::mic_muted(&d))
    }
}

/// The runner thread; dropping it ends the thread once it is idle.
pub struct Runner {
    tx: mpsc::Sender<RunnerIn>,
}

impl Runner {
    /// `mic_muted` is kept current while the ear listens.
    pub fn start(env: Box<dyn Env>, mic_muted: Arc<AtomicBool>) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("hako-jarvis".into())
            .spawn(move || {
                let mut brain = Brain::new(env);
                let ran = catch_unwind(AssertUnwindSafe(|| run(&mut brain, &rx, &mic_muted)));
                if ran.is_err() {
                    error!(
                        "jarvis: the command runner crashed; commands are off until hakod restarts"
                    );
                }
            });
        if let Err(e) = spawned {
            warn!("jarvis: cannot start its thread: {e}");
        }
        Self { tx }
    }

    pub fn send(&self, msg: RunnerIn) {
        let _ = self.tx.send(msg);
    }
}

fn run(brain: &mut Brain, rx: &mpsc::Receiver<RunnerIn>, mic_muted: &AtomicBool) {
    let mut listening = false;
    let mut poll_at = Instant::now();
    loop {
        let now = Instant::now();
        let mut next = brain.tick(now);
        if listening {
            if now >= poll_at {
                mic_muted.store(brain.mic_muted(), Ordering::Relaxed);
                poll_at = now + MIC_POLL;
            }
            next = Some(next.map_or(poll_at, |n| n.min(poll_at)));
        }
        let wait = next.map_or(Duration::from_secs(3600), |n| {
            n.saturating_duration_since(Instant::now())
        });
        match rx.recv_timeout(wait) {
            Ok(RunnerIn::Heard { kind, text, file }) => {
                brain.heard(kind, &text, &file, Instant::now())
            }
            Ok(RunnerIn::Failed { kind, message }) => brain.failed(kind, &message),
            Ok(RunnerIn::Listening(on)) => {
                listening = on;
                poll_at = Instant::now();
                if !on {
                    mic_muted.store(false, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

// ---------------------------------------------------------------------------
// The real environment.

/// Tools Jarvis waits for (wpctl, dbus-send) get this long.
const TOOL_WAIT: Duration = Duration::from_secs(5);

pub struct RealEnv {
    pub hub: HubTx,
    pub voice: Box<dyn Fn(JarvisOut) + Send>,
}

impl Env for RealEnv {
    fn hub(&mut self, req: HubReq) -> Result<Value, String> {
        let (reply, answer) = mpsc::channel();
        self.hub
            .send((req, reply))
            .map_err(|_| "hakod is shutting down".to_string())?;
        answer
            .recv_timeout(HUB_WAIT)
            .map_err(|_| "hakod did not answer".to_string())?
    }

    fn run(&mut self, argv: &[String]) -> Result<String, String> {
        crate::voice::run_tool(argv, TOOL_WAIT).map_err(|e| format!("{e:#}"))
    }

    fn spawn(&mut self, argv: &[String]) -> Result<(), String> {
        let (program, args) = argv.split_first().ok_or("nothing to run")?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Its own process group: a signal to hakod's group does not reach it.
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot run {program}: {e}"))?;
        // Reaped when it exits, so no zombie stays behind.
        let _ = std::thread::Builder::new()
            .name("hako-jarvis-child".into())
            .spawn(move || child.wait());
        Ok(())
    }

    fn notify(&mut self, summary: &str, body: &str) {
        let argv = ["notify-send", "-a", "hako", "--", summary, body].map(str::to_string);
        if let Err(e) = self.spawn(&argv) {
            debug!("jarvis: notify-send: {e}");
        }
    }

    fn voice(&mut self, out: JarvisOut) {
        (self.voice)(out);
    }

    fn apps(&mut self) -> Vec<apps::App> {
        apps::scan(&apps::dirs())
    }
}

#[cfg(test)]
mod tests {
    use super::actions::tests::Fake;
    use super::*;
    use hako_core::voice::{UserAction, UserCommand};
    use std::sync::Mutex;

    #[test]
    fn the_wake_word_is_verified_fuzzily_and_stripped() {
        let v = |t: &str| verify(t, "jarvis");
        assert_eq!(v("Jarvis, open Chrome."), Some("open Chrome.".into()));
        assert_eq!(v("jarvis open chrome"), Some("open chrome".into()));
        assert_eq!(v("Hey Jarvis: volume 30"), Some("volume 30".into()));
        assert_eq!(v("OK, Jervis, pause"), Some("pause".into()));
        assert_eq!(v("Jar vis, next song"), Some("next song".into()));
        assert_eq!(v("Jarviss lights off"), Some("lights off".into()));
        assert_eq!(v("Jarvis."), Some(String::new()));
        assert_eq!(v("Travis, open Chrome"), None, "another word");
        assert_eq!(
            v("I think that jarvis is great"),
            None,
            "not in the first three words"
        );
        assert_eq!(v("open chrome"), None);
        assert_eq!(v(""), None);
        assert_eq!(
            verify("Hey computer, lights on", "hey computer"),
            Some("lights on".into())
        );
        assert_eq!(soundex("jarvis"), "J612");
        assert_eq!(soundex("Robert"), "R163");
        assert_eq!(soundex("Ashcraft"), "A261");
    }

    #[test]
    fn yes_is_yes() {
        for t in ["Yes.", "yeah", "Yes, do it", "do it", "confirm"] {
            assert!(is_yes(t), "{t}");
        }
        for t in ["no", "nope, don't", "yes no", "maybe", "", "cancel that"] {
            assert!(!is_yes(t), "{t}");
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_wake_take_starts_with_the_ring_and_ends_at_the_pause() {
        let t = t0();
        let mut ear = Ear::default();
        ear.pcm(&vec![0.1; RING + 100]);
        ear.begin(Listen::Wake, t);
        ear.pcm(&[0.2; 50]);
        // Speech goes on, then a pause well after the word ends the take.
        assert_eq!(ear.speech(true, t + Duration::from_millis(100)), None);
        let (kind, pcm) = ear.speech(false, t + Duration::from_millis(2000)).unwrap();
        assert_eq!(kind, Listen::Wake);
        assert_eq!(
            pcm.len(),
            RING + 50,
            "two seconds before the wake, then the command"
        );
        assert!(ear.taking().is_none());
        // The ring refills only between takes.
        ear.pcm(&[0.3; 10]);
        assert_eq!(ear.ring.len(), 10);
    }

    #[test]
    fn a_pause_right_after_the_wake_word_waits_for_the_command() {
        let t = t0();
        let ms = |n| t + Duration::from_millis(n);
        let mut ear = Ear::default();
        ear.begin(Listen::Wake, t);
        // "Jarvis" ... (waits for the ping) ... "open chrome"
        assert_eq!(ear.speech(false, ms(700)), None);
        assert_eq!(ear.tick(ms(800)), (None, Some(ms(4000))));
        assert_eq!(ear.speech(true, ms(1500)), None);
        assert_eq!(
            ear.tick(ms(4100)).1,
            Some(ms(8000)),
            "speaking: the cap only"
        );
        assert!(ear.speech(false, ms(3000)).is_some());

        // Nothing follows: the take ends after the resume wait.
        ear.begin(Listen::Wake, t);
        assert_eq!(ear.speech(false, ms(500)), None);
        assert!(ear.tick(ms(4000)).0.is_some());

        // A long command is cut at the cap.
        ear.begin(Listen::Wake, t);
        ear.speech(true, ms(10));
        assert!(ear.tick(ms(7999)).0.is_none());
        assert!(ear.tick(ms(8000)).0.is_some());
    }

    #[test]
    fn push_and_confirm_takes() {
        let t = t0();
        let ms = |n| t + Duration::from_millis(n);
        let mut ear = Ear::default();
        ear.pcm(&[1.0; 100]);
        ear.begin(Listen::Push, t);
        ear.pcm(&vec![0.5; BLEED + 20]);
        assert_eq!(
            ear.speech(false, ms(5000)),
            None,
            "a pause does not end a push"
        );
        let (kind, pcm) = ear.release().unwrap();
        assert_eq!(
            (kind, pcm.len()),
            (Listen::Push, 20),
            "no ring, no ping bleed"
        );

        ear.begin(Listen::Confirm, t);
        assert_eq!(ear.release(), None);
        assert_eq!(ear.speech(false, ms(300)), None, "nothing said yet");
        ear.speech(true, ms(500));
        assert!(ear.speech(false, ms(1500)).is_some());
        ear.begin(Listen::Confirm, t);
        assert!(ear.tick(ms(6000)).0.is_some());
        ear.begin(Listen::Push, t);
        assert_eq!(ear.cancel(), Some(Listen::Push));
        assert_eq!(ear.tick(ms(1)), (None, None));
    }

    /// `Fake` behind a shared handle, so the test can look at it.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Fake>>);

    impl Env for Shared {
        fn hub(&mut self, req: HubReq) -> Result<Value, String> {
            self.0.lock().unwrap().hub(req)
        }
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.0.lock().unwrap().run(argv)
        }
        fn spawn(&mut self, argv: &[String]) -> Result<(), String> {
            self.0.lock().unwrap().spawn(argv)
        }
        fn notify(&mut self, summary: &str, body: &str) {
            self.0.lock().unwrap().notify(summary, body)
        }
        fn voice(&mut self, out: JarvisOut) {
            self.0.lock().unwrap().voice(out)
        }
        fn apps(&mut self) -> Vec<apps::App> {
            self.0.lock().unwrap().apps()
        }
    }

    fn outcomes(s: &Shared) -> Vec<(Option<String>, bool, String)> {
        s.0.lock()
            .unwrap()
            .voice
            .iter()
            .filter_map(|o| match o {
                JarvisOut::Done(c) => Some((c.action.clone(), c.ok, c.message.clone().unwrap())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn commands_run_and_report_false_wakes_are_dropped() {
        let shared = Shared::default();
        shared.0.lock().unwrap().profiles = vec!["Gaming".into()];
        let mut b = Brain::new(Box::new(shared.clone()));
        let file = VoiceFile::default();
        let now = Instant::now();
        b.heard(Listen::Wake, "Jarvis, gaming profile.", &file, now);
        b.heard(Listen::Wake, "and then he said", &file, now);
        b.heard(Listen::Wake, "Jarvis, blorp the flux", &file, now);
        b.heard(Listen::Push, "volume 30", &file, now);
        assert_eq!(b.false_wakes, 1);
        assert_eq!(
            outcomes(&shared),
            [
                (
                    Some("apply profile Gaming".into()),
                    true,
                    "Applied profile Gaming".into()
                ),
                (None, false, "Didn't catch that".into()),
                (Some("volume 30%".into()), true, "Volume 30%".into()),
            ]
        );
        let f = shared.0.lock().unwrap();
        assert!(f
            .hub
            .contains(&(Method::ApplyProfile, serde_json::json!({"name": "Gaming"}))));
        assert_eq!(f.notes.len(), 1, "the miss is notified");
    }

    #[test]
    fn timers_ring_at_their_deadline() {
        let shared = Shared::default();
        let mut b = Brain::new(Box::new(shared.clone()));
        let t = Instant::now();
        b.heard(
            Listen::Push,
            "timer 90 seconds called tea",
            &VoiceFile::default(),
            t,
        );
        b.heard(Listen::Push, "timer 1 minute", &VoiceFile::default(), t);
        assert_eq!(outcomes(&shared)[0].2, "Timer 1 min 30 s");
        assert_eq!(
            b.tick(t + Duration::from_secs(10)),
            Some(t + Duration::from_secs(60))
        );
        assert_eq!(
            b.tick(t + Duration::from_secs(60)),
            Some(t + Duration::from_secs(90))
        );
        assert_eq!(b.tick(t + Duration::from_secs(95)), None);
        let f = shared.0.lock().unwrap();
        let pings = f.voice.iter().filter(|o| **o == JarvisOut::Ping).count();
        assert_eq!(pings, 2);
        assert_eq!(
            f.notes,
            [
                ("Timer done".to_string(), "1 min".to_string()),
                ("Timer done".to_string(), "tea (1 min 30 s)".to_string())
            ]
        );
    }

    #[test]
    fn a_command_that_asks_first_runs_only_on_yes() {
        let shared = Shared::default();
        let mut b = Brain::new(Box::new(shared.clone()));
        let mut file = VoiceFile::default();
        file.jarvis.commands = vec![UserCommand {
            say: vec!["shut down".into()],
            action: UserAction::Run {
                program: "/usr/bin/systemctl".into(),
                args: vec!["poweroff".into()],
            },
            confirm: true,
        }];
        let t = Instant::now();
        b.heard(Listen::Push, "shut down", &file, t);
        assert!(shared.0.lock().unwrap().voice.contains(&JarvisOut::Confirm));
        assert!(shared.0.lock().unwrap().spawned.is_empty());
        b.heard(Listen::Confirm, "no", &file, t);
        b.heard(Listen::Push, "shut down", &file, t);
        b.heard(Listen::Confirm, "yes", &file, t);
        b.heard(Listen::Push, "shut down", &file, t);
        assert_eq!(
            b.tick(t + Duration::from_secs(31)),
            None,
            "not answered: dropped"
        );
        let got: Vec<_> = outcomes(&shared)
            .into_iter()
            .map(|(_, ok, m)| (ok, m))
            .collect();
        assert_eq!(
            got,
            [
                (false, "Not confirmed".to_string()),
                (true, "Ran \"shut down\"".to_string()),
                (false, "Not confirmed".to_string()),
            ]
        );
        assert_eq!(
            shared.0.lock().unwrap().spawned,
            [vec![
                "/usr/bin/systemctl".to_string(),
                "poweroff".to_string()
            ]]
        );
    }
}
