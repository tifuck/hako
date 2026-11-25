//! Model downloads: `curl` into `<file>.part` (resumed with `-C -`), then the
//! size and the SHA-256 pinned in the catalog are checked before the rename.
//! Progress is the `.part` size, read by the service.

use anyhow::{bail, Context, Result};
use hako_core::voice::CatalogFile;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tracing::{info, warn};

/// How often a running curl is checked (cancel, size cap).
const POLL: Duration = Duration::from_millis(100);
/// `Download::stop` values.
const RUN: u8 = 0;
/// Cancelled: the `.part` goes.
const CANCEL: u8 = 1;
/// Daemon stop: the `.part` stays for a resume.
const STOP: u8 = 2;

/// One file to fetch.
#[derive(Debug, Clone)]
pub struct Fetch {
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub dest: PathBuf,
}

impl Fetch {
    pub fn new(file: &CatalogFile, dir: &Path) -> Self {
        Self {
            url: file.url.clone(),
            size: file.size,
            sha256: file.sha256.clone(),
            dest: dir.join(&file.file),
        }
    }

    pub fn part(&self) -> PathBuf {
        part_path(&self.dest)
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// A download in flight (files in order, e.g. the VAD then the model).
pub struct Download {
    pub id: String,
    pub quant: String,
    /// The model file (progress is read from its `.part`).
    pub main: Fetch,
    stop: Arc<AtomicU8>,
    thread: Option<JoinHandle<()>>,
}

impl Download {
    /// Fetch `files` in order on a thread; `done` gets the outcome
    /// (`Ok(false)`: cancelled).
    pub fn start(
        id: String,
        quant: String,
        files: Vec<Fetch>,
        done: impl FnOnce(Result<bool, String>) + Send + 'static,
    ) -> Self {
        let main = files.last().cloned().expect("a download has a file");
        let stop = Arc::new(AtomicU8::new(RUN));
        let thread = std::thread::Builder::new()
            .name("hako-voice-download".into())
            .spawn({
                let stop = stop.clone();
                move || {
                    let mut result = Ok(true);
                    for f in &files {
                        match fetch(f, &stop) {
                            Ok(true) => {}
                            Ok(false) => {
                                result = Ok(false);
                                break;
                            }
                            Err(e) => {
                                result = Err(format!("{e:#}"));
                                break;
                            }
                        }
                    }
                    done(result);
                }
            })
            .ok();
        Self {
            id,
            quant,
            main,
            stop,
            thread,
        }
    }

    /// Bytes of the model file so far.
    pub fn progress(&self) -> u64 {
        std::fs::metadata(self.main.part())
            .map(|m| m.len())
            .unwrap_or(0)
            .min(self.main.size)
    }

    /// Stop curl and delete the `.part`. Blocking (briefly).
    pub fn cancel(mut self) {
        self.stop.store(CANCEL, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(self.main.part());
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        // Daemon stop: curl goes, the `.part` stays for a resume.
        if let Some(t) = self.thread.take() {
            self.stop.store(STOP, Ordering::Relaxed);
            let _ = t.join();
        }
    }
}

/// `curl` writing `url` into `part`, resuming what is there. A stalled
/// transfer fails, a slow one may take its time.
fn curl(f: &Fetch) -> Command {
    // Tests serve files from `file://` URLs.
    let proto = if cfg!(test) { "=https,file" } else { "=https" };
    let mut c = Command::new("curl");
    c.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        proto,
        "--proto-redir",
        proto,
        "--connect-timeout",
        "30",
        "--speed-limit",
        "1024",
        "--speed-time",
        "60",
        "--max-filesize",
    ])
    .arg(f.size.to_string())
    .args(["-C", "-", "-o"])
    .arg(f.part())
    .arg(&f.url);
    c
}

/// Fetch one file unless it is there already. `Ok(false)`: stopped (a
/// cancel deletes the `.part`).
fn fetch(f: &Fetch, stop: &AtomicU8) -> Result<bool> {
    if std::fs::metadata(&f.dest).is_ok_and(|m| m.len() == f.size) {
        return Ok(true);
    }
    if let Some(dir) = f.dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let part = f.part();
    // A complete `.part` (a stop between curl and the check) is only checked.
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > f.size {
        let _ = std::fs::remove_file(&part);
    }
    if have != f.size {
        info!("voice: downloading {}", f.url);
        let mut child = curl(f)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run curl")?;
        let status = loop {
            let why = stop.load(Ordering::Relaxed);
            if why != RUN {
                let _ = child.kill();
                let _ = child.wait();
                if why == CANCEL {
                    let _ = std::fs::remove_file(&part);
                }
                return Ok(false);
            }
            // curl before 8.4 checks `--max-filesize` only against the size
            // the server announces.
            if std::fs::metadata(&part).is_ok_and(|m| m.len() > f.size) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&part);
                bail!("the server sent more than {} bytes", f.size);
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(POLL);
        };
        if !status.success() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            bail!("download failed: {}", err.trim());
        }
    }
    let got = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if got != f.size {
        let _ = std::fs::remove_file(&part);
        bail!("download incomplete: {got} of {} bytes", f.size);
    }
    let sum = sha256_file(&part)?;
    if !sum.eq_ignore_ascii_case(&f.sha256) {
        let _ = std::fs::remove_file(&part);
        bail!("checksum mismatch: the download is not the pinned file");
    }
    std::fs::rename(&part, &f.dest).with_context(|| format!("rename to {}", f.dest.display()))?;
    info!("voice: downloaded {}", f.dest.display());
    Ok(true)
}

