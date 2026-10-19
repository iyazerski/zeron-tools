//! Append turns made outside Zeron (the pi CLI, Codex's own
//! apps) to the Zeron chat that resumes the session. Zeron resumes the
//! session either way; this makes its transcript show those turns too.

use std::collections::HashMap;

use anyhow::Result;
use zeron_proto::{Chat, HarnessId};

use crate::export::last_activity;
use crate::sources::{Source, after, maybe_running, written_after};
use crate::zeron::Profile;
use crate::{Tally, one_line, short_id};

/// Append each chat's unseen turns. Failures are reported and counted;
/// [`Tally::done`] counts chats that got turns.
pub fn run(profile: &mut Profile, dry_run: bool) -> Result<Tally> {
    // Forks share their session: extend the most recently active chat.
    let mut chats: HashMap<String, Chat> = HashMap::new();
    for chat in profile.chats()? {
        let Some(sid) = chat.harness_session_id.clone() else {
            continue;
        };
        if chat.parent_chat_id.is_none()
            && chats
                .get(&sid)
                .is_none_or(|old| last_activity(old) < last_activity(&chat))
        {
            chats.insert(sid, chat);
        }
    }
    let mut tally = Tally::default();
    let mut running = 0;
    for harness in [HarnessId::Pi, HarnessId::Codex] {
        let Some(source) = Source::for_harness(harness)? else {
            continue;
        };
        for (sid, file) in source.index()? {
            let Some(chat) = chats.get(&sid) else {
                continue;
            };
            if chat.config.as_ref().map(|c| c.harness) != Some(harness) {
                continue;
            }
            // Cheap check first: most sessions haven't changed since.
            let modified = source.modified(&file);
            if !written_after(modified, last_activity(chat).timestamp_millis()) {
                continue;
            }
            if maybe_running(modified) {
                running += 1;
                continue;
            }
            match refresh_one(profile, &source, &file, chat, dry_run) {
                Ok(0) => {}
                Ok(n) => {
                    tally.done += 1;
                    println!(
                        "{} {n} message(s) to chat {}  {}",
                        if dry_run { "would add" } else { "added" },
                        short_id(&chat.id),
                        one_line(chat.title.as_deref().unwrap_or("Untitled"), 50)
                    );
                }
                Err(e) => {
                    tally.failed += 1;
                    eprintln!("FAIL chat {}: {e:#}", chat.id);
                }
            }
        }
    }
    if running > 0 {
        eprintln!("skipped {running} session(s): written in the last 2 minutes, maybe mid-turn");
    }
    Ok(tally)
}

/// Append one chat's unseen turns; returns how many messages.
fn refresh_one(
    profile: &mut Profile,
    source: &Source,
    file: &std::path::Path,
    chat: &Chat,
    dry_run: bool,
) -> Result<usize> {
    let session = source.parse(file)?;
    let mut seen = profile.seen(&chat.id)?;
    // An aborted turn's entry has no length; the row's time covers it.
    seen.end = seen
        .end
        .max(chat.last_message_at.map(|t| t.timestamp_millis()));
    let new = after(&session.messages, &seen);
    if !new.is_empty() && !dry_run {
        profile.append(&chat.id, new)?;
        profile.save()?;
    }
    Ok(new.len())
}
