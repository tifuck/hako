//! A spoken command (wake word stripped) to an [`Action`]: the user's own
//! phrases first, then the built-in grammar. Pure: profile names and mixer
//! strip labels come in through [`Context`].

use hako_core::jarvis::{Action, MediaCommand};
use hako_core::voice::UserCommand;

/// What the grammar resolves names against.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    pub profiles: &'a [String],
    /// Mixer strip labels (inputs and outputs).
    pub strips: &'a [String],
    pub commands: &'a [UserCommand],
}

/// The command `text` asks for, if any.
pub fn parse(text: &str, ctx: &Context) -> Option<Action> {
    let w = Words::new(text);
    if w.is_empty() {
        return None;
    }
    user(&w, ctx.commands).or_else(|| builtin(&w, ctx))
}

// ---------------------------------------------------------------------------
// Words.

/// Leading words that only make a command polite.
const LEAD: &[&str] = &[
    "please", "can", "could", "would", "will", "you", "hey", "ok", "okay", "um", "uh", "so", "now",
    "just", "and", "oh",
];
const TRAIL: &[&str] = &["please", "thanks", "now", "thank", "you"];

/// Normalised words (lowercase, punctuation trimmed) with their byte spans
/// in the original text, polite words at either end dropped.
struct Words<'a> {
    raw: &'a str,
    w: Vec<String>,
    spans: Vec<(usize, usize)>,
}

impl<'a> Words<'a> {
    fn new(raw: &'a str) -> Self {
        let bytes = raw.as_bytes();
        // A comma between digits ("1,600") is part of the number.
        let sep = |i: usize, c: char| {
            c.is_whitespace()
                || matches!(c, ';' | '!' | '?' | '"' | '(' | ')' | '-' | '/')
                || (c == ','
                    && !(i > 0
                        && bytes[i - 1].is_ascii_digit()
                        && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)))
        };
        let mut w = Vec::new();
        let mut spans = Vec::new();
        let mut start = None;
        for (i, c) in raw.char_indices().chain([(raw.len(), ' ')]) {
            if sep(i, c) {
                if let Some(s) = start.take() {
                    let token = &raw[s..i];
                    let norm: String = token
                        .trim_matches(|c: char| !c.is_alphanumeric() && c != '%')
                        .to_lowercase()
                        .chars()
                        .filter(|&c| c != ',')
                        .collect();
                    if !norm.is_empty() {
                        w.push(norm);
                        spans.push((s, i));
                    }
                }
            } else if start.is_none() {
                start = Some(i);
            }
        }
        let mut lead = 0;
        while lead < w.len() && LEAD.contains(&w[lead].as_str()) {
            lead += 1;
        }
        let mut end = w.len();
        while end > lead && TRAIL.contains(&w[end - 1].as_str()) {
            end -= 1;
        }
        w.truncate(end);
        spans.truncate(end);
        w.drain(..lead);
        spans.drain(..lead);
        Self { raw, w, spans }
    }

    fn is_empty(&self) -> bool {
        self.w.is_empty()
    }

    fn len(&self) -> usize {
        self.w.len()
    }

    fn get(&self, i: usize) -> &str {
        self.w.get(i).map_or("", String::as_str)
    }

    /// The original text of words `from..to`.
    fn text(&self, from: usize, to: usize) -> String {
        if from >= to || to > self.len() {
            return String::new();
        }
        self.raw[self.spans[from].0..self.spans[to - 1].1]
            .trim_end_matches(['.', ',', '!', '?'])
            .trim()
            .to_string()
    }

    fn rest(&self, from: usize) -> String {
        self.text(from, self.len())
    }

    /// The words start with `phrase`: how many words it took.
    fn starts(&self, at: usize, phrase: &str) -> Option<usize> {
        let p: Vec<&str> = phrase.split(' ').collect();
        (self.w.len() >= at + p.len()
            && self.w[at..at + p.len()].iter().zip(&p).all(|(a, b)| a == b))
        .then_some(p.len())
    }

    fn has(&self, any: &[&str]) -> bool {
        self.w.iter().any(|w| any.contains(&w.as_str()))
    }
}

// ---------------------------------------------------------------------------
// Fuzzy matching.

pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Edits a word of this length may differ by and still match.
fn slack(len: usize) -> usize {
    match len {
        0..=3 => 0,
        4..=6 => 1,
        _ => 2,
    }
}

