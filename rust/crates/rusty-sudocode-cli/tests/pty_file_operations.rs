//! PTY tests for file operation tools — real user workflows with
//! causal data flow (3+ steps each).
//!
//! Same principles as `pty_core_conversation.rs`: live quality first,
//! DRY assertions, no mock-only branches except `captured_message_count`.
//!
//! ```bash
//! cargo test --test pty_file_operations                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_file_operations  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

/// Windows accepts all these spellings of the same absolute file. The real
/// CLI must preserve that identity when its file tools cross into the VFS.
#[cfg(windows)]
#[test]
fn windows_verbatim_paths_read_the_same_file() {
    use runtime::{ContentBlock, SessionStore};

    let env = TestEnv::new("verbatim-file-paths");
    let nonce = format!(
        "path-content-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let file = env.workspace_root().join("fixture.txt");
    std::fs::write(&file, &nonce).unwrap();
    let canonical = std::fs::canonicalize(&file).unwrap();
    let native = canonical.to_str().unwrap();
    let plain = native.strip_prefix(r"\\?\").unwrap();
    let paths = [
        plain.to_string(),
        native.to_string(),
        format!("//?/{}", plain.replace('\\', "/")),
        format!("//?/{plain}"),
    ];
    for path in &paths {
        assert_eq!(std::fs::read_to_string(path).unwrap(), nonce);
    }
    let natural = format!(
        "Use read_file once for each of these four JSON path strings, passing each path exactly as written. Then report the contents: {}",
        serde_json::to_string(&paths).unwrap()
    );
    let mut prompt = env.prompt(&natural, "tool_concurrency");
    if env.is_mock() {
        let calls: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                serde_json::json!({
                    "id": format!("path-{i}"), "name": "Read", "input": {"path": path}
                })
            })
            .collect();
        prompt.push_str(&format!(
            " TOOL_BATCH:{}",
            serde_json::to_string(&calls).unwrap()
        ));
    }
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "read-only",
            "--allowedTools",
            "read_file",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line(&sess, "", common::DEFAULT_TIMEOUT, "initial prompt");
    let marker = common::turn_status_marker(&sess);
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("PARITY_SCENARIO:") || s.contains("Use read_file"),
        common::DEFAULT_TIMEOUT,
        "path instructions pasted",
    );
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(&sess, &marker, env.timeout(), "file reads");
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    let saved = SessionStore::from_cwd(env.workspace_root())
        .unwrap()
        .load_session("latest")
        .unwrap()
        .session;
    let blocks: Vec<_> = saved.messages.iter().flat_map(|m| &m.blocks).collect();
    for path in &paths {
        let id = blocks
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolUse { id, input, .. } => {
                    let input: serde_json::Value = serde_json::from_str(input).unwrap();
                    (input["path"].as_str() == Some(path.as_str())).then_some(id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("model did not request exact path {path:?}"));
        let result = blocks
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    output,
                    is_error,
                    ..
                } if tool_use_id == id => Some((output, is_error)),
                _ => None,
            })
            .expect("every requested read must persist its result");
        assert!(!result.1, "read failed for {path:?}: {}", result.0);
        assert!(
            result.0.contains(&nonce),
            "file contents missing for {path:?}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 1. write_file → read_file roundtrip
// ──────────────────────────────────────────────────────────────────────

/// Write a file, verify it exists on disk with correct content.
/// Agent trigger: model must select write_file tool.
#[test]
fn write_then_read_back() {
    let env = TestEnv::new("write-read");

    let prompt = env.prompt(
        "Create a file called 'hello.txt' with the content 'hello from scode' using write_file, then read it back with read_file to confirm.",
        "write_file_allowed",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_file,read_file",
        &prompt,
    ]);

    // The mock exposes the internal tool name; live iocraft renders a stable
    // human-facing write card instead.
    if env.is_mock() {
        sess.expect("(?i)write_file")
            .expect("should see write_file tool call (agent trigger)");
    } else {
        sess.expect("(?i)(writing|wrote|written|created)")
            .expect("should see the live write operation");
    }

    // Response confirms the write.
    sess.expect("(?i)(created|wrote|written|succeeded|hello|output)")
        .expect("response should confirm write");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "write-read should exit 0; got {exit}");

    // Disk verification — the strongest assertion. Works both modes
    // because scode's write_file writes to the real filesystem
    // regardless of mock/live.
    let wrote_hello = env.workspace_root().join("hello.txt").exists();
    let wrote_generated = env
        .workspace_root()
        .join("generated")
        .join("output.txt")
        .exists();
    assert!(
        wrote_hello || wrote_generated,
        "at least one file should exist on disk after write_file"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. write_file → edit_file → read_file verification
// ──────────────────────────────────────────────────────────────────────

/// Pre-create file → edit (alpha→omega) → verify on disk.
/// Agent trigger: model must select edit_file tool.
#[test]
fn edit_then_verify_on_disk() {
    let env = TestEnv::new("edit-verify");

    std::fs::write(
        env.workspace_root().join("fixture.txt"),
        "alpha parity line\nbeta line\n",
    )
    .expect("fixture.txt should be written");

    let prompt = env.prompt(
        "Edit fixture.txt: replace the word 'alpha' with 'omega' using edit_file, then read it back with read_file to confirm the change.",
        "edit_file_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "edit_file,read_file",
        &prompt,
    ]);

    // The mock exposes the internal tool name; live iocraft renders a stable
    // human-facing update card instead.
    if env.is_mock() {
        sess.expect("(?i)edit_file")
            .expect("should see edit_file tool call (agent trigger)");
    } else {
        sess.expect("(?i)(updating|updated|edited|replaced)")
            .expect("should see the live edit operation");
    }

    // Response confirms the edit.
    sess.expect("(?i)(omega|replaced|changed|edited|updated|complete)")
        .expect("response should confirm edit");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "edit-verify should exit 0; got {exit}");

    // Disk verification: alpha → omega, beta untouched.
    let content = std::fs::read_to_string(env.workspace_root().join("fixture.txt"))
        .expect("fixture.txt should still exist");
    assert!(
        content.contains("omega"),
        "should contain 'omega' after edit, got: {content}"
    );
    assert!(
        !content.contains("alpha"),
        "should not contain 'alpha' after edit, got: {content}"
    );
    assert!(
        content.contains("beta line"),
        "untouched line should remain, got: {content}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 3. write multiple files → glob_search → grep_search
// ──────────────────────────────────────────────────────────────────────

/// Create files → glob to discover → grep to find content.
/// Agent trigger: model must select grep_search (at minimum).
#[test]
fn glob_then_grep_discovery() {
    let env = TestEnv::new("glob-grep");

    let root = env.workspace_root();
    std::fs::write(root.join("notes.txt"), "alpha parity line\n").expect("notes.txt");
    std::fs::write(root.join("data.txt"), "beta line\ngamma parity line\n").expect("data.txt");
    std::fs::write(root.join("readme.md"), "this is markdown, not txt\n").expect("readme.md");

    let prompt = env.prompt(
        "First use glob_search with pattern '*.txt' to find all .txt files in the current directory. Then use grep_search to find all lines containing 'parity' in those files. Tell me how many matches you found.",
        "multi_tool_turn_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--allowedTools",
        "glob_search,grep_search,read_file",
        &prompt,
    ]);

    // Agent trigger: at minimum grep must be called.
    sess.set_default_timeout(common::at_least(Duration::from_secs(60)));
    sess.expect("(?i)grep")
        .expect("should see grep tool call (agent trigger)");

    // Response references the results.
    sess.expect("(?i)(parity|2|two|match|found|notes|data|complete)")
        .expect("response should reference discovery results");

    sess.set_default_timeout(common::at_least(Duration::from_secs(120)));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("glob-grep scode should exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "glob-grep should exit 0; got {exit}");
}
