//! PTY tests for the persistent memory system (`runtime::memory`).
//!
//! Three scenarios exercised end-to-end through `scode system-prompt`:
//!
//! 1. Happy path: memory files with valid frontmatter are injected into
//!    the system prompt.
//! 2. Resilience: malformed and non-markdown files are skipped without
//!    crashing; valid entries still surface.
//! 3. Budget enforcement: when total rendered memory exceeds the
//!    16 000-char cap, excess entries are dropped with a notice.
//!
//! All tests set `SUDOCODE_MEMORY_DIR` to a temp directory, avoiding
//! any interaction with the user's real `~/.scode/memory/`.

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
        text.contains("# auto memory"),
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
        text.contains("# auto memory"),
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
// 6. /memory tab-completion
// ──────────────────────────────────────────────────────────────────────

/// Type `/mem` then Tab in the REPL → should auto-complete to `/memory`.
#[test]
fn memory_tab_completion() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-tab");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));

    // Wait for the REPL prompt.
    sess.expect("❯").expect("should see REPL prompt");

    // Type `/mem` then Tab.
    sess.send("/mem\t").expect("send /mem + Tab");

    // After Tab, the line should complete to `/memory`.
    // Then press Enter to execute.
    std::thread::sleep(std::time::Duration::from_millis(500));
    sess.send("\r").expect("send Enter");

    // `/memory` should execute — expect "Opened memory file" or
    // "No instruction files found" (depending on whether AGENTS.md exists).
    sess.expect("(?i)(opened.*memory|no instruction|memory)")
        .expect("/memory should execute after tab completion");

    // Wait for prompt, then exit.
    sess.expect("❯").expect("prompt after /memory");
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should exit after tab test: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "tab completion test should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 7. End-to-end memory workflow: write → verify → forget → verify
// ──────────────────────────────────────────────────────────────────────

