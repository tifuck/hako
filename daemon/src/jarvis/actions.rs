//! Running a resolved [`Action`]. Every side effect goes through [`Env`]
//! (hub requests, programs, notifications, the voice output), so the
//! commands each action makes are tested without running them. Programs
//! get their arguments as argv elements; nothing goes through a shell.

use super::apps::{self, App};
use super::{HubReq, JarvisOut};
use hako_core::catalog::{ControlKind, SettingKey, SettingMeta, SettingValue};
use hako_core::device::DeviceSnapshot;
use hako_core::ipc::{Method, SetParams};
use hako_core::jarvis::{Action, MediaCommand, DEFAULT_SEARCH_URL};
use hako_core::mixer::{MixerOp, MixerState};
use hako_core::voice::{UserAction, VoiceFile};
use serde_json::{json, Value};
use std::collections::HashMap;

/// What actions may do. `Real` in `super`; a recording fake in tests.
pub trait Env: Send {
    /// A request to the hub (the same validation as the UI's).
    fn hub(&mut self, req: HubReq) -> Result<Value, String>;
    /// Run a tool and wait for it (briefly): its stdout.
    fn run(&mut self, argv: &[String]) -> Result<String, String>;
    /// Start a program and leave it running (output ignored).
    fn spawn(&mut self, argv: &[String]) -> Result<(), String>;
    /// A desktop notification (best effort).
    fn notify(&mut self, summary: &str, body: &str);
    /// Something only the voice service can do (type, keys, dictate, pings).
    fn voice(&mut self, out: JarvisOut);
    fn apps(&mut self) -> Vec<App>;
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Percent-encode a search query (RFC 3986 unreserved characters kept).
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `[program, args...]` of a user `Run`, `{slot}` replaced inside each
/// argument (one argument stays one argument, whatever the slot holds).
pub fn user_argv(program: &str, args: &[String], slot: Option<&str>) -> Vec<String> {
    std::iter::once(program.to_string())
        .chain(args.iter().map(|a| fill(a, slot)))
        .collect()
}

fn fill(template: &str, slot: Option<&str>) -> String {
    template.replace("{slot}", slot.unwrap_or(""))
}

/// Remembers what "lights on" brings back.
#[derive(Default)]
pub struct Actions {
    brightness: HashMap<String, SettingValue>,
}

impl Actions {
    /// Run `action` (not `Timer`, which the runner keeps): a short message.
    pub fn run(
        &mut self,
        env: &mut dyn Env,
        action: &Action,
        file: &VoiceFile,
    ) -> Result<String, String> {
        match action {
            Action::OpenApp { name } => {
                let apps = env.apps();
                let app = apps::pick(&apps, name).ok_or(format!("No app called {name}"))?;
                let path = app.path.to_string_lossy().into_owned();
                env.spawn(&argv(&["gio", "launch", &path]))
                    .or_else(|_| env.spawn(&argv(&["gtk-launch", &app.id])))?;
                Ok(format!("Opened {}", app.name))
            }
            Action::OpenUrl { url } => {
                open(env, url)?;
                Ok(format!("Opened {url}"))
            }
            Action::Search { query } => {
                let base = file
                    .jarvis
                    .search_url
                    .as_deref()
                    .unwrap_or(DEFAULT_SEARCH_URL);
                env.spawn(&argv(&[
                    "xdg-open",
                    &base.replace("{q}", &url_encode(query)),
                ]))?;
                Ok(format!("Searched for {query}"))
            }
            Action::Type { text } => {
                env.voice(JarvisOut::Type(text.clone()));
                Ok("Typed".into())
            }
            Action::ApplyProfile { name } => apply_profile(env, name),
            Action::StripVolume { strip, percent } => {
                let (id, input, label) = find_strip(env, strip)?;
                let gain_db = percent_to_db(*percent);
                let op = if input {
                    MixerOp::SetInputGain { input: id, gain_db }
                } else {
                    MixerOp::SetOutputGain {
                        output: id,
                        gain_db,
                    }
                };
                patch_mixer(env, op)?;
                Ok(format!("{label} volume {percent}%"))
            }
            Action::StripMute { strip, on } => {
                let state = mixer(env)?;
                let (id, input, label) = strip_in(&state, strip)?;
                let now = if input {
                    state
                        .file
                        .live
                        .inputs
                        .iter()
                        .find(|i| i.id == id)
                        .is_some_and(|i| i.mute)
                } else {
                    state
                        .file
                        .live
                        .outputs
                        .iter()
                        .find(|o| o.id == id)
                        .is_some_and(|o| o.mute)
                };
                let mute = on.unwrap_or(!now);
                let op = if input {
                    MixerOp::SetInputMute { input: id, mute }
                } else {
                    MixerOp::SetOutputMute { output: id, mute }
                };
                patch_mixer(env, op)?;
                Ok(format!(
                    "{} {label}",
                    if mute { "Muted" } else { "Unmuted" }
                ))
            }
            Action::MicMute { on } => {
                let devices = devices(env)?;
                let mics: Vec<_> = devices
                    .iter()
                    .filter_map(|d| control(d, SettingKey::MicMute).map(|m| (d, m)))
                    .collect();
                if mics.is_empty() {
                    let arg = on.map_or("toggle", |m| if m { "1" } else { "0" });
                    env.run(&argv(&["wpctl", "set-mute", "@DEFAULT_AUDIO_SOURCE@", arg]))?;
                    return Ok(mute_message("Microphone", *on));
                }
                let mut said = None;
                for (d, _) in mics {
                    let now = current(d, SettingKey::MicMute)
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let mute = on.unwrap_or(!now);
                    set(env, d, SettingKey::MicMute, SettingValue::Bool(mute))?;
                    said = Some(mute);
                }
                Ok(mute_message("Microphone", said.or(*on)))
            }
            Action::ChatMix { balance } => {
                let b = i32::from(*balance).clamp(-100, 100);
                let devices = devices(env)?;
                let mut done = false;
                for d in &devices {
                    if let Some(meta) = control(d, SettingKey::ChatMixBalance) {
                        // The device's slider: low = game, high = chat.
                        let (lo, hi) = (meta.min.unwrap_or(0.0), meta.max.unwrap_or(100.0));
                        let n = lo + (hi - lo) * f64::from(b + 100) / 200.0;
                        if let Some(v) = value_for(meta, n) {
                            set(env, d, SettingKey::ChatMixBalance, v)?;
                            done = true;
                        }
                    }
                }
                if !done {
                    let (game, chat) = dial(b);
                    env.hub(HubReq::ChatMix { game, chat })?;
                }
                Ok(format!("ChatMix {b}"))
            }
            Action::Volume { percent } => {
                let level = format!("{:.2}", f32::from((*percent).min(100)) / 100.0);
                env.run(&argv(&[
                    "wpctl",
                    "set-volume",
                    "@DEFAULT_AUDIO_SINK@",
                    &level,
                ]))?;
                Ok(format!("Volume {percent}%"))
            }
            Action::VolumeStep { delta } => {
                let step = format!(
                    "{}%{}",
                    delta.unsigned_abs(),
                    if *delta < 0 { "-" } else { "+" }
                );
                env.run(&argv(&[
                    "wpctl",
                    "set-volume",
                    "-l",
                    "1.0",
                    "@DEFAULT_AUDIO_SINK@",
                    &step,
                ]))?;
                Ok(format!("Volume {}", if *delta < 0 { "down" } else { "up" }))
            }
            Action::SystemMute { on } => {
                let arg = on.map_or("toggle", |m| if m { "1" } else { "0" });
                env.run(&argv(&["wpctl", "set-mute", "@DEFAULT_AUDIO_SINK@", arg]))?;
                Ok(mute_message("Sound", *on))
            }
            Action::Lighting { on } => {
                let devices = devices(env)?;
                let mut n = 0;
                for d in &devices {
                    let Some(meta) = control(d, SettingKey::Brightness) else {
                        continue;
                    };
                    let key = d.id.key();
                    let value = if *on {
                        self.brightness
                            .get(&key)
                            .cloned()
                            .or_else(|| value_for(meta, meta.max.unwrap_or(100.0)))
                    } else {
                        if let Some(v) = current(d, SettingKey::Brightness) {
                            if v.as_float().is_some_and(|f| f > meta.min.unwrap_or(0.0)) {
                                self.brightness.insert(key, v.clone());
                            }
                        }
                        value_for(meta, meta.min.unwrap_or(0.0))
                    };
                    if let Some(v) = value {
                        set(env, d, SettingKey::Brightness, v)?;
                        n += 1;
                    }
                }
                if n == 0 {
                    return Err("No device with lighting is connected".into());
                }
                Ok(format!("Lights {}", if *on { "on" } else { "off" }))
            }
            Action::Brightness { percent } => {
                let devices = devices(env)?;
                let mut n = 0;
                for d in &devices {
                    let Some(meta) = control(d, SettingKey::Brightness) else {
                        continue;
                    };
                    let (lo, hi) = (meta.min.unwrap_or(0.0), meta.max.unwrap_or(100.0));
                    if let Some(v) = value_for(meta, lo + (hi - lo) * f64::from(*percent) / 100.0) {
                        set(env, d, SettingKey::Brightness, v)?;
                        n += 1;
                    }
                }
                if n == 0 {
                    return Err("No device with lighting is connected".into());
                }
                Ok(format!("Brightness {percent}%"))
            }
            Action::Dpi { dpi } => {
                let devices = devices(env)?;
                let mut set_to = None;
                for d in &devices {
                    let Some(meta) = control(d, SettingKey::Dpi) else {
                        continue;
                    };
                    if let Some(v) = value_for(meta, f64::from(*dpi)) {
                        set(env, d, SettingKey::Dpi, v.clone())?;
                        set_to = v.as_int();
                    }
                }
                let got = set_to.ok_or("No mouse with a DPI setting is connected")?;
                Ok(format!("DPI {got}"))
            }
            Action::Battery => {
                let devices = devices(env)?;
                let levels: Vec<String> = devices
                    .iter()
                    .filter(|d| d.connected)
                    .flat_map(|d| {
                        d.batteries.iter().filter(|b| b.present).filter_map(|b| {
                            let label = if b.label.is_empty() || b.label == d.name {
                                d.name.clone()
                            } else {
                                format!("{} {}", d.name, b.label)
                            };
                            b.percent.map(|p| {
                                format!(
                                    "{label} {p}%{}",
                                    if b.charging { " (charging)" } else { "" }
                                )
                            })
                        })
                    })
                    .collect();
                if levels.is_empty() {
                    return Err("No battery levels are known".into());
                }
                let text = levels.join(", ");
                env.notify("Battery", &text);
                Ok(text)
            }
            Action::Media { command } => media(env, *command),
            Action::Timer { .. } => Err("timers run in the Jarvis service".into()),
            Action::Dictate => {
                env.voice(JarvisOut::Dictate);
                Ok("Dictating".into())
            }
            Action::User { index, slot } => {
                let c = file
                    .jarvis
                    .commands
                    .get(*index)
                    .ok_or("That command no longer exists")?;
                let slot = slot.as_deref();
                let said = c.say.first().cloned().unwrap_or_default();
                match &c.action {
                    UserAction::Run { program, args } => {
                        if !program.starts_with('/') {
                            return Err("The program must be an absolute path".into());
                        }
                        env.spawn(&user_argv(program, args, slot))?;
                    }
                    UserAction::Keys { keys } => {
                        env.voice(JarvisOut::Keys(keys.iter().map(|k| k.0).collect()));
                    }
                    UserAction::Type { text } => env.voice(JarvisOut::Type(fill(text, slot))),
                    UserAction::Open { target } => open(env, &fill(target, slot))?,
                    UserAction::Profile { name } => return apply_profile(env, &fill(name, slot)),
                }
                Ok(format!("Ran \"{said}\""))
            }
        }
    }
}

fn mute_message(what: &str, on: Option<bool>) -> String {
    match on {
        Some(true) => format!("{what} muted"),
        Some(false) => format!("{what} unmuted"),
        None => format!("{what} mute toggled"),
    }
}

/// `xdg-open` for http(s) URLs and paths only.
fn open(env: &mut dyn Env, target: &str) -> Result<(), String> {
    let t = target.trim();
    let ok = t.starts_with("https://")
        || t.starts_with("http://")
        || t.starts_with("file://")
        || t.starts_with('/')
        || t.starts_with("~/");
    if !ok {
        return Err(format!("Not a web address or file: {t}"));
    }
    let t = match t.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME").map_or(t.to_string(), |h| format!("{h}/{rest}")),
        None => t.to_string(),
    };
    env.spawn(&argv(&["xdg-open", &t]))
}

