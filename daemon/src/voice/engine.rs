//! The `hako-stt` child process: found next to hakod, spawned so it dies
//! with hakod (`PR_SET_PDEATHSIG`), fed frames from a writer thread (a full
//! pipe never blocks the service) and read by a reader thread. The service
//! sees it through [`Spawner`] / [`Worker`], so tests use a fake.

use hako_core::voice::worker::{read_frame, write_msg, write_pcm, Frame, WorkerIn, WorkerOut};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use tracing::{debug, warn};

pub const BINARY: &str = "hako-stt";
/// Overrides where the worker binary is looked for last.
pub const BINARY_ENV: &str = "HAKO_STT_BIN";

#[derive(Debug, Clone, PartialEq)]
pub enum ToWorker {
    Msg(WorkerIn),
    Pcm(Vec<f32>),
}

/// A running worker; dropping it ends the process.
pub trait Worker: Send {
    fn send(&mut self, msg: ToWorker);
}

/// What the worker says; `None` once it is gone.
pub type WorkerSink = Box<dyn Fn(Option<WorkerOut>) + Send>;

pub trait Spawner: Send {
    /// The worker can be started (its binary is there), or why not.
    fn available(&self) -> Result<(), String>;
    fn spawn(&mut self, sink: WorkerSink) -> Result<Box<dyn Worker>, String>;
}

/// `hako-stt` next to hakod, else in `../lib/hako/` (packages), else `$HAKO_STT_BIN`.
pub fn find_binary() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().ok();
    let dir = exe.as_deref().and_then(Path::parent);
    let mut tried = Vec::new();
    let candidates = [
        dir.map(|d| d.join(BINARY)),
        dir.map(|d| d.join("../lib/hako").join(BINARY)),
        std::env::var_os(BINARY_ENV).map(PathBuf::from),
    ];
    for path in candidates.into_iter().flatten() {
        if path.is_file() {
            return Ok(path);
        }
        tried.push(path.display().to_string());
    }
    Err(format!(
        "the speech engine ({BINARY}) is not installed (looked in {}); reinstall hako or set {BINARY_ENV}",
        tried.join(", ")
    ))
}

pub struct ChildSpawner;

impl Spawner for ChildSpawner {
    fn available(&self) -> Result<(), String> {
        find_binary().map(|_| ())
    }

    fn spawn(&mut self, sink: WorkerSink) -> Result<Box<dyn Worker>, String> {
        spawn_path(&find_binary()?, sink)
    }
}

fn spawn_path(bin: &Path, sink: WorkerSink) -> Result<Box<dyn Worker>, String> {
    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child =
        hako_audio::spawn_owned(cmd).map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
        (Some(i), Some(o)) => (i, o),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the speech engine has no pipes".into());
        }
    };
    let (tx, rx) = mpsc::channel::<ToWorker>();
    let writer = std::thread::Builder::new()
        .name("hako-stt-in".into())
        .spawn(move || {
            let mut w = BufWriter::new(stdin);
            for msg in rx {
                let r = match &msg {
                    ToWorker::Msg(m) => write_msg(&mut w, m),
                    ToWorker::Pcm(pcm) => write_pcm(&mut w, pcm),
                };
                if let Err(e) = r {
                    debug!("hako-stt stdin: {e}");
                    return;
                }
            }
        })
        .map_err(|e| format!("cannot start the engine writer: {e}"))?;
    std::thread::Builder::new()
        .name("hako-stt-out".into())
        .spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                match read_frame::<WorkerOut>(&mut r) {
                    Ok(Some(Frame::Msg(m))) => sink(Some(m)),
                    Ok(Some(Frame::Pcm(_))) => {}
                    Ok(None) => break,
                    Err(e) => {
                        warn!("hako-stt: {e}");
                        break;
                    }
                }
            }
            sink(None);
        })
        .map_err(|e| format!("cannot start the engine reader: {e}"))?;
    Ok(Box::new(ChildWorker {
        child,
        tx: Some(tx),
        writer: Some(writer),
    }))
}

struct ChildWorker {
    child: Child,
    tx: Option<mpsc::Sender<ToWorker>>,
    writer: Option<JoinHandle<()>>,
}

impl Worker for ChildWorker {
    fn send(&mut self, msg: ToWorker) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }
}

impl Drop for ChildWorker {
    fn drop(&mut self) {
        self.tx = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Its write fails once the process is gone.
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_says_where_it_looked() {
        // Test binaries live in target/*/deps: no hako-stt next to them.
        if find_binary().is_ok() {
            return;
        }
        let e = find_binary().unwrap_err();
        assert!(e.contains(BINARY) && e.contains(BINARY_ENV), "{e}");
    }

    /// `cat` as the worker echoes our frames: the PCM frame comes back
    /// (skipped), then the `End` frame, which is no `WorkerOut`, ends the
    /// reader. So frames went in order through the writer and the child.
    #[test]
    fn frames_flow_through_a_child() {
        let (tx, rx) = mpsc::channel();
        let mut w = spawn_path(
            Path::new("/usr/bin/cat"),
            Box::new(move |m| {
                let _ = tx.send(m);
            }),
        )
        .unwrap();
        w.send(ToWorker::Pcm(vec![0.5; 16]));
        w.send(ToWorker::Msg(WorkerIn::End { session: 1 }));
        let got = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(got, None);
        drop(w);
    }
}
