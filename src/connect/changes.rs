//! What a chat's latest turn changed, as the Changes pane's "Latest turn"
//! scope shows it: the whole patch for the app's diff page, and its counts
//! for the chat screen.
//!
//! Zeron's "Latest turn" diff compares the turn's start with the files as they
//! are now, so later edits leak in. A finished turn's diff is captured once,
//! as soon as this process sees the turn end, and kept: a later view shows
//! the turn as it ended, and costs no git run.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::tools::cwd_of;
use crate::zeron::types::{Chat, CheckoutDiff, Session, SessionStatus, Space};
use crate::zeron::{Zeron, session_for, space_of};

/// How many finished turns' diffs to keep; each holds up to Zeron's patch cap.
const KEPT: usize = 24;

/// The `turn_changes` arguments.
#[derive(Debug, Deserialize)]
pub struct TurnArgs {
    /// The chat's id or id prefix.
    chat: String,
}

/// Return `{patch, files, additions, deletions, truncated}` for the chat's
/// latest turn; empty when Zeron recorded no turn of it.
pub async fn turn(zeron: &Zeron, args: TurnArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    let (spaces, sessions) = tokio::try_join!(zeron.spaces(), zeron.sessions())?;
    let diff = latest(zeron, &chat, &spaces, &sessions).await?;
    Ok(diff.map_or_else(|| json!(CheckoutDiff::default()), |d| json!(*d)))
}

/// `{files, additions, deletions}` of the chat's latest turn, or `None` when
/// it changed nothing.
pub(super) async fn counts(
    zeron: &Zeron,
    chat: &Chat,
    spaces: &[Space],
    sessions: &[Session],
) -> Result<Option<Value>> {
    let diff = latest(zeron, chat, spaces, sessions).await?;
    Ok(diff.filter(|d| !d.files.is_empty()).map(
        |d| json!({ "files": d.files.len(), "additions": d.additions, "deletions": d.deletions }),
    ))
}

/// Capture chat `chat_id`'s turn that just ended, so views show it as it
/// ended. Called when a session settles after a completed turn.
pub(super) async fn freeze(zeron: &Zeron, chat_id: &str) -> Result<()> {
    let chat = zeron.chat(chat_id).await?;
    let (spaces, sessions) = tokio::try_join!(zeron.spaces(), zeron.sessions())?;
    latest(zeron, &chat, &spaces, &sessions).await.map(drop)
}

/// The chat's latest turn diff: the kept capture of this turn, else a new
/// one, kept once the turn is over. `None` for a chat without a folder, or
/// one Zeron recorded no turn of.
async fn latest(
    zeron: &Zeron,
    chat: &Chat,
    spaces: &[Space],
    sessions: &[Session],
) -> Result<Option<Arc<CheckoutDiff>>> {
    let Some(cwd) = cwd_of(chat, space_of(chat, spaces)) else {
        return Ok(None);
    };
    let key = turn_key(chat, session_for(sessions, chat));
    if let Some(kept) = key.as_ref().and_then(|k| KEPT_TURNS.get(&chat.id, k)) {
        return Ok(Some(kept));
    }
    let Some(diff) = zeron.turn_diff(cwd, &chat.id).await? else {
        return Ok(None);
    };
    let diff = Arc::new(diff);
    if let Some(key) = key {
        KEPT_TURNS.put(&chat.id, key, diff.clone());
    }
    Ok(Some(diff))
}

/// What names a finished turn: the session's completed-turn marker and the
/// chat's last activity, as an interrupted turn moves only the latter. `None`
/// while a turn runs, when its changes are still moving.
fn turn_key(chat: &Chat, session: Option<&Session>) -> Option<String> {
    let session = session.filter(|s| s.status == SessionStatus::Idle)?;
    let at = chat.last_message_at?;
    Some(format!(
        "{}|{}",
        session.last_completed_turn.as_deref().unwrap_or_default(),
        at.to_rfc3339()
    ))
}

/// Finished turns' diffs by chat, newest last.
static KEPT_TURNS: LazyLock<Kept> = LazyLock::new(Kept::default);

#[derive(Default)]
struct Kept(Mutex<VecDeque<(String, String, Arc<CheckoutDiff>)>>);

impl Kept {
    /// Chat `chat`'s kept diff, if it's of turn `key`.
    fn get(&self, chat: &str, key: &str) -> Option<Arc<CheckoutDiff>> {
        let kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        kept.iter()
            .find(|(c, k, _)| c == chat && k == key)
            .map(|(.., diff)| diff.clone())
    }

    /// Keep `diff` as chat `chat`'s turn `key`, in place of its older one.
    fn put(&self, chat: &str, key: String, diff: Arc<CheckoutDiff>) {
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        kept.retain(|(c, ..)| c != chat);
        if kept.len() >= KEPT {
            kept.pop_front();
        }
        kept.push_back((chat.to_owned(), key, diff));
    }
}