/// Every member of a `tar -t` listing lies inside `dir/` (no absolute
/// path, no `..`), and the verbose listing has only files and directories
/// (no links that could point elsewhere).
pub fn check_members(names: &str, verbose: &str, dir: &str) -> Result<()> {
    let mut any = false;
    for name in names.lines().filter(|l| !l.is_empty()) {
        let path = Path::new(name);
        let inside = !path.is_absolute()
            && path.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
            && path
                .components()
                .find(|c| !matches!(c, std::path::Component::CurDir))
                .is_some_and(|c| c.as_os_str() == dir);
        if !inside {
            bail!("the archive holds {name:?}, outside {dir}/");
        }
        any = true;
    }
    if let Some(l) = verbose
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with(['-', 'd']))
    {
        bail!("the archive holds something that is not a file: {l}");
    }
    if !any {
        bail!("the archive is empty");
    }
    Ok(())
}

/// Extract the wake-word archive into `into` (its top directory
/// replaces an older copy), checked first and unpacked into a temporary
/// directory; the archive is deleted afterwards.
pub fn extract_kws(archive: &Path, into: &Path) -> Result<()> {
    let k = &hako_core::voice::CATALOG.kws;
    let list = |verbose: bool| -> Result<String> {
        let out = Command::new("tar")
            .arg(if verbose { "-tvjf" } else { "-tjf" })
            .arg(archive)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("cannot run tar")?;
        if !out.status.success() {
            bail!(
                "the archive cannot be read: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    check_members(&list(false)?, &list(true)?, &k.dir)?;
    let tmp = into.join(format!(".extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let result = (|| {
        let out = Command::new("tar")
            .args(["--no-same-owner", "--no-same-permissions", "-xjf"])
            .arg(archive)
            .arg("-C")
            .arg(&tmp)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("cannot run tar")?;
        if !out.status.success() {
            bail!(
                "extracting failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let got = tmp.join(&k.dir);
        for f in [&k.encoder, &k.decoder, &k.joiner, &k.tokens, &k.pieces] {
            if !got.join(f).is_file() {
                bail!("the archive has no {f}");
            }
        }
        let dest = into.join(&k.dir);
        match std::fs::remove_dir_all(&dest) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("delete {}", dest.display()));
            }
            _ => {}
        }
        std::fs::rename(&got, &dest).with_context(|| format!("rename to {}", dest.display()))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    if result.is_ok() {
        let _ = std::fs::remove_file(archive);
        info!("jarvis: wake-word model extracted to {}", into.display());
    }
    result
}

/// SHA-256 of a file (lowercase hex). `sha256sum` reads it over stdin, so
/// no path is ever parsed or printed back.
pub fn sha256_file(path: &Path) -> Result<String> {
    let input = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let out = Command::new("/usr/bin/sha256sum")
        .stdin(input)
        .stderr(Stdio::piped())
        .output()
        .context("cannot run sha256sum")?;
    if !out.status.success() {
        warn!("sha256sum: {}", String::from_utf8_lossy(&out.stderr).trim());
        bail!("sha256sum failed");
    }
    String::from_utf8(out.stdout)?
        .split_whitespace()
        .next()
        .map(str::to_string)
        .context("sha256sum printed no sum")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hako-voice-dl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn served(d: &Path, body: &[u8]) -> (String, String) {
        let src = d.join("served.bin");
        std::fs::write(&src, body).unwrap();
        (
            format!("file://{}", src.display()),
            sha256_file(&src).unwrap(),
        )
    }

    fn run(files: Vec<Fetch>) -> Result<bool, String> {
        let (tx, rx) = mpsc::channel();
        let dl = Download::start("m".into(), "Q8_0".into(), files, move |r| {
            let _ = tx.send(r);
        });
        let r = rx.recv_timeout(Duration::from_secs(30)).unwrap();
        drop(dl);
        r
    }

    #[test]
    fn verified_download_is_renamed_and_resumes_a_part() {
        let d = dir("ok");
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let (url, sum) = served(&d, &body);
        let f = Fetch {
            url,
            size: body.len() as u64,
            sha256: sum,
            dest: d.join("models/model.gguf"),
        };
        // Half of it from an earlier attempt: curl resumes after it.
        std::fs::create_dir_all(d.join("models")).unwrap();
        std::fs::write(f.part(), &body[..50_000]).unwrap();
        assert_eq!(run(vec![f.clone()]), Ok(true));
        assert_eq!(std::fs::read(&f.dest).unwrap(), body);
        assert!(!f.part().exists());
        // Present with the right size: not fetched again.
        assert_eq!(run(vec![f]), Ok(true));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_wrong_hash_or_size_is_refused_and_deleted() {
        let d = dir("bad");
        let (url, _) = served(&d, b"not the model");
        let f = Fetch {
            url: url.clone(),
            size: 13,
            sha256: "0".repeat(64),
            dest: d.join("model.gguf"),
        };
        let err = run(vec![f.clone()]).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(!f.dest.exists() && !f.part().exists());
        // Bigger than the catalog says: curl's cap refuses it.
        let small = Fetch { size: 5, ..f };
        assert!(run(vec![small.clone()]).is_err());
        assert!(!small.dest.exists());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn archive_members_must_stay_inside_their_directory() {
        let ok = check_members("m/\nm/a.onnx\n./m/b.txt\n", "drwx m/\n-rw a\n", "m");
        assert!(ok.is_ok(), "{ok:?}");
        for names in [
            "/etc/passwd\n",
            "m/../x\n",
            "m/a\nother/b\n",
            "../m/a\n",
            "",
        ] {
            assert!(check_members(names, "", "m").is_err(), "{names:?}");
        }
        let link = check_members("m/a\n", "lrwxrwxrwx u/g 0 m/a -> /etc/passwd\n", "m");
        assert!(link.is_err());
    }

    /// A real archive in the catalog's layout is extracted, replacing an
    /// older copy, and deleted.
    #[test]
    fn the_wake_model_archive_is_extracted() {
        let k = &hako_core::voice::CATALOG.kws;
        let d = dir("kws");
        let src = d.join("src");
        std::fs::create_dir_all(src.join(&k.dir)).unwrap();
        for f in [&k.encoder, &k.decoder, &k.joiner, &k.tokens, &k.pieces] {
            std::fs::write(src.join(&k.dir).join(f), b"x").unwrap();
        }
        let into = d.join("kws");
        std::fs::create_dir_all(into.join(&k.dir)).unwrap();
        std::fs::write(into.join(&k.dir).join("stale"), b"old").unwrap();
        let archive = into.join("m.tar.bz2");
        let made = Command::new("tar")
            .arg("-cjf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(&k.dir)
            .status()
            .unwrap();
        assert!(made.success());
        extract_kws(&archive, &into).unwrap();
        assert!(into.join(&k.dir).join(&k.encoder).is_file());
        assert!(!into.join(&k.dir).join("stale").exists());
        assert!(!archive.exists());
        let left: Vec<_> = std::fs::read_dir(&into).unwrap().flatten().collect();
        assert_eq!(left.len(), 1, "no temporary directory is left");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn https_only_outside_tests() {
        let f = Fetch {
            url: "https://example.invalid/x".into(),
            size: 1,
            sha256: String::new(),
            dest: "/nonexistent/x".into(),
        };
        let args: Vec<_> = curl(&f)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w == ["--max-filesize", "1"]));
        assert!(args.windows(2).any(|w| w == ["-C", "-"]));
        assert!(args.iter().any(|a| a == "--fail"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--proto" && w[1].starts_with("=https")));
        assert_eq!(
            part_path(Path::new("/m/a.gguf")),
            Path::new("/m/a.gguf.part")
        );
    }
}
