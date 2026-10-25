//! The running Zeron, through its engine's loopback IPC.
//!
//! Reads are the engine's watch streams: a snapshot is "subscribe, take the
//! first item, drop" (dropping cancels it), as Zeron's own MCP client does
//! (`crates/mcp/src/zeron.rs`). Writes go through `Mutate` and the chat's
//! command queue, so Zeron applies, saves, and syncs them itself.

pub mod ipc;
pub mod transcript;
pub mod types;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use self::ipc::{Ipc, Subscription};
use self::transcript::Entry;
use self::types::{Chat, Device, Harness, Model, Session, SidebarPreferences, Space};

/// The engine methods used here (`crates/rpc/src/lib.rs` `methods`).
pub mod methods {
    pub const ENGINE_READY: &str = "EngineReady";
    pub const LOCAL_DEVICE: &str = "LocalDevice";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_SPACES: &str = "WatchSpaces";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    pub const WATCH_SIDEBAR_PREFERENCES: &str = "WatchSidebarPreferences";
    pub const WATCH_DOC_MESSAGES: &str = "WatchDocMessages";
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    pub const LIST_MODELS: &str = "ListModels";
    pub const LIST_REFS: &str = "ListRefs";
    pub const MUTATE: &str = "Mutate";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
}

/// How long a snapshot or transcript may take. The engine answers in
/// milliseconds unless it's still starting.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

/// The engine's IPC port when `ZERON_IPC_PORT` isn't set.
pub const DEFAULT_IPC_PORT: u16 = 27654;

/// Zeron's data dir: `$ZERON_DATA_DIR`, else `~/.zeron`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("ZERON_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::home().join(".zeron"))
}

/// A client for the running Zeron's engine.
pub struct Zeron {
    ipc: Ipc,
}

impl Zeron {
    /// Return a client for the engine on `port` that uses `data_dir`; it
    /// dials on first use.
    pub fn new(port: u16, data_dir: &Path) -> Self {
        Self {
            ipc: Ipc::new(format!("ws://127.0.0.1:{port}"), data_dir.join("ipc-token")),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.ipc.call(method, params).await
    }

    pub async fn subscribe(&self, method: &str, params: Value) -> Result<Subscription> {
        self.ipc.subscribe(method, params).await
    }

    /// The first item of `method`'s stream.
    pub async fn snapshot(&self, method: &str, params: Value) -> Result<Value> {
        let mut stream = self.subscribe(method, params).await?;
        match tokio::time::timeout(SNAPSHOT_TIMEOUT, stream.next()).await {
            Ok(Ok(Some(item))) => Ok(item),
            Ok(Ok(None)) => bail!("{method}: the stream ended before its first snapshot"),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!(
                "{method}: no snapshot within {}s",
                SNAPSHOT_TIMEOUT.as_secs()
            ),
        }
    }

    async fn snapshot_as<T: DeserializeOwned>(&self, method: &str) -> Result<T> {
        let value = self.snapshot(method, json!({})).await?;
        parse(method, value)
    }

    pub async fn chats(&self) -> Result<Vec<Chat>> {
        self.snapshot_as(methods::WATCH_CHATS).await
    }

    pub async fn spaces(&self) -> Result<Vec<Space>> {
        self.snapshot_as(methods::WATCH_SPACES).await
    }

    pub async fn sessions(&self) -> Result<Vec<Session>> {
        self.snapshot_as(methods::WATCH_SESSIONS).await
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        self.snapshot_as(methods::WATCH_DEVICES).await
    }

    pub async fn sidebar(&self) -> Result<SidebarPreferences> {
        self.snapshot_as(methods::WATCH_SIDEBAR_PREFERENCES).await
    }

    /// This engine's device id.
    pub async fn local_device_id(&self) -> Result<String> {
        let reply = self.call(methods::LOCAL_DEVICE, json!({})).await?;
        reply
            .get("deviceId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("LocalDevice: no deviceId in the reply")
    }

    /// This device's row: its Zeron version and capabilities.
    pub async fn local_device(&self) -> Result<Device> {
        let (id, devices) = tokio::try_join!(self.local_device_id(), self.devices())?;
        devices
            .into_iter()
            .find(|d| d.id == id)
            .with_context(|| format!("WatchDevices: no row for this device ({id})"))
    }

    pub async fn harnesses(&self) -> Result<Vec<Harness>> {
        let value = self.call(methods::LIST_HARNESSES, json!({})).await?;
        parse(methods::LIST_HARNESSES, value)
    }

    pub async fn models(&self, harness: &str) -> Result<Vec<Model>> {
        let value = self
            .call(methods::LIST_MODELS, json!({ "harness": harness }))
            .await?;
        parse(methods::LIST_MODELS, value)
    }

    /// The chat's whole transcript, from its host device.
    pub async fn transcript(&self, chat: &Chat) -> Result<Vec<Entry>> {
        let params = json!({ "chatId": chat.id, "targetDeviceId": chat.device_id });
        let mut stream = self.subscribe(methods::WATCH_DOC_MESSAGES, params).await?;
        let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let frame = match tokio::time::timeout(left, stream.next()).await {
                Ok(frame) => frame?,
                Err(_) => bail!(
                    "WatchDocMessages: no transcript within {}s",
                    SNAPSHOT_TIMEOUT.as_secs()
                ),
            };
            let Some(frame) = frame else {
                bail!("WatchDocMessages: the stream ended before the transcript");
            };
            if let Some(entries) = transcript::from_frame(&frame) {
                return entries.context("WatchDocMessages: unexpected transcript");
            }
        }
    }

    /// Change a workspace entity (`{op: "renameChat", …}`).
    pub async fn mutate(&self, params: Value) -> Result<Value> {
        self.call(methods::MUTATE, params).await
    }

    /// Queue a command (`{kind: "run", …}`) on the chat; its host runs it.
    /// Returns the command's id.
    pub async fn queue_command(&self, chat_id: &str, command: Value) -> Result<String> {
        let reply = self
            .call(
                methods::QUEUE_COMMAND,
                json!({ "chatId": chat_id, "command": command }),
            )
            .await?;
        Ok(text_field(&reply, "commandId"))
    }

    /// The chat with this id or unique id prefix.
    pub async fn chat(&self, key: &str) -> Result<Chat> {
        let chats = self.chats().await?;
        find_chat(&chats, key).cloned()
    }
}

/// Parse a reply of `method` into `T`.
fn parse<T: DeserializeOwned>(method: &str, value: Value) -> Result<T> {
    serde_json::from_value(value).with_context(|| format!("{method}: unexpected reply"))
}

fn text_field(reply: &Value, name: &str) -> String {
    reply
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The one chat whose id is `key` or starts with it.
pub fn find_chat<'a>(chats: &'a [Chat], key: &str) -> Result<&'a Chat> {
    let key = key.trim();
    if key.is_empty() {
        bail!("chat is required");
    }
    if let Some(chat) = chats.iter().find(|c| c.id == key) {
        return Ok(chat);
    }
    let hits: Vec<_> = chats.iter().filter(|c| c.id.starts_with(key)).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => bail!("no chat matches {key}"),
        _ => bail!("{key} matches {} chats; use a longer prefix", hits.len()),
    }
}

/// The session that speaks for `chat`: its host's, else the freshest one (a
/// chat moved to another device can briefly have two).
pub fn session_for<'a>(sessions: &'a [Session], chat: &Chat) -> Option<&'a Session> {
    let mine = sessions.iter().filter(|s| s.chat_id == chat.id);
    mine.clone()
        .find(|s| s.device_id == chat.device_id)
        .or_else(|| mine.max_by_key(|s| s.updated_at))
}
