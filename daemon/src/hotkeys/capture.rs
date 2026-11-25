//! Detect: record the chord the user presses (`StartHotkeyCapture`).

use hako_core::hotkey::{is_modifier, HotkeyCaptureState, KEY_ESC, MAX_CHORD_KEYS};
use hako_core::remap::EvdevKey;
use std::time::{Duration, Instant};

/// A capture nobody finishes is cancelled after this.
pub const CAPTURE_LIMIT: Duration = Duration::from_secs(10);

pub struct Capture {
    pub state: HotkeyCaptureState,
    started: Instant,
}

impl Capture {
    pub fn new(now: Instant) -> Self {
        Self {
            state: HotkeyCaptureState::default(),
            started: now,
        }
    }

    /// One physical key edge; true when the state changed. The chord is
    /// the set at its largest, final once every key is up again; `Esc`
    /// alone cancels.
    pub fn key(&mut self, code: u16, value: i32) -> bool {
        // Left click is the UI's own (Detect / Cancel buttons).
        if self.state.done || code == EvdevKey::BTN_LEFT.0 {
            return false;
        }
        let key = EvdevKey(code);
        let s = &mut self.state;
        match value {
            1 if !s.held.contains(&key) => {
                s.held.push(key);
                if s.held.len() > s.chord.len() && s.held.len() <= MAX_CHORD_KEYS {
                    s.chord = s.held.clone();
                    // Modifiers first, as bindings store them.
                    s.chord.sort_by_key(|k| !is_modifier(k.0));
                }
            }
            // Releases of keys held before the capture began are not ours.
            0 if s.held.contains(&key) => {
                s.held.retain(|k| *k != key);
                if s.held.is_empty() {
                    s.cancelled = s.chord == [EvdevKey(KEY_ESC)];
                    s.done = true;
                }
            }
            _ => return false,
        }
        true
    }

    /// Cancel once [`CAPTURE_LIMIT`] has passed; true when it did.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.state.done || now.saturating_duration_since(self.started) < CAPTURE_LIMIT {
            return false;
        }
        self.cancel();
        true
    }

    pub fn cancel(&mut self) {
        self.state.held.clear();
        self.state.done = true;
        self.state.cancelled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::hotkey::{KEY_LEFTCTRL, KEY_SPACE};

    fn keys(codes: &[u16]) -> Vec<EvdevKey> {
        codes.iter().map(|&c| EvdevKey(c)).collect()
    }

    #[test]
    fn the_chord_is_the_largest_set_taken_when_all_keys_are_up() {
        let mut c = Capture::new(Instant::now());
        assert!(!c.key(KEY_SPACE, 0), "released before the capture began");
        assert!(c.key(KEY_SPACE, 1));
        assert!(c.key(KEY_LEFTCTRL, 1));
        assert!(!c.key(KEY_LEFTCTRL, 2), "autorepeat");
        assert_eq!(c.state.held, keys(&[KEY_SPACE, KEY_LEFTCTRL]));
        assert_eq!(c.state.chord, keys(&[KEY_LEFTCTRL, KEY_SPACE]));
        assert!(c.key(KEY_SPACE, 0));
        assert!(!c.state.done);
        assert!(!c.key(EvdevKey::BTN_LEFT.0, 1), "a click in the UI");
        assert!(c.key(KEY_LEFTCTRL, 0));
        assert!(c.state.done && !c.state.cancelled);
        assert_eq!(c.state.chord, keys(&[KEY_LEFTCTRL, KEY_SPACE]));
        assert!(!c.key(30, 1), "finished");
    }

    #[test]
    fn esc_alone_cancels_but_esc_in_a_chord_is_kept() {
        let mut c = Capture::new(Instant::now());
        c.key(KEY_ESC, 1);
        c.key(KEY_ESC, 0);
        assert!(c.state.done && c.state.cancelled);

        let mut c = Capture::new(Instant::now());
        c.key(KEY_LEFTCTRL, 1);
        c.key(KEY_ESC, 1);
        c.key(KEY_ESC, 0);
        c.key(KEY_LEFTCTRL, 0);
        assert!(c.state.done && !c.state.cancelled);
        assert_eq!(c.state.chord, keys(&[KEY_LEFTCTRL, KEY_ESC]));
    }

    #[test]
    fn a_capture_times_out_and_keeps_at_most_four_keys() {
        let t0 = Instant::now();
        let mut c = Capture::new(t0);
        for k in [29, 42, 56, 125, 30] {
            c.key(k, 1);
        }
        assert_eq!(c.state.chord.len(), MAX_CHORD_KEYS);
        assert!(!c.expire(t0 + Duration::from_secs(9)));
        assert!(c.expire(t0 + CAPTURE_LIMIT));
        assert!(c.state.done && c.state.cancelled && c.state.held.is_empty());
    }
}
