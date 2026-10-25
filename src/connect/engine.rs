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
        let mut prober = Prober {
            zeron,
            since_check: 0,
        };
        let mut first = true;
        loop {
            let current = tx.borrow().clone();
            let Some(state) = prober.next_state(current).await else {
                eprintln!("Zeron isn't answering");
                sleep(PROBE_RUNNING).await;
                continue;
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

/// Probes Zeron, and checks that it's still readable now and then.
struct Prober {
    zeron: Arc<Zeron>,
    /// Probes of a running Zeron since its last compatibility check.
    since_check: u32,
}

impl Prober {
    /// Probe once and return Zeron's state, given the `current` one; `None`
    /// if Zeron hangs, so the state stays.
    ///
    /// The compatibility check runs when Zeron comes up, on every probe while
    /// it's outdated (an update restarts it), and every [`CHECK_EVERY`]
    /// probes otherwise.
    async fn next_state(&mut self, current: Engine) -> Option<Engine> {
        let ready = self.zeron.call(methods::ENGINE_READY, json!({}));
        if tokio::time::timeout(PROBE_TIMEOUT, ready)
            .await
            .ok()?
            .is_err()
        {
            return Some(Engine::Closed);
        }
        if current.is_running() && self.since_check < CHECK_EVERY {
            self.since_check += 1;
            return Some(current);
        }
        self.since_check = 0;
        let why = match tokio::time::timeout(CHECK_TIMEOUT, check(&self.zeron)).await {
            Ok(Ok(state)) => return Some(state),
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "it didn't answer in time".into(),
        };
        // A check that couldn't finish keeps the state and runs next probe.
        eprintln!("can't check Zeron yet: {why}");
        self.since_check = CHECK_EVERY;
        Some(current)
    }
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
    // The script answers `closed`, `quit`, `killed` (force-quit), or `stuck`.
    let outcome = String::from_utf8_lossy(&out.stdout);
    match (out.status.success(), outcome.trim()) {
        (_, "stuck") => bail!("couldn't quit Zeron: it's still running"),
        (false, _) => bail!(
            "couldn't quit Zeron: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        (true, "killed") => eprintln!("force-quit Zeron for a restart"),
        (true, _) => eprintln!("quit Zeron for a restart"),
    }
    WAKE.notify_one();
    open().await
}
