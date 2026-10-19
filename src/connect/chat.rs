//! The chat screen's state in one call: the chat (like `get_chat`) and its
//! latest messages (like `read_chat`), from one read of the transcript.
//!
//! The phone polls this about once a second while a turn runs. Calling both
//! Zeron tools would fetch and render the whole transcript twice per poll;
//! their helpers are private to `zeron-mcp`, so this mirrors the fields the
//! app uses (`crates/mcp/src/tools.rs`: `summarize_chat`, `status_of`,
//! `get_chat`, `read_chat`). Unlike `read_chat`, each message keeps its prose,
//! tool calls, and errors in transcript order, as the desktop shows them.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use zeron_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use zeron_mcp::{RenderOptions, RenderedMessage, Zeron, render_entries};
use zeron_proto::{Chat, Session, SessionStatus, Space};

use super::composer::chat_arg;

/// Messages per page, as `read_chat`.
const DEFAULT_LIMIT: usize = 40;
const MAX_LIMIT: usize = 500;
/// A working session not updated for this long may have lost its host
/// (`SESSION_STALE` in `zeron-mcp`).
const STALE_AFTER: chrono::Duration = chrono::Duration::seconds(45);

/// Return `{chat, version, read}` for the `chat` argument.
///
/// `version` changes when the transcript moves (its length, last activity,
/// or status) or `limit` changes. `read` holds the last `limit` messages,
/// `total`, `olderRemaining`, and `pendingInput`; it is `null` when the
/// caller passes the `known` version and the chat isn't working, since then
/// nothing it shows can have changed.
pub async fn view(zeron: &Zeron, args: &Value) -> Result<Value> {
    let chat = zeron.resolve_chat(chat_arg(args)?).await?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |n| usize::try_from(n).unwrap_or(MAX_LIMIT))
        .clamp(1, MAX_LIMIT);
    let (spaces, sessions, entries) = tokio::try_join!(
        zeron.spaces(),
        zeron.sessions(),
        zeron.transcript_on(&chat.id, Some(&chat.device_id))
    )?;
    let rendered = render_entries(&entries, RenderOptions::default());
    let session = session_for(&sessions, &chat.id, &chat.device_id);
    let status = status(session, Utc::now());
    let version = format!(
        "{}|{}|{status}|{limit}",
        rendered.len(),
        chat.last_message_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    );
    // While a turn streams, its last message changes without moving the version.
    let unchanged = args.get("known").and_then(Value::as_str) == Some(version.as_str())
        && !status.starts_with("working");
    let read = (!unchanged).then(|| window(&entries, &rendered, limit));
    Ok(json!({
        "chat": summary(&chat, &spaces, &status),
        "version": version,
        "read": read,
    }))
}

/// A message as the app shows it: `read_chat`'s fields, with `parts` in
/// place of its `text`, `tools`, and `errors`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Message<'a> {
    id: &'a str,
    role: MessageRole,
    created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<MessageStatus>,
    parts: Vec<Part<'a>>,
}

/// One run of a message, in transcript order. Consecutive tool calls share
/// a group until prose or an error ends it, as the desktop folds them
/// (`ui/src/transcript.rs`); thinking is left out and doesn't end a group.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Part<'a> {
    Text { text: &'a str },
    Tools { tools: Vec<&'a str> },
    Error { error: &'a str },
}

/// The last `limit` messages, shaped like `read_chat`'s answer.
fn window(entries: &[SessionMessageEntry], rendered: &[RenderedMessage], limit: usize) -> Value {
    let start = rendered.len().saturating_sub(limit);
    let page = &rendered[start..];
    let owned = entries_of(entries, page);
    let messages: Vec<Message> = page
        .iter()
        .map(|m| Message {
            id: &m.id,
            role: m.role,
            created_at: m.created_at,
            status: m.status,
            parts: parts(m, owned.get(m.id.as_str()).map_or(&[], Vec::as_slice)),
        })
        .collect();
    json!({
        "total": rendered.len(),
        "olderRemaining": start,
        // From the whole transcript, not just the page shown.
        "pendingInput": rendered.iter().rev().find_map(|m| m.pending_input.clone()),
        "messages": messages,
    })
}

/// Map each message on `page` to its entries: its own and the continuations
/// `render_entries` folded into it, in order.
fn entries_of<'a>(
    entries: &'a [SessionMessageEntry],
    page: &[RenderedMessage],
) -> HashMap<&'a str, Vec<&'a SessionMessageEntry>> {
    let shown: HashSet<&str> = page.iter().map(|m| m.id.as_str()).collect();
    let mut owned: HashMap<&str, Vec<&SessionMessageEntry>> = HashMap::new();
    // As `render_entries`: a continuation folds into its parent only if the
    // parent was rendered before it; otherwise it stands alone.
    let mut roots: HashSet<&str> = HashSet::new();
    for entry in entries {
        let root = entry
            .continuation_of
            .as_deref()
            .filter(|parent| roots.contains(parent))
            .unwrap_or(&entry.id);
        if root == entry.id {
            roots.insert(&entry.id);
        }
        if shown.contains(root) {
            owned.entry(root).or_default().push(entry);
        }
    }
    owned
}

/// Split a message into its parts, in transcript order.
///
/// `render_entries` writes one line to `message.tools` per tool, image, and
/// visual part, in order; this walks the same parts to place each line.
fn parts<'a>(message: &'a RenderedMessage, entries: &[&'a SessionMessageEntry]) -> Vec<Part<'a>> {
    let mut lines = message.tools.iter().map(String::as_str);
    let mut parts = Vec::new();
    for part in entries.iter().flat_map(|e| &e.parts) {
        match part {
            MessagePart::Text { text, .. } if !text.trim().is_empty() => {
                parts.push(Part::Text { text });
            }
            MessagePart::Error { message, .. } => parts.push(Part::Error { error: message }),
            MessagePart::Tool { .. } | MessagePart::Image { .. } | MessagePart::Visual { .. } => {
                let Some(line) = lines.next() else { continue };
                match parts.last_mut() {
                    Some(Part::Tools { tools }) => tools.push(line),
                    _ => parts.push(Part::Tools { tools: vec![line] }),
                }
            }
            _ => {}
        }
    }
    parts
}

/// The chat fields the app shows, named as `get_chat` names them.
fn summary(chat: &Chat, spaces: &[Space], status: &str) -> Value {
    let space = chat
        .space_id
        .as_deref()
        .and_then(|id| spaces.iter().find(|s| s.id == id));
    let config = chat.config.as_ref();
    json!({
        "id": chat.id,
        "title": chat.title,
        "project": space.map(|s| json!({ "id": s.id, "name": s.display_name(), "path": s.path })),
        "branch": chat.branch,
        "harness": config.map(|c| c.harness),
        "model": config.and_then(|c| c.model.as_deref()),
        "reasoning": config.and_then(|c| c.reasoning),
        "status": status,
        "lastMessageAt": chat.last_message_at,
        "createdAt": chat.created_at,
    })
}

/// The session that speaks for a chat: its host device's, else the freshest
/// (`zeron_mcp::zeron::session_for`).
fn session_for<'a>(sessions: &'a [Session], chat_id: &str, host: &str) -> Option<&'a Session> {
    let mine = sessions.iter().filter(|s| s.chat_id == chat_id);
    mine.clone()
        .find(|s| s.device_id == host)
        .or_else(|| mine.max_by_key(|s| s.updated_at))
}

