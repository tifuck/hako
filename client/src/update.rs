//! Self-update from GitHub releases.
//!
//! Only release builds know their repo (`HAKO_REPO`, set by the release
//! workflow), and only an install from a release updates itself: the `.deb`
//! (`/usr/bin`, updated through apt) or `install-user.sh` (`~/.local/bin`,
//! updated from the tarball). Every other build never touches the network.
//!
//! Uses `curl`, `sha256sum` and `tar` like the daemon uses `pw-*`: present on
//! every desktop, no TLS stack in the binary.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: Option<&str> = option_env!("HAKO_REPO");
const DEB_BIN: &str = "/usr/bin/hako";
const UDEV_RULES: &str = "/etc/udev/rules.d/70-hako.rules";
/// Set for the restarted UI so it can confirm the update.
pub const UPDATED_ENV: &str = "HAKO_UPDATED";
pub const WARN_ENV: &str = "HAKO_UPDATE_WARN";

/// A published release newer than this build.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Asset name (tarball or `.deb`) and URL.
    name: String,
    url: String,
    sums_url: String,
}

/// A downloaded and verified release, installed on restart.
#[derive(Clone, Debug)]
pub struct Ready {
    pub version: String,
    file: PathBuf,
}

/// Where this build updates from: `(owner/repo, installed hako path)`.
/// Resolve once at startup: after an update the running exe is replaced.
pub fn target() -> Option<(&'static str, PathBuf)> {
    let repo = REPO.filter(|r| !r.is_empty())?;
    let exe = std::env::current_exe().ok()?;
    let user = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/hako");
    [user, PathBuf::from(DEB_BIN)]
        .into_iter()
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
    let tag = v["tag_name"].as_str().context("release has no tag")?;
    let version = tag.trim_start_matches('v');
    if !newer(version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let suffix = if deb_install() {
        format!("_{}.deb", deb_arch())
    } else {
        format!("-{}-linux.tar.gz", std::env::consts::ARCH)
    };
    let (name, url) =
        asset(&v, repo, |n| n.ends_with(&suffix)).context("no build for this computer")?;
    let (_, sums_url) =
        asset(&v, repo, |n| n == "SHA256SUMS").context("release has no checksums")?;
    Ok(Some(Release {
        version: version.into(),
        name,
        url,
        sums_url,
    }))
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
    let sums = String::from_utf8(run(curl().arg(&rel.sums_url))?)?;
    let want = expected_sum(&sums, &rel.name).context("checksums do not list the download")?;
    let got = String::from_utf8(run(Command::new("sha256sum").arg(&file))?)?;
    if got.split_whitespace().next() != Some(want.as_str()) {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("checksum mismatch, download discarded");
    }
    Ok(Ready {
        version: rel.version.clone(),
        file,
    })
}

/// Install a download, restarting the daemon. Returns a warning for the user
/// when the device rules could not be updated. The download is kept on
/// failure so the user can retry.
pub fn install(ready: &Ready) -> Result<Option<String>> {
    let dir = ready.file.parent().context("bad download path")?;
    let res = install_file(&ready.file, dir);
    if res.is_ok() {
        let _ = std::fs::remove_dir_all(dir);
    }
    res
}

fn install_file(file: &Path, dir: &Path) -> Result<Option<String>> {
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .context("bad download name")?;
    if name.ends_with(".deb") {
        // apt installs rules and units and restarts the daemons (postinst).
        run(Command::new("pkexec")
            .args(["apt-get", "install", "-y", "--allow-downgrades"])
            .arg(file))
        .context("install was cancelled or failed")?;
        return Ok(None);
    }
    run(Command::new("tar").arg("-xzf").arg(file).arg("-C").arg(dir))?;
    let root = dir.join(name.trim_end_matches(".tar.gz"));
    // Rules first, so the restarted daemon opens devices with the new access.
    let warn = update_udev(&root.join("udev/70-hako.rules"));
    run(Command::new("bash")
        .arg(root.join("scripts/install-user.sh"))
        .env("HAKO_SKIP_UDEV", "1"))
    .context("install failed")?;
    Ok(warn)
}

/// The device rules live in /etc: ask through polkit, only when they changed.
fn update_udev(new: &Path) -> Option<String> {
    let fresh = std::fs::read(new).ok()?;
    if std::fs::read(UDEV_RULES).ok().as_deref() == Some(fresh.as_slice()) {
        return None;
    }
    let ok = Command::new("pkexec")
        .args(["sh", "-c"])
        .arg(format!(
            "install -m 644 \"$1\" {UDEV_RULES} && udevadm control --reload-rules && udevadm trigger"
        ))
        .arg("sh")
        .arg(new)
        .status()
        .is_ok_and(|s| s.success());
    (!ok).then(|| {
        format!(
            "New device rules were not installed. Run: hakod --udev-rules | sudo tee {UDEV_RULES} && sudo udevadm control --reload-rules && sudo udevadm trigger"
        )
    })
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
    let out = cmd
        .output()
        .with_context(|| format!("cannot run {:?}", cmd.get_program()))?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `remote` > `local`, comparing dotted numbers; on a tie a release beats a
/// pre-release (`0.4.0` > `0.4.0-rc1`), as in semver.
fn newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| -> Option<(Vec<u64>, bool)> {
        let (nums, pre) = s.split_once('-').map_or((s, false), |(n, _)| (n, true));
        let nums = nums
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        Some((nums, !pre))
    };
    matches!((parse(remote), parse(local)), (Some(r), Some(l)) if r > l)
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
}
