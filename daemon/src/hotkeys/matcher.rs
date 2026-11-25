//! The chord matcher: pure state (key events and time in, edges and
//! pass-through verdicts out). The remap thread feeds it the physical events
//! of every node it reads; nothing here touches a device.

use super::HotkeyEdge;
use hako_core::hotkey::{
    is_modifier, side_neutral, HotkeyAction, HotkeyBinding, HotkeyMode, PassThrough, CHATTER_MS,
};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// An input node: the `N` of `/dev/input/eventN`.
pub type Node = u32;

/// What the matcher knows about a node it is fed from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeInfo {
    pub vendor_id: u16,
    /// The node's own product ID, then those of the registered device it
    /// belongs to (a receiver node carries its devices' input).
    pub product_ids: Vec<u16>,
    /// Grabbed by the remap engine: its events go through the virtual
    /// device, so Block can hold keys back.
    pub grabbed: bool,
    /// Keys remapped on the device the node belongs to.
    pub remapped: BTreeSet<u16>,
}

/// What the remap engine does with the event it just fed.
#[derive(Debug, Default, PartialEq)]
pub struct Verdict {
    /// Do not forward it (a held-back chord key).
    pub swallow: bool,
    /// Release these through the virtual device first: a Block chord's
    /// modifiers, which went down before the chord was complete.
    pub release: Vec<(Node, u16)>,
    /// Press these through the virtual device first: a Block modifier-only
    /// chord turned out to be the user's own shortcut.
    pub press: Vec<(Node, u16)>,
}

/// Mouse buttons (`BTN_LEFT`..`BTN_TASK`): always held back in a chord.
pub fn is_mouse_button(code: u16) -> bool {
    (0x110..=0x117).contains(&code)
}

/// A chord that is down right now.
struct Held {
    since: Instant,
    /// (node, code) of the chord's keys.
    keys: Vec<(Node, u16)>,
    /// This press started a session (it did not stop one, nor was it chatter).
    started: bool,
    /// A modifier-only chord saw another key go down: its release does nothing.
    spoiled: bool,
}

#[derive(Default)]
struct BindState {
    held: Option<Held>,
    /// Start sent, no Stop / Discard yet.
    running: bool,
    last_stop: Option<Instant>,
}

#[derive(Default)]
pub struct Matcher {
    bindings: Vec<(HotkeyBinding, BindState)>,
    nodes: BTreeMap<Node, NodeInfo>,
    /// Keys down on the seat, in press order.
    pressed: Vec<(Node, u16)>,
    /// Keys held back from apps until their release.
    swallowed: BTreeSet<(Node, u16)>,
    active: BTreeSet<HotkeyAction>,
    recording: bool,
    locked: bool,
    capturing: bool,
    edges: Vec<(HotkeyAction, HotkeyEdge)>,
}

fn ms(v: u32) -> Duration {
    Duration::from_millis(u64::from(v))
}

impl Matcher {
    /// New bindings; an unchanged binding keeps its session, a changed or
    /// removed one that was running is discarded.
    pub fn set_bindings(&mut self, list: &[HotkeyBinding]) {
        let mut old = std::mem::take(&mut self.bindings);
        for b in list {
            let state = old
                .iter()
                .position(|(o, _)| o == b)
                .map(|i| old.swap_remove(i).1)
                .unwrap_or_default();
            self.bindings.push((b.clone(), state));
        }
        for (b, st) in old {
            if st.running {
                self.edges.push((b.action, HotkeyEdge::Discard));
            }
        }
    }

