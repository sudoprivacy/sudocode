//! PTY tests for the persistent memory system (`runtime::memory`).
//!
//! Scenarios exercised end-to-end:
//!
//! 1. Happy path: memory files with valid frontmatter are injected into
//!    the system prompt.
//! 2. Resilience: malformed and non-markdown files are skipped without
//!    crashing; valid entries still surface.
//! 3. Budget enforcement: when total rendered memory exceeds the
//!    16 000-char cap, excess entries are dropped with a notice.
//! 4. Project-scoped memory path: `~/.scode/projects/<slug>/memory/`.
//! 5. Memory directory auto-creation.
//! 6. `/memory` slash command opens instruction files in `$EDITOR`.
//! 7. Write path: model can write to memory directory without prompts.
//!
//! Tests 1–5 use `scode system-prompt` (no API needed).
//! Test 6 uses the REPL with `EDITOR=true` (no API needed).
//! Test 7 requires `SCODE_TEST_BACKEND=live`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-pty-mem-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}

/// Write a valid memory entry file.
fn write_entry(dir: &Path, slug: &str, entry_type: &str, description: &str, body: &str) {
    let content = format!(
        "---\nname: {slug}\ndescription: {description}\nmetadata:\n  type: {entry_type}\n---\n\n{body}\n"
    );
    fs::write(dir.join(format!("{slug}.md")), content).expect("write entry");
}

