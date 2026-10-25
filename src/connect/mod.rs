//! `connect`: the background service for this Mac's Zeron.
//!
//! While Zeron runs, it keeps the sidebar short ([`maintain`]: archive old
//! chats, title untitled ones) and, once linked to a `zeron-relay`, lets a
//! phone use Zeron from anywhere. Everything goes through the engine's
//! loopback IPC ([`crate::zeron`]); nothing links Zeron's code.
//!
//! The Mac dials out to the relay over one WebSocket (no open ports, no VPN)
//! and keeps it open, reconnecting when it drops. The relay sends tool
//! calls; they run as [`tools`] (and the chat screen's [`chat`] and
//! [`composer`]), checked here and not on the relay: the raw IPC also opens
//! terminals and writes files, and a compromised relay must not reach those.
//! [`engine`] tracks whether Zeron is open (and opens it), and [`watch`]
//! reports chats that finish, need an answer, or fail.

mod agent;
mod chat;
mod composer;
mod engine;
mod maintain;
mod tools;
mod watch;

pub use agent::{install, uninstall};
pub use maintain::Upkeep;
pub use watch::NotifyWhen;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Semaphore, mpsc, watch as state};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use zeron_relay_proto::{CONNECT_PATH, Call, Engine, Notice, PROTOCOL, Up};

use self::tools::{Tool, Unavailable};
use crate::zeron::Zeron;

/// How often to ping the relay. Proxies drop idle sockets, and a reply
/// (or any frame) proves the link still works.
const PING_EVERY: Duration = Duration::from_secs(25);
/// Reconnect if nothing came from the relay for this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(70);
/// The first wait before reconnecting; it doubles after each quick failure.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
/// The longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that lasted this long worked, so the next reconnect starts
/// fast again. A shorter one (say, a relay refusing this protocol right
/// after hello) keeps backing off.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// How long a tool call may run here: under the relay's 30 s, so the error
/// still reaches the phone. A call that timed out may still have happened.
const TOOL_TIMEOUT: Duration = Duration::from_secs(25);
/// Tool calls running at once; more wait. Bounds the work a hung Zeron
/// piles up.
const MAX_CALLS: usize = 16;
/// Notices and replies buffered for the relay connection.
const QUEUE: usize = 64;

/// Where `connect` keeps the relay address and this Mac's token.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub relay: String,
    pub token: String,
}

// By hand, so logs never show the token.
impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link")
            .field("relay", &self.relay)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Link {
    /// Read the saved link from `path`, if `connect` was set up before.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Save the link to `path` atomically, readable only by the user (the
    /// token is a secret).
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        // A fresh file, so the mode applies: a leftover one keeps its own.
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", tmp.display()));
            }
            _ => {}
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// The relay link to use, and where to save it if it is new.
#[derive(Debug, Clone)]
pub struct RelayLink {
    pub link: Link,
    /// Where to save `link` once the relay accepts it; `None` when it was
    /// loaded from there.
    pub save_to: Option<PathBuf>,
}

/// How this `connect` was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// From a terminal: a running `connect` or a rejected token is an error.
    Manual,
    /// By the launch agent: it waits for a running `connect` to stop, and a
    /// rejected token ends the relay link for good, with a clean exit.
    Agent,
}

/// Make sure this is the only `connect` for `data_dir`: two would keep
/// replacing each other on the relay. The launch agent waits its turn (a
/// manual run may be holding it); a manual run fails at once. Hold the
/// returned file while connected.
pub async fn single_instance(data_dir: &Path, mode: RunMode) -> Result<std::fs::File> {
    let path = data_dir.join("zeron-tools-connect.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match (file.try_lock(), mode) {
        (Ok(()), _) => return Ok(file),
        (Err(std::fs::TryLockError::Error(e)), _) => {
            return Err(e).with_context(|| format!("locking {}", path.display()));
        }
        (Err(std::fs::TryLockError::WouldBlock), RunMode::Manual) => bail!(
            "connect already runs on this Mac (maybe in the background; stop it with zeron-tools connect --uninstall)"
        ),
        (Err(std::fs::TryLockError::WouldBlock), RunMode::Agent) => {}
    }
    eprintln!("another connect runs on this Mac; waiting for it to stop");
    tokio::task::spawn_blocking(move || file.lock().map(|()| file))
        .await?
        .with_context(|| format!("locking {}", path.display()))
}

