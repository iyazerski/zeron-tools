//! Offline access to a Zeron profile: the registry (chat/space rows) and the
//! per-chat transcript docs in `docs.sqlite3`.
//!
//! Writes go through the same zeron-doc/zeron-sync calls Zeron's own one-time
//! local→synced import uses (`crates/engine/src/local_import.rs`): a chat doc
//! is saved "born chat2" (cursor 0, epoch 2) and rows are upserted into the
//! registry. Zeron keeps the registry in memory while running and would
//! overwrite our writes, so [`Profile::open`] takes Zeron's instance lock.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use zeron_doc::{
    MessagePart, MessageStatus, REGISTRY_DOC_ID, RegistryDoc, SessionDoc, SessionMessageEntry,
    sanitize_tool_call,
};
use zeron_proto::{Chat, ChatConfig, HarnessId, Model, SandboxLevel, Space};
use zeron_sync::DocsStore;

use crate::sources::Session;

/// The Zeron release this build reads and writes, as a literal so `--version`
/// can embed it. Must match the git tag of the zeron-* dependencies in
/// Cargo.toml (see AGENTS.md).
macro_rules! zeron_version {
    () => {
        "0.2.113"
    };
}
pub(crate) use zeron_version;

pub const ZERON_VERSION: &str = zeron_version!();

/// `zeron_engine::chat2_host::CHAT2_DOC_EPOCH` (engine crate is not a dependency).
const CHAT2_DOC_EPOCH: u32 = 2;
/// Every chat Zeron creates today rides the chat2 room generation.
const CHAT2_ROOM_GEN: u32 = 2;

