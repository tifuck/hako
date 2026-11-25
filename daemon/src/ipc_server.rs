//! Unix socket IPC server.

use crate::hub::DaemonHub;
use crate::mixer::MixerService;
use anyhow::{bail, Result};
use hako_audio::meters::{MeterService, MeterSubscription, MeterTarget};
use hako_audio::mixer::Side;
use hako_core::device::DeviceId;
use hako_core::ipc::{
    EventKind, IpcEvent, IpcRequest, IpcResponse, MeterLevels, MeterTargetSpec, Method,
    SubscribeMetersParams, MAX_METER_TARGETS,
};
use hako_core::protocol::AdapterEvent;
use hako_core::remap::CancelCaptureParams;
use hako_core::{read_json_frame, write_json_frame, FrameTooLarge};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::unix::UCred;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Semaphore};
use tracing::{info, warn};

/// Meter frame period (~30 Hz).
const METER_PERIOD: Duration = Duration::from_millis(33);
/// How often meter targets are resolved again (AUX turning on or off).
const METER_RESOLVE: Duration = Duration::from_secs(1);
/// A subscriber that has not taken an event / meter frame for this long
/// stopped reading: it is dropped (and its meter streams with it).
const PUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections served at once; more are refused until one closes.
/// ponytail: one cap for all of the user's clients, not per client.
const MAX_CLIENTS: usize = 64;
/// A request connection that sends nothing for this long is closed, so
/// leaked ones cannot take every slot (the UI polls every 2 s).
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Bind the daemon socket, owner-only. Refuses to replace a socket another
/// daemon still answers on; a stale file from a crashed daemon is removed.
/// The returned lock on `lock_path` (held for the daemon's life) keeps two
/// daemons started at the same moment from both getting past that check, and
/// one per user whatever socket each was given (they would kill each other's
/// audio processes and share the devices).
pub fn bind(socket_path: &Path, lock_path: &Path) -> Result<(UnixListener, std::fs::File)> {
    for dir in [socket_path.parent(), lock_path.parent()]
        .into_iter()
        .flatten()
    {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    // SAFETY: valid open fd; flock has no other preconditions.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("hakod is already running (lock {})", lock_path.display());
    }
    if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
        bail!("hakod is already running on {}", socket_path.display());
    }
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    info!("listening on {}", socket_path.display());
    Ok((listener, lock))
}

pub async fn serve(listener: UnixListener, hub: Arc<DaemonHub>) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let mut full = false;
    loop {
        // Accept errors (out of fds, aborted handshake) are per connection;
        // they must not stop the daemon.
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let cred = stream.peer_cred();
        if !peer_allowed(&cred, uid) {
            warn!("rejected IPC peer: {cred:?}");
            continue;
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            if !full {
                warn!("{MAX_CLIENTS} IPC clients connected; refusing more");
            }
            full = true;
            tokio::spawn(refuse(stream));
            continue;
        };
        full = false;
        let hub = hub.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Err(e) = handle_client(stream, hub).await {
                warn!("client: {e}");
            }
        });
    }
}

/// Answer a connection over the limit with a `busy` error, not a bare close.
/// Its request is read first: closed before it sends it, the client would
/// only see a broken pipe.
async fn refuse(mut stream: UnixStream) {
    let id = match tokio::time::timeout(PUSH_TIMEOUT, read_json_frame(&mut stream)).await {
        Ok(Ok(v)) => v.get("id").cloned().unwrap_or_default(),
        _ => serde_json::Value::Null,
    };
    let msg = format!("hakod serves {MAX_CLIENTS} connections at once; try again later");
    let resp = IpcResponse::err(id, "busy", msg);
    let _ = push(write_json_frame(&mut stream, &resp), PUSH_TIMEOUT).await;
}

/// Clients can inject keystrokes (macros), so only our own user may connect.
fn peer_allowed(cred: &std::io::Result<UCred>, uid: u32) -> bool {
    matches!(cred, Ok(c) if c.uid() == uid)
}

async fn handle_client(mut stream: UnixStream, hub: Arc<DaemonHub>) -> Result<()> {
    let mut captures = Vec::new();
    let result = serve_requests(&mut stream, &hub, &mut captures, IDLE_TIMEOUT).await;
    // Listening must not outlive the client that asked for it (UI closed or
    // crashed mid-capture): input would stay ungrabbed and, for a recording,
    // every keyboard would keep being read.
    for capture in captures {
        hub.end_capture(capture);
    }
    result
}

