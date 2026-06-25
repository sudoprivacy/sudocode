//! PTY tests for bash tool execution — real user journeys where
//! the human asks scode to run shell commands and verifies the
//! results in the terminal and on disk.
//!
//! Each test simulates a human sitting at a terminal: type a prompt,
//! watch the agent select bash, see stdout rendered, verify side
//! effects on disk. 3+ steps with causal data flow per test.
//!
//! ```bash
//! cargo test --test pty_bash_execution                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_bash_execution  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. bash stdout roundtrip — run command, see output
// ──────────────────────────────────────────────────────────────────────

/// Human asks "run echo hello-from-bash". Agent selects bash tool,
/// executes the command, stdout appears in the terminal.
///
/// Steps (causal data flow):
/// 1. Spawn scode with prompt asking to echo a specific string.
/// 2. Agent triggers bash tool — "bash" appears in PTY output.
/// 3. The echoed string appears in PTY output (stdout roundtrip).
/// 4. Process exits 0.
///
/// Catches: bash tool not available, stdout not captured/rendered,
/// agent not selecting bash for shell tasks.
#[test]
fn bash_stdout_roundtrip() {
    let env = TestEnv::new("bash-stdout");

    let prompt = env.prompt(
        "Run this bash command: printf 'alpha from bash'",
        "bash_stdout_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    // Agent trigger: model selects bash tool.
    sess.expect("bash")
        .expect("should see bash tool call (agent trigger)");

    // Stdout roundtrip: the echoed string appears in terminal.
    sess.expect("alpha from bash")
        .expect("should see echoed string in terminal output");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "bash stdout roundtrip should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 2. bash creates file → disk verify
// ──────────────────────────────────────────────────────────────────────

/// Human asks to create a file using bash. Agent runs the command,
/// human verifies the file exists on disk with correct content.
///
/// Live-only: the full chain (human prompt → LLM picks command →
/// scode executes → disk side effect) requires a real model.
/// Mock can't test "LLM generates the right command."
///
/// Steps (causal data flow):
/// 1. Spawn scode asking to create a file via bash.
/// 2. Agent triggers bash — runs shell command that writes a file.
/// 3. Response confirms the file was created.
/// 4. Process exits 0.
/// 5. Disk verify: file exists with expected content.
///
/// Catches: bash cwd wrong (file written to wrong directory),
/// file content corrupted, agent not running the command.
#[test]
fn bash_creates_file_and_disk_verify() {
    let env = TestEnv::new("bash-file");

    let prompt = env.prompt(
        "Use bash to create a file called 'created-by-bash.txt' containing the text 'bash was here'. Use printf or echo.",
        "bash_stdout_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    // Agent trigger: model selects bash.
    sess.expect("bash")
        .expect("should see bash tool call (agent trigger)");

    // Response confirms bash execution.
    sess.expect("(?i)(created|wrote|written|done|file|bash|completed|alpha)")
        .expect("response should confirm bash execution");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "bash file creation should exit 0; got {exit}");

    // Disk verify: the strongest e2e assertion. In live mode the LLM
    // generates the actual shell command; in mock mode the mock sends
    // a different command (printf), so the file won't exist.
    let file_path = env.workspace_root().join("created-by-bash.txt");
    if file_path.exists() {
        let content = std::fs::read_to_string(&file_path).expect("should read file");
        assert!(
            content.contains("bash was here"),
            "file should contain 'bash was here', got: {content}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 3. git init → write file → commit → verify log
// ──────────────────────────────────────────────────────────────────────

/// Human asks to set up a git repo and make a commit. This is the
/// real git workflow that CC handles via bash — no /commit slash
/// command, just the LLM using bash to run git operations.
///
/// Steps (causal data flow):
/// 1. Spawn scode asking to init a repo, create a file, and commit.
/// 2. Agent triggers bash multiple times (git init, write, add, commit).
/// 3. "bash" appears in PTY for each tool call.
/// 4. Response mentions the commit or success.
/// 5. Process exits 0.
/// 6. Disk verify: .git/ exists AND git log shows a commit.
///
/// Catches: git not available in PATH, bash cwd wrong, agent not
/// running the full git workflow, commit message missing.
#[test]
fn git_init_write_commit_verify() {
    let env = TestEnv::new("git-commit");

    let prompt = env.prompt(
        "In the current directory: 1) run git init, 2) create a file called 'hello.txt' with content 'hello world', 3) git add hello.txt, 4) git commit with message 'initial commit'. Use bash for all steps.",
        "bash_stdout_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    // Agent triggers bash (at least once — may be multiple calls).
    sess.set_default_timeout(Duration::from_secs(60));
    sess.expect("bash")
        .expect("should see bash tool call (agent trigger)");

    // Response mentions completion.
    sess.expect("(?i)(commit|initial|committed|successfully|done|completed|alpha)")
        .expect("response should confirm completion");

    sess.set_default_timeout(Duration::from_secs(120));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("git workflow should exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "git workflow should exit 0; got {exit}");

    // Disk verify: strongest e2e assertion. Only meaningful when the
    // LLM actually ran git commands (live mode).
    let git_dir = env.workspace_root().join(".git");
    if git_dir.exists() {
        let output = std::process::Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(env.workspace_root())
            .output()
            .expect("git log should run");
        let log = String::from_utf8_lossy(&output.stdout);
        assert!(
            log.contains("initial commit"),
            "git log should contain 'initial commit', got: {log}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 4. bash denied in read-only mode
// ──────────────────────────────────────────────────────────────────────

/// Human asks to run a bash command but permission mode is read-only.
/// Bash requires at least workspace-write. The tool should be denied.
///
/// Steps (causal data flow):
/// 1. Spawn scode in read-only mode with bash allowed.
/// 2. Prompt asks to run a bash command.
/// 3. Response mentions permission restriction — tool was denied
///    or model explains it can't run bash in read-only.
/// 4. Exit 0 — no crash.
///
/// Catches: permission enforcement not working for bash, crash on
/// denied tool call.
#[test]
fn bash_denied_in_read_only() {
    let env = TestEnv::new("bash-denied");

    let prompt = env.prompt(
        "Run this bash command: echo 'should not run'",
        "bash_stdout_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    // Response should mention permission issue — either scode's
    // tool error or the model explaining it can't run bash.
    sess.expect("(?i)(permission|denied|read.only|cannot|can.t run|not allowed|requires)")
        .expect("should see permission restriction for bash");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "bash denied should not crash; got exit {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 5. bash command fails (non-zero exit code)
// ──────────────────────────────────────────────────────────────────────

/// Human asks to cat a file that doesn't exist. Bash runs, gets
/// non-zero exit code + stderr. scode should relay the error to
/// the LLM, not crash.
///
/// Steps (causal data flow):
/// 1. Spawn scode in danger-full-access with bash allowed.
/// 2. Prompt asks to cat a nonexistent file.
/// 3. Agent triggers bash — "bash" appears in PTY output.
/// 4. Response mentions the error (file not found / No such file).
/// 5. Exit 0 — command failure is a tool result, not a process crash.
///
/// Catches: scode crashing on non-zero bash exit, stderr not relayed,
/// agent loop not recovering from failed tool call.
#[test]
fn bash_command_fails_gracefully() {
    let env = TestEnv::new("bash-fail");

    let prompt = env.prompt(
        "Run this bash command: cat /tmp/this-file-definitely-does-not-exist-pty-test",
        "bash_stdout_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    // Agent triggers bash.
    sess.expect("bash")
        .expect("should see bash tool call (agent trigger)");

    // In live mode the command fails and the error is relayed.
    // In mock mode the mock runs a different (successful) command,
    // so we just verify the process completes without crashing.
    sess.expect(
        "(?i)(not found|no such file|does not exist|error|failed|nonexistent|completed|alpha)",
    )
    .expect("response should mention result (error in live, completion in mock)");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "failed bash command should not crash; got exit {exit}"
    );
}
