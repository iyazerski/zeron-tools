//! `update`: replace this binary with the latest GitHub release when it's
//! newer, and restart the background service on it.
//!
//! It downloads and checks the same files as `install.sh`, with the same
//! tools (`curl`, `shasum`, `tar`), so zeron-tools needs no HTTP client.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::connect;

const REPO: &str = "iyazerski/zeron-tools";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The release archive; `install.sh` downloads the same one.
const ASSET: &str = "zeron-tools-aarch64-apple-darwin.tar.gz";

/// Install the latest release over this binary if it's newer than [`VERSION`].
pub fn run() -> Result<()> {
    // GitHub redirects `releases/latest` to the latest release's tag page.
    let latest_url = run_tool(
        Command::new("/usr/bin/curl")
            .args(["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}"])
            .arg(format!("https://github.com/{REPO}/releases/latest")),
        "asking GitHub for the latest release",
    )?;
    let tag = latest_url
        .rsplit_once("/tag/")
        .map(|(_, tag)| tag.trim().to_string())
        .with_context(|| format!("unexpected latest release address {latest_url}"))?;
    let latest = tag.trim_start_matches('v');
    if !is_newer(latest, VERSION) {
        println!("zeron-tools {VERSION} is up to date");
        return Ok(());
    }
    println!("downloading zeron-tools {latest}…");
    let exe = std::env::current_exe().context("finding the zeron-tools binary")?;
    let dir = exe.parent().context("the binary has no folder")?;
    let staging = dir.join(".zeron-tools-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let installed = install(&tag, &staging, &exe);
    let _ = std::fs::remove_dir_all(&staging);
    installed?;
    println!("updated zeron-tools {VERSION} → {latest}");
    if connect::restart()? {
        println!("restarted connect in the background");
    }
    Ok(())
}

/// Download, verify, and unpack `tag`'s archive in `staging`, then move the
/// binary over `exe` in one step, so a failure never leaves a broken binary.
fn install(tag: &str, staging: &Path, exe: &Path) -> Result<()> {
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{ASSET}");
    for (url, file) in [
        (url.clone(), ASSET.to_string()),
        (format!("{url}.sha256"), format!("{ASSET}.sha256")),
    ] {
        run_tool(
            Command::new("/usr/bin/curl")
                .args(["-fsSL", "-o"])
                .arg(staging.join(file))
                .arg(url),
            "downloading the release",
        )?;
    }
    // The checksum file names the archive relative to its own folder.
    run_tool(
        Command::new("/usr/bin/shasum")
            .args(["-a", "256", "-c", &format!("{ASSET}.sha256")])
            .current_dir(staging),
        "checking the download",
    )?;
    run_tool(
        Command::new("/usr/bin/tar")
            .args(["-xzf", ASSET, "zeron-tools"])
            .current_dir(staging),
        "unpacking the release",
    )?;
    std::fs::rename(staging.join("zeron-tools"), exe)
        .with_context(|| format!("replacing {}", exe.display()))
}

/// Run `command` and return its trimmed stdout, or fail with `what` and its stderr.
fn run_tool(command: &mut Command, what: &str) -> Result<String> {
    let out = command
        .output()
        .with_context(|| format!("{what}: running {:?}", command.get_program()))?;
    if !out.status.success() {
        bail!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether version `a` (like `0.11.10`) is newer than `b`, comparing the
/// numbers in order.
fn is_newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parts(a) > parts(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(is_newer("0.11.10", "0.11.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.11.6", "0.11.6"));
        assert!(!is_newer("0.11.5", "0.11.6"));
    }
}
