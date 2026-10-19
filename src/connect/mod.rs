//! `connect`: link this Mac's running Zeron to a `zeron-relay`, so a phone
//! can use it from anywhere.
//!
//! The Mac dials out to the relay over one WebSocket (no open ports, no VPN)
//! and keeps it open, reconnecting when it drops. The relay sends tool calls;
//! they run against the engine's loopback IPC (`ws://127.0.0.1:<ipc port>`)
//! through `zeron_mcp::Tools`, the same layer behind Zeron's own
//! `list_chats`/`send_message`/... MCP tools. Only the tools the phone app
//! needs are allowed, checked here and not on the relay: the raw IPC also
//! opens terminals and writes files, and a compromised relay must not reach
//! those. [`engine`] tracks whether Zeron is open (and opens it), and
//! [`watch`] reports chats that finish, need an answer, or fail.

mod agent;
mod composer;
mod engine;
mod watch;

pub use agent::{install, uninstall};
pub use engine::VersionCheck;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch as state};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use zeron_mcp::{Origin, Tools, Zeron};
use zeron_relay_proto::{CONNECT_PATH, Call, Engine, Notice, PROTOCOL, Up};

/// How often to ping the relay. Proxies drop idle sockets, and a reply
/// (or any frame) proves the link still works.
const PING_EVERY: Duration = Duration::from_secs(25);
/// Reconnect if nothing came from the relay for this long.
const SILENCE_LIMIT: Duration = Duration::from_secs(70);
/// The longest wait between reconnect attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Where `connect` keeps the relay address and this Mac's token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub relay: String,
    pub token: String,
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

    /// Save the link to `path`, readable only by the user (the token is a secret).
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

pub struct Opts {
    pub link: Link,
    /// Where to save `link` once the relay accepts it, if it is new.
    pub save_to: Option<PathBuf>,
    pub ipc_port: u16,
    /// Notify even while the Mac is in use.
    pub notify_always: bool,
    /// Reads Zeron's version; `None` skips the check (`--ignore-version`).
    pub version: Option<VersionCheck>,
    /// Running as the launch agent: a rejected token ends it for good.
    pub agent: bool,
}

