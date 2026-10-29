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

/// Exercise the real background monitor, including its post-exit persistence
/// loop. A protocol-level rejection alone cannot show that this worker retires.
#[test]
fn background_exit_after_external_session_deletion_retires_its_monitor() {
    use runtime::background_tasks::BackgroundTaskStatus;
    use runtime::fs_backend::StdFsBackend;
    use runtime::history::HistoryRecord;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CHILD: &str = "SCODE_DELETED_BACKGROUND_CHILD";
    fn wait_for(mut predicate: impl FnMut() -> bool, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out: {label}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        std::fs::write(root.join("ready"), b"ready").unwrap();
        wait_for(|| root.join("release").exists(), "native child release");
        std::fs::write(root.join("effect"), b"independent native write completed").unwrap();
        return;
    }
    struct Release(std::path::PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("release"), b"release");
        }
    }
    let root = std::env::temp_dir().join(format!("scode-deleted-monitor-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let release = Release(root.clone());
    let path = root.join("session.jsonl");
    let mut session = Session::new()
        .with_workspace_root(&root)
        .with_persistence_path(&path);
    std::fs::write(
        &path,
        HistoryRecord::initialize(&session)
            .unwrap()
            .encode()
            .unwrap(),
    )
    .unwrap();
    session.activate_persistence().unwrap();
    let journal = CompletionJournal::open(&session, &root, true).unwrap();
    journal.set_task_protocol(true);
    journal.begin("before-external-delete", None).unwrap();
    let tasks = BackgroundTasks::default()
        .with_completion(Some(Arc::clone(&journal)))
        .for_tool(None, Some("native-child"));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "background_exit_after_external_session_deletion_retires_its_monitor",
            "--nocapture",
        ])
        .env(CHILD, &root)
        .current_dir(&root);
    let (task, output) = runtime::launch_background_command(
        command,
        ShellKind::Bash,
        "native child",
        "deletion retirement acceptance",
        Some(&tasks),
    )
    .unwrap();
    wait_for(
        || root.join("ready").exists(),
        "real background child ready",
    );
    CompletionJournal::purge_stored(
        Arc::new(StdFsBackend),
        &path,
        &session.session_id,
        &root,
        &journal.storage_ref(),
        "external-delete-with-live-child",
        None,
    )
    .unwrap();
    let history = std::fs::read(&path).unwrap();
    let journals: Vec<_> = std::fs::read_dir(path.with_extension("completions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert!(!journals.is_empty());
    std::fs::write(root.join("release"), b"release").unwrap();
    wait_for(
        || {
            tasks
                .get(&task)
                .is_some_and(|task| task.status == BackgroundTaskStatus::Completed)
        },
        "background monitor retires after observing deletion",
    );
    assert_eq!(
        std::fs::read(root.join("effect")).unwrap(),
        b"independent native write completed"
    );
    assert_eq!(std::fs::read(&path).unwrap(), history);
    for (path, bytes) in journals {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    session.close_persistence();
    drop(journal);
    drop(tasks);
    wait_for(
        || !path.exists(),
        "automatic cleanup after the native monitor retires",
    );
    std::fs::remove_file(output).unwrap();
    drop(release);
    std::fs::remove_dir_all(root).unwrap();
}
