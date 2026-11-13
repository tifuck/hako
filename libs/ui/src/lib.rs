//! hako control panel UI (snapshot-only; no HID / socket).
//!
//! Layout: top bar (brand, profiles, settings) · device rail · device page
//! with a header and sub-tabs. Device controls are rendered from the
//! daemon-declared `SettingMeta`; pages only decide grouping.

use hako_core::catalog::{SettingGroup, SettingKey, SettingMeta, SettingValue};
use hako_core::device::{DeviceKind, DeviceSnapshot, HubState};
use hako_core::ipc::{AppSettings, MeterTargetSpec};
use hako_core::mixer::MixerOp;
use hako_core::remap::{CaptureRole, DeviceRemaps, EvdevKey, RemapBinding};
use hako_core::DeviceId;

pub mod aux;
pub mod bindings;
pub mod brand;
pub mod controls;
pub mod mixer;
pub mod theme;
pub mod widgets;

use theme::pal;

/// Kept for callers that still use the old name.
pub fn apply_dark_theme(ctx: &egui::Context) {
    theme::apply(ctx);
}

/// Commands the UI wants the client to send to the daemon.
#[derive(Debug, Clone)]
pub enum UiCommand {
    Set {
        device_id: DeviceId,
        key: SettingKey,
        value: SettingValue,
    },
    /// Several values written together (a colour's R, G, B): one device write.
    SetMany {
        device_id: DeviceId,
        values: Vec<(SettingKey, SettingValue)>,
    },
    ApplyProfile(String),
    SaveProfile(String),
    ResetAudio,
    Refresh,
    /// Persist edited app settings (daemon writes config.toml).
    SetConfig(AppSettings),
    /// Ask the client to open a path in the file manager.
    OpenPath(String),
    StartCapture {
        device_id: DeviceId,
        role: CaptureRole,
        source: Option<EvdevKey>,
    },
    /// Ends a capture. For a recording, `keep` stores what was recorded.
    CancelCapture {
        device_id: DeviceId,
        keep: bool,
    },
    SetRemap {
        device_id: DeviceId,
        binding: RemapBinding,
    },
    ClearRemap {
        device_id: DeviceId,
        source: EvdevKey,
    },
    /// Look for a new release now and report the result.
    CheckUpdates,
    /// Install the offered release and restart.
    InstallUpdate,
    /// Hide the offer until the next check.
    DismissUpdate,
    /// Reload the mixer (`HakoAppUi::mixer` gets the answer).
    GetMixer,
    /// Mixer edits, applied in order, all or nothing.
    PatchMixer(Vec<MixerOp>),
}

pub struct HakoAppUi {
    pub state: Option<HubState>,
    pub selected: Option<DeviceId>,
    pub daemon_error: Option<String>,
    pub profile_draft: String,
    /// Floating notifications; add with `notify`.
    pub toasts: Vec<widgets::Toast>,
    /// Latest app settings from the daemon (also the edit draft).
    pub settings: Option<AppSettings>,
    /// Settings view active instead of a device page.
    pub show_settings: bool,
    /// Mixer page active instead of a device page (settings still win).
    pub show_mixer: bool,
    pub mixer: mixer::MixerUi,
    /// Newer release found by the client's update check.
    pub update: Option<widgets::UpdateOffer>,
    /// Commands sent but not yet answered (with the state after them);
    /// controls keep their local drafts until then.
    pub pending: bool,
    /// Meters the visible page shows, set by `ui` each frame; the client
    /// streams them only while this is non-empty.
    pub meter_targets: Vec<MeterTargetSpec>,
    /// Latest peaks for `meter_targets`, in order (linear, L/R).
    pub meter_levels: Vec<[f32; 2]>,
}

impl Default for HakoAppUi {
    fn default() -> Self {
        Self {
            state: None,
            selected: None,
            daemon_error: Some("Connecting to hakod…".into()),
            profile_draft: String::new(),
            toasts: Vec::new(),
            settings: None,
            show_settings: false,
            show_mixer: false,
            mixer: mixer::MixerUi::default(),
            update: None,
            pending: false,
            meter_targets: Vec::new(),
            meter_levels: Vec::new(),
        }
    }
}

impl HakoAppUi {
    /// Show a toast. Repeating the same message restarts it instead of stacking.
    pub fn notify(&mut self, tone: widgets::Tone, text: impl Into<String>) {
        let text = text.into();
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(widgets::Toast {
            tone,
            text,
            shown: None,
        });
    }

