//! PTY test for the `write_plan` tool — the plan file is the SSOT.
//!
//! The model calls `write_plan(content=...)`; the CLI writes that content to the
//! session plan file (here pointed at a temp file via `SUDOCODE_PLAN_FILE`) and
//! the plan text is the source of truth for approval/execution — not scraped
//! from chat. This test drives the deterministic mock scenario and asserts the
//! plan file holds the written content verbatim.
//!
//! ```bash
//! cargo test --test pty_write_plan                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_write_plan  # real API
//! ```

mod common;

use common::TestEnv;

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

    sess.expect("(?i)write_plan")
        .expect("model must invoke write_plan (agent trigger)");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "write_plan turn should exit 0; got {exit}");

    if env.is_mock() {
        let saved =
            common::read_file_with_retry(&plan_file, 10, std::time::Duration::from_millis(100))
                .expect("plan file should exist after write_plan");
        assert!(
            saved.contains("# Plan") && saved.contains("First step"),
            "plan file must hold the written content verbatim; got: {saved}"
        );
    }
}
