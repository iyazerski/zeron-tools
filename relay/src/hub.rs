//! The connected Macs: one WebSocket each, opened by `zeron-tools connect`.
//!
//! The app's calls go to a Mac as [`Call`]s and wait for the matching reply.
//! The Mac decides which tools may run; the relay only routes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use zeron_relay_proto::{Call, Engine, Notice, PROTOCOL, Up};

use crate::store::Machine;

/// How long a call may take. Tool calls never wait on a turn, so this only
/// trips when the Mac is unreachable.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Drop a Mac that sent nothing (it pings every 25 s) for this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(70);
/// The Mac must say hello this soon after connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct Link {
    /// Tells this connection apart from a newer one for the same Mac.
    conn: u64,
    calls: mpsc::Sender<Call>,
    pending: Pending,
    version: String,
    zeron: String,
    /// Unknown until the Mac reports it, right after its hello.
    engine: Option<Engine>,
}

/// A Mac's live state, for the app.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub online: bool,
    pub version: Option<String>,
    pub zeron: Option<String>,
    pub engine: Option<Engine>,
}

#[derive(Default)]
pub struct Hub {
    links: Mutex<HashMap<String, Link>>,
    next: AtomicU64,
}

impl Hub {
    pub fn status(&self, machine: &str) -> Status {
        match self.lock().get(machine) {
            Some(link) => Status {
                online: true,
                version: Some(link.version.clone()),
                zeron: Some(link.zeron.clone()),
                engine: link.engine.clone(),
            },
            None => Status {
                online: false,
                version: None,
                zeron: None,
                engine: None,
            },
        }
    }

    /// Run `tool` on `machine` and return its result.
    pub async fn call(&self, machine: &str, tool: String, args: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (calls, pending) = {
            let links = self.lock();
            let link = links.get(machine).ok_or("this Mac is offline")?;
            (link.calls.clone(), link.pending.clone())
        };
        let (tx, rx) = oneshot::channel();
        lock(&pending).insert(id, tx);
        if calls.send(Call { id, tool, args }).await.is_err() {
            lock(&pending).remove(&id);
            return Err("this Mac went offline".into());
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            // The connection ended and dropped the waiting reply.
            Ok(Err(_)) => Err("this Mac went offline".into()),
            Err(_) => {
                lock(&pending).remove(&id);
                Err("this Mac didn't answer in time".into())
            }
        }
    }

    /// Disconnect `machine`, if it's connected (after it was removed).
    pub fn drop_machine(&self, machine: &str) {
        self.lock().remove(machine);
    }

    /// Serve one Mac's connection until it ends, passing its notices to `notify`.
    pub async fn serve(
        self: Arc<Self>,
        machine: Machine,
        socket: WebSocket,
        notify: impl Fn(&Machine, Notice) + Send,
    ) {
        if let Err(e) = self.serve_inner(&machine, socket, notify).await {
            eprintln!("{}: {e:#}", machine.name);
        }
    }

    async fn serve_inner(
        &self,
        machine: &Machine,
        socket: WebSocket,
        notify: impl Fn(&Machine, Notice) + Send,
    ) -> Result<()> {
        let (mut sink, mut stream) = socket.split();
        let (version, zeron) = match tokio::time::timeout(HELLO_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(Message::Text(body)))) => match serde_json::from_str(&body)? {
                Up::Hello {
                    protocol,
                    version,
                    zeron,
                } if protocol == PROTOCOL => (version, zeron),
                Up::Hello { version, .. } => {
                    let reason = format!(
                        "zeron-tools {version} speaks another relay protocol; update zeron-tools or the relay"
                    );
                    let _ = sink.send(close(&reason)).await;
                    bail!(reason);
                }
                _ => bail!("expected a hello"),
            },
            _ => bail!("no hello"),
        };
        let conn = self.next.fetch_add(1, Ordering::Relaxed);
        let (calls_tx, mut calls) = mpsc::channel::<Call>(64);
        let pending = Pending::default();
        self.lock().insert(
            machine.id.clone(),
            Link {
                conn,
                calls: calls_tx,
                pending: pending.clone(),
                version: version.clone(),
                zeron: zeron.clone(),
                engine: None,
            },
        );
        eprintln!(
            "{} connected (zeron-tools {version}, Zeron {zeron})",
            machine.name
        );
        let result = async {
            loop {
                tokio::select! {
                    frame = tokio::time::timeout(SILENCE_LIMIT, stream.next()) => {
                        match frame.context("the Mac went silent")? {
                            Some(Ok(Message::Text(body))) => match serde_json::from_str(&body)? {
                                Up::Reply { id, result } => {
                                    if let Some(waiter) = lock(&pending).remove(&id) {
                                        let _ = waiter.send(result);
                                    }
                                }
                                Up::Notify(notice) => notify(machine, notice),
                                Up::Engine(engine) => {
                                    if let Some(link) = self.lock().get_mut(&machine.id).filter(|l| l.conn == conn) {
                                        link.engine = Some(engine);
                                    }
                                }
                                Up::Hello { .. } => {}
                            },
                            Some(Ok(Message::Close(_))) | None => return Ok(()),
                            Some(Ok(_)) => {}
                            Some(Err(e)) => return Err(e.into()),
                        }
                    }
                    call = calls.recv() => match call {
                        Some(call) => sink.send(Message::Text(serde_json::to_string(&call)?.into())).await?,
                        // A newer connection replaced this one, or the Mac was removed.
                        None => {
                            let _ = sink.send(close("replaced")).await;
                            return Ok(());
                        }
                    },
                }
            }
        }
        .await;
        let mut links = self.lock();
        if links.get(&machine.id).is_some_and(|l| l.conn == conn) {
            links.remove(&machine.id);
        }
        eprintln!("{} disconnected", machine.name);
        result
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        lock(&self.links)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn close(reason: &str) -> Message {
    Message::Close(Some(axum::extract::ws::CloseFrame {
        code: 1008,
        reason: reason.to_owned().into(),
    }))
}