    pub fn ui(&mut self, ctx: &egui::Context, out: &mut Vec<UiCommand>) {
        theme::set(self.settings.as_ref().map_or("", |s| s.theme.as_str()));
        theme::apply(ctx);
        controls::set_pending(ctx, self.pending);
        self.meter_targets.clear();
        self.mixer.sync(ctx.dragged_id().is_some());
        self.ensure_selection();
        self.top_bar(ctx, out);
        self.rail(ctx, out);
        let mixer_page = self.show_mixer && !self.show_settings;

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(pal().bg)
                    .inner_margin(egui::Margin::symmetric(28, 24)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // The console uses all the width it gets.
                        let cap = if mixer_page { f32::INFINITY } else { 1040.0 };
                        ui.set_max_width((ui.available_width() - 14.0).min(cap));
                        if self.show_settings {
                            widgets::page_transition(ui, "settings");
                            self.settings_page(ui, out);
                            return;
                        }
                        if mixer_page {
                            widgets::page_transition(ui, "mixer");
                            self.mixer_page(ui, out);
                            return;
                        }
                        let device = self.selected.as_ref().and_then(|id| {
                            self.state
                                .as_ref()
                                .and_then(|st| st.devices.iter().find(|d| &d.id == id).cloned())
                        });
                        match device {
                            Some(d) => {
                                let remaps = self.remaps_for(&d.id).unwrap_or_default();
                                self.device_page(ui, &d, &remaps, out);
                            }
                            None => empty_state(ui, self.state.is_some(), &self.daemon_error),
                        }
                    });
            });
        match widgets::toasts(ctx, &mut self.toasts, self.update.as_ref()) {
            Some(true) => out.push(UiCommand::InstallUpdate),
            Some(false) => {
                self.update = None;
                out.push(UiCommand::DismissUpdate);
            }
            None => {}
        }
        if ctx.input(|i| i.viewport().minimized == Some(true)) {
            self.meter_targets.clear();
        }
        // Fader moves go out at ~30 Hz, also after leaving the page mid-drag.
        if let Some(ops) = self.mixer.flush(ctx.input(|i| i.time)) {
            out.push(UiCommand::PatchMixer(ops));
        }
        if self.mixer.has_unsent() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    /// Meter for the `i`th of `meter_targets` (silent until frames arrive).
    fn meter_row(&self, ui: &mut egui::Ui, label: &str, i: usize) {
        let levels = self.meter_levels.get(i).copied().unwrap_or_default();
        widgets::row(ui, label, None, |ui| {
            let w = theme::SLIDER_W + theme::VALUE_W + ui.spacing().item_spacing.x;
            widgets::stereo_meter(ui, ("meter", label), levels, w);
        });
    }

    // ── Top bar ──────────────────────────────────────────────────

    fn top_bar(&mut self, ctx: &egui::Context, out: &mut Vec<UiCommand>) {
        egui::TopBottomPanel::top("top_bar")
            .exact_height(56.0)
            .frame(
                egui::Frame::new()
                    .fill(pal().rail)
                    .inner_margin(egui::Margin::symmetric(16, 0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    brand::logo(ui, 40.0);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let on = self.show_settings;
                        let gear = widgets::icon_button(ui, 32.0, "Settings", true, |p, r, c| {
                            widgets::icon_gear(p, r, if on { pal().accent } else { c })
                        });
                        if gear.clicked() {
                            self.show_settings = !self.show_settings;
                        }
                        ui.add_space(theme::SPACE_XS);
                        self.profile_menu(ui, out);
                        // Only a problem earns space in the top bar.
                        if self.daemon_error.is_some() {
                            ui.add_space(theme::SPACE_SM);
                            widgets::pill(ui, "Daemon offline", pal().danger);
                        }
                    });
                });
            });
    }

    /// Profile picker + "save current settings", drawn like every dropdown.
    /// Stays in place (disabled) while the daemon is offline.
    fn profile_menu(&mut self, ui: &mut egui::Ui, out: &mut Vec<UiCommand>) {
        let st = self.state.clone().filter(|_| self.daemon_error.is_none());
        let current = st.as_ref().and_then(|s| s.last_profile.clone());
        let profiles = st.as_ref().map(|s| s.profiles.clone()).unwrap_or_default();
        let draft = &mut self.profile_draft;
        let popup_id = egui::Id::new("profile_popup");
        let open = ui.memory(|m| m.is_popup_open(popup_id));
        let resp = ui
            .add_enabled_ui(st.is_some(), |ui| {
                widgets::dropdown_button(
                    ui,
                    current.as_deref().unwrap_or("No profile"),
                    PROFILE_MENU_W,
                    open,
                )
            })
            .inner;
        if resp.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup_id));
        }
        egui::popup::popup_below_widget(
            ui,
            popup_id,
            &resp,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                // Exactly as wide as the button it drops from.
                let inner = PROFILE_MENU_W - ui.spacing().menu_margin.sum().x;
                ui.set_width(inner);
                menu_heading(ui, "Profiles");
                if profiles.is_empty() {
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        widgets::hint(ui, "No saved profiles yet");
                    });
                }
                for p in &profiles {
                    let active = current.as_deref() == Some(p.as_str());
                    if ui.selectable_label(active, p).clicked() {
                        out.push(UiCommand::ApplyProfile(p.clone()));
                        ui.memory_mut(|m| m.close_popup());
                    }
                }
                ui.separator();
                menu_heading(ui, "Save current settings");
                ui.horizontal(|ui| {
                    let save_w = 64.0;
                    let field_w = inner - save_w - ui.spacing().item_spacing.x;
                    let edit = widgets::text_field(ui, draft, "Profile name", field_w);
                    let valid = valid_profile_name(draft);
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let save = ui
                        .add_enabled(
                            valid,
                            egui::Button::new("Save")
                                .min_size(egui::vec2(save_w, theme::CONTROL_H)),
                        )
                        .clicked();
                    if valid && (save || enter) {
                        out.push(UiCommand::SaveProfile(draft.trim().to_string()));
                        draft.clear();
                        ui.memory_mut(|m| m.close_popup());
                    }
                });
            },
        );
    }

    // ── Device rail ──────────────────────────────────────────────

    fn rail(&mut self, ctx: &egui::Context, out: &mut Vec<UiCommand>) {
        egui::SidePanel::left("rail")
            .exact_width(236.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(pal().rail)
                    .inner_margin(egui::Margin::symmetric(12, 18)),
            )
            .show(ctx, |ui| {
                ui.label(theme::eyebrow("Devices"));
                ui.add_space(theme::SPACE_SM);
                let Some(st) = &self.state else {
                    widgets::hint(ui, "Waiting for daemon…");
                    return;
                };
                let mut devices = st.devices.clone();
                devices.sort_by_key(|d| kind_order(d.kind));

                // Highlight is painted behind the entries and slides between them.
                let highlight = ui.painter().add(egui::Shape::Noop);
                let mut target = None;
                if devices.is_empty() {
                    widgets::hint(ui, "No supported devices found.");
                }
                for d in &devices {
                    let selected = !self.show_settings
                        && !self.show_mixer
                        && self.selected.as_ref() == Some(&d.id);
                    let resp = rail_entry(ui, d, selected);
                    if selected {
                        target = Some(resp.rect);
                    }
                    if resp.clicked() {
                        self.selected = Some(d.id.clone());
                        self.show_settings = false;
                        self.show_mixer = false;
                    }
                }

                ui.add_space(theme::SPACE_LG);
                ui.label(theme::eyebrow("Audio"));
                ui.add_space(theme::SPACE_SM);
                let selected = self.show_mixer && !self.show_settings;
                let (status, dot) = self.mixer.rail_status();
                let resp = rail_item(ui, selected, "Mixer", &status, dot, widgets::icon_mixer);
                if selected {
                    target = Some(resp.rect);
                }
                if resp.clicked() {
                    if !selected {
                        out.push(UiCommand::GetMixer);
                    }
                    self.show_mixer = true;
                    self.show_settings = false;
                }
                if let Some(r) = target {
                    let y = ui.ctx().animate_value_with_time(
                        egui::Id::new("rail_hl"),
                        r.top(),
                        theme::ANIM,
                    );
                    let hl = egui::Rect::from_min_size(egui::pos2(r.left(), y), r.size());
                    let bar = egui::Rect::from_min_size(
                        hl.min + egui::vec2(0.0, 14.0),
                        egui::vec2(3.0, hl.height() - 28.0),
                    );
                    ui.painter().set(
                        highlight,
                        egui::Shape::Vec(vec![
                            egui::Shape::rect_filled(hl, 10.0, pal().surface_hi),
                            egui::Shape::rect_filled(bar, 2.0, pal().accent),
                        ]),
                    );
                }
            });
    }

    fn remaps_for(&self, id: &DeviceId) -> Option<DeviceRemaps> {
        // The daemon re-keys remaps to the adapter's current id on every attach.
        let st = self.state.as_ref()?;
        st.remaps
            .iter()
            .find(|r| r.device_id.as_ref() == Some(id))
            .cloned()
    }

    fn ensure_selection(&mut self) {
        let Some(st) = &self.state else {
            return;
        };
        if !self
            .selected
            .as_ref()
            .is_some_and(|id| st.devices.iter().any(|d| &d.id == id))
        {
            self.selected = st
                .devices
                .iter()
                .min_by_key(|d| kind_order(d.kind))
                .map(|d| d.id.clone());
        }
    }
}

