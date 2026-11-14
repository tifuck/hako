//! Network page: LAN audio with other hako machines. Turn it on, pair
//! (Accept / Deny with a 6-digit code on both screens), then each paired
//! machine's link and receive buffer. Edits go out as `NetOp`s and are
//! applied to the local copy first; the daemon's state is adopted once
//! nothing is in flight.

use crate::theme::{self, pal};
use crate::widgets::{self, Tone};
use crate::{HakoAppUi, UiCommand};
use egui::{pos2, vec2, Color32, Rect, Stroke, Ui};
use hako_core::net::{
    validate_name, NetOp, NetState, PairRequest, PeerState, DEFAULT_PORT, DISCOVERY_PORT,
    JITTER_MAX_MS, JITTER_MIN_MS,
};

/// The network state as the page shows it.
#[derive(Default)]
pub struct NetUi {
    /// Daemon state plus local edits not yet answered.
    state: Option<NetState>,
    /// Newest daemon state, adopted once nothing is in flight.
    remote: Option<NetState>,
    /// Why the daemon couldn't report it (e.g. a hakod without network audio).
    error: Option<String>,
    /// `PatchNet` calls sent and not answered yet (the client sets this).
    pub busy: bool,
    /// Incoming requests already announced with a toast.
    announced: Vec<String>,
    /// "Connect to address" draft.
    address: String,
    /// Paired machine whose Forget waits for a second click.
    forget: Option<String>,
}

impl NetUi {
    /// A `GetNet` / `PatchNet` answer or a `NetUpdated` event.
    pub fn receive(&mut self, state: NetState) {
        self.remote = Some(state);
        self.error = None;
    }

    /// `GetNet` failed: the page explains instead of showing stale peers.
    pub fn unavailable(&mut self, why: String) {
        self.state = None;
        self.remote = None;
        self.error = Some(why);
    }

