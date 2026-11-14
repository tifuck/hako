//! Self-update from GitHub releases.
//!
//! Only release builds know their repo (`HAKO_REPO`, set by the release
//! workflow), and only an install from a release updates itself: the `.deb`
//! (`/usr/bin`, updated through apt) or `install-user.sh` (`~/.local/bin`,
//! updated from the tarball). Every other build never touches the network.
//!
//! Uses `curl`, `sha256sum`, `ssh-keygen` and `tar` like the daemon uses `pw-*`: present on
//! every desktop, no TLS stack in the binary.

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
    let body = run(curl()
        .args(["-H", "Accept: application/vnd.github+json"])
        .arg(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        )))
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

/// Download and verify `rel`, replacing any earlier download.
pub fn download(rel: &Release) -> Result<Ready> {
    let dir = download_dir()?;
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let file = dir.join(&rel.name);
    run(curl().arg("-o").arg(&file).arg(&rel.url)).context("download failed")?;
    let sums = run(curl().arg(&rel.sums_url))?;
    let signed = run(curl().arg(&rel.sig_url))
        .with_context(|| format!("v{} is not signed yet", rel.version))
        .and_then(|sig| {
            verify_signature(&dir, SIGNING_KEY, &sums, &sig)
                .map_err(|e| Rejected(format!("{e:#}")).into())
        });
    if let Err(e) = signed {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    let sums = String::from_utf8(sums)?;
    let Some(want) = expected_sum(&sums, &rel.name) else {
        let _ = std::fs::remove_dir_all(&dir);
        bail!(Rejected(format!(
            "the signed checksums do not list {}, download discarded",
            rel.name
        )));
    };
    let got = String::from_utf8(run(Command::new("sha256sum").arg(&file))?)?;
    if got.split_whitespace().next() != Some(want.as_str()) {
        let _ = std::fs::remove_dir_all(&dir);
        bail!(Rejected("checksum mismatch, download discarded".into()));
    }
    Ok(Ready {
        version: rel.version.clone(),
        file,
        sum: want,
    })
}

/// Install a download, restarting the daemon. Returns a warning for the user
/// when the device rules could not be updated. The download is kept on
/// failure so the user can retry.
pub fn install(ready: &Ready) -> Result<Option<String>> {
    let dir = ready.file.parent().context("bad download path")?;
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
            Command::new("pkexec").args(["sh", "-c"]).arg(
                "t=$(mktemp -d) && cat > \"$t/hako.deb\" && apt-get install -y \"$t/hako.deb\"; s=$?; rm -rf \"$t\"; exit $s",
            ),
            &data,
        )
        .context("install was cancelled or failed")?;
        return Ok(None);
    }
    run_input(
        Command::new("tar").args(["-xzf", "-", "-C"]).arg(dir),
        &data,
    )?;
    let root = name.trim_end_matches(".tar.gz");
    // Rules first, so the restarted daemon opens devices with the new access.
    let rules = run_input(
        Command::new("tar")
            .args(["-xzOf", "-"])
            .arg(format!("{root}/udev/70-hako.rules")),
        &data,
    )
    .context("the update has no device rules")
    .and_then(|r| update_udev(&r));
    let warn = rules.err().map(|e| {
        format!(
            "New device rules were not installed ({e:#}). Run: hakod --udev-rules | sudo tee {UDEV_RULES} && sudo udevadm control --reload-rules && sudo udevadm trigger"
        )
    });
    run(Command::new("bash")
        .arg(dir.join(root).join("scripts/install-user.sh"))
        .env("HAKO_SKIP_UDEV", "1"))
    .context("install failed")?;
    Ok(warn)
}

/// The device rules live in /etc: ask through polkit, only when they changed.
fn update_udev(fresh: &[u8]) -> Result<()> {
    if std::fs::read(UDEV_RULES).ok().as_deref() == Some(fresh) {
        return Ok(());
    }
    run_input(
        Command::new("pkexec").args(["sh", "-c"]).arg(format!(
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

fn curl() -> Command {
    let mut c = Command::new("curl");
    c.args([
        "-fsSL",
        "--proto",
        "=https",
        "--retry",
        "2",
        "--max-time",
        "300",
    ]);
    c
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
/// -n hako-release`). `dir` holds the files ssh-keygen reads.
fn verify_signature(dir: &Path, key: &str, data: &[u8], sig: &[u8]) -> Result<()> {
    let signers = dir.join("allowed_signers");
    let sig_file = dir.join("SHA256SUMS.sig");
    std::fs::write(
        &signers,
        format!("{SIG_NAMESPACE} namespaces=\"{SIG_NAMESPACE}\" {key}\n"),
    )?;
    std::fs::write(&sig_file, sig)?;
    run_input(
        Command::new("ssh-keygen")
            .args([
                "-Y",
                "verify",
                "-I",
                SIG_NAMESPACE,
                "-n",
                SIG_NAMESPACE,
                "-f",
            ])
            .arg(&signers)
            .arg("-s")
            .arg(&sig_file),
        data,
    )
    .context("release signature check failed (needs ssh-keygen from openssh-client)")?;
    Ok(())
}

/// Fails unless `data` still hashes to `want`, the sum `download` verified.
fn check_sum(data: &[u8], want: &str) -> Result<()> {
    let got = String::from_utf8(run_input(&mut Command::new("sha256sum"), data)?)?;
    if got.split_whitespace().next() != Some(want) {
        bail!("checksum mismatch, the download changed since it was verified");
    }
    Ok(())
}

/// `(dotted numbers, is a release)` of `1.2.3` or `1.2.3-rc1`. Only ASCII
/// letters, digits, `.` and `-` pass (no `..`), so a version is safe inside
/// a file name.
fn parse_version(s: &str) -> Option<(Vec<u64>, bool)> {
    if s.contains("..")
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b))
    {
        return None;
    }
    let (nums, pre) = s.split_once('-').map_or((s, false), |(n, _)| (n, true));
    let nums = nums
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    Some((nums, !pre))
}

/// `remote` > `local`, comparing dotted numbers; on a tie a release beats a
/// pre-release (`0.4.0` > `0.4.0-rc1`), as in semver.
fn newer(remote: &str, local: &str) -> bool {
    matches!((parse_version(remote), parse_version(local)), (Some(r), Some(l)) if r > l)
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

    #[test]
    fn only_the_release_key_signs_updates() {
        let dir = std::env::temp_dir().join(format!("hako-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let keygen = |name: &str| {
            let key = dir.join(name);
            run(Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
                .arg(&key))
            .unwrap();
            key
        };
        let (good, bad) = (keygen("good"), keygen("bad"));
        let sign = |key: &Path, data: &[u8]| {
            run_input(
                Command::new("ssh-keygen")
                    .args(["-q", "-Y", "sign", "-n", SIG_NAMESPACE, "-f"])
                    .arg(key),
                data,
            )
            .unwrap()
        };
        let public = std::fs::read_to_string(good.with_extension("pub")).unwrap();
        let sums = b"abc  hako.deb\n";
        let sig = sign(&good, sums);
        assert!(verify_signature(&dir, public.trim(), sums, &sig).is_ok());
        assert!(verify_signature(&dir, public.trim(), b"abd  hako.deb\n", &sig).is_err());
        assert!(verify_signature(&dir, public.trim(), sums, &sign(&bad, sums)).is_err());
        assert!(verify_signature(&dir, public.trim(), sums, b"not a signature").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn changed_download_fails_the_sum_check() {
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(check_sum(b"abc", abc).is_ok());
        assert!(check_sum(b"abd", abc).is_err());
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