fn rail_entry(ui: &mut egui::Ui, d: &DeviceSnapshot, selected: bool) -> egui::Response {
    let (status, color) = if d.connected {
        match d
            .batteries
            .iter()
            .find_map(|b| b.percent.map(|p| (p, b.charging)))
        {
            Some((pct, charging)) => (
                format!("{}{pct}%", if charging { "Charging · " } else { "" }),
                widgets::battery_color(pct, charging),
            ),
            None => ("Connected".to_string(), pal().ok),
        }
    } else {
        ("Unavailable".into(), theme::pal().danger)
    };
    rail_item(
        ui,
        selected,
        &short_name(&d.name),
        &status,
        color,
        |p, r, c| widgets::icon_device(p, r, c, d.kind),
    )
}

/// One rail entry: icon, name, and a status line with a coloured dot.
fn rail_item(
    ui: &mut egui::Ui,
    selected: bool,
    name: &str,
    status: &str,
    dot: egui::Color32,
    icon: impl FnOnce(&egui::Painter, egui::Rect, egui::Color32),
) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 58.0), egui::Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered() && !selected, 0.12);
    let p = ui.painter();
    if hover > 0.0 {
        p.rect_filled(rect, 10.0, pal().surface_hi.gamma_multiply(0.6 * hover));
    }
    let icon_rect = egui::Rect::from_center_size(
        rect.left_center() + egui::vec2(26.0, 0.0),
        egui::vec2(22.0, 22.0),
    );
    icon(
        p,
        icon_rect,
        if selected {
            pal().accent
        } else {
            pal().text_muted
        },
    );
    let x = rect.left() + 52.0;
    p.text(
        egui::pos2(x, rect.top() + 20.0),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.5),
        if selected {
            pal().text
        } else {
            pal().text.gamma_multiply(0.85)
        },
    );
    p.circle_filled(egui::pos2(x + 3.0, rect.top() + 39.0), 3.0, dot);
    p.text(
        egui::pos2(x + 12.0, rect.top() + 39.0),
        egui::Align2::LEFT_CENTER,
        status,
        egui::FontId::proportional(11.5),
        pal().text_muted,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn valid_profile_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty()
        && n.len() <= 48
        && n.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
}

// ── Device page ──────────────────────────────────────────────────

/// A device page. Setting groups are sections inside a page; related groups
/// share one (microphone sits with audio, power with device memory).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Tab {
    Performance,
    Lighting,
    Audio,
    LineIn,
    /// Host-side remaps; works for any mouse / keyboard.
    Bindings,
    /// Power, onboard memory and device details; every device has it.
    Device,
}

impl Tab {
    fn of(g: SettingGroup) -> Tab {
        match g {
            SettingGroup::Performance => Tab::Performance,
            SettingGroup::Lighting => Tab::Lighting,
            SettingGroup::Audio | SettingGroup::Microphone => Tab::Audio,
            SettingGroup::LineIn => Tab::LineIn,
            SettingGroup::Power | SettingGroup::Memory => Tab::Device,
        }
    }

    fn label(self, kind: DeviceKind) -> &'static str {
        match (self, kind) {
            (Tab::Performance, _) => "Performance",
            (Tab::Lighting, _) => "Lighting",
            (Tab::Audio, _) => "Audio",
            (Tab::LineIn, _) => "Line-in",
            (Tab::Bindings, DeviceKind::Mouse) => "Buttons",
            (Tab::Bindings, _) => "Key bindings",
            (Tab::Device, _) => "Device",
        }
    }
}

/// The pages a device has, in the order that suits its kind: what people
/// adjust most comes first, and `Device` is always last.
fn tabs_for(d: &DeviceSnapshot) -> Vec<Tab> {
    use Tab::*;
    let order: &[Tab] = match d.kind {
        DeviceKind::Mouse => &[Performance, Bindings, Lighting, Audio, LineIn, Device],
        DeviceKind::Keyboard => &[Lighting, Bindings, Performance, Audio, LineIn, Device],
        DeviceKind::Headset | DeviceKind::Other => &[Audio, LineIn, Lighting, Performance, Device],
    };
    order
        .iter()
        .copied()
        .filter(|&t| match t {
            Bindings => matches!(d.kind, DeviceKind::Mouse | DeviceKind::Keyboard),
            Device => true,
            t => controls::visible(d).any(|m| Tab::of(m.key.group()) == t),
        })
        .collect()
}

