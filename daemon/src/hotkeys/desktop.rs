//! Desktop shortcuts a hotkey would clash with: GNOME (`gsettings`) and KDE
//! (`kglobalshortcutsrc`). Best effort: other desktops are not checked.

use hako_core::hotkey::{side_neutral, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT};
use hako_core::remap::key_label;
use std::collections::BTreeSet;
use std::process::Command;

/// GNOME schemas that hold global keybindings.
const GNOME_SCHEMAS: [&str; 4] = [
    "org.gnome.desktop.wm.keybindings",
    "org.gnome.settings-daemon.plugins.media-keys",
    "org.gnome.shell.keybindings",
    "org.gnome.mutter.keybindings",
];

/// One desktop shortcut: its name and keys (side-neutral evdev codes).
#[derive(Debug, Clone, PartialEq)]
pub struct Shortcut {
    pub name: String,
    pub keys: BTreeSet<u16>,
}

/// Every shortcut of the running desktop (GNOME / KDE; unknown: both).
pub fn load() -> Vec<Shortcut> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let mut out = Vec::new();
    if desktop.is_empty() || desktop.contains("GNOME") {
        for schema in GNOME_SCHEMAS {
            // `timeout`: a hung tool must not hold a request.
            let listed = Command::new("timeout")
                .args(["2", "gsettings", "list-recursively", schema])
                .output();
            if let Some(o) = listed.ok().filter(|o| o.status.success()) {
                out.extend(parse_gsettings(&String::from_utf8_lossy(&o.stdout)));
            }
        }
    }
    if desktop.is_empty() || desktop.contains("KDE") {
        if let Some(text) = dirs_config("kglobalshortcutsrc") {
            out.extend(parse_kde(&text));
        }
    }
    out
}

fn dirs_config(file: &str) -> Option<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")))?;
    std::fs::read_to_string(dir.join(file)).ok()
}

/// `gsettings list-recursively` lines: `schema key ['<Super>space', ...]`
/// or `schema key '<Primary><Alt>t'`.
fn parse_gsettings(text: &str) -> Vec<Shortcut> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(_), Some(key), Some(value)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // Accelerators are the quoted strings of the value.
        for accel in value.split('\'').skip(1).step_by(2) {
            if let Some(keys) = parse_gtk_accel(accel) {
                out.push(Shortcut {
                    name: format!("{key} (GNOME)"),
                    keys,
                });
            }
        }
    }
    out
}

/// `<Primary><Shift>space` -> keys; None for anything not a key chord.
fn parse_gtk_accel(accel: &str) -> Option<BTreeSet<u16>> {
    let mut keys = BTreeSet::new();
    let mut rest = accel;
    while let Some(tail) = rest.strip_prefix('<') {
        let (modifier, after) = tail.split_once('>')?;
        keys.insert(modifier_code(modifier)?);
        rest = after;
    }
    keys.insert(key_code(rest)?);
    Some(keys)
}

/// `kglobalshortcutsrc`: `[component]` sections of `action=active,default,name`;
/// `active` may list several shortcuts separated by tabs, or `none`.
fn parse_kde(text: &str) -> Vec<Shortcut> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((action, value)) = line.split_once('=') else {
            continue;
        };
        if action.starts_with('_') || line.starts_with('[') {
            continue;
        }
        let fields: Vec<&str> = value.split(',').collect();
        let name = fields.get(2).filter(|n| !n.is_empty()).unwrap_or(&action);
        for shortcut in fields[0].split('\t') {
            if let Some(keys) = parse_kde_shortcut(shortcut) {
                out.push(Shortcut {
                    name: format!("{name} (KDE)"),
                    keys,
                });
            }
        }
    }
    out
}

/// `Meta+Ctrl+Space` -> keys.
fn parse_kde_shortcut(shortcut: &str) -> Option<BTreeSet<u16>> {
    let parts: Vec<&str> = shortcut.trim().split('+').collect();
    let (key, modifiers) = parts.split_last()?;
    let mut keys: BTreeSet<u16> = modifiers
        .iter()
        .map(|m| modifier_code(m))
        .collect::<Option<_>>()?;
    keys.insert(key_code(key)?);
    Some(keys)
}