/// Request loop of one connection. `captures` tracks the device and session
/// of the latest capture this connection started on each device. Ends when
/// no request comes for `idle`.
async fn serve_requests(
    stream: &mut UnixStream,
    hub: &DaemonHub,
    captures: &mut Vec<(DeviceId, u64)>,
    idle: Duration,
) -> Result<()> {
    loop {
        let Ok(read) = tokio::time::timeout(idle, read_json_frame(stream)).await else {
            info!("IPC client idle for {} s; closed", idle.as_secs());
            break;
        };
        let v = match read {
            Ok(v) => v,
            Err(e) => {
                // A bad frame gets an answer before the close.
                if !client_left(&e) {
                    warn!("client sent a bad frame: {e:#}");
                    let resp = IpcResponse::err(
                        serde_json::json!(null),
                        "invalid_value",
                        format!("{e:#}"),
                    );
                    let _ = push(write_json_frame(stream, &resp), PUSH_TIMEOUT).await;
                }
                break;
            }
        };
        // Event messages from server use type field; client requests use method
        let req: IpcRequest = match serde_json::from_value(v) {
            Ok(r) => r,
            Err(e) => {
                let resp =
                    IpcResponse::err(serde_json::json!(null), "invalid_value", e.to_string());
                push(write_json_frame(stream, &resp), PUSH_TIMEOUT).await?;
                continue;
            }
        };

        // A subscribed connection is events-only from here on.
        if req.method == Method::SubscribeEvents {
            let resp = IpcResponse::ok(req.id.clone(), serde_json::json!({"subscribed": true}));
            push(write_json_frame(stream, &resp), PUSH_TIMEOUT).await?;
            stream_events(stream, hub.subscribe()).await?;
            break;
        }

        if req.method == Method::SubscribeMeters {
            let targets = match serde_json::from_value::<SubscribeMetersParams>(req.params) {
                Ok(p) if p.targets.len() <= MAX_METER_TARGETS => p.targets,
                Ok(_) => {
                    let msg = format!("at most {MAX_METER_TARGETS} meter targets");
                    let resp = IpcResponse::err(req.id, "invalid_value", msg);
                    push(write_json_frame(stream, &resp), PUSH_TIMEOUT).await?;
                    continue;
                }
                Err(e) => {
                    let resp = IpcResponse::err(req.id, "invalid_value", e.to_string());
                    push(write_json_frame(stream, &resp), PUSH_TIMEOUT).await?;
                    continue;
                }
            };
            let resp = IpcResponse::ok(req.id.clone(), serde_json::json!({"subscribed": true}));
            push(write_json_frame(stream, &resp), PUSH_TIMEOUT).await?;
            stream_meters(stream, &hub.mixer, &hub.meters, &targets).await;
            break;
        }

        let resp = match req.method {
            Method::StartCapture => {
                let (resp, started) = hub.start_capture(req);
                if let Some((device, session)) = started {
                    captures.retain(|(d, _)| *d != device);
                    captures.push((device, session));
                }
                resp
            }
            Method::CancelCapture => {
                let params = req.params.clone();
                let resp = hub.handle(req).await;
                if resp.ok {
                    forget_cancelled(captures, params);
                }
                resp
            }
            _ => hub.handle(req).await,
        };
        reply(stream, &resp).await?;
    }
    Ok(())
}

/// Write a reply. One too large for a frame is answered with an error
/// instead: dropping the connection would leave the client retrying the same
/// request forever.
async fn reply(stream: &mut UnixStream, resp: &IpcResponse) -> Result<()> {
    // A client that stops reading must not hold this task (and its
    // captures) forever.
    match push(write_json_frame(stream, resp), PUSH_TIMEOUT).await {
        Err(e) if e.is::<FrameTooLarge>() => {
            warn!("reply to {}: {e}", resp.id);
            let err = IpcResponse::err(resp.id.clone(), "io", "response too large");
            push(write_json_frame(stream, &err), PUSH_TIMEOUT).await
        }
        written => written,
    }
}

