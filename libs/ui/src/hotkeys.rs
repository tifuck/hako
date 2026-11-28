//! App-wide hotkeys as the pages see them, and the one editor every page
//! uses for a binding: the Voice page and the device Bindings pages edit the
//! same entries. Edits are checked with `apply_hotkey_ops` before they go out
//! as `PatchHotkeys` and are applied to the local copy; the daemon's state is
//! adopted once nothing is in flight.

use crate::bindings::{key_name, keycap_chip};
use crate::theme::{self, pal};
use crate::widgets;
use crate::UiCommand;
use egui::{Color32, Ui};
use hako_core::device::DeviceSnapshot;
use hako_core::hotkey::{
    apply_hotkey_ops, is_modifier, side_neutral, HotkeyAction, HotkeyBinding, HotkeyCaptureState,
    HotkeyConflict, HotkeyMode, HotkeyOp, HotkeyScope, HotkeySource, HotkeyStatus, HotkeysFile,
    HotkeysState, PassThrough, HOLD_MS_RANGE, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTSHIFT,
    KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_RIGHTMETA, KEY_RIGHTSHIFT, LATCH_MS_RANGE,
};
use hako_core::remap::EvdevKey;

/// The hotkeys as the pages show them.
#[derive(Default)]
pub struct HotkeysUi {
    /// Daemon state plus local edits not yet answered.
    state: Option<HotkeysState>,
    /// Newest daemon state, adopted once nothing is in flight.
    remote: Option<HotkeysState>,
    /// Why the daemon couldn't report it (e.g. a hakod without hotkeys).
    error: Option<String>,
    /// `PatchHotkeys` calls sent and not answered yet (the client sets this).
    pub busy: bool,
    /// The running Detect as the daemon reports it.
    capture: Option<HotkeyCaptureState>,
    /// The binding a Detect records for (new or existing): its chord is
    /// replaced by the captured one once the capture is done.
    detecting: Option<HotkeyBinding>,
    /// Why the last edit of a binding was not sent: (binding id, reason).
    refused: Option<(String, String)>,
    /// The action of the last new binding a Detect could not add.
    refused_new: Option<HotkeyAction>,
    /// A Detect that records a chord for something else than a binding (a
    /// Jarvis command's keys), by owner.
    chord_for: Option<String>,
    /// Its result: (owner, chord), until the owner takes it.
    chord: Option<(String, Vec<EvdevKey>)>,
}

impl HotkeysUi {
    /// A `GetHotkeys` / `PatchHotkeys` answer or a `HotkeysUpdated` event.
    pub fn receive(&mut self, state: HotkeysState) {
        self.remote = Some(state);
        self.error = None;
    }

    /// `GetHotkeys` failed: the pages explain instead of showing stale keys.
    pub fn unavailable(&mut self, why: String) {
        self.state = None;
        self.remote = None;
        self.error = Some(why);
    }