fn modifier_code(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "primary" | "control" | "ctrl" | "ctl" => KEY_LEFTCTRL,
        "shift" => KEY_LEFTSHIFT,
        "alt" | "mod1" => KEY_LEFTALT,
        "super" | "meta" | "mod4" | "hyper" => KEY_LEFTMETA,
        _ => return None,
    })
}

/// A key name of GTK (`space`, `Page_Up`) or Qt (`Space`, `PgUp`).
fn key_code(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let code = match lower.as_str() {
        "" => return None,
        "space" => 57,
        "escape" | "esc" => 1,
        "return" | "enter" => 28,
        "tab" => 15,
        "backspace" => 14,
        "print" => 99,
        "pause" => 119,
        "home" => 102,
        "end" => 107,
        "page_up" | "prior" | "pgup" => 104,
        "page_down" | "next" | "pgdown" => 109,
        "insert" | "ins" => 110,
        "delete" | "del" => 111,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "menu" => 127,
        "grave" | "`" => 41,
        "minus" | "-" => 12,
        "equal" | "=" => 13,
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
                return match n {
                    1..=10 => Some(58 + n),
                    11 | 12 => Some(76 + n),
                    13..=24 => Some(170 + n),
                    _ => None,
                };
            }
            // Letters and digits: the label is the key's name.
            return (name.len() == 1)
                .then(|| (1..0x80).find(|&c| key_label(c).eq_ignore_ascii_case(name)))
                .flatten();
        }
    };
    Some(side_neutral(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::hotkey::KEY_SPACE;

    #[test]
    fn gnome_accelerators() {
        let text = "org.gnome.desktop.wm.keybindings switch-input-source ['<Super>space', 'XF86Keyboard']\n\
                    org.gnome.desktop.wm.keybindings close ['<Alt>F4']\n\
                    org.gnome.settings-daemon.plugins.media-keys terminal '<Primary><Alt>t'\n\
                    org.gnome.desktop.wm.keybindings minimize @as []\n";
        let got = parse_gsettings(text);
        let names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "switch-input-source (GNOME)",
                "close (GNOME)",
                "terminal (GNOME)"
            ]
        );
        assert_eq!(got[0].keys, BTreeSet::from([KEY_LEFTMETA, KEY_SPACE]));
        assert_eq!(got[1].keys, BTreeSet::from([KEY_LEFTALT, 62]));
        assert_eq!(got[2].keys, BTreeSet::from([KEY_LEFTCTRL, KEY_LEFTALT, 20]));
    }

    #[test]
    fn kde_shortcuts() {
        let text = "[kwin]\n_k_friendly_name=KWin\n\
                    Overview=Meta+W\tCtrl+Space,Meta+W,Toggle Overview\n\
                    Kill Window=none,Meta+Ctrl+Esc,Kill a Window\n\
                    [plasmashell]\nactivate task manager entry 1=Meta+1,Meta+1,\n";
        let got = parse_kde(text);
        let flat: Vec<_> = got
            .iter()
            .map(|s| (s.name.as_str(), s.keys.clone()))
            .collect();
        assert_eq!(
            flat,
            [
                ("Toggle Overview (KDE)", BTreeSet::from([KEY_LEFTMETA, 17])),
                (
                    "Toggle Overview (KDE)",
                    BTreeSet::from([KEY_LEFTCTRL, KEY_SPACE])
                ),
                (
                    "activate task manager entry 1 (KDE)",
                    BTreeSet::from([KEY_LEFTMETA, 2])
                ),
            ]
        );
    }

    #[test]
    fn key_names() {
        assert_eq!(key_code("F13"), Some(183));
        assert_eq!(key_code("F11"), Some(87));
        assert_eq!(key_code("F1"), Some(59));
        assert_eq!(key_code("Page_Up"), Some(104));
        assert_eq!(key_code("a"), Some(30));
        assert_eq!(key_code("XF86AudioPlay"), None);
        assert_eq!(key_code("Left Ctrl"), None);
    }
}
