//! `connect`: the background service for this Mac's Zeron.
//!
//! It keeps the sidebar short ([`maintain`]) and, once linked to a
//! `zeron-relay`, serves the phone's tool calls over one outgoing WebSocket.
//! Calls are checked here, not on the relay: the raw IPC also opens terminals
//! and writes files, and a compromised relay must not reach those.

mod changes;
mod chat;
mod composer;
mod dictation;
mod engine;
mod maintain;
mod power;
mod tools;
mod turns;
mod watch;

pub use engine::{EngineWatch, spawn as watch_engine};
pub use maintain::Upkeep;
pub use tools::ChatStatus;
pub use turns::Turns;
pub use watch::{NotifyWhen, run as watch_chats};

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
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::{Bytes, Message};
use zeron_relay_proto::{Battery, CONNECT_PATH, Call, Notice, PROTOCOL, Up};

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
    pub fn load(path: &Path) -> Result<Option<Self>> {
        json_file::load(path)
    }

    /// Save the link readable only by the user, as the token is a secret.
    pub fn save(&self, path: &Path) -> Result<()> {
        json_file::save(path, self)
    }
}

pub struct Opts {
    /// The relay link; without one, only the upkeep runs.
    pub relay: Option<Link>,
    pub zeron: Zeron,
    pub turns: Turns,
    pub notify: NotifyWhen,
    pub upkeep: Upkeep,
    pub mode: RunMode,
}

/// Run until the process is stopped, or the relay rejects the token of a
/// manual run.
pub async fn run(opts: Opts) -> Result<()> {
    let zeron = Arc::new(opts.zeron);
    let turns = Arc::new(opts.turns);
    let engine = engine::spawn(zeron.clone());
    let pi = opts.upkeep.pi.clone();
    let upkeep = tokio::spawn(maintain::run(zeron.clone(), engine.changes(), opts.upkeep));
    let (notices_tx, notices) = mpsc::channel(QUEUE);
    let notify = opts.relay.is_some().then_some(watch::Notices {
        to: notices_tx,
        when: opts.notify,
    });
    tokio::spawn(watch::run(
        zeron.clone(),
        engine.changes(),
        turns.clone(),
        notify,
    ));
    if let Some(link) = opts.relay {
        let relay = Relay {
            link,
            mode: opts.mode,
            pi,
        };
        relay.run(zeron, turns, engine, notices).await?;
    }
    // A failing exit would make launchd restart it forever, so the upkeep
    // goes on without the relay.
    upkeep.await?;
    Ok(())
}

struct Relay {
    link: Link,
    mode: RunMode,
    /// The title model, which also cleans up dictation.
    pi: Option<Pi>,
}

