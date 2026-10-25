//! The engine's loopback RPC (`ws://127.0.0.1:<ipc port>`), as Zeron's own
//! client speaks it (`crates/rpc/src/lib.rs`): one JSON object per WebSocket
//! text message.
//!
//! - client → engine: `{id, method, params}` to call or subscribe, `{id,
//!   cancel: true}` to stop a stream;
//! - engine → client: `{id, ok}` or `{id, err}` for a call; `{id, item}`…
//!   then `{id, done: true}` (or `{id, err}`) for a stream.
//!
//! The engine rejects a handshake that carries an `Origin` header (a browser
//! dial); tungstenite sends none. It also wants `Authorization: Bearer
//! <token>`, where the token is what the engine wrote to
//! `{data_dir}/ipc-token` when it started (`crates/rpc/src/ipc_auth.rs`).

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// How long dialing may take. It's localhost, but another process holding
/// the port could accept TCP and never finish the handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Items a stream buffers before the reader waits for its consumer.
const STREAM_BUFFER: usize = 64;
/// The engine's reply to a method it doesn't have.
const UNKNOWN_METHOD: &str = "unknown method: ";

/// A lazily dialed, reconnecting connection to the engine.
pub struct Ipc {
    url: String,
    /// The engine's token file; read on every dial, as each engine start
    /// writes a new token.
    token: PathBuf,
    conn: Mutex<Option<Arc<Conn>>>,
}

/// Why a call or stream failed.
#[derive(Debug)]
enum CallError {
    /// The connection dropped; a new one may work.
    Closed,
    /// The engine answered with an error.
    Failed(String),
}

impl CallError {
    /// The error to report for a failed `method`.
    fn into_error(self, method: &str) -> anyhow::Error {
        match self {
            Self::Closed => anyhow!("{method}: the connection to Zeron closed"),
            Self::Failed(message) => EngineError {
                method: method.to_owned(),
                message,
            }
            .into(),
        }
    }
}

/// An error the engine answered a call or stream with.
#[derive(Debug)]
pub struct EngineError {
    method: String,
    message: String,
}

impl EngineError {
    /// Whether the engine doesn't have the method called.
    fn is_unknown_method(&self) -> bool {
        self.message.contains(UNKNOWN_METHOD)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.method, self.message)
    }
}

impl std::error::Error for EngineError {}

impl Ipc {
    /// Return a client for `url` that authenticates with the token in
    /// `token`; nothing connects until the first call.
    pub fn new(url: String, token: PathBuf) -> Self {
        Self {
            url,
            token,
            conn: Mutex::new(None),
        }
    }

    /// Call `method` and return its result, redialing once if the engine
    /// restarted since the last call.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let conn = self.conn().await?;
        let result = match conn.call(method, &params).await {
            Err(CallError::Closed) => self.conn().await?.call(method, &params).await,
            other => other,
        };
        result.map_err(|e| e.into_error(method))
    }

    /// Open a stream of `method`'s items. Dropping it cancels it in the engine.
    pub async fn subscribe(&self, method: &str, params: Value) -> Result<Subscription> {
        let conn = self.conn().await?;
        match conn.subscribe(method, &params) {
            Some(subscription) => Ok(subscription),
            None => self
                .conn()
                .await?
                .subscribe(method, &params)
                .ok_or_else(|| anyhow!("{method}: the connection to Zeron closed")),
        }
    }

    /// The open connection, dialing a new one if there is none or it closed.
    async fn conn(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref().filter(|c| !c.out.is_closed()) {
            return Ok(conn.clone());
        }
        let conn = Conn::dial(&self.url, &self.token).await?;
        *slot = Some(conn.clone());
        Ok(conn)
    }
}

/// Whether `error` says the engine doesn't have the method called.
pub fn is_unknown_method(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<EngineError>())
        .any(EngineError::is_unknown_method)
}

/// One WebSocket to the engine. A task owns the socket; it ends when every
/// sender of `out` is gone or the socket closes.
struct Conn {
    out: mpsc::UnboundedSender<String>,
    pending: Arc<StdMutex<Pending>>,
    next: AtomicU64,
}

