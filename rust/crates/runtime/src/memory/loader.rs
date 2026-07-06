//! Filesystem discovery for memory entries.
//!
//! The default location is `~/.scode/projects/<slug>/memory/`, where
//! `<slug>` is derived from the git root (or cwd). `SUDOCODE_MEMORY_DIR`
//! takes precedence when set.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::entry::MemoryEntry;
use super::index::ParsedIndex;

pub const MEMORY_DIR_ENV: &str = "SUDOCODE_MEMORY_DIR";
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

/// Resolve the default memory directory for a given working directory.
///
/// Lookup order:
/// 1. `SUDOCODE_MEMORY_DIR` environment variable (primary override).
/// 2. `~/.scode/projects/<slug>/memory/` where slug is derived from
///    the git root (or `cwd` if not in a git repo).
/// 3. Relative `.scode/projects/<slug>/memory/` if `$HOME` is unavailable.
#[must_use]
pub fn default_memory_dir_for(cwd: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os(MEMORY_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let base = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let slug = sanitize_path(&base.to_string_lossy());
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".scode")
            .join("projects")
            .join(&slug)
            .join("memory");
    }
    PathBuf::from(".scode")
        .join("projects")
        .join(&slug)
        .join("memory")
}

/// Resolve the default memory directory using the process's current
/// working directory. Convenience wrapper around [`default_memory_dir_for`].
#[must_use]
pub fn default_memory_dir() -> PathBuf {
    default_memory_dir_for(&std::env::current_dir().unwrap_or_default())
}

/// Resolve the per-agent-type memory directory for a given working
/// directory + sub-agent type. Path shape:
/// `<workspace-base>/agent-memory/<agent_type>/` where
/// `<workspace-base>` follows the same resolution rules as
/// [`default_memory_dir_for`] MINUS its trailing `memory/` segment.
///
/// Mirrors CC-fork's `agentMemory.ts` scoping (see
/// `~/.claude/projects/<slug>/agent-memory/<agentType>/`) so agent A
/// can `remember X=42` without leaking that into agent B's memory
/// index. Distinct from the workspace-scoped
/// [`default_memory_dir_for`] which every non-subagent turn uses.
///
/// `SUDOCODE_MEMORY_DIR` still wins as a top-level override so tests
/// can pin the base — the agent-type suffix is appended to that
/// override (`<SUDOCODE_MEMORY_DIR>/agent-memory/<agent_type>/`).
///
/// `agent_type` is sanitized identically to workspace slugs so an
/// agent name containing punctuation stays filesystem-safe.
#[must_use]
pub fn agent_memory_dir_for(cwd: &Path, agent_type: &str) -> PathBuf {
    let base = agent_memory_base_dir(cwd);
    let sanitized_agent = sanitize_path(agent_type.trim());
    base.join("agent-memory").join(sanitized_agent)
}

/// Base directory (parent of the trailing `memory/` or
/// `agent-memory/<type>/` segment). Not part of the public API — it
/// exists so [`agent_memory_dir_for`] and [`default_memory_dir_for`]
/// stay consistent when the override or fallback path shifts.
fn agent_memory_base_dir(cwd: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os(MEMORY_DIR_ENV) {
        // The env override targets the workspace memory dir; the
        // agent-memory subdirs live alongside it.
        return PathBuf::from(dir);
    }
    let base = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let slug = sanitize_path(&base.to_string_lossy());
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".scode")
            .join("projects")
            .join(&slug);
    }
    PathBuf::from(".scode").join("projects").join(&slug)
}

/// Ensure the memory directory exists, creating it (and parents) if needed.
/// Errors are silently ignored — a missing directory simply means no
/// memory will be loaded.
pub fn ensure_memory_dir_exists(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
}

/// Find the canonical git root for a directory by running
/// `git rev-parse --show-toplevel`. Returns `None` when outside a
/// git repository or if the command fails.
fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }
    Some(PathBuf::from(root))
}

/// Sanitize a path string for use as a directory name, matching CC's
/// `sessionStoragePortable.ts:sanitizePath`. Non-alphanumeric ASCII
/// chars are replaced with `-`. If the result exceeds 200 chars, it is
/// truncated and a hash suffix is appended for uniqueness.
fn sanitize_path(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.len() <= 200 {
        return sanitized;
    }
    let hash = simple_hash(name);
    format!("{}-{}", &sanitized[..200], hash)
}

