//! Whether this Mac's Zeron can take the phone's calls, and starting it.
//!
//! `connect` runs whether or not Zeron is open (at login, as a launch
//! agent), so it probes the engine's IPC and tells the relay when Zeron
//! opens or closes. When Zeron comes up (and now and then while it runs) it
//! also checks that the data this tool reads still parses; if Zeron changed
//! it, Zeron counts as outdated and calls are refused, rather than failing
//! one by one. The phone can open Zeron, or restart it when it hangs.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::{Notify, watch};
use zeron_relay_proto::Engine;

use crate::zeron::{Zeron, ipc, methods};

/// How often to probe while Zeron is closed: short, so it shows up soon
/// after the phone opens it.
const PROBE_CLOSED: Duration = Duration::from_secs(3);
/// How often to probe while it runs.
const PROBE_RUNNING: Duration = Duration::from_secs(15);
/// Re-check compatibility this often while Zeron runs: an update restarts
/// it, but the IPC client may reconnect between two probes.
const CHECK_EVERY: u32 = 20;
/// A probe gets this long: a hung engine keeps its last state, rather than
/// stalling the prober.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The compatibility check reads a few snapshots; a starting engine may be slow.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Zeron's bundle id, for `open -b` and quitting it.
const BUNDLE_ID: &str = "sh.zeron.app";
/// Quits an app by bundle id, force-quitting it if it hangs.
const QUIT_SCRIPT: &str = include_str!("quit.js");

/// Wakes the prober early, so a restart shows up without waiting a full
/// probe interval.
static WAKE: Notify = Notify::const_new();

/// Probe Zeron forever and publish its state.
pub fn spawn(zeron: Arc<Zeron>) -> watch::Receiver<Engine> {
    let (tx, rx) = watch::channel(Engine::Closed);
    tokio::spawn(async move {
        let mut checks = 0;
        let mut first = true;
        loop {
            let probe = zeron.call(methods::ENGINE_READY, json!({}));
            let Ok(answer) = tokio::time::timeout(PROBE_TIMEOUT, probe).await else {
                eprintln!("Zeron isn't answering");
                sleep(PROBE_RUNNING).await;
                continue;
            };
            let now = tx.borrow().clone();
            // Check when Zeron comes up, on every probe while it's outdated
            // (an update restarts it), and every CHECK_EVERY probes otherwise.
            let state = match answer {
                Err(_) => Engine::Closed,
                Ok(_) if now.is_running() && checks < CHECK_EVERY => {
                    checks += 1;
                    now
                }
                Ok(_) => {
                    checks = 0;
                    let why = match tokio::time::timeout(CHECK_TIMEOUT, check(&zeron)).await {
                        Ok(Ok(state)) => Ok(state),
                        Ok(Err(e)) => Err(format!("{e:#}")),
                        Err(_) => Err("it didn't answer in time".into()),
                    };
                    // One that couldn't finish keeps the state and runs next probe.
                    why.unwrap_or_else(|e| {
                        eprintln!("can't check Zeron yet: {e}");
                        checks = CHECK_EVERY;
                        now
                    })
                }
            };
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
            sleep(match state {
                Engine::Closed => PROBE_CLOSED,
                _ => PROBE_RUNNING,
            })
            .await;
        }
    });
    rx
}

/// Sleep for `wait`, or until something wakes the prober.
async fn sleep(wait: Duration) {
    tokio::select! {
        () = tokio::time::sleep(wait) => {}
        () = WAKE.notified() => {}
    }
}

/// The state of a reachable Zeron: running if everything this tool reads
/// still parses, else outdated. `Err` if it couldn't tell (say, Zeron is
/// still starting).
async fn check(zeron: &Zeron) -> Result<Engine> {
    let version = || async {
        let device = zeron.local_device().await.ok();
        device
            .and_then(|d| d.version)
            .unwrap_or_else(|| "unknown".into())
    };
    let reads = async {
        tokio::try_join!(
            zeron.local_device(),
            zeron.chats(),
            zeron.spaces(),
            zeron.sessions(),
            zeron.sidebar(),
            zeron.harnesses(),
        )
    };
    match reads.await {
        Ok((device, ..)) => Ok(Engine::Running {
            running: device.version,
        }),
        Err(e) if incompatible(&e) => {
            eprintln!("Zeron changed in a way this zeron-tools can't read: {e:#}");
            Ok(Engine::Outdated {
                running: version().await,
            })
        }
        Err(e) => Err(e),
    }
}

/// Whether `error` means Zeron changed: a reply that doesn't parse, or a
/// method it no longer has. Anything else (a timeout, a closed socket) may pass.
fn incompatible(error: &anyhow::Error) -> bool {
    ipc::is_unknown_method(error) || error.chain().any(|e| e.is::<serde_json::Error>())
}

/// Why a call can't run in `state`, if it can't.
pub fn refusal(state: &Engine) -> Option<String> {
    match state {
        Engine::Running { .. } => None,
        Engine::Closed => Some("Zeron isn't open on this Mac".into()),
        Engine::Outdated { running } => Some(format!(
            "this Mac runs Zeron {running}, which this zeron-tools can't read; update zeron-tools"
        )),
    }
}

/// The log line for a new state.
fn describe(state: &Engine) -> String {
    match state {
        Engine::Running { running: Some(v) } => format!("Zeron {v} is open"),
        _ => refusal(state).unwrap_or_else(|| "Zeron is open".into()),
    }
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
