//! Self-update from GitHub releases.
//!
//! Only release builds know their repo (`HAKO_REPO`, set by the release
//! workflow), and only an install from a release updates itself: the `.deb`
//! (`/usr/bin`, updated through apt) or `install-user.sh` (`~/.local/bin`,
//! updated from the tarball). Every other build never touches the network.
//!
//! Uses `curl`, `sha256sum`, `ssh-keygen` and `tar` like the daemon uses `pw-*`: present on
//! every desktop, no TLS stack in the binary. Every program whose result
//! decides what root installs, and the one `pkexec` runs (it resolves it
//! through the caller's PATH), is an absolute path: PATH may list folders
//! the user's processes can write, like `~/.local/bin`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const REPO: Option<&str> = option_env!("HAKO_REPO");
const DEB_BIN: &str = "/usr/bin/hako";
/// Present only when dpkg installed hako; another package manager's
/// `/usr/bin/hako` is updated by that package manager.
const DEB_LIST: &str = "/var/lib/dpkg/info/hako.list";
const UDEV_RULES: &str = "/etc/udev/rules.d/70-hako.rules";
/// Set for the restarted UI so it can confirm the update.
pub const UPDATED_ENV: &str = "HAKO_UPDATED";
pub const WARN_ENV: &str = "HAKO_UPDATE_WARN";
/// `publish.sh` signs every release's `SHA256SUMS` with this key, whose
/// private half never leaves the maintainer's machine: a hijacked GitHub
/// account or release token cannot ship an update without it.
const SIGNING_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJMt1vykoBFjxuEOvgx8kwO1jhHc4NtrJ3s0TchN+Aqf hako-release";
const SIG_NAMESPACE: &str = "hako-release";
/// Download caps: a release publisher can't fill memory or the disk. Real
/// sizes: API answer ~10 KB, `SHA256SUMS` and its signature < 1 KB, builds
/// ~30 MB.
const API_MAX: u64 = 1 << 20;
const SUMS_MAX: u64 = 64 << 10;
const ASSET_MAX: u64 = 256 << 20;
/// `install-user.sh`'s exit code when the files are installed but
/// restarting the daemon failed.
const RESTART_FAILED: i32 = 3;

/// A release that failed verification (signature or checksum): worth telling
/// the user about, unlike a network failure.
#[derive(Debug)]
pub struct Rejected(String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Rejected {}

/// `e` is (or wraps) a [`Rejected`] release.
pub fn rejected(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Rejected>().is_some()
}

/// A published release newer than this build.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Asset name (tarball or `.deb`) and URL.
    name: String,
    url: String,
    sums_url: String,
    /// Signature of `SHA256SUMS`, on the mirror's `signatures` branch.
    sig_url: String,
}

/// A downloaded and verified release, installed on restart.
#[derive(Clone, Debug)]
pub struct Ready {
    pub version: String,
    file: PathBuf,
    /// SHA-256 the download was verified against.
    sum: String,
}

impl Ready {
    /// False once a failed install discarded the download.
    pub fn on_disk(&self) -> bool {
        self.file.exists()
    }
}

/// Where this build updates from: `(owner/repo, installed hako path)`.
/// Resolve once at startup: after an update the running exe is replaced.
pub fn target() -> Option<(&'static str, PathBuf)> {
    let repo = REPO.filter(|r| !r.is_empty())?;
    let exe = std::env::current_exe().ok()?;
    let user = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/hako");
    let deb = Path::new(DEB_LIST).exists().then(|| PathBuf::from(DEB_BIN));
    [Some(user), deb]
        .into_iter()
        .flatten()
        .find(|bin| std::fs::canonicalize(bin).is_ok_and(|b| b == exe))
        .map(|bin| (repo, bin))
}

/// Installed from the `.deb`, so apt owns the files.
fn deb_install() -> bool {
    std::env::current_exe().is_ok_and(|e| e.starts_with("/usr/"))
}

/// Debian name for this CPU, as used in `.deb` file names.
fn deb_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The latest release, if newer than this build.
pub fn check(repo: &str) -> Result<Option<Release>> {
    let body = fetch(
        &[
            "-H",
            "Accept: application/vnd.github+json",
            &format!("https://api.github.com/repos/{repo}/releases/latest"),
        ],
        API_MAX,
    )
    .context("GitHub did not answer")?;
    let v: serde_json::Value = serde_json::from_slice(&body)?;
    release(&v, repo, env!("CARGO_PKG_VERSION"), deb_install())
}

