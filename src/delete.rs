//! Delete chats and the harness sessions they resume, and spot chats the
//! agent never finished: you sent a prompt, and every reply was interrupted,
//! failed, or never came.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, bail};
use zeron_doc::{MessageRole, MessageStatus, SessionMessageEntry};
use zeron_mcp::{RenderOptions, render_entries};
use zeron_proto::{Chat, HarnessId};

use crate::sources::Source;
use crate::zeron::Profile;
use crate::{Tally, find_chat, harness_name, one_line, short_id, short_path};

#[derive(Debug, Clone)]
pub struct Opts<'a> {
    /// Chat ids or id prefixes.
    pub chats: &'a [String],
    /// Every chat [`unfinished`] flags.
    pub unfinished: bool,
    /// Sessions Zeron started that no chat resumes anymore.
    pub orphaned: bool,
    /// Chat or session ids (or prefixes) never to delete.
    pub keep: &'a [String],
    /// Keep the harness sessions of deleted chats.
    pub zeron_only: bool,
    /// Codex binary, for `codex delete`.
    pub codex: &'a Path,
    pub dry_run: bool,
}

/// Delete the chats `opts` picks, then their harness sessions (unless
/// another chat still resumes one), then orphaned sessions. Failures are
/// reported and counted.
pub fn run(profile: &mut Profile, opts: &Opts) -> Result<Tally> {
    if opts.chats.is_empty() && !opts.unfinished && !opts.orphaned {
        bail!("pick chats with --chat, --unfinished, or --orphaned");
    }
    let every = profile.chats()?;
    let spaces = profile.spaces()?;
    let kept = |id: &str| opts.keep.iter().any(|k| id.starts_with(k.as_str()));
    // Chosen chats with why.
    let mut doomed: Vec<(&Chat, String)> = vec![];
    for prefix in opts.chats {
        doomed.push((find_chat(&every, prefix)?, "picked".into()));
    }
    if opts.unfinished {
        for chat in &every {
            if doomed.iter().any(|(c, _)| c.id == chat.id) {
                continue;
            }
            if let Some(u) = unfinished(&profile.transcript(&chat.id)?) {
                let prompts = if u.prompts == 1 { "prompt" } else { "prompts" };
                doomed.push((chat, format!("{} {prompts}, {}", u.prompts, u.outcome)));
            }
        }
    }
    doomed.retain(|(c, _)| !kept(&c.id) && !c.harness_session_id.as_deref().is_some_and(kept));
    doomed.sort_by_key(|(c, _)| c.created_at);
    let doomed_ids: HashSet<&str> = doomed.iter().map(|(c, _)| c.id.as_str()).collect();
    // Sessions chats outside this run still resume: never delete those.
    let resumed: HashSet<&str> = every
        .iter()
        .filter(|c| !doomed_ids.contains(c.id.as_str()))
        .filter_map(|c| c.harness_session_id.as_deref())
        .collect();
    // Open only the stores this run touches (Codex lists every rollout).
    let mut sources: HashMap<HarnessId, Source> = HashMap::new();
    for harness in [HarnessId::Pi, HarnessId::Codex] {
        let used = doomed
            .iter()
            .any(|(c, _)| c.config.as_ref().is_some_and(|cfg| cfg.harness == harness));
        if (opts.orphaned || (used && !opts.zeron_only))
            && let Some(source) = Source::for_harness(harness)?
        {
            sources.insert(harness, source);
        }
    }
    let verb = if opts.dry_run {
        "would delete"
    } else {
        "deleted"
    };
    let mut tally = Tally::default();
    for (chat, why) in &doomed {
        let project = spaces
            .iter()
            .find(|s| Some(&s.id) == chat.space_id.as_ref())
            .map_or("no project", |s| s.display_name());
        println!(
            "{verb} chat {}  {why}  {project}  {}",
            short_id(&chat.id),
            one_line(chat.title.as_deref().unwrap_or("Untitled"), 50)
        );
        if !opts.dry_run
            && let Err(e) = profile.delete_chat(&chat.id)
        {
            tally.failed += 1;
            eprintln!("FAIL chat {}: {e:#}", chat.id);
            continue;
        }
        tally.done += 1;
        let harness = chat.config.as_ref().map(|c| c.harness);
        let (Some(harness), Some(sid)) = (harness, chat.harness_session_id.as_deref()) else {
            continue;
        };
        if opts.zeron_only {
            continue;
        }
        if resumed.contains(sid) {
            println!("  kept session {} (another chat resumes it)", short_id(sid));
            continue;
        }
        let Some(source) = sources.get(&harness) else {
            println!(
                "  kept {} session {} (not supported)",
                harness_name(harness),
                short_id(sid)
            );
            continue;
        };
        let session = format!("{} session {}", harness_name(harness), short_id(sid));
        if opts.dry_run {
            println!("  {verb} {session}");
            continue;
        }
        match source.delete_session(sid, opts.codex) {
            Ok(true) => println!("  {verb} {session}"),
            Ok(false) => println!("  {session} was already gone"),
            Err(e) => {
                tally.failed += 1;
                eprintln!("FAIL session {sid}: {e:#}");
            }
        }
    }
    if opts.orphaned {
        let linked: HashSet<&str> = every
            .iter()
            .filter_map(|c| c.harness_session_id.as_deref())
            .collect();
        for harness in [HarnessId::Pi, HarnessId::Codex] {
            let Some(source) = sources.get(&harness) else {
                continue;
            };
            for file in source.files()? {
                // Skip linked sessions before reading them: files can be huge.
                if source
                    .file_id(&file)
                    .is_some_and(|id| linked.contains(id) || kept(id))
                {
                    continue;
                }
                let s = match source.summarize(&file) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("skip {}: {e:#}", file.display());
                        continue;
                    }
                };
                if !s.started_by_zeron || linked.contains(s.id.as_str()) || kept(&s.id) {
                    continue;
                }
                println!(
                    "{verb} {} session {}  orphaned  {}  {}",
                    harness_name(harness),
                    short_id(&s.id),
                    short_path(&s.cwd),
                    one_line(s.name.as_deref().unwrap_or(&s.first_user), 50)
                );
                if opts.dry_run {
                    tally.done += 1;
                    continue;
                }
                match source.delete_session(&s.id, opts.codex) {
                    Ok(_) => tally.done += 1,
                    Err(e) => {
                        tally.failed += 1;
                        eprintln!("FAIL session {}: {e:#}", s.id);
                    }
                }
            }
        }
    }
    Ok(tally)
}

