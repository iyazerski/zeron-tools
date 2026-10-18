//! Import harness sessions as Zeron chats that resume the original session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};

use crate::sources::Source;
use crate::zeron::{ImportAs, Profile};
use crate::{Tally, short_id, short_path};

/// When an import creates a Zeron project for a cwd that has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projects {
    /// Never: such chats land under "No project".
    Never,
    /// For every cwd.
    Always,
    /// Only for a cwd that is a git repository root (where Zeron's own
    /// projects usually point); others land under "No project".
    GitRoots,
}

#[derive(Debug, Clone)]
pub struct Opts<'a> {
    /// Chat title (single session only; default: the session's own name).
    pub title: Option<&'a str>,
    pub projects: Projects,
    /// Delete the chat already linked to a session and import it again.
    pub replace: bool,
    pub dry_run: bool,
}

/// Import `files` one by one, persisting each so a later failure keeps
/// earlier imports. Failures are reported and counted, not returned.
pub fn run(profile: &mut Profile, source: &Source, files: &[PathBuf], opts: &Opts) -> Tally {
    let mut tally = Tally::default();
    for file in files {
        match import_one(profile, source, file, opts) {
            Ok(()) => tally.done += 1,
            Err(e) => {
                tally.failed += 1;
                eprintln!("FAIL {}: {e:#}", file.display());
            }
        }
    }
    tally
}

fn import_one(profile: &mut Profile, source: &Source, file: &Path, opts: &Opts) -> Result<()> {
    let session = source.parse(file)?;
    let linked = profile.linked_chat(&session.id)?;
    if let Some(chat) = &linked
        && !opts.replace
    {
        bail!(
            "already linked to chat {} (use --replace to re-import)",
            chat.id
        );
    }
    if opts.dry_run {
        println!(
            "would import {}  {} messages  {}{}",
            short_id(&session.id),
            session.messages.len(),
            short_path(&session.cwd),
            linked
                .map(|c| format!("  (replacing chat {})", short_id(&c.id)))
                .unwrap_or_default()
        );
        return Ok(());
    }
    if let Some(chat) = linked {
        profile.delete_chat(&chat.id)?;
        println!("deleted chat {} (replacing it)", chat.id);
    }
    let project = match opts.projects {
        Projects::Never => None,
        Projects::Always => Some(session.cwd.as_path()),
        Projects::GitRoots => session
            .cwd
            .join(".git")
            .exists()
            .then_some(session.cwd.as_path()),
    };
    let how = ImportAs {
        title: opts.title,
        project,
        sandbox: None,
    };
    let done = profile.import(&session, &how)?;
    profile.save()?;
    println!(
        "imported {} -> chat {}  {} messages  {}{}",
        short_id(&session.id),
        done.chat_id,
        done.messages,
        short_path(&session.cwd),
        match (&done.space_id, done.created_space) {
            (_, true) => " (new project)",
            (None, _) => " (no project)",
            _ => "",
        }
    );
    if done.cwd_missing {
        println!(
            "  note: {} is missing; the chat can't resume until it exists",
            short_path(&session.cwd)
        );
    }
    if let Some(model) = done.dropped_model {
        println!("  note: Zeron no longer offers {model}; the chat uses Zeron's default model");
    }
    Ok(())
}

/// Sessions written within this long may still be running in their harness.
pub const ACTIVE_WITHIN: Duration = Duration::from_secs(60 * 60);

/// Sessions worth importing: listed by `sessions` (not linked, not hidden),
/// with an existing folder outside `exclude`, and not written within
/// [`ACTIVE_WITHIN`]. Prints how many were skipped and why.
pub fn pending(profile: &Profile, source: &Source, exclude: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let linked: HashSet<String> = profile
        .chats()?
        .into_iter()
        .filter_map(|c| c.harness_session_id)
        .collect();
    let now = SystemTime::now();
    let (mut out, mut missing, mut excluded, mut active) = (vec![], 0, 0, 0);
    for file in source.files()? {
        // Skip linked sessions before reading them: files can be huge.
        if source.file_id(&file).is_some_and(|id| linked.contains(id)) {
            continue;
        }
        let s = match source.summarize(&file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip {}: {e:#}", file.display());
                continue;
            }
        };
        if s.hidden_reason.is_some() || linked.contains(&s.id) {
            continue;
        }
        if !s.cwd.is_dir() {
            missing += 1;
        } else if exclude.iter().any(|e| s.cwd.starts_with(e)) {
            excluded += 1;
        } else if source
            .modified(&file)
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age < ACTIVE_WITHIN)
        {
            active += 1;
        } else {
            out.push(file);
        }
    }
    for (n, why) in [
        (missing, "their folder is gone"),
        (excluded, "their folder is excluded"),
        (active, "they were active in the last hour"),
    ] {
        if n > 0 {
            eprintln!("skipped {n} session(s): {why}");
        }
    }
    Ok(out)
}