/// Run `scode system-prompt` with the given env vars and return the output.
fn run_system_prompt(cwd: &Path, envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.current_dir(cwd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.arg("system-prompt");
    cmd.output().expect("scode binary should launch")
}

// ──────────────────────────────────────────────────────────────────────
// 1. Happy path: entries injected into system prompt
// ──────────────────────────────────────────────────────────────────────

/// Create two valid memory files and an index, verify the rendered
/// system prompt contains their content.
#[test]
fn memory_entries_injected_into_system_prompt() {
    let root = unique_temp_dir("inject");
    let memory_dir = root.join("memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    write_entry(
        &memory_dir,
        "feedback_testing",
        "feedback",
        "Testing best practices",
        "Always run tests before committing",
    );
    write_entry(
        &memory_dir,
        "user_role",
        "user",
        "Who the user is",
        "Senior Rust developer",
    );
    fs::write(
        memory_dir.join("MEMORY.md"),
        "# Key Learnings\n\n\
         - [Testing](feedback_testing.md) — testing best practices\n\
         - [Role](user_role.md) — who the user is\n",
    )
    .expect("write MEMORY.md");

    let output = run_system_prompt(
        &root,
        &[("SUDOCODE_MEMORY_DIR", memory_dir.to_str().expect("utf8"))],
    );
    assert!(
        output.status.success(),
        "system-prompt should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = String::from_utf8(output.stdout).expect("stdout utf8");

    // The memory section header must be present.
    assert!(
        text.contains("# Persistent memory"),
        "system-prompt missing '# Persistent memory' section;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // The MEMORY.md index content is passed through.
    assert!(
        text.contains("Key Learnings"),
        "system-prompt missing MEMORY.md index content;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // Both entry names appear.
    assert!(
        text.contains("name: feedback_testing"),
        "system-prompt missing feedback_testing entry;\nstdout tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        text.contains("name: user_role"),
        "system-prompt missing user_role entry;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // Both bodies appear.
    assert!(
        text.contains("Always run tests before committing"),
        "system-prompt missing feedback_testing body;\nstdout tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        text.contains("Senior Rust developer"),
        "system-prompt missing user_role body;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // Type annotations appear.
    assert!(
        text.contains("type: feedback"),
        "system-prompt missing type: feedback;\nstdout tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        text.contains("type: user"),
        "system-prompt missing type: user;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    fs::remove_dir_all(root).ok();
}

// ──────────────────────────────────────────────────────────────────────
// 2. Resilience: malformed / non-markdown files are skipped
// ──────────────────────────────────────────────────────────────────────

/// Mix valid, malformed (unterminated frontmatter), and non-markdown files.
/// The command must exit 0, include the valid entry, and silently skip the rest.
#[test]
fn memory_resilient_on_missing_or_malformed() {
    let root = unique_temp_dir("resilient");
    let memory_dir = root.join("memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    // Valid entry.
    write_entry(
        &memory_dir,
        "valid_entry",
        "project",
        "A valid memory entry",
        "This entry should survive",
    );

    // Malformed entry: missing closing `---`.
    fs::write(
        memory_dir.join("malformed.md"),
        "---\nname: broken\ndescription: bad entry\nmetadata:\n  type: feedback\nThis has no closing delimiter\n",
    )
    .expect("write malformed entry");

    // Non-markdown file: should be ignored entirely.
    fs::write(memory_dir.join("notes.txt"), "plain text, not markdown")
        .expect("write non-markdown file");

    let output = run_system_prompt(
        &root,
        &[("SUDOCODE_MEMORY_DIR", memory_dir.to_str().expect("utf8"))],
    );
    assert!(
        output.status.success(),
        "system-prompt should exit 0 even with malformed files; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = String::from_utf8(output.stdout).expect("stdout utf8");

    // The valid entry must still appear.
    assert!(
        text.contains("name: valid_entry"),
        "valid entry missing after malformed sibling;\nstdout tail:\n{}",
        tail(&text, 40)
    );
    assert!(
        text.contains("This entry should survive"),
        "valid entry body missing after malformed sibling;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // The malformed entry must NOT appear.
    assert!(
        !text.contains("name: broken"),
        "malformed entry should be skipped;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // The non-markdown file must NOT appear.
    assert!(
        !text.contains("plain text, not markdown"),
        "non-markdown file should be ignored;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    fs::remove_dir_all(root).ok();
}

// ──────────────────────────────────────────────────────────────────────
// 3. Budget enforcement: large entries get dropped
// ──────────────────────────────────────────────────────────────────────

/// Create many large memory files whose total exceeds the 16 000-char
/// budget. Verify the output stays within budget and includes a
/// "dropped" notice.
#[test]
fn memory_budget_truncates_large_entries() {
    let root = unique_temp_dir("budget");
    let memory_dir = root.join("memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    // Each entry body is ~1900 chars. With the preamble, index, and
    // per-entry overhead, 12 entries will exceed the 16 000-char cap.
    let large_body = "x".repeat(1_900);
    for i in 0..12 {
        write_entry(
            &memory_dir,
            &format!("entry_{i:02}"),
            "project",
            &format!("Large entry number {i}"),
            &large_body,
        );
    }

    let output = run_system_prompt(
        &root,
        &[("SUDOCODE_MEMORY_DIR", memory_dir.to_str().expect("utf8"))],
    );
    assert!(
        output.status.success(),
        "system-prompt should exit 0 with budget overflow; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let text = String::from_utf8(output.stdout).expect("stdout utf8");

    // The memory section must be present (some entries rendered).
    assert!(
        text.contains("# Persistent memory"),
        "system-prompt missing memory section under budget pressure;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // At least one entry rendered (the first ones fit the budget).
    assert!(
        text.contains("name: entry_"),
        "no entries rendered under budget pressure;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // The budget-drop notice must appear.
    assert!(
        text.contains("dropped"),
        "missing 'dropped' budget notice;\nstdout tail:\n{}",
        tail(&text, 40)
    );

    // The notice mentions the 16000-char budget.
    assert!(
        text.contains("16000-char budget"),
        "budget notice should mention '16000-char budget';\nstdout tail:\n{}",
        tail(&text, 40)
    );

    fs::remove_dir_all(root).ok();
}

// ──────────────────────────────────────────────────────────────────────
// 4. Project-scoped memory path
// ──────────────────────────────────────────────────────────────────────

/// When run inside a git repo without `SUDOCODE_MEMORY_DIR`, the
/// resolved memory path should be project-scoped under
/// `~/.scode/projects/<slug>/memory/`.
#[test]
fn memory_project_scoped_path() {
    // HOME is not set on Windows; the loader falls back to a relative
    // path in that case so we can only verify this on Unix-like systems.
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            eprintln!("skipping memory_project_scoped_path: HOME not set (Windows)");
            return;
        }
    };

    let root = unique_temp_dir("proj-scope");
    fs::create_dir_all(&root).expect("create root");

    // Init a git repo so `find_git_root` succeeds.
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .expect("git init");

    // Run without SUDOCODE_MEMORY_DIR so it falls through to the
    // project-scoped default.
    let output = run_system_prompt(&root, &[]);
    assert!(
        output.status.success(),
        "system-prompt should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The memory directory auto-created should be under projects/<slug>/memory.
    let projects_dir = PathBuf::from(&home).join(".scode").join("projects");
    assert!(
        projects_dir.exists(),
        "~/.scode/projects/ should exist after system-prompt runs"
    );

    // There should be at least one slug directory under projects/.
    let entries: Vec<_> = fs::read_dir(&projects_dir)
        .expect("read projects dir")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !entries.is_empty(),
        "~/.scode/projects/ should have at least one slug directory"
    );

    fs::remove_dir_all(root).ok();
}

// ──────────────────────────────────────────────────────────────────────
// 5. Memory directory auto-creation
// ──────────────────────────────────────────────────────────────────────

/// When `SUDOCODE_MEMORY_DIR` points to a non-existent directory,
/// running `scode system-prompt` should auto-create it.
#[test]
fn memory_directory_auto_created() {
    let root = unique_temp_dir("auto-create");
    fs::create_dir_all(&root).expect("create root");

    let memory_dir = root.join("does-not-exist-yet").join("memory");
    assert!(!memory_dir.exists(), "precondition: dir should not exist");

    let output = run_system_prompt(
        &root,
        &[("SUDOCODE_MEMORY_DIR", memory_dir.to_str().expect("utf8"))],
    );
    assert!(
        output.status.success(),
        "system-prompt should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        memory_dir.exists(),
        "SUDOCODE_MEMORY_DIR should be auto-created after system-prompt runs"
    );

    fs::remove_dir_all(root).ok();
}

// ──────────────────────────────────────────────────────────────────────
// 6. /memory slash command opens instruction files in $EDITOR
// ──────────────────────────────────────────────────────────────────────

/// `/memory` in the REPL should spawn `$EDITOR` on an instruction file
/// and report "Opened memory file". We set `EDITOR=true` (a no-op that
/// exits 0) so the command returns immediately.
///
/// Uses `TestEnv` for auth config, then spawns with `EDITOR=true` via
/// the `spawn_with_editor` helper.
#[test]
fn memory_slash_command_opens_editor() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-slash");
    let root = env.workspace_root().to_path_buf();

    // Pre-create an AGENTS.md so /memory has a file to open.
    fs::write(root.join("AGENTS.md"), "# Project rules\n").expect("write AGENTS.md");

    // Spawn with EDITOR=true so the editor call succeeds immediately.
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));

    // Wait for the REPL prompt.
    sess.expect("❯").expect("should see REPL prompt");

    // Send /memory command.
    sess.send("/memory\r").expect("send /memory");

    // Expect the confirmation message.
    sess.expect("(?i)opened.*memory.*file")
        .expect("should see 'Opened memory file' confirmation");

    // Expect the editor hint.
    sess.expect("(?i)editor").expect("should see editor hint");

    // Wait for the REPL to return to prompt.
    sess.expect("❯").expect("should return to REPL prompt");

    // Clean exit.
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        panic!("scode should exit after /memory: {e}");
    });
    assert_eq!(exit, 0, "/memory flow should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 7. Write path: model writes to memory dir (live only)
// ──────────────────────────────────────────────────────────────────────

/// In live mode, ask the model to remember something. Verify a file
/// appears in the memory directory. Skipped in mock mode.
#[test]
fn memory_write_path_live() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-write");
    if env.is_mock() {
        eprintln!("skipping memory_write_path_live: mock mode (run with SCODE_TEST_BACKEND=live)");
        return;
    }

    let root = env.workspace_root().to_path_buf();

    // Init a git repo so the memory dir is deterministic.
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .expect("git init");

    let prompt = env.prompt(
        "Remember this: my favorite language is Rust. Save it to memory now.",
        "memory_write",
    );

    let mut sess = env.spawn(&["--permission-mode", "auto", &prompt]);
    sess.set_default_timeout(Duration::from_secs(60));

    // Wait for the model to finish (tool call + response).
    sess.expect("(?i)(write_file|saved|remembered|memory)")
        .expect("should see memory write activity");

    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("scode should exit after write: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "memory write should exit 0; got {exit}");

    // Verify a .md file was created in the memory directory.
    let home = std::env::var("HOME").unwrap_or_default();
    let projects_dir = PathBuf::from(&home).join(".scode").join("projects");
    if projects_dir.exists() {
        let has_memory_file = walkdir(&projects_dir, "memory", ".md");
        assert!(
            has_memory_file,
            "expected a .md file under ~/.scode/projects/*/memory/ after model write"
        );
    }
}

/// Recursively check if a directory tree contains a file matching the criteria.
fn walkdir(dir: &Path, subdir_name: &str, extension: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == subdir_name) {
                // Check for files with the given extension in this subdir.
                if let Ok(sub_entries) = fs::read_dir(&path) {
                    for sub in sub_entries.flatten() {
                        if sub.path().extension().is_some_and(|e| e == &extension[1..]) {
                            return true;
                        }
                    }
                }
            }
            if walkdir(&path, subdir_name, extension) {
                return true;
            }
        }
    }
    false
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Return the last `n` lines of `s`, useful for assertion messages.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
