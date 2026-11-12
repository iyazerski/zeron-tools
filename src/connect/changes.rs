//! What each of a chat's turns changed: the counts for the buttons under
//! its turns, and a turn's whole patch for the app's diff page.
//!
//! Zeron forgets a turn's starting snapshot once another turn begins, so a
//! turn's diff is captured as soon as it ends and kept in [`Turns`].

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::tools::cwd_of;
use super::turns::Turns;
use crate::zeron::transcript::{self, Message, Role};
use crate::zeron::types::{Chat, Session, SessionStatus, Space};
use crate::zeron::{Zeron, session_for, space_of};

#[derive(Debug, Deserialize)]
pub struct TurnArgs {
    chat: String,
    /// The id of the user message that started the turn.
    turn: String,
}

/// Return `{patch, files, additions, deletions, truncated}` for the turn.
pub async fn turn(zeron: &Zeron, turns: &Turns, args: TurnArgs) -> Result<Value> {
    let chat = zeron.chat(&args.chat).await?;
    if let Some(diff) = turns.get(&chat.id, &args.turn).await? {
        return Ok(json!(diff));
    }
    // Not kept yet: only the finished latest turn can still be captured.
    capture_latest(zeron, turns, &chat).await?;
    match turns.get(&chat.id, &args.turn).await? {
        Some(diff) => Ok(json!(diff)),
        None => bail!("this turn's changes weren't kept"),
    }
}

/// The kept turns of `messages` that changed files, as
/// `{turn, after, files, additions, deletions}`: `turn` is the user message
/// that started it, and `after` its last message, which the button follows.
pub(super) async fn list(
    zeron: &Zeron,
    turns: &Turns,
    chat: &Chat,
    spaces: &[Space],
    sessions: &[Session],
    messages: &[Message<'_>],
) -> Result<Vec<Value>> {
    if let Err(e) = capture(zeron, turns, chat, spaces, sessions, messages).await {
        // The turns kept before still show.
        eprintln!("can't capture {}'s latest turn: {e:#}", chat.id);
    }
    let counts = turns.counts(&chat.id).await?;
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role() != Role::User {
            continue;
        }
        let Some(c) = counts.get(m.id()).filter(|c| c.files > 0) else {
            continue;
        };
        let end = messages[i + 1..]
            .iter()
            .position(|n| n.role() == Role::User)
            .map_or(messages.len(), |n| i + 1 + n);
        out.push(json!({
            "turn": m.id(),
            "after": messages[end - 1].id(),
            "files": c.files,
            "additions": c.additions,
            "deletions": c.deletions,
        }));
    }
    Ok(out)
}

/// Capture chat `chat_id`'s turn that just ended, so views show it as it
/// ended. Called when a session settles after a completed turn.
pub(super) async fn freeze(zeron: &Zeron, turns: &Turns, chat_id: &str) -> Result<()> {
    let chat = zeron.chat(chat_id).await?;
    capture_latest(zeron, turns, &chat).await
}

async fn capture_latest(zeron: &Zeron, turns: &Turns, chat: &Chat) -> Result<()> {
    let (spaces, sessions, entries) =
        tokio::try_join!(zeron.spaces(), zeron.sessions(), zeron.transcript(chat))?;
    let messages = transcript::messages(&entries);
    capture(zeron, turns, chat, &spaces, &sessions, &messages).await
}

/// Keep the chat's latest turn's diff, if the turn is over and not kept yet.
async fn capture(
    zeron: &Zeron,
    turns: &Turns,
    chat: &Chat,
    spaces: &[Space],
    sessions: &[Session],
    messages: &[Message<'_>],
) -> Result<()> {
    let idle = session_for(sessions, chat).is_some_and(|s| s.status == SessionStatus::Idle);
    let Some(start) = messages.iter().rposition(|m| m.role() == Role::User) else {
        return Ok(());
    };
    // A prompt with no reply yet may not have started its turn, and Zeron's
    // snapshot would still be the previous turn's.
    if !idle || start + 1 == messages.len() {
        return Ok(());
    }
    let turn = messages[start].id();
    if turns.has(&chat.id, turn).await? {
        return Ok(());
    }
    let Some(cwd) = cwd_of(chat, space_of(chat, spaces)) else {
        return Ok(());
    };
    match zeron.turn_diff(cwd, &chat.id).await? {
        Some(diff) => turns.put(&chat.id, turn, &diff).await,
        None => Ok(()),
    }
}
