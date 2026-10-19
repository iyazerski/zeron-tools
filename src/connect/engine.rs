//! Whether this Mac's Zeron can take the phone's calls, and starting it.
//!
//! `connect` runs whether or not Zeron is open (at login, as a launch
//! agent), so it probes the engine's IPC and tells the relay when Zeron
//! opens, closes, or runs a version this build doesn't target. The phone
//! can open Zeron, or restart it when it hangs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::{Notify, watch};
use zeron_mcp::Zeron;
use zeron_relay_proto::Engine;

use crate::zeron::ZERON_VERSION;

/// How often to probe while Zeron is closed: short, so it shows up soon
/// after the phone opens it.
const PROBE_CLOSED: Duration = Duration::from_secs(3);
/// How often to probe while it runs.
const PROBE_RUNNING: Duration = Duration::from_secs(15);
/// Re-check the version this often while Zeron runs. Zeron records its
/// version in the profile when it boots, maybe after the IPC is up.
const VERSION_EVERY: u32 = 20;
/// A probe gets this long: a hung engine keeps its last state, rather than
/// stalling the prober.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Zeron's bundle id, for `open -b` and quitting it.
const BUNDLE_ID: &str = "sh.zeron.app";
/// Quits an app by bundle id, force-quitting it if it hangs.
const QUIT_SCRIPT: &str = include_str!("quit.js");

/// Wakes the prober early, so a restart shows up without waiting a full
/// probe interval.
static WAKE: Notify = Notify::const_new();

/// Reads the version Zeron last booted with from the profile.
pub type VersionCheck = Arc<dyn Fn() -> Result<Option<String>> + Send + Sync>;

/// Probe Zeron forever and publish its state. `version` is `None` with
/// `--ignore-version`.
pub fn spawn(zeron: Arc<Zeron>, version: Option<VersionCheck>) -> watch::Receiver<Engine> {
    let (tx, rx) = watch::channel(Engine::Closed);
    tokio::spawn(async move {
        let mut checks = 0;
        let mut first = true;
        loop {
            let probe = zeron.call(zeron_rpc::methods::ENGINE_READY, json!({}));
            let Ok(answer) = tokio::time::timeout(PROBE_TIMEOUT, probe).await else {
                eprintln!("Zeron isn't answering");
                tokio::select! {
                    () = tokio::time::sleep(PROBE_RUNNING) => {}
                    () = WAKE.notified() => {}
                }
                continue;
            };
            let reachable = answer.is_ok();
            let was_closed = *tx.borrow() == Engine::Closed;
            let state = if !reachable {
                Engine::Closed
            } else if was_closed || checks % VERSION_EVERY == 0 {
                running_state(version.clone()).await
            } else {
                tx.borrow().clone()
            };
            checks = if reachable { checks + 1 } else { 0 };
            tx.send_if_modified(|now| {
                let changed = *now != state;
                // Log the first state too, so the log says where things stand.
                if changed || first {
                    eprintln!("{}", describe(&state));
                }
                *now = state.clone();
                changed
            });
            first = false;
            let wait = match state {
                Engine::Closed => PROBE_CLOSED,
                _ => PROBE_RUNNING,
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = WAKE.notified() => {}
            }
        }
    });
    rx
}

/// The state of a reachable Zeron: running, unless its version is wrong.
/// An unreadable version counts as running, since the engine answers.
async fn running_state(version: Option<VersionCheck>) -> Engine {
    let Some(version) = version else {
        return Engine::Running;
    };
    match tokio::task::spawn_blocking(move || version()).await {
        Ok(Ok(Some(running))) if running != ZERON_VERSION => Engine::Outdated { running },
        Ok(Ok(_)) => Engine::Running,
        Ok(Err(e)) => {
            eprintln!("can't read Zeron's version: {e:#}");
            Engine::Running
        }
        Err(e) => {
            eprintln!("can't read Zeron's version: {e}");
            Engine::Running
        }
    }
}

/// Why a call can't run in `state`, if it can't.
pub fn refusal(state: &Engine) -> Option<String> {
    match state {
        Engine::Running => None,
        Engine::Closed => Some("Zeron isn't open on this Mac".into()),
        Engine::Outdated { running } => Some(format!(
            "this Mac runs Zeron {running}, but its zeron-tools targets {ZERON_VERSION}; update zeron-tools"
        )),
    }
}

/// The log line for a new state.
fn describe(state: &Engine) -> String {
    refusal(state).unwrap_or_else(|| "Zeron is open".into())
}

/// Start Zeron hidden in the background, as if opened from the Dock.
pub async fn open() -> Result<()> {
    let out = tokio::process::Command::new("/usr/bin/open")
        .args(["-g", "-j", "-b", BUNDLE_ID])
        .output()
        .await
        .context("running open")?;
    if !out.status.success() {
        bail!(
            "couldn't open Zeron: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    WAKE.notify_one();
    Ok(())
}

/// Quit Zeron (force-quitting it if it hangs), then start it again. Works
/// while Zeron is closed too: then it just opens it.
pub async fn restart() -> Result<()> {
    let out = tokio::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", QUIT_SCRIPT, BUNDLE_ID])
        .output()
        .await
        .context("running osascript")?;
    let result = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || result == "stuck" {
        bail!(
            "couldn't quit Zeron: {}",
            if result == "stuck" {
                "it's still running".into()
            } else {
                String::from_utf8_lossy(&out.stderr).trim().to_string()
            }
        );
    }
    eprintln!(
        "{} Zeron for a restart",
        if result == "killed" {
            "force-quit"
        } else {
            "quit"
        }
    );
    WAKE.notify_one();
    open().await
}
