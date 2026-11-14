//! Network page: rendering every state, and what its controls send.

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use hako_core::net::{LinkStats, NearbyPeer, NetOp, NetState, PairRequest, PeerState};
use hako_ui::{HakoAppUi, UiCommand};

fn peer(id: &str, name: &str, connected: bool) -> PeerState {
    PeerState {
        id: id.into(),
        name: name.into(),
        jitter_ms: 12,
        connected,
        address: Some("192.168.1.20:47821".into()),
        link: connected.then_some(LinkStats {
            buffered_ms: 11.6,
            dropouts: 2,
            receiving: true,
            sending: false,
            rtt_ms: 0.4,
        }),
        problem: None,
    }
}

fn sample() -> NetState {
    NetState {
        enabled: true,
        name: "Laptop".into(),
        id: "aabbccddeeff0011".into(),
        port: 47821,
        problem: None,
        peers: vec![
            peer("0011223344556677", "Desktop", true),
            peer("8899aabbccddeeff", "Studio", false),
        ],
        nearby: vec![NearbyPeer {
            id: "2233445566778899".into(),
            name: "Tower".into(),
            key: "22".repeat(32),
            address: "192.168.1.30:47821".into(),
        }],
        requests: vec![
            PairRequest {
                id: "1122334455667788".into(),
                name: "Couch".into(),
                address: "192.168.1.40:47821".into(),
                code: "123456".into(),
                incoming: true,
            },
            PairRequest {
                id: "2233445566778899".into(),
                name: "Tower".into(),
                address: "192.168.1.30:47821".into(),
                code: "654 321".into(),
                incoming: false,
            },
        ],
    }
}

fn app(net: Option<NetState>, page: bool) -> HakoAppUi {
    let mut app = HakoAppUi {
        daemon_error: None,
        show_net: page,
        ..Default::default()
    };
    if let Some(n) = net {
        app.net.receive(n);
    }
    app
}

/// Runs `script` against the page and returns what it sent.
fn drive(mut app: HakoAppUi, script: impl FnOnce(&mut Harness<'_>)) -> Vec<UiCommand> {
    let mut out = Vec::new();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(1180.0, 1400.0))
        .build(|ctx| app.ui(ctx, &mut out));
    harness.run_steps(3);
    script(&mut harness);
    harness.run_steps(2);
    drop(harness);
    out
}

fn patches(out: &[UiCommand]) -> Vec<NetOp> {
    out.iter()
        .filter_map(|c| match c {
            UiCommand::PatchNet(ops) => Some(ops.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn every_state_renders_without_sending() {
    let mut off = sample();
    off.enabled = false;
    let mut broken = sample();
    broken.problem = Some("port 47821 is taken".into());
    let mut alone = sample();
    alone.peers.clear();
    alone.nearby.clear();
    alone.requests.clear();
    let mut unavailable = app(None, true);
    unavailable
        .net
        .unavailable("This hakod has no network audio yet.".into());
    for (a, size) in [
        (app(None, true), [1180.0, 780.0]),
        (unavailable, [1180.0, 780.0]),
        (app(Some(off), true), [1180.0, 780.0]),
        (app(Some(broken), true), [1180.0, 780.0]),
        (app(Some(alone), true), [1180.0, 780.0]),
        (app(Some(sample()), true), [1180.0, 1400.0]),
        (app(Some(sample()), true), [900.0, 600.0]),
    ] {
        let mut a = a;
        let mut out = Vec::new();
        let mut h = Harness::builder()
            .with_size(egui::vec2(size[0], size[1]))
            .build(|ctx| a.ui(ctx, &mut out));
        h.run_steps(3);
        drop(h);
        assert!(out.is_empty(), "rendering sent {out:?}");
    }
}

#[test]
fn accepting_an_incoming_request_sends_respond() {
    let out = drive(app(Some(sample()), true), |h| {
        h.get_by_label("Couch wants to pair");
        h.get_by_label("123 456");
        h.get_by_label("Accept").click();
    });
    assert_eq!(
        patches(&out),
        [NetOp::Respond {
            id: "1122334455667788".into(),
            accept: true
        }]
    );
}

#[test]
fn pairing_a_nearby_machine_names_its_key() {
    let mut st = sample();
    st.requests.clear();
    let out = drive(app(Some(st), true), |h| {
        h.get_by_label("Pair").click();
    });
    assert_eq!(
        patches(&out),
        [NetOp::Pair {
            address: "192.168.1.30:47821".into(),
            key: Some("22".repeat(32)),
        }]
    );
}

#[test]
fn the_switch_turns_network_audio_on() {
    let mut off = sample();
    off.enabled = false;
    let out = drive(app(Some(off), true), |h| {
        h.get_by_role_and_label(egui::accesskit::Role::CheckBox, "Network audio")
            .click();
    });
    assert_eq!(patches(&out), [NetOp::SetEnabled { on: true }]);
}

#[test]
fn forget_needs_a_second_click() {
    let mut one = sample();
    one.peers.truncate(1);
    one.requests.clear();
    let out = drive(app(Some(one.clone()), true), |h| {
        h.get_by_label("Forget").click();
    });
    assert!(patches(&out).is_empty(), "{out:?}");
    let out = drive(app(Some(one), true), |h| {
        h.get_by_label("Forget").click();
        h.run_steps(2);
        h.get_by_label("Cancel");
        h.get_by_label("Forget").click();
    });
    assert_eq!(
        patches(&out),
        [NetOp::Forget {
            id: "0011223344556677".into()
        }]
    );
}

#[test]
fn a_new_incoming_request_toasts_once_unless_the_page_is_open() {
    let ctx = egui::Context::default();
    let mut out = Vec::new();
    let mut a = app(Some(sample()), false);
    for _ in 0..3 {
        let _ = ctx.run(Default::default(), |ctx| a.ui(ctx, &mut out));
    }
    let toasts: Vec<&str> = a.toasts.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(toasts, ["Couch wants to pair — open Network"]);

    let mut a = app(Some(sample()), true);
    let _ = ctx.run(Default::default(), |ctx| a.ui(ctx, &mut out));
    assert!(a.toasts.is_empty());
    // A request seen on the page is not announced after leaving it.
    a.show_net = false;
    let _ = ctx.run(Default::default(), |ctx| a.ui(ctx, &mut out));
    assert!(a.toasts.is_empty());
}