fn apply_profile(env: &mut dyn Env, name: &str) -> Result<String, String> {
    env.hub(HubReq::Ipc(Method::ApplyProfile, json!({ "name": name })))?;
    Ok(format!("Applied profile {name}"))
}

/// ChatMix balance (-100 game .. 100 chat) as the headset dial (game, chat).
pub fn dial(balance: i32) -> (f32, f32) {
    let b = balance.clamp(-100, 100) as f32 / 100.0;
    if b >= 0.0 {
        (1.0 - b, 1.0)
    } else {
        (1.0, 1.0 + b)
    }
}

/// Spoken strip volume (100% = 0 dB, the fader's unity mark) as gain: the
/// UI fader's taper below 0 dB, so "50" lands where the fader's midpoint
/// to unity is.
pub fn percent_to_db(percent: u8) -> f32 {
    const TAPER: [(f32, f32); 5] = [
        (hako_core::mixer::GAIN_MIN_DB, 0.0),
        (-40.0, 0.12),
        (-20.0, 0.36),
        (-10.0, 0.56),
        (0.0, 0.8),
    ];
    let pos = f32::from(percent.min(100)) / 100.0 * 0.8;
    let db = TAPER.windows(2).find(|w| pos <= w[1].1).map_or(0.0, |w| {
        w[0].0 + (pos - w[0].1) / (w[1].1 - w[0].1) * (w[1].0 - w[0].0)
    });
    (db * 10.0).round() / 10.0
}