/// Zeron's data dir: `ZERON_DATA_DIR`, else `~/.zeron`.
pub fn data_dir() -> PathBuf {
    std::env::var_os("ZERON_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".zeron"))
}

pub struct Profile {
    data_dir: PathBuf,
    dir: PathBuf,
    store: DocsStore,
    registry: RegistryDoc,
    device: String,
    dirty: bool,
    /// Held for the profile's lifetime; `None` for read-only opens.
    lock: Option<File>,
}

/// What [`Profile::import`] created.
#[derive(Debug, Clone)]
pub struct Imported {
    pub chat_id: String,
    pub space_id: Option<String>,
    pub created_space: bool,
    pub messages: usize,
    /// The session's model, when Zeron no longer offers it (the chat then
    /// runs on Zeron's default model).
    pub dropped_model: Option<String>,
    /// The session's folder is gone: the history shows, but resuming fails
    /// until it exists again. No project is created for it.
    pub cwd_missing: bool,
}

impl Profile {
    /// Open the profile at `profile_dir`. With `write`, first take Zeron's
    /// lock in `data_dir`, failing if Zeron is running; read-only opens may
    /// see slightly stale data while it runs. Fails (unless `ignore_version`)
    /// if Zeron last booted a different version than [`ZERON_VERSION`].
    pub fn open(
        data_dir: &Path,
        profile_dir: &Path,
        write: bool,
        ignore_version: bool,
    ) -> Result<Self> {
        let lock = write.then(|| lock(data_dir)).transpose()?;
        if !profile_dir.join("docs.sqlite3").is_file() {
            bail!("no Zeron profile at {}", profile_dir.display());
        }
        let store = DocsStore::open(profile_dir)?;
        let bytes = store
            .load_snapshot(REGISTRY_DOC_ID)?
            .context("profile has no registry snapshot")?;
        let device: String = serde_json::from_slice::<serde_json::Value>(&bytes)?["deviceId"]
            .as_str()
            .context("registry snapshot has no deviceId")?
            .into();
        let mut registry = RegistryDoc::from_bytes(&bytes, &device)?;
        // Fold writes straight into rows, like the engine does for a profile
        // without a sync transport.
        registry.set_local_only(true);
        let profile = Self {
            data_dir: data_dir.into(),
            dir: profile_dir.into(),
            store,
            registry,
            device,
            dirty: false,
            lock,
        };
        let version = profile.zeron_version()?;
        if !ignore_version && version.as_deref() != Some(ZERON_VERSION) {
            bail!(
                "this build targets Zeron {ZERON_VERSION}, but the profile was last opened by Zeron {}; \
                 sync zeron-tools with that release (see AGENTS.md) or pass --ignore-version",
                version.as_deref().unwrap_or("<unknown>")
            );
        }
        Ok(profile)
    }

    /// The version this device's engine last booted with.
    pub fn zeron_version(&self) -> Result<Option<String>> {
        Ok(self
            .registry
            .read_devices()?
            .into_iter()
            .find(|d| d.id == self.device)
            .and_then(|d| d.version))
    }

    /// Copy `docs.sqlite3` into `dest/docs.sqlite3` (consistent, WAL included).
    pub fn backup(&self, dest: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dest)?;
        let target = dest.join("docs.sqlite3");
        if target.exists() {
            bail!("{} already exists", target.display());
        }
        let conn = rusqlite::Connection::open(self.dir.join("docs.sqlite3"))?;
        conn.execute("VACUUM INTO ?1", [target.to_string_lossy()])?;
        Ok(target)
    }

    pub fn chats(&self) -> Result<Vec<Chat>> {
        Ok(self.registry.read_chats()?)
    }

    pub fn spaces(&self) -> Result<Vec<Space>> {
        Ok(self.registry.read_spaces()?)
    }

    /// The chat already resuming `session_id`, if any.
    pub fn linked_chat(&self, session_id: &str) -> Result<Option<Chat>> {
        Ok(self
            .chats()?
            .into_iter()
            .find(|c| c.harness_session_id.as_deref() == Some(session_id)))
    }

    /// The transcript Zeron shows for `chat_id` (continuations not joined).
    pub fn transcript(&self, chat_id: &str) -> Result<Vec<SessionMessageEntry>> {
        let Some(bytes) = self.store.load_snapshot(chat_id)? else {
            return Ok(vec![]);
        };
        let doc = loro::LoroDoc::new();
        doc.import(&bytes)?;
        Ok(SessionDoc::from_doc(doc).read_entries()?)
    }

    /// Write `session` as a new chat. The chat doc is saved immediately; the
    /// registry row is persisted by [`Self::save`]. With `create_project`, a
    /// cwd that matches no project becomes one (Zeron stamps its git fields
    /// on next boot); otherwise the chat lands under "No project".
    pub fn import(
        &mut self,
        session: &Session,
        title: Option<&str>,
        create_project: bool,
    ) -> Result<Imported> {
        if self.lock.is_none() {
            bail!("profile was opened read-only");
        }
        if let Some(chat) = self.linked_chat(&session.id)? {
            bail!(
                "session {} is already linked to chat {}",
                session.id,
                chat.id
            );
        }
        let cwd = session.cwd.to_string_lossy().into_owned();
        let mut created_space = false;
        let mut space_id = self
            .spaces()?
            .into_iter()
            .find(|s| s.device_id == self.device && Path::new(&s.path) == session.cwd)
            .map(|s| s.id);
        let cwd_missing = !session.cwd.is_dir();
        if space_id.is_none() && create_project && !cwd_missing {
            let space = Space {
                id: uuid::Uuid::new_v4().to_string(),
                device_id: self.device.clone(),
                path: cwd.clone(),
                name: None,
                git_detected: false,
                git_checked_at: None,
                checkout_id: None,
                repository_id: None,
                created_at: at(session.created_ms),
            };
            self.registry.upsert_space(&space)?;
            space_id = Some(space.id);
            created_space = true;
        }

        // Zeron passes the chat's model to the harness on resume; a retired
        // one would fail the turn (or, for Codex, the resume itself).
        let (model, dropped_model) = match (&session.model, self.model_catalog(session.harness)) {
            (Some(m), Some(known)) if !known.contains(m) => (None, Some(m.clone())),
            (m, _) => (m.clone(), None),
        };
        let chat_id = uuid::Uuid::new_v4().to_string();
        let doc = SessionDoc::init(&chat_id)?;
        for message in &session.messages {
            doc.push_message(&self.entry(message))?;
        }
        let bytes = doc.export_snapshot()?;
        // Read it back the way the engine will before committing anything.
        let reread = loro::LoroDoc::new();
        reread.import(&bytes)?;
        let count = SessionDoc::from_doc(reread).read_entries()?.len();
        if count != session.messages.len() {
            bail!(
                "round-trip mismatch: wrote {} messages, read {count}",
                session.messages.len()
            );
        }
        self.store
            .save_snapshot_with_cursor(&chat_id, &bytes, 0, CHAT2_DOC_EPOCH)?;

        let preview = session
            .messages
            .iter()
            .rev()
            .map(|m| m.text())
            .find(|t| !t.trim().is_empty())
            .map(|t| t.chars().take(120).collect());
        self.registry.upsert_chat(&Chat {
            id: chat_id.clone(),
            device_id: self.device.clone(),
            title: title.map(str::to_owned).or_else(|| session.name.clone()),
            archived: session.archived,
            // Zeron renders project-less cwds home-relative.
            cwd: Some(
                match (
                    &space_id,
                    cwd.strip_prefix(&*crate::home().to_string_lossy()),
                ) {
                    (None, Some(rest)) => format!("~{rest}"),
                    _ => cwd.clone(),
                },
            ),
            branch: None,
            checkout_id: None,
            source_context: None,
            config: Some(ChatConfig {
                harness: session.harness,
                model: model.clone(),
                reasoning: session.reasoning,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            last_message_preview: preview,
            last_message_at: Some(at(session.last_ms)),
            created_at: at(session.created_ms),
            harness_session_id: Some(session.id.clone()),
            harness_session_cwd: Some(cwd),
            space_id: space_id.clone(),
            // Imported history is not "unread".
            last_seen_at: Some(at(session.last_ms)),
            room_gen: Some(CHAT2_ROOM_GEN),
            parent_chat_id: None,
        })?;
        self.dirty = true;
        Ok(Imported {
            chat_id,
            space_id,
            created_space,
            messages: session.messages.len(),
            dropped_model,
            cwd_missing,
        })
    }

    /// Model ids Zeron last listed for `harness` (its catalog cache, newest
    /// fetch wins); `None` when Zeron has cached none.
    fn model_catalog(&self, harness: HarnessId) -> Option<HashSet<String>> {
        // `zeron_engine::model_catalogs::Saved` (the engine isn't a dependency).
        #[derive(serde::Deserialize)]
        struct Saved {
            schema_version: u32,
            fetched_at: u64,
            catalog: Vec<Model>,
        }
        let name = serde_json::to_value(harness).ok()?.as_str()?.to_owned();
        std::fs::read_dir(self.data_dir.join("model-catalogs").join(name))
            .ok()?
            .filter_map(|e| {
                serde_json::from_slice::<Saved>(&std::fs::read(e.ok()?.path()).ok()?).ok()
            })
            .filter(|s| s.schema_version == 1 && !s.catalog.is_empty())
            .max_by_key(|s| s.fetched_at)
            .map(|s| s.catalog.into_iter().map(|m| m.id).collect())
    }

    pub fn rename(&mut self, chat_id: &str, title: &str) -> Result<bool> {
        let renamed = self.registry.rename_chat(chat_id, title)?;
        self.dirty |= renamed;
        Ok(renamed)
    }

    /// Persist registry changes.
    pub fn save(&mut self) -> Result<()> {
        if self.dirty {
            if self.lock.is_none() {
                bail!("profile was opened read-only");
            }
            self.store
                .save_snapshot(REGISTRY_DOC_ID, &self.registry.to_bytes()?)?;
            self.dirty = false;
        }
        Ok(())
    }

    fn entry(&self, m: &crate::sources::Message) -> SessionMessageEntry {
        SessionMessageEntry {
            id: uuid::Uuid::new_v4().to_string(),
            role: m.role,
            // Zeron's render-parts policy: tool inputs never enter the doc.
            parts: m
                .parts
                .iter()
                .map(|p| match p {
                    MessagePart::Tool { call, .. } => {
                        let mut p = p.clone();
                        if let MessagePart::Tool { call: slot, .. } = &mut p {
                            *slot = sanitize_tool_call(call);
                        }
                        p
                    }
                    other => other.clone(),
                })
                .collect(),
            created_at: m.created_ms,
            device_id: self.device.clone(),
            status: Some(if m.aborted {
                MessageStatus::Aborted
            } else {
                MessageStatus::Complete
            }),
            continuation_of: None,
            duration_ms: m.duration_ms,
        }
    }
}

/// Take Zeron's instance lock (`engine.lock`, the flock the engine holds while
/// running), so Zeron can't run, or start, while we write.
fn lock(data_dir: &Path) -> Result<File> {
    let path = data_dir.join("engine.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!(
                "Zeron is running (it holds {}); quit it first",
                path.display()
            );
        }
    }
    Ok(file)
}

