//! Jarvis: the actions a spoken command may run. One registry: the command
//! grammar and the UI's "what can I say" list both come from here.
//! Design: `docs/superpowers/specs/2026-11-25-voice-stt-design.md` (Jarvis).

use serde::{Deserialize, Serialize};

/// A resolved command. Arguments are data, never shell text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Launch a desktop application (fuzzy name of its `.desktop` entry).
    OpenApp { name: String },
    /// Open a URL or file with the desktop's default handler.
    OpenUrl { url: String },
    /// Web search (`[jarvis] search_url`, default DuckDuckGo).
    Search { query: String },
    /// Type text into the focused field.
    Type { text: String },
    /// Apply a hako profile.
    ApplyProfile { name: String },
    /// Mixer strip (input or output, by label) volume in percent.
    StripVolume { strip: String, percent: u8 },
    /// Mute / unmute a mixer strip; `None` toggles.
    StripMute { strip: String, on: Option<bool> },
    /// Mute / unmute the microphone (headset mic, else the default source).
    MicMute { on: Option<bool> },
    /// ChatMix balance: -100 all game .. 100 all chat.
    ChatMix { balance: i8 },
    /// System output volume in percent (default sink).
    Volume { percent: u8 },
    /// Change the system volume by this many percent.
    VolumeStep { delta: i8 },
    /// Mute / unmute the system output; `None` toggles.
    SystemMute { on: Option<bool> },
    /// Device lighting on (last brightness) or off, on every device.
    Lighting { on: bool },
    /// Lighting brightness in percent on every device that has it.
    Brightness { percent: u8 },
    /// Mouse DPI (every connected mouse with a DPI setting).
    Dpi { dpi: u32 },
    /// Speak / show device battery levels.
    Battery,
    /// Media: play/pause, next, previous, stop (MPRIS).
    Media { command: MediaCommand },
    /// A timer that pings and notifies when it ends.
    Timer { seconds: u32, label: Option<String> },
    /// Dictate into the focused field (as if the dictation hotkey was pressed).
    Dictate,
    /// Run the user's command number `index` (`[jarvis] commands`), with the
    /// slot text when its phrase had one.
    User { index: usize, slot: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaCommand {
    PlayPause,
    Next,
    Previous,
    Stop,
}

impl Action {
    /// Short description for the UI and notifications.
    pub fn describe(&self) -> String {
        match self {
            Self::OpenApp { name } => format!("open {name}"),
            Self::OpenUrl { url } => format!("open {url}"),
            Self::Search { query } => format!("search for {query}"),
            Self::Type { text } => format!("type \"{text}\""),
            Self::ApplyProfile { name } => format!("apply profile {name}"),
            Self::StripVolume { strip, percent } => format!("{strip} volume {percent}%"),
            Self::StripMute { strip, on } => format!("{} {strip}", mute_word(*on)),
            Self::MicMute { on } => format!("{} microphone", mute_word(*on)),
            Self::ChatMix { balance } => format!("ChatMix {balance}"),
            Self::Volume { percent } => format!("volume {percent}%"),
            Self::VolumeStep { delta } => format!("volume {delta:+}%"),
            Self::SystemMute { on } => format!("{} sound", mute_word(*on)),
            Self::Lighting { on } => format!("lighting {}", if *on { "on" } else { "off" }),
            Self::Brightness { percent } => format!("brightness {percent}%"),
            Self::Dpi { dpi } => format!("DPI {dpi}"),
            Self::Battery => "battery levels".into(),
            Self::Media { command } => format!("media {command:?}").to_lowercase(),
            Self::Timer { seconds, .. } => format!("timer {}", duration_words(*seconds)),
            Self::Dictate => "dictate".into(),
            Self::User { index, .. } => format!("your command #{}", index + 1),
        }
    }
}

fn mute_word(on: Option<bool>) -> &'static str {
    match on {
        Some(true) => "mute",
        Some(false) => "unmute",
        None => "toggle mute of",
    }
}

/// `90` -> `1 min 30 s`.
pub fn duration_words(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} h"));
    }
    if m > 0 {
        parts.push(format!("{m} min"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s} s"));
    }
    parts.join(" ")
}

/// One built-in action as the UI lists it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActionSpec {
    /// The `action` tag.
    pub name: &'static str,
    pub description: &'static str,
    /// Example phrases (the grammar understands these and close variants).
    pub examples: &'static [&'static str],
    /// `(argument, JSON type, description)`.
    pub args: &'static [(&'static str, &'static str, &'static str)],
}