/// A connected device reports its microphone muted (the headset's switch).
pub fn mic_muted(devices: &[DeviceSnapshot]) -> bool {
    devices.iter().any(|d| {
        d.connected && current(d, SettingKey::MicMute).and_then(SettingValue::as_bool) == Some(true)
    })
}

pub fn devices(env: &mut dyn Env) -> Result<Vec<DeviceSnapshot>, String> {
    let v = env.hub(HubReq::Ipc(Method::GetSnapshot, Value::Null))?;
    serde_json::from_value(v).map_err(|e| format!("device list: {e}"))
}

/// The writable control `key` of a connected device.
fn control(d: &DeviceSnapshot, key: SettingKey) -> Option<&SettingMeta> {
    d.connected
        .then(|| {
            d.controls.iter().find(|c| {
                c.key == key
                    && !c.read_only
                    && c.kind != ControlKind::ReadOnly
                    && c.blocked.is_none()
            })
        })
        .flatten()
}

fn current(d: &DeviceSnapshot, key: SettingKey) -> Option<&SettingValue> {
    d.settings.get(key.as_str())
}

/// The value closest to `n` the control accepts (a slider snapped onto its
/// grid, or the nearest dropdown choice).
fn value_for(meta: &SettingMeta, n: f64) -> Option<SettingValue> {
    match meta.kind {
        ControlKind::Dropdown => meta
            .options
            .iter()
            .filter_map(|o| Some((o.value.as_float()?, &o.value)))
            .min_by(|a, b| (a.0 - n).abs().total_cmp(&(b.0 - n).abs()))
            .map(|(_, v)| v.clone()),
        ControlKind::Slider | ControlKind::ColorChannel => {
            let v = SettingValue::Float(n);
            if meta.validate(&v).is_ok() {
                Some(v)
            } else {
                meta.nearest_valid(&v)
            }
        }
        _ => None,
    }
}

