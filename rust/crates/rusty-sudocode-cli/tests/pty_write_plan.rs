//! PTY coverage of persisted `write_plan` drafts.
//!
//! The default draft path follows the restored session ID. `SUDOCODE_PLAN_FILE`
//! selects an explicit path. Both journeys exercise the CLI and verify the
//! saved Markdown, including headless review that requires human input.
//!
//! ```bash
//! cargo test --test pty_write_plan                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_write_plan  # real API
//! ```

mod common;

use common::TestEnv;
use runtime::{ContentBlock, Session, SessionStore};

#[test]
fn resumed_plan_uses_the_persisted_session_identity() {
    let env = TestEnv::new("resumed-plan-identity");
    let store = SessionStore::from_cwd(env.workspace_root()).unwrap();
    let session = Session::new().with_workspace_root(env.workspace_root().to_path_buf());
    let handle = store.create_handle(&session.session_id);
    session.save_to_path(&handle.path).unwrap();
    let prompt = env.prompt(
        "Call write_plan once to save a short Markdown plan. Include context, constraints, and acceptance criteria. Save the plan without executing its steps.",
        "write_plan_roundtrip",
    );
    let mut cli = env.spawn(&[
        "-p",
        &prompt,
        "--resume",
        &session.session_id,
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_plan",
    ]);
    assert_eq!(
        cli.expect_eof().unwrap(),
        1,
        "{}",
        common::screen_tail(&cli, 12000),
    );
    assert!(
        common::screen_tail(&cli, 12000).contains("needs_input"),
        "headless review must report its need for human input",
    );
    let restored = Session::load_from_path(&handle.path).unwrap();
    assert!(
        restored
            .messages()
            .iter()
            .any(|message| message.blocks.iter().any(
                |block| matches!(block, ContentBlock::ToolUse { name, .. } if name == "write_plan")
            )),
        "the model must invoke the plan tool"
    );
    let plan = env
        .workspace_root()
        .join(".sudocode")
        .join(format!("{}.plan.md", session.session_id));
    let saved = std::fs::read_to_string(&plan).expect("the resumed session owns its plan file");
    for heading in ["## Context", "## Constraints", "## Acceptance Criteria"] {
        assert!(
            saved.contains(heading),
            "missing {heading} in {}",
            plan.display()
        );
    }
    assert_eq!(store.list_sessions().unwrap().len(), 1);
}

#[test]
fn write_plan_persists_the_plan_file() {
    let env = TestEnv::new("write-plan");

    let plan_file = env.workspace_root().join("plan.md");

    let prompt = env.prompt(
        "Call write_plan with a short markdown plan as `content`. Just call the tool.",
        "write_plan_roundtrip",
    );

    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan",
            &prompt,
        ],
        &[("SUDOCODE_PLAN_FILE", plan_file.to_str().unwrap())],
    );

    sess.expect("(?i)write_?plan")
        .expect("model must invoke write_plan (agent trigger)");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "write_plan turn should exit 0; got {exit}");

    if env.is_mock() {
        let saved =
            common::read_file_with_retry(&plan_file, 10, std::time::Duration::from_millis(100))
                .expect("plan file should exist after write_plan");
        // The plan file is now always structured: the three framing sections
        // (from the required schema params) precede the plan body.
        assert!(
            saved.contains("## Context")
                && saved.contains("## Constraints")
                && saved.contains("## Acceptance Criteria"),
            "plan file must carry the composed framing sections; got: {saved}"
        );
        assert!(
            saved.contains("# Plan") && saved.contains("First step"),
            "plan file must hold the written body verbatim; got: {saved}"
        );
    }
}
