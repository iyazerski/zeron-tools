//! What the companion remembers between runs, as the phone app keeps it in
//! local storage: which turns were seen, folded projects, and the last new
//! chat's picks.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::json_file;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::data::Chat;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Each chat's last activity the companion has shown, so later turns read "Done".
    seen: BTreeMap<String, Option<DateTime<Utc>>>,
    /// Project sections folded in the list, by project id (`home` for none).
    pub collapsed: BTreeSet<String>,
    /// The picks of the last chat started here.
    pub new_chat: NewChat,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NewChat {
    /// `None` starts in the most recent chat's project.
    pub project: Option<Option<String>>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
}

impl Prefs {
    /// Load the prefs at `path`; a missing or broken file starts fresh.
    pub fn load(path: PathBuf) -> Self {
        let mut prefs = json_file::load::<Prefs>(&path)
            .unwrap_or_else(|e| {
                eprintln!("{e:#}");
                None
            })
            .unwrap_or_default();
        prefs.path = path;
        prefs
    }

    /// Save the prefs; losing them only resets what the list shows as new.
    pub fn save(&self) {
        if let Err(e) = json_file::save(&self.path, self) {
            eprintln!("{e:#}");
        }
    }

    /// Count chats not met before as seen, so only later turns read "Done".
    pub fn prime(&mut self, chats: &[Chat]) {
        let mut changed = false;
        for c in chats {
            if !self.seen.contains_key(&c.id) {
                self.seen.insert(c.id.clone(), c.last_message_at);
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    pub fn mark_seen(&mut self, chat: &Chat) {
        let Some(at) = chat.last_message_at else {
            return;
        };
        if self.seen.insert(chat.id.clone(), Some(at)) != Some(Some(at)) {
            self.save();
        }
    }

    /// Whether the chat moved since it was last shown here.
    pub fn unseen(&self, chat: &Chat) -> bool {
        matches!(
            (self.seen.get(&chat.id), chat.last_message_at),
            (Some(Some(seen)), Some(at)) if at > *seen
        )
    }

    pub fn toggle_collapsed(&mut self, key: &str) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_owned());
        }
        self.save();
    }
}