    pub fn state(&self) -> Option<&HotkeysState> {
        self.state.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// A `HotkeyCapture` event or the `StartHotkeyCapture` answer.
    pub fn capture_update(&mut self, c: HotkeyCaptureState) {
        self.capture = Some(c);
    }

    /// The binding a running Detect is for.
    pub fn detecting(&self) -> Option<&HotkeyBinding> {
        self.detecting.as_ref()
    }

    /// Adopt the daemon's state unless edits are on their way or a slider
    /// is being dragged.
    pub(crate) fn sync(&mut self, dragging: bool) {
        if self.state.is_none() || (!self.busy && !dragging) {
            if let Some(r) = self.remote.take() {
                self.state = Some(r);
            }
        }
    }

    /// The file, when it can be edited (`hotkeys.toml` was readable).
    pub fn file(&self) -> Option<&HotkeysFile> {
        self.state
            .as_ref()
            .filter(|s| s.load_error.is_none())
            .map(|s| &s.file)
    }

    fn status(&self, id: &str) -> Option<&HotkeyStatus> {
        self.state.as_ref()?.status.iter().find(|s| s.id == id)
    }

    /// Checks `ops` against the local file and sends them; a refused edit is
    /// kept to be shown at binding `id`. Returns whether it was sent.
    pub(crate) fn send(&mut self, id: &str, ops: Vec<HotkeyOp>, out: &mut Vec<UiCommand>) -> bool {
        let Some(file) = self.file() else {
            return false;
        };
        match apply_hotkey_ops(file, &ops) {
            Ok(next) => {
                if let Some(s) = self.state.as_mut() {
                    s.file = next;
                }
                self.refused = None;
                out.push(UiCommand::PatchHotkeys(ops));
                true
            }
            Err(e) => {
                self.refused = Some((id.to_string(), e));
                false
            }
        }
    }

    /// A slider being dragged: the local copy follows, the release is sent.
    fn preview(&mut self, b: HotkeyBinding) {
        if let Some(slot) = self
            .state
            .as_mut()
            .and_then(|s| s.file.bindings.iter_mut().find(|x| x.id == b.id))
        {
            *slot = b;
        }
    }

    /// Record a chord for `b` (a new binding or an existing one).
    pub(crate) fn detect(&mut self, b: HotkeyBinding, out: &mut Vec<UiCommand>) {
        self.capture = None;
        self.refused = None;
        self.chord_for = None;
        self.detecting = Some(b);
        out.push(UiCommand::StartHotkeyCapture);
    }

    /// Record a chord for `owner` (not a binding); `take_chord` gives it.
    pub(crate) fn detect_chord(&mut self, owner: &str, out: &mut Vec<UiCommand>) {
        self.capture = None;
        self.detecting = None;
        self.chord = None;
        self.chord_for = Some(owner.to_string());
        out.push(UiCommand::StartHotkeyCapture);
    }

    /// A Detect for `owner` is running.
    pub(crate) fn detecting_chord(&self, owner: &str) -> bool {
        self.chord_for.as_deref() == Some(owner)
    }

    /// The chord a finished Detect recorded for `owner`.
    pub(crate) fn take_chord(&mut self, owner: &str) -> Option<Vec<EvdevKey>> {
        self.chord
            .take_if(|(o, _)| o == owner)
            .map(|(_, keys)| keys)
    }

    /// The keys held during the running Detect.
    pub(crate) fn capture_caps(&self, ui: &mut Ui) {
        capture_caps(ui, self.capture.as_ref());
    }

    pub(crate) fn cancel_detect(&mut self, out: &mut Vec<UiCommand>) {
        self.detecting = None;
        self.chord_for = None;
        self.capture = None;
        out.push(UiCommand::CancelHotkeyCapture);
    }

    /// Every frame: a finished Detect gives its chord to the binding it was
    /// for (checked like any edit).
    pub(crate) fn frame(&mut self, out: &mut Vec<UiCommand>) {
        if !self.capture.as_ref().is_some_and(|c| c.done) {
            return;
        }
        let c = self.capture.take().unwrap_or_default();
        if let Some(owner) = self.chord_for.take() {
            if !c.cancelled && !c.chord.is_empty() {
                self.chord = Some((owner, c.chord));
            }
            return;
        }
        let Some(mut b) = self.detecting.take() else {
            return;
        };
        if c.cancelled || c.chord.is_empty() {
            return;
        }
        b.keys = c.chord;
        let id = b.id.clone();
        let action = b.action;
        if !self.send(&id, vec![HotkeyOp::Set { binding: b }], out) {
            self.refused_new = Some(action);
        }
    }
}

/// A free id for a new binding of `action` (`dictate`, `dictate-2`, ...).
pub fn new_id(file: &HotkeysFile, action: HotkeyAction) -> String {
    let base = match action {
        HotkeyAction::VoiceTranscribe => "dictate",
        HotkeyAction::VoiceTranscribeAlt => "dictate-alt",
        HotkeyAction::VoiceCancel => "cancel",
        HotkeyAction::JarvisListen => "jarvis",
    };
    (1..)
        .map(|n| match n {
            1 => base.to_string(),
            n => format!("{base}-{n}"),
        })
        .find(|id| file.bindings.iter().all(|b| &b.id != id))
        .unwrap_or_else(|| base.to_string())
}

/// One key of a chord as a keycap: `Ctrl` for either side, `Right Ctrl`
/// with `exact_side`.
pub fn cap_label(k: EvdevKey, exact_side: bool) -> String {
    if exact_side || !is_modifier(k.0) {
        return key_name(k);
    }
    let name = key_name(EvdevKey(side_neutral(k.0)));
    name.strip_prefix("Left ").unwrap_or(&name).to_string()
}

/// The codes a chord key stands for on a device: both sides of a modifier
/// unless the binding wants the exact side.
fn codes(k: u16, exact_side: bool) -> Vec<u16> {
    if exact_side || !is_modifier(k) {
        return vec![k];
    }
    let left = side_neutral(k);
    let right = match left {
        KEY_LEFTCTRL => KEY_RIGHTCTRL,
        KEY_LEFTSHIFT => KEY_RIGHTSHIFT,
        KEY_LEFTALT => KEY_RIGHTALT,
        _ => KEY_RIGHTMETA,
    };
    vec![left, right]
}

/// The binding contains `code` (either side of a modifier unless exact).
pub fn uses_key(b: &HotkeyBinding, code: u16) -> bool {
    b.keys
        .iter()
        .any(|k| codes(k.0, b.exact_side).contains(&code))
}

/// `Ctrl + Space · Dictate (Smart)`.
pub fn describe(b: &HotkeyBinding) -> String {
    format!("{} · {}", b.chord_label(), summary_action(b))
}

/// `Voice: Dictate (Smart)`: what a key or button in the hotkey does.
pub fn summary(b: &HotkeyBinding) -> String {
    format!("{}: {}", b.action.feature(), summary_action(b))
}

fn summary_action(b: &HotkeyBinding) -> String {
    if b.action.is_momentary() {
        b.action.label().to_string()
    } else {
        format!("{} ({})", b.action.label(), b.mode.label())
    }
}

/// A device key that is part of a hotkey.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyMark {
    /// Printed on the key: `Voice` on the trigger, `+Voice` on the others.
    pub label: String,
    /// `Voice: Dictate (Smart)`.
    pub summary: String,
    /// The whole chord, mode and action, for the tooltip.
    pub tip: String,
    pub binding: String,
}