/// One spoken word against one expected word.
fn word_eq(heard: &str, want: &str) -> bool {
    heard == want
        || heard.trim_end_matches('s') == want.trim_end_matches('s')
        || levenshtein(heard, want) <= slack(heard.len().min(want.len()))
}

/// Lowercase letters and digits only, for comparing names.
pub(crate) fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// How well `heard` names `name` (0 = not at all): exact, then close.
pub(crate) fn name_score(heard: &str, name: &str) -> u32 {
    let (h, n) = (squash(heard), squash(name));
    if h.is_empty() || n.is_empty() {
        return 0;
    }
    if h == n {
        return 3;
    }
    if levenshtein(&h, &n) <= slack(h.len().min(n.len())) {
        return 2;
    }
    0
}

/// The candidate `heard` names best (ties: the first).
pub(crate) fn pick<'c>(heard: &str, candidates: &'c [String]) -> Option<&'c String> {
    let mut best: Option<(u32, &String)> = None;
    for c in candidates {
        let s = name_score(heard, c);
        if s > 0 && best.is_none_or(|(b, _)| s > b) {
            best = Some((s, c));
        }
    }
    best.map(|(_, c)| c)
}

// ---------------------------------------------------------------------------
// Numbers and durations.

fn unit_value(w: &str) -> Option<i64> {
    const UNITS: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    if let Some(i) = UNITS.iter().position(|u| *u == w) {
        return Some(i as i64);
    }
    if let Some(i) = TENS
        .iter()
        .position(|t| *t == w || (*t == "forty" && w == "fourty"))
    {
        return Some(20 + 10 * i as i64);
    }
    None
}

fn digits(w: &str) -> Option<i64> {
    let d = w.trim_end_matches('%');
    (!d.is_empty() && d.len() <= 9 && d.bytes().all(|b| b.is_ascii_digit()))
        .then(|| d.parse().ok())
        .flatten()
}

/// A number at `at` (digits or words: "forty", "one thousand six hundred",
/// "sixteen hundred"), with a trailing "percent": (value, words used).
fn number(w: &Words, at: usize) -> Option<(i64, usize)> {
    let mut i = at;
    let mut value = if let Some(d) = digits(w.get(i)) {
        i += 1;
        Some(d)
    } else {
        let (mut total, mut current, mut any) = (0i64, 0i64, false);
        loop {
            let word = w.get(i);
            let next = w.get(i + 1);
            if let Some(v) = unit_value(word) {
                current += v;
            } else if (word == "a" || word == "an")
                && !any
                && matches!(next, "hundred" | "thousand")
            {
                current = 1;
            } else if word == "hundred" && i > at {
                current = current.max(1) * 100;
            } else if word == "thousand" && i > at {
                total += current.max(1) * 1000;
                current = 0;
            } else if word == "and" && any && unit_value(next).is_some() {
            } else {
                break;
            }
            any = true;
            i += 1;
        }
        any.then_some(total + current)
    };
    // "sixteen hundred" / "1 thousand"
    if value.is_some() && i > at {
        match w.get(i) {
            "hundred" => {
                value = value.map(|v| v * 100);
                i += 1;
            }
            "thousand" => {
                value = value.map(|v| v * 1000);
                i += 1;
            }
            _ => {}
        }
    }
    let v = value?;
    if w.starts(i, "per cent").is_some() {
        i += 2;
    } else if matches!(w.get(i), "percent" | "%") {
        i += 1;
    }
    Some((v, i - at))
}

/// The first number anywhere: (value, start, words used).
fn find_number(w: &Words) -> Option<(i64, usize, usize)> {
    (0..w.len()).find_map(|i| number(w, i).map(|(v, n)| (v, i, n)))
}

fn time_unit(w: &str) -> Option<u32> {
    match w {
        "s" | "sec" | "secs" | "second" | "seconds" => Some(1),
        "m" | "min" | "mins" | "minute" | "minutes" => Some(60),
        "h" | "hr" | "hrs" | "hour" | "hours" => Some(3600),
        _ => None,
    }
}

