//! Every keyboard and mouse of the machine, from sysfs (nothing is opened):
//! what hotkeys listen to, also devices the hub does not register.

use super::matcher::Node;
use crate::remap::readable;
use std::path::{Path, PathBuf};

/// A `/dev/input/event*` node of a keyboard or mouse.
#[derive(Debug, Clone, PartialEq)]
pub struct InputNode {
    pub path: PathBuf,
    pub node: Node,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    /// This user may open it (seat ACL / group).
    pub readable: bool,
}

/// `/dev/input/event12` -> 12.
pub fn node_number(path: &Path) -> Option<Node> {
    path.file_name()?
        .to_str()?
        .strip_prefix("event")?
        .parse()
        .ok()
}

/// Every keyboard / mouse node except virtual devices named in `skip`.
pub fn scan(skip: &[&str]) -> Vec<InputNode> {
    scan_in(Path::new("/sys/class/input"), skip, readable)
}

/// [`scan`] under the sysfs input class directory `class`.
fn scan_in(class: &Path, skip: &[&str], readable: impl Fn(&Path) -> bool) -> Vec<InputNode> {
    let Ok(dir) = std::fs::read_dir(class) else {
        return Vec::new();
    };
    let mut out: Vec<InputNode> = dir
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let node = file.strip_prefix("event")?.parse().ok()?;
            let dev = e.path().join("device");
            let read = |f: &str| std::fs::read_to_string(dev.join(f)).ok();
            let name = read("name").unwrap_or_default().trim().to_string();
            if skip.contains(&name.as_str())
                || !key_capable(
                    &read("capabilities/key")?,
                    read("capabilities/rel").as_deref(),
                )
            {
                return None;
            }
            let id = |f: &str| {
                read(&format!("id/{f}")).and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
            };
            let path = Path::new("/dev/input").join(&file);
            Some(InputNode {
                readable: readable(&path),
                path,
                node,
                name,
                vendor_id: id("vendor").unwrap_or(0),
                product_id: id("product").unwrap_or(0),
            })
        })
        .collect();
    out.sort_by_key(|n| n.node);
    out
}

/// Bit `bit` of a sysfs capability bitmap (hex words, most significant first).
fn has_bit(bitmap: &str, bit: usize) -> bool {
    let word_bits = 8 * std::mem::size_of::<libc::c_ulong>();
    let words: Vec<&str> = bitmap.split_whitespace().collect();
    let Some(word) = words
        .len()
        .checked_sub(1 + bit / word_bits)
        .map(|i| words[i])
    else {
        return false;
    };
    u64::from_str_radix(word, 16).is_ok_and(|w| w >> (bit % word_bits) & 1 == 1)
}

/// What udev tags `ID_INPUT_KEYBOARD` (`Esc` .. `D` all present) or
/// `ID_INPUT_MOUSE` (`BTN_LEFT` with relative X), so the hako udev rule
/// makes exactly these readable.
fn key_capable(keys: &str, rel: Option<&str>) -> bool {
    let keyboard = (1..=31).all(|k| has_bit(keys, k));
    let mouse = has_bit(keys, 0x110) && rel.is_some_and(|r| has_bit(r, 0));
    keyboard || mouse
}

/// `hakod --diagnose` section: every keyboard / mouse and whether hotkeys
/// (and remaps) can read it.
pub fn report(skip: &[&str]) -> String {
    let mut out = String::from(
        "\n## Keyboards and mice (hotkeys)\n\n\
         | Node | USB ID | Name | Readable |\n|---|---|---|---|\n",
    );
    for n in scan(skip) {
        out += &format!(
            "| {} | `{:04x}:{:04x}` | {} | {} |\n",
            n.path.display(),
            n.vendor_id,
            n.product_id,
            n.name.replace('|', "/"),
            if n.readable { "yes" } else { "**no**" }
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bitmaps() {
        // A full keyboard (low word all ones) and a mouse (BTN_LEFT, REL_X).
        let keyboard = "1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe";
        assert!(key_capable(keyboard, Some("0")));
        let mouse = "1f0000 0 0 0 0";
        assert!(has_bit(mouse, 0x110));
        assert!(key_capable(mouse, Some("143")));
        assert!(!key_capable(mouse, Some("0")), "a touchpad's buttons");
        // Power button: KEY_POWER only.
        assert!(!key_capable("10000000000000 0", None));
        assert!(!has_bit("", 3));
    }

    #[test]
    fn scan_lists_keyboards_and_mice_but_not_our_own_devices() {
        let class = std::env::temp_dir().join(format!("hako-inputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&class);
        let node = |name: &str, dev: &str, keys: &str, rel: &str| {
            let d = class.join(name).join("device");
            std::fs::create_dir_all(d.join("id")).unwrap();
            std::fs::create_dir_all(d.join("capabilities")).unwrap();
            std::fs::write(d.join("name"), format!("{dev}\n")).unwrap();
            std::fs::write(d.join("id/vendor"), "1532\n").unwrap();
            std::fs::write(d.join("id/product"), "026b\n").unwrap();
            std::fs::write(d.join("capabilities/key"), format!("{keys}\n")).unwrap();
            std::fs::write(d.join("capabilities/rel"), format!("{rel}\n")).unwrap();
        };
        node("event12", "Keyboard", "fffffffffffffffe", "0");
        node("event3", "Mouse", "1f0000 0 0 0 0", "3");
        node("event4", "Power Button", "10000000000000 0", "0");
        node("event5", "hako remap", "fffffffffffffffe", "3");
        node("input7", "Keyboard", "fffffffffffffffe", "0");
        let got = scan_in(&class, &["hako remap"], |p| p.ends_with("event12"));
        let _ = std::fs::remove_dir_all(&class);
        let short: Vec<_> = got
            .iter()
            .map(|n| (n.node, n.name.as_str(), n.readable))
            .collect();
        assert_eq!(short, [(3, "Mouse", false), (12, "Keyboard", true)]);
        assert_eq!((got[1].vendor_id, got[1].product_id), (0x1532, 0x026b));
        assert_eq!(got[1].path, PathBuf::from("/dev/input/event12"));
    }
}