/// Visible controls of one setting group, in driver order.
fn group(d: &DeviceSnapshot, g: SettingGroup) -> Vec<&SettingMeta> {
    controls::visible(d)
        .filter(|m| m.key.group() == g)
        .collect()
}

impl HakoAppUi {
    fn device_page(
        &mut self,
        ui: &mut egui::Ui,
        d: &DeviceSnapshot,
        remaps: &DeviceRemaps,
        out: &mut Vec<UiCommand>,
    ) {
        device_header(ui, d);
        ui.add_space(theme::SPACE_LG);

        if !d.connected && d.controls.is_empty() {
            widgets::section(
                ui,
                "Device unavailable",
                |_| {},
                |ui| {
                    ui.label(
                        egui::RichText::new(
                            d.last_error
                                .as_deref()
                                .unwrap_or("Controls appear once the device is reachable."),
                        )
                        .color(pal().text_muted),
                    );
                },
            );
            return;
        }

        let tabs = tabs_for(d);
        let labels: Vec<&str> = tabs.iter().map(|t| t.label(d.kind)).collect();
        let tab_id = egui::Id::new(("tab", d.id.key()));
        // Kept as a `Tab`: pages come and go with the device's controls.
        let mut tab = ui
            .ctx()
            .data(|m| m.get_temp::<Tab>(tab_id))
            .filter(|t| tabs.contains(t))
            .unwrap_or(tabs[0]);
        let active = tabs.iter().position(|t| *t == tab).unwrap_or(0);
        if let Some(i) = tab_bar(ui, tab_id, &labels, active) {
            tab = tabs[i];
            ui.ctx().data_mut(|m| m.insert_temp(tab_id, tab));
        }
        ui.add_space(theme::SPACE_LG);
        widgets::page_transition(ui, (d.id.key(), tab));

        match tab {
            Tab::Bindings if d.kind == DeviceKind::Mouse => {
                bindings::mouse_page(ui, d, remaps, out)
            }
            Tab::Bindings => bindings::keyboard_page(ui, d, remaps, out),
            Tab::Lighting => lighting_tab(ui, d, &group(d, SettingGroup::Lighting), out),
            Tab::LineIn => self.line_in_tab(ui, d, out),
            Tab::Performance => performance_tab(ui, d, out),
            Tab::Audio => audio_tab(ui, d, out),
            Tab::Device => device_tab(ui, d, out),
        }
    }

