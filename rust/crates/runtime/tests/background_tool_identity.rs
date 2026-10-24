//! The tool identity a background launch requires.
//!
//! `launch_background_command` registers every terminal it starts in the
//! session's completion journal, and that registration is keyed by the TOOL
//! that asked for it. The key arrives out-of-band: the caller scopes its
//! registry handle with `for_tool(tool_use_id)` first. A caller that forgets
//! does not merely lose a journal entry — `prepare_terminal` returns
//! `Err("terminal tool identity missing")` and the launch FAILS.
//!
//! That failure is invisible in the easy configuration. With no journal bound,
//! or with the task protocol off, `prepare_terminal` short-circuits to
//! `Ok(None)` and an unscoped caller works fine.
//!
//! The protocol can only be switched on from an ACP session, which is why a
//! REPL/PTY test cannot reach this branch. The derivation is about the WRITER,
//! not the call sites: `set_task_protocol` is also called from
//! `engine-core/subagent_completion/control.rs` (in `install_journal` and in
//! the `CompletionAction::Configure` branch), but both only propagate
//! `delivery.tasks` — and the sole constructor of `Configure`, the only thing
//! that assigns that field, is `engine-acp/session_driver.rs`. So the test
//! below binds the journal and enables the protocol explicitly.

use std::path::Path;

use runtime::background_tasks::BackgroundTasks;
use runtime::subagent_completion::{CompletionJournal, ShellKind};
use runtime::Session;

/// Bind a journal with the task protocol enabled — the only configuration in
/// which the tool-identity requirement is live.
fn tasks_with_live_journal(cwd: &Path) -> BackgroundTasks {
    let session = Session::new()
        .with_workspace_root(cwd.to_path_buf())
        .with_persistence_path(cwd.join("session.jsonl"));
    let journal = CompletionJournal::open(&session, cwd, true).expect("open completion journal");
    journal.set_task_protocol(true);
    BackgroundTasks::default().with_completion(Some(journal))
}

#[test]
fn tool_identity_is_required_and_scoping_clears_that_gate() {
    let cwd = std::env::temp_dir().join(format!("scode-tool-identity-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("workspace");
    let tasks = tasks_with_live_journal(&cwd);

    // Unscoped: no tool identity, so the journal cannot attribute the terminal
    // and the launch is refused outright. This is the regression — a `Monitor`
    // that forgot `for_tool` could not start a watch at all.
    let refused = runtime::launch_background_command(
        runtime::shell_command_in(&cwd, "printf 'identity\\n'"),
        ShellKind::Bash,
        "printf 'identity\\n'",
        "unscoped probe",
        Some(&tasks),
    );
    let error = refused
        .expect_err("a launch with no tool identity must be refused, not silently unattributed")
        .to_string();
    assert!(
        error.contains("terminal tool identity missing"),
        "the refusal must name the missing identity; got {error}"
    );

    // Scoped exactly as the tool dispatcher does it. This synthetic journal has
    // no live completion source, so the launch still cannot finish — but the
    // refusal has MOVED PAST the identity gate, which is exactly what the fix
    // buys. Asserting the error changed (rather than asserting success) keeps
    // this test honest about what a journal without a running turn can prove.
    let scoped = tasks.for_tool(Some("msg_identity_probe"), Some("toolu_identity_probe"));
    let scoped_error = runtime::launch_background_command(
        runtime::shell_command_in(&cwd, "printf 'identity\\n'"),
        ShellKind::Bash,
        "printf 'identity\\n'",
        "scoped probe",
        Some(&scoped),
    )
    .expect_err("this synthetic journal has no active completion source")
    .to_string();
    assert!(
        !scoped_error.contains("terminal tool identity missing"),
        "with `for_tool` supplied, the identity gate must no longer be the \
         blocker; got {scoped_error}"
    );
    assert!(
        scoped_error.contains("no active completion source"),
        "the remaining failure should be the missing live source, not identity; \
         got {scoped_error}"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}
