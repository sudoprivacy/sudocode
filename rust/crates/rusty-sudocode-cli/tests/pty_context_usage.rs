//! PTY tests for `/context` — the context-window occupancy grid (Claude Code
//! parity).
//!
//! The command reads what the next request would carry straight from the live
//! runtime, so these tests drive the real REPL and assert the rendered screen:
//! the grid + legend on a fresh session, the per-source footer, `/context all`
//! expansion, the message category after a completed turn, and the usage
//! message on a bad argument.
//!
//! ```bash
//! cargo test --test pty_context_usage                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_context_usage  # real API
//! ```

mod common;

use std::time::Duration;

use common::{screen_tail, TestEnv};

fn exit_repl(sess: &mut pty_expect::PtySession) {
    sess.expect("❯").expect("prompt before exit");
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}

// ──────────────────────────────────────────────────────────────────────
// 1. Fresh session: grid, legend, footer, expand hint
// ──────────────────────────────────────────────────────────────────────

/// Before any turn, `/context` renders the grid with the fixed-overhead
/// categories (system prompt, tools, agent catalog), free space, the
/// auto-compaction reserve, and the hint to expand.
#[test]
fn context_renders_grid_and_legend_on_fresh_session() {
    let env = TestEnv::new("context-fresh");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    sess.send("/context\r").expect("send /context");

    sess.expect("Context Usage").expect("report title");
    // First grid row: used squares lead, free squares follow.
    sess.expect("⛁").expect("grid shows used squares");
    sess.expect("⛶").expect("grid shows free squares");
    sess.expect("Estimated usage by category")
        .expect("legend heading");
    sess.expect("System prompt:")
        .expect("system prompt category");
    sess.expect("System tools:").expect("system tools category");
    sess.expect("Agent types:").expect("agent types category");
    sess.expect("Free space:").expect("free space row");
    sess.expect("⛝ Autocompact buffer:")
        .expect("auto-compaction reserve row");
    sess.expect("Auto-compact threshold:")
        .expect("threshold line");
    sess.expect("Agent types · /agents")
        .expect("agent types footer");
    sess.expect("/context all to expand")
        .expect("expand hint on the compact form");

    // No turn yet, so nothing is in the transcript.
    sess.expect("❯").expect("prompt after report");
    let screen = screen_tail(&sess, 4000);
    eprintln!("--- /context screen ---\n{screen}\n---");
    assert!(
        !screen.contains("Messages:"),
        "no Messages category before the first turn:\n{screen}"
    );

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}

// ──────────────────────────────────────────────────────────────────────
// 2. `/context all` expands the footer to one line per item
// ──────────────────────────────────────────────────────────────────────

/// The expanded form lists every system tool and agent type by name with its
/// own token figure, and drops the expand hint.
#[test]
fn context_all_lists_tools_and_agent_types() {
    let env = TestEnv::new("context-all");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    sess.send("/context all\r").expect("send /context all");

    sess.expect("Context Usage").expect("report title");
    sess.expect("System tools").expect("system tools section");
    sess.expect("└ bash:").expect("bash is a core tool");
    sess.expect("└ read_file:")
        .expect("read_file is a core tool");
    sess.expect("Agent types · /agents")
        .expect("agent types section");
    sess.expect(r"└ general-purpose \(Built-in\):")
        .expect("built-in agent type listed with its source");

    // Let the report finish drawing before reading the screen.
    sess.expect("❯").expect("prompt after report");
    let screen = screen_tail(&sess, 6000);
    assert!(
        !screen.contains("/context all to expand"),
        "expanded form must not carry the expand hint:\n{screen}"
    );
    eprintln!("--- /context all screen ---\n{screen}\n---");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}

// ──────────────────────────────────────────────────────────────────────
// 3. After a turn, the transcript shows up as the Messages category
// ──────────────────────────────────────────────────────────────────────

/// A completed turn adds a `Messages` row; the headline total then comes from
/// the provider's reported occupancy rather than the local estimate.
#[test]
fn context_counts_messages_after_a_turn() {
    let env = TestEnv::new("context-after-turn");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    sess.send(&format!("{prompt}\r")).expect("send prompt");
    sess.expect("4").expect("response should contain '4'");
    sess.expect("❯").expect("prompt after the turn");

    sess.send("/context\r").expect("send /context");
    sess.expect("Context Usage").expect("report title");
    sess.expect("Messages:")
        .expect("messages category after a turn");
    sess.expect("/context all to expand")
        .expect("compact form hint");

    exit_repl(&mut sess);
}

// ──────────────────────────────────────────────────────────────────────
// 4. Instruction files are the Memory files category
// ──────────────────────────────────────────────────────────────────────

/// An `AGENTS.md` in the workspace lands in the prompt as project
/// instructions; `/context` counts it under Memory files and `/context all`
/// names the file.
#[test]
fn context_lists_workspace_instruction_files_as_memory() {
    let env = TestEnv::new("context-memory");
    std::fs::write(
        env.workspace_root().join("AGENTS.md"),
        "# Project rules\n\nAlways answer in one sentence.\n",
    )
    .expect("write AGENTS.md");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    sess.send("/context\r").expect("send /context");
    sess.expect("Memory files:").expect("memory files category");
    sess.expect("Memory files · /memory")
        .expect("memory files footer");
    sess.expect("└ 1 file ·")
        .expect("one instruction file counted");
    sess.expect("❯").expect("prompt after compact report");

    sess.send("/context all\r").expect("send /context all");
    sess.expect("AGENTS.md:")
        .expect("expanded form names the file");

    exit_repl(&mut sess);
}

// ──────────────────────────────────────────────────────────────────────
// 5. Unknown argument → usage, not a report
// ──────────────────────────────────────────────────────────────────────

/// `/context` takes only `all`; anything else prints the usage line and the
/// REPL stays up.
#[test]
fn context_rejects_unknown_argument() {
    let env = TestEnv::new("context-bad-arg");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    sess.send("/context clear\r").expect("send /context clear");
    sess.expect(r"Usage            /context \[all\]")
        .expect("usage line");
    sess.expect("Unknown argument clear")
        .expect("names the rejected argument");

    exit_repl(&mut sess);
}
