//! Hako control panel UI (snapshot-only; no HID / socket).
//!
//! Layout: top bar (brand, profiles, settings) · device rail · device page
//! with a header and sub-tabs. Device controls are rendered from the
//! daemon-declared `SettingMeta`; pages only decide grouping.

use hako_core::catalog::{SettingGroup, SettingKey, SettingMeta, SettingValue};
use hako_core::device::{DeviceKind, DeviceSnapshot, HubState};
use hako_core::ipc::AppSettings;
use hako_core::remap::{CaptureRole, DeviceRemaps, EvdevKey, RemapBinding};
use hako_core::DeviceId;

pub mod aux;
pub mod bindings;
pub mod controls;
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
    /// Newer release found by the client's update check.
    pub update: Option<widgets::UpdateOffer>,
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
            update: None,
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
        self.ensure_selection();
        self.top_bar(ctx, out);
        self.rail(ctx);

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
                        ui.set_max_width((ui.available_width() - 14.0).min(1040.0));
                        if self.show_settings {
                            widgets::page_transition(ui, "settings");
                            self.settings_page(ui, out);
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
                    // Brand mark: Pip on the theme accent.
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, 8.0, pal().accent);
                    widgets::pip(ui.painter(), r);
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new("Hako")
                            .size(15.0)
                            .strong()
                            .color(pal().text),
                    );

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
                        ui.add_space(theme::SPACE_SM);
                        let (color, text) = if self.daemon_error.is_none() {
                            (pal().ok, "Daemon online")
                        } else {
                            (pal().danger, "Daemon offline")
                        };
                        widgets::pill(ui, text, color);
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

    fn rail(&mut self, ctx: &egui::Context) {
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
                if st.devices.is_empty() {
                    widgets::hint(ui, "No supported devices found.");
                    return;
                }
                let mut devices = st.devices.clone();
                devices.sort_by_key(|d| kind_order(d.kind));

                // Highlight is painted behind the entries and slides between them.
                let highlight = ui.painter().add(egui::Shape::Noop);
                let mut target = None;
                for d in &devices {
                    let selected = !self.show_settings && self.selected.as_ref() == Some(&d.id);
                    let resp = rail_entry(ui, d, selected);
                    if selected {
                        target = Some(resp.rect);
                    }
                    if resp.clicked() {
                        self.selected = Some(d.id.clone());
                        self.show_settings = false;
                    }
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
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 58.0), egui::Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id, resp.hovered() && !selected, 0.12);
    let p = ui.painter();
    if hover > 0.0 {
        p.rect_filled(rect, 10.0, pal().surface_hi.gamma_multiply(0.6 * hover));
    }
    let icon = egui::Rect::from_center_size(
        rect.left_center() + egui::vec2(26.0, 0.0),
        egui::vec2(22.0, 22.0),
    );
    widgets::icon_device(
        p,
        icon,
        if selected {
            pal().accent
        } else {
            pal().text_muted
        },
        d.kind,
    );
    let x = rect.left() + 52.0;
    p.text(
        egui::pos2(x, rect.top() + 20.0),
        egui::Align2::LEFT_CENTER,
        short_name(&d.name),
        egui::FontId::proportional(13.5),
        if selected {
            pal().text
        } else {
            pal().text.gamma_multiply(0.85)
        },
    );
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
            None => (kind_label(d.kind).to_string(), pal().text_faint),
        }
    } else {
        ("Unavailable".into(), theme::pal().danger)
    };
    p.circle_filled(
        egui::pos2(x + 3.0, rect.top() + 39.0),
        3.0,
        if d.connected {
            color
        } else {
            theme::pal().danger
        },
    );
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Group(SettingGroup),
    /// Host-side remaps; works for any mouse / keyboard.
    Bindings,
}

impl Tab {
    fn label(self, kind: DeviceKind) -> &'static str {
        match (self, kind) {
            (Tab::Group(g), _) => g.label(),
            (Tab::Bindings, DeviceKind::Mouse) => "Buttons",
            (Tab::Bindings, _) => "Key bindings",
        }
    }
}