/// Calls and streams waiting for frames, by id.
#[derive(Default)]
struct Pending {
    calls: HashMap<u64, oneshot::Sender<Result<Value, CallError>>>,
    streams: HashMap<u64, mpsc::Sender<Result<Value, CallError>>>,
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

impl Conn {
    async fn dial(url: &str, token: &Path) -> Result<Arc<Self>> {
        // The engine writes the file when it starts, so a missing one means
        // Zeron isn't running (or never ran with this data dir).
        let token = match std::fs::read_to_string(token) {
            Ok(token) => token.trim().to_owned(),
            Err(e) => bail!(
                "no Zeron engine at {url} (reading {}: {e}); is Zeron running?",
                token.display()
            ),
        };
        let mut request = url.into_client_request()?;
        request.headers_mut().insert(
            "authorization",
            HeaderValue::try_from(format!("Bearer {token}"))?,
        );
        let dial = tokio_tungstenite::connect_async(request);
        let (socket, _) = match tokio::time::timeout(CONNECT_TIMEOUT, dial).await {
            Ok(Ok(socket)) => socket,
            Ok(Err(e)) => bail!("no Zeron engine at {url} ({e}); is Zeron running?"),
            Err(_) => bail!("timed out dialing Zeron at {url}"),
        };
        let (out, out_rx) = mpsc::unbounded_channel::<String>();
        let pending = Arc::new(StdMutex::new(Pending::default()));
        tokio::spawn(pump(socket, out_rx, pending.clone()));
        Ok(Arc::new(Self {
            out,
            pending,
            next: AtomicU64::new(1),
        }))
    }

    async fn call(&self, method: &str, params: &Value) -> Result<Value, CallError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).calls.insert(id, tx);
        let frame = json!({ "id": id, "method": method, "params": params });
        if self.out.send(frame.to_string()).is_err() {
            lock(&self.pending).calls.remove(&id);
            return Err(CallError::Closed);
        }
        rx.await.unwrap_or(Err(CallError::Closed))
    }

    /// Start a stream; `None` if the connection is closed.
    fn subscribe(self: &Arc<Self>, method: &str, params: &Value) -> Option<Subscription> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        lock(&self.pending).streams.insert(id, tx);
        let frame = json!({ "id": id, "method": method, "params": params });
        if self.out.send(frame.to_string()).is_err() {
            lock(&self.pending).streams.remove(&id);
            return None;
        }
        Some(Subscription {
            id,
            method: method.to_owned(),
            rx,
            conn: self.clone(),
        })
    }
}

/// A stream from the engine. Dropping it cancels it there.
pub struct Subscription {
    id: u64,
    method: String,
    rx: mpsc::Receiver<Result<Value, CallError>>,
    conn: Arc<Conn>,
}

impl Subscription {
    /// Return the next item, or `None` once the stream ended.
    pub async fn next(&mut self) -> Result<Option<Value>> {
        self.rx
            .recv()
            .await
            .transpose()
            .map_err(|e| e.into_error(&self.method))
    }
}

