//! The user's home folder.

use std::path::PathBuf;

/// The home folder, else `/`.
pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| "/".into())
}
