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
use hako_core::{read_json_frame, write_json_frame};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Meter frame period (~30 Hz).
const METER_PERIOD: Duration = Duration::from_millis(33);
/// How often meter targets are resolved again (AUX turning on or off).
const METER_RESOLVE: Duration = Duration::from_secs(1);
/// A subscriber that has not taken an event / meter frame for this long
/// stopped reading: it is dropped (and its meter streams with it).
const PUSH_TIMEOUT: Duration = Duration::from_secs(5);

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
        // Clients can inject keystrokes (macros), so only our own user may connect.
        match stream.peer_cred() {
            Ok(cred) if cred.uid() == uid => {}
            other => {
                warn!("rejected IPC peer: {other:?}");
                continue;
            }
        }
        let hub = hub.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, hub).await {
                warn!("client: {e}");
            }
        });
    }
}

async fn handle_client(mut stream: UnixStream, hub: Arc<DaemonHub>) -> Result<()> {
    let mut captures = Vec::new();
    let result = serve_requests(&mut stream, &hub, &mut captures).await;
    // Listening must not outlive the client that asked for it (UI closed or
    // crashed mid-capture): input would stay ungrabbed and, for a recording,
    // every keyboard would keep being read.
    for capture in captures {
        hub.end_capture(capture);
    }
    result
}

/// Request loop of one connection. `captures` tracks the device and session
/// of the latest capture this connection started on each device.
async fn serve_requests(
    stream: &mut UnixStream,
    hub: &DaemonHub,
    captures: &mut Vec<(DeviceId, u64)>,
) -> Result<()> {
    loop {
        let v = match read_json_frame(stream).await {
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
                    let _ = write_json_frame(stream, &resp).await;
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
                write_json_frame(stream, &resp).await?;
                continue;
            }
        };

        // A subscribed connection is events-only from here on; the client
        // leaving shows up as a failed write.
        if req.method == Method::SubscribeEvents {
            let resp = IpcResponse::ok(req.id.clone(), serde_json::json!({"subscribed": true}));
            write_json_frame(stream, &resp).await?;
            let mut rx = hub.subscribe();
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    // A slow client only missed some events; keep streaming.
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("event subscriber lagged, dropped {n} event(s)");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let last = matches!(ev, AdapterEvent::ShuttingDown);
                if push(stream, &to_ipc_event(ev)?, PUSH_TIMEOUT)
                    .await
                    .is_err()
                    || last
                {
                    break;
                }
            }
            break;
        }

        if req.method == Method::SubscribeMeters {
            let targets = match serde_json::from_value::<SubscribeMetersParams>(req.params) {
                Ok(p) if p.targets.len() <= MAX_METER_TARGETS => p.targets,
                Ok(_) => {
                    let msg = format!("at most {MAX_METER_TARGETS} meter targets");
                    let resp = IpcResponse::err(req.id, "invalid_value", msg);
                    write_json_frame(stream, &resp).await?;
                    continue;
                }
                Err(e) => {
                    let resp = IpcResponse::err(req.id, "invalid_value", e.to_string());
                    write_json_frame(stream, &resp).await?;
                    continue;
                }
            };
            let resp = IpcResponse::ok(req.id.clone(), serde_json::json!({"subscribed": true}));
            write_json_frame(stream, &resp).await?;
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
        write_json_frame(stream, &resp).await?;
    }
    Ok(())
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
        if push(stream, &ev, PUSH_TIMEOUT).await.is_err() {
            break;
        }
    }
}

/// Write a frame the client did not ask for; fails if it is not taken in time.
async fn push(stream: &mut UnixStream, ev: &IpcEvent, limit: Duration) -> Result<()> {
    match tokio::time::timeout(limit, write_json_frame(stream, ev)).await {
        Ok(written) => written,
        Err(_) => {
            warn!("subscriber stopped reading; dropped");
            bail!("subscriber stopped reading")
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
    async fn a_subscriber_that_stops_reading_is_dropped() {
        let (mut server, _client) = UnixStream::pair().unwrap();
        let ev = IpcEvent {
            kind: EventKind::Meters,
            payload: serde_json::json!("x".repeat(256 * 1024)),
        };
        let limit = Duration::from_millis(100);
        let mut pushed = 0;
        while push(&mut server, &ev, limit).await.is_ok() {
            pushed += 1;
            assert!(pushed < 64, "the socket never filled up");
        }
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