/// Events until the client leaves: a failed push, or EOF on its side even
/// with no event to send (it sends nothing after subscribing).
async fn stream_events(
    stream: &mut UnixStream,
    mut rx: broadcast::Receiver<AdapterEvent>,
) -> Result<()> {
    let mut buf = [0u8; 64];
    loop {
        let ev = tokio::select! {
            ev = rx.recv() => ev,
            read = stream.read(&mut buf) => match read {
                Ok(n) if n > 0 => continue,
                _ => return Ok(()),
            },
        };
        let ev = match ev {
            Ok(ev) => ev,
            // A slow client only missed some events; keep streaming.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("event subscriber lagged, dropped {n} event(s)");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let last = matches!(ev, AdapterEvent::ShuttingDown);
        match push(write_json_frame(stream, &to_ipc_event(ev)?), PUSH_TIMEOUT).await {
            // Nothing was written: the client catches up on its next poll.
            Err(e) if e.is::<FrameTooLarge>() => warn!("event skipped: {e}"),
            Err(_) => return Ok(()),
            Ok(()) => {}
        }
        if last {
            return Ok(());
        }
    }
}

/// Stop tracking the capture of the device a successful `CancelCapture`
/// with these params named (others keep running).
fn forget_cancelled(captures: &mut Vec<(DeviceId, u64)>, params: serde_json::Value) {
    if let Ok(p) = serde_json::from_value::<CancelCaptureParams>(params) {
        captures.retain(|(device, _)| *device != p.device_id);
    }
}

/// Meter frames until the client leaves (a failed write). Its streams go
/// with it: the subscription is dropped on return.
async fn stream_meters(
    stream: &mut UnixStream,
    mixer: &Arc<MixerService>,
    meters: &MeterService,
    specs: &[MeterTargetSpec],
) {
    let mut tick = tokio::time::interval(METER_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut resolved: Vec<Option<MeterTarget>> = Vec::new();
    let mut sub: Option<MeterSubscription> = None;
    let mut resolve_at = Instant::now();
    loop {
        tick.tick().await;
        if Instant::now() >= resolve_at {
            resolve_at = Instant::now() + METER_RESOLVE;
            // The getters read the published status, never waiting on audio work.
            let (mixer, owned) = (mixer.clone(), specs.to_vec());
            let Ok(now) = tokio::task::spawn_blocking(move || {
                owned
                    .iter()
                    .map(|s| resolve_meter(&mixer, s))
                    .collect::<Vec<_>>()
            })
            .await
            else {
                break;
            };
            if now != resolved {
                // Subscribed before the old one drops: shared streams stay up.
                let targets: Vec<MeterTarget> = now.iter().flatten().cloned().collect();
                sub = (!targets.is_empty()).then(|| meters.subscribe(targets));
                resolved = now;
            }
        }
        let read = sub.as_ref().map(|s| s.read()).unwrap_or_default();
        let ev = IpcEvent {
            kind: EventKind::Meters,
            payload: serde_json::to_value(meter_frame(&resolved, read, specs.len()))
                .unwrap_or_default(),
        };
        if push(write_json_frame(stream, &ev), PUSH_TIMEOUT)
            .await
            .is_err()
        {
            break;
        }
    }
}

/// Finish a frame write; fails if it is not taken in time.
async fn push(write: impl std::future::Future<Output = Result<()>>, limit: Duration) -> Result<()> {
    match tokio::time::timeout(limit, write).await {
        Ok(written) => written,
        Err(_) => {
            warn!("client stopped reading; dropped");
            bail!("client stopped reading")
        }
    }
}

fn resolve_meter(mixer: &MixerService, spec: &MeterTargetSpec) -> Option<MeterTarget> {
    let (node_name, monitor_sink) = match spec {
        // The headset's old line-in page: line-in is a mixer input now.
        MeterTargetSpec::AuxInput | MeterTargetSpec::AuxOutput => return None,
        MeterTargetSpec::Node { name, monitor_sink } => (name.clone(), *monitor_sink),
        // The strip's post-fader (playback) node, captured like a source.
        MeterTargetSpec::MixerInput { id } => (mixer.meter_node(Side::Input, id)?, false),
        MeterTargetSpec::MixerOutput { id } => (mixer.meter_node(Side::Output, id)?, false),
    };
    Some(MeterTarget {
        node_name,
        monitor_sink,
    })
}

/// `read` holds the resolved targets' peaks in order; unresolved targets
/// read 0. Rounded to 3 decimals to keep frames small.
fn meter_frame(resolved: &[Option<MeterTarget>], read: Vec<[f32; 2]>, n: usize) -> MeterLevels {
    let mut read = read.into_iter();
    let round = |v: f32| (v * 1000.0).round() / 1000.0;
    let levels = (0..n)
        .map(|i| match resolved.get(i) {
            Some(Some(_)) => read.next().unwrap_or_default().map(round),
            _ => [0.0, 0.0],
        })
        .collect();
    MeterLevels { levels }
}

/// The read failed because the client went away (EOF, reset), not because
/// it sent something malformed.
fn client_left(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| {
        use std::io::ErrorKind::*;
        matches!(e.kind(), UnexpectedEof | ConnectionReset | BrokenPipe)
    })
}

fn to_ipc_event(ev: AdapterEvent) -> Result<IpcEvent> {
    let (kind, payload) = match ev {
        AdapterEvent::Updated(s) => (EventKind::DeviceUpdated, serde_json::to_value(s)?),
        AdapterEvent::Connected(s) => (EventKind::DeviceConnected, serde_json::to_value(s)?),
        AdapterEvent::Disconnected(id) => {
            (EventKind::DeviceDisconnected, serde_json::to_value(id)?)
        }
        AdapterEvent::RemapsUpdated(s) => (EventKind::RemapsUpdated, serde_json::to_value(s)?),
        AdapterEvent::RemapCaptured(s) => (EventKind::RemapCaptured, serde_json::to_value(s)?),
        AdapterEvent::MixerUpdated(s) => (EventKind::MixerUpdated, serde_json::to_value(s)?),
        AdapterEvent::NetUpdated(s) => (EventKind::NetUpdated, serde_json::to_value(s)?),
        AdapterEvent::HotkeysUpdated(s) => (EventKind::HotkeysUpdated, serde_json::to_value(s)?),
        AdapterEvent::HotkeyCapture(s) => (EventKind::HotkeyCapture, serde_json::to_value(s)?),
        AdapterEvent::VoiceUpdated(s) => (EventKind::VoiceUpdated, serde_json::to_value(s)?),
        AdapterEvent::VoicePartial(s) => (EventKind::VoicePartial, serde_json::to_value(s)?),
        AdapterEvent::Error { message, .. } => {
            (EventKind::Error, serde_json::json!({ "message": message }))
        }
        AdapterEvent::ShuttingDown => (EventKind::DaemonShuttingDown, serde_json::json!({})),
    };
    Ok(IpcEvent { kind, payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closed_client_is_not_a_bad_frame() {
        use tokio::io::AsyncWriteExt;
        let (mut server, client) = UnixStream::pair().unwrap();
        drop(client);
        assert!(client_left(
            &read_json_frame(&mut server).await.unwrap_err()
        ));

        let (mut server, mut client) = UnixStream::pair().unwrap();
        client.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        assert!(!client_left(
            &read_json_frame(&mut server).await.unwrap_err()
        ));
    }

    #[tokio::test]
    async fn only_our_own_user_may_connect() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let ours = stream.peer_cred();
        let uid = ours.as_ref().unwrap().uid();
        assert!(peer_allowed(&ours, uid));
        assert!(!peer_allowed(&ours, uid.wrapping_add(1)));
        assert!(!peer_allowed(&Err(std::io::Error::other("no creds")), uid));
    }

    #[tokio::test]
    async fn a_reply_too_large_for_a_frame_is_an_error_not_a_close() {
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let big = IpcResponse::ok(serde_json::json!(7), "x".repeat(hako_core::MAX_FRAME_BYTES));
        let read = tokio::time::timeout(Duration::from_secs(2), read_json_frame(&mut client));
        let (wrote, read) = tokio::join!(reply(&mut server, &big), read);
        wrote.unwrap();
        let resp: IpcResponse = serde_json::from_value(read.unwrap().unwrap()).unwrap();
        assert_eq!(resp.id, serde_json::json!(7));
        assert_eq!(resp.error.unwrap().code, "io");
    }

    /// Through `serve` on a real socket: a ping is answered; a frame header
    /// over the limit gets an error reply, then the connection closes.
    #[tokio::test]
    async fn serve_answers_a_ping_and_refuses_an_oversize_frame() {
        use tokio::io::AsyncWriteExt;
        let within = Duration::from_secs(5);
        let hub = Arc::new(crate::hub::tests::hub("ipc-serve"));
        let sock = hub.paths.root.join("d.sock");
        let server = tokio::spawn(serve(UnixListener::bind(&sock).unwrap(), hub.clone()));

        let mut pinger = UnixStream::connect(&sock).await.unwrap();
        let ping = IpcRequest {
            id: serde_json::json!(1),
            method: Method::Ping,
            params: serde_json::Value::Null,
        };
        write_json_frame(&mut pinger, &ping).await.unwrap();
        let v = tokio::time::timeout(within, read_json_frame(&mut pinger)).await;
        let resp: IpcResponse = serde_json::from_value(v.unwrap().unwrap()).unwrap();
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.id, serde_json::json!(1));
        assert_eq!(resp.result.unwrap()["pong"], true);

        let mut big = UnixStream::connect(&sock).await.unwrap();
        let len = hako_core::MAX_FRAME_BYTES as u32 + 1;
        big.write_all(&len.to_le_bytes()).await.unwrap();
        let v = tokio::time::timeout(within, read_json_frame(&mut big)).await;
        let resp: IpcResponse = serde_json::from_value(v.unwrap().unwrap()).unwrap();
        assert_eq!(resp.error.unwrap().code, "invalid_value");
        let mut rest = Vec::new();
        let n = tokio::time::timeout(within, big.read_to_end(&mut rest)).await;
        assert_eq!(n.unwrap().unwrap(), 0, "connection left open");

        server.abort();
        let _ = server.await;
        drop((pinger, big));
        // Each connection task lets go of the hub once it sees its client leave.
        let mut hub = hub;
        for _ in 0..500 {
            match Arc::try_unwrap(hub) {
                Ok(hub) => return crate::hub::tests::close(hub),
                Err(shared) => hub = shared,
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("a connection task outlived its client");
    }

    /// An idle request connection is closed; one over the limit is told why.
    #[tokio::test]
    async fn idle_connections_close_and_refused_ones_get_an_answer() {
        let hub = crate::hub::tests::hub("ipc-idle");
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let idle = Duration::from_millis(100);
        let mut captures = Vec::new();
        let served = serve_requests(&mut server, &hub, &mut captures, idle);
        assert!(tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .is_ok_and(|r| r.is_ok()));
        drop(server);
        let mut rest = Vec::new();
        assert_eq!(client.read_to_end(&mut rest).await.unwrap(), 0);
        crate::hub::tests::close(hub);

        let (server, mut client) = UnixStream::pair().unwrap();
        let refused = tokio::spawn(refuse(server));
        let ping = IpcRequest {
            id: serde_json::json!(9),
            method: Method::Ping,
            params: serde_json::Value::Null,
        };
        write_json_frame(&mut client, &ping).await.unwrap();
        let v = tokio::time::timeout(Duration::from_secs(5), read_json_frame(&mut client)).await;
        let resp: IpcResponse = serde_json::from_value(v.unwrap().unwrap()).unwrap();
        assert_eq!(resp.id, serde_json::json!(9));
        assert_eq!(resp.error.unwrap().code, "busy");
        refused.await.unwrap();
    }

    #[tokio::test]
    async fn a_subscriber_that_stops_reading_is_dropped() {
        let (mut server, _client) = UnixStream::pair().unwrap();
        let ev = IpcEvent {
            kind: EventKind::Meters,
            payload: serde_json::json!("x".repeat(256 * 1024)),
        };
        let limit = Duration::from_millis(100);
        let mut pushed = 0;
        while push(write_json_frame(&mut server, &ev), limit)
            .await
            .is_ok()
        {
            pushed += 1;
            assert!(pushed < 64, "the socket never filled up");
        }
    }

    #[tokio::test]
    async fn an_idle_subscriber_that_leaves_is_dropped_at_once() {
        let (mut server, client) = UnixStream::pair().unwrap();
        let (events, rx) = broadcast::channel(8);
        drop(client);
        // No event ever comes: only the client's EOF can end the stream.
        let ended = tokio::time::timeout(Duration::from_secs(1), stream_events(&mut server, rx));
        assert!(ended.await.is_ok(), "subscriber outlived its client");
        drop(events);
    }

    #[test]
    fn cancelling_another_devices_capture_keeps_ours_tracked() {
        let id = |product_id| DeviceId {
            vendor_id: 0x1234,
            product_id,
            instance: "x".into(),
        };
        let params = |product_id| serde_json::json!({ "device_id": id(product_id) });
        // Captures on two devices; each is forgotten only by its own cancel.
        let mut ours = vec![(id(1), 7), (id(2), 8)];
        forget_cancelled(&mut ours, params(3));
        forget_cancelled(&mut ours, serde_json::json!({}));
        assert_eq!(ours.len(), 2);
        forget_cancelled(&mut ours, params(1));
        assert_eq!(ours, [(id(2), 8)]);
    }

    #[test]
    fn meter_frames_keep_target_order_with_gaps() {
        let t = |n: &str| {
            Some(MeterTarget {
                node_name: n.into(),
                monitor_sink: false,
            })
        };
        let resolved = [t("a"), None, t("b")];
        let frame = meter_frame(&resolved, vec![[0.12345, 1.5], [0.0, 0.25]], 3);
        assert_eq!(frame.levels, vec![[0.123, 1.5], [0.0, 0.0], [0.0, 0.25]]);
        // Nothing resolved: every target reads 0.
        assert_eq!(meter_frame(&[], Vec::new(), 2).levels, vec![[0.0; 2]; 2]);
    }

    /// End to end over a socket pair: frames at ~30 Hz in target order, an
    /// unresolved target reads 0, and the meter stream goes with the client.
    #[tokio::test]
    #[ignore = "needs a live PipeWire session (pw-cli, pw-dump)"]
    async fn meter_stream_follows_its_client() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let meter_nodes = || {
            let dump = Command::new("pw-dump").output().unwrap();
            String::from_utf8_lossy(&dump.stdout)
                .matches(r#""node.name": "hako-meter""#)
                .count()
        };
        let sink = format!("hako-test-ipc-{}", std::process::id());
        let mut cli = Command::new("pw-cli")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        writeln!(
            cli.stdin.as_mut().unwrap(),
            "create-node adapter {{ factory.name=support.null-audio-sink node.name={sink} \
             media.class=Audio/Sink audio.position=[FL FR] object.linger=false }}"
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Never dropped: AudioService's Drop tears down the session's audio.
        let audio = Arc::new(hako_audio::AudioService::new());
        let meters = MeterService::new();
        // Off (no mixer.toml in an empty dir): never builds anything.
        let root = std::env::temp_dir().join(format!("hako-test-ipc-{}", std::process::id()));
        let paths = hako_core::config::ConfigPaths {
            config_toml: root.join("config.toml"),
            devices_dir: root.join("devices"),
            profiles_dir: root.join("profiles"),
            remaps_dir: root.join("remaps"),
            root,
        };
        let (events, _) = tokio::sync::broadcast::channel(8);
        let mixer = Arc::new(MixerService::start(paths, audio.clone(), events, 256));
        let specs = [
            MeterTargetSpec::AuxInput,
            MeterTargetSpec::Node {
                name: sink.clone(),
                monitor_sink: true,
            },
        ];
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let client_side = async {
            let started = Instant::now();
            for _ in 0..30 {
                let ev: IpcEvent =
                    serde_json::from_value(read_json_frame(&mut client).await.unwrap()).unwrap();
                assert_eq!(ev.kind, EventKind::Meters);
                let frame: MeterLevels = serde_json::from_value(ev.payload).unwrap();
                assert_eq!(frame.levels.len(), 2);
                assert_eq!(frame.levels[0], [0.0, 0.0], "legacy AUX meters read 0");
            }
            let elapsed = started.elapsed();
            let nodes = meter_nodes();
            drop(client);
            (elapsed, nodes)
        };
        let ((elapsed, nodes), ()) = tokio::join!(
            client_side,
            stream_meters(&mut server, &mixer, &meters, &specs)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after = meter_nodes();
        let _ = cli.kill();
        let _ = cli.wait();
        std::mem::forget(audio);

        assert!(
            (Duration::from_millis(800)..Duration::from_millis(1300)).contains(&elapsed),
            "30 frames took {elapsed:?}"
        );
        assert_eq!(nodes, 1, "one stream for the node target");
        assert_eq!(after, 0, "meter stream outlived its client");
    }

    #[tokio::test]
    async fn bind_is_owner_only_and_refuses_a_live_daemon() {
        let dir = std::env::temp_dir().join(format!("hako-test-{}", std::process::id()));
        let (sock, lock) = (dir.join("d.sock"), dir.join("d.lock"));
        let live = bind(&sock, &lock).unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            bind(&sock, &lock).is_err(),
            "second daemon must not steal the socket"
        );
        assert!(
            bind(&dir.join("other.sock"), &lock).is_err(),
            "nor run beside it on another socket"
        );
        drop(live);
        // Stale file (daemon gone) is replaced.
        assert!(bind(&sock, &lock).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
