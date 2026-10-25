//! The user's home folder, and paths as people type and read them.

use std::path::{Path, PathBuf};

/// The home folder: `$HOME`, else `/`.
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

/// Expand a leading `~/` to the home folder.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
}

/// `path` with the home folder shown as `~`.
pub fn short_path(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// `program` as an absolute path: itself if it names a path, else its
/// first match on `PATH`.
pub fn which(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_programs_on_path() {
        assert!(which(Path::new("sh")).is_some_and(|p| p.is_absolute()));
        assert_eq!(which(Path::new("/bin/sh")), Some(PathBuf::from("/bin/sh")));
        assert_eq!(which(Path::new("surely-not-a-program")), None);
    }
}
