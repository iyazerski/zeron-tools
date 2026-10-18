//! Offline access to a MonoCode install (https://github.com/hardbeat920/monocode).
//!
//! MonoCode keeps chats ("sessions") in `monocode.db`, one row each with its
//! transcript as a JSON list of blocks (`src-tauri/src/session_store.rs`).
//! Its project rail lives in the web view's `localStorage`
//! (`src/features/projects/model/recents.ts`), which WebKit stores in a
//! SQLite file of UTF-16 values. MonoCode caches both while running, so
//! writes need it quit ([`ensure_quit`]).

pub mod blocks;
pub mod push;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The MonoCode release this build reads and writes, as a literal so
/// `--version` can embed it.
macro_rules! monocode_version {
    () => {
        "0.14.0"
    };
}
pub(crate) use monocode_version;

pub const MONOCODE_VERSION: &str = monocode_version!();
/// The newest `schema_migrations` version of [`MONOCODE_VERSION`].
const SCHEMA_VERSION: i64 = 19;

const RECENT_PROJECTS: &str = "monocode.recentProjects";
const RAIL_ORDER: &str = "monocode.projectRailOrder";
const PROJECT_LOCATIONS: &str = "monocode.projectLocations";
const DEFAULT_MODELS: &str = "monocode.defaultModels";

/// Where a MonoCode install keeps its data.
#[derive(Debug, Clone)]
pub struct Paths {
    pub db: PathBuf,
    /// WebKit's `localStorage` file for the app's origin.
    pub local_storage: PathBuf,
}

impl Paths {
    /// The installed app's data: `MONOCODE_DATA_DIR` (else
    /// `~/Library/Application Support/com.monocode.desktop`) and WebKit's
    /// `localStorage` under `~/Library/WebKit/com.monocode.desktop`. `None`
    /// when MonoCode has no database, i.e. isn't installed or never ran.
    pub fn installed() -> Result<Option<Self>> {
        let library = crate::home().join("Library");
        let dir = std::env::var_os("MONOCODE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| library.join("Application Support/com.monocode.desktop"));
        let db = dir.join("monocode.db");
        if !db.is_file() {
            return Ok(None);
        }
        // WebsiteData/Default/<salted origin>/<salted origin>/LocalStorage/
        let root = library.join("WebKit/com.monocode.desktop/WebsiteData/Default");
        let mut found = vec![];
        for a in read_dirs(&root) {
            for b in read_dirs(&a) {
                let file = b.join("LocalStorage/localstorage.sqlite3");
                if file.is_file() {
                    found.push(file);
                }
            }
        }
        // The app has one origin; anything else would be a page it opened.
        let local_storage = match found.as_slice() {
            [one] => one.clone(),
            _ => found
                .into_iter()
                .find(|f| has_monocode_keys(f).unwrap_or(false))
                .with_context(|| format!("no MonoCode localStorage under {}", root.display()))?,
        };
        Ok(Some(Self { db, local_storage }))
    }
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.is_dir())
        .collect()
}

fn has_monocode_keys(file: &Path) -> Result<bool> {
    let conn = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn
        .query_row(
            "SELECT 1 FROM ItemTable WHERE key LIKE 'monocode.%' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Fail if MonoCode is running: it would overwrite our writes from memory.
pub fn ensure_quit() -> Result<()> {
    let running = std::process::Command::new("/usr/bin/pgrep")
        .args(["-x", "monocode"])
        .stdout(std::process::Stdio::null())
        .status()
        .context("running pgrep")?
        .success();
    if running {
        bail!("MonoCode is running; quit it first (Cmd+Q)");
    }
    Ok(())
}

/// A MonoCode chat, without its transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct Chat {
    pub id: String,
    /// The project folder.
    pub cwd: String,
    /// `pi`, `codex`, ...
    pub harness: String,
    /// `<harness>:<model>`, e.g. `pi:anthropic/claude-opus-5-5`.
    pub model: String,
    pub model_settings: Value,
    pub runtime_mode: String,
    /// `<harness label> · <title>`, or just the label while untitled.
    pub title: String,
    /// The harness session it resumes.
    pub provider_session_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
    /// The checkout it runs in, when not the project folder (a worktree).
    pub worktree_cwd: Option<String>,
}

pub struct Monocode {
    db: Connection,
    local: Connection,
    write: bool,
}

/// One project rail entry (`RecentProject` in `recents.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecentProject {
    path: String,
    #[serde(default)]
    opened_at: i64,
}

