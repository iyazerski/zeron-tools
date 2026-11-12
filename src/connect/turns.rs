//! Each finished turn's diff, kept on disk so the apps can show what any
//! turn changed, not only the latest. Zeron itself keeps only the snapshot
//! the latest turn started from, so a turn is captured when it ends and
//! saved under the user message that started it.
//!
//! Both the Companion and `connect` write here. A write replaces the file
//! whole, so two capturing one turn at once only repeat the work.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::json_file;
use crate::zeron::types::CheckoutDiff;

/// The most the kept diffs may take on disk; the oldest go first.
const BUDGET: u64 = 50 * 1024 * 1024;

/// The counts of one kept turn's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub files: usize,
    pub additions: u32,
    pub deletions: u32,
}

impl From<&CheckoutDiff> for Counts {
    fn from(d: &CheckoutDiff) -> Self {
        Self {
            files: d.files.len(),
            additions: d.additions,
            deletions: d.deletions,
        }
    }
}

/// The kept diffs: `<dir>/<chat id>/<message id>.json`, each a
/// [`CheckoutDiff`].
pub struct Turns {
    dir: PathBuf,
    /// Counts by file, so a chat screen poll doesn't parse whole patches.
    counts: Mutex<HashMap<PathBuf, Counts>>,
}

impl Turns {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("zeron-tools-turns"),
            counts: Mutex::default(),
        }
    }

    pub async fn get(&self, chat: &str, turn: &str) -> Result<Option<CheckoutDiff>> {
        let path = self.path(chat, turn)?;
        blocking(move || json_file::load(&path)).await
    }

    pub async fn has(&self, chat: &str, turn: &str) -> Result<bool> {
        let path = self.path(chat, turn)?;
        blocking(move || Ok(path.exists())).await
    }

    pub async fn counts(&self, chat: &str) -> Result<HashMap<String, Counts>> {
        let dir = self.dir.join(id(chat)?);
        let kept: Vec<(String, PathBuf)> = blocking(move || {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
            };
            Ok(entries
                .filter_map(|e| {
                    let path = e.ok()?.path();
                    let turn = path.file_name()?.to_str()?.strip_suffix(".json")?;
                    Some((turn.to_owned(), path))
                })
                .collect())
        })
        .await?;
        let mut out = HashMap::with_capacity(kept.len());
        for (turn, path) in kept {
            let known = self.lock().get(&path).copied();
            let counts = match known {
                Some(counts) => counts,
                None => {
                    let read = path.clone();
                    let Some(diff) =
                        blocking(move || json_file::load::<CheckoutDiff>(&read)).await?
                    else {
                        continue;
                    };
                    let counts = Counts::from(&diff);
                    self.lock().insert(path, counts);
                    counts
                }
            };
            out.insert(turn, counts);
        }
        Ok(out)
    }

    /// Keep `diff` as chat `chat`'s turn `turn`, then drop the oldest diffs
    /// past [`BUDGET`].
    pub async fn put(&self, chat: &str, turn: &str, diff: &CheckoutDiff) -> Result<()> {
        let path = self.path(chat, turn)?;
        let bytes = serde_json::to_vec(diff)?;
        self.lock().insert(path.clone(), Counts::from(diff));
        let root = self.dir.clone();
        let dropped = blocking(move || {
            let dir = path.parent().unwrap_or(&root);
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
            json_file::save_bytes(&path, &bytes)?;
            prune(&root, BUDGET)
        })
        .await?;
        let mut counts = self.lock();
        for path in dropped {
            counts.remove(&path);
        }
        Ok(())
    }

    fn path(&self, chat: &str, turn: &str) -> Result<PathBuf> {
        Ok(self.dir.join(id(chat)?).join(format!("{}.json", id(turn)?)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Counts>> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `id` if it's safe as a file name: Zeron's ids are UUID-like.
fn id(id: &str) -> Result<&str> {
    let safe = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !safe {
        bail!("unexpected id {id:?}");
    }
    Ok(id)
}

/// Delete the oldest kept diffs under `root` until they fit in `budget`
/// bytes; return the deleted paths.
fn prune(root: &Path, budget: u64) -> Result<Vec<PathBuf>> {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for chat in std::fs::read_dir(root)?.flatten() {
        let Ok(turns) = std::fs::read_dir(chat.path()) else {
            continue;
        };
        for turn in turns.flatten() {
            let Ok(meta) = turn.metadata() else { continue };
            if meta.is_file() {
                let at = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((at, meta.len(), turn.path()));
            }
        }
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_unstable();
    let mut dropped = Vec::new();
    for (_, size, path) in files {
        if total <= budget {
            break;
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        total -= size;
        dropped.push(path);
    }
    Ok(dropped)
}

/// Run file work off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(patch: &str) -> CheckoutDiff {
        CheckoutDiff {
            patch: patch.into(),
            files: vec![crate::zeron::types::DiffFile {
                path: "a.rs".into(),
            }],
            additions: 2,
            deletions: 1,
            ..CheckoutDiff::default()
        }
    }

    #[tokio::test]
    async fn keeps_turns_and_drops_the_oldest_past_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let turns = Turns::new(dir.path());
        assert!(turns.counts("chat-1").await.unwrap().is_empty());
        turns.put("chat-1", "m1", &diff("one")).await.unwrap();
        turns.put("chat-1", "m2", &diff("two")).await.unwrap();
        assert_eq!(
            turns.get("chat-1", "m2").await.unwrap().unwrap().patch,
            "two"
        );
        assert!(turns.get("chat-1", "m3").await.unwrap().is_none());
        let counts = turns.counts("chat-1").await.unwrap();
        let want = Counts {
            files: 1,
            additions: 2,
            deletions: 1,
        };
        assert_eq!(counts.get("m1"), Some(&want));
        assert_eq!(counts.len(), 2);
        assert!(turns.get("../x", "m1").await.is_err());

        let size = std::fs::metadata(turns.path("chat-1", "m1").unwrap())
            .unwrap()
            .len();
        let dropped = prune(&turns.dir, size).unwrap();
        assert_eq!(dropped, [turns.path("chat-1", "m1").unwrap()]);
    }
}