/// One tab per setting group the driver declares (in group order), then remaps.
fn tabs_for(d: &DeviceSnapshot) -> Vec<Tab> {
    let mut groups: Vec<SettingGroup> = controls::visible(d).map(|m| m.key.group()).collect();
    groups.sort();
    groups.dedup();
    let mut tabs: Vec<Tab> = groups.into_iter().map(Tab::Group).collect();
    if matches!(d.kind, DeviceKind::Mouse | DeviceKind::Keyboard) {
        tabs.push(Tab::Bindings);
    }
    tabs
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
        if tabs.is_empty() {
            widgets::section(
                ui,
                "No controls",
                |_| {},
                |ui| {
                    widgets::hint(ui, "This device has no adjustable settings yet.");
                },
            );
            return;
        }
        let labels: Vec<&str> = tabs.iter().map(|t| t.label(d.kind)).collect();
        let tab_id = egui::Id::new(("tab", d.id.key()));
        let mut tab = ui
            .ctx()
            .data(|m| m.get_temp::<usize>(tab_id))
            .unwrap_or(0)
            .min(tabs.len() - 1);
        if let Some(i) = tab_bar(ui, tab_id, &labels, tab) {
            tab = i;
            ui.ctx().data_mut(|m| m.insert_temp(tab_id, tab));
        }
        ui.add_space(theme::SPACE_LG);
        widgets::page_transition(ui, (d.id.key(), tab));

        let group = match tabs[tab] {
            Tab::Bindings if d.kind == DeviceKind::Mouse => {
                return bindings::mouse_page(ui, d, remaps, out)
            }
            Tab::Bindings => return bindings::keyboard_page(ui, d, remaps, out),
            Tab::Group(g) => g,
        };
        let metas: Vec<&SettingMeta> = controls::visible(d)
            .filter(|m| m.key.group() == group)
            .collect();
        // Keys with a dedicated widget; everything else renders as plain rows.
        let special = |m: &&SettingMeta| {
            matches!(
                m.key,
                SettingKey::Dpi
                    | SettingKey::ReportRate
                    | SettingKey::ChatMixGame
                    | SettingKey::ChatMixChat
            )
        };
        let rest: Vec<&SettingMeta> = metas.iter().copied().filter(|m| !special(m)).collect();
        match group {
            SettingGroup::Lighting => lighting_tab(ui, d, &metas, out),
            SettingGroup::LineIn => self.line_in_tab(ui, d, out),
            SettingGroup::Performance => {
                if let Some(m) = controls::meta(d, SettingKey::Dpi) {
                    widgets::section(
                        ui,
                        "Sensitivity",
                        |_| {},
                        |ui| controls::gated(ui, d, m, |ui| controls::dpi(ui, d, m, out)),
                    );
                }
                if let Some(m) = controls::meta(d, SettingKey::ReportRate) {
                    widgets::section(
                        ui,
                        "Polling rate",
                        |_| {},
                        |ui| {
                            controls::gated(ui, d, m, |ui| {
                                ui.horizontal_wrapped(|ui| controls::choice(ui, d, m, out));
                            });
                            ui.add_space(theme::SPACE_XS);
                            widgets::hint(ui, "Higher rates lower latency but cost CPU; 8000 Hz needs a direct USB port.");
                        },
                    );
                }
            }
            SettingGroup::Audio => {
                if controls::meta(d, SettingKey::ChatMixGame).is_some() {
                    widgets::section(
                        ui,
                        "ChatMix",
                        |_| {},
                        |ui| {
                            controls::chatmix(ui, d);
                            ui.add_space(theme::SPACE_XS);
                            widgets::hint(ui, "Set by the headset's ChatMix dial. Route games to Hako_Game and voice apps to Hako_Chat.");
                        },
                    );
                }
            }
            SettingGroup::Microphone => {
                widgets::section(
                    ui,
                    "Microphone",
                    |_| {},
                    |ui| {
                        controls::rows(ui, d, &rest, out);
                        ui.add_space(theme::SPACE_XS);
                        widgets::hint(
                            ui,
                            "Affects the boom mic only, not headset playback volume.",
                        );
                    },
                );
                return;
            }
            SettingGroup::Power => {}
            SettingGroup::Memory => {
                let saved: Vec<&str> = controls::visible(d)
                    .filter(|m| m.persistent && m.blocked.is_none())
                    .map(|m| m.label.as_str())
                    .collect();
                widgets::section(
                    ui,
                    "Device memory",
                    |_| {},
                    |ui| {
                        controls::rows(ui, d, &rest, out);
                        ui.add_space(theme::SPACE_XS);
                        widgets::hint(
                            ui,
                            &format!(
                                "When allowed, these settings are written to the device itself: {}. \
                                 Writes happen only when you change one of them; profiles never include them.",
                                saved.join(", ")
                            ),
                        );
                    },
                );
                return;
            }
        }
        // Lighting / Line-in draw their own rows; elsewhere leftovers go in a section.
        if !matches!(group, SettingGroup::Lighting | SettingGroup::LineIn) && !rest.is_empty() {
            widgets::section(
                ui,
                group.label(),
                |_| {},
                |ui| controls::rows(ui, d, &rest, out),
            );
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
                    for (k, v) in [
                        ("Version", format!("v{}", st.health.version)),
                        ("Uptime", fmt_uptime(st.health.uptime_secs)),
                        ("Devices", st.health.adapter_count.to_string()),
                    ] {
                        widgets::divider(ui);
                        widgets::row(ui, k, None, |ui| {
                            ui.label(egui::RichText::new(v).color(pal().text_muted));
                        });
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
                    &format!("Hako v{}", env!("CARGO_PKG_VERSION")),
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
                    Some("Removes Hako sinks and line-in links, restores the default output"),
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
                    "Hako: one control panel for your keyboard, mouse, and headset on Linux.",
                );
                widgets::hint(ui, "MIT License. Device protocols reimplemented from public notes (OpenRazer, Logitech HID++, HeadsetControl).");
            },
        );
    }
}

fn lighting_tab(
    ui: &mut egui::Ui,
    d: &DeviceSnapshot,
    metas: &[&SettingMeta],
    out: &mut Vec<UiCommand>,
) {
    let effect = metas.iter().find(|m| m.key == SettingKey::Effect).copied();
    if let Some(m) = effect {
        widgets::section(
            ui,
            "Effect",
            |_| {},
            |ui| controls::gated(ui, d, m, |ui| controls::effect_tiles(ui, d, m, out)),
        );
    }
    let rest: Vec<&SettingMeta> = metas
        .iter()
        .filter(|m| m.key != SettingKey::Effect)
        .copied()
        .collect();
    if !rest.is_empty() {
        widgets::section(
            ui,
            "Customize",
            |_| {},
            |ui| controls::rows(ui, d, &rest, out),
        );
    }
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
        let sub = if b.charging {
            format!("{} · charging", b.label)
        } else {
            b.label.clone()
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
        ui.add_space(theme::SPACE_XL * 3.0);
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
