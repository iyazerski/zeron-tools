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
//! those. [`watch`] reports chats that finish, need an answer, or fail.

mod watch;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use zeron_mcp::{Origin, Tools, Zeron};
use zeron_relay_proto::{CONNECT_PATH, Call, Notice, PROTOCOL, Up};

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
}

/// Stay connected to the relay until the process is stopped or the relay
/// rejects the token.
pub async fn run(opts: Opts) -> Result<()> {
    let url = socket_url(&opts.link.relay)?;
    // No origin chat: messages go out as the user's own, not attributed to a
    // chat (as they would be if this ran inside a Zeron terminal).
    let zeron = Arc::new(Zeron::new(
        format!("ws://127.0.0.1:{}", opts.ipc_port),
        Origin::default(),
    ));
    let tools = Arc::new(Tools::new(zeron.clone()));
    // Notices wait here while the relay is unreachable.
    let (notices_tx, mut notices) = mpsc::channel(64);
    tokio::spawn(watch::run(
        zeron,
        tools.clone(),
        notices_tx,
        opts.notify_always,
    ));
    let mut backoff = Duration::from_secs(1);
    let mut save_to = opts.save_to;
    loop {
        let accepted = || -> Result<()> {
            if let Some(path) = save_to.take() {
                opts.link.save(&path)?;
                eprintln!(
                    "saved the link to {}; next time run just zeron-tools connect",
                    path.display()
                );
            }
            Ok(())
        };
        match session(&url, &opts.link.token, &tools, &mut notices, accepted).await {
            Ok(()) => {
                eprintln!("the relay closed the connection; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) if rejected(&e) => {
                bail!("the relay rejected this Mac's token; add the Mac again in the app")
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
    tools: &Arc<Tools>,
    notices: &mut mpsc::Receiver<Notice>,
    accepted: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).context("bad token")?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request).await?;
    eprintln!("connected to {url}");
    accepted()?;
    let (mut sink, mut stream) = socket.split();
    let hello = Up::Hello {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        zeron: crate::zeron::ZERON_VERSION.into(),
    };
    sink.send(text(&hello)?).await?;
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
                        let (tools, replies) = (tools.clone(), replies_tx.clone());
                        tokio::spawn(async move {
                            let reply = Up::Reply { id: call.id, result: run_tool(&tools, call.tool, call.args).await };
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
            _ = ping.tick() => {
                if heard.elapsed() > SILENCE_LIMIT {
                    bail!("no answer from the relay for {}s", SILENCE_LIMIT.as_secs());
                }
                sink.send(Message::Ping(vec![])).await?;
            }
        }
    }
}

/// Run one allowed tool and return its result, or why it can't run.
async fn run_tool(tools: &Tools, tool: String, args: Value) -> Result<Value, String> {
    match allowed_args(&tool, args) {
        Some(args) => tools.call(&tool, args).await,
        None => Err(format!("tool {tool} is not available from the phone")),
    }
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