/// The release `v` (GitHub's release JSON) offers, if newer than `local`.
/// The tag is not signed: the asset name binds the version to the download,
/// since the signed `SHA256SUMS` lists each build under its versioned name,
/// so an old build re-published under a newer tag finds no checksum.
fn release(v: &serde_json::Value, repo: &str, local: &str, deb: bool) -> Result<Option<Release>> {
    let tag = v["tag_name"].as_str().context("release has no tag")?;
    let version = tag.trim_start_matches('v');
    if parse_version(version).is_none() {
        bail!("the latest release's tag {tag:?} is not a version");
    }
    if !newer(version, local) {
        return Ok(None);
    }
    let name = asset_name(version, deb);
    let (_, url) = asset(v, repo, |n| n == name)
        .with_context(|| format!("no build for this computer ({name})"))?;
    let (_, sums_url) =
        asset(v, repo, |n| n == "SHA256SUMS").context("release has no checksums")?;
    Ok(Some(Release {
        version: version.into(),
        name,
        url,
        sums_url,
        sig_url: format!("https://raw.githubusercontent.com/{repo}/signatures/{tag}.sig"),
    }))
}

/// The name the release workflow gives this computer's build of `version`
/// (a version [`parse_version`] accepted, so it is a plain file name).
fn asset_name(version: &str, deb: bool) -> String {
    if deb {
        format!("hako_{version}_{}.deb", deb_arch())
    } else {
        format!("hako-{version}-{}-linux.tar.gz", std::env::consts::ARCH)
    }
}

/// `(name, url)` of the first asset `want` accepts, following only download
/// links of this repo's own releases. Owner and repo names are
/// case-insensitive on GitHub (and builds made before a rename keep the old
/// spelling), so the prefix is too.
fn asset(
    v: &serde_json::Value,
    repo: &str,
    want: impl Fn(&str) -> bool,
) -> Option<(String, String)> {
    let prefix = format!("https://github.com/{repo}/releases/download/").to_ascii_lowercase();
    v["assets"].as_array().into_iter().flatten().find_map(|a| {
        let name = a["name"].as_str().filter(|n| want(n))?;
        let url = a["browser_download_url"]
            .as_str()
            .filter(|u| u.to_ascii_lowercase().starts_with(&prefix))?;
        Some((name.to_string(), url.to_string()))
    })
}

/// Private download folder: in the user's cache, never a guessable `/tmp`
/// path another user could swap the package in before it is installed as root.
fn download_dir() -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("no home directory")?;
    Ok(cache.join("hako/update"))
}

/// Every hako window shares `dir`: the lock is held for a whole download or
/// install, so one window never deletes the files another is installing.
fn lock(dir: &Path) -> Result<std::fs::File> {
    let path = dir.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another hako window is updating; try again in a moment")
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Removes the download folder when dropped, unless `keep` was set.
struct Discard<'a> {
    dir: &'a Path,
    keep: bool,
}

impl Drop for Discard<'_> {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(self.dir);
        }
    }
}

/// Download and verify `rel`, replacing any earlier download. The signed
/// checksums come first: only a build they list is downloaded.
pub fn download(rel: &Release) -> Result<Ready> {
    download_to(&download_dir()?, SIGNING_KEY, rel)
}

/// [`download`] into `dir`, trusting releases signed by `key`.
fn download_to(dir: &Path, key: &str, rel: &Release) -> Result<Ready> {
    let _lock = lock(dir)?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let mut discard = Discard { dir, keep: false };
    let sums = fetch(&[&rel.sums_url], SUMS_MAX)?;
    fetch(&[&rel.sig_url], SUMS_MAX)
        .with_context(|| format!("v{} is not signed yet", rel.version))
        .and_then(|sig| {
            verify_signature(key, &sums, &sig).map_err(|e| Rejected(format!("{e:#}")).into())
        })?;
    let sums = String::from_utf8(sums)?;
    let Some(want) = expected_sum(&sums, &rel.name) else {
        bail!(Rejected(format!(
            "the signed checksums do not list {}, not downloaded",
            rel.name
        )));
    };
    let file = dir.join(&rel.name);
    std::fs::File::create(&file)
        .map_err(anyhow::Error::from)
        .and_then(|mut out| copy_capped(curl(ASSET_MAX).arg(&rel.url), ASSET_MAX, &mut out))
        .context("download failed")?;
    let got = String::from_utf8(run(Command::new("/usr/bin/sha256sum").arg(&file))?)?;
    if got.split_whitespace().next() != Some(want.as_str()) {
        bail!(Rejected("checksum mismatch, download discarded".into()));
    }
    discard.keep = true;
    Ok(Ready {
        version: rel.version.clone(),
        file,
        sum: want,
    })
}