fn set(
    env: &mut dyn Env,
    d: &DeviceSnapshot,
    key: SettingKey,
    value: SettingValue,
) -> Result<(), String> {
    let params = SetParams {
        device_id: d.id.clone(),
        key,
        value,
    };
    let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
    env.hub(HubReq::Ipc(Method::Set, params))
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", d.name))
}

fn mixer(env: &mut dyn Env) -> Result<MixerState, String> {
    let v = env.hub(HubReq::Ipc(Method::GetMixer, Value::Null))?;
    serde_json::from_value(v).map_err(|e| format!("mixer: {e}"))
}

fn patch_mixer(env: &mut dyn Env, op: MixerOp) -> Result<(), String> {
    env.hub(HubReq::Ipc(Method::PatchMixer, json!({ "ops": [op] })))
        .map(|_| ())
}

/// (id, is an input, label) of the strip `name` says.
fn strip_in(state: &MixerState, name: &str) -> Result<(String, bool, String), String> {
    let live = &state.file.live;
    let labels: Vec<String> = live
        .inputs
        .iter()
        .map(|i| i.label.clone())
        .chain(live.outputs.iter().map(|o| o.label.clone()))
        .collect();
    let label =
        super::intent::pick(name, &labels).ok_or(format!("No mixer strip called {name}"))?;
    if let Some(i) = live.inputs.iter().find(|i| &i.label == label) {
        return Ok((i.id.clone(), true, i.label.clone()));
    }
    let o = live
        .outputs
        .iter()
        .find(|o| &o.label == label)
        .ok_or(format!("No mixer strip called {name}"))?;
    Ok((o.id.clone(), false, o.label.clone()))
}

fn find_strip(env: &mut dyn Env, name: &str) -> Result<(String, bool, String), String> {
    strip_in(&mixer(env)?, name)
}