/// Why a chat counts as unfinished, for the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfinished {
    pub prompts: usize,
    /// What happened to the last reply: `no reply`, `interrupted`, `cut off`,
    /// `no answer`, or `error: <message>`.
    pub outcome: String,
}

/// `Some` if no assistant turn in `entries` ever completed with an answer
/// (text, no error). A chat without any prompt counts too.
pub fn unfinished(entries: &[SessionMessageEntry]) -> Option<Unfinished> {
    // Zeron's renderer folds continuation entries into their turn.
    let messages = render_entries(
        entries,
        RenderOptions {
            include_reasoning: false,
            include_tools: false,
        },
    );
    let replies: Vec<_> = messages
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .collect();
    let answered = replies.iter().any(|m| {
        // Docs older than message status have none; their turns finished.
        m.status.is_none_or(|s| s == MessageStatus::Complete)
            && m.errors.is_empty()
            && !m.text.trim().is_empty()
    });
    if answered {
        return None;
    }
    let outcome = match replies.last() {
        None => "no reply".into(),
        Some(m) => match (m.errors.first(), m.status) {
            (Some(error), _) => format!("error: {}", crate::one_line(error, 80)),
            (None, Some(MessageStatus::Aborted)) => "interrupted".into(),
            (None, Some(MessageStatus::Streaming)) => "cut off".into(),
            _ => "no answer".into(),
        },
    };
    Some(Unfinished {
        prompts: messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count(),
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use zeron_doc::MessagePart;

    use super::*;

    fn entry(
        role: MessageRole,
        status: Option<MessageStatus>,
        parts: Vec<MessagePart>,
    ) -> SessionMessageEntry {
        SessionMessageEntry {
            id: format!("{role:?}{}", parts.len()),
            role,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
            duration_ms: None,
        }
    }

    fn text(t: &str) -> Vec<MessagePart> {
        vec![MessagePart::Text {
            id: "p".into(),
            text: t.into(),
        }]
    }

    fn outcome(entries: &[SessionMessageEntry]) -> Option<String> {
        unfinished(entries).map(|u| format!("{} {}", u.prompts, u.outcome))
    }

    #[test]
    fn chats_without_a_finished_answer_are_unfinished() {
        let prompt = entry(MessageRole::User, None, text("Fix the bug"));
        let reply = |status, parts| entry(MessageRole::Assistant, status, parts);
        let complete = Some(MessageStatus::Complete);
        assert_eq!(outcome(&[]).as_deref(), Some("0 no reply"));
        assert_eq!(
            outcome(std::slice::from_ref(&prompt)).as_deref(),
            Some("1 no reply")
        );
        let interrupted = reply(Some(MessageStatus::Aborted), text("I'm looking…"));
        assert_eq!(
            outcome(&[prompt.clone(), interrupted.clone()]).as_deref(),
            Some("1 interrupted")
        );
        let failed = reply(
            complete,
            vec![MessagePart::Error {
                id: "e".into(),
                message: "404 model: gone".into(),
            }],
        );
        assert_eq!(
            outcome(&[prompt.clone(), failed]).as_deref(),
            Some("1 error: 404 model: gone")
        );
        // One finished answer anywhere keeps the chat.
        let answer = reply(complete, text("Fixed."));
        assert_eq!(
            outcome(&[prompt.clone(), interrupted, prompt.clone(), answer]),
            None
        );
        assert_eq!(outcome(&[prompt, reply(None, text("Old doc"))]), None);
    }
}
