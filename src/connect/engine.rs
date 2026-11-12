//! Whether this Mac's Zeron can take the phone's calls, and starting it.
//!
//! It probes the engine's IPC and also checks now and then that the data
//! this tool reads still parses. If Zeron changed it, Zeron counts as
//! outdated and calls are refused, rather than failing one by one.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::sync::{Notify, watch};
use zeron_relay_proto::Engine;

use crate::zeron::{Zeron, daemon, ipc, methods};

/// How often to probe while Zeron is closed: short, so it shows up soon
/// after the phone opens it.
const PROBE_CLOSED: Duration = Duration::from_secs(3);
const PROBE_RUNNING: Duration = Duration::from_secs(15);
/// Re-check compatibility this often while Zeron runs: an update restarts
/// it, but the IPC client may reconnect between two probes.
const CHECK_EVERY: u32 = 20;
/// A probe gets this long: a hung engine keeps its last state, rather than
/// stalling the prober.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The compatibility check reads a few snapshots; a starting engine may be slow.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct EngineWatch {
    state: watch::Receiver<Engine>,
    /// Wakes the prober early, so a start shows up without waiting a full
    /// probe interval.
    wake: Arc<Notify>,
}

impl EngineWatch {
    pub fn now(&self) -> Engine {
        self.state.borrow().clone()
    }

    pub fn changes(&self) -> watch::Receiver<Engine> {
        self.state.clone()
    }

    pub async fn open(&self) -> Result<()> {
        daemon::start().await?;
        self.wake.notify_one();
        Ok(())
    }

    #[cfg(test)]
    pub fn fixed(state: Engine) -> Self {
        Self {
            state: watch::channel(state).1,
            wake: Arc::default(),
        }
    }
}

pub fn spawn(zeron: Arc<Zeron>) -> EngineWatch {
    let (tx, rx) = watch::channel(Engine::Closed);
    let wake = Arc::new(Notify::new());
    let woken = wake.clone();
    tokio::spawn(async move {
        let mut prober = Prober {
            zeron,
            check: Check::Due,
        };
        let mut first = true;
        loop {
            let current = tx.borrow().clone();
            let Some(state) = prober.next_state(current).await else {
                eprintln!("Zeron isn't answering");
                sleep(PROBE_RUNNING, &woken).await;
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
            let wait = match state {
                Engine::Closed => PROBE_CLOSED,
                _ => PROBE_RUNNING,
            };
            sleep(wait, &woken).await;
        }
    });
    EngineWatch { state: rx, wake }
}

async fn sleep(wait: Duration, wake: &Notify) {
    tokio::select! {
        () = tokio::time::sleep(wait) => {}
        () = wake.notified() => {}
    }
}

struct Prober {
    zeron: Arc<Zeron>,
    check: Check,
}

/// When the next compatibility check runs.
#[derive(Clone, Copy)]
enum Check {
    Due,
    /// After this many more probes of a running Zeron.
    After(u32),
}

impl Prober {
    /// Probe once and return Zeron's state, or `None` if Zeron hangs. The
    /// compatibility check runs when Zeron comes up, on every probe while
    /// it's outdated, and every [`CHECK_EVERY`] probes otherwise.
    async fn next_state(&mut self, current: Engine) -> Option<Engine> {
        let ready = self.zeron.call(methods::ENGINE_READY, json!({}));
        if tokio::time::timeout(PROBE_TIMEOUT, ready)
            .await
            .ok()?
            .is_err()
        {
            return Some(Engine::Closed);
        }
        if let (true, Check::After(left @ 1..)) = (current.is_running(), self.check) {
            self.check = Check::After(left - 1);
            return Some(current);
        }
        let why = match tokio::time::timeout(CHECK_TIMEOUT, check(&self.zeron)).await {
            Ok(Ok(state)) => {
                self.check = Check::After(CHECK_EVERY);
                return Some(state);
            }
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "it didn't answer in time".into(),
        };
        // A check that couldn't finish keeps the state and runs next probe.
        eprintln!("can't check Zeron yet: {why}");
        self.check = Check::Due;
        Some(current)
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

fn describe(state: &Engine) -> String {
    match state {
        Engine::Running { running: Some(v) } => format!("Zeron {v} is open"),
        _ => refusal(state).unwrap_or_else(|| "Zeron is open".into()),
    }
}