/// Port of CC's `simpleHash` from `sessionStoragePortable.ts`.
/// Produces a base-36 string of the absolute value of a Java-style
/// `hashCode` (shift-5 multiply-add).
fn simple_hash(s: &str) -> String {
    let mut hash: i32 = 0;
    for c in s.chars() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(c as i32);
    }
    let abs = (hash as i64).unsigned_abs();
    format_radix_36(abs)
}

/// Format an unsigned 64-bit integer in base 36 (digits 0–9, a–z),
/// matching JavaScript's `Number.prototype.toString(36)`.
fn format_radix_36(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).expect("base36 chars are valid utf8")
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
    fn falls_back_to_project_scoped_path_when_env_missing() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cwd = temp_dir("fallback-cwd");
        let prior_env = std::env::var_os(MEMORY_DIR_ENV);
        let prior_home = std::env::var_os("HOME");
        std::env::remove_var(MEMORY_DIR_ENV);
        std::env::set_var("HOME", "/tmp/sudocode-test-home");
        let resolved = default_memory_dir_for(&cwd);
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
        // The path should be under projects/<slug>/memory/
        let slug = sanitize_path(&cwd.to_string_lossy());
        assert_eq!(
            resolved,
            PathBuf::from("/tmp/sudocode-test-home/.scode/projects")
                .join(&slug)
                .join("memory")
        );
        fs::remove_dir_all(cwd).ok();
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
    fn agent_memory_dir_lives_under_workspace_agent_memory_subdir() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cwd = temp_dir("agent-scoped-cwd");
        let prior_env = std::env::var_os(MEMORY_DIR_ENV);
        let prior_home = std::env::var_os("HOME");
        std::env::remove_var(MEMORY_DIR_ENV);
        std::env::set_var("HOME", "/tmp/sudocode-test-home-agent");

        let a = agent_memory_dir_for(&cwd, "Explore");
        let b = agent_memory_dir_for(&cwd, "general-purpose");

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

        let slug = sanitize_path(&cwd.to_string_lossy());
        let base = PathBuf::from("/tmp/sudocode-test-home-agent/.scode/projects").join(&slug);
        assert_eq!(a, base.join("agent-memory").join("Explore"));
        assert_eq!(b, base.join("agent-memory").join("general-purpose"));
        // Agent-scoped path must NEVER equal the workspace-scoped path.
        assert_ne!(a, base.join("memory"));
        assert_ne!(a, b, "different agent types must produce different dirs");
        fs::remove_dir_all(cwd).ok();
    }

    #[test]
    fn agent_memory_dir_env_override_is_respected_but_still_scopes_by_type() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = temp_dir("agent-scoped-env-override");
        let prior_env = std::env::var_os(MEMORY_DIR_ENV);
        std::env::set_var(MEMORY_DIR_ENV, &base);

        let a = agent_memory_dir_for(Path::new("/does/not/matter"), "Plan");

        if let Some(value) = prior_env {
            std::env::set_var(MEMORY_DIR_ENV, value);
        } else {
            std::env::remove_var(MEMORY_DIR_ENV);
        }

        assert_eq!(a, base.join("agent-memory").join("Plan"));
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn agent_memory_dir_sanitizes_punctuated_agent_names() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = temp_dir("agent-scoped-sanitize");
        let prior_env = std::env::var_os(MEMORY_DIR_ENV);
        std::env::set_var(MEMORY_DIR_ENV, &base);

        let a = agent_memory_dir_for(Path::new("/anywhere"), "my.custom/agent name");

        if let Some(value) = prior_env {
            std::env::set_var(MEMORY_DIR_ENV, value);
        } else {
            std::env::remove_var(MEMORY_DIR_ENV);
        }

        // sanitize_path replaces non-alphanumeric ASCII chars with -,
        // so `.`, `/`, and spaces should all collapse to hyphens.
        let last = a.file_name().expect("has filename");
        assert_eq!(last, std::ffi::OsStr::new("my-custom-agent-name"));
        fs::remove_dir_all(base).ok();
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