    pub fn set_active(&mut self, active: BTreeSet<HotkeyAction>) {
        self.active = active;
    }

    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
    }

    /// The session is locked (logind `LockedHint`): nothing fires.
    pub fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    /// A capture runs: keys are tracked, nothing fires.
    pub fn set_capturing(&mut self, capturing: bool) {
        self.capturing = capturing;
    }

    pub fn active(&self) -> &BTreeSet<HotkeyAction> {
        &self.active
    }

    /// The nodes fed from now on; keys of nodes that went away are released.
    pub fn sync_nodes(&mut self, nodes: BTreeMap<Node, NodeInfo>, now: Instant) {
        let gone: Vec<Node> = (self.nodes.keys())
            .filter(|n| !nodes.contains_key(n))
            .copied()
            .collect();
        for node in gone {
            self.remove_node(node, now);
        }
        self.nodes = nodes;
    }

    /// The node is gone (unplugged, closed): each of its keys is released.
    pub fn remove_node(&mut self, node: Node, now: Instant) {
        let keys: Vec<u16> = (self.pressed.iter())
            .filter(|k| k.0 == node)
            .map(|k| k.1)
            .collect();
        for code in keys {
            self.up(node, code, now);
        }
        self.swallowed.retain(|k| k.0 != node);
        self.nodes.remove(&node);
    }

    /// `down` is what the node reports held (`EVIOCGKEY`) as it is opened:
    /// keys we missed the release of go up; keys we missed the press of are
    /// tracked without firing anything.
    pub fn resync(&mut self, node: Node, down: &[u16], now: Instant) {
        self.release_missing(node, down, now);
        for &code in down {
            if !self.pressed.contains(&(node, code)) {
                self.pressed.push((node, code));
            }
        }
    }

    /// Watchdog: keys `node` no longer reports held go up. Keys it reports
    /// that we have not seen are left alone: their press may still be
    /// queued on the node, and must fire then.
    pub fn release_missing(&mut self, node: Node, down: &[u16], now: Instant) {
        let lost: Vec<u16> = (self.pressed.iter())
            .filter(|k| k.0 == node && !down.contains(&k.1))
            .map(|k| k.1)
            .collect();
        for code in lost {
            self.up(node, code, now);
        }
    }

    /// Nodes whose keys hold a Hold / Smart session open: re-read them
    /// periodically so a lost release can never leave it stuck.
    pub fn watch_nodes(&self) -> BTreeSet<Node> {
        (self.bindings.iter())
            .filter(|(b, st)| st.running && b.mode != HotkeyMode::Toggle)
            .filter_map(|(_, st)| st.held.as_ref().filter(|h| h.started))
            .flat_map(|h| h.keys.iter().map(|k| k.0))
            .collect()
    }

    /// Modifiers apps see held: physically down and not released for them.
    pub fn held_modifiers(&self) -> Vec<u16> {
        let mut out = Vec::new();
        for k in &self.pressed {
            if is_modifier(k.1) && !self.swallowed.contains(k) && !out.contains(&k.1) {
                out.push(k.1);
            }
        }
        out
    }

    pub fn take_edges(&mut self) -> Vec<(HotkeyAction, HotkeyEdge)> {
        std::mem::take(&mut self.edges)
    }

    /// One key event (`value` 0 up, 1 down, 2 autorepeat) of `node`.
    pub fn key(&mut self, node: Node, code: u16, value: i32, now: Instant) -> Verdict {
        match value {
            1 => self.down(node, code, now),
            0 => Verdict {
                swallow: self.up(node, code, now),
                ..Verdict::default()
            },
            // Autorepeat changes nothing; a held-back key's repeats stay back.
            _ => Verdict {
                swallow: self.swallowed.contains(&(node, code)),
                ..Verdict::default()
            },
        }
    }

    fn fireable(&self, action: HotkeyAction) -> bool {
        self.active.contains(&action) && (action != HotkeyAction::VoiceCancel || self.recording)
    }

    /// `node`'s keys count for `b`: in its scope, and (unless the user chose
    /// both) none of the chord's keys is remapped on that device.
    fn covers(&self, b: &HotkeyBinding, node: Node) -> bool {
        let Some(info) = self.nodes.get(&node) else {
            return false;
        };
        (info.product_ids.iter()).any(|&p| b.scope.covers(info.vendor_id, p))
            && (b.with_remaps
                || !b.keys.iter().any(|k| {
                    info.remapped.iter().any(|&r| {
                        r == k.0 || (!b.exact_side && side_neutral(r) == side_neutral(k.0))
                    })
                }))
    }

    /// The chord keys if the press of `code` on `node` completes binding `i`.
    fn completes(&self, i: usize, node: Node, code: u16) -> Option<Vec<(Node, u16)>> {
        let (b, st) = &self.bindings[i];
        if !b.enabled || st.held.is_some() || !self.fireable(b.action) || !self.covers(b, node) {
            return None;
        }
        let norm = |c: u16| if b.exact_side { c } else { side_neutral(c) };
        let chord = b.chord();
        // Modifiers go down first, in any order; the trigger completes it.
        let trigger = if b.modifier_only() {
            chord.contains(&norm(code))
        } else {
            b.trigger().map(|t| norm(t.0)) == Some(norm(code))
        };
        if !trigger {
            return None;
        }
        let keys: Vec<(Node, u16)> = (self.pressed.iter())
            .filter(|k| self.covers(b, k.0))
            .copied()
            .collect();
        let down: BTreeSet<u16> = keys.iter().map(|k| norm(k.1)).collect();
        (down == chord).then_some(keys)
    }

    fn down(&mut self, node: Node, code: u16, now: Instant) -> Verdict {
        let mut v = Verdict::default();
        if self.pressed.contains(&(node, code)) {
            v.swallow = self.swallowed.contains(&(node, code));
            return v;
        }
        self.pressed.push((node, code));
        // A modifier-only chord followed by another key was the user's own
        // shortcut (`Ctrl+Super+X`): throw the session away.
        for (b, st) in &mut self.bindings {
            let Some(h) = st.held.as_mut() else { continue };
            if !b.modifier_only() || h.spoiled || h.keys.contains(&(node, code)) {
                continue;
            }
            h.spoiled = true;
            if h.started && st.running {
                st.running = false;
                st.last_stop = Some(now);
                self.edges.push((b.action, HotkeyEdge::Discard));
            }
            for k in &h.keys {
                if self.swallowed.remove(k) {
                    v.press.push(*k);
                }
            }
        }
        if self.locked || self.capturing {
            return v;
        }
        for i in 0..self.bindings.len() {
            if let Some(keys) = self.completes(i, node, code) {
                self.activate(i, keys, (node, code), now, &mut v);
            }
        }
        v
    }

    fn activate(
        &mut self,
        i: usize,
        keys: Vec<(Node, u16)>,
        trigger: (Node, u16),
        now: Instant,
        v: &mut Verdict,
    ) {
        let (b, st) = &mut self.bindings[i];
        let chatter = st
            .last_stop
            .is_some_and(|t| now.saturating_duration_since(t) < ms(CHATTER_MS));
        let edge = if chatter {
            None
        } else if b.action.is_momentary() {
            Some(HotkeyEdge::Fire)
        } else if st.running && b.mode != HotkeyMode::Hold {
            Some(HotkeyEdge::Stop)
        } else {
            Some(HotkeyEdge::Start)
        };
        match edge {
            Some(HotkeyEdge::Start) => st.running = true,
            Some(HotkeyEdge::Stop) => {
                st.running = false;
                st.last_stop = Some(now);
            }
            _ => {}
        }
        if let Some(e) = edge {
            self.edges.push((b.action, e));
        }
        // Held back from apps: the whole chord with Block, mouse buttons
        // always; only what the remap engine grabbed can be.
        let block = b.pass == PassThrough::Block;
        for &k in &keys {
            let grabbed = self.nodes.get(&k.0).is_some_and(|n| n.grabbed);
            if !grabbed || !(block || is_mouse_button(k.1)) {
                continue;
            }
            self.swallowed.insert(k);
            if k == trigger {
                v.swallow = true;
            } else {
                v.release.push(k);
            }
        }
        st.held = Some(Held {
            since: now,
            keys,
            started: edge == Some(HotkeyEdge::Start),
            spoiled: false,
        });
    }

    /// A key went up; true when it was held back (its release is too).
    fn up(&mut self, node: Node, code: u16, now: Instant) -> bool {
        self.pressed.retain(|k| *k != (node, code));
        let swallow = self.swallowed.remove(&(node, code));
        for (b, st) in &mut self.bindings {
            if !st
                .held
                .as_ref()
                .is_some_and(|h| h.keys.contains(&(node, code)))
            {
                continue;
            }
            let Some(h) = st.held.take() else { continue };
            if h.spoiled || !h.started || b.action.is_momentary() {
                continue;
            }
            let long = now.saturating_duration_since(h.since);
            let edge = match b.mode {
                HotkeyMode::Toggle => continue,
                HotkeyMode::Hold if long < ms(b.min_hold_ms) => HotkeyEdge::Discard,
                HotkeyMode::Hold => HotkeyEdge::Stop,
                // A tap latches: it keeps recording until the next press.
                HotkeyMode::Smart if long < ms(b.latch_ms) => continue,
                HotkeyMode::Smart => HotkeyEdge::Stop,
            };
            st.running = false;
            st.last_stop = Some(now);
            self.edges.push((b.action, edge));
        }
        swallow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hako_core::hotkey::{
        HotkeyScope, KEY_ESC, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTCTRL, KEY_SPACE,
    };
    use HotkeyAction::{JarvisListen, VoiceCancel, VoiceTranscribe};
    use HotkeyEdge::{Discard, Fire, Start, Stop};

    /// (ms since start, node, code, value)
    type Step = (u64, Node, u16, i32);

    const KBD: Node = 3;
    const MOUSE: Node = 7;
    const A: u16 = 30;
    const X: u16 = 45;
    const SIDE: u16 = 0x113;

    fn binding(mode: HotkeyMode, keys: &[u16]) -> HotkeyBinding {
        HotkeyBinding {
            mode,
            ..HotkeyBinding::new("t", VoiceTranscribe, keys)
        }
    }

    fn matcher(bindings: &[HotkeyBinding]) -> Matcher {
        let mut m = Matcher::default();
        m.set_bindings(bindings);
        m.set_active(HotkeyAction::ALL.into_iter().collect());
        let info = |pid| NodeInfo {
            vendor_id: 0x1234,
            product_ids: vec![pid],
            grabbed: true,
            remapped: BTreeSet::new(),
        };
        m.sync_nodes(
            BTreeMap::from([(KBD, info(0x0001)), (MOUSE, info(0x0002))]),
            Instant::now(),
        );
        m
    }

    /// A scripted run: (ms since start, node, code, value) -> edges seen.
    fn run(m: &mut Matcher, steps: &[Step]) -> Vec<(HotkeyAction, HotkeyEdge)> {
        let t0 = Instant::now();
        for &(t, node, code, value) in steps {
            m.key(node, code, value, t0 + Duration::from_millis(t));
        }
        m.take_edges()
    }

    const CTRL: u16 = KEY_LEFTCTRL;
    const SPACE: u16 = KEY_SPACE;

    fn press(t: u64, code: u16) -> Step {
        (t, KBD, code, 1)
    }
    fn release(t: u64, code: u16) -> Step {
        (t, KBD, code, 0)
    }

    #[test]
    fn modes_turn_presses_into_edges() {
        use HotkeyMode::{Hold, Smart, Toggle};
        let chord = [CTRL, SPACE];
        let tap = |t: u64, hold: u64| {
            [
                press(t, CTRL),
                press(t + 5, SPACE),
                release(t + 5 + hold, SPACE),
                release(t + 10 + hold, CTRL),
            ]
        };
        let table: &[(HotkeyMode, Vec<Step>, Vec<HotkeyEdge>)] = &[
            // Hold: start on press, stop on release; a brush is discarded.
            (Hold, tap(0, 500).to_vec(), vec![Start, Stop]),
            (Hold, tap(0, 50).to_vec(), vec![Start, Discard]),
            // Toggle: releases do nothing, the next press stops.
            (Toggle, tap(0, 500).to_vec(), vec![Start]),
            (
                Toggle,
                [tap(0, 50), tap(1000, 50)].concat(),
                vec![Start, Stop],
            ),
            // Smart: a long press is push-to-talk, a tap latches.
            (Smart, tap(0, 500).to_vec(), vec![Start, Stop]),
            (Smart, tap(0, 100).to_vec(), vec![Start]),
            (
                Smart,
                [tap(0, 100), tap(1000, 100)].concat(),
                vec![Start, Stop],
            ),
            (
                Smart,
                [tap(0, 100), tap(1000, 900)].concat(),
                vec![Start, Stop],
            ),
        ];
        for (mode, steps, want) in table {
            let mut m = matcher(&[binding(*mode, &chord)]);
            let got: Vec<_> = run(&mut m, steps).into_iter().map(|e| e.1).collect();
            assert_eq!(&got, want, "{mode:?} {steps:?}");
        }
    }

    #[test]
    fn exact_match_and_key_order() {
        let chord = [CTRL, SPACE];
        let table: &[(&[Step], usize)] = &[
            (&[press(0, CTRL), press(5, SPACE)], 1),
            // Space then Ctrl is typing.
            (&[press(0, SPACE), press(5, CTRL)], 0),
            // An extra modifier or key.
            (
                &[press(0, CTRL), press(1, KEY_LEFTSHIFT), press(5, SPACE)],
                0,
            ),
            (&[press(0, CTRL), press(1, A), press(5, SPACE)], 0),
            // Autorepeat of the trigger is not a press.
            (&[press(0, CTRL), (5, KBD, SPACE, 2)], 0),
            // Right Ctrl counts as Ctrl unless the binding is exact.
            (&[press(0, KEY_RIGHTCTRL), press(5, SPACE)], 1),
            // A chord may span devices.
            (&[(0, MOUSE, CTRL, 1), press(5, SPACE)], 1),
        ];
        for (steps, starts) in table {
            let mut m = matcher(&[binding(HotkeyMode::Hold, &chord)]);
            assert_eq!(run(&mut m, steps).len(), *starts, "{steps:?}");
        }
        let exact = HotkeyBinding {
            exact_side: true,
            ..binding(HotkeyMode::Hold, &[KEY_RIGHTCTRL, SPACE])
        };
        let mut m = matcher(&[exact]);
        assert!(run(&mut m, &[press(0, CTRL), press(5, SPACE)]).is_empty());
        let mut m = matcher(&[HotkeyBinding {
            exact_side: true,
            ..binding(HotkeyMode::Hold, &[KEY_RIGHTCTRL, SPACE])
        }]);
        assert_eq!(
            run(&mut m, &[press(0, KEY_RIGHTCTRL), press(5, SPACE)]),
            [(VoiceTranscribe, Start)]
        );
    }

    #[test]
    fn other_keys_after_activation_do_not_end_it() {
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[CTRL, SPACE])]);
        let edges = run(
            &mut m,
            &[
                press(0, CTRL),
                press(5, SPACE),
                press(300, A),
                release(310, A),
                release(500, SPACE),
            ],
        );
        assert_eq!(edges, [(VoiceTranscribe, Start), (VoiceTranscribe, Stop)]);
    }

    #[test]
    fn modifier_only_chords_fire_on_the_last_modifier_and_discard_on_another_key() {
        let chord = [CTRL, KEY_LEFTMETA];
        let mut m = matcher(&[binding(HotkeyMode::Hold, &chord)]);
        // Either order; the release of either modifier stops.
        let edges = run(
            &mut m,
            &[press(0, KEY_LEFTMETA), press(5, CTRL), release(400, CTRL)],
        );
        assert_eq!(edges, [(VoiceTranscribe, Start), (VoiceTranscribe, Stop)]);
        let mut m = matcher(&[binding(HotkeyMode::Hold, &chord)]);
        let edges = run(
            &mut m,
            &[
                press(0, CTRL),
                press(5, KEY_LEFTMETA),
                press(300, X),
                release(400, X),
                release(500, CTRL),
            ],
        );
        assert_eq!(
            edges,
            [(VoiceTranscribe, Start), (VoiceTranscribe, Discard)]
        );
    }

    #[test]
    fn chatter_after_a_stop_is_ignored() {
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[SPACE])]);
        let edges = run(
            &mut m,
            &[
                press(0, SPACE),
                release(500, SPACE),
                press(550, SPACE),
                release(560, SPACE),
                press(700, SPACE),
            ],
        );
        assert_eq!(
            edges,
            [
                (VoiceTranscribe, Start),
                (VoiceTranscribe, Stop),
                (VoiceTranscribe, Start)
            ]
        );
    }

    #[test]
    fn scope_disabled_and_inactive_bindings_never_fire() {
        let scoped = HotkeyBinding {
            scope: HotkeyScope::Device {
                vendor_id: 0x1234,
                product_id: 0x0002,
            },
            ..binding(HotkeyMode::Hold, &[SPACE])
        };
        let mut m = matcher(std::slice::from_ref(&scoped));
        assert!(
            run(&mut m, &[press(0, SPACE)]).is_empty(),
            "keyboard is out of scope"
        );
        assert_eq!(run(&mut m, &[(10, MOUSE, SPACE, 1)]).len(), 1);

        let mut m = matcher(&[HotkeyBinding {
            enabled: false,
            ..binding(HotkeyMode::Hold, &[SPACE])
        }]);
        assert!(run(&mut m, &[press(0, SPACE)]).is_empty());

        let mut m = matcher(&[binding(HotkeyMode::Hold, &[SPACE])]);
        m.set_active(BTreeSet::from([JarvisListen]));
        assert!(run(&mut m, &[press(0, SPACE)]).is_empty());
    }

    #[test]
    fn cancel_fires_on_press_only_while_recording() {
        let cancel = HotkeyBinding::new("c", VoiceCancel, &[KEY_ESC]);
        let mut m = matcher(&[cancel]);
        assert!(run(&mut m, &[press(0, KEY_ESC), release(5, KEY_ESC)]).is_empty());
        m.set_recording(true);
        assert_eq!(
            run(&mut m, &[press(500, KEY_ESC), release(900, KEY_ESC)]),
            [(VoiceCancel, Fire)]
        );
    }

    #[test]
    fn jarvis_listen_behaves_like_dictation() {
        let b = HotkeyBinding {
            mode: HotkeyMode::Hold,
            ..HotkeyBinding::new("j", JarvisListen, &[SPACE])
        };
        let mut m = matcher(&[b]);
        let edges = run(&mut m, &[press(0, SPACE), release(500, SPACE)]);
        assert_eq!(edges, [(JarvisListen, Start), (JarvisListen, Stop)]);
    }

    #[test]
    fn remapped_keys_hide_a_device_unless_the_user_chose_both() {
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[CTRL, SPACE])]);
        let mut nodes = m.nodes.clone();
        nodes.get_mut(&KBD).unwrap().remapped.insert(KEY_RIGHTCTRL);
        m.sync_nodes(nodes, Instant::now());
        assert!(run(&mut m, &[press(0, CTRL), press(5, SPACE)]).is_empty());
        let mut m2 = matcher(&[HotkeyBinding {
            with_remaps: true,
            ..binding(HotkeyMode::Hold, &[CTRL, SPACE])
        }]);
        m2.sync_nodes(m.nodes.clone(), Instant::now());
        assert_eq!(run(&mut m2, &[press(0, CTRL), press(5, SPACE)]).len(), 1);
    }

    #[test]
    fn missed_releases_unplug_and_watchdog_end_a_hold() {
        let t0 = Instant::now();
        let at = |ms_: u64| t0 + Duration::from_millis(ms_);
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[CTRL, SPACE])]);
        m.key(KBD, CTRL, 1, at(0));
        m.key(KBD, SPACE, 1, at(5));
        assert_eq!(m.watch_nodes(), BTreeSet::from([KBD]));
        // EVIOCGKEY still reports both: nothing changes.
        m.release_missing(KBD, &[CTRL, SPACE, A], at(250));
        assert_eq!(m.take_edges(), [(VoiceTranscribe, Start)]);
        // Space's release was lost.
        m.release_missing(KBD, &[CTRL, A], at(500));
        assert_eq!(m.take_edges(), [(VoiceTranscribe, Stop)]);
        assert!(m.watch_nodes().is_empty());

        // A resync never fires: a key it finds down is only tracked.
        m.resync(KBD, &[CTRL, SPACE], at(1000));
        assert!(m.take_edges().is_empty());

        // Unplug mid-hold = release.
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[CTRL, SIDE])]);
        m.key(KBD, CTRL, 1, at(0));
        m.key(MOUSE, SIDE, 1, at(5));
        let mut nodes = m.nodes.clone();
        nodes.remove(&MOUSE);
        m.sync_nodes(nodes, at(400));
        assert_eq!(
            m.take_edges(),
            [(VoiceTranscribe, Start), (VoiceTranscribe, Stop)]
        );
        assert_eq!(m.held_modifiers(), [CTRL]);
    }

    #[test]
    fn nothing_fires_while_locked_or_capturing() {
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[CTRL, SPACE])]);
        m.set_locked(true);
        assert!(run(
            &mut m,
            &[press(0, CTRL), press(5, SPACE), release(9, SPACE)]
        )
        .is_empty());
        m.set_locked(false);
        m.set_capturing(true);
        assert!(run(&mut m, &[press(20, SPACE)]).is_empty());
        m.set_capturing(false);
        assert_eq!(
            run(&mut m, &[release(30, SPACE), press(40, SPACE)]).len(),
            1
        );
    }

    #[test]
    fn block_holds_back_the_chord_and_releases_its_modifiers() {
        let b = HotkeyBinding {
            pass: PassThrough::Block,
            ..binding(HotkeyMode::Hold, &[CTRL, SPACE])
        };
        let t0 = Instant::now();
        let mut m = matcher(&[b]);
        assert_eq!(m.key(KBD, CTRL, 1, t0), Verdict::default());
        assert_eq!(m.held_modifiers(), [CTRL]);
        let v = m.key(KBD, SPACE, 1, t0);
        assert!(v.swallow);
        assert_eq!(v.release, [(KBD, CTRL)]);
        // Apps saw Ctrl go up: no modifier is held for typing.
        assert!(m.held_modifiers().is_empty());
        assert!(m.key(KBD, SPACE, 2, t0).swallow);
        assert!(m.key(KBD, SPACE, 0, t0).swallow);
        assert!(m.key(KBD, CTRL, 0, t0).swallow);
        // A sequence that is not the chord passes untouched.
        assert_eq!(m.key(KBD, CTRL, 1, t0), Verdict::default());
        assert_eq!(m.key(KBD, 46, 1, t0), Verdict::default());
        assert_eq!(m.key(KBD, 46, 0, t0), Verdict::default());
        assert_eq!(m.key(KBD, CTRL, 0, t0), Verdict::default());

        // A Listen chord is not held back, except its mouse buttons.
        let mut m = matcher(&[binding(HotkeyMode::Hold, &[CTRL, SIDE])]);
        m.key(KBD, CTRL, 1, t0);
        let v = m.key(MOUSE, SIDE, 1, t0);
        assert!(v.swallow && v.release.is_empty());
        assert_eq!(m.held_modifiers(), [CTRL]);
        assert!(m.key(MOUSE, SIDE, 0, t0).swallow);
    }

    #[test]
    fn block_gives_a_discarded_modifier_chord_back() {
        let b = HotkeyBinding {
            pass: PassThrough::Block,
            ..binding(HotkeyMode::Hold, &[CTRL, KEY_LEFTMETA])
        };
        let t0 = Instant::now();
        let mut m = matcher(&[b]);
        m.key(KBD, CTRL, 1, t0);
        let v = m.key(KBD, KEY_LEFTMETA, 1, t0);
        assert!(v.swallow);
        assert_eq!(v.release, [(KBD, CTRL)]);
        // The user's own Ctrl+Super+X: both modifiers go down again first.
        let v = m.key(KBD, X, 1, t0);
        assert!(!v.swallow);
        assert_eq!(v.press, [(KBD, CTRL), (KBD, KEY_LEFTMETA)]);
        assert!(!m.key(KBD, CTRL, 0, t0).swallow);
        assert_eq!(
            m.take_edges(),
            [(VoiceTranscribe, Start), (VoiceTranscribe, Discard)]
        );
    }

    #[test]
    fn a_changed_binding_discards_its_running_session() {
        let b = binding(HotkeyMode::Toggle, &[SPACE]);
        let mut m = matcher(std::slice::from_ref(&b));
        run(&mut m, &[press(0, SPACE), release(5, SPACE)]);
        m.set_bindings(std::slice::from_ref(&b));
        assert!(m.take_edges().is_empty(), "unchanged: keeps running");
        m.set_bindings(&[HotkeyBinding {
            mode: HotkeyMode::Hold,
            ..b
        }]);
        assert_eq!(m.take_edges(), [(VoiceTranscribe, Discard)]);
    }
}
