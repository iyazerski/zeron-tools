//! Archive older chats so the sidebar lists only recent ones.
//!
//! Zeron's sidebar draws a row for every unarchived top-level chat, with no
//! paging (`crates/ui/src/state.rs::visible_chats`), while archived chats
//! live on a paged shelf that can be hidden. Archiving all but the most
//! recent chats of each project keeps the list short. Archived chats stay
//! searchable and resumable.
//!
//! This runs again and again in the background, so it remembers what it
//! archived: a chat you restored stays restored until it has new activity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::zeron::Zeron;
use crate::zeron::types::{Chat, Space};
use crate::{one_line, short_id};

/// The chats this tool archived, each with its last activity then.
#[derive(Debug)]
pub struct Memory {
    path: PathBuf,
    archived: HashMap<String, DateTime<Utc>>,
}

impl Memory {
    /// An empty memory, saved to `path`.
    pub fn empty(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            archived: HashMap::new(),
        }
    }

    /// Read the memory saved at `path`; a missing file is an empty memory.
    pub fn load(path: &Path) -> Result<Self> {
        let archived = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self {
            path: path.to_owned(),
            archived,
        })
    }

    /// Whether this tool archived `chat` before, and it had no activity since.
    fn restored(&self, chat: &Chat) -> bool {
        self.archived.get(&chat.id) == Some(&chat.last_activity())
    }

    fn save(&self) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.archived)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

/// Archive the [`stale`] chats, except pinned ones, those in sidebar
/// sections, and those restored since this tool archived them. Returns how
/// many it archived; a chat that fails is logged and skipped.
pub async fn run(zeron: &Zeron, keep: usize, memory: &mut Memory) -> Result<usize> {
    let (chats, spaces, sidebar) =
        tokio::try_join!(zeron.chats(), zeron.spaces(), zeron.sidebar())?;
    let placed: HashSet<&str> = sidebar.placed().collect();
    let chosen: Vec<&Chat> = stale(&chats, &spaces, keep, &placed)
        .into_iter()
        .filter(|c| !memory.restored(c))
        .collect();
    let mut archived = 0;
    for chat in chosen {
        let op = json!({ "op": "setChatArchived", "chatId": chat.id, "archived": true });
        match zeron.mutate(op).await {
            Ok(_) => {
                archived += 1;
                memory
                    .archived
                    .insert(chat.id.clone(), chat.last_activity());
                eprintln!(
                    "archived {}  {}  {}",
                    short_id(&chat.id),
                    chat.last_activity().format("%Y-%m-%d"),
                    one_line(chat.title().unwrap_or("Untitled"), 50)
                );
            }
            Err(e) => eprintln!("couldn't archive {}: {e:#}", short_id(&chat.id)),
        }
    }
    // Forget deleted chats, and ones that are back in use.
    let known: HashMap<&str, &Chat> = chats.iter().map(|c| (c.id.as_str(), c)).collect();
    let before = memory.archived.len();
    memory.archived.retain(|id, at| {
        known
            .get(id.as_str())
            .is_some_and(|c| c.last_activity() == *at)
    });
    if archived > 0 || memory.archived.len() != before {
        memory.save()?;
    }
    Ok(archived)
}

/// Unarchived top-level chats beyond the `keep` most recently active in their
/// project (project-less chats form one group), oldest first. Chats in
/// `placed` (pinned or in a sidebar section) never count as stale and don't
/// use up `keep`.
pub fn stale<'a>(
    chats: &'a [Chat],
    spaces: &[Space],
    keep: usize,
    placed: &HashSet<&str>,
) -> Vec<&'a Chat> {
    // Group like the sidebar does: every checkout of one repository is one project.
    let project: HashMap<&str, String> = spaces
        .iter()
        .map(|s| (s.id.as_str(), s.project_key()))
        .collect();
    let mut groups: HashMap<Option<&str>, Vec<&Chat>> = HashMap::new();
    for chat in chats {
        if chat.archived || !chat.is_top_level() || placed.contains(chat.id.as_str()) {
            continue;
        }
        let key = chat
            .space_id
            .as_deref()
            .map(|id| project.get(id).map_or(id, String::as_str));
        groups.entry(key).or_default().push(chat);
    }
    let mut out = vec![];
    for mut group in groups.into_values() {
        // The sidebar's "last updated" order, newest first.
        group.sort_by(|a, b| {
            b.last_activity()
                .cmp(&a.last_activity())
                .then_with(|| a.id.cmp(&b.id))
        });
        out.extend(group.into_iter().skip(keep));
    }
    out.sort_by_key(|c| c.last_activity());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, space: Option<&str>, day: u32) -> Chat {
        serde_json::from_value(json!({
            "id": id,
            "deviceId": "d",
            "createdAt": "2026-09-01T00:00:00Z",
            "lastMessageAt": format!("2026-09-{day:02}T00:00:00Z"),
            "spaceId": space,
        }))
        .unwrap()
    }

    fn space(id: &str, repo: Option<&str>) -> Space {
        serde_json::from_value(json!({
            "id": id, "deviceId": "d", "path": format!("/w/{id}"), "repositoryId": repo,
        }))
        .unwrap()
    }

    fn ids(chats: Vec<&Chat>) -> Vec<&str> {
        chats.into_iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn keeps_the_most_recent_chats_of_each_project() {
        let spaces = [
            space("a", Some("r")),
            space("a2", Some("r")),
            space("b", None),
        ];
        let mut archived = chat("archived", Some("a"), 1);
        archived.archived = true;
        let mut child = chat("child", Some("a"), 1);
        child.parent_chat_id = Some("a-new".into());
        let chats = [
            chat("a-old", Some("a"), 2),
            chat("a2-mid", Some("a2"), 5), // another checkout of the same repo
            chat("a-new", Some("a"), 9),
            chat("b-old", Some("b"), 3),
            chat("b-new", Some("b"), 4),
            chat("none-old", None, 1),
            chat("none-new", None, 8),
            chat("pinned", Some("a"), 1),
            archived,
            child,
        ];
        let pinned = HashSet::from(["pinned"]);
        assert_eq!(
            ids(stale(&chats, &spaces, 1, &pinned)),
            ["none-old", "a-old", "b-old", "a2-mid"]
        );
        assert_eq!(ids(stale(&chats, &spaces, 2, &pinned)), ["a-old"]);
    }

    #[test]
    fn a_restored_chat_stays_until_it_has_new_activity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archived.json");
        let mut memory = Memory::load(&path).unwrap();
        let old = chat("old", None, 1);
        memory.archived.insert(old.id.clone(), old.last_activity());
        memory.save().unwrap();
        let memory = Memory::load(&path).unwrap();
        assert!(memory.restored(&old));
        assert!(!memory.restored(&chat("old", None, 2)));
        assert!(!memory.restored(&chat("other", None, 1)));
    }
}
