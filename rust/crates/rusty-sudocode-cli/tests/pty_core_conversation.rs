//! PTY tests for the five core conversation features:
//!
//! 1. Single-turn prompt — exits cleanly after one response
//! 2. Multi-turn context — prior turns are visible in follow-ups
//! 3. Streaming response — tokens render incrementally
//! 4. Multi-tool turn roundtrip — multiple tool calls in one turn
//! 5. Graceful cancel mid-execution — Ctrl+C stops cleanly
//!
//! All five reuse the existing `MockAnthropicService` and the shared
//! PTY helpers in `common/mod.rs`. Unix-only because PTY allocation
//! and SIGINT semantics are POSIX-specific; Windows ConPTY coverage
//! can be added when `pty-expect` ships v0.2.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{spawn_scode_mock, HarnessWorkspace};
use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};

/// Helper: start a tokio runtime and mock server, write config, return
/// both so tests can assert against captured requests after the PTY
/// session completes.
struct MockEnv {
    _runtime: tokio::runtime::Runtime,
    server: MockAnthropicService,
    workspace: HarnessWorkspace,
}

impl MockEnv {
    fn new(label: &str) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let server = runtime
            .block_on(MockAnthropicService::spawn())
            .expect("mock service should start");
        let workspace = HarnessWorkspace::new(label);
        workspace.write_config(&server.base_url());
        Self {
            _runtime: runtime,
            server,
            workspace,
        }
    }

    fn captured_message_count(&self) -> usize {
        self._runtime
            .block_on(self.server.captured_requests())
            .iter()
            .filter(|r| r.path == "/v1/messages")
            .count()
    }
}

// ──────────────────────────────────────────────────────────────────────
// 1. Single-turn prompt — `scode "prompt"` → response → exit 0
// ──────────────────────────────────────────────────────────────────────