impl Relay {
    /// Stay connected to the relay until it rejects the token, which is `Ok`
    /// for the launch agent.
    async fn run(
        self,
        zeron: Arc<Zeron>,
        turns: Arc<Turns>,
        engine: EngineWatch,
        mut notices: mpsc::Receiver<Notice>,
    ) -> Result<()> {
        let url = socket_url(&self.link.relay)?;
        let mac = Arc::new(Mac {
            zeron,
            turns,
            pi: self.pi,
            slots: Semaphore::new(MAX_CALLS),
        });
        let mut battery = power::spawn();
        let mut backoff = MIN_BACKOFF;
        loop {
            let mut connected = None;
            let result: Result<()> = async {
                let socket = dial(&url, &self.link.token).await?;
                eprintln!("connected to {url}");
                connected = Some(Instant::now());
                serve(socket, &mac, &engine, &mut battery, &mut notices).await
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
    engine: &EngineWatch,
    battery: &mut state::Receiver<Option<Battery>>,
    notices: &mut mpsc::Receiver<Notice>,
) -> Result<()> {
    let (mut sink, mut stream) = socket.split();
    sink.send(encode(&hello())?).await?;
    let mut changes = engine.changes();
    let now = changes.borrow_and_update().clone();
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
            Ok(()) = changes.changed() => {
                let now = changes.borrow_and_update().clone();
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
                sink.send(Message::Ping(Bytes::new())).await?;
            }
        }
    }
}

/// Run `call` within [`MAX_CALLS`] and return its reply. The wait for a slot
/// counts toward [`TOOL_TIMEOUT`], so the reply still reaches the relay in
/// time, and the call sees Zeron's state when it starts.
async fn run_call(mac: Arc<Mac>, engine: EngineWatch, call: Call) -> Up {
    let Call { id, tool, args } = call;
    let run = async {
        let _slot = mac
            .slots
            .acquire()
            .await
            .map_err(|_| "connect is stopping")?;
        call_tool(
            &mac.zeron,
            &mac.turns,
            mac.pi.as_ref(),
            &engine,
            &tool,
            args,
        )
        .await
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

struct Mac {
    zeron: Arc<Zeron>,
    turns: Arc<Turns>,
    pi: Option<Pi>,
    /// Bounds the calls running at once to [`MAX_CALLS`].
    slots: Semaphore,
}

/// A call the phone may make, by its tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    OpenZeron,
    ChatView,
    TurnChanges,
    ComposerInfo,
    SetChatModel,
    ListSkills,
    PolishDictation,
    /// One of Zeron's own tools.
    Tool(Tool),
}

impl Request {
    fn needs_zeron(self) -> bool {
        !matches!(self, Self::OpenZeron | Self::PolishDictation)
    }
}

impl FromStr for Request {
    type Err = Unavailable;

    fn from_str(name: &str) -> Result<Self, Unavailable> {
        Ok(match name {
            "open_zeron" => Self::OpenZeron,
            "chat_view" => Self::ChatView,
            "turn_changes" => Self::TurnChanges,
            "composer_info" => Self::ComposerInfo,
            "set_chat_model" => Self::SetChatModel,
            "list_skills" => Self::ListSkills,
            "polish_dictation" => Self::PolishDictation,
            other => Self::Tool(other.parse()?),
        })
    }
}

/// Run one of the phone's tools by name within [`TOOL_TIMEOUT`], or say why
/// it can't run in the current `engine` state. The companion window calls it
/// directly.
pub async fn call_tool(
    zeron: &Zeron,
    turns: &Turns,
    pi: Option<&Pi>,
    engine: &EngineWatch,
    tool: &str,
    args: Value,
) -> Result<Value, String> {
    let request: Request = tool.parse().map_err(|e: Unavailable| e.to_string())?;
    if request.needs_zeron()
        && let Some(why) = engine::refusal(&engine.now())
    {
        return Err(why);
    }
    let run = dispatch(zeron, turns, pi, engine, request, args);
    match tokio::time::timeout(TOOL_TIMEOUT, run).await {
        Ok(result) => result.map_err(|e| format!("{e:#}")),
        Err(_) => Err("Zeron didn't answer in time".into()),
    }
}

async fn dispatch(
    zeron: &Zeron,
    turns: &Turns,
    pi: Option<&Pi>,
    engine: &EngineWatch,
    request: Request,
    args: Value,
) -> Result<Value> {
    match request {
        Request::OpenZeron => engine.open().await.map(|()| opening()),
        Request::ChatView => chat::view(zeron, turns, parse_args(args)?).await,
        Request::TurnChanges => changes::turn(zeron, turns, parse_args(args)?).await,
        Request::ComposerInfo => composer::info(zeron, parse_args(args)?).await,
        Request::SetChatModel => composer::set_model(zeron, parse_args(args)?).await,
        Request::ListSkills => composer::skills(zeron, parse_args(args)?).await,
        Request::PolishDictation => dictation::polish(pi, parse_args(args)?).await,
        Request::Tool(tool) => tools::call(zeron, tool, args).await,
    }
}

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

fn rejected(error: &anyhow::Error) -> bool {
    use tokio_tungstenite::tungstenite::Error;
    matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Http(response)) if response.status() == StatusCode::UNAUTHORIZED
    )
}

fn encode(up: &Up) -> Result<Message> {
    Ok(Message::Text(serde_json::to_string(up)?.into()))
}

pub fn link_path(data_dir: &Path) -> PathBuf {
    data_dir.join("zeron-tools-connect.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_relay_proto::Engine;

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
        let turns = Turns::new(dir.path());
        let args = || json!({ "text": "um hello there" });
        let polished = call_tool(
            &zeron,
            &turns,
            Some(&pi),
            &EngineWatch::fixed(Engine::Closed),
            "polish_dictation",
            args(),
        )
        .await
        .unwrap();
        assert_eq!(polished, json!({ "text": "Tidy: um hello there" }));
        let without = call_tool(
            &zeron,
            &turns,
            None,
            &EngineWatch::fixed(Engine::Closed),
            "polish_dictation",
            args(),
        )
        .await
        .unwrap_err();
        assert!(without.contains("--no-rename"), "{without}");
    }

    #[tokio::test]
    async fn calls_wait_for_a_readable_zeron() {
        // Nothing listens here: a call that got through would fail differently.
        let zeron = Zeron::new(9, Path::new("/nonexistent"));
        let turns = Turns::new(Path::new("/nonexistent"));
        let call = |engine: Engine, tool: &'static str| {
            let (zeron, turns) = (&zeron, &turns);
            async move {
                let engine = EngineWatch::fixed(engine);
                call_tool(zeron, turns, None, &engine, tool, json!({})).await
            }
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
            socket
                .send(Message::Text(call.to_string().into()))
                .await
                .unwrap();
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
            turns: Arc::new(Turns::new(Path::new("/nonexistent"))),
            pi: None,
            slots: Semaphore::new(MAX_CALLS),
        });
        let engine = EngineWatch::fixed(Engine::Closed);
        let (_battery_tx, mut battery) = state::channel(None);
        let (_notices_tx, mut notices) = mpsc::channel(1);
        let socket = dial(&url, "t").await.unwrap();
        tokio::spawn(async move { serve(socket, &mac, &engine, &mut battery, &mut notices).await });
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
    fn logs_never_show_the_token() {
        let link = Link {
            relay: "https://r".into(),
            token: "secret".into(),
        };
        assert!(!format!("{link:?}").contains("secret"), "{link:?}");
    }
}
