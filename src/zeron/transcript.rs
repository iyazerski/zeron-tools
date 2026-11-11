//! Chat transcripts: the entries `WatchDocMessages` sends, grouped into
//! messages and rendered as flat, readable lines.
//!
//! The engine keeps a message as a list of parts (text, thinking, tool
//! calls, input requests, errors) and splits long turns into continuation
//! entries (`crates/doc/src/schema.rs`, `parts.rs` in Zeron). Grouping and
//! rendering follow Zeron's own `read_chat` (`crates/mcp/src/transcript.rs`):
//! continuations fold into their turn, prose joins, and each tool call
//! becomes one capped line. A part that doesn't parse (a new kind, or a
//! changed one) is skipped rather than failing the transcript.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::text::clip;

/// The most characters of an `exec` command in its tool line.
const COMMAND_CHARS: usize = 160;
/// The most characters of a tool's first output line in its tool line.
const OUTPUT_CHARS: usize = 200;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub role: Role,
    #[serde(default, deserialize_with = "lenient_parts")]
    pub parts: Vec<Part>,
    /// Epoch milliseconds.
    pub created_at: i64,
    /// `streaming`, `complete`, or `aborted`.
    #[serde(default)]
    pub status: Option<String>,
    /// The entry this one continues (a long turn's next chunk).
    #[serde(default)]
    pub continuation_of: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Part {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Image {
        #[serde(default)]
        name: String,
    },
    Visual {
        #[serde(default)]
        title: String,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        /// `{kind, …}`: `exec {command}`, `readFile {path}`, …
        call: Value,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        resolved: bool,
        #[serde(default)]
        output: Option<String>,
    },
    /// A question the agent asked; unresolved, it waits for an answer.
    #[serde(rename_all = "camelCase")]
    Input {
        request_id: String,
        #[serde(default)]
        questions: Value,
        #[serde(default)]
        resolved: bool,
    },
    Error {
        message: String,
    },
    #[serde(other)]
    Other,
}

/// Parse each part on its own, so one odd part doesn't lose the transcript.
fn lenient_parts<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Part>, D::Error> {
    let raw = Vec::<Value>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|part| serde_json::from_value(part).unwrap_or(Part::Other))
        .collect())
}

/// The transcript in a `WatchDocMessages` frame, if the frame has one: the
/// stream opens with `{reset: [entries]}`, then sends deltas.
pub fn from_frame(frame: &Value) -> Option<Result<Vec<Entry>, serde_json::Error>> {
    let reset = frame.get("reset")?;
    Some(Vec::<Entry>::deserialize(reset))
}

/// One message: an entry and the continuations folded into it, borrowed
/// from the transcript. Its fields are read on demand, so a caller pays only
/// for what it uses.
#[derive(Debug, Clone)]
pub struct Message<'a> {
    first: &'a Entry,
    continuations: Vec<&'a Entry>,
}

/// A question the agent waits on, with the id that answers it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInput<'a> {
    /// Zeron's question list, passed through as it came.
    pub questions: &'a Value,
    pub request_id: &'a str,
}

impl<'a> PendingInput<'a> {
    /// The first question's text, if it has one.
    pub fn first_question(self) -> Option<&'a str> {
        self.questions.get(0)?.get("question")?.as_str()
    }
}

impl<'a> Message<'a> {
    pub fn id(&self) -> &'a str {
        &self.first.id
    }

    pub fn role(&self) -> Role {
        self.first.role
    }

    pub fn created_at(&self) -> i64 {
        self.first.created_at
    }

    /// Its parts in transcript order: its own entry's, then each continuation's.
    pub fn parts(&self) -> impl DoubleEndedIterator<Item = &'a Part> + '_ {
        std::iter::once(self.first)
            .chain(self.continuations.iter().copied())
            .flat_map(|e| &e.parts)
    }

    /// The status the last continuation set: it carries the turn's live one.
    pub fn status(&self) -> Option<&'a str> {
        std::iter::once(self.first)
            .chain(self.continuations.iter().copied())
            .rev()
            .find_map(|e| e.status.as_deref())
    }

    /// Its prose parts, joined with blank lines.
    pub fn text(&self) -> String {
        let texts: Vec<&str> = self
            .parts()
            .filter_map(|p| match p {
                Part::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n\n")
    }

    /// The latest question it asks that isn't answered yet.
    pub fn pending_input(&self) -> Option<PendingInput<'a>> {
        self.parts().rev().find_map(|p| match p {
            Part::Input {
                request_id,
                questions,
                resolved: false,
            } => Some(PendingInput {
                questions,
                request_id,
            }),
            _ => None,
        })
    }
}

/// Group `entries` into messages in order, folding each continuation into
/// the message it continues (one whose parent isn't listed yet stands alone).
pub fn messages(entries: &[Entry]) -> Vec<Message<'_>> {
    let mut messages: Vec<Message> = Vec::with_capacity(entries.len());
    // Where each message sits, by its first entry's id.
    let mut at: HashMap<&str, usize> = HashMap::with_capacity(entries.len());
    for entry in entries {
        if let Some(&parent) = entry.continuation_of.as_deref().and_then(|id| at.get(id)) {
            messages[parent].continuations.push(entry);
            continue;
        }
        at.entry(&entry.id).or_insert(messages.len());
        messages.push(Message {
            first: entry,
            continuations: Vec::new(),
        });
    }
    messages
}