/// The chat's status label, as `zeron-mcp`'s `status_of` words it.
fn status(session: Option<&Session>, now: DateTime<Utc>) -> String {
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
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(chat: &str, device: &str, status: SessionStatus, updated: &str) -> Session {
        serde_json::from_value(json!({
            "chatId": chat, "deviceId": device, "status": status,
            "startedAt": null, "updatedAt": updated,
        }))
        .unwrap()
    }

    #[test]
    fn the_host_session_names_the_status() {
        let now = "2026-10-19T10:01:00Z".parse().unwrap();
        let sessions = [
            session("c", "other", SessionStatus::Errored, "2026-10-19T10:00:59Z"),
            session("c", "host", SessionStatus::Working, "2026-10-19T10:00:50Z"),
            session(
                "d",
                "host",
                SessionStatus::AwaitingInput,
                "2026-10-19T10:00:59Z",
            ),
        ];
        let of = |sessions: &[Session]| status(session_for(sessions, "c", "host"), now);
        assert_eq!(of(&sessions), "working");
        // Without the host's row, the freshest one speaks.
        assert_eq!(of(&sessions[..1]), "errored");
        let stale = [session(
            "c",
            "host",
            SessionStatus::Working,
            "2026-10-19T09:59:00Z",
        )];
        assert_eq!(of(&stale), "working (stale — host may be offline)");
        assert_eq!(of(&[]), "idle");
    }

    #[test]
    fn the_window_keeps_prose_and_tools_in_order() {
        let entry =
            |id: &str, role: &str, continues: Option<&str>, parts: Value| -> SessionMessageEntry {
                serde_json::from_value(json!({
                    "id": id, "role": role, "parts": parts, "createdAt": 1,
                    "deviceId": "d", "status": "complete", "continuationOf": continues,
                }))
                .unwrap()
            };
        let exec = |id: &str, command: &str| {
            json!({
                "kind": "tool", "id": id, "call": { "kind": "exec", "command": command }, "resolved": true,
            })
        };
        let text = |id: &str, text: &str| json!({ "kind": "text", "id": id, "text": text });
        let entries = [
            entry("u1", "user", None, json!([text("t", "old")])),
            entry("u2", "user", None, json!([text("t", "go")])),
            entry(
                "a1",
                "assistant",
                None,
                json!([
                    exec("x1", "ls"),
                    { "kind": "reasoning", "id": "r", "text": "hmm" },
                    exec("x2", "pwd"),
                    text("t1", "Found it."),
                    text("t2", " "),
                    exec("x3", "cargo test"),
                ]),
            ),
            // A long turn continues in a new entry; its tools join the open group.
            entry(
                "a2",
                "assistant",
                Some("a1"),
                json!([
                    exec("x4", "cargo build"),
                    { "kind": "error", "id": "e", "message": "boom" },
                    text("t3", "Done."),
                ]),
            ),
        ];
        let rendered = render_entries(&entries, RenderOptions::default());
        let read = window(&entries, &rendered, 2);
        assert_eq!(read["olderRemaining"], 1);
        assert_eq!(
            read["messages"],
            json!([
                { "id": "u2", "role": "user", "createdAt": 1, "status": "complete",
                  "parts": [{ "kind": "text", "text": "go" }] },
                { "id": "a1", "role": "assistant", "createdAt": 1, "status": "complete", "parts": [
                    { "kind": "tools", "tools": ["exec: ls", "exec: pwd"] },
                    { "kind": "text", "text": "Found it." },
                    { "kind": "tools", "tools": ["exec: cargo test", "exec: cargo build"] },
                    { "kind": "error", "error": "boom" },
                    { "kind": "text", "text": "Done." },
                ] },
            ])
        );
    }
}