/// Pass `out`'s frames to the engine and the engine's frames to `pending`
/// until either side closes, then fail every call and stream still waiting.
async fn pump(
    socket: Socket,
    mut out: mpsc::UnboundedReceiver<String>,
    pending: Arc<StdMutex<Pending>>,
) {
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            text = out.recv() => match text {
                Some(text) => {
                    if sink.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            },
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Some(cancel) = route(&pending, &text).await
                        && sink.send(Message::Text(cancel)).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    // Dropping the receiver marks `out` closed, so the next call redials.
    drop(out);
    let mut pending = lock(&pending);
    pending.calls.clear();
    for (_, stream) in pending.streams.drain() {
        let _ = stream.try_send(Err(CallError::Closed));
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Still listed means still running in the engine.
        if lock(&self.conn.pending).streams.remove(&self.id).is_some() {
            let _ = self
                .conn
                .out
                .send(json!({ "id": self.id, "cancel": true }).to_string());
        }
    }
}

/// An engine frame: exactly one of `ok`, `err`, `item`, or `done` matters.
#[derive(Deserialize)]
struct ServerFrame {
    id: u64,
    /// Present but `null` is a call whose result is `null`.
    #[serde(default, deserialize_with = "present")]
    ok: Option<Value>,
    #[serde(default)]
    err: Option<String>,
    #[serde(default, deserialize_with = "present")]
    item: Option<Value>,
    #[serde(default)]
    done: bool,
}

/// Deserialize a field that is there, even as `null`, as `Some`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// Hand one engine frame to its call or stream. Return a cancel frame to
/// send if it was an item for a stream nobody reads anymore.
async fn route(pending: &StdMutex<Pending>, text: &str) -> Option<String> {
    let Ok(frame) = serde_json::from_str::<ServerFrame>(text) else {
        return None;
    };
    let id = frame.id;
    if let Some(err) = frame.err {
        let mut pending = lock(pending);
        if let Some(call) = pending.calls.remove(&id) {
            let _ = call.send(Err(CallError::Failed(err)));
        } else if let Some(stream) = pending.streams.remove(&id) {
            let _ = stream.try_send(Err(CallError::Failed(err)));
        }
        return None;
    }
    if let Some(value) = frame.ok {
        if let Some(call) = lock(pending).calls.remove(&id) {
            let _ = call.send(Ok(value));
        }
        return None;
    }
    if let Some(item) = frame.item {
        // Send outside the lock: a full buffer waits for the consumer.
        let stream = lock(pending).streams.get(&id).cloned()?;
        if stream.send(Ok(item)).await.is_err() {
            lock(pending).streams.remove(&id);
            return Some(json!({ "id": id, "cancel": true }).to_string());
        }
        return None;
    }
    if frame.done {
        lock(pending).streams.remove(&id);
    }
    None
}

fn lock(pending: &StdMutex<Pending>) -> MutexGuard<'_, Pending> {
    pending.lock().expect("the IPC routing lock is poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;

    const TOKEN: &str = "the-token";

    /// Write `token` as an engine would and return its dir and file.
    fn token_file(token: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc-token");
        std::fs::write(&path, format!("{token}\n")).unwrap();
        (dir, path)
    }

    /// Serve one connection like the engine, refusing a handshake without
    /// `TOKEN` as its bearer: `Echo` answers its params, `Null` answers
    /// `null`, `Count` streams 1, 2 and ends, `Forever` streams until
    /// cancelled (then reports it with a `Cancelled` call), and anything else
    /// is an unknown method.
    #[expect(
        clippy::result_large_err,
        reason = "tungstenite's handshake callback fixes the error type"
    )]
    fn auth(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        let bearer = request.headers().get("authorization");
        if bearer.is_some_and(|b| b == format!("Bearer {TOKEN}").as_str()) {
            return Ok(response);
        }
        let mut refusal = ErrorResponse::new(None);
        *refusal.status_mut() = StatusCode::UNAUTHORIZED;
        Err(refusal)
    }

    async fn engine() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let Ok(socket) = tokio_tungstenite::accept_hdr_async(tcp, auth).await else {
                return;
            };
            let (mut sink, mut stream) = socket.split();
            let mut cancelled = Vec::new();
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                let frame: Value = serde_json::from_str(&text).unwrap();
                let id = frame["id"].clone();
                if frame["cancel"] == true {
                    cancelled.push(id);
                    continue;
                }
                let replies = match frame["method"].as_str().unwrap() {
                    "Echo" => vec![json!({ "id": id, "ok": frame["params"] })],
                    "Null" => vec![json!({ "id": id, "ok": null })],
                    "Count" => vec![
                        json!({ "id": id, "item": 1 }),
                        json!({ "id": id, "item": 2 }),
                        json!({ "id": id, "done": true }),
                    ],
                    "Forever" => vec![json!({ "id": id, "item": "first" })],
                    "Cancelled" => vec![json!({ "id": id, "ok": cancelled })],
                    other => vec![json!({ "id": id, "err": format!("{UNKNOWN_METHOD}{other}") })],
                };
                for reply in replies {
                    sink.send(Message::Text(reply.to_string())).await.unwrap();
                }
            }
        });
        url
    }

    #[tokio::test]
    async fn calls_streams_and_errors_follow_the_engine_frames() {
        let (_dir, token) = token_file(TOKEN);
        let ipc = Ipc::new(engine().await, token);
        assert_eq!(
            ipc.call("Echo", json!({ "a": 1 })).await.unwrap(),
            json!({ "a": 1 })
        );
        assert_eq!(ipc.call("Null", json!({})).await.unwrap(), Value::Null);
        let unknown = ipc.call("Nope", json!({})).await.unwrap_err();
        assert!(is_unknown_method(&unknown), "{unknown:#}");

        let mut count = ipc.subscribe("Count", json!({})).await.unwrap();
        assert_eq!(count.next().await.unwrap(), Some(json!(1)));
        assert_eq!(count.next().await.unwrap(), Some(json!(2)));
        assert_eq!(count.next().await.unwrap(), None);

        let mut forever = ipc.subscribe("Forever", json!({})).await.unwrap();
        assert_eq!(forever.next().await.unwrap(), Some(json!("first")));
        let id = forever.id;
        drop(forever);
        assert_eq!(ipc.call("Cancelled", json!({})).await.unwrap(), json!([id]));
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused() {
        let (_dir, token) = token_file("a-stale-token");
        let ipc = Ipc::new(engine().await, token);
        let error = ipc.call("Echo", json!({})).await.unwrap_err();
        assert!(format!("{error:#}").contains("401"), "{error:#}");
    }

    #[tokio::test]
    async fn nothing_listening_is_reported_as_zeron_not_running() {
        let (dir, token) = token_file(TOKEN);
        let listening = Ipc::new("ws://127.0.0.1:9".into(), token);
        let no_token = Ipc::new(engine().await, dir.path().join("missing"));
        for ipc in [listening, no_token] {
            let error = ipc.call("Echo", json!({})).await.unwrap_err();
            assert!(
                format!("{error:#}").contains("is Zeron running"),
                "{error:#}"
            );
        }
    }
}
