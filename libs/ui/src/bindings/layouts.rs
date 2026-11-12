//! Keyboard layouts as data: ANSI full-size, TKL, 75% and 60%.

/// One keycap, positioned in key units (1u = one alpha key).
/// `code` 0 = not remappable (Fn is handled in firmware).
#[derive(Clone, Copy, Debug)]
pub struct Cap {
    pub label: &'static str,
    pub code: u16,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// (legend, evdev code, width, gap before), all in key units.
type K = (&'static str, u16, f32, f32);

const FN_ROW: &[K] = &[
    ("Esc", 1, 1.0, 0.0),
    ("F1", 59, 1.0, 1.0),
    ("F2", 60, 1.0, 0.0),
    ("F3", 61, 1.0, 0.0),
    ("F4", 62, 1.0, 0.0),
    ("F5", 63, 1.0, 0.5),
    ("F6", 64, 1.0, 0.0),
    ("F7", 65, 1.0, 0.0),
    ("F8", 66, 1.0, 0.0),
    ("F9", 67, 1.0, 0.5),
    ("F10", 68, 1.0, 0.0),
    ("F11", 87, 1.0, 0.0),
    ("F12", 88, 1.0, 0.0),
];
/// Number row after its first key (` on most boards, Esc on a 60%).
const NUM_ROW: &[K] = &[
    ("1", 2, 1.0, 0.0),
    ("2", 3, 1.0, 0.0),
    ("3", 4, 1.0, 0.0),
    ("4", 5, 1.0, 0.0),
    ("5", 6, 1.0, 0.0),
    ("6", 7, 1.0, 0.0),
    ("7", 8, 1.0, 0.0),
    ("8", 9, 1.0, 0.0),
    ("9", 10, 1.0, 0.0),
    ("0", 11, 1.0, 0.0),
    ("-", 12, 1.0, 0.0),
    ("=", 13, 1.0, 0.0),
    ("Back", 14, 2.0, 0.0),
];
const QWERTY: &[K] = &[
    ("Tab", 15, 1.5, 0.0),
    ("Q", 16, 1.0, 0.0),
    ("W", 17, 1.0, 0.0),
    ("E", 18, 1.0, 0.0),
    ("R", 19, 1.0, 0.0),
    ("T", 20, 1.0, 0.0),
    ("Y", 21, 1.0, 0.0),
    ("U", 22, 1.0, 0.0),
    ("I", 23, 1.0, 0.0),
    ("O", 24, 1.0, 0.0),
    ("P", 25, 1.0, 0.0),
    ("[", 26, 1.0, 0.0),
    ("]", 27, 1.0, 0.0),
    ("\\", 43, 1.5, 0.0),
];
const HOME: &[K] = &[
    ("Caps", 58, 1.75, 0.0),
    ("A", 30, 1.0, 0.0),
    ("S", 31, 1.0, 0.0),
    ("D", 32, 1.0, 0.0),
    ("F", 33, 1.0, 0.0),
    ("G", 34, 1.0, 0.0),
    ("H", 35, 1.0, 0.0),
    ("J", 36, 1.0, 0.0),
    ("K", 37, 1.0, 0.0),
    ("L", 38, 1.0, 0.0),
    (";", 39, 1.0, 0.0),
    ("'", 40, 1.0, 0.0),
    ("Enter", 28, 2.25, 0.0),
];
/// Left Shift through `/`; the right Shift width differs per layout.
const ZROW: &[K] = &[
    ("Shift", 42, 2.25, 0.0),
    ("Z", 44, 1.0, 0.0),
    ("X", 45, 1.0, 0.0),
    ("C", 46, 1.0, 0.0),
    ("V", 47, 1.0, 0.0),
    ("B", 48, 1.0, 0.0),
    ("N", 49, 1.0, 0.0),
    ("M", 50, 1.0, 0.0),
    (",", 51, 1.0, 0.0),
    (".", 52, 1.0, 0.0),
    ("/", 53, 1.0, 0.0),
];
const BOTTOM: &[K] = &[
    ("Ctrl", 29, 1.25, 0.0),
    ("Super", 125, 1.25, 0.0),
    ("Alt", 56, 1.25, 0.0),
    ("", 57, 6.25, 0.0),
    ("Alt", 100, 1.25, 0.0),
    ("Fn", 0, 1.25, 0.0),
    ("Menu", 127, 1.25, 0.0),
    ("Ctrl", 97, 1.25, 0.0),
];
const BOTTOM_75: &[K] = &[
    ("Ctrl", 29, 1.25, 0.0),
    ("Super", 125, 1.25, 0.0),
    ("Alt", 56, 1.25, 0.0),
    ("", 57, 6.25, 0.0),
    ("Alt", 100, 1.0, 0.0),
    ("Fn", 0, 1.0, 0.0),
    ("Ctrl", 97, 1.0, 0.0),
    ("←", 105, 1.0, 0.0),
    ("↓", 108, 1.0, 0.0),
    ("→", 106, 1.0, 0.0),
];
const ARROWS: &[K] = &[
    ("←", 105, 1.0, 0.0),
    ("↓", 108, 1.0, 0.0),
    ("→", 106, 1.0, 0.0),
];

fn row(out: &mut Vec<Cap>, mut x: f32, y: f32, keys: &[K]) {
    for &(label, code, w, gap) in keys {
        x += gap;
        out.push(Cap {
            label,
            code,
            x,
            y,
            w,
            h: 1.0,
        });
        x += w;
    }
}

/// 15u alpha block; `y` is the number row.
fn alphas(out: &mut Vec<Cap>, y: f32, first: K, rshift: f32, bottom: &[K]) {
    row(out, 0.0, y, &[first]);
    row(out, 1.0, y, NUM_ROW);
    row(out, 0.0, y + 1.0, QWERTY);
    row(out, 0.0, y + 2.0, HOME);
    row(out, 0.0, y + 3.0, ZROW);
    row(out, 12.25, y + 3.0, &[("Shift", 54, rshift, 0.0)]);
    row(out, 0.0, y + 4.0, bottom);
}

fn nav_cluster(out: &mut Vec<Cap>, x: f32) {
    row(
        out,
        x,
        0.0,
        &[
            ("PrtSc", 99, 1.0, 0.0),
            ("ScrLk", 70, 1.0, 0.0),
            ("Pause", 119, 1.0, 0.0),
        ],
    );
    row(
        out,
        x,
        1.25,
        &[
            ("Ins", 110, 1.0, 0.0),
            ("Home", 102, 1.0, 0.0),
            ("PgUp", 104, 1.0, 0.0),
        ],
    );
    row(
        out,
        x,
        2.25,
        &[
            ("Del", 111, 1.0, 0.0),
            ("End", 107, 1.0, 0.0),
            ("PgDn", 109, 1.0, 0.0),
        ],
    );
    row(out, x + 1.0, 4.25, &[("↑", 103, 1.0, 0.0)]);
    row(out, x, 5.25, ARROWS);
}

fn numpad(out: &mut Vec<Cap>, x: f32) {
    row(
        out,
        x,
        1.25,
        &[
            ("Num", 69, 1.0, 0.0),
            ("/", 98, 1.0, 0.0),
            ("*", 55, 1.0, 0.0),
            ("-", 74, 1.0, 0.0),
        ],
    );
    row(
        out,
        x,
        2.25,
        &[
            ("7", 71, 1.0, 0.0),
            ("8", 72, 1.0, 0.0),
            ("9", 73, 1.0, 0.0),
        ],
    );
    row(
        out,
        x,
        3.25,
        &[
            ("4", 75, 1.0, 0.0),
            ("5", 76, 1.0, 0.0),
            ("6", 77, 1.0, 0.0),
        ],
    );
    row(
        out,
        x,
        4.25,
        &[
            ("1", 79, 1.0, 0.0),
            ("2", 80, 1.0, 0.0),
            ("3", 81, 1.0, 0.0),
        ],
    );
    row(out, x, 5.25, &[("0", 82, 2.0, 0.0), (".", 83, 1.0, 0.0)]);
    for (label, code, y) in [("+", 78, 2.25), ("Ent", 96, 4.25)] {
        out.push(Cap {
            label,
            code,
            x: x + 3.0,
            y,
            w: 1.0,
            h: 2.0,
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Full,
    Tkl,
    P75,
    P60,
}

impl Layout {
    pub const ALL: [Self; 4] = [Self::Full, Self::Tkl, Self::P75, Self::P60];

    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "Full size",
            Self::Tkl => "TKL",
            Self::P75 => "75%",
            Self::P60 => "60%",
        }
    }

    /// Best guess from the product name; the user can override it.
    // ponytail: name heuristics; a `layout` field on the driver Model would be exact.
    pub fn guess(name: &str) -> Self {
        let name = name.to_lowercase();
        let has = |t: &str| name.split(|c: char| !c.is_alphanumeric()).any(|w| w == t);
        if has("tkl") || has("tenkeyless") || has("core") {
            Self::Tkl
        } else if has("75") || has("65") {
            Self::P75
        } else if has("60") || has("mini") {
            Self::P60
        } else {
            Self::Full
        }
    }

    pub fn caps(self) -> Vec<Cap> {
        let mut out = Vec::with_capacity(104);
        let tilde = ("`", 41, 1.0, 0.0);
        match self {
            Self::Full | Self::Tkl => {
                row(&mut out, 0.0, 0.0, FN_ROW);
                alphas(&mut out, 1.25, tilde, 2.75, BOTTOM);
                nav_cluster(&mut out, 15.25);
                if self == Self::Full {
                    numpad(&mut out, 18.5);
                }
            }
            Self::P75 => {
                let top: Vec<K> = FN_ROW
                    .iter()
                    .map(|&(l, c, w, _)| (l, c, w, 0.0))
                    .chain([
                        ("PrtSc", 99, 1.0, 0.0),
                        ("Pause", 119, 1.0, 0.0),
                        ("Del", 111, 1.0, 0.0),
                    ])
                    .collect();
                row(&mut out, 0.0, 0.0, &top);
                alphas(&mut out, 1.0, tilde, 1.75, BOTTOM_75);
                for (i, (label, code)) in
                    [("Home", 102), ("PgUp", 104), ("PgDn", 109), ("End", 107)]
                        .into_iter()
                        .enumerate()
                {
                    row(&mut out, 15.0, 1.0 + i as f32, &[(label, code, 1.0, 0.0)]);
                }
                row(&mut out, 14.0, 4.0, &[("↑", 103, 1.0, 0.0)]);
            }
            Self::P60 => alphas(&mut out, 0.0, ("Esc", 1, 1.0, 0.0), 2.75, BOTTOM),
        }
        out
    }

    /// Bounding size in key units.
    pub fn size(self) -> (f32, f32) {
        self.caps()
            .iter()
            .fold((0.0, 0.0), |(w, h): (f32, f32), c| {
                (w.max(c.x + c.w), h.max(c.y + c.h))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_have_unique_codes_and_no_overlap() {
        for (layout, keys, size) in [
            (Layout::Full, 103, (22.5, 6.25)),
            (Layout::Tkl, 86, (18.25, 6.25)),
            (Layout::P75, 83, (16.0, 6.0)),
            (Layout::P60, 60, (15.0, 5.0)),
        ] {
            let caps = layout.caps();
            let mut seen = std::collections::HashSet::new();
            for c in caps.iter().filter(|c| c.code != 0) {
                assert!(seen.insert(c.code), "{layout:?}: duplicate {}", c.label);
            }
            assert_eq!(seen.len(), keys, "{layout:?}");
            assert_eq!(layout.size(), size, "{layout:?}");
            for (i, a) in caps.iter().enumerate() {
                for b in &caps[i + 1..] {
                    let apart = a.x + a.w <= b.x + 0.01
                        || b.x + b.w <= a.x + 0.01
                        || a.y + a.h <= b.y + 0.01
                        || b.y + b.h <= a.y + 0.01;
                    assert!(apart, "{layout:?}: {} overlaps {}", a.label, b.label);
                }
            }
        }
    }

    #[test]
    fn layout_guessed_from_product_name() {
        for (name, want) in [
            ("Razer Huntsman V2 TKL", Layout::Tkl),
            ("Corsair K60 PRO TKL", Layout::Tkl),
            ("HyperX Alloy Origins Core", Layout::Tkl),
            ("HyperX Alloy Origins 60", Layout::P60),
            ("Razer Huntsman Mini", Layout::P60),
            ("HyperX Alloy Origins 65", Layout::P75),
            ("Corsair K70 RGB", Layout::Full),
        ] {
            assert_eq!(Layout::guess(name), want, "{name}");
        }
    }
}