/// Keys of this device that are part of an enabled hotkey covering it.
pub fn key_marks(file: &HotkeysFile, vendor_id: u16, product_id: u16) -> Vec<(u16, KeyMark)> {
    let mut marks: Vec<(u16, KeyMark)> = Vec::new();
    for b in file.for_device(vendor_id, product_id).filter(|b| b.enabled) {
        let trigger = b.trigger();
        let feature = b.action.feature();
        for k in &b.keys {
            let label = if Some(*k) == trigger {
                feature.to_string()
            } else {
                format!("+{feature}")
            };
            for code in codes(k.0, b.exact_side) {
                if marks.iter().all(|(c, _)| *c != code) {
                    marks.push((
                        code,
                        KeyMark {
                            label: label.clone(),
                            summary: summary(b),
                            tip: describe(b),
                            binding: b.id.clone(),
                        },
                    ));
                }
            }
        }
    }
    marks
}

/// The chord as keycaps.
pub(crate) fn chord_caps(ui: &mut Ui, keys: &[EvdevKey], exact_side: bool, color: Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                ui.label(egui::RichText::new("+").color(pal().text_faint));
            }
            keycap_chip(ui, &cap_label(*k, exact_side), color);
        }
    });
}

/// Names of devices a scope stands for.
pub fn scope_label(
    scope: &HotkeyScope,
    devices: &[DeviceSnapshot],
    st: Option<&HotkeysState>,
) -> String {
    match scope {
        HotkeyScope::Any => "All keyboards and mice".into(),
        HotkeyScope::Device {
            vendor_id,
            product_id,
        } => device_name(*vendor_id, *product_id, devices, st),
    }
}

