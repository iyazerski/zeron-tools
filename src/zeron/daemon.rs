//! Zeron's engine as a launchd service of its own, through Zeron's `daemon`
//! command, so it runs without the app: it starts at login, restarts after
//! a crash, and once the app updates itself, the engine restarts into the
//! new version as soon as no agent works (`crates/update` `SupersededRestart`).
//! The app, when opened, connects to it instead of starting its own engine
//! (`crates/ui/src/state.rs` `attach_to_daemon`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::types::SessionStatus;
use super::{Zeron, methods};

/// The service's launchd label, as Zeron's `daemon` command names it.
const LABEL: &str = "sh.zeron.app";
/// The app's bundle id, to quit it.
const APP_ID: &str = "sh.zeron.app";
/// The engine executable in the app bundle.
const ENGINE: &str = "Zeron.app/Contents/MacOS/Zeron Engine";
/// Quits an app by bundle id, force-quitting it if it hangs.
const QUIT_SCRIPT: &str = include_str!("quit.js");
/// How long the app's engine may take to drain and let go of the data dir.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether the service is installed.
pub fn installed() -> bool {
    plist_path().exists()
}

/// Run Zeron's engine as a service, on `port` and with `data_dir`, unless it
/// already does; then just make sure it runs. The app's own engine holds the
/// data dir, so it stops first, and the app with it, since the app would
/// start its engine again. That's refused while an agent works.
pub async fn install(zeron: &Zeron, data_dir: &Path, port: u16) -> Result<()> {
    if installed() {
        return start().await;
    }
    let running = zeron.call(methods::ENGINE_READY, json!({})).await.is_ok();
    if running {
        let device = zeron.local_device_id().await?;
        let working = zeron
            .sessions()
            .await?
            .into_iter()
            .filter(|s| s.device_id == device && s.status == SessionStatus::Working)
            .count();
        if working > 0 {
            bail!(
                "agents work in {working} chat(s): run this again once they finish, since moving Zeron's engine to a service restarts it"
            );
        }
    }
    quit_app().await?;
    if running {
        zeron
            .call(methods::STOP_ENGINE, json!({}))
            .await
            .context("stopping Zeron's engine")?;
    }
    wait_until_released(data_dir).await?;
    daemon(
        &["install"],
        &[
            ("ZERON_DATA_DIR", data_dir.to_string_lossy().into_owned()),
            ("ZERON_IPC_PORT", port.to_string()),
        ],
    )
    .await
}

/// Start the service if it's stopped.
pub async fn start() -> Result<()> {
    require_installed()?;
    daemon(&["start"], &[]).await
}

/// Restart the service; launchd kills the engine if it hangs.
pub async fn restart() -> Result<()> {
    require_installed()?;
    daemon(&["restart"], &[]).await
}

/// The way to stop the service, for the user to run.
pub fn uninstall_command() -> String {
    format!(
        "\"{}\" daemon uninstall",
        engine_binary()
            .unwrap_or_else(|_| Path::new("/Applications").join(ENGINE))
            .display()
    )
}

fn require_installed() -> Result<()> {
    if !installed() {
        bail!("Zeron's engine doesn't run as a service on this Mac: run zeron-tools install");
    }
    Ok(())
}

/// Run the engine's `daemon` command with `args`, adding `env`.
async fn daemon(args: &[&str], env: &[(&str, String)]) -> Result<()> {
    let engine = engine_binary()?;
    let out = tokio::process::Command::new(&engine)
        .arg("daemon")
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .await
        .with_context(|| format!("running {}", engine.display()))?;
    if !out.status.success() {
        bail!(
            "Zeron's `daemon {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The installed app's engine executable.
fn engine_binary() -> Result<PathBuf> {
    [
        PathBuf::from("/Applications"),
        crate::paths::home().join("Applications"),
    ]
    .into_iter()
    .map(|dir| dir.join(ENGINE))
    .find(|path| path.is_file())
    .context("Zeron isn't installed in /Applications or ~/Applications")
}

fn plist_path() -> PathBuf {
    crate::paths::home()
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Quit the app, force-quitting it if it hangs.
async fn quit_app() -> Result<()> {
    let out = tokio::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", QUIT_SCRIPT, APP_ID])
        .output()
        .await
        .context("running osascript")?;
    // The script answers `closed`, `quit`, `killed` (force-quit), or `stuck`.
    let outcome = String::from_utf8_lossy(&out.stdout);
    match (out.status.success(), outcome.trim()) {
        (_, "stuck") => bail!("couldn't quit Zeron: it's still running"),
        (false, _) => bail!(
            "couldn't quit Zeron: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        (true, "closed") => {}
        (true, _) => println!("quit Zeron; open it again any time, it connects to the service"),
    }
    Ok(())
}

/// Wait until no engine holds `data_dir`. An engine flocks `engine.lock`
/// there until it exits, which is after it closes its IPC port.
async fn wait_until_released(data_dir: &Path) -> Result<()> {
    let path = data_dir.join("engine.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
        if Instant::now() >= deadline {
            bail!("Zeron's engine didn't stop in {} s", STOP_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
