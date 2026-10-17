//! Harness session readers. Each source turns its native session files into a
//! harness-neutral [`Session`]; [`crate::zeron`] writes that into Zeron.

pub mod codex;
pub mod pi;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

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
    /// Archived in the harness; imported as an archived chat.
    pub archived: bool,
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
    pub name: Option<String>,
    /// Why this is not a conversation worth importing (empty, a subagent, a
    /// scripted run, Zeron's own leftover); hidden from listings by default.
    pub hidden_reason: Option<String>,
    pub archived: bool,
}

/// A harness's session store, opened once per command.
pub enum Source {
    Pi {
        agent_dir: PathBuf,
    },
    Codex {
        home: PathBuf,
        names: HashMap<String, String>,
    },
}

impl Source {
    /// Open the Pi store at `PI_CODING_AGENT_DIR` or `~/.pi/agent`.
    pub fn pi() -> Self {
        Self::Pi {
            agent_dir: pi::agent_dir(),
        }
    }

    /// Open the Codex store at `CODEX_HOME` or `~/.codex`.
    pub fn codex() -> Self {
        let home = codex::codex_home();
        let names = codex::thread_names(&home);
        Self::Codex { home, names }
    }

    pub fn files(&self) -> Result<Vec<PathBuf>> {
        match self {
            Self::Pi { agent_dir } => pi::session_files(agent_dir),
            Self::Codex { home, .. } => codex::session_files(home),
        }
    }

    /// The session id encoded in a session file's name.
    pub fn file_id<'a>(&self, path: &'a Path) -> Option<&'a str> {
        match self {
            Self::Pi { .. } => pi::file_id(path),
            Self::Codex { .. } => codex::file_id(path),
        }
    }

    pub fn summarize(&self, path: &Path) -> Result<SessionSummary> {
        match self {
            Self::Pi { agent_dir } => pi::summarize(agent_dir, path),
            Self::Codex { names, .. } => codex::summarize(path, names),
        }
    }

    pub fn parse(&self, path: &Path) -> Result<Session> {
        match self {
            Self::Pi { .. } => pi::parse(path),
            Self::Codex { names, .. } => codex::parse(path, names),
        }
    }
}