/// A duration at `at` ("5 minutes", "an hour and a half", "1 hour 30
/// minutes", "half a minute"): (seconds, words used).
fn duration(w: &Words, at: usize) -> Option<(u32, usize)> {
    let mut i = at;
    let mut total = 0u32;
    loop {
        let start = i;
        if total > 0 && w.get(i) == "and" {
            i += 1;
        }
        if w.get(i) == "half" && matches!(w.get(i + 1), "a" | "an") {
            if let Some(u) = time_unit(w.get(i + 2)) {
                total += u / 2;
                i += 3;
                continue;
            }
        }
        if total > 0 && w.starts(i, "a half").is_some() {
            // "an hour and a half": half of the last unit.
            let last = time_unit(w.get(start.saturating_sub(1))).unwrap_or(0);
            total += last / 2;
            i += 2;
            continue;
        }
        let (amount, used) = match w.get(i) {
            "a" | "an" if time_unit(w.get(i + 1)).is_some() => (1, 1),
            _ => match number(w, i) {
                Some((v, n)) if (0..=100_000).contains(&v) => (v as u32, n),
                _ => {
                    i = start;
                    break;
                }
            },
        };
        let Some(unit) = time_unit(w.get(i + used)) else {
            i = start;
            break;
        };
        total = total.saturating_add(amount.saturating_mul(unit));
        i += used + 1;
    }
    (i > at && total > 0).then_some((total, i - at))
}

// ---------------------------------------------------------------------------
// The user's commands.

/// A phrase split at its `{slot}`: words before, a slot or not, words after.
fn split_phrase(phrase: &str) -> (Vec<String>, bool, Vec<String>) {
    let norm = |s: &str| Words::new(s).w;
    match (phrase.find('{'), phrase.find('}')) {
        (Some(a), Some(b)) if a < b => (norm(&phrase[..a]), true, norm(&phrase[b + 1..])),
        _ => (norm(phrase), false, Vec::new()),
    }
}

fn user(w: &Words, commands: &[UserCommand]) -> Option<Action> {
    // (words matched, index, slot); more matched words wins.
    let mut best: Option<(usize, usize, Option<String>)> = None;
    for (index, c) in commands.iter().enumerate() {
        for phrase in &c.say {
            let (pre, slot, post) = split_phrase(phrase);
            let fixed = pre.len() + post.len();
            let found = if slot {
                (w.len() > fixed
                    && w.w[..pre.len()]
                        .iter()
                        .zip(&pre)
                        .all(|(h, p)| word_eq(h, p))
                    && w.w[w.len() - post.len()..]
                        .iter()
                        .zip(&post)
                        .all(|(h, p)| word_eq(h, p)))
                .then(|| Some(w.text(pre.len(), w.len() - post.len())))
            } else {
                (fixed > 0 && w.len() == fixed && w.w.iter().zip(&pre).all(|(h, p)| word_eq(h, p)))
                    .then_some(None)
            };
            if let Some(slot) = found {
                if best.as_ref().is_none_or(|(n, ..)| fixed > *n) {
                    best = Some((fixed, index, slot));
                }
            }
        }
    }
    best.map(|(_, index, slot)| Action::User { index, slot })
}

// ---------------------------------------------------------------------------
// The built-in grammar.

const MIC: &[&str] = &["mic", "microphone", "mike", "mics"];
const LIGHTS: &[&str] = &[
    "light",
    "lights",
    "lighting",
    "rgb",
    "led",
    "leds",
    "backlight",
    "backlighting",
];
const VOLUME: &[&str] = &["volume", "level", "sound", "loudness"];
const MEDIA_NOUNS: &[&str] = &[
    "music", "song", "track", "playback", "video", "media", "the", "this", "one", "it", "playing",
    "audio", "podcast",
];

fn builtin(w: &Words, ctx: &Context) -> Option<Action> {
    timer(w)
        .or_else(|| search(w))
        .or_else(|| typing(w))
        .or_else(|| dictate(w))
        .or_else(|| profile(w, ctx))
        .or_else(|| open(w))
        .or_else(|| mute(w, ctx))
        .or_else(|| chatmix(w))
        .or_else(|| lighting(w))
        .or_else(|| dpi(w))
        .or_else(|| battery(w))
        .or_else(|| volume(w, ctx))
        .or_else(|| media(w))
}

fn timer(w: &Words) -> Option<Action> {
    if !w.has(&["timer", "countdown", "alarm"]) && w.starts(0, "remind me in").is_none() {
        return None;
    }
    let (seconds, at, used) = (0..w.len()).find_map(|i| duration(w, i).map(|(s, n)| (s, i, n)))?;
    let after = at + used;
    let label = match w.get(after) {
        "called" | "named" | "labelled" | "labeled" | "for" | "to" => Some(w.rest(after + 1)),
        _ => None,
    }
    .filter(|l| !l.is_empty());
    Some(Action::Timer { seconds, label })
}

