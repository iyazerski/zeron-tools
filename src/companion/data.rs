//! What the companion reads from the phone's tools, as the phone app
//! (`relay/assets/app.js`) reads it.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::connect::ChatStatus;

/// A chat as `list_chats` and `chat_view` summarize it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    pub title: Option<String>,
    #[serde(default)]
    pub status: ChatStatus,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub project: Option<Project>,
    pub last_message_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_message_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub path: String,
}

impl Chat {
    pub fn is_working(&self) -> bool {
        self.status.is_working()
    }

    pub fn title(&self) -> &str {
        self.title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or("Untitled chat")
    }

    /// When the chat last moved, for sorting and its age.
    pub fn active_at(&self) -> DateTime<Utc> {
        self.last_message_at.unwrap_or(self.created_at)
    }
}

#[derive(Debug, Deserialize)]
pub struct Chats {
    pub chats: Vec<Chat>,
}

/// `chat_view`'s answer; `read` is `None` when nothing changed since `known`.
#[derive(Debug, Deserialize)]
pub struct ChatView {
    pub chat: Chat,
    pub version: String,
    pub read: Option<Read>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Read {
    pub messages: Vec<Message>,
    #[serde(default)]
    pub older_remaining: usize,
    pub pending_input: Option<PendingInput>,
    /// The finished turns whose changes were kept and changed files.
    #[serde(default)]
    pub turn_changes: Vec<TurnChanges>,
}

/// The counts of a turn's changes, for the button that opens them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TurnChanges {
    /// The user message that started the turn.
    pub turn: String,
    /// The turn's last message, which the button follows.
    pub after: String,
    pub files: usize,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub role: Role,
    /// Epoch milliseconds.
    pub created_at: i64,
    pub status: Option<MessageStatus>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Streaming,
    Aborted,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    #[serde(other)]
    System,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Part {
    Text { text: String },
    Tools { tools: Vec<String> },
    Error { error: String },
}

impl Message {
    /// The message's prose, joined as `read_chat` joins it.
    pub fn text(&self) -> String {
        let texts: Vec<&str> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n\n")
    }

    /// Whether the message ends with prose, so `answer` has one.
    pub fn has_answer(&self) -> bool {
        matches!(self.parts.last(), Some(Part::Text { .. }))
    }

    /// The prose after the message's last tool group or error: the answer
    /// that ends the turn, as Markdown; `None` when it ends without prose.
    pub fn answer(&self) -> Option<String> {
        let tail = self
            .parts
            .iter()
            .rposition(|p| !matches!(p, Part::Text { .. }))
            .map_or(0, |i| i + 1);
        let texts: Vec<&str> = self.parts[tail..]
            .iter()
            .filter_map(|p| match p {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!texts.is_empty()).then(|| texts.join("\n\n"))
    }
}

/// Questions the agent waits on, as Zeron asks them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInput {
    pub request_id: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    pub id: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub prefill: Option<String>,
}

/// `polish_dictation`'s answer: what dictation heard, cleaned up.
#[derive(Debug, Deserialize)]
pub struct Polished {
    pub text: String,
}

/// `composer_info`: what the composer shows under the text.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerInfo {
    pub branch: Option<String>,
    /// Set for a git project, the only place the branch shows.
    pub checkout: Option<Checkout>,
    pub context_usage: Option<ContextUsage>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Checkout {
    Local,
    Worktree,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub window: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Models {
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Harness {
    pub id: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Harnesses {
    pub harnesses: Vec<Harness>,
}

#[derive(Debug, Deserialize)]
pub struct Projects {
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The reference that invokes the skill when sent.
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Deserialize)]
pub struct Skills {
    pub skills: Vec<Skill>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub chat_id: String,
    /// Why the first message didn't go, though the chat was created.
    #[serde(default)]
    pub send_error: Option<String>,
}

/// A model and reasoning pick, as `set_chat_model` saves it.
#[derive(Debug, Deserialize)]
pub struct Picked {
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

/// The reasoning levels for `model`: its own, else its harness's.
pub fn levels_of<'a>(models: &'a [Model], harness: &'a [String], model: &str) -> &'a [String] {
    models
        .iter()
        .find(|m| m.id == model)
        .map(|m| m.reasoning_levels.as_slice())
        .filter(|l| !l.is_empty())
        .unwrap_or(harness)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_is_the_prose_after_the_last_tools_or_error() {
        let text = |t: &str| Part::Text { text: t.into() };
        let tools = || Part::Tools {
            tools: vec!["ran ls".into()],
        };
        let error = || Part::Error {
            error: "boom".into(),
        };
        let cases = [
            (vec![text("a")], Some("a")),
            (
                vec![text("a"), tools(), text("b"), tools(), text("c")],
                Some("c"),
            ),
            (
                vec![text("a"), tools(), text("b"), text("c")],
                Some("b\n\nc"),
            ),
            (vec![text("a"), error(), text("b")], Some("b")),
            (vec![text("a"), tools()], None),
            (vec![], None),
        ];
        for (parts, want) in cases {
            let m = Message {
                id: "m".into(),
                role: Role::Assistant,
                created_at: 0,
                status: None,
                parts,
            };
            assert_eq!(m.answer().as_deref(), want);
            assert_eq!(m.has_answer(), want.is_some());
        }
    }
}