/// The labels the grammar resolves strip names against.
pub fn strip_labels(state: &MixerState) -> Vec<String> {
    let live = &state.file.live;
    live.inputs
        .iter()
        .map(|i| i.label.clone())
        .chain(live.outputs.iter().map(|o| o.label.clone()))
        .collect()
}

/// `org.mpris.MediaPlayer2.*` names in a `ListNames` reply.
pub fn mpris_players(list_names: &str) -> Vec<String> {
    list_names
        .lines()
        .filter_map(|l| l.trim().strip_prefix("string \""))
        .filter_map(|s| s.strip_suffix('"'))
        .filter(|s| s.starts_with("org.mpris.MediaPlayer2."))
        .map(str::to_string)
        .collect()
}

fn media(env: &mut dyn Env, command: MediaCommand) -> Result<String, String> {
    let names = env.run(&argv(&[
        "dbus-send",
        "--session",
        "--print-reply",
        "--dest=org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.ListNames",
    ]))?;
    let players = mpris_players(&names);
    let playing = players.iter().find(|p| {
        env.run(&argv(&[
            "dbus-send",
            "--session",
            "--print-reply",
            &format!("--dest={p}"),
            "/org/mpris/MediaPlayer2",
            "org.freedesktop.DBus.Properties.Get",
            "string:org.mpris.MediaPlayer2.Player",
            "string:PlaybackStatus",
        ]))
        .is_ok_and(|s| s.contains("\"Playing\""))
    });
    let player = playing
        .or(players.first())
        .ok_or("No media player is running")?;
    let method = match command {
        MediaCommand::PlayPause => "PlayPause",
        MediaCommand::Next => "Next",
        MediaCommand::Previous => "Previous",
        MediaCommand::Stop => "Stop",
    };
    env.run(&argv(&[
        "dbus-send",
        "--session",
        "--type=method_call",
        &format!("--dest={player}"),
        "/org/mpris/MediaPlayer2",
        &format!("org.mpris.MediaPlayer2.Player.{method}"),
    ]))?;
    let app = player.trim_start_matches("org.mpris.MediaPlayer2.");
    Ok(format!("{method} ({app})"))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use hako_core::catalog::SettingOption;
    use hako_core::device::{BatteryState, DeviceId, DeviceKind};
    use hako_core::remap::EvdevKey;
    use hako_core::voice::UserCommand;
    use std::path::PathBuf;

    /// Records every effect; answers hub requests from `devices` / `mixer`.
    #[derive(Default)]
    pub struct Fake {
        pub hub: Vec<(Method, Value)>,
        pub chatmix: Vec<(f32, f32)>,
        pub ran: Vec<Vec<String>>,
        pub spawned: Vec<Vec<String>>,
        pub notes: Vec<(String, String)>,
        pub voice: Vec<JarvisOut>,
        pub devices: Vec<DeviceSnapshot>,
        pub mixer: MixerState,
        pub profiles: Vec<String>,
        pub apps: Vec<App>,
        /// `dbus-send` answers.
        pub players: String,
        pub playing: Option<String>,
    }

    impl Env for Fake {
        fn hub(&mut self, req: HubReq) -> Result<Value, String> {
            match req {
                HubReq::ChatMix { game, chat } => {
                    self.chatmix.push((game, chat));
                    Ok(Value::Null)
                }
                HubReq::Ipc(method, params) => {
                    self.hub.push((method, params.clone()));
                    match method {
                        Method::GetSnapshot => Ok(serde_json::to_value(&self.devices).unwrap()),
                        Method::GetMixer => Ok(serde_json::to_value(&self.mixer).unwrap()),
                        Method::ListProfiles => Ok(json!({ "profiles": self.profiles })),
                        Method::ApplyProfile if params["name"] == "missing" => {
                            Err("profile not found".into())
                        }
                        _ => Ok(Value::Null),
                    }
                }
            }
        }
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.ran.push(argv.to_vec());
            if argv.iter().any(|a| a == "org.freedesktop.DBus.ListNames") {
                return Ok(self.players.clone());
            }
            if argv.iter().any(|a| a == "string:PlaybackStatus") {
                let playing = self
                    .playing
                    .as_ref()
                    .is_some_and(|p| argv.contains(&format!("--dest={p}")));
                return Ok(if playing {
                    "variant string \"Playing\""
                } else {
                    "variant string \"Paused\""
                }
                .into());
            }
            Ok(String::new())
        }
        fn spawn(&mut self, argv: &[String]) -> Result<(), String> {
            self.spawned.push(argv.to_vec());
            Ok(())
        }
        fn notify(&mut self, summary: &str, body: &str) {
            self.notes.push((summary.into(), body.into()));
        }
        fn voice(&mut self, out: JarvisOut) {
            self.voice.push(out);
        }
        fn apps(&mut self) -> Vec<App> {
            self.apps.clone()
        }
    }

    pub fn device(
        name: &str,
        kind: DeviceKind,
        controls: Vec<SettingMeta>,
        settings: &[(SettingKey, SettingValue)],
    ) -> DeviceSnapshot {
        DeviceSnapshot {
            id: DeviceId {
                vendor_id: 0x1234,
                product_id: name.len() as u16,
                instance: name.into(),
            },
            kind,
            name: name.into(),
            connected: true,
            batteries: Vec::new(),
            settings: settings
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.clone()))
                .collect(),
            controls,
            last_error: None,
        }
    }

    fn sets(f: &Fake) -> Vec<(String, Value)> {
        f.hub
            .iter()
            .filter(|(m, _)| *m == Method::Set)
            .map(|(_, p)| {
                (
                    p["key"].as_str().unwrap().to_string(),
                    p["value"]["v"].clone(),
                )
            })
            .collect()
    }

    fn run(f: &mut Fake, a: Action) -> Result<String, String> {
        Actions::default().run(f, &a, &VoiceFile::default())
    }

    #[test]
    fn device_settings_go_through_the_hub_onto_each_controls_grid() {
        let keyboard = device(
            "Keyboard",
            DeviceKind::Keyboard,
            vec![SettingMeta::slider(
                SettingKey::Brightness,
                "Brightness",
                0.0,
                255.0,
                1.0,
            )],
            &[(SettingKey::Brightness, SettingValue::Int(200))],
        );
        let mouse = device(
            "Mouse",
            DeviceKind::Mouse,
            vec![
                SettingMeta::slider(SettingKey::Dpi, "DPI", 100.0, 25600.0, 50.0),
                SettingMeta::slider(SettingKey::Brightness, "Brightness", 0.0, 100.0, 10.0)
                    .blocked("not confirmed"),
            ],
            &[],
        );
        let headset = device(
            "Headset",
            DeviceKind::Headset,
            vec![
                SettingMeta::toggle(SettingKey::MicMute, "Mic mute"),
                SettingMeta::dropdown(
                    SettingKey::ChatMixBalance,
                    "Balance",
                    (0..=10).map(|i| SettingOption::int(i * 10, "x")).collect(),
                ),
            ],
            &[(SettingKey::MicMute, SettingValue::Bool(false))],
        );
        let mut f = Fake {
            devices: vec![keyboard, mouse, headset],
            ..Fake::default()
        };
        let mut actions = Actions::default();
        let file = VoiceFile::default();
        assert_eq!(
            actions.run(&mut f, &Action::Brightness { percent: 50 }, &file),
            Ok("Brightness 50%".into())
        );
        assert_eq!(
            actions.run(&mut f, &Action::Dpi { dpi: 1625 }, &file),
            Ok("DPI 1650".into())
        );
        assert_eq!(
            actions.run(&mut f, &Action::MicMute { on: None }, &file),
            Ok("Microphone muted".into())
        );
        assert_eq!(
            actions.run(&mut f, &Action::ChatMix { balance: 40 }, &file),
            Ok("ChatMix 40".into())
        );
        assert_eq!(
            sets(&f),
            [
                (s("brightness"), json!(128)),
                (s("dpi"), json!(1650)),
                (s("mic_mute"), json!(true)),
                (s("chat_mix_balance"), json!(70)),
            ]
        );
        assert!(f.chatmix.is_empty(), "the headset's own balance is used");

        // Off remembers the level, on brings it back (else the maximum).
        f.hub.clear();
        actions
            .run(&mut f, &Action::Lighting { on: false }, &file)
            .unwrap();
        actions
            .run(&mut f, &Action::Lighting { on: true }, &file)
            .unwrap();
        assert_eq!(
            sets(&f),
            [(s("brightness"), json!(0)), (s("brightness"), json!(200))]
        );

        // No headset balance: the ChatMix dial; no mic control: wpctl.
        let mut f = Fake::default();
        run(&mut f, Action::ChatMix { balance: -100 }).unwrap();
        run(&mut f, Action::MicMute { on: Some(false) }).unwrap();
        assert_eq!(f.chatmix, [(1.0, 0.0)]);
        assert_eq!(
            f.ran,
            [argv(&["wpctl", "set-mute", "@DEFAULT_AUDIO_SOURCE@", "0"])]
        );
        assert!(run(&mut f, Action::Dpi { dpi: 800 }).is_err());
        assert!(run(&mut f, Action::Lighting { on: true }).is_err());
    }

    fn s(x: &str) -> String {
        x.to_string()
    }

    #[test]
    fn mixer_strips_by_label() {
        use hako_core::mixer::{Input, InputKind};
        let mut f = Fake::default();
        f.mixer.file.live.inputs.push(Input {
            id: "game".into(),
            label: "Game".into(),
            kind: InputKind::Virtual {
                node_name: "Hako_Game".into(),
                apps: Vec::new(),
            },
            chain: Default::default(),
            gain_db: 0.0,
            mute: true,
            solo: false,
            routes: Vec::new(),
            chatmix: None,
        });
        assert_eq!(
            run(
                &mut f,
                Action::StripVolume {
                    strip: "game".into(),
                    percent: 100
                }
            ),
            Ok("Game volume 100%".into())
        );
        assert_eq!(
            run(
                &mut f,
                Action::StripMute {
                    strip: "Game".into(),
                    on: None
                }
            ),
            Ok("Unmuted Game".into())
        );
        let patches: Vec<MixerOp> = f
            .hub
            .iter()
            .filter(|(m, _)| *m == Method::PatchMixer)
            .map(|(_, p)| serde_json::from_value(p["ops"][0].clone()).unwrap())
            .collect();
        assert_eq!(
            patches,
            [
                MixerOp::SetInputGain {
                    input: s("game"),
                    gain_db: 0.0
                },
                MixerOp::SetInputMute {
                    input: s("game"),
                    mute: false
                },
            ]
        );
        assert!(run(
            &mut f,
            Action::StripMute {
                strip: "music".into(),
                on: None
            }
        )
        .is_err());
        assert_eq!(percent_to_db(100), 0.0);
        assert_eq!(percent_to_db(70), -10.0);
        assert_eq!(percent_to_db(0), hako_core::mixer::GAIN_MIN_DB);
        assert!(percent_to_db(40) < percent_to_db(50));
        assert_eq!(dial(0), (1.0, 1.0));
        assert_eq!(dial(50), (0.5, 1.0));
    }

    #[test]
    fn system_audio_media_and_web_use_their_tools_with_plain_arguments() {
        let mut f = Fake {
            players: "array [\n string \"org.freedesktop.DBus\"\n string \"org.mpris.MediaPlayer2.firefox.instance_1\"\n string \"org.mpris.MediaPlayer2.spotify\"\n]".into(),
            playing: Some("org.mpris.MediaPlayer2.spotify".into()),
            ..Fake::default()
        };
        run(&mut f, Action::Volume { percent: 40 }).unwrap();
        run(&mut f, Action::VolumeStep { delta: -5 }).unwrap();
        run(&mut f, Action::SystemMute { on: None }).unwrap();
        assert_eq!(
            run(
                &mut f,
                Action::Media {
                    command: MediaCommand::Next
                }
            ),
            Ok("Next (spotify)".into())
        );
        assert_eq!(
            f.ran[0],
            argv(&["wpctl", "set-volume", "@DEFAULT_AUDIO_SINK@", "0.40"])
        );
        assert_eq!(
            f.ran[1],
            argv(&[
                "wpctl",
                "set-volume",
                "-l",
                "1.0",
                "@DEFAULT_AUDIO_SINK@",
                "5%-"
            ])
        );
        assert_eq!(
            f.ran[2],
            argv(&["wpctl", "set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
        );
        assert_eq!(
            f.ran.last().unwrap(),
            &argv(&[
                "dbus-send",
                "--session",
                "--type=method_call",
                "--dest=org.mpris.MediaPlayer2.spotify",
                "/org/mpris/MediaPlayer2",
                "org.mpris.MediaPlayer2.Player.Next",
            ])
        );

        let mut file = VoiceFile::default();
        let mut actions = Actions::default();
        actions
            .run(
                &mut f,
                &Action::Search {
                    query: "rust & c++ ?".into(),
                },
                &file,
            )
            .unwrap();
        file.jarvis.search_url = Some("https://example.org/s?x={q}".into());
        actions
            .run(
                &mut f,
                &Action::Search {
                    query: "a b".into(),
                },
                &file,
            )
            .unwrap();
        actions
            .run(
                &mut f,
                &Action::OpenUrl {
                    url: "https://github.com".into(),
                },
                &file,
            )
            .unwrap();
        assert!(actions
            .run(
                &mut f,
                &Action::OpenUrl {
                    url: "-e rm".into()
                },
                &file
            )
            .is_err());
        assert_eq!(
            f.spawned,
            [
                argv(&[
                    "xdg-open",
                    "https://duckduckgo.com/?q=rust%20%26%20c%2B%2B%20%3F"
                ]),
                argv(&["xdg-open", "https://example.org/s?x=a%20b"]),
                argv(&["xdg-open", "https://github.com"]),
            ]
        );
        let mut none = Fake::default();
        assert!(run(
            &mut none,
            Action::Media {
                command: MediaCommand::PlayPause
            }
        )
        .is_err());
    }

    #[test]
    fn apps_launch_by_desktop_file() {
        let mut f = Fake {
            apps: vec![App {
                id: "firefox.desktop".into(),
                path: PathBuf::from("/usr/share/applications/firefox.desktop"),
                name: "Firefox".into(),
                generic: None,
                keywords: Vec::new(),
            }],
            ..Fake::default()
        };
        assert_eq!(
            run(
                &mut f,
                Action::OpenApp {
                    name: "firefox".into()
                }
            ),
            Ok("Opened Firefox".into())
        );
        assert_eq!(
            f.spawned,
            [argv(&[
                "gio",
                "launch",
                "/usr/share/applications/firefox.desktop"
            ])]
        );
        assert!(run(
            &mut f,
            Action::OpenApp {
                name: "steam".into()
            }
        )
        .is_err());
    }

    #[test]
    fn user_commands_pass_the_slot_as_one_argument() {
        assert_eq!(
            user_argv(
                "/usr/bin/kitty",
                &[s("-e"), s("{slot}"), s("x={slot}")],
                Some("htop; rm -rf ~")
            ),
            [
                s("/usr/bin/kitty"),
                s("-e"),
                s("htop; rm -rf ~"),
                s("x=htop; rm -rf ~")
            ]
        );
        assert_eq!(
            user_argv("/bin/true", &[s("{slot}")], None),
            [s("/bin/true"), s("")]
        );

        let mut file = VoiceFile::default();
        let cmd = |action| UserCommand {
            say: vec![s("go {x}")],
            action,
            confirm: false,
        };
        file.jarvis.commands = vec![
            cmd(UserAction::Run {
                program: s("/usr/bin/kitty"),
                args: vec![s("{slot}")],
            }),
            cmd(UserAction::Keys {
                keys: vec![EvdevKey(29), EvdevKey(46)],
            }),
            cmd(UserAction::Type {
                text: s("hi {slot}"),
            }),
            cmd(UserAction::Open {
                target: s("https://x.org/{slot}"),
            }),
            cmd(UserAction::Profile { name: s("{slot}") }),
        ];
        let mut f = Fake::default();
        let mut a = Actions::default();
        for index in 0..5 {
            let r = a.run(
                &mut f,
                &Action::User {
                    index,
                    slot: Some(s("two words")),
                },
                &file,
            );
            assert!(r.is_ok(), "{index}: {r:?}");
        }
        assert!(a
            .run(
                &mut f,
                &Action::User {
                    index: 9,
                    slot: None
                },
                &file
            )
            .is_err());
        assert_eq!(
            f.spawned,
            [
                argv(&["/usr/bin/kitty", "two words"]),
                argv(&["xdg-open", "https://x.org/two words"])
            ]
        );
        assert_eq!(
            f.voice,
            [
                JarvisOut::Keys(vec![29, 46]),
                JarvisOut::Type(s("hi two words"))
            ]
        );
        assert_eq!(
            f.hub.last().unwrap(),
            &(Method::ApplyProfile, json!({"name": "two words"}))
        );
    }

    #[test]
    fn battery_levels_are_read_from_the_snapshots() {
        let mut d = device("Arctis", DeviceKind::Headset, Vec::new(), &[]);
        d.batteries = vec![
            BatteryState {
                label: s("Headset"),
                percent: Some(80),
                charging: false,
                present: true,
            },
            BatteryState {
                label: s("Spare"),
                percent: Some(100),
                charging: true,
                present: true,
            },
            BatteryState {
                label: s("Gone"),
                percent: None,
                charging: false,
                present: false,
            },
        ];
        let mut f = Fake {
            devices: vec![d],
            ..Fake::default()
        };
        assert_eq!(
            run(&mut f, Action::Battery),
            Ok(s("Arctis Headset 80%, Arctis Spare 100% (charging)"))
        );
        assert_eq!(f.notes.len(), 1);
        assert!(run(&mut Fake::default(), Action::Battery).is_err());
    }
}