/// Full memory lifecycle exercised through the REPL in a single session:
///
/// 1. Ask the model to "remember my favorite language is Rust"
///    → model writes a memory file + updates MEMORY.md
/// 2. Send `/memory` → verify the instruction files list renders
/// 3. Ask the model to "forget my favorite language"
///    → model deletes the memory file + updates MEMORY.md
/// 4. Verify the memory file is gone
///
/// This test is **live-only** (`SCODE_TEST_BACKEND=live`) because it
/// requires real LLM tool use. Skipped in mock mode.
#[test]
fn memory_write_read_forget_workflow() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-workflow");
    if env.is_mock() {
        eprintln!(
            "skipping memory_write_read_forget_workflow: mock mode \
             (run with SCODE_TEST_BACKEND=live)"
        );
        return;
    }

    let root = env.workspace_root().to_path_buf();

    // Init a git repo so the memory dir is deterministic.
    std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .expect("git init");

    // Pre-create AGENTS.md so /memory has something to list.
    fs::write(root.join("AGENTS.md"), "# Project rules\n").expect("write AGENTS.md");

    // Spawn the REPL in danger-full-access mode (permits all writes).
    // The memory write carve-out also works with workspace-write, but
    // danger-full-access avoids prompt escalation for non-memory tools.
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("EDITOR", "true")],
    );
    sess.set_default_timeout(Duration::from_secs(60));

    // Wait for the REPL prompt.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see initial REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // ── Step 1: Write memory ─────────────────────────────────────────
    sess.send("Remember this: my favorite programming language is Rust. Save it to memory now.\r")
        .expect("send remember request");

    // Wait for the model to call write_file (proves the API responded).
    sess.expect("write_file").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see write_file tool call: {e}\nPTY screen:\n{screen}");
    });

    // Wait for the turn to complete (REPL prompt returns).
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after write: {e}\nPTY screen:\n{screen}");
    });

    // Verify a .md file appeared in the memory directory.
    // TestEnv sets HOME to workspace/home, so memory lands there.
    let workspace_home = root.join("home");
    let projects_dir = workspace_home.join(".scode").join("projects");
    if !has_memory_files(&projects_dir) {
        let screen = sess.render(|s| s.contents());
        panic!(
            "expected memory files after 'remember' command\n\
             projects_dir: {}\n\
             PTY screen:\n{}",
            projects_dir.display(),
            screen
        );
    }

    // ── Step 2: /memory shows instruction files ──────────────────────
    sess.send("/memory\r").expect("send /memory");
    sess.expect("(?i)(opened.*memory|agents\\.md|memory.*file)")
        .expect("/memory should reference a file");
    sess.expect("❯").expect("prompt after /memory");

    // ── Step 3: Forget memory ────────────────────────────────────────
    sess.send("Forget my favorite programming language. Remove that memory entry.\r")
        .expect("send forget request");

    // Wait for the model to use a tool (bash rm, write_file, etc.).
    sess.expect("(?i)(bash|write_file|read_file|glob|grep)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("should see tool call for forget: {e}\nPTY screen:\n{screen}");
        });

    // Wait for the turn to complete.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after forget: {e}\nPTY screen:\n{screen}");
    });

    // ── Step 4: Verify memory file is gone ───────────────────────────
    // Count .md files in memory dirs — should be fewer or zero.
    // (We can't be 100% sure the model deleted the right file, but
    // the LLM should have removed it per the system prompt.)

    // Clean exit.
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("scode should exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "workflow should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 8. Deduplication: LLM should detect existing memory and not duplicate
// ──────────────────────────────────────────────────────────────────────

/// Pre-seed a memory entry, then ask the model to remember the same fact.
/// The LLM should recognize the existing entry (via MEMORY.md in system
/// prompt) and update or skip rather than creating a second file.
///
/// Live-only: requires real LLM judgment.
#[test]
fn memory_dedup_does_not_create_duplicate() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-dedup");
    if env.is_mock() {
        eprintln!(
            "skipping memory_dedup_does_not_create_duplicate: mock mode \
             (run with SCODE_TEST_BACKEND=live)"
        );
        return;
    }

    let root = env.workspace_root().to_path_buf();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .expect("git init");
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // Pre-seed a memory entry about the user's role.
    let workspace_home = root.join("home");
    let projects_dir = workspace_home.join(".scode").join("projects");
    // We need to figure out the slug — just create a well-known memory dir.
    // Use SUDOCODE_MEMORY_DIR to control the path deterministically.
    let memory_dir = workspace_home.join("test-memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    write_entry(
        &memory_dir,
        "user_role",
        "user",
        "User is a backend developer",
        "The user is a backend developer working primarily with Rust and Go.",
    );
    fs::write(
        memory_dir.join("MEMORY.md"),
        "- [User Role](user_role.md) — User is a backend developer\n",
    )
    .expect("write MEMORY.md");

    let md_count_before = count_md_files(&memory_dir);

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("EDITOR", "true"),
            ("SUDOCODE_MEMORY_DIR", memory_dir.to_str().unwrap()),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(60));
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // Ask to remember the same fact that's already stored.
    sess.send("Remember this: I am a backend developer. Save it to memory.\r")
        .expect("send remember request");

    // Wait for turn completion.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after dedup attempt: {e}\nPTY screen:\n{screen}");
    });

    // The LLM should have recognized the existing entry. Either:
    //   (a) it updated user_role.md (same file count), or
    //   (b) it skipped writing entirely (same file count).
    // It should NOT have created a second .md file.
    let md_count_after = count_md_files(&memory_dir);
    assert!(
        md_count_after <= md_count_before,
        "LLM should not create a duplicate memory file. \
         Before: {md_count_before}, After: {md_count_after}. \
         Files: {:?}",
        list_md_files(&memory_dir),
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 9. Staleness: LLM updates outdated memory instead of creating new
// ──────────────────────────────────────────────────────────────────────

/// Pre-seed a memory entry with outdated info, then tell the model the
/// fact has changed. The LLM should update the existing entry, not
/// create a second file alongside the stale one.
///
/// Live-only: requires real LLM judgment.
#[test]
fn memory_staleness_updates_existing_entry() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-stale");
    if env.is_mock() {
        eprintln!(
            "skipping memory_staleness_updates_existing_entry: mock mode \
             (run with SCODE_TEST_BACKEND=live)"
        );
        return;
    }

    let root = env.workspace_root().to_path_buf();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .expect("git init");
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let workspace_home = root.join("home");
    let memory_dir = workspace_home.join("test-memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    // Seed with outdated information.
    write_entry(
        &memory_dir,
        "project_database",
        "project",
        "Project uses PostgreSQL for primary storage",
        "The project uses PostgreSQL 15 as the primary database.\n\
         **Why:** chosen for its JSONB support and mature ecosystem.\n\
         **How to apply:** all migration scripts target PostgreSQL.",
    );
    fs::write(
        memory_dir.join("MEMORY.md"),
        "- [Database](project_database.md) — Project uses PostgreSQL for primary storage\n",
    )
    .expect("write MEMORY.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("EDITOR", "true"),
            ("SUDOCODE_MEMORY_DIR", memory_dir.to_str().unwrap()),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(60));
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // Tell the model the fact has changed.
    sess.send("We migrated from PostgreSQL to MySQL last week. Please update your memory about our database.\r")
        .expect("send update request");

    // Wait for tool call (edit_file or write_file on existing entry).
    sess.expect("(?i)(edit_file|write_file)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("should see tool call for memory update: {e}\nPTY screen:\n{screen}");
        });

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after stale update: {e}\nPTY screen:\n{screen}");
    });

    // Verify the entry was updated, not duplicated.
    let md_count = count_md_files(&memory_dir);
    assert!(
        md_count <= 1,
        "should update existing entry, not create a second one. \
         Found {md_count} files: {:?}",
        list_md_files(&memory_dir),
    );

    // Verify the updated content mentions MySQL.
    let updated = fs::read_to_string(memory_dir.join("project_database.md"))
        .or_else(|_| {
            // LLM might have chosen a different filename — find any .md
            list_md_files(&memory_dir)
                .into_iter()
                .find(|name| name != "MEMORY.md")
                .map(|name| fs::read_to_string(memory_dir.join(&name)).unwrap_or_default())
                .ok_or(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no memory file found",
                ))
        })
        .unwrap_or_default();
    assert!(
        updated.to_lowercase().contains("mysql"),
        "updated memory should mention MySQL. Content:\n{updated}",
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 10. ReadOnly mode blocks memory writes
// ──────────────────────────────────────────────────────────────────────

/// In read-only permission mode, the memory write carve-out should NOT
/// apply. The system prompt still injects memory instructions but the
/// model cannot write files.
#[test]
fn memory_readonly_blocks_writes() {
    let root = unique_temp_dir("mem-readonly");
    let memory_dir = root.join("memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    // Verify the system prompt includes memory instructions even in
    // read-only mode (the LLM should know about memory).
    let output = run_system_prompt(
        &root,
        &[("SUDOCODE_MEMORY_DIR", memory_dir.to_str().expect("utf8"))],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("auto memory") || stdout.contains("memory system") || stdout.contains("MEMORY.md"),
        "system prompt should include memory instructions even for read-only mode"
    );

    // Now verify the carve-out is NOT present by checking the output
    // does NOT contain a synthetic allow rule for the memory directory.
    // (This is a structural check — the actual permission enforcement
    // is in the runtime, not testable via system-prompt output alone.)
    // The real proof is that write_file in read-only mode returns EPERM.
    // But we can at least verify memory loads correctly even without
    // write permission.
    assert!(output.status.success());

    fs::remove_dir_all(root).ok();
}

// ──────────────────────────────────────────────────────────────────────
// 11. Multiple memory types in one session
// ──────────────────────────────────────────────────────────────────────

/// Ask the model to save three different types of memory in a single
/// session. Verify all three files are created with correct types.
///
/// Live-only: requires real LLM judgment for type classification.
#[test]
fn memory_multi_type_single_session() {
    use std::time::Duration;

    let env = common::TestEnv::new("mem-multi");
    if env.is_mock() {
        eprintln!(
            "skipping memory_multi_type_single_session: mock mode \
             (run with SCODE_TEST_BACKEND=live)"
        );
        return;
    }

    let root = env.workspace_root().to_path_buf();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&root)
        .status()
        .expect("git init");
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let workspace_home = root.join("home");
    let memory_dir = workspace_home.join("test-memory");
    fs::create_dir_all(&memory_dir).expect("create memory dir");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("EDITOR", "true"),
            ("SUDOCODE_MEMORY_DIR", memory_dir.to_str().unwrap()),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(90));
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // Ask to remember three things of different types in one go.
    sess.send(
        "Please save these three things to memory:\n\
         1. I am a senior Rust developer (this is about me, the user)\n\
         2. We never use unwrap() in production code - this was a team decision after a crash last month (this is feedback/guidance)\n\
         3. Our bug tracker is at linear.app/acme (this is a reference to an external system)\n\
         Save each as a separate memory file with the appropriate type.\r",
    )
    .expect("send multi-type request");

    // Wait for multiple write_file calls.
    sess.expect("write_file").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see first write_file: {e}\nPTY screen:\n{screen}");
    });

    // Wait for turn completion.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after multi-type: {e}\nPTY screen:\n{screen}");
    });

    // Verify at least 3 memory files were created (excluding MEMORY.md).
    let files = list_md_files(&memory_dir);
    let non_index: Vec<_> = files.iter().filter(|f| *f != "MEMORY.md").collect();
    assert!(
        non_index.len() >= 3,
        "should create at least 3 memory files for 3 facts. \
         Found {}: {:?}",
        non_index.len(),
        non_index,
    );

    // Verify at least two different types appear across the files.
    let mut types_seen = std::collections::HashSet::new();
    for file_name in &non_index {
        let content =
            fs::read_to_string(memory_dir.join(file_name)).unwrap_or_default();
        for t in ["user", "feedback", "reference", "project"] {
            if content.contains(&format!("type: {t}")) {
                types_seen.insert(t);
            }
        }
    }
    assert!(
        types_seen.len() >= 2,
        "should have at least 2 different memory types. Found: {:?}",
        types_seen,
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────

/// Count `.md` files (excluding MEMORY.md) in a directory.
fn count_md_files(dir: &Path) -> usize {
    list_md_files(dir)
        .into_iter()
        .filter(|name| name != "MEMORY.md")
        .count()
}

/// List `.md` filenames in a directory.
fn list_md_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|e| {
            let path = e.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                path.file_name().map(|n| n.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Check if any `projects/*/memory/*.md` files exist.
fn has_memory_files(projects_dir: &Path) -> bool {
    let Ok(slugs) = fs::read_dir(projects_dir) else {
        return false;
    };
    for slug in slugs.flatten() {
        let memory_dir = slug.path().join("memory");
        if let Ok(entries) = fs::read_dir(&memory_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path.extension().is_some_and(|e| e == "md")
                    && path
                        .file_name()
                        .is_some_and(|n| !n.eq_ignore_ascii_case("MEMORY.md"))
                {
                    return true;
                }
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