/// Stay connected to the relay until the process is stopped or the relay
/// rejects the token.
pub async fn run(opts: Opts) -> Result<()> {
    let link = opts.link.clone();
    let url = socket_url(&link.relay)?;
    // No origin chat: messages go out as the user's own, not attributed to a
    // chat (as they would be if this ran inside a Zeron terminal).
    let zeron = Arc::new(Zeron::new(
        format!("ws://127.0.0.1:{}", opts.ipc_port),
        Origin::default(),
    ));
    let mac = Arc::new(Mac {
        zeron: zeron.clone(),
        tools: Arc::new(Tools::new(zeron.clone())),
    });
    let mut engine = engine::spawn(zeron.clone(), opts.version);
    // Notices wait here while the relay is unreachable.
    let (notices_tx, mut notices) = mpsc::channel(64);
    tokio::spawn(watch::run(
        zeron,
        mac.tools.clone(),
        engine.clone(),
        notices_tx,
        opts.notify_always,
    ));
    let mut backoff = Duration::from_secs(1);
    let mut save_to = opts.save_to;
    loop {
        let accepted = || -> Result<()> {
            if let Some(path) = save_to.take() {
                link.save(&path)?;
                eprintln!(
                    "saved the link to {}; next time run just zeron-tools connect",
                    path.display()
                );
            }
            Ok(())
        };
        let result = session(&url, &link.token, &mac, &mut engine, &mut notices, accepted).await;
        match result {
            Ok(()) => {
                eprintln!("the relay closed the connection; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) if rejected(&e) => {
                let why = "the relay rejected this Mac's token; add the Mac again in the app";
                // A failing exit would make launchd restart it forever.
                if opts.agent {
                    eprintln!("{why}");
                    return Ok(());
                }
                bail!(why)
            }
            Err(e) => eprintln!("relay: {e:#}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Serve one connection to the relay until it ends. `Ok` means it was
/// connected and then closed; an error may come from connecting.
async fn session(
    url: &str,
    token: &str,
    mac: &Arc<Mac>,
    engine: &mut state::Receiver<Engine>,
    notices: &mut mpsc::Receiver<Notice>,
    accepted: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let socket = dial(url, token).await?;
    eprintln!("connected to {url}");
    accepted()?;
    let (mut sink, mut stream) = socket.split();
    sink.send(text(&hello())?).await?;
    let now = engine.borrow_and_update().clone();
    sink.send(text(&Up::Engine(now))?).await?;
    // Replies from tool calls running in their own tasks.
    let (replies_tx, mut replies) = mpsc::channel::<Up>(64);
    let mut ping = tokio::time::interval(PING_EVERY);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            frame = stream.next() => {
                heard = Instant::now();
                match frame {
                    Some(Ok(Message::Text(body))) => {
                        let call: Call = serde_json::from_str(&body).context("bad call from the relay")?;
                        let (mac, replies) = (mac.clone(), replies_tx.clone());
                        let now = engine.borrow().clone();
                        tokio::spawn(async move {
                            let reply = Up::Reply { id: call.id, result: run_tool(&mac, &now, call.tool, call.args).await };
                            // The connection may be gone by now; the relay times the call out.
                            let _ = replies.send(reply).await;
                        });
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
        zeron: crate::zeron::ZERON_VERSION.into(),
    }
}

/// This Mac's Zeron, as the relay's calls reach it.
struct Mac {
    zeron: Arc<Zeron>,
    tools: Arc<Tools>,
}

/// Run one allowed tool and return its result, or why it can't run.
/// `open_zeron` and `restart_zeron` are ours, not Zeron tools: they work
/// while Zeron is closed or hangs. `composer_info` and `set_chat_model`
/// are ours too, for the chat's composer ([`composer`]).
async fn run_tool(mac: &Mac, engine: &Engine, tool: String, args: Value) -> Result<Value, String> {
    let started = match tool.as_str() {
        "open_zeron" => Some(engine::open().await),
        "restart_zeron" => Some(engine::restart().await),
        _ => None,
    };
    if let Some(started) = started {
        return started
            .map(|()| json!({ "opening": true }))
            .map_err(|e| format!("{e:#}"));
    }
    let ours = matches!(tool.as_str(), "composer_info" | "set_chat_model");
    let args = if ours {
        args
    } else {
        allowed_args(&tool, args)
            .ok_or_else(|| format!("tool {tool} is not available from the phone"))?
    };
    if let Some(why) = engine::refusal(engine) {
        return Err(why);
    }
    let result = match tool.as_str() {
        "composer_info" => composer::info(&mac.zeron, &args).await,
        "set_chat_model" => composer::set_model(&mac.zeron, &args).await,
        _ => return mac.tools.call(&tool, args).await,
    };
    result.map_err(|e| format!("{e:#}"))
}

/// Return the arguments to pass to `tool`, or `None` if the phone may not call it.
///
/// Calls never block on a turn (`wait`): the app polls instead, and a
/// request held open for minutes would die on a flaky mobile connection.
/// New chats are always standalone sessions.
fn allowed_args(tool: &str, mut args: Value) -> Option<Value> {
    let object = args.as_object_mut()?;
    match tool {
        "list_chats" | "get_chat" | "read_chat" | "list_projects" | "list_harnesses"
        | "list_models" | "interrupt_chat" | "respond_to_input" | "archive_chat" => {}
        "send_message" => {
            object.insert("wait".into(), json!(false));
        }
        "create_chat" => {
            object.insert("wait".into(), json!(false));
            object.insert("kind".into(), json!("chat"));
            object.remove("parent");
        }
        _ => return None,
    }
    Some(args)
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

    #[test]
    fn phone_calls_never_wait_and_create_standalone_chats() {
        let args = allowed_args(
            "send_message",
            json!({ "chat": "c", "text": "hi", "wait": true }),
        );
        assert_eq!(
            args,
            Some(json!({ "chat": "c", "text": "hi", "wait": false }))
        );
        let args = allowed_args("create_chat", json!({ "parent": "p", "prompt": "hi" }));
        assert_eq!(
            args,
            Some(json!({ "prompt": "hi", "wait": false, "kind": "chat" }))
        );
    }

    #[test]
    fn tools_outside_the_app_are_refused() {
        for tool in [
            "show_visual",
            "send_messages",
            "create_chats",
            "wait_for_turn",
            "nope",
        ] {
            assert_eq!(allowed_args(tool, json!({})), None, "{tool}");
        }
        assert_eq!(allowed_args("read_chat", json!("not an object")), None);
    }

    #[tokio::test]
    async fn calls_wait_for_a_matching_zeron() {
        // Nothing listens here: a call that got through would fail differently.
        let zeron = Arc::new(Zeron::new("ws://127.0.0.1:9".into(), Origin::default()));
        let mac = Mac {
            zeron: zeron.clone(),
            tools: Arc::new(Tools::new(zeron)),
        };
        let call = |engine: Engine, tool: &str| {
            let (mac, tool) = (&mac, tool.to_owned());
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
        let refused = call(Engine::Running, "open_terminal").await.unwrap_err();
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
        assert_eq!(Link::load(&path).unwrap(), Some(link));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
