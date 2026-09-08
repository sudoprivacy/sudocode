//! PTY test: plugin/config hook progress reaches the terminal while a turn runs.
//!
//! A hook is a shell command the user wires in front of (or behind) every tool
//! call, and it can block the call. When one runs, the terminal says so —
//! `[hook PreToolUse] bash: <command>` as it starts, `[hook done PreToolUse] …`
//! when it returns. Without those lines a slow hook is indistinguishable from a
//! hung agent, and a denying hook looks like the model silently refusing.
//!
//! **Why this test exists.** The engine/renderer split (#100) moved this output
//! across the seam: the runtime no longer writes to the terminal, it emits
//! `EngineEvent::HookProgress` and the renderer formats it (`render_engine::
//! render_hook_progress`). The atomic flip that introduced the seam had already
//! silently dropped two REPL features this way — an audit caught them, not a
//! test — and hook progress was restored the same way with no behavioural test
//! behind it. This is that test: it fails if the observer stops installing the
//! turn's hook-progress sink, if the adapter stops forwarding the event, or if
//! the renderer stops printing it.
//!
//! Mock backend, so the hook fires on a scripted tool call rather than on a
//! live model's choice of tool.
//!
//! ```bash
//! cargo test --test pty_hook_progress
//! ```

mod common;

use std::fs;
use std::time::Duration;

use common::TestEnv;

/// A hook command that runs on every platform and allows the tool through.
///
/// `echo` is a builtin of both shells the runner uses (`cmd /C` on Windows,
/// `sh -lc` elsewhere), so this needs nothing on PATH. Exit 0 with non-JSON
/// stdout is the "allow" outcome, which keeps the turn on its normal path —
/// this test is about the progress lines, not about denial.
const HOOK_COMMAND: &str = "echo hook-observed";

/// Wire `command` as a `PreToolUse` hook for the workspace.
///
/// Project scope (`.nexus/sudocode/settings.json`) rather than the config home:
/// the hook then belongs to this test's temp workspace and cannot leak into
/// another test's session or the developer's own.
fn write_pre_tool_use_hook(env: &TestEnv, command: &str) {
    let dir = env.workspace_root().join(".nexus").join("sudocode");
    fs::create_dir_all(&dir).expect("create project config dir");
    fs::write(
        dir.join("settings.json"),
        format!("{{\n  \"hooks\": {{\n    \"PreToolUse\": [\"{command}\"]\n  }}\n}}\n"),
    )
    .expect("write project settings.json");
}

/// A hook running in front of a tool call announces itself, then announces that
/// it finished — both on the terminal, while the turn is still going.
#[test]
fn pre_tool_use_hook_progress_reaches_the_terminal() {
    let env = TestEnv::new("hook-progress");
    write_pre_tool_use_hook(&env, HOOK_COMMAND);

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
    sess.set_default_timeout(Duration::from_secs(60));
    // Wide enough that a hook line never wraps: the assertions below match a
    // whole line, and a wrap would split it across two rows that no single
    // pattern spans.
    sess.resize(50, 120).expect("resize pty");

    // The start line names the event, the tool it is gating, and the command
    // that is about to run — the three things a user needs to identify a hook
    // that is taking too long. Asserted as one pattern so a line that dropped
    // any of them fails here rather than passing on a partial match.
    sess.expect(r"\[hook PreToolUse\] bash: echo hook-observed")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("hook start should be announced: {e}\nPTY screen:\n{screen}");
        });

    // And the completion line, which is what tells the user the turn moved on
    // rather than stalled inside the hook.
    sess.expect(r"\[hook done PreToolUse\] bash: echo hook-observed")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("hook completion should be announced: {e}\nPTY screen:\n{screen}");
        });

    // The hook allowed the call, so the turn carried on through the tool and
    // fed its result back to the model. Matched after the two lines above, so
    // this also pins the ordering: hook progress is reported live, ahead of the
    // work it gates, not replayed once the turn is over.
    //
    // The prefix only. What follows it is the tool result with the hook's own
    // feedback merged in (`merge_hook_feedback`) — a hooks concern with its own
    // tests, where this one is about the progress lines.
    if env.is_mock() {
        sess.expect("bash completed:").unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("the hooked tool call should still run: {e}\nPTY screen:\n{screen}");
        });
    }

    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "a hooked turn should exit 0; got {exit}");
}

/// With no hook configured, none of that output appears.
///
/// The guard for the other direction: an assertion on `[hook` alone would pass
/// against a build that printed hook lines unconditionally, which would be its
/// own bug — every tool call in every session gaining two lines of noise.
#[test]
fn no_hook_configured_prints_no_hook_progress() {
    let env = TestEnv::new("hook-progress-absent");

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
    sess.set_default_timeout(Duration::from_secs(60));

    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "an unhooked turn should exit 0; got {exit}");

    let screen = sess.render(|s| s.contents());
    assert!(
        !screen.contains("[hook "),
        "no hook is configured, so no hook progress should be printed; PTY screen:\n{screen}"
    );
}
