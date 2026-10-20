//! Background upkeep while Zeron runs: archive old chats ([`archive`]) and
//! title untitled ones ([`rename`]), a while after Zeron opens and then on
//! a fixed interval. Both write through the engine, so Zeron applies the
//! changes as if made in its window.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use zeron_relay_proto::Engine;

use crate::archive::{self, Memory};
use crate::rename::{self, Pi, Tried};
use crate::zeron::Zeron;

/// How long after Zeron opens the first run waits: it may still be syncing.
const START_DELAY: Duration = Duration::from_secs(60);
/// The time between runs while Zeron stays open.
const EVERY: Duration = Duration::from_secs(15 * 60);

/// What to keep up.
pub struct Opts {
    /// Keep this many most recently active chats per project; `None`
    /// doesn't archive.
    pub keep: Option<usize>,
    /// The model that titles untitled chats; `None` doesn't title them.
    pub pi: Option<Pi>,
    /// Where the archive's memory lives.
    pub memory: PathBuf,
}

impl Opts {
    /// Whether there is anything to do.
    pub fn any(&self) -> bool {
        self.keep.is_some() || self.pi.is_some()
    }
}

/// Run the upkeep whenever Zeron runs, until the engine watch ends.
pub async fn run(zeron: Arc<Zeron>, mut engine: watch::Receiver<Engine>, opts: Opts) {
    if !opts.any() {
        return;
    }
    let mut memory = Memory::load(&opts.memory).unwrap_or_else(|e| {
        eprintln!("starting with an empty archive memory: {e:#}");
        Memory::empty(&opts.memory)
    });
    let mut tried = Tried::new();
    loop {
        let was_running = engine.borrow().is_running();
        if engine.wait_for(Engine::is_running).await.is_err() {
            return;
        }
        tokio::time::sleep(if was_running { EVERY } else { START_DELAY }).await;
        if !engine.borrow().is_running() {
            continue;
        }
        if let Some(keep) = opts.keep
            && let Err(e) = archive::run(&zeron, keep, &mut memory).await
        {
            eprintln!("archiving old chats failed: {e:#}");
        }
        if let Some(pi) = &opts.pi
            && let Err(e) = rename::run(&zeron, pi, &mut tried).await
        {
            eprintln!("titling chats failed: {e:#}");
        }
    }
}
