//! The relay's saved state: passkeys, sign-in sessions, and Macs.
//!
//! One JSON file, rewritten whole on each change: there's one user and a
//! handful of records, so a database would be overkill. Sessions and Mac
//! tokens are kept as SHA-256 hashes, so the file alone can't sign anyone in.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use webauthn_rs::prelude::{AuthenticationResult, Passkey};

/// How long a sign-in lasts.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// The saved file's contents.
#[derive(Default, Serialize, Deserialize)]
struct Data {
    /// The WebAuthn user handle; one user, made on first start.
    user: Option<Uuid>,
    #[serde(default)]
    passkeys: Vec<Passkey>,
    #[serde(default)]
    sessions: Vec<SessionRecord>,
    #[serde(default)]
    machines: Vec<MachineRecord>,
}

/// A sign-in session, by its token's hash.
#[derive(Serialize, Deserialize)]
struct SessionRecord {
    hash: String,
    /// Unix seconds.
    expires: u64,
}

/// A Mac, with its connection token's hash.
#[derive(Serialize, Deserialize)]
struct MachineRecord {
    id: String,
    name: String,
    token_hash: String,
}

/// A Mac as the app shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
}

impl From<&MachineRecord> for Machine {
    fn from(record: &MachineRecord) -> Self {
        Self {
            id: record.id.clone(),
            name: record.name.clone(),
        }
    }
}

/// The relay's state, in memory and in `<data dir>/state.json`.
pub struct Store {
    path: PathBuf,
    data: Mutex<Data>,
    rng: SystemRandom,
}

impl Store {
    /// Load the state from `<dir>/state.json`, or start empty.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("state.json");
        let mut data: Data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let fresh = data.user.is_none();
        data.user.get_or_insert_with(Uuid::new_v4);
        let store = Self {
            path,
            data: Mutex::new(data),
            rng: SystemRandom::new(),
        };
        if fresh {
            store.save(&store.lock())?;
        }
        Ok(store)
    }

    /// The WebAuthn user handle.
    pub fn user(&self) -> Uuid {
        self.lock()
            .user
            .expect("the user is created when the store opens")
    }

    /// The saved passkeys.
    pub fn passkeys(&self) -> Vec<Passkey> {
        self.lock().passkeys.clone()
    }

    /// Save `passkey`, replacing an older one with its credential id.
    pub fn add_passkey(&self, passkey: Passkey) -> Result<()> {
        let mut data = self.lock();
        data.passkeys.retain(|p| p.cred_id() != passkey.cred_id());
        data.passkeys.push(passkey);
        self.save(&data)
    }

    /// Record a sign-in's new signature counter and backup state.
    pub fn update_passkeys(&self, result: &AuthenticationResult) -> Result<()> {
        let mut data = self.lock();
        let mut changed = false;
        for passkey in &mut data.passkeys {
            changed |= passkey.update_credential(result).unwrap_or(false);
        }
        if changed { self.save(&data) } else { Ok(()) }
    }

    /// Start a session and return its cookie value.
    pub fn new_session(&self) -> Result<String> {
        let token = self.token()?;
        let mut data = self.lock();
        let now = now();
        data.sessions.retain(|s| s.expires > now);
        data.sessions.push(SessionRecord {
            hash: hash(&token),
            expires: now + SESSION_TTL.as_secs(),
        });
        self.save(&data)?;
        Ok(token)
    }

    /// Whether `token` is a live session's cookie value.
    pub fn session_valid(&self, token: &str) -> bool {
        let hash = hash(token);
        let now = now();
        self.lock()
            .sessions
            .iter()
            .any(|s| s.hash == hash && s.expires > now)
    }

    /// End the session with this cookie value.
    pub fn end_session(&self, token: &str) -> Result<()> {
        let hash = hash(token);
        let mut data = self.lock();
        data.sessions.retain(|s| s.hash != hash);
        self.save(&data)
    }

    /// The added Macs.
    pub fn machines(&self) -> Vec<Machine> {
        self.lock().machines.iter().map(Machine::from).collect()
    }

    /// Add a Mac named `name`; return it and its token (shown once).
    pub fn add_machine(&self, name: &str) -> Result<(Machine, String)> {
        let token = self.token()?;
        let machine = Machine {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
        };
        let mut data = self.lock();
        data.machines.push(MachineRecord {
            id: machine.id.clone(),
            name: machine.name.clone(),
            token_hash: hash(&token),
        });
        self.save(&data)?;
        Ok((machine, token))
    }

    /// Forget the Mac `id`; its token stops working.
    pub fn remove_machine(&self, id: &str) -> Result<()> {
        let mut data = self.lock();
        data.machines.retain(|m| m.id != id);
        self.save(&data)
    }

    /// The Mac a connection token belongs to.
    pub fn machine_for_token(&self, token: &str) -> Option<Machine> {
        let hash = hash(token);
        self.lock()
            .machines
            .iter()
            .find(|m| m.token_hash == hash)
            .map(Machine::from)
    }

    /// A fresh 256-bit secret, base64url.
    fn token(&self) -> Result<String> {
        let mut bytes = [0u8; 32];
        self.rng
            .fill(&mut bytes)
            .map_err(|_| anyhow!("generating a token"))?;
        Ok(B64.encode(bytes))
    }

    fn lock(&self) -> MutexGuard<'_, Data> {
        self.data.lock().expect("the store lock is poisoned")
    }

    fn save(&self, data: &Data) -> Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(data)?)
    }
}

/// A token's SHA-256, base64url: what the file keeps instead of the token.
fn hash(token: &str) -> String {
    B64.encode(digest(&SHA256, token.as_bytes()))
}

/// Now, in Unix seconds.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write `bytes` to `path` atomically, readable only by this user.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    // A fresh file, so the mode applies: a leftover one keeps its own.
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", tmp.display()));
        }
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_machine_tokens_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let session = store.new_session().unwrap();
        let (mac, token) = store.add_machine("Work Mac").unwrap();
        let user = store.user();

        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.user(), user);
        assert!(store.session_valid(&session));
        assert!(!store.session_valid("guess"));
        assert_eq!(store.machine_for_token(&token), Some(mac.clone()));
        assert_eq!(store.machine_for_token("guess"), None);

        // Only hashes are saved.
        let saved = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
        assert!(!saved.contains(&session) && !saved.contains(&token));

        store.end_session(&session).unwrap();
        assert!(!store.session_valid(&session));
        store.remove_machine(&mac.id).unwrap();
        assert_eq!(store.machine_for_token(&token), None);
    }
}
