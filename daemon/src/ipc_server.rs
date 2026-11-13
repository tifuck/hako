//! Unix socket IPC server.

use crate::hub::DaemonHub;
use anyhow::{bail, Result};
use hako_core::device::DeviceId;
use hako_core::ipc::{EventKind, IpcEvent, IpcRequest, IpcResponse, Method};
use hako_core::protocol::AdapterEvent;
use hako_core::remap::StartCaptureParams;
use hako_core::{read_json_frame, write_json_frame};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{info, warn};

/// Bind the daemon socket, owner-only. Refuses to replace a socket another
/// daemon still answers on; a stale file from a crashed daemon is removed.
/// The returned lock (held for the daemon's life) keeps two daemons started
/// at the same moment from both getting past that check.
pub fn bind(socket_path: &Path) -> Result<(UnixListener, std::fs::File)> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(socket_path.with_extension("lock"))?;
    // SAFETY: valid open fd; flock has no other preconditions.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        || std::os::unix::net::UnixStream::connect(socket_path).is_ok()
    {
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
    let mut capture = None;
    let result = serve_requests(&mut stream, &hub, &mut capture).await;
    // Listening must not outlive the client that asked for it (UI closed or
    // crashed mid-capture): input would stay ungrabbed and, for a recording,
    // every keyboard would keep being read.
    if let Some(device) = capture {
        hub.end_capture(device);
    }
    result
}

/// Request loop of one connection. `capture` tracks the device of the last
/// capture this connection started.
async fn serve_requests(
    stream: &mut UnixStream,
    hub: &DaemonHub,
    capture: &mut Option<DeviceId>,
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
                if write_json_frame(stream, &to_ipc_event(ev)?).await.is_err() || last {
                    break;
                }
            }
            break;
        }

        let method = req.method;
        let started = (method == Method::StartCapture)
            .then(|| serde_json::from_value::<StartCaptureParams>(req.params.clone()).ok())
            .flatten();
        let resp = hub.handle(req).await;
        if resp.ok {
            match method {
                Method::StartCapture => *capture = started.map(|p| p.device_id),
                Method::CancelCapture => *capture = None,
                _ => {}
            }
        }
        write_json_frame(stream, &resp).await?;
    }
    Ok(())
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
    async fn bind_is_owner_only_and_refuses_a_live_daemon() {
        let dir = std::env::temp_dir().join(format!("hako-test-{}", std::process::id()));
        let sock = dir.join("d.sock");
        let live = bind(&sock).unwrap();
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(
            bind(&sock).is_err(),
            "second daemon must not steal the socket"
        );
        drop(live);
        // Stale file (daemon gone) is replaced.
        assert!(bind(&sock).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