/// The unanswered question of the whole transcript, if any.
pub fn pending_input<'a>(messages: &[Message<'a>]) -> Option<PendingInput<'a>> {
    messages.iter().rev().find_map(Message::pending_input)
}

/// The log line of a tool call, image, or visual: `kind: subject → first
/// output line`, capped so a build log stays one short line.
pub fn tool_line(part: &Part) -> Option<String> {
    let (call, is_error, resolved, output) = match part {
        Part::Image { name } => return Some(format!("image: {name}")),
        Part::Visual { title } => return Some(format!("visual: {title}")),
        Part::Tool {
            call,
            is_error,
            resolved,
            output,
        } => (call, *is_error, *resolved, output.as_deref()),
        _ => return None,
    };
    let field = |name: &str| call.get(name).and_then(Value::as_str);
    let kind = field("kind").unwrap_or("tool");
    let mut line = match (kind, field("path")) {
        ("exec", _) => format!(
            "exec: {}",
            clip(field("command").unwrap_or_default(), COMMAND_CHARS)
        ),
        ("readFile", Some(path)) => format!("read: {path}"),
        ("writeFile", Some(path)) => format!("write: {path}"),
        ("editFile", Some(path)) => format!("edit: {path}"),
        ("applyPatch", Some(path)) => format!("patch: {path}"),
        ("applyPatch", None) => "patch".into(),
        ("search", path) => {
            let pattern = field("pattern").unwrap_or_default();
            match path {
                Some(path) => format!("search: {pattern} in {path}"),
                None => format!("search: {pattern}"),
            }
        }
        ("glob", _) => format!("glob: {}", field("pattern").unwrap_or_default()),
        ("webFetch", _) => format!("fetch: {}", field("url").unwrap_or_default()),
        ("webSearch", _) => format!("web search: {}", field("query").unwrap_or_default()),
        ("todo", _) => {
            let items = call.get("items").and_then(Value::as_array);
            let total = items.map_or(0, Vec::len);
            let done = items.into_iter().flatten().filter(|i| i["done"] == true);
            format!("todo: {}/{total} done", done.count())
        }
        ("mcp", _) => format!(
            "mcp: {}/{}",
            field("server").unwrap_or_default(),
            field("tool").unwrap_or_default()
        ),
        // `unknown {name}`, or a kind added since: name what there is.
        _ => field("name").unwrap_or(kind).to_owned(),
    };
    if let Some(first) = output.and_then(|o| o.trim().lines().next()) {
        line.push_str(" → ");
        line.push_str(&clip(first, OUTPUT_CHARS));
    }
    if is_error {
        line.push_str(" (error)");
    } else if !resolved {
        line.push_str(" (running)");
    }
    Some(line)
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    /// An entry from JSON parts, as the engine sends it.
    pub fn entry(id: &str, role: &str, continues: Option<&str>, parts: Value) -> Entry {
        serde_json::from_value(json!({
            "id": id, "role": role, "parts": parts, "createdAt": 1_700_000_000_000_i64,
            "deviceId": "d", "status": "complete", "continuationOf": continues,
        }))
        .unwrap()
    }

    #[test]
    fn renders_text_tools_and_pending_input_and_folds_continuations() {
        let entries = [
            entry(
                "u1",
                "user",
                None,
                json!([{ "kind": "text", "id": "t", "text": "hello" }]),
            ),
            entry(
                "a1",
                "assistant",
                None,
                json!([
                    { "kind": "reasoning", "id": "r", "text": "thinking" },
                    { "kind": "tool", "id": "x", "call": { "kind": "exec", "command": "cargo test" },
                      "resolved": true, "output": "ok\nmore" },
                    { "kind": "brandNew", "id": "n", "whatever": 1 },
                    { "kind": "text", "id": "t2", "text": 42 },
                    { "kind": "input", "id": "i", "requestId": "req-1", "questions": [] },
                ]),
            ),
            entry(
                "a2",
                "assistant",
                Some("a1"),
                json!([
                    { "kind": "tool", "id": "y", "call": { "kind": "somethingNew", "name": "Agent" } },
                    { "kind": "text", "id": "t3", "text": "done" },
                ]),
            ),
        ];
        // The odd parts (a new kind, a text that isn't text) are skipped.
        assert_eq!(entries[1].parts[2], Part::Other);
        assert_eq!(entries[1].parts[3], Part::Other);
        let messages = messages(&entries);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "hello");
        assert_eq!(messages[1].text(), "done");
        let tools: Vec<String> = entries[1..]
            .iter()
            .flat_map(|e| &e.parts)
            .filter_map(tool_line)
            .collect();
        assert_eq!(tools, ["exec: cargo test → ok", "Agent (running)"]);
        // The app reads this shape.
        assert_eq!(
            serde_json::to_string(&pending_input(&messages).unwrap()).unwrap(),
            r#"{"questions":[],"requestId":"req-1"}"#
        );
    }

    #[test]
    fn only_the_opening_frame_carries_the_transcript() {
        let reset =
            json!({ "reset": [{ "id": "u", "role": "user", "parts": [], "createdAt": 1 }] });
        assert_eq!(from_frame(&reset).unwrap().unwrap()[0].id, "u");
        assert!(from_frame(&json!({ "upsert": [], "count": 1 })).is_none());
    }
}
