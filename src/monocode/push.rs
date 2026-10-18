//! Copy Zeron's chats to MonoCode, one way.
//!
//! MonoCode can't import sessions, but chats it starts are plain pi and
//! Codex sessions, which `sync` imports into Zeron like any other. So only
//! this direction needs code. A Zeron chat and a MonoCode chat are the same
//! conversation when they resume the same harness session; that session's
//! file holds every turn, made in either app, and is the source of
//! MonoCode transcripts:
//! - a Zeron chat MonoCode lacks gets a MonoCode chat (title, archived flag,
//!   model, transcript copied once);
//! - a MonoCode chat whose session got turns elsewhere gets them appended.
//!
//! A state file remembers every session MonoCode had, so a chat deleted in
//! MonoCode isn't created again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zeron_proto::{Chat, HarnessId};

use super::{Monocode, blocks};
use crate::export::last_activity;
use crate::sources::{Source, after, maybe_running, written_after};
use crate::zeron::Profile;
use crate::{Tally, one_line, short_path};

/// Sessions MonoCode had a chat for at some sync: session id → chat id.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    seen: BTreeMap<String, String>,
}

impl State {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically: a torn file would forget what was deleted.
    fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// A harness's store and its session id → file index.
struct Store {
    source: Source,
    files: HashMap<String, PathBuf>,
}

impl Store {
    fn file(&self, sid: &str) -> Option<&PathBuf> {
        self.files.get(sid)
    }
}

/// Copy new Zeron chats to MonoCode and append unseen turns to MonoCode
/// chats. `state` is the file remembering MonoCode's sessions. Failures are
/// reported and counted; [`Tally::done`] counts chats created or extended.
pub fn run(profile: &Profile, mono: &Monocode, state_path: &Path, dry_run: bool) -> Result<Tally> {
    let mut state = State::load(state_path)?;
    let monos: HashMap<String, super::Chat> = mono
        .chats()?
        .into_iter()
        .map(|c| (c.provider_session_id.clone(), c))
        .collect();
    for (sid, chat) in &monos {
        state.seen.insert(sid.clone(), chat.id.clone());
    }
    // Forks share their session: copy the most recently active chat.
    let mut zeron: HashMap<String, Chat> = HashMap::new();
    for chat in profile.chats()? {
        let harness = chat.config.as_ref().map(|c| c.harness);
        let Some(sid) = chat.harness_session_id.clone() else {
            continue;
        };
        if chat.parent_chat_id.is_none()
            && harness.and_then(blocks::harness).is_some()
            && zeron
                .get(&sid)
                .is_none_or(|old| last_activity(old) < last_activity(&chat))
        {
            zeron.insert(sid, chat);
        }
    }
    let mut stores: HashMap<&'static str, Store> = HashMap::new();
    for (name, harness) in [("pi", HarnessId::Pi), ("codex", HarnessId::Codex)] {
        if let Some(source) = Source::for_harness(harness)? {
            let files = source.index()?;
            stores.insert(name, Store { source, files });
        }
    }
    // For chats on a model Zeron dropped (they run on Zeron's default): the
    // model of the most recently active chat on the same harness.
    let mut latest: HashMap<HarnessId, (i64, String)> = HashMap::new();
    for chat in zeron.values() {
        if let Some(config) = &chat.config
            && let Some(model) = &config.model
        {
            let at = last_activity(chat).timestamp_millis();
            if latest.get(&config.harness).is_none_or(|(t, _)| *t < at) {
                latest.insert(config.harness, (at, model.clone()));
            }
        }
    }
    let fallback: HashMap<HarnessId, String> =
        latest.into_iter().map(|(h, (_, m))| (h, m)).collect();
    let spaces = profile.spaces()?;
    let mut tally = Tally::default();
    let mut skipped: BTreeMap<&'static str, usize> = BTreeMap::new();
    // Project folders of new chats, with when each was last used.
    let mut rail: HashMap<String, i64> = HashMap::new();

    // MonoCode chats whose session got turns elsewhere.
    let mut extended = vec![];
    for (sid, m) in &monos {
        let Some(store) = stores.get(m.harness.as_str()) else {
            continue;
        };
        let Some(file) = store.file(sid) else {
            continue;
        };
        let modified = store.source.modified(file);
        if !written_after(modified, m.updated_at) {
            continue;
        }
        if maybe_running(modified) {
            *skipped
                .entry("written in the last 2 minutes, maybe mid-turn")
                .or_default() += 1;
            continue;
        }
        match extend(mono, store, file, m, dry_run) {
            Ok(0) => {}
            Ok(n) => extended.push((m, n)),
            Err(e) => {
                tally.failed += 1;
                eprintln!("FAIL MonoCode chat {}: {e:#}", m.id);
            }
        }
    }
    for (m, n) in extended {
        tally.done += 1;
        println!(
            "{} {n} message(s) to MonoCode chat  {}",
            if dry_run { "would add" } else { "added" },
            one_line(&m.title, 50)
        );
    }

    // Zeron chats MonoCode never had.
    let mut new: Vec<(&String, &Chat)> = zeron
        .iter()
        .filter(|(sid, _)| !monos.contains_key(*sid))
        .collect();
    new.sort_by_key(|(_, c)| c.created_at);
    for (sid, z) in new {
        if state.seen.contains_key(sid) {
            *skipped.entry("deleted in MonoCode").or_default() += 1;
            continue;
        }
        // MonoCode files every chat under a project folder.
        let Some(space) = z
            .space_id
            .as_ref()
            .and_then(|id| spaces.iter().find(|s| &s.id == id))
        else {
            *skipped.entry("in no Zeron project").or_default() += 1;
            continue;
        };
        match create(mono, &stores, &fallback, z, &space.path, dry_run) {
            Ok(Some((id, messages))) => {
                tally.done += 1;
                println!(
                    "{} {}  {messages} message(s){}  {}",
                    if dry_run { "would copy" } else { "copied" },
                    one_line(z.title.as_deref().unwrap_or("Untitled"), 50),
                    if z.archived { ", archived" } else { "" },
                    space.display_name()
                );
                let at = rail.entry(space.path.clone()).or_default();
                *at = (*at).max(last_activity(z).timestamp_millis());
                state.seen.insert(sid.clone(), id);
            }
            Ok(None) => {
                *skipped
                    .entry("their pi/Codex session is gone or empty")
                    .or_default() += 1
            }
            Err(e) => {
                tally.failed += 1;
                eprintln!("FAIL chat {}: {e:#}", z.id);
            }
        }
    }

    let on_rail = mono.projects()?;
    rail.retain(|path, _| !on_rail.contains(path));
    let mut added: Vec<&String> = rail.keys().collect();
    added.sort();
    for path in added {
        println!(
            "{} MonoCode project {}",
            if dry_run { "would add" } else { "added" },
            short_path(Path::new(path))
        );
    }
    if !dry_run {
        mono.add_projects(&rail)?;
        state.save(state_path)?;
    }
    for (why, n) in skipped {
        eprintln!("skipped {n} chat(s): {why}");
    }
    // `rememberProject` keeps 20 rail entries once another folder opens.
    let projects = on_rail.len() + rail.len();
    if projects > 20 {
        eprintln!(
            "note: MonoCode's project rail lists {projects} folders; opening another folder in \
             MonoCode trims it to the 20 most recent (their chats stay)"
        );
    }
    Ok(tally)
}

/// Append the turns `m` hasn't seen; returns how many messages.
fn extend(
    mono: &Monocode,
    store: &Store,
    file: &Path,
    m: &super::Chat,
    dry_run: bool,
) -> Result<usize> {
    let session = store.source.parse(file)?;
    let new = after(&session.messages, &blocks::seen(&mono.blocks(&m.id)?));
    if !new.is_empty() && !dry_run {
        mono.append(&m.id, &blocks::from_messages(new), session.last_ms)?;
    }
    Ok(new.len())
}

/// Create a MonoCode chat for `z` in project folder `project`; returns its
/// id and message count, or `None` when the harness session is gone or has
/// no prompt (MonoCode lists only chats with one). `fallback`: a model per
/// harness for chats without one.
fn create(
    mono: &Monocode,
    stores: &HashMap<&'static str, Store>,
    fallback: &HashMap<HarnessId, String>,
    z: &Chat,
    project: &str,
    dry_run: bool,
) -> Result<Option<(String, usize)>> {
    let config = z.config.as_ref().context("chat has no config")?;
    let h = blocks::harness(config.harness).context("harness not supported")?;
    let sid = z
        .harness_session_id
        .as_deref()
        .context("chat has no session")?;
    let Some((store, file)) = stores.get(h).and_then(|s| Some((s, s.file(sid)?))) else {
        return Ok(None);
    };
    let session = store.source.parse(file)?;
    let transcript = blocks::from_messages(&session.messages);
    if !transcript.iter().any(|b| b["role"] == "user") {
        return Ok(None);
    }
    let model = match config
        .model
        .as_ref()
        .or_else(|| fallback.get(&config.harness))
    {
        Some(m) => blocks::model(h, m),
        None => mono
            .default_model(h)?
            .context("no model, and MonoCode has no default for this harness")?,
    };
    let cwd = z
        .harness_session_cwd
        .clone()
        .unwrap_or_else(|| project.to_owned());
    let chat = super::Chat {
        id: uuid::Uuid::new_v4().to_string(),
        cwd: project.to_owned(),
        harness: h.to_owned(),
        model,
        model_settings: blocks::settings(h, config.reasoning.or(session.reasoning)),
        runtime_mode: blocks::runtime_mode(config.sandbox).to_owned(),
        title: blocks::title(h, z.title.as_deref()),
        provider_session_id: sid.to_owned(),
        created_at: z.created_at.timestamp_millis(),
        // The transcript's end, so the next run's turn check starts there.
        updated_at: session.last_ms.max(last_activity(z).timestamp_millis()),
        archived: z.archived,
        worktree_cwd: (cwd != project).then_some(cwd),
    };
    if !dry_run {
        mono.insert(&chat, &transcript)?;
    }
    Ok(Some((chat.id, session.messages.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(State::load(&path).unwrap().seen.is_empty());
        let mut state = State::default();
        state.seen.insert("sid".into(), "chat".into());
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap().seen["sid"], "chat");
    }
}