    fn line_in_tab(&mut self, ui: &mut egui::Ui, d: &DeviceSnapshot, out: &mut Vec<UiCommand>) {
        let on = d
            .settings
            .get(SettingKey::AuxEnabled.as_str())
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let processing = self
            .state
            .as_ref()
            .is_some_and(|s| s.audio.aux_processing_active);
        if on {
            // "After processing" only means something while a chain runs;
            // without one the output is the headset's whole mix.
            self.meter_targets.push(MeterTargetSpec::AuxInput);
            if processing {
                self.meter_targets.push(MeterTargetSpec::AuxOutput);
            }
        }
        widgets::section(
            ui,
            "Line-in monitor",
            |ui| {
                if on {
                    widgets::pill(
                        ui,
                        if processing { "Processing" } else { "Direct" },
                        theme::pal().ok,
                    );
                }
            },
            |ui| {
                let m: Vec<&SettingMeta> = [SettingKey::AuxEnabled, SettingKey::AuxCapturePercent]
                    .iter()
                    .filter_map(|k| controls::meta(d, *k))
                    .collect();
                controls::rows(ui, d, &m, out);
                if on {
                    widgets::divider(ui);
                    self.meter_row(ui, "Input", 0);
                }
            },
        );

        // The chain slides open below the toggle once monitoring is on.
        let reveal = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            egui::Id::new(("aux_chain_reveal", d.id.key())),
            on,
        );
        let mut reveal = reveal;
        reveal.set_open(on);
        reveal.show_body_unindented(ui, |ui| {
            let have = self.settings.is_some();
            let mut draft = self.settings.clone().unwrap_or_default();
            let n = draft
                .aux_effects
                .effects
                .iter()
                .filter(|e| e.enabled)
                .count();
            widgets::section(
                ui,
                "Processing chain",
                |ui| {
                    ui.label(
                        egui::RichText::new(format!("{n} active"))
                            .small()
                            .color(pal().text_faint),
                    );
                },
                |ui| {
                    if !have {
                        widgets::hint(ui, "Loading settings from the daemon…");
                        return;
                    }
                    let commit = aux::chain_editor(ui, &mut draft.aux_effects);
                    if let Some(err) = self.state.as_ref().and_then(|s| s.audio.last_error.clone())
                    {
                        ui.add_space(theme::SPACE_SM);
                        ui.colored_label(theme::pal().warn, err);
                    }
                    if processing {
                        ui.add_space(theme::SPACE_SM);
                        widgets::divider(ui);
                        self.meter_row(ui, "After processing", 1);
                    }
                    if self.settings.as_ref() != Some(&draft) {
                        self.settings = Some(draft.clone());
                    }
                    if commit {
                        out.push(UiCommand::SetConfig(draft.clone()));
                    }
                },
            );
        });
    }

    // ── Settings page ────────────────────────────────────────────

    fn settings_page(&mut self, ui: &mut egui::Ui, out: &mut Vec<UiCommand>) {
        ui.label(egui::RichText::new("Settings").heading().color(pal().text));
        ui.add_space(theme::SPACE_LG);

        let have = self.settings.is_some();
        let mut draft = self.settings.clone().unwrap_or_default();
        let before = draft.clone();
        widgets::section(
            ui,
            "Appearance",
            |_| {},
            |ui| {
                ui.add_enabled_ui(have, |ui| theme_picker(ui, &mut draft.theme));
            },
        );
        widgets::section(
            ui,
            "Audio routing",
            |_| {},
            |ui| {
                ui.add_enabled_ui(have, |ui| {
                    widgets::row(
                        ui,
                        "ChatMix takes the default output",
                        Some("Applies next time ChatMix starts"),
                        |ui| {
                            widgets::switch(ui, &mut draft.chatmix_set_default_sink);
                        },
                    );
                    widgets::divider(ui);
                    widgets::row(
                        ui,
                        "Line-in takes the default output",
                        Some("Applies next time line-in turns on"),
                        |ui| {
                            widgets::switch(ui, &mut draft.aux_set_default_sink);
                        },
                    );
                    widgets::divider(ui);
                    widgets::row(ui, "Resume line-in on connect", None, |ui| {
                        widgets::switch(ui, &mut draft.aux_auto_enable);
                    });
                    widgets::divider(ui);
                    widgets::row(
                        ui,
                        "Audio latency",
                        Some("Lower values use more CPU and may crackle on slow systems"),
                        |ui| {
                            let label = |value: &str| {
                                AUDIO_LATENCY_LABELS
                                    .iter()
                                    .find(|(v, _)| *v == value)
                                    .map_or(value.to_string(), |(_, label)| label.to_string())
                            };
                            widgets::dropdown(
                                "audio_latency",
                                label(&draft.audio_latency),
                                theme::DROPDOWN_W,
                            )
                            .show_ui(ui, |ui| {
                                for (value, text) in AUDIO_LATENCY_LABELS {
                                    ui.selectable_value(
                                        &mut draft.audio_latency,
                                        value.into(),
                                        text,
                                    );
                                }
                            });
                        },
                    );
                });
            },
        );
        widgets::section(
            ui,
            "Daemon",
            |_| {},
            |ui| {
                ui.add_enabled_ui(have, |ui| {
                    widgets::row(ui, "Log level", Some("Applies on daemon restart"), |ui| {
                        widgets::dropdown("log_level", draft.log_level.clone(), theme::DROPDOWN_W)
                            .show_ui(ui, |ui| {
                                for lvl in ["error", "warn", "info", "debug"] {
                                    ui.selectable_value(&mut draft.log_level, lvl.into(), lvl);
                                }
                            });
                    });
                });
                if let Some(st) = &self.state {
                    widgets::divider(ui);
                    ui.add_space(theme::SPACE_XS);
                    for (k, v) in [
                        ("Version", format!("v{}", st.health.version)),
                        ("Uptime", fmt_uptime(st.health.uptime_secs)),
                        ("Devices", st.health.adapter_count.to_string()),
                    ] {
                        widgets::fact(ui, k, &v);
                    }
                    if let Some(e) = &st.health.last_error {
                        widgets::divider(ui);
                        widgets::row(ui, "Last error", None, |ui| {
                            ui.colored_label(theme::pal().warn, e);
                        });
                    }
                }
            },
        );
        widgets::section(
            ui,
            "Updates",
            |_| {},
            |ui| {
                ui.add_enabled_ui(have, |ui| {
                    widgets::row(
                        ui,
                        "Check for updates automatically",
                        Some("Looks for a new release on GitHub every few hours"),
                        |ui| {
                            widgets::switch(ui, &mut draft.check_updates);
                        },
                    );
                });
                widgets::divider(ui);
                widgets::row(
                    ui,
                    &format!("hako v{}", env!("CARGO_PKG_VERSION")),
                    Some("Installed version"),
                    |ui| {
                        if widgets::button(ui, "Check now").clicked() {
                            out.push(UiCommand::CheckUpdates);
                        }
                    },
                );
            },
        );
        if have && draft != before {
            self.settings = Some(draft.clone());
            out.push(UiCommand::SetConfig(draft));
        }

        widgets::section(
            ui,
            "Maintenance",
            |_| {},
            |ui| {
                widgets::row(
                    ui,
                    "Reset audio graph",
                    Some("Removes hako sinks and line-in links, restores the default output"),
                    |ui| {
                        if widgets::button(ui, "Reset").clicked() {
                            out.push(UiCommand::ResetAudio);
                        }
                    },
                );
                widgets::divider(ui);
                let dir = self
                    .settings
                    .as_ref()
                    .map(|s| s.config_dir.clone())
                    .unwrap_or_default();
                widgets::row(
                    ui,
                    "Configuration folder",
                    (!dir.is_empty()).then_some(dir.as_str()),
                    |ui| {
                        if ui
                            .add_enabled_ui(!dir.is_empty(), |ui| widgets::button(ui, "Open"))
                            .inner
                            .clicked()
                        {
                            out.push(UiCommand::OpenPath(dir.clone()));
                        }
                    },
                );
                widgets::divider(ui);
                widgets::row(ui, "Reload device state", None, |ui| {
                    if widgets::button(ui, "Refresh").clicked() {
                        out.push(UiCommand::Refresh);
                    }
                });
            },
        );
        widgets::section(
            ui,
            "About",
            |_| {},
            |ui| {
                widgets::hint(
                    ui,
                    "hako: one control panel for your keyboard, mouse, and headset on Linux.",
                );
                widgets::hint(ui, "MIT License. Device protocols reimplemented from public notes (OpenRazer, Logitech HID++, HeadsetControl).");
            },
        );
    }
}