fn at(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::Device;

    const DEVICE: &str = "dev-test";

    /// A data dir holding a minimal local profile last booted by `version`.
    fn data_dir(version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/local");
        std::fs::create_dir_all(&profile).unwrap();
        let mut registry = RegistryDoc::new(DEVICE);
        registry.set_local_only(true);
        registry
            .upsert_device(&Device {
                id: DEVICE.into(),
                name: "test".into(),
                platform: "macos".into(),
                last_seen_at: None,
                created_at: None,
                version: Some(version.into()),
                cursor_sdk_version: None,
                capabilities: vec![],
            })
            .unwrap();
        DocsStore::open(&profile)
            .unwrap()
            .save_snapshot(REGISTRY_DOC_ID, &registry.to_bytes().unwrap())
            .unwrap();
        dir
    }

    fn open(dir: &Path, write: bool) -> Result<Profile> {
        Profile::open(dir, &dir.join("profiles/local"), write, false)
    }

    fn fixture() -> Session {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-session.jsonl");
        crate::sources::pi::parse(&path).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_then_rename_survives_reopen() {
        let dir = data_dir(ZERON_VERSION);
        let mut session = fixture();
        // The fixture's folder doesn't exist: never a project, but still imported.
        let mut missing = session.clone();
        missing.id = "01a0aaaa-0000-7000-8000-00000000000f".into();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        session.cwd = repo.clone();
        let chat_id = {
            let mut profile = open(dir.path(), true).unwrap();
            let gone = profile.import(&missing, None, true).unwrap();
            assert!(gone.cwd_missing && !gone.created_space && gone.space_id.is_none());
            let done = profile.import(&session, None, true).unwrap();
            assert!(done.created_space && !done.cwd_missing);
            assert!(
                profile.import(&session, None, true).is_err(),
                "double import must fail"
            );
            assert!(profile.rename(&done.chat_id, "Renamed").unwrap());
            profile.save().unwrap();
            done.chat_id
        };

        let profile = open(dir.path(), false).unwrap();
        let chat = profile.linked_chat(&session.id).unwrap().expect("chat row");
        assert_eq!(chat.id, chat_id);
        assert_eq!(chat.title.as_deref(), Some("Renamed"));
        assert_eq!(chat.harness_session_cwd.as_deref(), repo.to_str());
        assert_eq!(chat.room_gen, Some(CHAT2_ROOM_GEN));
        let space = profile.spaces().unwrap().pop().expect("created project");
        assert_eq!(chat.space_id.as_ref(), Some(&space.id));
        assert_eq!(
            profile.transcript(&chat_id).unwrap().len(),
            session.messages.len()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn models_zeron_no_longer_offers_are_dropped() {
        let dir = data_dir(ZERON_VERSION);
        let catalogs = dir.path().join("model-catalogs/pi");
        std::fs::create_dir_all(&catalogs).unwrap();
        let catalog = |fetched_at: u64, id: &str| {
            serde_json::json!({"schema_version": 1, "fetched_at": fetched_at, "binary_version": null,
                "catalog": [{"id": id, "label": id, "reasoningLevels": [], "options": []}]})
            .to_string()
        };
        // The newest fetch wins: the fixture's model was listed only before.
        std::fs::write(
            catalogs.join("old.json"),
            catalog(1, "anthropic/claude-opus-5-5"),
        )
        .unwrap();
        std::fs::write(
            catalogs.join("new.json"),
            catalog(2, "anthropic/claude-opus-6"),
        )
        .unwrap();
        let mut profile = open(dir.path(), true).unwrap();
        let done = profile.import(&fixture(), None, false).unwrap();
        assert_eq!(
            done.dropped_model.as_deref(),
            Some("anthropic/claude-opus-5-5")
        );
        let chat = profile.linked_chat(&fixture().id).unwrap().unwrap();
        assert_eq!(chat.config.unwrap().model, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_need_zeron_quit_and_matching_version() {
        let dir = data_dir(ZERON_VERSION);
        let _zeron = lock(dir.path()).unwrap();
        assert!(
            open(dir.path(), true).is_err(),
            "lock held by a running Zeron"
        );
        assert!(open(dir.path(), false).is_ok(), "reads don't need the lock");

        let other = data_dir("0.0.1");
        let err = open(other.path(), false).err().expect("version mismatch");
        assert!(err.to_string().contains("0.0.1"));
        assert!(
            Profile::open(
                other.path(),
                &other.path().join("profiles/local"),
                false,
                true
            )
            .is_ok()
        );
    }
}