fn search(w: &Words) -> Option<Action> {
    let verbs = [
        "search the web for",
        "search online for",
        "search for",
        "search",
        "web search for",
        "web search",
        "google",
        "look up",
    ];
    let n = verbs.iter().find_map(|v| w.starts(0, v))?;
    let query = w.rest(n);
    (!query.is_empty()).then_some(Action::Search { query })
}

fn typing(w: &Words) -> Option<Action> {
    let n = w.starts(0, "type")?;
    let text = w.rest(n);
    (!text.is_empty()).then_some(Action::Type { text })
}

fn dictate(w: &Words) -> Option<Action> {
    let all = w.w.join(" ");
    [
        "dictate",
        "dictation",
        "start dictation",
        "start dictating",
        "take a note",
        "take note",
        "take notes",
        "write this down",
    ]
    .contains(&all.as_str())
    .then_some(Action::Dictate)
}

/// `apply profile work`, `gaming profile`, `switch to streaming`.
fn profile(w: &Words, ctx: &Context) -> Option<Action> {
    let verbs = [
        "apply",
        "load",
        "use",
        "activate",
        "switch to",
        "change to",
        "go to",
        "set",
        "open",
        "enable",
    ];
    let mut at = verbs.iter().find_map(|v| w.starts(0, v)).unwrap_or(0);
    let verb = at > 0;
    if w.get(at) == "the" || w.get(at) == "my" {
        at += 1;
    }
    let mut end = w.len();
    let mut keyword = false;
    if w.get(at) == "profile" {
        at += 1;
        keyword = true;
    }
    if matches!(w.get(end - 1), "profile" | "mode") && end > at + 1 {
        keyword |= w.get(end - 1) == "profile";
        end -= 1;
    }
    if at >= end {
        return None;
    }
    let heard = w.text(at, end);
    if let Some(name) = pick(&heard, ctx.profiles) {
        // A bare name only with a verb ("switch to streaming").
        if verb || keyword || end < w.len() {
            return Some(Action::ApplyProfile { name: name.clone() });
        }
        return None;
    }
    // An unknown name said as a profile: the apply says it is missing.
    keyword.then_some(Action::ApplyProfile { name: heard })
}

/// `open firefox`, `open youtube dot com`, `go to github.com`.
fn open(w: &Words) -> Option<Action> {
    let app = ["open", "launch", "start", "run", "fire up", "bring up"]
        .iter()
        .find_map(|v| w.starts(0, v));
    let site = ["go to", "visit", "browse to", "navigate to"]
        .iter()
        .find_map(|v| w.starts(0, v));
    let mut at = app.or(site)?;
    if matches!(w.get(at), "the" | "my") {
        at += 1;
    }
    let mut end = w.len();
    if app.is_some() && matches!(w.get(end - 1), "app" | "application") && end > at + 1 {
        end -= 1;
    }
    let target = w.text(at, end);
    if target.is_empty() {
        return None;
    }
    if let Some(url) = as_url(&target) {
        return Some(Action::OpenUrl { url });
    }
    app.map(|_| Action::OpenApp { name: target })
}

/// `youtube dot com` / `github.com/x` / `https://...` as a URL.
fn as_url(target: &str) -> Option<String> {
    let t = target.trim().to_lowercase();
    if t.starts_with("https://") || t.starts_with("http://") {
        return (!t.contains(char::is_whitespace)).then(|| target.trim().to_string());
    }
    let joined = t.replace(" dot ", ".").replace(" slash ", "/");
    if joined.contains(char::is_whitespace) {
        return None;
    }
    let host = joined.split('/').next()?;
    let tld = host.rsplit_once('.')?.1;
    let ok = (2..=6).contains(&tld.len())
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    ok.then(|| format!("https://{joined}"))
}