/// Every built-in action (not `User`, which is per user).
pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        name: "open_app",
        description: "Launch a desktop application",
        examples: &["open firefox", "launch steam", "start discord"],
        args: &[("name", "string", "application name")],
    },
    ActionSpec {
        name: "open_url",
        description: "Open a website or file",
        examples: &["open youtube.com", "go to github.com"],
        args: &[("url", "string", "URL or path")],
    },
    ActionSpec {
        name: "search",
        description: "Search the web",
        examples: &["search for rust lifetimes", "google best headset"],
        args: &[("query", "string", "search terms")],
    },
    ActionSpec {
        name: "type",
        description: "Type text into the focused field",
        examples: &["type hello world"],
        args: &[("text", "string", "text to type")],
    },
    ActionSpec {
        name: "apply_profile",
        description: "Apply a saved hako profile",
        examples: &[
            "gaming profile",
            "apply profile work",
            "switch to streaming",
        ],
        args: &[("name", "string", "profile name")],
    },
    ActionSpec {
        name: "strip_volume",
        description: "Set a mixer strip's volume",
        examples: &["game volume 40", "set music to 70 percent"],
        args: &[
            ("strip", "string", "strip label"),
            ("percent", "integer", "0-100"),
        ],
    },
    ActionSpec {
        name: "strip_mute",
        description: "Mute or unmute a mixer strip",
        examples: &["mute chat", "unmute music"],
        args: &[
            ("strip", "string", "strip label"),
            ("on", "boolean|null", "true mute, false unmute, null toggle"),
        ],
    },
    ActionSpec {
        name: "mic_mute",
        description: "Mute or unmute the microphone",
        examples: &["mute my mic", "unmute microphone"],
        args: &[("on", "boolean|null", "true mute, false unmute, null toggle")],
    },
    ActionSpec {
        name: "chat_mix",
        description: "Set the ChatMix balance between game and chat",
        examples: &["more game", "more chat", "chatmix center"],
        args: &[("balance", "integer", "-100 all game to 100 all chat")],
    },
    ActionSpec {
        name: "volume",
        description: "Set the system volume",
        examples: &["volume 30", "set volume to 50 percent"],
        args: &[("percent", "integer", "0-100")],
    },
    ActionSpec {
        name: "volume_step",
        description: "Turn the system volume up or down",
        examples: &["volume up", "turn it down"],
        args: &[("delta", "integer", "percent change, negative is down")],
    },
    ActionSpec {
        name: "system_mute",
        description: "Mute or unmute the system sound",
        examples: &["mute sound", "unmute"],
        args: &[("on", "boolean|null", "true mute, false unmute, null toggle")],
    },
    ActionSpec {
        name: "lighting",
        description: "Turn device lighting on or off",
        examples: &["lights off", "turn on the lighting"],
        args: &[("on", "boolean", "on or off")],
    },
    ActionSpec {
        name: "brightness",
        description: "Set lighting brightness",
        examples: &["brightness 50"],
        args: &[("percent", "integer", "0-100")],
    },
    ActionSpec {
        name: "dpi",
        description: "Set mouse DPI",
        examples: &["dpi 800", "set sensitivity to 1600"],
        args: &[("dpi", "integer", "dots per inch")],
    },
    ActionSpec {
        name: "battery",
        description: "Report device battery levels",
        examples: &["battery", "how much battery does my headset have"],
        args: &[],
    },
    ActionSpec {
        name: "media",
        description: "Control music and video playback",
        examples: &[
            "pause",
            "play",
            "next song",
            "previous track",
            "stop the music",
        ],
        args: &[("command", "string", "play_pause, next, previous or stop")],
    },
    ActionSpec {
        name: "timer",
        description: "Start a timer",
        examples: &["timer 5 minutes", "set a timer for 30 seconds called tea"],
        args: &[
            ("seconds", "integer", "length"),
            ("label", "string|null", "name"),
        ],
    },
    ActionSpec {
        name: "dictate",
        description: "Start dictating into the focused field",
        examples: &["take a note", "dictate"],
        args: &[],
    },
];

/// The default search when `[jarvis] search_url` is unset.
pub const DEFAULT_SEARCH_URL: &str = "https://duckduckgo.com/?q={q}";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_name_is_an_action_tag() {
        // A spec's name with its args parses as an Action.
        for spec in ACTIONS {
            let mut obj = serde_json::Map::new();
            obj.insert("action".into(), spec.name.into());
            for (arg, kind, _) in spec.args {
                let v = match *kind {
                    "string" | "string|null" => {
                        if spec.name == "media" {
                            "play_pause".into()
                        } else {
                            "x".into()
                        }
                    }
                    "integer" => 1.into(),
                    "boolean" | "boolean|null" => true.into(),
                    _ => serde_json::Value::Null,
                };
                obj.insert((*arg).into(), v);
            }
            let a: Action =
                serde_json::from_value(obj.into()).unwrap_or_else(|e| panic!("{}: {e}", spec.name));
            assert!(!a.describe().is_empty());
        }
    }

    #[test]
    fn durations_read_well() {
        assert_eq!(duration_words(90), "1 min 30 s");
        assert_eq!(duration_words(3600), "1 h");
        assert_eq!(duration_words(0), "0 s");
    }
}
