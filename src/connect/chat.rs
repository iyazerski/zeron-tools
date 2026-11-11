//! The chat screen's state in one call: the chat (like `list_chats` rows)
//! and its latest messages (like Zeron's `read_chat`), from one read of the
//! transcript.
//!
//! The phone polls this about once a second while a turn runs. Unlike
//! `read_chat`, each message keeps its prose, tool calls, and errors in
//! transcript order, as the desktop shows them.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::changes;
use super::tools::{ChatStatus, summary};
use crate::zeron::transcript::{self, Part, Role};
use crate::zeron::{Zeron, session_for};

/// Messages per page, as `read_chat`.
const DEFAULT_LIMIT: usize = 40;
const MAX_LIMIT: usize = 500;

/// The `chat_view` arguments.
#[derive(Debug, Deserialize)]
pub struct ViewArgs {
    /// The chat's id or id prefix.
    chat: String,
    /// How many of the latest messages to return.
    limit: Option<usize>,
    /// The `version` the caller already shows.
    known: Option<String>,
}

/// Return `{chat, version, read}` for the chat.
///
/// `version` changes when the transcript moves (its length, last activity,
/// or status) or `limit` changes. `read` holds the last `limit` messages,
/// `total`, `olderRemaining`, `pendingInput`, and, once the turn is over
/// and changed files, `turnChanges`; it is `null` when the caller passes
/// the `known` version and the chat isn't working, since then nothing it
/// shows can have changed.
pub async fn view(zeron: &Zeron, args: ViewArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let (spaces, sessions, entries) =
        tokio::try_join!(zeron.spaces(), zeron.sessions(), zeron.transcript(&chat))?;
    let messages = transcript::messages(&entries);
    let status = ChatStatus::of(session_for(&sessions, &chat), Utc::now());
    let version = format!(
        "{}|{}|{}|{limit}",
        messages.len(),
        chat.last_message_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        status.label(),
    );
    // While a turn streams, its last message changes without moving the version.
    let unchanged = args.known.as_deref() == Some(version.as_str()) && !status.is_working();
    let mut read = (!unchanged).then(|| window(&messages, limit));
    if let Some(read) = read.as_mut()
        && !status.is_working()
    {
        match changes::counts(zeron, &chat, &spaces, &sessions).await {
            Ok(counts) => read["turnChanges"] = json!(counts),
            // The chat screen still shows; it only loses its changes button.
            Err(e) => eprintln!("can't read {}'s turn changes: {e:#}", chat.id),
        }
    }
    Ok(json!({
        "chat": summary(&chat, &spaces, &sessions),
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
    role: Role,
    created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    parts: Vec<Shown<'a>>,
}

/// One run of a message, in transcript order. Consecutive tool calls share
/// a group until prose or an error ends it, as the desktop folds them
/// (`ui/src/transcript.rs`); thinking is left out and doesn't end a group.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Shown<'a> {
    Text { text: &'a str },
    Tools { tools: Vec<String> },
    Error { error: &'a str },
}

/// The last `limit` messages, shaped like `read_chat`'s answer.
fn window(all: &[transcript::Message], limit: usize) -> Value {
    let start = all.len().saturating_sub(limit);
    let messages: Vec<Message> = all[start..]
        .iter()
        .map(|m| Message {
            id: m.id(),
            role: m.role(),
            created_at: m.created_at(),
            status: m.status(),
            parts: parts(m.parts()),
        })
        .collect();
    json!({
        "total": all.len(),
        "olderRemaining": start,
        // From the whole transcript, not just the page shown.
        "pendingInput": transcript::pending_input(all),
        "messages": messages,
    })
}

/// A message's parts, in transcript order.
fn parts<'a>(all: impl Iterator<Item = &'a Part>) -> Vec<Shown<'a>> {
    let mut shown = Vec::new();
    for part in all {
        match part {
            Part::Text { text } if !text.trim().is_empty() => shown.push(Shown::Text { text }),
            Part::Error { message } => shown.push(Shown::Error { error: message }),
            _ => {
                let Some(line) = transcript::tool_line(part) else {
                    continue;
                };
                match shown.last_mut() {
                    Some(Shown::Tools { tools }) => tools.push(line),
                    _ => shown.push(Shown::Tools { tools: vec![line] }),
                }
            }
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zeron::transcript::tests::entry;

    #[test]
    fn the_window_keeps_prose_and_tools_in_order() {
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
        let read = window(&transcript::messages(&entries), 2);
        assert_eq!(read["olderRemaining"], 1);
        assert_eq!(
            read["messages"],
            json!([
                { "id": "u2", "role": "user", "createdAt": 1_700_000_000_000_i64, "status": "complete",
                  "parts": [{ "kind": "text", "text": "go" }] },
                { "id": "a1", "role": "assistant", "createdAt": 1_700_000_000_000_i64, "status": "complete", "parts": [
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
