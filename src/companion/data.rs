//! What the companion reads from the phone's tools, as the phone app
//! (`relay/assets/app.js`) reads it.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// A chat as `list_chats` and `chat_view` summarize it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    pub id: String,
    pub title: Option<String>,
    #[serde(default)]
    pub status: Status,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub project: Option<Project>,
    pub branch: Option<String>,
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

/// A chat's status, read from Zeron's label (like `working` or
/// `working (stale — …)`) as the app groups it: by its first word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum Status {
    #[default]
    Idle,
    Working {
        stale: bool,
    },
    Input,
    Failed,
}

impl From<String> for Status {
    fn from(label: String) -> Self {
        match label.split(' ').next().unwrap_or_default() {
            "working" => Self::Working {
                stale: label.contains("stale"),
            },
            "awaitingInput" => Self::Input,
            "errored" => Self::Failed,
            _ => Self::Idle,
        }
    }
}

impl Chat {
    pub fn is_working(&self) -> bool {
        matches!(self.status, Status::Working { .. })
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
    fn statuses_group_by_their_first_word() {
        let cases = [
            ("working", Status::Working { stale: false }),
            ("working (stale — 5m)", Status::Working { stale: true }),
            ("awaitingInput", Status::Input),
            ("errored", Status::Failed),
            ("idle", Status::Idle),
        ];
        for (label, want) in cases {
            let got: Status = serde_json::from_value(serde_json::json!(label)).unwrap();
            assert_eq!(got, want, "{label}");
        }
    }
}
