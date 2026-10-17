//! Harness session readers. Each source turns its native session files into a
//! harness-neutral [`Session`]; [`crate::zeron`] writes that into Zeron.

pub mod codex;
pub mod pi;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// Zeron itself started the session (Pi: Zeron's session record; Codex:
    /// the `zeron-native` originator). Unlinked, it's a leftover of a
    /// deleted chat or a failed first attempt.
    pub started_by_zeron: bool,
    pub archived: bool,
}

/// A harness's session store, opened once per command.
pub enum Source {
    Pi {
        agent_dir: PathBuf,
    },
    Codex {
        names: HashMap<String, String>,
        /// Thread id → its rollout segments, in order.
        threads: HashMap<String, Vec<PathBuf>>,
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
    pub fn codex() -> Result<Self> {
        let home = codex::codex_home();
        Ok(Self::Codex {
            names: codex::thread_names(&home),
            threads: codex::threads(codex::session_files(&home)?),
        })
    }

    /// The store behind a Zeron harness, if zeron-tools can read it.
    pub fn for_harness(harness: HarnessId) -> Result<Option<Self>> {
        Ok(match harness {
            HarnessId::Pi => Some(Self::pi()),
            HarnessId::Codex => Some(Self::codex()?),
            _ => None,
        })
    }

    /// When the session at `path` was last written (a Codex thread's newest
    /// segment).
    pub fn modified(&self, path: &Path) -> Option<SystemTime> {
        let files = match self {
            Self::Pi { .. } => vec![path.to_path_buf()],
            Self::Codex { threads, .. } => Self::segments(threads, path),
        };
        files
            .iter()
            .filter_map(|f| f.metadata().and_then(|m| m.modified()).ok())
            .max()
    }

    /// One file per session (a Codex thread's first segment), oldest first.
    pub fn files(&self) -> Result<Vec<PathBuf>> {
        match self {
            Self::Pi { agent_dir } => pi::session_files(agent_dir),
            Self::Codex { threads, .. } => {
                let mut files: Vec<PathBuf> = threads
                    .values()
                    .filter_map(|s| s.first().cloned())
                    .collect();
                files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
                Ok(files)
            }
        }
    }

    /// All files of the session `path` belongs to.
    fn segments(threads: &HashMap<String, Vec<PathBuf>>, path: &Path) -> Vec<PathBuf> {
        codex::file_id(path)
            .and_then(|id| threads.get(id))
            .cloned()
            .unwrap_or_else(|| vec![path.to_path_buf()])
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
            Self::Codex { names, threads } => {
                codex::summarize(&Self::segments(threads, path), names)
            }
        }
    }

    /// Delete session `id` from the harness's own store. Codex threads go
    /// through `codex` (the CLI). Returns false if the session was not found.
    pub fn delete_session(&self, id: &str, codex: &Path) -> Result<bool> {
        match self {
            Self::Pi { agent_dir } => pi::delete_session(agent_dir, id),
            Self::Codex { threads, .. } => {
                // `codex delete` can't tell "gone" from other failures.
                if !threads.contains_key(id) {
                    return Ok(false);
                }
                codex::delete_thread(codex, id)?;
                Ok(true)
            }
        }
    }

    pub fn parse(&self, path: &Path) -> Result<Session> {
        match self {
            Self::Pi { .. } => pi::parse(path),
            Self::Codex { names, threads } => codex::parse(&Self::segments(threads, path), names),
        }
    }
}