fn device_name(v: u16, p: u16, devices: &[DeviceSnapshot], st: Option<&HotkeysState>) -> String {
    devices
        .iter()
        .find(|d| d.id.vendor_id == v && d.id.product_id == p)
        .map(|d| d.name.clone())
        .or_else(|| {
            st?.inputs
                .iter()
                .find(|i| i.vendor_id == v && i.product_id == p)
                .map(|i| i.name.clone())
        })
        .unwrap_or_else(|| format!("{v:04x}:{p:04x}"))
}

const MODE_HINTS: [(HotkeyMode, &str); 3] = [
    (
        HotkeyMode::Smart,
        "Hold to talk, or tap to start and tap again to stop.",
    ),
    (HotkeyMode::Hold, "Records while the keys are held."),
    (HotkeyMode::Toggle, "Press to start, press again to stop."),
];

/// The shared editor for one binding: chord, Detect, mode, pass-through,
/// sides, timing, devices, on/off and remove. `here` is the device page it
/// sits on (vendor, product, name), offered as the scope; without one (the
/// Voice page's lists) the options fold into a summary line.
pub(crate) fn editor(
    ui: &mut Ui,
    hk: &mut HotkeysUi,
    b: &HotkeyBinding,
    here: Option<(u16, u16, &str)>,
    devices: &[DeviceSnapshot],
    out: &mut Vec<UiCommand>,
) {
    let editable = hk.file().is_some();
    ui.push_id(("hotkey", &b.id), |ui| {
        let mut next = b.clone();
        let mut remove = false;
        let open_id = ui.id().with("open");
        let mut open = here.is_some() || ui.data(|m| m.get_temp::<bool>(open_id).unwrap_or(false));
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_H);
            ui.label(
                egui::RichText::new(b.action.label())
                    .strong()
                    .color(pal().text),
            );
            let live = hk.detecting.as_ref().is_some_and(|d| d.id == b.id);
            if live {
                capture_caps(ui, hk.capture.as_ref());
            } else {
                let color = if b.enabled {
                    theme::HOTKEY
                } else {
                    pal().text_faint
                };
                chord_caps(ui, &b.keys, b.exact_side, color);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if here.is_none() {
                    let tip = if open { "Hide options" } else { "Show options" };
                    if widgets::icon_button(ui, 26.0, tip, true, |p, r, c| {
                        widgets::icon_chevron(p, r, c, !open)
                    })
                    .clicked()
                    {
                        open = !open;
                        ui.data_mut(|m| m.insert_temp(open_id, open));
                    }
                }
                ui.add_enabled_ui(editable, |ui| {
                    if widgets::icon_button(ui, 26.0, "Remove hotkey", true, widgets::icon_close)
                        .clicked()
                    {
                        remove = true;
                    }
                    let resp = widgets::switch(ui, &mut next.enabled);
                    resp.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Checkbox,
                            true,
                            next.enabled,
                            "Hotkey enabled",
                        )
                    });
                    if live {
                        if widgets::button(ui, "Cancel").clicked() {
                            hk.cancel_detect(out);
                        }
                    } else if widgets::button(ui, "Detect…")
                        .on_hover_text(
                            "Press the new keys on any keyboard or mouse; Esc alone cancels",
                        )
                        .clicked()
                    {
                        hk.detect(b.clone(), out);
                    }
                });
            });
        });
        if open {
            ui.add_enabled_ui(editable, |ui| {
                options(ui, hk, b, &mut next, here, devices);
            });
        } else {
            let scope = match &b.scope {
                HotkeyScope::Any => scope_label(&b.scope, devices, hk.state.as_ref()),
                s => format!("Only {}", scope_label(s, devices, hk.state.as_ref())),
            };
            let pass = match b.pass {
                PassThrough::Listen => "Listen",
                PassThrough::Block => "Block",
            };
            let parts = if b.action.is_momentary() {
                ["Fires when pressed", pass, &scope].join(" · ")
            } else {
                [b.mode.label(), pass, &scope].join(" · ")
            };
            widgets::hint(ui, &parts);
            block_warning(ui, b);
        }
        if remove {
            hk.send(&b.id, vec![HotkeyOp::Remove { id: b.id.clone() }], out);
        } else if next != *b {
            hk.send(&b.id, vec![HotkeyOp::Set { binding: next }], out);
        }
        if let Some((_, why)) = hk.refused.as_ref().filter(|(id, _)| *id == b.id) {
            ui.colored_label(pal().danger, why);
        }
    });
}