    pub fn state(&self) -> Option<&NetState> {
        self.state.as_ref()
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

    /// Status line under "Network" in the rail, and its dot colour.
    pub(crate) fn rail_status(&self) -> (String, Color32) {
        match (&self.state, &self.error) {
            (None, Some(_)) => ("Not available".into(), pal().text_faint),
            (None, None) => ("Loading…".into(), pal().text_faint),
            (Some(s), _) if s.requests.iter().any(|r| r.incoming) => {
                ("Pairing request".into(), pal().accent)
            }
            (Some(s), _) if !s.enabled => ("Off".into(), pal().text_faint),
            (Some(s), _) if s.problem.is_some() => ("Problem".into(), pal().warn),
            (Some(s), _) => match s.peers.iter().filter(|p| p.connected).count() {
                0 => ("On".into(), pal().ok),
                n => (format!("{n} connected"), pal().ok),
            },
        }
    }
}

/// What an op changes locally until the daemon answers.
fn apply_local(st: &mut NetState, op: &NetOp) {
    match op {
        NetOp::SetEnabled { on } => st.enabled = *on,
        // Empty means the hostname, which only the daemon knows.
        NetOp::SetName { name } if !name.is_empty() => st.name.clone_from(name),
        NetOp::Respond { id, .. } => st.requests.retain(|r| &r.id != id),
        NetOp::Forget { id } => st.peers.retain(|p| &p.id != id),
        NetOp::SetJitter { id, jitter_ms } => {
            if let Some(p) = st.peers.iter_mut().find(|p| &p.id == id) {
                p.jitter_ms = *jitter_ms;
            }
        }
        NetOp::SetName { .. } | NetOp::Pair { .. } => {}
    }
}

/// "123456" as "123 456"; anything else as sent.
fn fmt_code(code: &str) -> String {
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// Two machines linked through a third, for the rail.
pub fn icon_network(p: &egui::Painter, r: Rect, c: Color32) {
    let top = pos2(r.center().x, r.top() + 3.0);
    let left = pos2(r.left() + 3.0, r.bottom() - 3.0);
    let right = pos2(r.right() - 3.0, r.bottom() - 3.0);
    let s = Stroke::new(1.5_f32, c);
    p.line_segment([top, left], s);
    p.line_segment([top, right], s);
    p.line_segment([left, right], s);
    for at in [top, left, right] {
        p.circle_filled(at, 3.2, c);
    }
}

impl HakoAppUi {
    /// Adopt daemon state and toast incoming requests not seen yet (unless
    /// the page is open: the card is there).
    pub(crate) fn net_frame(&mut self, dragging: bool) {
        self.net.sync(dragging);
        let Some(st) = &self.net.state else {
            return;
        };
        let incoming: Vec<&PairRequest> = st.requests.iter().filter(|r| r.incoming).collect();
        let fresh: Vec<String> = incoming
            .iter()
            .filter(|r| !self.net.announced.contains(&r.id))
            .map(|r| r.name.clone())
            .collect();
        self.net.announced = incoming.iter().map(|r| r.id.clone()).collect();
        if !(self.show_net && !self.show_settings) {
            for name in fresh {
                self.notify(Tone::Info, format!("{name} wants to pair — open Network"));
            }
        }
    }

    pub(crate) fn net_page(&mut self, ui: &mut Ui, out: &mut Vec<UiCommand>) {
        let mut ops = Vec::new();
        let st = self.net.state.clone();
        let mut on = st.as_ref().is_some_and(|s| s.enabled);
        ui.horizontal(|ui| {
            ui.set_min_height(theme::CONTROL_H);
            ui.label(
                egui::RichText::new("Network audio")
                    .heading()
                    .color(pal().text),
            );
            if st.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let resp = widgets::switch(ui, &mut on);
                    resp.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Checkbox,
                            true,
                            on,
                            "Network audio",
                        )
                    });
                    if resp.changed() {
                        ops.push(NetOp::SetEnabled { on });
                    }
                });
            }
        });
        ui.label(
            egui::RichText::new(
                "Stream audio to and from other computers running hako on this network.",
            )
            .color(pal().text_muted),
        );
        ui.add_space(theme::SPACE_LG);

        let Some(st) = st else {
            let body = self.net.error.as_deref().unwrap_or("Loading…");
            widgets::section(
                ui,
                "Not available",
                |_| {},
                |ui| {
                    ui.label(egui::RichText::new(body).color(pal().text_muted));
                },
            );
            return;
        };
        if let Some(p) = &st.problem {
            ui.colored_label(pal().danger, p);
            ui.add_space(theme::SPACE_MD);
        }
        if !st.enabled {
            let port = if st.port == 0 { DEFAULT_PORT } else { st.port };
            widgets::hint(
                ui,
                &format!(
                    "Turning it on opens port {port} on this computer. A firewall must allow {DISCOVERY_PORT}/udp and {port}/tcp+udp."
                ),
            );
            ui.add_space(theme::SPACE_MD);
        }

        this_computer(ui, &st, &mut ops);
        if st.enabled {
            for r in &st.requests {
                request_card(ui, r, &mut ops);
            }
        }
        if !st.peers.is_empty() {
            self.paired(ui, &st, &mut ops);
        }
        if st.enabled {
            self.nearby(ui, &st, &mut ops);
        }

        if let Some(local) = &mut self.net.state {
            for op in &ops {
                apply_local(local, op);
            }
        }
        if !ops.is_empty() {
            out.push(UiCommand::PatchNet(ops));
        }
    }

    fn paired(&mut self, ui: &mut Ui, st: &NetState, ops: &mut Vec<NetOp>) {
        widgets::section(
            ui,
            "Paired",
            |_| {},
            |ui| {
                for (k, p) in st.peers.iter().enumerate() {
                    if k > 0 {
                        widgets::divider(ui);
                    }
                    self.peer_row(ui, p, ops);
                }
                ui.add_space(theme::SPACE_SM);
                widgets::hint(
                    ui,
                    // ponytail: the UI font has no arrows, so the node names
                    // ("hako → name" / "hako ← name") are described instead.
                    "In the Mixer, each connected computer is an output device (audio sent to it) and an input source (audio it sends).",
                );
            },
        );
    }

    fn peer_row(&mut self, ui: &mut Ui, p: &PeerState, ops: &mut Vec<NetOp>) {
        ui.push_id(&p.id, |ui| {
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                ui.set_min_height(theme::CONTROL_H);
                ui.label(egui::RichText::new(&p.name).strong().color(pal().text));
                if p.connected {
                    widgets::pill(ui, "Connected", pal().ok);
                } else {
                    widgets::pill(ui, "Offline", pal().text_faint);
                }
                if let Some(a) = &p.address {
                    ui.label(egui::RichText::new(a).small().color(pal().text_muted));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.net.forget.as_deref() == Some(p.id.as_str()) {
                        if widgets::button(ui, "Cancel").clicked() {
                            self.net.forget = None;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Forget").color(pal().on_accent),
                                )
                                .fill(pal().danger)
                                .min_size(vec2(theme::BUTTON_MIN_W, theme::CONTROL_H)),
                            )
                            .on_hover_text("Unpair: pairing again needs a new Accept")
                            .clicked()
                        {
                            ops.push(NetOp::Forget { id: p.id.clone() });
                            self.net.forget = None;
                        }
                    } else if widgets::button(ui, "Forget").clicked() {
                        self.net.forget = Some(p.id.clone());
                    }
                });
            });
            if let Some(l) = p.link.filter(|_| p.connected) {
                ui.horizontal(|ui| {
                    widgets::pill(
                        ui,
                        "Sending",
                        if l.sending {
                            pal().accent
                        } else {
                            pal().text_faint
                        },
                    );
                    widgets::pill(
                        ui,
                        "Receiving",
                        if l.receiving {
                            pal().ok
                        } else {
                            pal().text_faint
                        },
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "buffer {:.0} ms · {} dropout{} · round trip {:.1} ms",
                            l.buffered_ms,
                            l.dropouts,
                            if l.dropouts == 1 { "" } else { "s" },
                            l.rtt_ms
                        ))
                        .small()
                        .color(pal().text_muted),
                    );
                });
            }
            if let Some(problem) = &p.problem {
                ui.colored_label(pal().warn, problem);
            }
            widgets::row(
                ui,
                "Receive buffer",
                Some("More absorbs Wi-Fi hiccups, less is lower latency"),
                |ui| {
                    let (min, max) = (JITTER_MIN_MS as f32, JITTER_MAX_MS as f32);
                    let mut ms = p.jitter_ms as f32;
                    ui.label(widgets::value_text(format!("{} ms", p.jitter_ms)));
                    let resp = widgets::slider(
                        ui,
                        egui::Slider::new(&mut ms, min..=max)
                            .logarithmic(true)
                            .show_value(false)
                            .clamping(egui::SliderClamping::Always),
                        theme::SLIDER_W,
                    );
                    widgets::knob(ui, &resp, widgets::fraction(ms, min, max, true));
                    let op = NetOp::SetJitter {
                        id: p.id.clone(),
                        jitter_ms: ms.round() as u32,
                    };
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        ops.push(op);
                    } else if let Some(local) = self.net.state.as_mut().filter(|_| resp.changed()) {
                        // Dragging: the copy follows, the release is sent.
                        apply_local(local, &op);
                    }
                },
            );
        });
    }

    fn nearby(&mut self, ui: &mut Ui, st: &NetState, ops: &mut Vec<NetOp>) {
        widgets::section(
            ui,
            "Nearby",
            |_| {},
            |ui| {
                if st.nearby.is_empty() {
                    widgets::hint(ui, "Looking for other computers…");
                }
                for n in &st.nearby {
                    let asked = st.requests.iter().any(|r| r.id == n.id);
                    ui.push_id(&n.id, |ui| {
                        widgets::row(ui, &n.name, Some(&n.address), |ui| {
                            if ui
                                .add_enabled_ui(!asked, |ui| widgets::button(ui, "Pair"))
                                .inner
                                .clicked()
                            {
                                ops.push(NetOp::Pair {
                                    address: n.address.clone(),
                                    key: (!n.key.is_empty()).then(|| n.key.clone()),
                                });
                            }
                        });
                    });
                }
                widgets::divider(ui);
                widgets::row(
                    ui,
                    "Connect to address",
                    Some("For networks that hide other computers: host or host:port"),
                    |ui| {
                        let address = self.net.address.trim().to_string();
                        let connect = ui
                            .add_enabled_ui(!address.is_empty(), |ui| {
                                widgets::button(ui, "Connect")
                            })
                            .inner
                            .clicked();
                        let edit =
                            widgets::text_field(ui, &mut self.net.address, "192.168.1.20", 200.0);
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if !address.is_empty() && (connect || enter) {
                            ops.push(NetOp::Pair { address, key: None });
                            self.net.address.clear();
                        }
                    },
                );
            },
        );
    }
}