fn mute(w: &Words, ctx: &Context) -> Option<Action> {
    let first = w.get(0);
    let mut on = match first {
        "mute" | "silence" => Some(true),
        "unmute" => Some(false),
        "toggle" => None,
        _ => {
            // "mic off", "microphone on"
            if MIC.contains(&first) && w.len() == 2 {
                return match w.get(1) {
                    "off" => Some(Action::MicMute { on: Some(true) }),
                    "on" => Some(Action::MicMute { on: Some(false) }),
                    _ => None,
                };
            }
            return None;
        }
    };
    let mut at = 1;
    if first == "toggle" {
        if w.get(1) != "mute" {
            // "toggle my mic"
            on = None;
        } else {
            at = 2;
        }
    }
    while matches!(w.get(at), "the" | "my" | "a") {
        at += 1;
    }
    let rest = w.text(at, w.len());
    let rest_words = &w.w[at.min(w.len())..];
    if rest_words.iter().any(|r| MIC.contains(&r.as_str())) {
        return Some(Action::MicMute { on });
    }
    if rest.is_empty()
        || rest_words.iter().all(|r| {
            [
                "sound",
                "audio",
                "volume",
                "everything",
                "computer",
                "system",
                "speakers",
                "pc",
                "it",
                "all",
                "output",
            ]
            .contains(&r.as_str())
        })
    {
        if first == "toggle" && w.get(1) != "mute" {
            return None;
        }
        return Some(Action::SystemMute { on });
    }
    pick(&rest, ctx.strips).map(|s| Action::StripMute {
        strip: s.clone(),
        on,
    })
}

fn chatmix(w: &Words) -> Option<Action> {
    let all = w.w.join(" ");
    let mut s = all.as_str();
    for p in [
        "chatmix ",
        "chat mix ",
        "balance ",
        "set chatmix to ",
        "set chat mix to ",
    ] {
        if let Some(r) = all.strip_prefix(p) {
            s = r;
        }
    }
    let named = s.len() != all.len() || w.has(&["chatmix"]);
    let fixed = match s.trim_start_matches("to ") {
        "more game" => Some(-50),
        "more chat" => Some(50),
        "all game" | "only game" | "full game" => Some(-100),
        "all chat" | "only chat" | "full chat" => Some(100),
        "center" | "centre" | "middle" | "reset" | "even" | "balanced" | "equal" | "half"
            if named =>
        {
            Some(0)
        }
        _ => None,
    };
    if let Some(balance) = fixed {
        return Some(Action::ChatMix { balance });
    }
    if !named {
        return None;
    }
    let (v, at, _) = find_number(w)?;
    let negative = at > 0 && matches!(w.get(at - 1), "minus" | "negative");
    let v = if negative { -v } else { v };
    (-100..=100)
        .contains(&v)
        .then_some(Action::ChatMix { balance: v as i8 })
}

fn lighting(w: &Words) -> Option<Action> {
    let lights = w.has(LIGHTS);
    if w.has(&["brightness"]) || lights {
        if let Some((v, ..)) = find_number(w) {
            return Some(Action::Brightness {
                percent: v.clamp(0, 100) as u8,
            });
        }
    }
    if !lights {
        return None;
    }
    if w.has(&["off"]) {
        Some(Action::Lighting { on: false })
    } else if w.has(&["on"]) {
        Some(Action::Lighting { on: true })
    } else {
        None
    }
}

fn dpi(w: &Words) -> Option<Action> {
    if !w.has(&["dpi", "cpi", "sensitivity", "dpis"]) {
        return None;
    }
    let (v, ..) = find_number(w)?;
    (50..=100_000)
        .contains(&v)
        .then_some(Action::Dpi { dpi: v as u32 })
}

fn battery(w: &Words) -> Option<Action> {
    w.has(&["battery", "batteries", "charge", "charged"])
        .then_some(Action::Battery)
}