/// Keys held right now during a Detect, or the prompt.
fn capture_caps(ui: &mut Ui, c: Option<&HotkeyCaptureState>) {
    ui.ctx().request_repaint();
    let held = c.map(|c| c.held.as_slice()).unwrap_or_default();
    if held.is_empty() {
        let t = ui.input(|i| i.time) as f32;
        ui.label(
            egui::RichText::new("Press the keys…").color(
                pal()
                    .accent
                    .gamma_multiply(0.6 + 0.4 * (t * 4.0).sin().abs()),
            ),
        );
    } else {
        chord_caps(ui, held, true, pal().accent);
    }
}

fn options(
    ui: &mut Ui,
    hk: &mut HotkeysUi,
    b: &HotkeyBinding,
    next: &mut HotkeyBinding,
    here: Option<(u16, u16, &str)>,
    devices: &[DeviceSnapshot],
) {
    let modifiers = b.keys.iter().any(|k| is_modifier(k.0));
    if b.action.is_momentary() {
        widgets::hint(ui, "Fires when pressed.");
    } else {
        widgets::row(
            ui,
            "Mode",
            MODE_HINTS
                .iter()
                .find(|(m, _)| *m == b.mode)
                .map(|(_, h)| *h),
            |ui| {
                let labels: Vec<&str> = HotkeyMode::ALL.iter().map(|m| m.label()).collect();
                let sel = HotkeyMode::ALL.iter().position(|m| *m == b.mode);
                if let Some(i) = widgets::segmented(ui, ui.id().with("mode"), &labels, sel) {
                    next.mode = HotkeyMode::ALL[i];
                }
            },
        );
        let timing = match b.mode {
            HotkeyMode::Smart => Some((
                "Push-to-talk after",
                "Held at least this long, release stops; a shorter tap keeps recording",
                LATCH_MS_RANGE,
                b.latch_ms,
            )),
            HotkeyMode::Hold => Some((
                "Minimum hold",
                "A shorter press is ignored as a brush",
                HOLD_MS_RANGE,
                b.min_hold_ms,
            )),
            HotkeyMode::Toggle => None,
        };
        if let Some((label, hint, range, ms)) = timing {
            let (ms, commit) = ms_row(ui, label, hint, ms, range);
            let mut edited = b.clone();
            if b.mode == HotkeyMode::Smart {
                edited.latch_ms = ms;
            } else {
                edited.min_hold_ms = ms;
            }
            if commit {
                next.latch_ms = edited.latch_ms;
                next.min_hold_ms = edited.min_hold_ms;
            } else if edited != *b {
                hk.preview(edited);
            }
        }
    }
    widgets::row(
        ui,
        "Pass-through",
        Some(match b.pass {
            PassThrough::Listen => "The keys also reach the app in focus",
            PassThrough::Block => "Apps never see the chord (the keyboard is grabbed)",
        }),
        |ui| {
            let sel = Some(usize::from(b.pass == PassThrough::Block));
            if let Some(i) = widgets::segmented(ui, ui.id().with("pass"), &["Listen", "Block"], sel)
            {
                next.pass = if i == 1 {
                    PassThrough::Block
                } else {
                    PassThrough::Listen
                };
            }
        },
    );
    block_warning(ui, b);
    if modifiers {
        widgets::row(
            ui,
            "Exact side",
            Some("Off: left and right Ctrl, Shift, Alt and Super count the same"),
            |ui| {
                widgets::switch(ui, &mut next.exact_side);
            },
        );
    }
    widgets::row(
        ui,
        "Devices",
        Some("Which keyboards and mice can fire it"),
        |ui| {
            let st = hk.state.as_ref();
            let mut options = vec![HotkeyScope::Any];
            let mut add = |s: HotkeyScope| {
                if !options.contains(&s) {
                    options.push(s);
                }
            };
            if let Some((v, p, _)) = here {
                add(HotkeyScope::Device {
                    vendor_id: v,
                    product_id: p,
                });
            }
            add(b.scope.clone());
            for i in st.map(|s| s.inputs.as_slice()).unwrap_or_default() {
                add(HotkeyScope::Device {
                    vendor_id: i.vendor_id,
                    product_id: i.product_id,
                });
            }
            let label = |s: &HotkeyScope| match (s, here) {
                (
                    HotkeyScope::Device {
                        vendor_id,
                        product_id,
                    },
                    Some((v, p, name)),
                ) if *vendor_id == v && *product_id == p => {
                    format!("Only {name}")
                }
                (HotkeyScope::Any, _) => scope_label(s, devices, st),
                _ => format!("Only {}", scope_label(s, devices, st)),
            };
            widgets::dropdown("scope", label(&b.scope), theme::DROPDOWN_W).show_ui(ui, |ui| {
                for s in &options {
                    ui.selectable_value(&mut next.scope, s.clone(), label(s));
                }
            });
        },
    );
}