/// What `connect` runs with.
pub struct Opts {
    /// The relay link; without one, only the upkeep runs.
    pub relay: Option<RelayLink>,
    pub zeron: Zeron,
    pub notify: NotifyWhen,
    pub upkeep: Upkeep,
    pub mode: RunMode,
}

/// Run until the process is stopped, or the relay rejects the token of a
/// manual run.
pub async fn run(opts: Opts) -> Result<()> {
    let zeron = Arc::new(opts.zeron);
    let engine = engine::spawn(zeron.clone());
    let upkeep = tokio::spawn(maintain::run(zeron.clone(), engine.clone(), opts.upkeep));
    if let Some(RelayLink { link, save_to }) = opts.relay {
        let relay = Relay {
            link,
            save_to,
            notify: opts.notify,
            mode: opts.mode,
        };
        relay.run(zeron, engine).await?;
    }
    // A failing exit would make launchd restart it forever, so the upkeep
    // goes on without the relay.
    upkeep.await?;
    Ok(())
}

/// The link to the relay, kept up by [`Relay::run`].
struct Relay {
    link: Link,
    /// Where to save `link` once the relay first accepts it.
    save_to: Option<PathBuf>,
    notify: NotifyWhen,
    mode: RunMode,
}

impl Relay {
    /// Stay connected to the relay until it rejects the token. Returns `Ok`
    /// then if running as the launch agent.
    async fn run(mut self, zeron: Arc<Zeron>, mut engine: state::Receiver<Engine>) -> Result<()> {
        let url = socket_url(&self.link.relay)?;
        let mac = Arc::new(Mac {
            zeron: zeron.clone(),
            slots: Semaphore::new(MAX_CALLS),
        });
        // Notices wait here while the relay is unreachable.
        let (notices_tx, mut notices) = mpsc::channel(QUEUE);
        tokio::spawn(watch::run(zeron, engine.clone(), notices_tx, self.notify));
        let mut backoff = MIN_BACKOFF;
        loop {
            let mut connected = None;
            let result: Result<()> = async {
                let socket = dial(&url, &self.link.token).await?;
                eprintln!("connected to {url}");
                connected = Some(Instant::now());
                self.save_accepted_link()?;
                serve(socket, &mac, &mut engine, &mut notices).await
            }
            .await;
            if connected.is_some_and(|at| at.elapsed() >= STABLE_AFTER) {
                backoff = MIN_BACKOFF;
            }
            match result {
                Ok(()) => eprintln!("the relay closed the connection; reconnecting"),
                Err(e) if rejected(&e) => {
                    let why = "the relay rejected this Mac's token; add the Mac again in the app";
                    match self.mode {
                        RunMode::Agent => {
                            eprintln!("{why}");
                            return Ok(());
                        }
                        RunMode::Manual => bail!(why),
                    }
                }
                Err(e) => eprintln!("relay: {e:#}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Save a new link, once: the relay just accepted it.
    fn save_accepted_link(&mut self) -> Result<()> {
        if let Some(path) = self.save_to.take() {
            self.link.save(&path)?;
            eprintln!(
                "saved the link to {}; next time run just zeron-tools connect",
                path.display()
            );
        }
        Ok(())
    }
}

/// Serve one connection to the relay until it ends. `Ok` means the relay
/// closed it.
async fn serve(
    socket: Socket,
    mac: &Arc<Mac>,
    engine: &mut state::Receiver<Engine>,
    notices: &mut mpsc::Receiver<Notice>,
) -> Result<()> {
    let (mut sink, mut stream) = socket.split();
    sink.send(text(&hello())?).await?;
    let now = engine.borrow_and_update().clone();
    sink.send(text(&Up::Engine(now))?).await?;
    // Replies from tool calls running in their own tasks.
    let (replies_tx, mut replies) = mpsc::channel::<Up>(QUEUE);
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            frame = stream.next() => {
                heard = Instant::now();
                match frame {
                    Some(Ok(Message::Text(body))) => {
                        let call: Call = serde_json::from_str(&body).context("bad call from the relay")?;
                        let now = engine.borrow().clone();
                        spawn_call(mac.clone(), now, call, replies_tx.clone());
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
            Some(reply) = replies.recv() => sink.send(text(&reply)?).await?,
            Some(notice) = notices.recv() => sink.send(text(&Up::Notify(notice))?).await?,
            Ok(()) = engine.changed() => {
                let now = engine.borrow_and_update().clone();
                sink.send(text(&Up::Engine(now))?).await?;
            }
            _ = ping.tick() => {
                if heard.elapsed() > SILENCE_LIMIT {
                    bail!("no answer from the relay for {}s", SILENCE_LIMIT.as_secs());
                }
                sink.send(Message::Ping(vec![])).await?;
            }
        }
    }
}

/// Run `call` in its own task, within [`MAX_CALLS`] and [`TOOL_TIMEOUT`],
/// and send its reply to `replies`.
fn spawn_call(mac: Arc<Mac>, engine: Engine, call: Call, replies: mpsc::Sender<Up>) {
    tokio::spawn(async move {
        let result = match mac.slots.acquire().await {
            Ok(_slot) => {
                let run = run_tool(&mac, &engine, &call.tool, call.args);
                tokio::time::timeout(TOOL_TIMEOUT, run)
                    .await
                    .unwrap_or_else(|_| Err("Zeron didn't answer in time".into()))
            }
            Err(_) => Err("connect is stopping".into()),
        };
        // The connection may be gone by now; the relay times the call out.
        let _ = replies
            .send(Up::Reply {
                id: call.id,
                result,
            })
            .await;
    });
}

/// Check that the relay accepts `link`'s token, then disconnect.
pub async fn verify(link: &Link) -> Result<()> {
    let socket = match dial(&socket_url(&link.relay)?, &link.token).await {
        Err(e) if rejected(&e) => {
            bail!("the relay rejected this token; add the Mac again in the app")
        }
        other => other.with_context(|| format!("reaching {}", link.relay))?,
    };
    // Say hello, so the relay logs a short visit rather than a bad client.
    let (mut sink, _) = socket.split();
    sink.send(text(&hello())?).await?;
    sink.close().await?;
    Ok(())
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the WebSocket to the relay with this Mac's token.
async fn dial(url: &str, token: &str) -> Result<Socket> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).context("bad token")?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(socket)
}

fn hello() -> Up {
    Up::Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// This Mac's Zeron, as the relay's calls reach it.
struct Mac {
    zeron: Arc<Zeron>,
    /// Limits the tool calls running at once ([`MAX_CALLS`]).
    slots: Semaphore,
}

/// A call the phone may make, by its tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    /// Start Zeron; works while it's closed.
    OpenZeron,
    /// Quit and start Zeron; works while it's closed or hangs.
    RestartZeron,
    /// The chat screen ([`chat::view`]).
    ChatView,
    /// The composer footer ([`composer::info`]).
    ComposerInfo,
    /// The composer's model picker ([`composer::set_model`]).
    SetChatModel,
    /// One of Zeron's own tools ([`tools`]).
    Tool(Tool),
}

impl Request {
    /// Whether it needs a Zeron that is open and readable.
    fn needs_zeron(self) -> bool {
        !matches!(self, Self::OpenZeron | Self::RestartZeron)
    }
}

impl FromStr for Request {
    type Err = Unavailable;

    fn from_str(name: &str) -> Result<Self, Unavailable> {
        Ok(match name {
            "open_zeron" => Self::OpenZeron,
            "restart_zeron" => Self::RestartZeron,
            "chat_view" => Self::ChatView,
            "composer_info" => Self::ComposerInfo,
            "set_chat_model" => Self::SetChatModel,
            other => Self::Tool(other.parse()?),
        })
    }
}

/// Run one allowed tool and return its result, or why it can't run.
async fn run_tool(mac: &Mac, engine: &Engine, tool: &str, args: Value) -> Result<Value, String> {
    let request: Request = tool.parse().map_err(|e: Unavailable| e.to_string())?;
    if request.needs_zeron()
        && let Some(why) = engine::refusal(engine)
    {
        return Err(why);
    }
    let zeron = &mac.zeron;
    let result = match request {
        Request::OpenZeron => engine::open().await.map(|()| opening()),
        Request::RestartZeron => engine::restart().await.map(|()| opening()),
        Request::ChatView => chat::view(zeron, &args).await,
        Request::ComposerInfo => composer::info(zeron, &args).await,
        Request::SetChatModel => composer::set_model(zeron, &args).await,
        Request::Tool(tool) => tools::call(zeron, tool, args).await,
    };
    result.map_err(|e| format!("{e:#}"))
}

/// The answer to a call that starts Zeron.
fn opening() -> Value {
    json!({ "opening": true })
}

/// The relay's WebSocket URL for an `https://` (or local `http://`) address.
fn socket_url(relay: &str) -> Result<String> {
    let base = relay.trim_end_matches('/');
    let rest = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("the relay address must start with https:// (got {relay})")
    };
    Ok(format!("{rest}{CONNECT_PATH}"))
}

/// Whether connecting failed because the relay refused the token.
fn rejected(error: &anyhow::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Http(response)) if response.status() == StatusCode::UNAUTHORIZED
    )
}

fn text(up: &Up) -> Result<Message> {
    Ok(Message::Text(serde_json::to_string(up)?))
}

/// Where the link is saved: `<data dir>/zeron-tools-connect.json`.
pub fn link_path(data_dir: &Path) -> PathBuf {
    data_dir.join("zeron-tools-connect.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn calls_wait_for_a_readable_zeron() {
        // Nothing listens here: a call that got through would fail differently.
        let mac = Mac {
            zeron: Arc::new(Zeron::new(9, Path::new("/nonexistent"))),
            slots: Semaphore::new(MAX_CALLS),
        };
        let call = |engine: Engine, tool: &'static str| {
            let mac = &mac;
            async move { run_tool(mac, &engine, tool, json!({})).await }
        };
        let closed = call(Engine::Closed, "list_chats").await.unwrap_err();
        assert_eq!(closed, "Zeron isn't open on this Mac");
        let closed = call(Engine::Closed, "set_chat_model").await.unwrap_err();
        assert_eq!(closed, "Zeron isn't open on this Mac");
        let outdated = Engine::Outdated {
            running: "0.0.1".into(),
        };
        let outdated = call(outdated, "list_chats").await.unwrap_err();
        assert!(outdated.contains("update zeron-tools"), "{outdated}");
        let refused = call(Engine::Running { running: None }, "open_terminal")
            .await
            .unwrap_err();
        assert!(refused.contains("not available"), "{refused}");
    }

    #[test]
    fn relay_addresses_become_socket_urls() {
        assert_eq!(
            socket_url("https://zeron.example.com/").unwrap(),
            "wss://zeron.example.com/connect"
        );
        assert_eq!(
            socket_url("http://localhost:8790").unwrap(),
            "ws://localhost:8790/connect"
        );
        assert!(socket_url("zeron.example.com").is_err());
    }

    #[test]
    fn links_round_trip_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = link_path(dir.path());
        assert_eq!(Link::load(&path).unwrap(), None);
        let link = Link {
            relay: "https://r".into(),
            token: "t".into(),
        };
        link.save(&path).unwrap();
        assert_eq!(Link::load(&path).unwrap(), Some(link.clone()));
        assert!(!format!("{link:?}").contains("\"t\""), "{link:?}");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
