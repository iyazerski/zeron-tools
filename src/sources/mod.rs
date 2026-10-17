//! Harness session readers. Each source turns its native session files into a
//! harness-neutral [`Session`]; [`crate::zeron`] writes that into Zeron.

pub mod pi;

use std::path::PathBuf;

use zeron_doc::{MessagePart, MessageRole};
use zeron_proto::{HarnessId, ReasoningLevel};

/// One conversation read from a harness's native storage.
#[derive(Debug, Clone)]
pub struct Session {
    pub harness: HarnessId,
    /// Harness-native session id: what Zeron stores as `harnessSessionId` and
    /// hands back to the harness to resume the conversation.
    pub id: String,
    /// Absolute cwd the session ran in (Zeron resumes only from the same cwd).
    pub cwd: PathBuf,
    pub created_ms: i64,
    pub last_ms: i64,
    /// Zeron model id, e.g. `anthropic/claude-opus-5-5` for Pi.
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// A name the harness itself stored for the session, if any.
    pub name: Option<String>,
    pub messages: Vec<Message>,
}

/// One transcript row: a user prompt or a whole assistant turn.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    pub created_ms: i64,
    pub duration_ms: Option<i64>,
    pub aborted: bool,
}

impl Message {
    /// Join the message's prose parts (no reasoning, no tools).
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Cheap listing row for `zeron-tools sessions`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub cwd: PathBuf,
    pub created_ms: i64,
    pub user_messages: usize,
    pub first_user: String,
}
