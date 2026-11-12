//! Self-update from GitHub releases.
//!
//! Only release builds know their repo (`HAKO_REPO`, set by the release
//! workflow), and only an install made by `install-user.sh` (`~/.local/bin`)
//! updates itself. Every other build never touches the network.
//!
//! Uses `curl`, `sha256sum` and `tar` like the daemon uses `pw-*`: present on
//! every desktop, no TLS stack in the binary.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: Option<&str> = option_env!("HAKO_REPO");
const UDEV_RULES: &str = "/etc/udev/rules.d/70-hako.rules";
/// Set for the restarted UI so it can confirm the update.
pub const UPDATED_ENV: &str = "HAKO_UPDATED";
pub const WARN_ENV: &str = "HAKO_UPDATE_WARN";

/// A published release newer than this build.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    /// Tarball asset name and URL.
    name: String,
    url: String,
    sums_url: String,
}

/// Where this build updates from: `(owner/repo, installed hako path)`.
/// Resolve once at startup: after an update the running exe is replaced.
pub fn target() -> Option<(&'static str, PathBuf)> {
    let repo = REPO.filter(|r| !r.is_empty())?;
    let bin = PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/hako");
    let exe = std::env::current_exe().ok()?;
    (std::fs::canonicalize(&bin).ok()? == exe).then_some((repo, bin))
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
    let suffix = format!("-{}-linux.tar.gz", std::env::consts::ARCH);
    // Only follow download links of this repo's own releases.
    let prefix = format!("https://github.com/{repo}/releases/download/");
    let asset = |want: &dyn Fn(&str) -> bool| {
        v["assets"].as_array().into_iter().flatten().find_map(|a| {
            let name = a["name"].as_str().filter(|n| want(n))?;
            let url = a["browser_download_url"]
                .as_str()
                .filter(|u| u.starts_with(&prefix))?;
            Some((name.to_string(), url.to_string()))
        })
    };
    let (name, url) = asset(&|n| n.ends_with(&suffix)).context("no build for this computer")?;
    let (_, sums_url) = asset(&|n| n == "SHA256SUMS").context("release has no checksums")?;
    Ok(Some(Release {
        version: version.into(),
        name,
        url,
        sums_url,
    }))
}

/// Download, verify and install `rel`, restarting the daemon. Returns a
/// warning for the user when the device rules could not be updated.
pub fn install(rel: &Release) -> Result<Option<String>> {
    let dir = std::env::temp_dir().join(format!("hako-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let res = install_in(rel, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    res
}

fn install_in(rel: &Release, dir: &Path) -> Result<Option<String>> {
    let tarball = dir.join(&rel.name);
    run(curl().arg("-o").arg(&tarball).arg(&rel.url)).context("download failed")?;
    let sums = String::from_utf8(run(curl().arg(&rel.sums_url))?)?;
    let want = expected_sum(&sums, &rel.name).context("checksums do not list the download")?;
    let got = String::from_utf8(run(Command::new("sha256sum").arg(&tarball))?)?;
    if got.split_whitespace().next() != Some(want.as_str()) {
        bail!("checksum mismatch, download discarded");
    }
    run(Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(dir))?;
    let root = dir.join(rel.name.trim_end_matches(".tar.gz"));
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

/// `remote` > `local`, comparing dotted numbers (pre-release suffix ignored).
fn newer(remote: &str, local: &str) -> bool {
    let parse = |s: &str| -> Option<Vec<u64>> {
        s.split('-')
            .next()?
            .split('.')
            .map(|p| p.parse().ok())
            .collect()
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
}
