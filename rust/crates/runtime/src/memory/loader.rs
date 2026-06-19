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
