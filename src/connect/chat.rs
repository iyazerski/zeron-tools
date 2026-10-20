//! The chat screen's state in one call: the chat (like `list_chats` rows)
//! and its latest messages (like Zeron's `read_chat`), from one read of the
//! transcript.
//!
//! The phone polls this about once a second while a turn runs. Unlike
//! `read_chat`, each message keeps its prose, tool calls, and errors in
//! transcript order, as the desktop shows them.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value, json};

use super::composer::chat_arg;
use super::tools::{status, summary};
use crate::zeron::transcript::{self, Entry, Part, RenderOptions, Rendered, Role};
use crate::zeron::{Zeron, session_for};

/// Messages per page, as `read_chat`.
const DEFAULT_LIMIT: usize = 40;
const MAX_LIMIT: usize = 500;

/// Return `{chat, version, read}` for the `chat` argument.
///
/// `version` changes when the transcript moves (its length, last activity,
/// or status) or `limit` changes. `read` holds the last `limit` messages,
/// `total`, `olderRemaining`, and `pendingInput`; it is `null` when the
/// caller passes the `known` version and the chat isn't working, since then
/// nothing it shows can have changed.
pub async fn view(zeron: &Zeron, args: &Value) -> Result<Value> {
    let chat = zeron.chat(chat_arg(args)?).await?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |n| usize::try_from(n).unwrap_or(MAX_LIMIT))
        .clamp(1, MAX_LIMIT);
    let (spaces, sessions, entries) =
        tokio::try_join!(zeron.spaces(), zeron.sessions(), zeron.transcript(&chat))?;
    let rendered = transcript::render(&entries, RenderOptions::default());
    let status = status(session_for(&sessions, &chat), Utc::now());
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
fn window(entries: &[Entry], rendered: &[Rendered], limit: usize) -> Value {
    let start = rendered.len().saturating_sub(limit);
    let page = &rendered[start..];
    let owned = entries_of(entries, page);
    let messages: Vec<Message> = page
        .iter()
        .map(|m| Message {
            id: &m.id,
            role: m.role,
            created_at: m.created_at,
            status: m.status.as_deref(),
            parts: parts(owned.get(m.id.as_str()).map_or(&[], Vec::as_slice)),
        })
        .collect();
    json!({
        "total": rendered.len(),
        "olderRemaining": start,
        // From the whole transcript, not just the page shown.
        "pendingInput": transcript::pending_input(rendered),
        "messages": messages,
    })
}

/// Map each message on `page` to its entries: its own and the continuations
/// folded into it, in order.
fn entries_of<'a>(entries: &'a [Entry], page: &[Rendered]) -> HashMap<&'a str, Vec<&'a Entry>> {
    let shown: HashSet<&str> = page.iter().map(|m| m.id.as_str()).collect();
    let mut owned: HashMap<&str, Vec<&Entry>> = HashMap::new();
    // As `transcript::render`: a continuation folds into its parent only if
    // the parent came before it; otherwise it stands alone.
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

/// A message's parts, in transcript order.
fn parts<'a>(entries: &[&'a Entry]) -> Vec<Shown<'a>> {
    let mut shown = Vec::new();
    for part in entries.iter().flat_map(|e| &e.parts) {
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
        let rendered = transcript::render(&entries, RenderOptions::default());
        let read = window(&entries, &rendered, 2);
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