fn performance_tab(ui: &mut egui::Ui, d: &DeviceSnapshot, out: &mut Vec<UiCommand>) {
    if let Some(m) = controls::meta(d, SettingKey::Dpi) {
        widgets::section(
            ui,
            "Sensitivity",
            |_| {},
            |ui| controls::gated(ui, d, m, |ui| controls::dpi(ui, d, m, out)),
        );
    }
    if let Some(m) = controls::meta(d, SettingKey::ReportRate) {
        let fast = m
            .options
            .iter()
            .any(|o| o.value.as_int().is_some_and(|hz| hz > 1000));
        widgets::section(
            ui,
            "Polling rate",
            |_| {},
            |ui| {
                controls::gated(ui, d, m, |ui| {
                    ui.horizontal_wrapped(|ui| controls::choice(ui, d, m, out));
                });
                ui.add_space(theme::SPACE_XS);
                widgets::hint(
                    ui,
                    if fast {
                        "Higher rates lower latency but use more CPU. Rates above 1000 Hz need a direct USB port."
                    } else {
                        "Higher rates lower latency but use more CPU."
                    },
                );
            },
        );
    }
    use SettingKey::*;
    let (stages, rest): (Vec<&SettingMeta>, Vec<&SettingMeta>) =
        group(d, SettingGroup::Performance)
            .into_iter()
            .filter(|m| !matches!(m.key, Dpi | ReportRate))
            .partition(|m| {
                matches!(
                    m.key,
                    DpiStage1 | DpiStage2 | DpiStage3 | DpiStage4 | DpiStage5 | DpiStageActive
                )
            });
    for (title, metas) in [
        ("DPI stages", stages),
        (
            if d.kind == DeviceKind::Mouse {
                "Sensor"
            } else {
                "Response"
            },
            rest,
        ),
    ] {
        if !metas.is_empty() {
            widgets::section(ui, title, |_| {}, |ui| controls::rows(ui, d, &metas, out));
        }
    }
}

/// ChatMix, then output, then the microphone.
fn audio_tab(ui: &mut egui::Ui, d: &DeviceSnapshot, out: &mut Vec<UiCommand>) {
    if controls::meta(d, SettingKey::ChatMixGame).is_some() {
        widgets::section(
            ui,
            "ChatMix",
            |_| {},
            |ui| {
                controls::chatmix(ui, d);
                ui.add_space(theme::SPACE_SM);
                widgets::hint(
                    ui,
                    "Follows the dial on the base station. Send games to Hako_Game and voice chat to Hako_Chat.",
                );
            },
        );
    }
    let sound: Vec<&SettingMeta> = group(d, SettingGroup::Audio)
        .into_iter()
        .filter(|m| !matches!(m.key, SettingKey::ChatMixGame | SettingKey::ChatMixChat))
        .collect();
    if !sound.is_empty() {
        widgets::section(ui, "Sound", |_| {}, |ui| controls::rows(ui, d, &sound, out));
    }
    let mic = group(d, SettingGroup::Microphone);
    if !mic.is_empty() {
        widgets::section(
            ui,
            "Microphone",
            |_| {},
            |ui| controls::rows(ui, d, &mic, out),
        );
    }
}

/// Power, onboard memory and what the device is.
fn device_tab(ui: &mut egui::Ui, d: &DeviceSnapshot, out: &mut Vec<UiCommand>) {
    let power = group(d, SettingGroup::Power);
    if !power.is_empty() {
        widgets::section(ui, "Power", |_| {}, |ui| controls::rows(ui, d, &power, out));
    }
    let memory = group(d, SettingGroup::Memory);
    if !memory.is_empty() {
        let saved: Vec<&str> = controls::visible(d)
            .filter(|m| {
                m.persistent && m.blocked.is_none() && m.key.group() != SettingGroup::Memory
            })
            .map(|m| m.label.as_str())
            .collect();
        widgets::section(
            ui,
            "Device memory",
            |_| {},
            |ui| {
                controls::rows(ui, d, &memory, out);
                if !saved.is_empty() {
                    ui.add_space(theme::SPACE_XS);
                    widgets::hint(
                        ui,
                        &format!(
                            "When allowed, {} {} written to the device itself, only when you change {}. Profiles never include {}.",
                            saved.join(", "),
                            if saved.len() == 1 { "is" } else { "are" },
                            if saved.len() == 1 { "it" } else { "one" },
                            if saved.len() == 1 { "it" } else { "them" },
                        ),
                    );
                }
            },
        );
    }
    widgets::section(
        ui,
        "About this device",
        |_| {},
        |ui| {
            let fact = widgets::fact;
            fact(ui, "Model", &d.name);
            fact(ui, "Type", kind_label(d.kind));
            fact(
                ui,
                "USB ID",
                &format!("{:04x}:{:04x}", d.id.vendor_id, d.id.product_id),
            );
            for b in &d.batteries {
                let pct = b.percent.map_or("—".into(), |p| format!("{p}%"));
                let text = if b.charging {
                    format!("{pct}, charging")
                } else {
                    pct
                };
                let name = if d.batteries.len() == 1 {
                    "Battery".to_string()
                } else {
                    format!("{} battery", b.label)
                };
                fact(ui, &name, &text);
            }
            ui.add_space(theme::SPACE_XS);
            widgets::divider(ui);
            let problem = d.last_error.as_deref().filter(|_| d.connected);
            widgets::row(ui, "Status", problem, |ui| match (d.connected, problem) {
                (false, _) => {
                    widgets::pill(ui, "Unavailable", pal().danger);
                }
                (true, Some(_)) => {
                    widgets::pill(ui, "Warning", pal().warn);
                }
                (true, None) => {
                    widgets::pill(ui, "Connected", pal().ok);
                }
            });
        },
    );
}

fn lighting_tab(
    ui: &mut egui::Ui,
    d: &DeviceSnapshot,
    metas: &[&SettingMeta],
    out: &mut Vec<UiCommand>,
) {
    let find = |k: SettingKey| metas.iter().find(|m| m.key == k).copied();
    let Some(effect) = find(SettingKey::Effect) else {
        widgets::section(
            ui,
            "Lighting",
            |_| {},
            |ui| controls::rows(ui, d, metas, out),
        );
        return;
    };
    // Brightness applies to every effect, so it sits in the card's header;
    // what the chosen effect offers (color, speed, direction) follows its tiles.
    let brightness = find(SettingKey::Brightness);
    let options: Vec<&SettingMeta> = metas
        .iter()
        .filter(|m| !matches!(m.key, SettingKey::Effect | SettingKey::Brightness))
        .copied()
        .collect();
    let mut header_out = Vec::new();
    widgets::section(
        ui,
        "Effect",
        |ui| {
            if let Some(b) = brightness {
                controls::inline_slider(ui, d, b, &mut header_out);
            }
        },
        |ui| {
            controls::gated(ui, d, effect, |ui| {
                controls::effect_tiles(ui, d, effect, out)
            });
            if !options.is_empty() {
                ui.add_space(theme::SPACE_SM);
                widgets::divider(ui);
                controls::rows(ui, d, &options, out);
            }
        },
    );
    out.append(&mut header_out);
}

