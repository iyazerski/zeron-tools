//! The Zeron tools the phone app calls, run against the engine's IPC.
//!
//! They mirror Zeron's own MCP tools of the same names
//! (`crates/mcp/src/tools.rs`) and answer in the same shapes, cut to what
//! the app uses: no waiting on turns (the app polls), no other devices, and
//! new chats are always standalone. The raw IPC also opens terminals and
//! writes files; only the [`Tool`]s are reachable from the phone.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::zeron::transcript::{self, RenderOptions};
use crate::zeron::types::{Chat, ChatConfig, Harness, Session, SessionStatus, Space};
use crate::zeron::{Zeron, session_for};

/// A working session not updated for this long may have lost its host
/// (`SESSION_STALE` in Zeron's MCP).
const STALE_AFTER: chrono::Duration = chrono::Duration::seconds(45);
/// New chats run with this sandbox, as Zeron's `create_chat` does.
const DEFAULT_SANDBOX: &str = "workspace-write";
/// Devices with this capability give a project-less chat its own folder
/// (`zeron_proto::capabilities::PROJECTLESS_SCRATCH_V1`).
const PROJECTLESS_SCRATCH: &str = "projectless-scratch-v1";
/// Where those folders go (`zeron_proto::projectless::SCRATCH_ROOT`).
const SCRATCH_ROOT: &str = "~/.zeron/scratch";
/// Chats `list_chats` returns without a `limit`, and at most.
const DEFAULT_CHATS: usize = 50;
const MAX_CHATS: usize = 500;

/// A Zeron tool the phone may call through [`call`], by its MCP name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    ListChats,
    ListProjects,
    ListHarnesses,
    ListModels,
    SendMessage,
    CreateChat,
    InterruptChat,
    RespondToInput,
    ArchiveChat,
}

impl FromStr for Tool {
    type Err = Unavailable;

    fn from_str(name: &str) -> Result<Self, Unavailable> {
        Ok(match name {
            "list_chats" => Self::ListChats,
            "list_projects" => Self::ListProjects,
            "list_harnesses" => Self::ListHarnesses,
            "list_models" => Self::ListModels,
            "send_message" => Self::SendMessage,
            "create_chat" => Self::CreateChat,
            "interrupt_chat" => Self::InterruptChat,
            "respond_to_input" => Self::RespondToInput,
            "archive_chat" => Self::ArchiveChat,
            _ => return Err(Unavailable(name.to_owned())),
        })
    }
}

/// A tool name the phone may not call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable(pub String);

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool {} is not available from the phone", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// Run `tool` with its JSON `args`.
pub async fn call(zeron: &Zeron, tool: Tool, args: Value) -> Result<Value> {
    match tool {
        Tool::ListChats => list_chats(zeron, parse(args)?).await,
        Tool::ListProjects => list_projects(zeron).await,
        Tool::ListHarnesses => list_harnesses(zeron).await,
        Tool::ListModels => list_models(zeron, parse(args)?).await,
        Tool::SendMessage => send_message(zeron, parse(args)?).await,
        Tool::CreateChat => create_chat(zeron, parse(args)?).await,
        Tool::InterruptChat => interrupt_chat(zeron, parse(args)?).await,
        Tool::RespondToInput => respond_to_input(zeron, parse(args)?).await,
        Tool::ArchiveChat => archive_chat(zeron, parse(args)?).await,
    }
}

fn parse<T: DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).context("invalid arguments")
}

#[derive(Deserialize)]
struct ChatArgs {
    chat: String,
}

#[derive(Deserialize)]
struct ListChatsArgs {
    limit: Option<usize>,
}

/// The chats the sidebar lists (unarchived and top-level, so not side chats),
/// most recently active first.
async fn list_chats(zeron: &Zeron, args: ListChatsArgs) -> Result<Value> {
    let (mut chats, spaces, sessions) =
        tokio::try_join!(zeron.chats(), zeron.spaces(), zeron.sessions())?;
    chats.retain(|c| !c.archived && c.is_top_level());
    chats.sort_by_key(|c| std::cmp::Reverse(c.last_activity()));
    let limit = args.limit.unwrap_or(DEFAULT_CHATS).clamp(1, MAX_CHATS);
    Ok(json!({
        "total": chats.len(),
        "chats": chats.iter().take(limit).map(|c| summary(c, &spaces, &sessions)).collect::<Vec<_>>(),
    }))
}