/// `volume 30`, `set the volume to 50 percent`, `game volume 40`, `volume
/// up`, `turn it down by 20`, `louder`.
fn volume(w: &Words, ctx: &Context) -> Option<Action> {
    let up = w.has(&["up", "louder", "raise", "increase"]);
    let down = w.has(&["down", "quieter", "softer", "lower", "decrease", "reduce"]);
    let num = find_number(w);
    let had_volume = w.has(VOLUME);
    if had_volume && w.has(&["max", "maximum", "full"]) {
        return Some(Action::Volume { percent: 100 });
    }
    // Everything that is neither filler nor the number names the strip.
    const FILLER: &[&str] = &[
        "set", "turn", "put", "make", "change", "the", "of", "to", "at", "percent", "per", "cent",
        "my", "it", "this", "by", "up", "down", "volume", "level", "sound", "loudness", "louder",
        "quieter", "softer", "raise", "lower", "increase", "decrease", "reduce", "%",
    ];
    let rest: Vec<usize> = (0..w.len())
        .filter(|&i| !num.is_some_and(|(_, at, n)| (at..at + n).contains(&i)))
        .filter(|&i| !FILLER.contains(&w.get(i)))
        .collect();
    let named = rest
        .first()
        .zip(rest.last())
        .filter(|(a, b)| *b + 1 - *a == rest.len())
        .and_then(|(a, b)| pick(&w.text(*a, b + 1), ctx.strips));
    if up != down && (named.is_none() && rest.is_empty()) {
        let first_ok = had_volume
            || matches!(w.get(0), "turn" | "louder" | "quieter" | "softer")
            || w.has(&["louder", "quieter", "softer"]);
        if first_ok {
            let step = num.map_or(10, |(v, ..)| v.clamp(1, 100)) as i8;
            return Some(Action::VolumeStep {
                delta: if up { step } else { -step },
            });
        }
    }
    let (v, ..) = num?;
    if up || down {
        return None;
    }
    let percent = v.clamp(0, 100) as u8;
    match named {
        Some(strip) => Some(Action::StripVolume {
            strip: strip.clone(),
            percent,
        }),
        None if rest.is_empty() && had_volume => Some(Action::Volume { percent }),
        None => None,
    }
}