fn device_header(ui: &mut egui::Ui, d: &DeviceSnapshot) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 60.0), egui::Sense::hover());
    let mut left = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    header_identity(&mut left, d);
    // Batteries: measured blocks laid out from the right edge inward, the
    // reading and gauge on one line with the label under it.
    let p = ui.painter();
    let big = egui::FontId::proportional(17.0);
    let small = egui::FontId::proportional(11.5);
    let gauge = egui::vec2(26.0, 12.0);
    let mut x = rect.right();
    for b in d.batteries.iter().rev() {
        let pct = b
            .percent
            .map(|p| format!("{p}%"))
            .unwrap_or_else(|| "—".into());
        // One battery needs no name (drivers often call it after the device).
        let name = if d.batteries.len() == 1 {
            "Battery"
        } else {
            b.label.as_str()
        };
        let sub = if b.charging {
            format!("{name} · charging")
        } else {
            name.to_string()
        };
        let pct = p.layout_no_wrap(pct, big.clone(), pal().text);
        let sub = p.layout_no_wrap(sub, small.clone(), pal().text_faint);
        let top_w = pct.size().x + 8.0 + gauge.x;
        let w = top_w.max(sub.size().x);
        let left = x - w;
        let top_y = rect.center().y - 2.0 - pct.size().y;
        widgets::paint_battery(
            p,
            egui::Rect::from_min_size(
                egui::pos2(
                    left + pct.size().x + 8.0,
                    top_y + (pct.size().y - gauge.y) / 2.0,
                ),
                gauge,
            ),
            b.percent,
            b.charging,
        );
        p.galley(egui::pos2(left, top_y), pct, pal().text);
        p.galley(
            egui::pos2(left, rect.center().y + 4.0),
            sub,
            pal().text_faint,
        );
        x = left - theme::SPACE_XL;
    }
}

fn header_identity(ui: &mut egui::Ui, d: &DeviceSnapshot) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
        ui.painter().rect_filled(tile, 14.0, theme::pal().surface);
        ui.painter().rect_stroke(
            tile,
            14.0,
            egui::Stroke::new(1.0_f32, pal().border),
            egui::StrokeKind::Inside,
        );
        widgets::icon_device(ui.painter(), tile.shrink(15.0), pal().accent, d.kind);
        ui.add_space(theme::SPACE_SM);
        ui.vertical(|ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&d.name).heading().color(pal().text));
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(kind_label(d.kind)).color(pal().text_muted));
                if d.connected {
                    widgets::pill(ui, "Connected", theme::pal().ok);
                } else {
                    widgets::pill(ui, "Unavailable", theme::pal().danger);
                }
                if let Some(e) = d.last_error.as_deref().filter(|_| d.connected) {
                    widgets::pill(ui, "Warning", theme::pal().warn).on_hover_text(e);
                }
            });
        });
    });
}

/// Underlined tabs; the indicator slides to the active tab.
fn tab_bar(ui: &mut egui::Ui, id: egui::Id, tabs: &[&str], active: usize) -> Option<usize> {
    let mut clicked = None;
    let mut rects = Vec::new();
    let resp = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 26.0;
        for (i, t) in tabs.iter().enumerate() {
            let color = if i == active {
                pal().text
            } else {
                pal().text_muted
            };
            let r = ui.add(
                egui::Label::new(egui::RichText::new(*t).size(14.5).color(color))
                    .sense(egui::Sense::click()),
            );
            if r.clicked() && i != active {
                clicked = Some(i);
            }
            rects.push(r.on_hover_cursor(egui::CursorIcon::PointingHand).rect);
        }
    });
    let base_y = resp.response.rect.bottom() + 8.0;
    let full = ui.available_width();
    ui.painter().line_segment(
        [
            egui::pos2(resp.response.rect.left(), base_y),
            egui::pos2(resp.response.rect.left() + full, base_y),
        ],
        egui::Stroke::new(1.0_f32, pal().border),
    );
    if let Some(r) = rects.get(active) {
        let l = ui
            .ctx()
            .animate_value_with_time(id.with("ind_l"), r.left(), theme::ANIM);
        let w = ui
            .ctx()
            .animate_value_with_time(id.with("ind_w"), r.width(), theme::ANIM);
        ui.painter().rect_filled(
            egui::Rect::from_min_size(egui::pos2(l, base_y - 1.0), egui::vec2(w, 2.5)),
            1.0,
            pal().accent,
        );
    }
    ui.add_space(8.0);
    clicked
}

fn empty_state(ui: &mut egui::Ui, connected: bool, daemon_error: &Option<String>) {
    ui.vertical_centered(|ui| {
        ui.add_space(theme::SPACE_XL * 2.5);
        brand::pip(ui, 88.0, !connected);
        ui.add_space(theme::SPACE_MD);
        let (title, body) = match daemon_error {
            Some(e) if !connected => ("Waiting for the daemon", e.as_str()),
            _ if connected => (
                "No devices",
                "Plug in a supported keyboard, mouse, or headset.",
            ),
            _ => ("Connecting…", "Loading devices from hakod."),
        };
        ui.label(egui::RichText::new(title).heading().color(pal().text));
        ui.add_space(theme::SPACE_SM);
        ui.label(egui::RichText::new(body).color(pal().text_muted));
        if daemon_error.is_some() {
            ui.add_space(theme::SPACE_MD);
            // A box that hugs the command, centered under the text.
            let galley = ui.painter().layout_no_wrap(
                "systemctl --user enable --now hakod".into(),
                egui::TextStyle::Monospace.resolve(ui.style()),
                pal().text,
            );
            let (r, _) = ui
                .allocate_exact_size(galley.size() + egui::vec2(32.0, 20.0), egui::Sense::hover());
            ui.painter().rect(
                r,
                10.0,
                pal().surface,
                egui::Stroke::new(1.0_f32, pal().border),
                egui::StrokeKind::Inside,
            );
            ui.painter()
                .galley(r.center() - galley.size() / 2.0, galley, pal().text);
            ui.add_space(theme::SPACE_XS);
            widgets::hint(
                ui,
                "Start it with this command; the app reconnects on its own.",
            );
        }
    });
}