async fn list_projects(zeron: &Zeron) -> Result<Value> {
    let spaces = zeron.spaces().await?;
    Ok(json!({
        "projects": spaces.iter().map(|s| json!({
            "id": s.id,
            "name": s.display_name(),
            "path": s.path,
            "deviceId": s.device_id,
            "git": s.git_detected,
        })).collect::<Vec<_>>(),
    }))
}

async fn list_harnesses(zeron: &Zeron) -> Result<Value> {
    let harnesses = zeron.harnesses().await?;
    Ok(json!({
        "harnesses": harnesses.iter().map(|h| json!({
            "id": h.id,
            "name": h.name,
            "available": h.available(),
            "installed": h.installed,
            "steersMidTurn": h.steers_mid_turn(),
            "reasoningLevels": h.reasoning_levels,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct ListModelsArgs {
    harness: String,
}

async fn list_models(zeron: &Zeron, args: ListModelsArgs) -> Result<Value> {
    let models = zeron.models(&args.harness).await?;
    Ok(json!({
        "harness": args.harness,
        "models": models.iter().map(|m| json!({
            "id": m.id,
            "label": m.label,
            "description": m.description,
            "reasoningLevels": m.reasoning_levels,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct SendArgs {
    chat: String,
    text: String,
}

/// Send `text` to a chat: start a turn, or steer the running one.
async fn send_message(zeron: &Zeron, args: SendArgs) -> Result<Value> {
    let text = args.text.trim();
    if text.is_empty() {
        bail!("text is empty");
    }
    let chat = zeron.chat(&args.chat).await?;
    let (spaces, sessions, harnesses) =
        tokio::try_join!(zeron.spaces(), zeron.sessions(), zeron.harnesses())?;
    let space = space_of(&chat, &spaces);
    let session = session_for(&sessions, &chat);
    let sent = deliver(zeron, &chat, space, &harnesses, session, text).await?;
    Ok(json!({ "chatId": chat.id, "title": chat.title, "sent": sent }))
}

#[derive(Deserialize)]
struct CreateChatArgs {
    project: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    reasoning: Option<String>,
    branch: Option<String>,
    prompt: Option<String>,
}

/// Create a standalone chat on this Mac, in a project or not, and send it
/// the first prompt.
async fn create_chat(zeron: &Zeron, args: CreateChatArgs) -> Result<Value> {
    let space = match args.project.as_deref() {
        Some(key) => Some(find_space(&zeron.spaces().await?, key)?.clone()),
        None => None,
    };
    let (device, harnesses) = tokio::try_join!(zeron.local_device(), zeron.harnesses())?;
    let harness = pick_harness(args.harness, &harnesses)?;
    if let Some(model) = args.model.as_deref() {
        let models = zeron.models(&harness).await?;
        if !models.iter().any(|m| m.id == model) {
            bail!("{model} isn't one of {harness}'s models");
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = Utc::now();
    // A project-less chat runs in its own scratch folder where the host makes
    // them; naming it here keeps creating and the first run in agreement.
    let scratch = space.is_none() && device.capabilities.iter().any(|c| c == PROJECTLESS_SCRATCH);
    let cwd = scratch.then(|| scratch_cwd(&id, created_at));
    let chat = Chat {
        id,
        device_id: space.as_ref().map_or(device.id, |s| s.device_id.clone()),
        title: None,
        archived: false,
        cwd,
        branch: args.branch.filter(|b| !b.trim().is_empty()),
        config: Some(ChatConfig {
            harness,
            model: args.model,
            reasoning: args.reasoning,
            model_options: Map::new(),
            rest: Map::from_iter([("sandbox".to_owned(), json!(DEFAULT_SANDBOX))]),
        }),
        last_message_preview: None,
        last_message_at: None,
        created_at,
        harness_session_cwd: None,
        space_id: space.as_ref().map(|s| s.id.clone()),
        parent_chat_id: None,
    };
    zeron.mutate(create_op(&chat)).await?;
    let mut result = json!({
        "chatId": chat.id,
        "kind": "chat",
        "deviceId": chat.device_id,
        "project": space.as_ref().map(project),
        "harness": chat.harness(),
        "model": chat.model(),
        "reasoning": chat.reasoning(),
    });
    if let Some(prompt) = args
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        // The new row may not be in WatchChats yet: use what was just written.
        result["sent"] = deliver(zeron, &chat, space.as_ref(), &harnesses, None, prompt).await?;
    }
    Ok(result)
}

/// The `createChat` mutation that writes `chat`.
fn create_op(chat: &Chat) -> Value {
    let mut op = json!({
        "op": "createChat",
        "chatId": chat.id,
        "deviceId": chat.device_id,
        "config": chat.config,
    });
    let optional = [
        ("spaceId", &chat.space_id),
        ("branch", &chat.branch),
        ("cwd", &chat.cwd),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            op[key] = json!(value);
        }
    }
    op
}

async fn interrupt_chat(zeron: &Zeron, args: ChatArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    let command_id = zeron
        .queue_command(&chat.id, json!({ "kind": "interrupt" }))
        .await?;
    Ok(json!({ "chatId": chat.id, "commandId": command_id }))
}

#[derive(Deserialize)]
struct RespondArgs {
    chat: String,
    request_id: Option<String>,
    #[serde(default)]
    answers: Vec<Answer>,
}

#[derive(Deserialize)]
struct Answer {
    question_id: String,
    labels: Vec<String>,
}

/// Answer the question a chat waits on (the newest one, without `request_id`).
async fn respond_to_input(zeron: &Zeron, args: RespondArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    if args.answers.is_empty() {
        bail!("answers is empty");
    }
    let request_id = match args.request_id {
        Some(id) => id,
        None => {
            let entries = zeron.transcript(&chat).await?;
            let rendered = transcript::render(&entries, RenderOptions::default());
            transcript::pending_input(&rendered)
                .and_then(|p| p["requestId"].as_str().map(str::to_owned))
                .context("this chat has no pending question")?
        }
    };
    let answers: Vec<Value> = args
        .answers
        .iter()
        .map(|a| json!({ "questionId": a.question_id, "labels": a.labels }))
        .collect();
    let command = json!({ "kind": "respondInput", "requestId": request_id, "answers": answers });
    let command_id = zeron.queue_command(&chat.id, command).await?;
    Ok(json!({ "chatId": chat.id, "requestId": request_id, "commandId": command_id }))
}

#[derive(Deserialize)]
struct ArchiveArgs {
    chat: String,
    archived: Option<bool>,
}

async fn archive_chat(zeron: &Zeron, args: ArchiveArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    let archived = args.archived.unwrap_or(true);
    zeron
        .mutate(json!({ "op": "setChatArchived", "chatId": chat.id, "archived": archived }))
        .await?;
    Ok(json!({ "chatId": chat.id, "title": chat.title, "archived": archived }))
}

/// Start a turn with `text`, or steer the running one, as the desktop
/// composer would. A chat waiting on a question must be answered first.
async fn deliver(
    zeron: &Zeron,
    chat: &Chat,
    space: Option<&Space>,
    harnesses: &[Harness],
    session: Option<&Session>,
    text: &str,
) -> Result<Value> {
    let status = status(session, Utc::now());
    let (delivery, command) = match session.map(|s| s.status) {
        // A quiet tool call can outlive the stale window: let the host decide.
        Some(SessionStatus::Working) => {
            let id = uuid::Uuid::new_v4().to_string();
            let command = json!({ "kind": "steer", "prompt": text, "messageId": id });
            ("steer", command)
        }
        Some(SessionStatus::AwaitingInput) => {
            bail!("this chat is waiting for an answer; answer its question first")
        }
        _ => {
            let request = run_request(chat, space, harnesses, text)?;
            let id = uuid::Uuid::new_v4().to_string();
            let command = json!({ "kind": "run", "request": request, "messageId": id });
            ("run", command)
        }
    };
    let id = zeron.queue_command(&chat.id, command).await?;
    Ok(json!({ "delivery": delivery, "id": id, "chatStatusAtSend": status }))
}

/// The request of a new turn that sends `text` with the chat's settings.
fn run_request(
    chat: &Chat,
    space: Option<&Space>,
    harnesses: &[Harness],
    text: &str,
) -> Result<Value> {
    let harness = match chat.harness() {
        Some(h) => h.to_owned(),
        None => default_harness(harnesses)?,
    };
    let config = chat.config.as_ref();
    let cwd = chat
        .cwd
        .clone()
        .or_else(|| space.map(|s| s.path.clone()))
        .unwrap_or_else(|| "~".into());
    let sandbox = config
        .and_then(|c| c.rest.get("sandbox").cloned())
        .unwrap_or_else(|| json!(DEFAULT_SANDBOX));
    Ok(json!({
        "prompt": text,
        "harness": harness,
        "model": chat.model(),
        "reasoning": chat.reasoning(),
        "modelOptions": config.map(|c| c.model_options.clone()).unwrap_or_default(),
        "cwd": cwd,
        "sandbox": sandbox,
        "autoApprove": false,
        "resume": null,
    }))
}

/// The harness a new chat asked for, if it's available, else the default one.
fn pick_harness(requested: Option<String>, harnesses: &[Harness]) -> Result<String> {
    let Some(id) = requested else {
        return default_harness(harnesses);
    };
    if !harnesses.iter().any(|h| h.id == id && h.available()) {
        bail!("harness {id} is not available on this Mac");
    }
    Ok(id)
}

/// Claude Code when it's offered, else the first available harness.
fn default_harness(harnesses: &[Harness]) -> Result<String> {
    harnesses
        .iter()
        .find(|h| h.id == "claude-code" && h.available())
        .or_else(|| harnesses.iter().find(|h| h.available() && h.id != "mock"))
        .map(|h| h.id.clone())
        .context("no agent is available on this Mac")
}

/// The scratch folder of a new project-less chat: `<date>-<the id's last 8
/// letters and digits>` (`zeron_proto::projectless::scratch_cwd`).
fn scratch_cwd(chat_id: &str, created_at: DateTime<Utc>) -> String {
    let id: Vec<char> = chat_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let id: String = id[id.len().saturating_sub(8)..].iter().collect();
    let date = created_at.format("%Y-%m-%d");
    if id.is_empty() {
        format!("{SCRATCH_ROOT}/{date}")
    } else {
        format!("{SCRATCH_ROOT}/{date}-{id}")
    }
}

/// The project with this id, folder, or name.
fn find_space<'a>(spaces: &'a [Space], key: &str) -> Result<&'a Space> {
    let path = key.trim_end_matches('/');
    spaces
        .iter()
        .find(|s| s.id == key)
        .or_else(|| spaces.iter().find(|s| s.path.trim_end_matches('/') == path))
        .or_else(|| {
            spaces
                .iter()
                .find(|s| s.display_name().eq_ignore_ascii_case(key))
        })
        .with_context(|| format!("no project matches {key}"))
}

pub(super) fn space_of<'a>(chat: &Chat, spaces: &'a [Space]) -> Option<&'a Space> {
    let id = chat.space_id.as_deref()?;
    spaces.iter().find(|s| s.id == id)
}

fn project(space: &Space) -> Value {
    json!({ "id": space.id, "name": space.display_name(), "path": space.path })
}

/// A chat as `list_chats` and `get_chat` describe it.
pub(super) fn summary(chat: &Chat, spaces: &[Space], sessions: &[Session]) -> Value {
    let space = space_of(chat, spaces);
    let session = session_for(sessions, chat);
    json!({
        "id": chat.id,
        "title": chat.title,
        "project": space.map(project),
        "deviceId": chat.device_id,
        "cwd": chat.cwd.clone().or_else(|| space.map(|s| s.path.clone())),
        "branch": chat.branch,
        "harness": chat.harness(),
        "model": chat.model(),
        "reasoning": chat.reasoning(),
        "archived": chat.archived,
        "parentChatId": chat.parent_chat_id,
        "status": status(session, Utc::now()),
        "statusAgeSecs": session.map(|s| (Utc::now() - s.updated_at).num_seconds().max(0)),
        "lastMessageAt": chat.last_message_at,
        "lastMessagePreview": chat.last_message_preview,
        "createdAt": chat.created_at,
    })
}

/// The chat's status label, worded as Zeron's MCP `status_of`.
pub(super) fn status(session: Option<&Session>, now: DateTime<Utc>) -> String {
    let Some(session) = session else {
        return "idle".into();
    };
    match session.status {
        SessionStatus::Working if now - session.updated_at > STALE_AFTER => {
            "working (stale — host may be offline)"
        }
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaitingInput",
        SessionStatus::Errored => "errored",
        SessionStatus::Idle => "idle",
        SessionStatus::Other => "unknown",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(chat: &str, device: &str, status: &str, updated: &str) -> Session {
        serde_json::from_value(json!({
            "chatId": chat, "deviceId": device, "status": status, "updatedAt": updated,
        }))
        .unwrap()
    }

    fn chat(id: &str, host: &str) -> Chat {
        serde_json::from_value(
            json!({ "id": id, "deviceId": host, "createdAt": "2026-10-19T09:00:00Z" }),
        )
        .unwrap()
    }

    #[test]
    fn the_host_session_names_the_status() {
        let now = "2026-10-19T10:01:00Z".parse().unwrap();
        let sessions = [
            session("c", "other", "errored", "2026-10-19T10:00:59Z"),
            session("c", "host", "working", "2026-10-19T10:00:50Z"),
            session("d", "host", "awaitingInput", "2026-10-19T10:00:59Z"),
        ];
        let of = |sessions: &[Session]| status(session_for(sessions, &chat("c", "host")), now);
        assert_eq!(of(&sessions), "working");
        // Without the host's row, the freshest one speaks.
        assert_eq!(of(&sessions[..1]), "errored");
        let stale = [session("c", "host", "working", "2026-10-19T09:59:00Z")];
        assert_eq!(of(&stale), "working (stale — host may be offline)");
        assert_eq!(of(&[]), "idle");
    }

    #[test]
    fn only_the_apps_tools_are_offered() {
        for tool in ["list_chats", "send_message", "create_chat", "archive_chat"] {
            assert!(tool.parse::<Tool>().is_ok(), "{tool}");
        }
        for tool in [
            "open_terminal",
            "show_visual",
            "send_messages",
            "wait_for_turn",
            "nope",
        ] {
            assert_eq!(
                tool.parse::<Tool>(),
                Err(Unavailable(tool.into())),
                "{tool}"
            );
        }
    }

    #[test]
    fn scratch_folders_are_dated_and_named_by_the_ids_end() {
        let at = "2026-10-19T10:00:00Z".parse().unwrap();
        assert_eq!(
            scratch_cwd("01a0d3bf-536f-79b2-ae0b-bcf3502434d5", at),
            "~/.zeron/scratch/2026-10-19-502434d5"
        );
        assert_eq!(scratch_cwd("--", at), "~/.zeron/scratch/2026-10-19");
    }

    #[test]
    fn the_default_agent_is_claude_code_when_offered() {
        let harness = |id: &str, installed: bool| -> Harness {
            serde_json::from_value(json!({ "id": id, "name": id, "installed": installed })).unwrap()
        };
        let all = [
            harness("mock", true),
            harness("codex", true),
            harness("claude-code", true),
        ];
        assert_eq!(default_harness(&all).unwrap(), "claude-code");
        let some = [
            harness("mock", true),
            harness("codex", true),
            harness("claude-code", false),
        ];
        assert_eq!(default_harness(&some).unwrap(), "codex");
        assert!(default_harness(&[harness("mock", true)]).is_err());
    }
}