/// Remove a download no release wants any more (hako was updated another
/// way, e.g. by apt). Left alone while another window downloads or installs.
pub fn discard_stale() {
    if let Ok(dir) = download_dir() {
        remove_download(&dir);
    }
}

fn remove_download(dir: &Path) {
    if dir.exists() {
        if let Ok(_lock) = lock(dir) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Install a download, restarting the daemon. Returns a warning for the user
/// when the device rules could not be updated or the daemon did not restart
/// (the update is installed either way). The download is kept on
/// failure so the user can retry.
pub fn install(ready: &Ready) -> Result<Option<String>> {
    let dir = ready.file.parent().context("bad download path")?;
    let _lock = lock(dir)?;
    let res = install_file(&ready.file, &ready.sum, dir);
    if res.is_ok() {
        let _ = std::fs::remove_dir_all(dir);
    }
    res
}

fn install_file(file: &Path, sum: &str, dir: &Path) -> Result<Option<String>> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad download name")?;
    // The download folder is the user's: anything running as them could swap
    // the file after `download` checked it. Root only gets these checked
    // bytes over stdin, never a path to read.
    let data = std::fs::read(file)?;
    if let Err(e) = check_sum(&data, sum) {
        // Spent: the next check downloads it again instead of retrying this.
        let _ = std::fs::remove_dir_all(dir);
        return Err(e);
    }
    if name.ends_with(".deb") {
        // apt installs rules and units and restarts the daemons (postinst).
        run_input(
            Command::new("/usr/bin/pkexec").args(["/usr/bin/sh", "-c"]).arg(
                "t=$(mktemp -d) && cat > \"$t/hako.deb\" && apt-get install -y \"$t/hako.deb\"; s=$?; rm -rf \"$t\"; exit $s",
            ),
            &data,
        )
        .context("install was cancelled or failed")?;
        return Ok(None);
    }
    run_input(
        Command::new("/usr/bin/tar")
            .args(["-xzf", "-", "-C"])
            .arg(dir),
        &data,
    )?;
    let root = name.trim_end_matches(".tar.gz");
    // Rules first, so the restarted daemon opens devices with the new access.
    let rules = run_input(
        Command::new("/usr/bin/tar")
            .args(["-xzOf", "-"])
            .arg(format!("{root}/udev/70-hako.rules")),
        &data,
    )
    .context("the update has no device rules")
    .and_then(|r| update_udev(&r));
    let rules = rules.err().map(|e| {
        format!(
            "New device rules were not installed ({e:#}). Run: hakod --udev-rules | sudo tee {UDEV_RULES} && sudo udevadm control --reload-rules && sudo udevadm trigger"
        )
    });
    let out = Command::new("/usr/bin/bash")
        .arg(dir.join(root).join("scripts/install-user.sh"))
        .env("HAKO_SKIP_UDEV", "1")
        .stdin(Stdio::null())
        .output()
        .context("cannot run bash")?;
    let restart = script_result(&out)?;
    let warn: Vec<String> = rules.into_iter().chain(restart).collect();
    Ok((!warn.is_empty()).then(|| warn.join(" ")))
}

/// `install-user.sh`'s outcome: a warning when the files were installed but
/// the daemon did not restart (the update is in place, so no error).
fn script_result(out: &std::process::Output) -> Result<Option<String>> {
    match out.status.code() {
        Some(0) => Ok(None),
        Some(RESTART_FAILED) => Ok(Some(
            "Installed, but restarting the daemon failed: run systemctl --user restart hakod"
                .into(),
        )),
        _ => bail!(
            "install failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The device rules live in /etc: ask through polkit, only when they changed.
fn update_udev(fresh: &[u8]) -> Result<()> {
    if std::fs::read(UDEV_RULES).ok().as_deref() == Some(fresh) {
        return Ok(());
    }
    run_input(
        Command::new("/usr/bin/pkexec").args(["/usr/bin/sh", "-c"]).arg(format!(
            "install -m 644 /dev/stdin {UDEV_RULES} && udevadm control --reload-rules && udevadm trigger"
        )),
        fresh,
    )
    .context("cancelled or failed")?;
    Ok(())
}

/// Replace this process with the freshly installed UI. Returns only on failure.
pub fn restart(bin: &Path, warn: Option<String>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(bin);
    cmd.args(std::env::args_os().skip(1)).env(UPDATED_ENV, "1");
    if let Some(w) = warn {
        cmd.env(WARN_ENV, w);
    }
    cmd.exec()
}

/// `curl` refusing bodies over `max` bytes.
fn curl(max: u64) -> Command {
    let mut c = Command::new("curl");
    c.args([
        "-fsSL",
        "--proto",
        // Tests serve releases from `file://` URLs.
        if cfg!(test) { "=https,file" } else { "=https" },
        "--retry",
        "2",
        "--max-time",
        "300",
        "--max-filesize",
    ])
    .arg(max.to_string());
    c
}

/// `curl args`'s body, at most `max` bytes of it.
fn fetch(args: &[&str], max: u64) -> Result<Vec<u8>> {
    read_capped(curl(max).args(args), max)
}

/// Run to completion; stdout on success, stderr as the error.
fn read_capped(cmd: &mut Command, max: u64) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    copy_capped(cmd, max, &mut body)?;
    Ok(body)
}

/// Run to completion, its stdout into `sink`; stderr as the error. More
/// than `max` bytes of stdout is an error, read no further: curl before 8.4
/// checks `--max-filesize` only against the size the server announces.
fn copy_capped(cmd: &mut Command, max: u64, sink: &mut impl std::io::Write) -> Result<()> {
    use std::io::Read;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {:?}", cmd.get_program()))?;
    let copied = std::io::copy(
        &mut child.stdout.take().context("no stdout")?.take(max + 1),
        sink,
    );
    if !copied.as_ref().is_ok_and(|&n| n <= max) {
        let _ = child.kill();
        let _ = child.wait();
        copied?;
        bail!("more than {max} bytes");
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Run to completion; stdout on success, stderr as the error.
fn run(cmd: &mut Command) -> Result<Vec<u8>> {
    run_input(cmd, &[])
}

/// [`run`] with `input` as stdin, written from another thread so a child
/// filling its stdout pipe cannot deadlock.
fn run_input(cmd: &mut Command, input: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {:?}", cmd.get_program()))?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let (out, wrote) = std::thread::scope(|s| {
        let writer = s.spawn(move || stdin.write_all(input));
        (child.wait_with_output(), writer.join())
    });
    let out = out?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // A child that exits early must not pass as having read all of `input`.
    wrote
        .map_err(|_| anyhow::anyhow!("stdin writer panicked"))?
        .with_context(|| format!("cannot feed {:?}", cmd.get_program()))?;
    Ok(out.stdout)
}

/// Fails unless `sig` is `key`'s signature of `data` (`ssh-keygen -Y sign
/// -n hako-release`). ssh-keygen reads the signers and the signature from
/// pipes, never from a file another process of the user could swap between
/// writing and checking it. bash only passes its arguments on (no PATH, no
/// environment for `BASH_ENV` or exported functions).
fn verify_signature(key: &str, data: &[u8], sig: &[u8]) -> Result<()> {
    let sig = std::str::from_utf8(sig).context("the release signature is not text")?;
    run_input(
        Command::new("/usr/bin/bash")
            .env_clear()
            .args([
                "-c",
                r#"exec /usr/bin/ssh-keygen -Y verify -I "$1" -n "$1" -f <(printf '%s\n' "$2") -s <(printf '%s' "$3")"#,
                "bash",
                SIG_NAMESPACE,
            ])
            .arg(format!(
                "{SIG_NAMESPACE} namespaces=\"{SIG_NAMESPACE}\" {key}"
            ))
            .arg(sig),
        data,
    )
    .context("release signature check failed (needs ssh-keygen from openssh-client)")?;
    Ok(())
}

/// Fails unless `data` still hashes to `want`, the sum `download` verified.
fn check_sum(data: &[u8], want: &str) -> Result<()> {
    let got = String::from_utf8(run_input(&mut Command::new("/usr/bin/sha256sum"), data)?)?;
    if got.split_whitespace().next() != Some(want) {
        bail!("checksum mismatch, the download changed since it was verified");
    }
    Ok(())
}

/// A pre-release identifier; numeric ones sort numerically and first, as in semver.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    Num(u64),
    Text(String),
}

/// `(dotted numbers, pre-release identifiers)` of `1.2.3` or `1.2.3-rc.1`.
/// Only ASCII letters, digits, `.` and `-` pass (no `..`), so a version is
/// safe inside a file name.
fn parse_version(s: &str) -> Option<(Vec<u64>, Option<Vec<PreId>>)> {
    if s.contains("..")
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b))
    {
        return None;
    }
    let (nums, pre) = s.split_once('-').map_or((s, None), |(n, p)| (n, Some(p)));
    let nums = nums
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let pre = match pre {
        None => None,
        Some(p) => Some(
            p.split('.')
                .map(|id| match id.parse() {
                    Ok(n) => Some(PreId::Num(n)),
                    Err(_) => (!id.is_empty()).then(|| PreId::Text(id.into())),
                })
                .collect::<Option<_>>()?,
        ),
    };
    Some((nums, pre))
}

