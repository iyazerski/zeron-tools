//! Background upkeep while Zeron runs: archive old chats ([`archive`]) and
//! title ones Zeron couldn't ([`rename`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use zeron_relay_proto::Engine;

use crate::archive::{self, Memory};
use crate::pi::Pi;
use crate::rename::{self, Tried};
use crate::zeron::Zeron;

/// How long after Zeron opens the first run waits: it may still be syncing.
const START_DELAY: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(15 * 60);

#[derive(Debug)]
pub struct Upkeep {
    /// Keep this many most recently active chats per project; `None`
    /// doesn't archive.
    pub keep: Option<usize>,
    /// The model that titles chats Zeron couldn't; `None` doesn't title them.
    pub pi: Option<Pi>,
    pub memory: PathBuf,
}

impl Upkeep {
    pub fn has_work(&self) -> bool {
        self.keep.is_some() || self.pi.is_some()
    }
}

/// Run the upkeep whenever Zeron runs, until the engine watch ends.
pub async fn run(zeron: Arc<Zeron>, mut engine: watch::Receiver<Engine>, upkeep: Upkeep) {
    if !upkeep.has_work() {
        return;
    }
    let mut memory = Memory::load(&upkeep.memory).unwrap_or_else(|e| {
        eprintln!("starting with an empty archive memory: {e:#}");
        Memory::empty(&upkeep.memory)
    });
    let mut tried = Tried::new();
    let mut delay = START_DELAY;
    loop {
        if engine.wait_for(Engine::is_running).await.is_err() {
            return;
        }
        // Zeron closing during the wait starts it over when Zeron reopens.
        let closed = engine.wait_for(|e| !e.is_running());
        match tokio::time::timeout(delay, closed).await {
            Err(_) => {}
            Ok(Ok(_)) => {
                delay = START_DELAY;
                continue;
            }
            Ok(Err(_)) => return,
        }
        delay = EVERY;
        if let Some(keep) = upkeep.keep
            && let Err(e) = archive::run(&zeron, keep, &mut memory).await
        {
            eprintln!("archiving old chats failed: {e:#}");
        }
        if let Some(pi) = &upkeep.pi
            && let Err(e) = rename::run(&zeron, pi, &mut tried).await
        {
            eprintln!("titling chats failed: {e:#}");
        }
    }
}
