//! The chat screen's state in one call: the chat (like `get_chat`) and its
//! latest messages (like `read_chat`), from one read of the transcript.
//!
//! The phone polls this about once a second while a turn runs. Calling both
//! Zeron tools would fetch and render the whole transcript twice per poll;
//! their helpers are private to `zeron-mcp`, so this mirrors the fields the
//! app uses (`crates/mcp/src/tools.rs`: `summarize_chat`, `status_of`,
//! `get_chat`, `read_chat`).

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
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
    let read = (!unchanged).then(|| window(&rendered, limit));
    Ok(json!({
        "chat": summary(&chat, &spaces, &status),
        "version": version,
        "read": read,
    }))
}

/// The last `limit` messages, shaped like `read_chat`'s answer.
fn window(rendered: &[RenderedMessage], limit: usize) -> Value {
    let start = rendered.len().saturating_sub(limit);
    json!({
        "total": rendered.len(),
        "olderRemaining": start,
        // From the whole transcript, not just the page shown.
        "pendingInput": rendered.iter().rev().find_map(|m| m.pending_input.clone()),
        "messages": &rendered[start..],
    })
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
}