/// `remote` > `local` in semver order: dotted numbers (missing ones are 0),
/// then a release beats its pre-releases (`0.4.0` > `0.4.0-rc1`), then
/// pre-releases compare identifier by identifier (`0.4.0-rc2` > `0.4.0-rc1`).
fn newer(remote: &str, local: &str) -> bool {
    let (Some((mut r, r_pre)), Some((mut l, l_pre))) =
        (parse_version(remote), parse_version(local))
    else {
        return false;
    };
    let n = r.len().max(l.len());
    r.resize(n, 0);
    l.resize(n, 0);
    (r, r_pre.is_none(), r_pre) > (l, l_pre.is_none(), l_pre)
}

/// Hash for `name` from `sha256sum` output (`<hash>  <name>` or `<hash> *<name>`).
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, file) = l.split_once(char::is_whitespace)?;
        (file.trim_start().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0.0", "0.99.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("garbage", "0.1.0"));
        // A release supersedes its own pre-releases, not the other way round.
        assert!(newer("0.4.0", "0.4.0-rc1"));
        assert!(!newer("0.4.0-rc1", "0.4.0"));
        assert!(newer("0.4.1-rc1", "0.4.0"));
        // Missing parts are zeros; pre-releases are ordered among themselves.
        assert!(!newer("0.4.0", "0.4"));
        assert!(!newer("0.4", "0.4.0"));
        assert!(newer("0.4.1", "0.4"));
        assert!(newer("0.4.0-rc2", "0.4.0-rc1"));
        assert!(!newer("0.4.0-rc1", "0.4.0-rc2"));
        assert!(newer("0.4.0-rc.10", "0.4.0-rc.9"));
        assert!(newer("0.4.0-rc.1", "0.4.0-rc"));
        assert!(newer("0.4.0-rc", "0.4.0-1"));
        assert!(!newer("0.4.0-", "0.3.0"));
    }

    #[test]
    fn checksum_lines_are_matched_by_name() {
        let sums = "AB12  hako-1.0.0-x86_64-linux.tar.gz\ncd34 *other.tar.gz\n";
        assert_eq!(
            expected_sum(sums, "hako-1.0.0-x86_64-linux.tar.gz").as_deref(),
            Some("ab12")
        );
        assert_eq!(expected_sum(sums, "other.tar.gz").as_deref(), Some("cd34"));
        assert_eq!(expected_sum(sums, "missing.tar.gz"), None);
    }

    /// A fresh ed25519 key `dir/name` (public half in `name.pub`).
    fn keygen(dir: &Path, name: &str) -> PathBuf {
        let key = dir.join(name);
        run(Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
            .arg(&key))
        .unwrap();
        key
    }

    fn sign(key: &Path, namespace: &str, data: &[u8]) -> Vec<u8> {
        run_input(
            Command::new("ssh-keygen")
                .args(["-q", "-Y", "sign", "-n", namespace, "-f"])
                .arg(key),
            data,
        )
        .unwrap()
    }

    fn public(key: &Path) -> String {
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        public.trim().into()
    }

    #[test]
    fn only_the_release_key_signs_updates() {
        let dir = std::env::temp_dir().join(format!("hako-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (good, bad) = (keygen(&dir, "good"), keygen(&dir, "bad"));
        let public = public(&good);
        let sums = b"abc  hako.deb\n";
        let sig = sign(&good, SIG_NAMESPACE, sums);
        assert!(verify_signature(&public, sums, &sig).is_ok());
        assert!(verify_signature(&public, b"abd  hako.deb\n", &sig).is_err());
        assert!(verify_signature(&public, sums, &sign(&bad, SIG_NAMESPACE, sums)).is_err());
        assert!(verify_signature(&public, sums, &sign(&good, "other", sums)).is_err());
        assert!(verify_signature(&public, sums, b"not a signature").is_err());
        assert!(verify_signature(&public, sums, b"\xff").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn downloads_need_signed_sums_that_list_and_match_them() {
        let root = std::env::temp_dir().join(format!("hako-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (good, bad) = (keygen(&root, "good"), keygen(&root, "bad"));
        let name = "hako-9.9.9-x86_64-linux.tar.gz";
        let sum = String::from_utf8(run_input(&mut Command::new("sha256sum"), b"build").unwrap())
            .unwrap()[..64]
            .to_string();
        let listed = format!("{sum}  {name}\n");
        // Serves `sums`, `sig` and (if any) `asset` from `root/case`, then
        // downloads them into `root/case/update`. Each case has its own lock:
        // a child another test forks can hold one for a moment.
        let serve = |case: &str, sums: &str, sig: Vec<u8>, asset: Option<&[u8]>| {
            let srv = root.join(case);
            std::fs::create_dir_all(&srv).unwrap();
            let url = |file: &str, body: &[u8]| {
                std::fs::write(srv.join(file), body).unwrap();
                format!("file://{}", srv.join(file).display())
            };
            let rel = Release {
                version: "9.9.9".into(),
                name: name.into(),
                url: match asset {
                    Some(a) => url(name, a),
                    None => format!("file://{}", srv.join(name).display()),
                },
                sums_url: url("SHA256SUMS", sums.as_bytes()),
                sig_url: url("SHA256SUMS.sig", &sig),
            };
            let dir = srv.join("update");
            (download_to(&dir, &public(&good), &rel), dir)
        };
        let signed = |sums: &str| sign(&good, SIG_NAMESPACE, sums.as_bytes());

        let (ready, dir) = serve("ok", &listed, signed(&listed), Some(b"build"));
        let ready = ready.unwrap();
        assert_eq!((ready.file, ready.sum), (dir.join(name), sum.clone()));
        // Nothing ssh-keygen reads is ever written to the user's folder.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, [name]);

        let rejected_with = |(res, dir): (Result<Ready>, PathBuf), msg: &str| {
            let e = res.unwrap_err();
            assert!(rejected(&e) && format!("{e:#}").contains(msg), "{e:#}");
            assert!(!dir.exists());
        };
        let flipped = serve("flipped", &listed, signed(&listed), Some(b"bujld"));
        rejected_with(flipped, "checksum mismatch");
        // No asset is served: fetching it would fail as "download failed".
        let other = format!("{sum}  hako-9.9.9-other-linux.tar.gz\n");
        rejected_with(
            serve("unlisted", &other, signed(&other), None),
            "do not list",
        );
        let by_bad = sign(&bad, SIG_NAMESPACE, listed.as_bytes());
        rejected_with(serve("bad-key", &listed, by_bad, None), "signature");
        let wrong_ns = sign(&good, "other", listed.as_bytes());
        rejected_with(serve("namespace", &listed, wrong_ns, None), "signature");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn changed_download_fails_the_sum_check() {
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(check_sum(b"abc", abc).is_ok());
        assert!(check_sum(b"abd", abc).is_err());
    }

    #[test]
    fn a_download_changed_after_its_check_is_discarded_not_installed() {
        let dir = std::env::temp_dir().join(format!("hako-inst-{}/update", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hako-9.9.9-x86_64-linux.tar.gz");
        std::fs::write(&file, b"abd").unwrap();
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let e = install_file(&file, abc, &dir).unwrap_err();
        assert!(format!("{e:#}").contains("checksum mismatch"), "{e:#}");
        assert!(!dir.exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn downloads_stop_at_their_cap() {
        let head = |n: &str| {
            let mut c = Command::new("head");
            c.args(["-c", n, "/dev/zero"]);
            c
        };
        assert_eq!(read_capped(&mut head("100"), 100).unwrap().len(), 100);
        assert!(read_capped(&mut head("101"), 100).is_err());
        // An endless body is cut off, not read into memory or onto disk.
        assert!(read_capped(&mut Command::new("yes"), 1 << 16).is_err());
        let path = std::env::temp_dir().join(format!("hako-cap-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        assert!(copy_capped(&mut Command::new("yes"), 1 << 16, &mut file).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), (1 << 16) + 1);
        std::fs::remove_file(&path).unwrap();
        assert!(curl(5)
            .get_args()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|a| a == ["--max-filesize", "5"]));
    }

    #[test]
    fn one_window_updates_at_a_time() {
        let dir = std::env::temp_dir().join(format!("hako-lock-{}/update", std::process::id()));
        let held = lock(&dir).unwrap();
        assert!(lock(&dir).is_err());
        drop(held);
        // A child another test forked while `held` was open keeps the lock
        // until it execs.
        let free = (0..100).any(|_| {
            let free = lock(&dir).is_ok();
            if !free {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            free
        });
        assert!(free);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_stale_download_is_removed_unless_a_window_uses_it() {
        let dir = std::env::temp_dir().join(format!("hako-stale-{}/update", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hako.deb"), b"old").unwrap();
        let held = lock(&dir).unwrap();
        remove_download(&dir);
        assert!(dir.join("hako.deb").exists());
        drop(held);
        // A child another test forked while `held` was open keeps the lock
        // until it execs.
        for _ in 0..100 {
            remove_download(&dir);
            if !dir.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!dir.exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_failed_daemon_restart_still_installs() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: b"boom\n".to_vec(),
        };
        assert_eq!(script_result(&out(0)).unwrap(), None);
        assert!(script_result(&out(RESTART_FAILED))
            .unwrap()
            .unwrap()
            .contains("systemctl --user restart hakod"));
        let e = script_result(&out(1)).unwrap_err();
        assert_eq!(format!("{e:#}"), "install failed: boom");
    }

    #[test]
    fn assets_match_repo_case_insensitively_and_only_that_repo() {
        let v = serde_json::json!({ "assets": [
            { "name": "hako_0.2.0_amd64.deb",
              "browser_download_url": "https://github.com/other/hako/releases/download/v0.2.0/hako_0.2.0_amd64.deb" },
            { "name": "hako_0.2.0_amd64.deb",
              "browser_download_url": "https://github.com/tifuck/hako/releases/download/v0.2.0/hako_0.2.0_amd64.deb" },
        ]});
        let deb = |n: &str| n.ends_with("_amd64.deb");
        let (_, url) = asset(&v, "tifuck/Hako", deb).unwrap();
        assert!(url.starts_with("https://github.com/tifuck/"));
        assert!(asset(&v, "someone/hako", deb).is_none());
    }

    #[test]
    fn the_asset_name_binds_the_release_version() {
        let tar = |v: &str| format!("hako-{v}-{}-linux.tar.gz", std::env::consts::ARCH);
        let rel = |tag: &str, build: &str| {
            let url = |n: &str| format!("https://github.com/o/r/releases/download/{tag}/{n}");
            serde_json::json!({ "tag_name": tag, "assets": [
                { "name": build, "browser_download_url": url(build) },
                { "name": "SHA256SUMS", "browser_download_url": url("SHA256SUMS") },
            ]})
        };
        // An old build (and its signed sums) re-published under a newer tag.
        assert!(release(&rel("v99.0.0", &tar("0.2.5")), "o/r", "0.2.6", false).is_err());
        let deb = format!("hako_99.0.0_{}.deb", deb_arch());
        assert!(release(&rel("v99.0.0", &deb), "o/r", "0.2.6", false).is_err());
        let r = release(&rel("v0.3.0", &tar("0.3.0")), "o/r", "0.2.6", false)
            .unwrap()
            .unwrap();
        assert_eq!((r.version.as_str(), r.name), ("0.3.0", tar("0.3.0")));
        let r = release(&rel("v99.0.0", &deb), "o/r", "0.2.6", true)
            .unwrap()
            .unwrap();
        assert_eq!(r.name, deb);
        assert!(
            release(&rel("v0.2.6", &tar("0.2.6")), "o/r", "0.2.6", false)
                .unwrap()
                .is_none()
        );
        // A tag that is no version is an error, not "up to date", and can't
        // put a path into the file name.
        for tag in ["latest", "v", "v1.0.0-x/../../y", "v1.0.0-rc..1", "v1.0.0 "] {
            let r = release(
                &rel(tag, &tar(tag.trim_start_matches('v'))),
                "o/r",
                "0.2.6",
                false,
            );
            assert!(r.is_err(), "{tag:?}");
        }
    }
}
