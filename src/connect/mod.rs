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
//! reports chats that finish, need an answer, or fail. [`power`] reports the
//! battery.

mod chat;
mod composer;
mod dictation;
mod engine;
mod maintain;
mod power;
mod tools;
mod watch;

pub use engine::spawn as watch_engine;
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
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use zeron_relay_proto::{Battery, CONNECT_PATH, Call, Engine, Notice, PROTOCOL, Up};

use self::tools::{Tool, Unavailable, parse_args};
use crate::json_file;
use crate::pi::Pi;
use crate::service::RunMode;
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
/// Tool calls running or waiting on one connection; more are refused.
const MAX_QUEUED_CALLS: usize = 64;
/// Notices buffered while the relay is unreachable.
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
    /// Read the saved link from `path`, if zeron-tools was installed with one.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        json_file::load(path)
    }

    /// Save the link to `path`, readable only by the user (the token is a
    /// secret).
    pub fn save(&self, path: &Path) -> Result<()> {
        json_file::save(path, self)
    }
}

/// What `connect` runs with.
pub struct Opts {
    /// The relay link; without one, only the upkeep runs.
    pub relay: Option<Link>,
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
    let pi = opts.upkeep.pi.clone();
    let upkeep = tokio::spawn(maintain::run(zeron.clone(), engine.clone(), opts.upkeep));
    if let Some(link) = opts.relay {
        let relay = Relay {
            link,
            notify: opts.notify,
            mode: opts.mode,
            pi,
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
    notify: NotifyWhen,
    mode: RunMode,
    /// The title model, which also cleans up dictation.
    pi: Option<Pi>,
}

impl Relay {
    /// Stay connected to the relay until it rejects the token. Returns `Ok`
    /// then if running as the launch agent.
    async fn run(self, zeron: Arc<Zeron>, mut engine: state::Receiver<Engine>) -> Result<()> {
        let url = socket_url(&self.link.relay)?;
        let mac = Arc::new(Mac {
            zeron: zeron.clone(),
            pi: self.pi,
            slots: Semaphore::new(MAX_CALLS),
        });
        // Notices wait here while the relay is unreachable.
        let (notices_tx, mut notices) = mpsc::channel(QUEUE);
        tokio::spawn(watch::run(zeron, engine.clone(), notices_tx, self.notify));
        let mut battery = power::spawn();
        let mut backoff = MIN_BACKOFF;
        loop {
            let mut connected = None;
            let result: Result<()> = async {
                let socket = dial(&url, &self.link.token).await?;
                eprintln!("connected to {url}");
                connected = Some(Instant::now());
                serve(socket, &mac, &mut engine, &mut battery, &mut notices).await
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
}

/// Serve one connection to the relay until it ends. `Ok` means the relay
/// closed it.
async fn serve(
    socket: Socket,
    mac: &Arc<Mac>,
    engine: &mut state::Receiver<Engine>,
    battery: &mut state::Receiver<Option<Battery>>,
    notices: &mut mpsc::Receiver<Notice>,
) -> Result<()> {
    let (mut sink, mut stream) = socket.split();
    sink.send(encode(&hello())?).await?;
    let now = engine.borrow_and_update().clone();
    sink.send(encode(&Up::Engine(now))?).await?;
    let now = *battery.borrow_and_update();
    sink.send(encode(&Up::Power { battery: now })?).await?;
    // This connection's calls; dropping the set when it ends stops them, as
    // their replies have nowhere to go.
    let mut calls: JoinSet<Up> = JoinSet::new();
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            frame = stream.next() => {
                heard = Instant::now();
                match frame {
                    Some(Ok(Message::Text(body))) => match serde_json::from_str::<Call>(&body) {
                        Ok(call) if calls.len() >= MAX_QUEUED_CALLS => {
                            let busy = Up::Reply { id: call.id, result: Err("this Mac is busy; try again".into()) };
                            sink.send(encode(&busy)?).await?;
                        }
                        Ok(call) => {
                            calls.spawn(run_call(mac.clone(), engine.clone(), call));
                        }
                        // The relay times out the call it held.
                        Err(e) => eprintln!("skipped a bad call from the relay: {e}"),
                    },
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
            Some(Ok(reply)) = calls.join_next() => sink.send(encode(&reply)?).await?,
            Some(notice) = notices.recv() => sink.send(encode(&Up::Notify(notice))?).await?,
            Ok(()) = engine.changed() => {
                let now = engine.borrow_and_update().clone();
                sink.send(encode(&Up::Engine(now))?).await?;
            }
            Ok(()) = battery.changed() => {
                let now = *battery.borrow_and_update();
                sink.send(encode(&Up::Power { battery: now })?).await?;
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

/// Run `call` within [`MAX_CALLS`] and return its reply. The wait for a slot
/// counts toward [`TOOL_TIMEOUT`], so the reply still reaches the relay in
/// time, and the call sees Zeron's state when it starts.
async fn run_call(mac: Arc<Mac>, engine: state::Receiver<Engine>, call: Call) -> Up {
    let Call { id, tool, args } = call;
    let run = async {
        let _slot = mac
            .slots
            .acquire()
            .await
            .map_err(|_| "connect is stopping")?;
        let now = engine.borrow().clone();
        call_tool(&mac.zeron, mac.pi.as_ref(), &now, &tool, args).await
    };
    let result = tokio::time::timeout(TOOL_TIMEOUT, run)
        .await
        .unwrap_or_else(|_| Err("Zeron didn't answer in time".into()));
    Up::Reply { id, result }
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
    sink.send(encode(&hello())?).await?;
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
    pi: Option<Pi>,
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
    /// The composer's `$` menu ([`composer::skills`]).
    ListSkills,
    /// Clean up what dictation heard ([`dictation::polish`]).
    PolishDictation,
    /// One of Zeron's own tools ([`tools`]).
    Tool(Tool),
}

impl Request {
    /// Whether it needs a Zeron that is open and readable.
    fn needs_zeron(self) -> bool {
        !matches!(
            self,
            Self::OpenZeron | Self::RestartZeron | Self::PolishDictation
        )
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
            "list_skills" => Self::ListSkills,
            "polish_dictation" => Self::PolishDictation,
            other => Self::Tool(other.parse()?),
        })
    }
}

/// Run one of the phone's tools by name and return its result, or why it
/// can't run in Zeron's `engine` state, within [`TOOL_TIMEOUT`]; `pi` is the
/// model for the calls that need one. Clients on this Mac (the companion
/// window) call it directly; the relay's calls come through here too.
pub async fn call_tool(
    zeron: &Zeron,
    pi: Option<&Pi>,
    engine: &Engine,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let request: Request = tool.parse().map_err(|e: Unavailable| e.to_string())?;
    if request.needs_zeron()
        && let Some(why) = engine::refusal(engine)
    {
        return Err(why);
    }
    match tokio::time::timeout(TOOL_TIMEOUT, dispatch(zeron, pi, request, args)).await {
        Ok(result) => result.map_err(|e| format!("{e:#}")),
        Err(_) => Err("Zeron didn't answer in time".into()),
    }
}

/// Run `request` with its JSON `args`.
async fn dispatch(zeron: &Zeron, pi: Option<&Pi>, request: Request, args: Value) -> Result<Value> {
    match request {
        Request::OpenZeron => engine::open().await.map(|()| opening()),
        Request::RestartZeron => engine::restart().await.map(|()| opening()),
        Request::ChatView => chat::view(zeron, parse_args(args)?).await,
        Request::ComposerInfo => composer::info(zeron, parse_args(args)?).await,
        Request::SetChatModel => composer::set_model(zeron, parse_args(args)?).await,
        Request::ListSkills => composer::skills(zeron, parse_args(args)?).await,
        Request::PolishDictation => dictation::polish(pi, parse_args(args)?).await,
        Request::Tool(tool) => tools::call(zeron, tool, args).await,
    }
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

/// One message for the relay, as a JSON text frame.
fn encode(up: &Up) -> Result<Message> {
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
    async fn dictation_is_cleaned_up_by_the_model_even_while_zeron_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        // A stand-in for pi that tidies whatever it reads.
        let binary = dir.path().join("pi");
        std::fs::write(&binary, "#!/bin/sh\necho \"  Tidy: $(cat)  \"\n").unwrap();
        std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let pi = Pi {
            binary,
            model: "m".into(),
            thinking: "low".into(),
        };
        let zeron = Zeron::new(9, Path::new("/nonexistent"));
        let args = || json!({ "text": "um hello there" });
        let polished = call_tool(
            &zeron,
            Some(&pi),
            &Engine::Closed,
            "polish_dictation",
            args(),
        )
        .await
        .unwrap();
        assert_eq!(polished, json!({ "text": "Tidy: um hello there" }));
        let without = call_tool(&zeron, None, &Engine::Closed, "polish_dictation", args())
            .await
            .unwrap_err();
        assert!(without.contains("--no-rename"), "{without}");
    }

    #[tokio::test]
    async fn calls_wait_for_a_readable_zeron() {
        // Nothing listens here: a call that got through would fail differently.
        let zeron = Zeron::new(9, Path::new("/nonexistent"));
        let call = |engine: Engine, tool: &'static str| {
            let zeron = &zeron;
            async move { call_tool(zeron, None, &engine, tool, json!({})).await }
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

    #[tokio::test]
    async fn the_link_skips_bad_calls_and_answers_the_rest() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let relay = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            socket.send(Message::Text("nonsense".into())).await.unwrap();
            let call = json!({ "id": 7, "tool": "list_chats", "args": {} });
            socket.send(Message::Text(call.to_string())).await.unwrap();
            while let Some(Ok(frame)) = socket.next().await {
                if let Message::Text(body) = frame
                    && let Ok(Up::Reply { id, result }) = serde_json::from_str(&body)
                {
                    return (id, result);
                }
            }
            panic!("no reply");
        });
        let mac = Arc::new(Mac {
            zeron: Arc::new(Zeron::new(9, Path::new("/nonexistent"))),
            pi: None,
            slots: Semaphore::new(MAX_CALLS),
        });
        let (_engine_tx, mut engine) = state::channel(Engine::Closed);
        let (_battery_tx, mut battery) = state::channel(None);
        let (_notices_tx, mut notices) = mpsc::channel(1);
        let socket = dial(&url, "t").await.unwrap();
        tokio::spawn(
            async move { serve(socket, &mac, &mut engine, &mut battery, &mut notices).await },
        );
        let (id, result) = relay.await.unwrap();
        assert_eq!(
            (id, result),
            (7, Err("Zeron isn't open on this Mac".into()))
        );
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
    fn links_round_trip_without_logging_the_token() {
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
    }
}
