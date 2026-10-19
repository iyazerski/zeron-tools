//! The connected Macs: one WebSocket each, opened by `zeron-tools connect`.
//!
//! The app's calls go to a Mac as [`Call`]s and wait for the matching reply.
//! The Mac decides which tools may run; the relay only routes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::time::{Duration, Instant};

use anyhow::{Result, bail};
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
        // Forget the waiter however the call ends, also when the app hangs up.
        let _waiting = Waiting {
            pending: &pending,
            id,
        };
        // The deadline covers queueing too, not just the Mac's answer.
        let exchange = async move {
            let sent = calls.send(Call { id, tool, args }).await;
            // Don't keep the link's call queue open: a replaced connection
            // ends when its last sender is gone.
            drop(calls);
            if sent.is_err() {
                return Err("this Mac went offline".into());
            }
            // An error: the connection ended and dropped the waiting reply.
            rx.await
                .unwrap_or_else(|_| Err("this Mac went offline".into()))
        };
        tokio::time::timeout(CALL_TIMEOUT, exchange)
            .await
            .unwrap_or_else(|_| Err("this Mac didn't answer in time".into()))
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
            // Outgoing calls must not postpone this, so it's a fixed deadline
            // that only frames from the Mac move.
            let mut heard = Instant::now();
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(heard + SILENCE_LIMIT) => bail!("the Mac went silent"),
                    frame = stream.next() => {
                        heard = Instant::now();
                        match frame {
                            Some(Ok(Message::Text(body))) => match serde_json::from_str(&body) {
                                // A frame this relay can't read: skip it, and
                                // let a reply it held time out.
                                Err(e) => eprintln!("{}: skipped a bad message: {e}", machine.name),
                                Ok(Up::Reply { id, result }) => {
                                    if let Some(waiter) = lock(&pending).remove(&id) {
                                        let _ = waiter.send(result);
                                    }
                                }
                                Ok(Up::Notify(notice)) => notify(machine, notice),
                                Ok(Up::Engine(engine)) => {
                                    if let Some(link) = self.lock().get_mut(&machine.id).filter(|l| l.conn == conn) {
                                        link.engine = Some(engine);
                                    }
                                }
                                Ok(Up::Hello { .. }) => {}
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
        // Fail this connection's calls now rather than at their timeout:
        // unlist it, refuse new calls, then drop the waiting replies.
        {
            let mut links = self.lock();
            if links.get(&machine.id).is_some_and(|l| l.conn == conn) {
                links.remove(&machine.id);
            }
        }
        calls.close();
        lock(&pending).clear();
        eprintln!("{} disconnected", machine.name);
        result
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Link>> {
        lock(&self.links)
    }
}

/// Removes a call's waiter from its connection when the call ends.
struct Waiting<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        lock(self.pending).remove(&self.id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::WebSocketUpgrade;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message as Frame;

    /// A call waiting on a Mac fails as soon as its connection drops.
    #[tokio::test]
    async fn calls_fail_at_once_when_the_mac_disconnects() {
        let hub = Arc::new(Hub::default());
        let mac = Machine {
            id: "m".into(),
            name: "Mac".into(),
        };
        let serve = hub.clone();
        let app = axum::Router::new().route(
            "/connect",
            axum::routing::get(move |ws: WebSocketUpgrade| {
                let (hub, mac) = (serve.clone(), mac.clone());
                async move { ws.on_upgrade(move |socket| hub.serve(mac, socket, |_, _| {})) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/connect"))
            .await
            .unwrap();
        let hello = Up::Hello {
            protocol: PROTOCOL,
            version: "1".into(),
            zeron: "1".into(),
        };
        socket
            .send(Frame::text(serde_json::to_string(&hello).unwrap()))
            .await
            .unwrap();
        while !hub.status("m").online {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let caller = hub.clone();
        let call =
            tokio::spawn(async move { caller.call("m", "list_chats".into(), json!({})).await });
        let Some(Ok(Frame::Text(body))) = socket.next().await else {
            panic!("the Mac got no call");
        };
        assert_eq!(
            serde_json::from_str::<Call>(&body).unwrap().tool,
            "list_chats"
        );

        drop(socket);
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the call waited for its timeout")
            .unwrap();
        assert_eq!(result, Err("this Mac went offline".into()));
        assert!(!hub.status("m").online);
    }
}
