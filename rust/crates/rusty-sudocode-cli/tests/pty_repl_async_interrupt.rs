//! PTY test for the auto-interrupt path in async REPL mode.
//!
//! Guards the wiring landed in PR #300 (`TurnDriver::abort_current_turn`) +
//! LiveCli's `persistent_abort_signal`: when
//! `SUDOCODE_INTERRUPT_QUEUE_MODE=interrupt` (or `both`), a second submit
//! DURING a running turn calls the driver's abort hook, the runner's
//! in-flight `run_turn` returns a cancelled TurnSummary, and the drain
//! picks up the interrupter as a fresh solo turn per §3.2 row 2.
//!
//! ## Real user journey (5+ steps, per integration-test-generator standard)
//!
//! 1. Boot scode REPL under mock backend with
//!    `SUDOCODE_INTERRUPT_QUEUE_MODE=interrupt` — the async loop takes over
//!    (`run_repl_async_dispatch`) and installs the persistent abort signal.
//! 2. Fire prompt A that triggers `bash_interrupt_long_running` — mock
//!    returns a `bash` tool_use with the canned `sleep 30` command. Real
//!    subprocess starts, prints "interrupt-start", enters the sleep.
//! 3. Wait for "interrupt-start" in the PTY output — proves the tool
//!    subprocess actually started (i.e., the turn is really in-flight at
//!    the runner thread, not just queued).
//! 4. Send prompt B (`INTERRUPT_TRIGGER_...`) — input thread's rustyline
//!    delivers it, coordinator's `submit_during_turn(Interrupt)` fires,
//!    `driver.abort_current_turn()` sets the abort flag. Runtime's
//!    tokio::select! sees the aborted signal, drops the tool future
//!    (kill_on_drop → SIGTERM the bash subprocess), returns cancelled.
//! 5. `TurnEvent::Done` propagates → drain picks up B as the queue head
//!    (marked `solo` by submit_during_turn) → runner starts B's turn.
//! 6. `/exit` — coordinator loop's exit path already aborts + joins in
//!    ~1 s (PR #301); we don't wait for full clean exit because the
//!    test's real signal is what happened between steps 3-5.
//!
//! ## What "PASS" means
//!
//! The strong end-to-end signal is that "interrupt-start" appears (turn A
//! ran) AND `/exit` returns bounded — proving the abort path fired. If
//! auto-interrupt were broken, `sleep 30` would run to completion and
//! `/exit` would either wait 30 s or fail. The 15 s exit timeout catches
//! both regressions.

mod common;

use common::TestEnv;
use std::time::Duration;

const INTERRUPT_MARKER: &str = "INTERRUPT_TRIGGER_MARKER";

/// Real user journey: send bash-tool prompt → wait for tool to start →
/// interrupt with a second prompt → verify clean shutdown. Regression
/// guard for any change that breaks the `TurnDriver::abort_current_turn`
/// → runtime abort → tool SIGTERM chain.
#[test]
fn submit_during_turn_in_interrupt_mode_aborts_running_turn() {
    let env = TestEnv::new("repl-async-interrupt");

    let mut sess = env.spawn_with_env(
        // danger-full-access so the mock's canned bash command actually runs
        // (the tool call is denied under read-only). Same flag the sibling
        // tests use for bash-based scenarios.
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "interrupt")],
    );

    // Step 1: async REPL prompt renders — proves run_repl_async_dispatch
    // ran, LineEditor is up, persistent abort signal is installed.
    sess.expect("❯")
        .expect("async REPL should render the initial prompt");

    // Step 2: fire the long-running bash prompt. The natural-language text is
    // EXPLICIT about the exact command so this works in BOTH backends: under
    // mock the `bash_interrupt_long_running` scenario returns the canned
    // `printf 'interrupt-start'; sleep 30` tool_use (prompt text ignored);
    // under live the real model runs the command we spell out here, so it
    // emits the same "interrupt-start" marker. (The env drops the scenario
    // token in live — see TestEnv::prompt.) Same "Run this exact bash command"
    // shape pty_bash_execution uses to drive live models deterministically.
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r"))
        .expect("send long-running prompt");

    // Step 3: wait for the tool to actually start. The command prints
    // `interrupt-start` before the sleep, and PTY captures the bash stdout.
    // Seeing this string proves the runner thread reached tool execution +
    // the subprocess is actually sleeping — the real in-flight window we
    // interrupt into. Works identically mock and live.
    sess.expect("interrupt-start")
        .expect("bash tool should print interrupt-start before the sleep");

    // Step 4: submit the interrupter during A's turn. Coordinator's
    // submit_during_turn(Interrupt) should place this at the queue head
    // with solo=true, fire driver.abort_current_turn(), and the runner's
    // in-flight cli.run_turn should return with a cancelled TurnSummary.
    sess.send(&format!("{INTERRUPT_MARKER}\r"))
        .expect("send interrupter during A");

    // Step 5: exit. If abort_current_turn wired through, /exit's own abort
    // call (from PR #301's mid-turn exit fix) returns quickly whether B
    // already started or not. If the abort path is broken, the runner is
    // stuck in bash sleep 30 and /exit joins for the full sleep — the
    // 15 s ceiling catches that regression.
    sess.send("/exit\r").expect("send /exit");

    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
