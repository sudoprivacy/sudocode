//! Exercise the shared edit contract through the CLI and persisted tool results.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, SessionStore};
use serde_json::json;
use std::fs;

#[test]
fn ambiguous_edits_preserve_bytes_and_can_be_repaired() {
    let env = TestEnv::new_mock("edit-uniqueness");
    let fixtures = [
        (
            "lf.txt",
            "head\n重复\n重复\ntail\n",
            "重复\n",
            "head\n重复\n",
            "head\n修复\n",
        ),
        (
            "crlf.txt",
            "head\r\n重复\r\n重复\r\ntail\r\n",
            "重复\n",
            "head\n重复\n",
            "head\n修复\n",
        ),
        ("overlap.txt", "aaa", "aa", "aaa", "fixed"),
    ];
    for (path, content, ..) in fixtures {
        fs::write(env.workspace_root().join(path), content).unwrap();
    }
    fs::write(env.workspace_root().join("empty.txt"), "untouched").unwrap();
    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "edit_file",
    ]);
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "initial prompt");
    let mut ambiguous: Vec<_> = fixtures.iter().map(|(path, _, old, ..)| json!({"id": format!("ambiguous-{path}"), "name": "edit_file", "input": {"path": path, "old_string": old, "new_string": "WRONG"}})).collect();
    for is_all in [false, true] {
        ambiguous.push(json!({"id": format!("empty-{is_all}"), "name": "edit_file", "input": {"path": "empty.txt", "old_string": "", "new_string": "WRONG", "replace_all": is_all}}));
    }
    run_edit_batch(&env, &mut sess, "reject ambiguous and empty", &ambiguous);
    for (path, content, ..) in fixtures {
        assert_eq!(
            fs::read(env.workspace_root().join(path)).unwrap(),
            content.as_bytes()
        );
    }
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("empty.txt")).unwrap(),
        "untouched"
    );
    run_edit_batch(&env, &mut sess, "repair with unique context", &fixtures.iter().map(|(path, _, _, old, new)| json!({"id": format!("unique-{path}"), "name": "edit_file", "input": {"path": path, "old_string": old, "new_string": new}})).collect::<Vec<_>>());
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("lf.txt")).unwrap(),
        "head\n修复\n重复\ntail\n"
    );
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("crlf.txt")).unwrap(),
        "head\r\n修复\r\n重复\r\ntail\r\n"
    );
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("overlap.txt")).unwrap(),
        "fixed"
    );
    run_edit_batch(
        &env,
        &mut sess,
        "replace every remaining occurrence",
        &[
            json!({"id": "all", "name": "edit_file", "input": {"path": "lf.txt", "old_string": "\n", "new_string": "|", "replace_all": true}}),
        ],
    );
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("lf.txt")).unwrap(),
        "head|修复|重复|tail|"
    );
    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    assert_saved_edit_results(&env);
}

#[test]
fn live_model_recovers_with_unique_context() {
    let env = TestEnv::new("edit-unique-context-live");
    if !env.is_live() {
        return;
    }
    fs::write(
        env.workspace_root().join("repeat.txt"),
        "first=alpha\nsecond=alpha\n",
    )
    .unwrap();
    let prompt = "In repeat.txt change only second=alpha to second=omega using edit_file. Include enough surrounding context to uniquely match; preserve the first line. Read back and verify the final content, then say EDIT_VERIFIED.";
    let mut sess = env.spawn(&[
        "-p",
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "read_file,edit_file",
        prompt,
    ]);
    sess.set_default_timeout(common::LIVE_TURN_BUDGET);
    let exit = sess.expect_eof().unwrap_or_else(|error| {
        panic!(
            "live edit did not finish: {error}; screen: {}",
            sess.render(common::screen_contents)
        )
    });
    assert_eq!(exit, 0);
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("repeat.txt")).unwrap(),
        "first=alpha\nsecond=omega\n"
    );
}

fn run_edit_batch(
    env: &TestEnv,
    sess: &mut pty_expect::PtySession,
    label: &str,
    calls: &[serde_json::Value],
) {
    let prompt = env.prompt(
        &format!(
            "Perform the {label} edits exactly as specified. TOOL_BATCH:{}",
            serde_json::to_string(calls).unwrap()
        ),
        "tool_concurrency",
    );
    let marker = common::turn_status_marker(sess);
    sess.send(&prompt).unwrap();
    common::expect_screen_settled(
        sess,
        |s| common::input_line_of(s).contains(label),
        common::DEFAULT_TIMEOUT,
        "edit batch pasted",
    );
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(sess, &marker, env.timeout(), label);
}

fn assert_saved_edit_results(env: &TestEnv) {
    let saved = SessionStore::from_cwd(env.workspace_root())
        .unwrap()
        .load_session("latest")
        .unwrap()
        .session;
    let results: Vec<_> = saved
        .messages()
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                output,
                is_error,
                ..
            } => Some((tool_use_id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 9);
    for (id, output, is_error) in results {
        if id.starts_with("ambiguous-") {
            assert!(
                is_error && output.contains("more than once"),
                "{id}: {output}"
            );
        } else if id.starts_with("empty-") {
            assert!(
                is_error && output.contains("must not be empty"),
                "{id}: {output}"
            );
        } else {
            assert!(!is_error, "{id}: {output}");
        }
    }
}
