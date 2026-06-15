//! Filesystem discovery for memory entries.
//!
//! The default location is `~/.scode/memory/`, but `SUDOCODE_MEMORY_DIR`
//! takes precedence when set.

use std::path::{Path, PathBuf};

use super::entry::MemoryEntry;
use super::index::ParsedIndex;

pub const MEMORY_DIR_ENV: &str = "SUDOCODE_MEMORY_DIR";
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

/// Resolve the default memory directory.
///
/// Lookup order:
/// 1. `SUDOCODE_MEMORY_DIR` environment variable (primary override).
/// 2. `$HOME/.scode/memory/`.
/// 3. Relative `.scode/memory/` if neither is available.
#[must_use]
pub fn default_memory_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MEMORY_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".scode").join("memory");
    }
    PathBuf::from(".scode").join("memory")
}

/// Load and parse `MEMORY.md` from the given directory, if present.
pub fn load_index(memory_dir: &Path) -> std::io::Result<Option<ParsedIndex>> {
    let path = memory_dir.join(MEMORY_INDEX_FILE);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let mut parsed = ParsedIndex::parse(raw);
            parsed.path = Some(path);
            Ok(Some(parsed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Discover memory entries in `memory_dir`. Skips `MEMORY.md`, hidden files,
/// and non-`.md` files. Returns entries sorted by `name` for determinism.
///
/// Files that fail to parse are skipped — a corrupt entry should not break
/// loading the rest of the memory store.
pub fn load_entries(memory_dir: &Path) -> std::io::Result<Vec<MemoryEntry>> {
    let mut entries = Vec::new();
    let read_dir = match std::fs::read_dir(memory_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(err) => return Err(err),
    };

    for dir_entry in read_dir {
        let dir_entry = dir_entry?;
        let path = dir_entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if name.eq_ignore_ascii_case(MEMORY_INDEX_FILE) {
            continue;
        }
        if !name.to_ascii_lowercase().ends_with(".md") {
            continue;
        }
        if let Ok(entry) = MemoryEntry::from_file(&path) {
            entries.push(entry);
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("runtime-memory-{prefix}-{nanos}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn env_var_overrides_default() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let custom = temp_dir("envvar");
        let prior = std::env::var_os(MEMORY_DIR_ENV);
        std::env::set_var(MEMORY_DIR_ENV, &custom);
        let resolved = default_memory_dir();
        if let Some(value) = prior {
            std::env::set_var(MEMORY_DIR_ENV, value);
        } else {
            std::env::remove_var(MEMORY_DIR_ENV);
        }
        assert_eq!(resolved, custom);
        fs::remove_dir_all(custom).ok();
    }

    #[test]
    fn falls_back_to_home_when_env_missing() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior_env = std::env::var_os(MEMORY_DIR_ENV);
        let prior_home = std::env::var_os("HOME");
        std::env::remove_var(MEMORY_DIR_ENV);
        std::env::set_var("HOME", "/tmp/sudocode-test-home");
        let resolved = default_memory_dir();
        if let Some(value) = prior_env {
            std::env::set_var(MEMORY_DIR_ENV, value);
        } else {
            std::env::remove_var(MEMORY_DIR_ENV);
        }
        if let Some(value) = prior_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        assert_eq!(
            resolved,
            PathBuf::from("/tmp/sudocode-test-home/.scode/memory")
        );
    }

    #[test]
    fn load_entries_skips_index_hidden_and_non_md() {
        let dir = temp_dir("entries");
        fs::write(dir.join("MEMORY.md"), "# Key Learnings\n").unwrap();
        fs::write(dir.join(".hidden.md"), "ignored").unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();
        fs::write(
            dir.join("good.md"),
            "---\nname: good\ndescription: a good entry\nmetadata:\n  type: user\n---\nbody\n",
        )
        .unwrap();
        let entries = load_entries(&dir).expect("load entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "good");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_index_returns_none_when_missing() {
        let dir = temp_dir("noindex");
        assert!(load_index(&dir).expect("load").is_none());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_entries_handles_missing_dir() {
        let dir = std::env::temp_dir().join("runtime-memory-does-not-exist-xyz");
        // Ensure it really doesn't exist.
        fs::remove_dir_all(&dir).ok();
        let entries = load_entries(&dir).expect("missing dir is empty");
        assert!(entries.is_empty());
    }
}