/// Width of the profile dropdown (wide enough for the save field inside).
const PROFILE_MENU_W: f32 = 232.0;

/// `AppSettings::audio_latency` values (`hako_core::AUDIO_LATENCIES`) and their labels.
const AUDIO_LATENCY_LABELS: [(&str, &str); 3] = [
    ("low", "Low (2.7 ms)"),
    ("balanced", "Balanced (5.3 ms)"),
    ("safe", "Safe (10.7 ms)"),
];

/// Small heading inside a popup menu, inset to line up with its items.
fn menu_heading(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(ui.spacing().button_padding.x - ui.spacing().item_spacing.x);
        ui.label(theme::eyebrow(text));
    });
}

/// Theme cards with a miniature of each palette.
fn theme_picker(ui: &mut egui::Ui, selected: &mut String) {
    let gap = theme::SPACE_MD;
    let n = theme::THEMES.len();
    let cols = (((ui.available_width() + gap) / (168.0 + gap)).floor() as usize).clamp(2, n);
    let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
    let current = theme::current().id;
    for row in theme::THEMES.chunks(cols) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for t in row {
                let active = t.id == current;
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(w, 112.0), egui::Sense::click());
                let hot = ui
                    .ctx()
                    .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                let p = ui.painter();
                let c = &t.palette;
                // Miniature: rail, a card with a title, a line and an accent control.
                let shot = egui::Rect::from_min_size(rect.min, egui::vec2(w, 76.0));
                p.rect_filled(shot, 10.0, c.bg);
                let rail = egui::Rect::from_min_size(shot.min, egui::vec2(w * 0.24, 76.0));
                p.rect_filled(
                    rail,
                    egui::CornerRadius {
                        nw: 10,
                        sw: 10,
                        ne: 0,
                        se: 0,
                    },
                    c.rail,
                );
                p.rect_filled(
                    egui::Rect::from_min_size(
                        rail.min + egui::vec2(6.0, 12.0),
                        egui::vec2(rail.width() - 12.0, 8.0),
                    ),
                    3.0,
                    c.surface_hi,
                );
                let card = egui::Rect::from_min_max(
                    egui::pos2(rail.right() + 8.0, shot.top() + 10.0),
                    egui::pos2(shot.right() - 8.0, shot.bottom() - 10.0),
                );
                p.rect_filled(card, 6.0, c.surface);
                p.rect_stroke(
                    card,
                    6.0,
                    egui::Stroke::new(1.0_f32, c.border),
                    egui::StrokeKind::Inside,
                );
                let line = |y: f32, len: f32, col: egui::Color32| {
                    p.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(card.left() + 8.0, card.top() + y),
                            egui::vec2(card.width() * len, 5.0),
                        ),
                        2.5,
                        col,
                    );
                };
                line(9.0, 0.45, c.text);
                line(21.0, 0.7, c.text_faint);
                let track = egui::Rect::from_min_size(
                    egui::pos2(card.left() + 8.0, card.bottom() - 15.0),
                    egui::vec2(card.width() - 16.0, 4.0),
                );
                p.rect_filled(track, 2.0, c.surface_hover);
                p.rect_filled(
                    egui::Rect::from_min_size(track.min, egui::vec2(track.width() * 0.6, 4.0)),
                    2.0,
                    c.accent,
                );
                p.circle_filled(
                    egui::pos2(track.left() + track.width() * 0.6, track.center().y),
                    4.5,
                    c.accent,
                );
                let ring = if active {
                    pal().accent
                } else {
                    pal().border.lerp_to_gamma(pal().border_hi, hot)
                };
                p.rect_stroke(
                    shot,
                    10.0,
                    egui::Stroke::new(if active { 2.0_f32 } else { 1.0 }, ring),
                    egui::StrokeKind::Outside,
                );
                p.text(
                    egui::pos2(rect.left() + 2.0, rect.bottom() - 12.0),
                    egui::Align2::LEFT_CENTER,
                    t.label,
                    egui::FontId::proportional(13.5),
                    if active { pal().text } else { pal().text_muted },
                );
                if active {
                    let check = egui::Rect::from_center_size(
                        egui::pos2(rect.right() - 9.0, rect.bottom() - 12.0),
                        egui::vec2(16.0, 16.0),
                    );
                    p.circle_filled(check.center(), 8.0, pal().accent);
                    let s = egui::Stroke::new(1.6_f32, pal().on_accent);
                    let m = check.center();
                    p.line_segment([m + egui::vec2(-4.0, 0.0), m + egui::vec2(-1.0, 3.0)], s);
                    p.line_segment([m + egui::vec2(-1.0, 3.0), m + egui::vec2(4.0, -3.0)], s);
                }
                if resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                    && !active
                {
                    *selected = t.id.to_string();
                }
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
}

// ── Helpers ──────────────────────────────────────────────────────

fn fmt_uptime(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86400,
        (secs % 86400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

fn kind_order(k: DeviceKind) -> u8 {
    match k {
        DeviceKind::Keyboard => 0,
        DeviceKind::Mouse => 1,
        DeviceKind::Headset => 2,
        DeviceKind::Other => 3,
    }
}

fn kind_label(k: DeviceKind) -> &'static str {
    match k {
        DeviceKind::Keyboard => "Keyboard",
        DeviceKind::Mouse => "Mouse",
        DeviceKind::Headset => "Headset",
        DeviceKind::Other => "Device",
    }
}

fn short_name(name: &str) -> String {
    ["Razer ", "Logitech ", "SteelSeries ", "Arctis "]
        .iter()
        .fold(name.to_string(), |n, p| n.replace(p, ""))
}

#[cfg(test)]
mod tests;