fn this_computer(ui: &mut Ui, st: &NetState, ops: &mut Vec<NetOp>) {
    widgets::section(
        ui,
        "This computer",
        |_| {},
        |ui| {
            widgets::row(
                ui,
                "Name",
                Some("Shown to other computers; empty uses the hostname"),
                |ui| {
                    if let Some(name) = name_field(ui, &st.name) {
                        ops.push(NetOp::SetName { name });
                    }
                },
            );
            if !st.id.is_empty() {
                widgets::hint(ui, &format!("Id {}", st.id));
            }
        },
    );
}

/// Name field that commits on Enter or when it loses focus; empty is valid
/// (the hostname).
fn name_field(ui: &mut Ui, name: &str) -> Option<String> {
    let id = egui::Id::new("net_name");
    let mut draft = ui
        .data(|m| m.get_temp::<String>(id))
        .unwrap_or_else(|| name.to_string());
    let resp = widgets::text_field(ui, &mut draft, "Hostname", 200.0);
    if resp.has_focus() {
        ui.data_mut(|m| m.insert_temp(id, draft));
        return None;
    }
    ui.data_mut(|m| m.remove_temp::<String>(id));
    let draft = draft.trim();
    (resp.lost_focus() && draft != name && (draft.is_empty() || validate_name(draft).is_ok()))
        .then(|| draft.to_string())
}