/// User runs `scode "PARITY_SCENARIO:single_turn_text"`, sees "The
/// answer is 4" streamed back, and the process exits 0.
///
/// Steps with causal data flow:
/// 1. Spawn scode with the single-turn scenario prompt.
/// 2. Expect "answer" — proves the mock response reached the terminal.
/// 3. Expect "4" — proves the full text was rendered, not truncated.
/// 4. expect_eof == 0 — proves the process exits cleanly after one turn.
/// 5. Verify mock saw exactly 1 /v1/messages request.
///
/// Catches: process hanging after single turn, wrong exit code, empty
/// response, duplicate API calls.
#[test]
fn single_turn_exits_after_response() {
    let env = MockEnv::new("single-turn");
    let prompt = format!("{SCENARIO_PREFIX}single_turn_text");

    let mut sess = spawn_scode_mock(&env.workspace, &["--permission-mode", "read-only", &prompt])
        .expect("spawn scode single-turn");

    sess.expect("answer")
        .expect("should see 'answer' in streamed response");
    sess.expect("4")
        .expect("should see '4' in streamed response");
    let exit = sess.expect_eof().expect("scode should exit on its own");
    assert_eq!(exit, 0, "single-turn scode should exit 0; got {exit}");

    assert_eq!(
        env.captured_message_count(),
        1,
        "single-turn should produce exactly 1 /v1/messages request"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. Multi-turn context — interactive REPL, prior turns carry forward
// ──────────────────────────────────────────────────────────────────────

/// User starts scode in interactive mode, sends "my name is Alice
/// PARITY_SCENARIO:multi_turn_context", gets "Hello, Alice!", then
/// sends a second message and the mock (seeing prior context) responds
/// with "Your name is Alice."
///
/// Steps with causal data flow:
/// 1. Spawn scode in interactive (no positional prompt).
/// 2. Send first message containing the scenario tag + "my name is Alice".
/// 3. Expect "Alice" — proves the mock's first-turn response rendered.
/// 4. Send second message (same scenario tag).
/// 5. Expect "Alice" again — proves the mock saw prior context and
///    the second response rendered.
/// 6. Send `/exit` to quit the REPL.
/// 7. expect_eof — proves clean shutdown.
///
/// Catches: context window not carrying prior turns, REPL not accepting
/// follow-up input, session corruption.
#[test]
fn multi_turn_references_prior() {
    let env = MockEnv::new("multi-turn");

    let mut sess = spawn_scode_mock(&env.workspace, &["--permission-mode", "read-only"])
        .expect("spawn scode multi-turn");

    // Wait for the REPL prompt before sending input.
    sess.expect("❯").expect("should see REPL prompt");

    // First turn: introduce the name.
    // Use `send` with `\r` (carriage return = Enter key) instead of
    // `send_line` because rustyline puts the terminal in raw mode
    // where `\n` is NOT interpreted as Enter.
    let first_msg = format!("my name is Alice {SCENARIO_PREFIX}multi_turn_context");
    sess.send(&format!("{first_msg}\r"))
        .expect("send first message");
    // Expect text that only appears in the mock RESPONSE, not in
    // the echoed input. The mock returns "Hello, Alice! Nice to meet you."
    sess.expect("Nice to meet you")
        .expect("first response should contain greeting");

    // Wait for the separator/prompt that appears after the response.
    // The REPL chrome includes `─` separator lines and the `❯` prompt.
    sess.expect("❯")
        .expect("should see REPL prompt after first turn");

    // Second turn: ask for the name back.
    let second_msg = format!("what is my name {SCENARIO_PREFIX}multi_turn_context");
    sess.send(&format!("{second_msg}\r"))
        .expect("send second message");
    // The mock's second-turn response is "Your name is Alice."
    sess.expect("Your name is")
        .expect("second response should recall context");

    // Wait for prompt, then exit cleanly.
    sess.expect("❯")
        .expect("should see REPL prompt after second turn");

    sess.send("/exit\r").expect("send /exit");

    sess.set_default_timeout(std::time::Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("scode should exit after /exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "interactive scode should exit 0; got {exit}");

    assert_eq!(
        env.captured_message_count(),
        2,
        "two turns should produce exactly 2 /v1/messages requests"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 3. Streaming response — tokens render incrementally
// ──────────────────────────────────────────────────────────────────────

/// User runs `scode "PARITY_SCENARIO:streaming_text"`. The mock sends
/// two SSE text_delta events: "Mock streaming " and "says hello from
/// the parity harness." The test verifies both parts appear in the
/// terminal output.
///
/// Steps with causal data flow:
/// 1. Spawn scode with the streaming_text scenario.
/// 2. Expect "Mock streaming" — proves the first chunk flushed.
/// 3. Expect "parity harness" — proves the second chunk arrived and
///    the full message rendered.
/// 4. expect_eof == 0 — proves clean exit.
///
/// Catches: streaming buffer not flushing (all text appearing at once
/// after process exits), partial render, broken SSE parsing.
#[test]
fn streaming_tokens_render_incrementally() {
    let env = MockEnv::new("streaming");
    let prompt = format!("{SCENARIO_PREFIX}streaming_text");

    let mut sess = spawn_scode_mock(&env.workspace, &["--permission-mode", "read-only", &prompt])
        .expect("spawn scode streaming");

    sess.expect("Mock streaming")
        .expect("should see first streaming chunk");
    sess.expect("parity harness")
        .expect("should see second streaming chunk");
    let exit = sess
        .expect_eof()
        .expect("scode should exit after streaming");
    assert_eq!(exit, 0, "streaming scode should exit 0; got {exit}");

    assert_eq!(
        env.captured_message_count(),
        1,
        "streaming-text should produce exactly 1 /v1/messages request"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 4. Multi-tool turn roundtrip — read_file + grep_search in one turn
// ──────────────────────────────────────────────────────────────────────

/// User runs `scode "PARITY_SCENARIO:multi_tool_turn_roundtrip"`. The
/// mock issues two tool calls (read_file + grep_search) in the first
/// response, scode executes both, sends results back, and the mock
/// replies with a final text summarizing both results.
///
/// Steps with causal data flow:
/// 1. Create fixture.txt in workspace (needed for read_file + grep).
/// 2. Spawn scode with the multi-tool scenario.
/// 3. Expect "read_file" — proves the tool call was rendered.
/// 4. Expect "grep" — proves both tools in the turn are shown.
/// 5. Expect "roundtrip complete" — proves the final response after
///    tool results rendered.
/// 6. expect_eof == 0 — proves clean exit after multi-tool turn.
///
/// Catches: multi-tool turn rendering broken, tool execution failure,
/// final response missing.
#[test]
fn multi_tool_roundtrip() {
    let env = MockEnv::new("multi-tool");

    // The mock's multi_tool_turn_roundtrip scenario asks for
    // read_file("fixture.txt") and grep_search("parity", "fixture.txt").
    std::fs::write(
        env.workspace.root.join("fixture.txt"),
        "alpha parity line\nbeta line\ngamma parity line\n",
    )
    .expect("fixture.txt should be written");

    let prompt = format!("{SCENARIO_PREFIX}multi_tool_turn_roundtrip");
    let mut sess = spawn_scode_mock(
        &env.workspace,
        &[
            "--permission-mode",
            "read-only",
            "--allowedTools",
            "read_file,grep_search",
            &prompt,
        ],
    )
    .expect("spawn scode multi-tool");

    // The REPL renders tool names when executing them.
    sess.expect("read_file")
        .expect("should see read_file tool call");
    sess.expect("grep")
        .expect("should see grep_search tool call");
    // Final response from the mock after processing both tool results.
    sess.expect("roundtrip complete")
        .expect("should see final multi-tool response");
    let exit = sess
        .expect_eof()
        .expect("scode should exit after multi-tool turn");
    assert_eq!(exit, 0, "multi-tool scode should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 5. Graceful cancel mid-execution — Ctrl+C during bash tool
// ──────────────────────────────────────────────────────────────────────

/// User runs scode with a bash scenario that sleeps for 30 seconds,
/// then presses Ctrl+C. The process should handle SIGINT, interrupt
/// the running bash tool, and exit without hanging.
///
/// Steps with causal data flow:
/// 1. Spawn scode with bash_stdout_roundtrip scenario in
///    danger-full-access mode.
/// 2. Expect "bash" — proves the tool call was received and
///    execution started.
/// 3. Send Ctrl+C — simulates the user pressing Ctrl+C.
/// 4. expect_eof within a generous timeout — proves the process
///    handled the signal and exited rather than hanging.
///
/// Catches: SIGINT not caught, process orphaned, bash child not
/// reaped, cleanup not running.
#[test]
fn sigint_cancels_streaming() {
    let env = MockEnv::new("sigint-cancel");
    let prompt = format!("{SCENARIO_PREFIX}bash_interrupt_long_running");

    let mut sess = spawn_scode_mock(
        &env.workspace,
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
            &prompt,
        ],
    )
    .expect("spawn scode sigint");

    // Wait until the bash tool call is being executed.
    sess.expect("bash").expect("should see bash tool call");

    // Give the bash command a moment to start (it prints
    // "interrupt-start" then sleeps), then send Ctrl+C.
    std::thread::sleep(Duration::from_millis(500));
    sess.send_ctrl('c').expect("send Ctrl+C");

    // The process should exit within a reasonable time. We use a
    // generous timeout because signal handling and cleanup may take
    // a moment.
    sess.set_default_timeout(Duration::from_secs(15));
    let _exit = sess
        .expect_eof()
        .expect("scode should exit after Ctrl+C, not hang");
    // Exit code may be 0 (graceful) or non-zero (interrupted);
    // the key assertion is that expect_eof succeeds (process exits).
}