fn media(w: &Words) -> Option<Action> {
    let first = w.get(0);
    let (command, at) = match first {
        "play" | "pause" | "resume" | "unpause" => (MediaCommand::PlayPause, 1),
        "next" | "skip" => (MediaCommand::Next, 1),
        "previous" | "prev" | "back" => (MediaCommand::Previous, 1),
        "last" if w.has(&["song", "track", "one", "video"]) => (MediaCommand::Previous, 1),
        "go" if w.get(1) == "back" => (MediaCommand::Previous, 2),
        "stop" => (MediaCommand::Stop, 1),
        _ => return None,
    };
    w.w[at..]
        .iter()
        .all(|r| MEDIA_NOUNS.contains(&r.as_str()))
        .then_some(Action::Media { command })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::voice::UserAction;

    fn ctx_parse(text: &str) -> Option<Action> {
        let profiles = ["Gaming".to_string(), "Work".into(), "Streaming".into()];
        let strips = [
            "Game".to_string(),
            "Chat".into(),
            "Music".into(),
            "Line in".into(),
            "Headset".into(),
        ];
        let commands = vec![
            UserCommand {
                say: vec!["open {app} in the terminal".into(), "terminal {app}".into()],
                action: UserAction::Run {
                    program: "/usr/bin/kitty".into(),
                    args: vec!["{slot}".into()],
                },
                confirm: false,
            },
            UserCommand {
                say: vec!["game mode".into(), "gaming time".into()],
                action: UserAction::Profile {
                    name: "Gaming".into(),
                },
                confirm: false,
            },
            UserCommand {
                say: vec!["shut it all down".into()],
                action: UserAction::Run {
                    program: "/usr/bin/systemctl".into(),
                    args: vec!["poweroff".into()],
                },
                confirm: true,
            },
        ];
        parse(
            text,
            &Context {
                profiles: &profiles,
                strips: &strips,
                commands: &commands,
            },
        )
    }

    fn user(index: usize, slot: Option<&str>) -> Action {
        Action::User {
            index,
            slot: slot.map(str::to_string),
        }
    }

    #[test]
    fn numbers_in_words_and_digits() {
        let n = |s: &str| number(&Words::new(s), 0).map(|(v, _)| v);
        assert_eq!(n("forty"), Some(40));
        assert_eq!(n("forty two"), Some(42));
        assert_eq!(n("one thousand six hundred"), Some(1600));
        assert_eq!(n("sixteen hundred"), Some(1600));
        assert_eq!(n("a hundred"), Some(100));
        assert_eq!(n("one hundred and twenty"), Some(120));
        assert_eq!(n("1,600"), Some(1600));
        assert_eq!(n("50%"), Some(50));
        assert_eq!(n("three thousand two hundred and fifty"), Some(3250));
        assert_eq!(n("zero"), Some(0));
        assert_eq!(n("game"), None);
        assert_eq!(number(&Words::new("40 per cent"), 0), Some((40, 3)));
        let d = |s: &str| duration(&Words::new(s), 0).map(|(v, _)| v);
        assert_eq!(d("5 minutes"), Some(300));
        assert_eq!(d("an hour and a half"), Some(5400));
        assert_eq!(d("1 hour 30 minutes"), Some(5400));
        assert_eq!(d("half a minute"), Some(30));
        assert_eq!(d("ninety seconds"), Some(90));
        assert_eq!(d("two minutes and thirty seconds"), Some(150));
        assert_eq!(d("five"), None);
    }

    #[test]
    fn every_registered_example_parses_as_its_action() {
        // The examples in the registry, with a context that names their
        // profiles and strips.
        let profiles = ["gaming".to_string(), "work".into(), "streaming".into()];
        let strips = ["game".to_string(), "music".into(), "chat".into()];
        let ctx = Context {
            profiles: &profiles,
            strips: &strips,
            commands: &[],
        };
        for spec in hako_core::jarvis::ACTIONS {
            for ex in spec.examples {
                let a = parse(ex, &ctx).unwrap_or_else(|| panic!("{ex:?}: no match"));
                let tag = serde_json::to_value(&a).unwrap()["action"].clone();
                assert_eq!(tag, spec.name, "{ex:?} -> {a:?}");
            }
        }
    }

    #[test]
    fn phrasings() {
        use Action::*;
        use MediaCommand::*;
        let s = |x: &str| x.to_string();
        let table: Vec<(&str, Action)> = vec![
            ("open firefox", OpenApp { name: s("firefox") }),
            ("Open Firefox.", OpenApp { name: s("Firefox") }),
            (
                "could you please launch the steam app",
                OpenApp { name: s("steam") },
            ),
            (
                "start Visual Studio Code",
                OpenApp {
                    name: s("Visual Studio Code"),
                },
            ),
            (
                "open youtube dot com",
                OpenUrl {
                    url: s("https://youtube.com"),
                },
            ),
            (
                "go to github.com",
                OpenUrl {
                    url: s("https://github.com"),
                },
            ),
            (
                "open https://example.org/a",
                OpenUrl {
                    url: s("https://example.org/a"),
                },
            ),
            (
                "search for rust lifetimes",
                Search {
                    query: s("rust lifetimes"),
                },
            ),
            (
                "Google best headset 2026?",
                Search {
                    query: s("best headset 2026"),
                },
            ),
            (
                "look up the weather in Paris",
                Search {
                    query: s("the weather in Paris"),
                },
            ),
            (
                "type Hello, world!",
                Type {
                    text: s("Hello, world"),
                },
            ),
            ("gaming profile", ApplyProfile { name: s("Gaming") }),
            ("apply profile work", ApplyProfile { name: s("Work") }),
            (
                "switch to streaming",
                ApplyProfile {
                    name: s("Streaming"),
                },
            ),
            ("load the gamin profile", ApplyProfile { name: s("Gaming") }),
            ("apply profile party", ApplyProfile { name: s("party") }),
            (
                "game volume 40",
                StripVolume {
                    strip: s("Game"),
                    percent: 40,
                },
            ),
            (
                "game volume forty",
                StripVolume {
                    strip: s("Game"),
                    percent: 40,
                },
            ),
            (
                "set music to 70 percent",
                StripVolume {
                    strip: s("Music"),
                    percent: 70,
                },
            ),
            (
                "set the volume of chat to 25%",
                StripVolume {
                    strip: s("Chat"),
                    percent: 25,
                },
            ),
            (
                "line in volume 80",
                StripVolume {
                    strip: s("Line in"),
                    percent: 80,
                },
            ),
            (
                "mute chat",
                StripMute {
                    strip: s("Chat"),
                    on: Some(true),
                },
            ),
            (
                "unmute the music",
                StripMute {
                    strip: s("Music"),
                    on: Some(false),
                },
            ),
            (
                "toggle mute headset",
                StripMute {
                    strip: s("Headset"),
                    on: None,
                },
            ),
            ("mute my mic", MicMute { on: Some(true) }),
            ("unmute microphone", MicMute { on: Some(false) }),
            ("toggle my mic", MicMute { on: None }),
            ("mic off", MicMute { on: Some(true) }),
            ("more game", ChatMix { balance: -50 }),
            ("more chat", ChatMix { balance: 50 }),
            ("chatmix center", ChatMix { balance: 0 }),
            ("chat mix all game", ChatMix { balance: -100 }),
            ("chatmix minus thirty", ChatMix { balance: -30 }),
            ("volume 30", Volume { percent: 30 }),
            ("set volume to 50 percent", Volume { percent: 50 }),
            ("set the volume to fifty", Volume { percent: 50 }),
            ("volume max", Volume { percent: 100 }),
            ("volume up", VolumeStep { delta: 10 }),
            ("turn it down", VolumeStep { delta: -10 }),
            ("turn the volume up by 20", VolumeStep { delta: 20 }),
            ("louder", VolumeStep { delta: 10 }),
            ("mute sound", SystemMute { on: Some(true) }),
            ("unmute", SystemMute { on: Some(false) }),
            ("mute", SystemMute { on: Some(true) }),
            ("toggle mute", SystemMute { on: None }),
            ("lights off", Lighting { on: false }),
            ("turn on the lighting", Lighting { on: true }),
            ("RGB off please", Lighting { on: false }),
            ("brightness 50", Brightness { percent: 50 }),
            ("set the lights to 20 percent", Brightness { percent: 20 }),
            ("dpi 800", Dpi { dpi: 800 }),
            ("set sensitivity to 1600", Dpi { dpi: 1600 }),
            ("set dpi to sixteen hundred", Dpi { dpi: 1600 }),
            ("mouse dpi one thousand six hundred", Dpi { dpi: 1600 }),
            ("battery", Battery),
            ("how much battery does my headset have", Battery),
            ("is my mouse charged", Battery),
            ("pause", Media { command: PlayPause }),
            ("play", Media { command: PlayPause }),
            ("resume the music", Media { command: PlayPause }),
            ("next song", Media { command: Next }),
            ("skip this track", Media { command: Next }),
            ("previous track", Media { command: Previous }),
            ("go back", Media { command: Previous }),
            ("stop the music", Media { command: Stop }),
            (
                "timer 5 minutes",
                Timer {
                    seconds: 300,
                    label: None,
                },
            ),
            (
                "set a timer for 30 seconds called tea",
                Timer {
                    seconds: 30,
                    label: Some(s("tea")),
                },
            ),
            (
                "set a timer for an hour and a half",
                Timer {
                    seconds: 5400,
                    label: None,
                },
            ),
            (
                "ten minute timer",
                Timer {
                    seconds: 600,
                    label: None,
                },
            ),
            ("take a note", Dictate),
            ("dictate", Dictate),
        ];
        for (text, want) in table {
            assert_eq!(ctx_parse(text), Some(want), "{text:?}");
        }
    }

    #[test]
    fn user_commands_come_first_and_fill_their_slot() {
        assert_eq!(
            ctx_parse("open htop in the terminal"),
            Some(user(0, Some("htop")))
        );
        assert_eq!(
            ctx_parse("terminal Midnight Commander"),
            Some(user(0, Some("Midnight Commander")))
        );
        // Fuzzy: one wrong letter in a long word.
        assert_eq!(
            ctx_parse("open htop in the terminel"),
            Some(user(0, Some("htop")))
        );
        assert_eq!(ctx_parse("Game mode."), Some(user(1, None)));
        assert_eq!(ctx_parse("gaming time"), Some(user(1, None)));
        assert_eq!(ctx_parse("please shut it all down"), Some(user(2, None)));
        // An empty slot is no match: the built-in open takes it.
        assert_eq!(
            ctx_parse("open in the terminal"),
            Some(Action::OpenApp {
                name: "in the terminal".into()
            })
        );
    }

    #[test]
    fn nonsense_is_no_command() {
        for text in [
            "",
            "   ",
            "the quick brown fox",
            "banana",
            "set the thing",
            "volume",
            "dpi",
            "play some jazz",
            "toggle",
            "open",
            "chatmix",
            "game volume",
        ] {
            assert_eq!(ctx_parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn fuzzy_names() {
        let names = ["Discord".to_string(), "Line in".into(), "Game".into()];
        assert_eq!(pick("discord", &names), Some(&names[0]));
        assert_eq!(pick("discort", &names), Some(&names[0]));
        assert_eq!(pick("line-in", &names), Some(&names[1]));
        assert_eq!(pick("lime", &names), None);
        assert_eq!(pick("gam", &names), None, "short words need an exact match");
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