/// An incoming request (Accept / Deny) or one we sent (Cancel).
fn request_card(ui: &mut Ui, r: &PairRequest, ops: &mut Vec<NetOp>) {
    let mut frame = theme::card();
    if r.incoming {
        frame = frame.stroke(Stroke::new(1.5_f32, pal().accent));
    }
    ui.push_id(&r.id, |ui| {
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = if r.incoming {
                format!("{} wants to pair", r.name)
            } else {
                format!("Waiting for {} to accept", r.name)
            };
            ui.label(
                egui::RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(pal().text),
            );
            ui.label(
                egui::RichText::new(&r.address)
                    .small()
                    .color(pal().text_muted),
            );
            ui.add_space(theme::SPACE_SM);
            ui.label(
                egui::RichText::new(fmt_code(&r.code))
                    .size(if r.incoming { 34.0 } else { 24.0 })
                    .monospace()
                    .strong()
                    .color(pal().text),
            );
            ui.add_space(theme::SPACE_XS);
            widgets::hint(ui, &format!("Check that {} shows the same code", r.name));
            ui.add_space(theme::SPACE_MD);
            ui.horizontal(|ui| {
                let respond = |accept| NetOp::Respond {
                    id: r.id.clone(),
                    accept,
                };
                if r.incoming {
                    if widgets::primary_button(ui, "Accept").clicked() {
                        ops.push(respond(true));
                    }
                    if widgets::button(ui, "Deny").clicked() {
                        ops.push(respond(false));
                    }
                } else if widgets::button(ui, "Cancel").clicked() {
                    ops.push(respond(false));
                }
            });
        });
    });
    ui.add_space(theme::SPACE_MD);
}