/// Why Block suits a held chord with a modifier.
fn block_warning(ui: &mut Ui, b: &HotkeyBinding) {
    if b.keys.iter().any(|k| is_modifier(k.0))
        && b.pass == PassThrough::Listen
        && b.mode != HotkeyMode::Toggle
        && !b.action.is_momentary()
    {
        ui.colored_label(
            pal().warn,
            egui::RichText::new(
                "Block is recommended: while a modifier is held, apps could read typed text as shortcuts.",
            )
            .small(),
        );
    }
}

/// Milliseconds slider; returns the value and whether it is final.
fn ms_row(ui: &mut Ui, label: &str, hint: &str, ms: u32, (lo, hi): (u32, u32)) -> (u32, bool) {
    let mut v = ms.clamp(lo, hi) as f32;
    let mut commit = false;
    widgets::row(ui, label, Some(hint), |ui| {
        ui.label(widgets::value_text(format!("{} ms", v.round())));
        let resp = widgets::slider(
            ui,
            egui::Slider::new(&mut v, lo as f32..=hi as f32)
                .step_by(10.0)
                .show_value(false),
            theme::SLIDER_W,
        );
        widgets::knob(ui, &resp, widgets::fraction(v, lo as f32, hi as f32, false));
        commit = resp.drag_stopped() || (resp.changed() && !resp.dragged());
    });
    (v.round() as u32, commit)
}

/// Where a binding's presses come from, its devices and what keeps it from
/// firing.
pub(crate) fn status_lines(ui: &mut Ui, hk: &HotkeysUi, b: &HotkeyBinding) {
    let Some(s) = hk.status(&b.id) else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        let (text, color) = match s.source {
            HotkeySource::Evdev => ("Keyboard input", pal().ok),
            HotkeySource::Portal => ("Desktop shortcut", pal().accent),
            HotkeySource::Cli => ("Command line only", pal().warn),
        };
        widgets::pill(ui, text, color).on_hover_text(match s.source {
            HotkeySource::Evdev => "hako reads the keys itself",
            HotkeySource::Portal => {
                "The desktop reports the shortcut (hako cannot read these devices)"
            }
            HotkeySource::Cli => "Nothing can see these keys: use `hako voice start|stop|toggle`",
        });
        if !s.active {
            widgets::pill(ui, "Inactive", pal().text_faint)
                .on_hover_text("Its feature is off, so pressing it does nothing");
        }
        if !s.devices.is_empty() {
            widgets::hint(ui, &format!("Fires from {}", s.devices.join(", ")));
        }
    });
    for p in &s.problems {
        ui.colored_label(pal().warn, p);
    }
}