impl Monocode {
    /// Open MonoCode's stores. Writers must call [`ensure_quit`] first. Fails
    /// (unless `ignore_version`) if the database isn't at the schema of
    /// [`MONOCODE_VERSION`].
    pub fn open(paths: &Paths, write: bool, ignore_version: bool) -> Result<Self> {
        let flags = if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let db = Connection::open_with_flags(&paths.db, flags)
            .with_context(|| format!("opening {}", paths.db.display()))?;
        let local = Connection::open_with_flags(&paths.local_storage, flags)
            .with_context(|| format!("opening {}", paths.local_storage.display()))?;
        let schema: Option<i64> =
            db.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })?;
        if !ignore_version && schema != Some(SCHEMA_VERSION) {
            bail!(
                "this build targets MonoCode {MONOCODE_VERSION} (database schema {SCHEMA_VERSION}), \
                 but the database is at schema {}; update zeron-tools or pass --ignore-version",
                schema.map_or("<none>".into(), |s| s.to_string())
            );
        }
        Ok(Self { db, local, write })
    }

    /// Copy both stores into `dest` (consistent, WAL included).
    pub fn backup(&self, dest: &Path) -> Result<()> {
        std::fs::create_dir_all(dest)?;
        for (conn, name) in [
            (&self.db, "monocode.db"),
            (&self.local, "monocode-localstorage.sqlite3"),
        ] {
            let target = dest.join(name);
            if target.exists() {
                bail!("{} already exists", target.display());
            }
            conn.execute("VACUUM INTO ?1", [target.to_string_lossy()])?;
        }
        Ok(())
    }

    /// Harness sessions any MonoCode chat resumes.
    pub fn resumed_sessions(&self) -> Result<HashSet<String>> {
        let mut stmt = self.db.prepare(
            "SELECT provider_session_id FROM sessions WHERE provider_session_id IS NOT NULL",
        )?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids)
    }

    /// The chats a user sees and can sync: pi and Codex chats with a session
    /// and a sent message, not drafts, automation runs, hidden inbox asks, or
    /// orchestration workers (the filters of `list_by_project` and the
    /// sidebar).
    pub fn chats(&self) -> Result<Vec<Chat>> {
        let mut stmt = self.db.prepare(
            "SELECT id, cwd, harness, model, model_settings, runtime_mode, title,
                    provider_session_id, created_at, updated_at, archived, worktree_cwd
               FROM sessions
              WHERE harness IN ('pi', 'codex')
                AND provider_session_id IS NOT NULL AND provider_session_id != ''
                AND has_user_message = 1 AND is_draft = 0 AND sidebar_hidden = 0
                AND automation_id IS NULL AND inbox_ask IS NULL
                AND id NOT IN (SELECT session_id FROM orchestration_workers)
              ORDER BY created_at",
        )?;
        let chats = stmt
            .query_map([], |r| {
                Ok(Chat {
                    id: r.get(0)?,
                    cwd: r.get(1)?,
                    harness: r.get(2)?,
                    model: r.get(3)?,
                    model_settings: serde_json::from_str(&r.get::<_, String>(4)?)
                        .unwrap_or(Value::Null),
                    runtime_mode: r.get(5)?,
                    title: r.get(6)?,
                    provider_session_id: r.get(7)?,
                    created_at: r.get(8)?,
                    updated_at: r.get(9)?,
                    archived: r.get::<_, i64>(10)? != 0,
                    worktree_cwd: r.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(chats)
    }

    /// A chat's transcript blocks.
    pub fn blocks(&self, chat_id: &str) -> Result<Vec<Value>> {
        let raw: String = self.db.query_row(
            "SELECT blocks_json FROM sessions WHERE id = ?1",
            [chat_id],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn writable(&self) -> Result<()> {
        if !self.write {
            bail!("MonoCode was opened read-only");
        }
        Ok(())
    }

    /// Add a chat with its transcript (as MonoCode's `session_upsert` would,
    /// minus the git branch, which it fills in on the next save).
    pub fn insert(&self, chat: &Chat, blocks: &[Value]) -> Result<()> {
        self.writable()?;
        if !blocks.iter().any(|b| b["role"] == "user") {
            bail!("a MonoCode chat needs a user message");
        }
        self.db.execute(
            "INSERT INTO sessions (id, cwd, harness, model, model_settings, runtime_mode, title,
                                   provider_session_id, blocks_json, created_at, updated_at,
                                   archived, worktree_cwd, has_user_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1)",
            params![
                chat.id,
                chat.cwd,
                chat.harness,
                chat.model,
                chat.model_settings.to_string(),
                chat.runtime_mode,
                chat.title,
                chat.provider_session_id,
                Value::from(blocks.to_vec()).to_string(),
                chat.created_at,
                chat.updated_at,
                chat.archived,
                chat.worktree_cwd,
            ],
        )?;
        Ok(())
    }

    /// Append transcript blocks; the chat's last activity becomes at least
    /// `updated_at`.
    pub fn append(&self, chat_id: &str, new: &[Value], updated_at: i64) -> Result<()> {
        self.writable()?;
        let mut blocks = self.blocks(chat_id)?;
        blocks.extend_from_slice(new);
        self.db.execute(
            "UPDATE sessions SET blocks_json = ?2, updated_at = MAX(updated_at, ?3) WHERE id = ?1",
            params![chat_id, Value::from(blocks).to_string(), updated_at],
        )?;
        Ok(())
    }

    /// Folders on the project rail.
    pub fn projects(&self) -> Result<Vec<String>> {
        Ok(self
            .get_json::<Vec<RecentProject>>(RECENT_PROJECTS)?
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.path)
            .collect())
    }

    /// Put folders on the project rail, as opening them would
    /// (`rememberProject`, plus the rail order and the folder identity
    /// MonoCode uses to find a moved project). `opened` maps each folder to
    /// when it was last used; the rail lists the most recent first.
    pub fn add_projects(&self, opened: &HashMap<String, i64>) -> Result<()> {
        self.writable()?;
        let mut recents: Vec<RecentProject> = self.get_json(RECENT_PROJECTS)?.unwrap_or_default();
        let mut order: Vec<String> = self.get_json(RAIL_ORDER)?.unwrap_or_default();
        let mut locations: serde_json::Map<String, Value> =
            self.get_json(PROJECT_LOCATIONS)?.unwrap_or_default();
        let mut new: Vec<(&String, &i64)> = opened
            .iter()
            .filter(|(path, _)| !recents.iter().any(|r| &r.path == *path))
            .collect();
        if new.is_empty() {
            return Ok(());
        }
        new.sort_by_key(|(_, at)| std::cmp::Reverse(**at));
        for (path, at) in new {
            recents.push(RecentProject {
                path: path.clone(),
                opened_at: *at,
            });
            if !order.contains(path) {
                order.push(path.clone());
            }
            // `fs.rs` `project_identity`.
            if let Ok(meta) = std::fs::metadata(path) {
                use std::os::unix::fs::MetadataExt;
                locations.insert(
                    path.clone(),
                    serde_json::json!({
                        "path": path,
                        "identity": format!("unix:{}:{}", meta.dev(), meta.ino()),
                    }),
                );
            }
        }
        recents.sort_by_key(|r| std::cmp::Reverse(r.opened_at));
        self.set(RECENT_PROJECTS, &serde_json::to_string(&recents)?)?;
        self.set(RAIL_ORDER, &serde_json::to_string(&order)?)?;
        self.set(PROJECT_LOCATIONS, &Value::Object(locations).to_string())?;
        Ok(())
    }

    /// The model new chats of `harness` default to (`<harness>:<model>`).
    pub fn default_model(&self, harness: &str) -> Result<Option<String>> {
        Ok(self
            .get_json::<HashMap<String, String>>(DEFAULT_MODELS)?
            .and_then(|mut m| m.remove(harness)))
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.get(key)?
            .map(|raw| serde_json::from_str(&raw).with_context(|| format!("parsing {key}")))
            .transpose()
    }

    /// WebKit stores `localStorage` values as UTF-16LE blobs.
    fn get(&self, key: &str) -> Result<Option<String>> {
        let blob: Option<Vec<u8>> = self
            .local
            .query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        blob.map(|b| {
            let units: Vec<u16> = b
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16(&units).with_context(|| format!("decoding {key}"))
        })
        .transpose()
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let blob: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
        self.local.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            params![key, blob],
        )?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An empty MonoCode install at the targeted schema (the tables and
    /// columns this module touches, as in `session_store.rs`).
    pub fn install(dir: &Path) -> Paths {
        let paths = Paths {
            db: dir.join("monocode.db"),
            local_storage: dir.join("localstorage.sqlite3"),
        };
        let db = Connection::open(&paths.db).unwrap();
        db.execute_batch(&format!(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
             INSERT INTO schema_migrations VALUES ({SCHEMA_VERSION}, 0);
             CREATE TABLE sessions (
               id TEXT PRIMARY KEY, cwd TEXT NOT NULL, harness TEXT NOT NULL, model TEXT NOT NULL,
               model_settings TEXT NOT NULL DEFAULT '{{}}', runtime_mode TEXT NOT NULL,
               title TEXT NOT NULL, provider_session_id TEXT, blocks_json TEXT NOT NULL DEFAULT '[]',
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, branch TEXT,
               archived INTEGER NOT NULL DEFAULT 0, worktree_cwd TEXT,
               has_user_message INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
               is_draft INTEGER NOT NULL DEFAULT 0, automation_id TEXT,
               sidebar_hidden INTEGER NOT NULL DEFAULT 0, inbox_ask TEXT);
             CREATE TABLE orchestration_workers (session_id TEXT PRIMARY KEY, lead_id TEXT NOT NULL);"
        ))
        .unwrap();
        Connection::open(&paths.local_storage)
            .unwrap()
            .execute_batch(
                "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB NOT NULL ON CONFLICT FAIL);",
            )
            .unwrap();
        paths
    }

    #[test]
    fn projects_round_trip_through_utf16_local_storage() {
        let dir = tempfile::tempdir().unwrap();
        let paths = install(dir.path());
        let mono = Monocode::open(&paths, true, false).unwrap();
        mono.set(RECENT_PROJECTS, r#"[{"path":"/p/old","openedAt":5}]"#)
            .unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        mono.add_projects(&HashMap::from([(folder.clone(), 9), ("/p/old".into(), 1)]))
            .unwrap();
        assert_eq!(mono.projects().unwrap(), [folder.clone(), "/p/old".into()]);
        let order: Vec<String> = mono.get_json(RAIL_ORDER).unwrap().unwrap();
        assert_eq!(order, std::slice::from_ref(&folder));
        let locations: Value = mono.get_json(PROJECT_LOCATIONS).unwrap().unwrap();
        assert!(
            locations[&folder]["identity"]
                .as_str()
                .unwrap()
                .starts_with("unix:")
        );
    }

    #[test]
    fn other_schemas_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = install(dir.path());
        Connection::open(&paths.db)
            .unwrap()
            .execute("INSERT INTO schema_migrations VALUES (20, 0)", [])
            .unwrap();
        let err = Monocode::open(&paths, false, false).err().unwrap();
        assert!(err.to_string().contains("schema 20"), "{err}");
        assert!(Monocode::open(&paths, false, true).is_ok());
    }
}
