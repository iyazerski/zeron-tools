//! Chat transcripts: the entries `WatchDocMessages` sends, and a flat,
//! readable rendering of them.
//!
//! The engine keeps a message as a list of parts (text, thinking, tool
//! calls, input requests, errors) and splits long turns into continuation
//! entries (`crates/doc/src/schema.rs`, `parts.rs` in Zeron). Rendering
//! follows Zeron's own `read_chat` (`crates/mcp/src/transcript.rs`):
//! continuations fold into their turn, prose joins, and each tool call
//! becomes one capped line. A part that doesn't parse (a new kind, or a
//! changed one) is skipped rather than failing the transcript.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub role: Role,
    #[serde(default, deserialize_with = "lenient_parts")]
    pub parts: Vec<Part>,
    /// Epoch milliseconds.
    pub created_at: i64,
    #[serde(default)]
    pub device_id: String,
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
    /// Where a forked chat's own history starts: entries above it were
    /// copied from the source chat.
    #[serde(rename_all = "camelCase")]
    Fork {
        source_chat_id: String,
        #[serde(default)]
        source_title: String,
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

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Include the model's thinking.
    pub include_reasoning: bool,
    /// Include the one-line-per-call tool log.
    pub include_tools: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            include_reasoning: false,
            include_tools: true,
        }
    }
}

/// One message as people and agents read it; its fields match Zeron's
/// `read_chat`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rendered {
    pub id: String,
    pub role: Role,
    pub created_at: i64,
    pub created_at_iso: String,
    pub device_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Prose parts joined with blank lines.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// One line per tool call, image, and visual, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// An unanswered question the agent waits on: `{requestId, questions}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_input: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Render `entries` in order, folding each continuation into the message it
/// continues (one whose parent isn't rendered yet stands alone).
pub fn render(entries: &[Entry], options: RenderOptions) -> Vec<Rendered> {
    let mut rendered: Vec<Rendered> = Vec::with_capacity(entries.len());
    for entry in entries {
        let message = render_one(entry, options);
        if let Some(parent) = entry
            .continuation_of
            .as_ref()
            .and_then(|id| rendered.iter_mut().find(|m| &m.id == id))
        {
            fold_into(parent, message);
            continue;
        }
        rendered.push(message);
    }
    rendered
}

/// The unanswered question of the whole transcript, if any.
pub fn pending_input(rendered: &[Rendered]) -> Option<Value> {
    rendered.iter().rev().find_map(|m| m.pending_input.clone())
}

fn fold_into(parent: &mut Rendered, child: Rendered) {
    if !child.text.is_empty() {
        if !parent.text.is_empty() {
            parent.text.push_str("\n\n");
        }
        parent.text.push_str(&child.text);
    }
    match (&mut parent.reasoning, child.reasoning) {
        (Some(existing), Some(more)) => {
            existing.push_str("\n\n");
            existing.push_str(&more);
        }
        (slot @ None, Some(more)) => *slot = Some(more),
        _ => {}
    }
    parent.tools.extend(child.tools);
    parent.errors.extend(child.errors);
    if child.pending_input.is_some() {
        parent.pending_input = child.pending_input;
    }
    // The continuation carries the turn's live status.
    if child.status.is_some() {
        parent.status = child.status;
    }
}

fn render_one(entry: &Entry, options: RenderOptions) -> Rendered {
    let mut text = Vec::new();
    let mut reasoning = Vec::new();
    let mut tools = Vec::new();
    let mut pending_input = None;
    let mut errors = Vec::new();
    for part in &entry.parts {
        match part {
            Part::Text { text: t } if !t.trim().is_empty() => text.push(t.as_str()),
            Part::Reasoning { text: t } if options.include_reasoning && !t.trim().is_empty() => {
                reasoning.push(t.as_str());
            }
            Part::Tool { .. } if !options.include_tools => {}
            Part::Image { .. } | Part::Visual { .. } | Part::Tool { .. } => {
                tools.extend(tool_line(part));
            }
            Part::Input {
                request_id,
                questions,
                resolved: false,
            } => {
                pending_input = Some(json!({ "requestId": request_id, "questions": questions }));
            }
            Part::Error { message } => errors.push(message.clone()),
            _ => {}
        }
    }
    Rendered {
        id: entry.id.clone(),
        role: entry.role,
        created_at: entry.created_at,
        created_at_iso: iso(entry.created_at),
        device_id: entry.device_id.clone(),
        status: entry.status.clone(),
        text: text.join("\n\n"),
        reasoning: (!reasoning.is_empty()).then(|| reasoning.join("\n\n")),
        tools,
        pending_input,
        errors,
    }
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
        ("exec", _) => format!("exec: {}", clip(field("command").unwrap_or_default(), 160)),
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
        line.push_str(&clip(first, 200));
    }
    if is_error {
        line.push_str(" (error)");
    } else if !resolved {
        line.push_str(" (running)");
    }
    Some(line)
}

fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

fn iso(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
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
        let rendered = render(&entries, RenderOptions::default());
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[0].text, "hello");
        let a = &rendered[1];
        assert_eq!(a.text, "done");
        assert_eq!(a.tools, ["exec: cargo test → ok", "Agent (running)"]);
        assert!(a.reasoning.is_none(), "reasoning is opt-in");
        assert_eq!(pending_input(&rendered).unwrap()["requestId"], "req-1");
        assert_eq!(a.created_at_iso, "2023-11-14T22:13:20Z");
        let with_reasoning = render(
            &entries,
            RenderOptions {
                include_reasoning: true,
                include_tools: false,
            },
        );
        assert_eq!(with_reasoning[1].reasoning.as_deref(), Some("thinking"));
        assert!(with_reasoning[1].tools.is_empty());
    }

    #[test]
    fn only_the_opening_frame_carries_the_transcript() {
        let reset =
            json!({ "reset": [{ "id": "u", "role": "user", "parts": [], "createdAt": 1 }] });
        assert_eq!(from_frame(&reset).unwrap().unwrap()[0].id, "u");
        assert!(from_frame(&json!({ "upsert": [], "count": 1 })).is_none());
    }
}