/// The binding's clashes with remaps (on one device if `only` is set), each
/// with its three answers, and with desktop shortcuts.
pub(crate) fn conflicts(
    ui: &mut Ui,
    hk: &mut HotkeysUi,
    b: &HotkeyBinding,
    only: Option<(u16, u16)>,
    devices: &[DeviceSnapshot],
    out: &mut Vec<UiCommand>,
) {
    let Some(list) = hk.status(&b.id).map(|s| s.conflicts.clone()) else {
        return;
    };
    for (i, c) in list.iter().enumerate() {
        match c {
            HotkeyConflict::Remap {
                device,
                vendor_id,
                product_id,
                key,
            } => {
                if only.is_some_and(|o| o != (*vendor_id, *product_id)) {
                    continue;
                }
                if b.with_remaps {
                    widgets::hint(
                        ui,
                        &format!(
                            "{} is remapped on {device}: both the remap and the hotkey run.",
                            key_name(*key)
                        ),
                    );
                    continue;
                }
                ui.push_id(("conflict", i), |ui| {
                    ui.colored_label(
                        pal().warn,
                        format!(
                            "{} is also remapped on {device}. Keep which?",
                            key_name(*key)
                        ),
                    );
                    ui.horizontal(|ui| {
                        if widgets::button(ui, "Keep remap")
                            .on_hover_text("Remove this hotkey")
                            .clicked()
                        {
                            hk.send(&b.id, vec![HotkeyOp::Remove { id: b.id.clone() }], out);
                        }
                        let dev = devices.iter().find(|d| {
                            d.id.vendor_id == *vendor_id && d.id.product_id == *product_id
                        });
                        let keep = ui
                            .add_enabled_ui(dev.is_some(), |ui| widgets::button(ui, "Keep hotkey"))
                            .inner
                            .on_hover_text("Reset the key's remap on that device")
                            .on_disabled_hover_text("Connect the device to change its remaps");
                        if keep.clicked() {
                            if let Some(d) = dev {
                                out.push(UiCommand::ClearRemap {
                                    device_id: d.id.clone(),
                                    source: *key,
                                });
                            }
                        }
                        if widgets::button(ui, "Both")
                            .on_hover_text("The remap still runs and the hotkey also fires")
                            .clicked()
                        {
                            let binding = HotkeyBinding {
                                with_remaps: true,
                                ..b.clone()
                            };
                            hk.send(&b.id, vec![HotkeyOp::Set { binding }], out);
                        }
                    });
                });
            }
            HotkeyConflict::Desktop { shortcut } => {
                ui.colored_label(
                    pal().warn,
                    format!("The desktop also uses this chord ({shortcut})"),
                );
            }
        }
    }
}

/// A Detect running for a binding not in the file yet, or why the one that
/// just ended was refused; with `feature`, only for actions of that page.
pub(crate) fn new_binding_row(
    ui: &mut Ui,
    hk: &mut HotkeysUi,
    feature: Option<&str>,
    out: &mut Vec<UiCommand>,
) {
    let known = |hk: &HotkeysUi, id: &str| {
        hk.state
            .as_ref()
            .is_some_and(|s| s.file.bindings.iter().any(|b| b.id == id))
    };
    let ours =
        |a: Option<HotkeyAction>| feature.is_none_or(|f| a.is_some_and(|a| a.feature() == f));
    if let Some(b) = hk
        .detecting
        .clone()
        .filter(|d| !known(hk, &d.id) && ours(Some(d.action)))
    {
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_H);
            ui.label(
                egui::RichText::new(format!("New: {}", b.action.label()))
                    .strong()
                    .color(pal().text),
            );
            capture_caps(ui, hk.capture.as_ref());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::button(ui, "Cancel").clicked() {
                    hk.cancel_detect(out);
                }
            });
        });
    } else if let Some((_, why)) = hk
        .refused
        .as_ref()
        .filter(|(id, _)| !known(hk, id) && ours(hk.refused_new))
    {
        ui.colored_label(pal().danger, why);
    }
}

/// The action picker for a new binding; with `feature`, only that page's.
pub(crate) fn action_picker(
    ui: &mut Ui,
    id_salt: &str,
    feature: Option<&str>,
    action: &mut HotkeyAction,
) {
    widgets::dropdown(id_salt, action.label(), theme::DROPDOWN_W).show_ui(ui, |ui| {
        for a in HotkeyAction::ALL
            .into_iter()
            .filter(|a| feature.is_none_or(|f| a.feature() == f))
        {
            ui.selectable_value(action, a, a.label());
        }
    });
}
