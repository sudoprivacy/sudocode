//! Compare the real CLI's terminal rows with independent upstream Codex snapshots.
mod common;

use common::TestEnv;
use serde_json::json;
use std::time::{Duration, Instant};

fn wait_file(path: &std::path::Path) {
    let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
    while !path.exists() {
        assert!(Instant::now() < deadline, "waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn wide_grid_and_narrow_records_match_codex_snapshots() {
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex_tables.json")).unwrap();
    for case in reference["cases"].as_array().unwrap() {
        let env = TestEnv::new_mock("codex-table-reference");
        let root = env.workspace_root();
        let mut session = env.spawn_with_env(&[], &[("NO_COLOR", "1")]);
        session.resize(90, 100).unwrap();
        common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "table prompt ready");
        let marker = common::turn_status_marker(&session);
        let document = format!(
            "{}\n\nTableSnapshotDone.",
            case["markdown"].as_str().unwrap()
        );
        let prompt = common::terminal_host::prose_prompt(
            &env,
            &document,
            json!({
                "start_ready":root.join("table-start-ready"), "start_release":root.join("table-start-release"),
                "chunk_chars":document.chars().count(),
            }),
        );
        session.send(&format!("{prompt}\r")).unwrap();
        wait_file(&root.join("table-start-ready"));
        let width = u16::try_from(case["width"].as_u64().unwrap())
            .expect("Codex fixture width fits u16")
            .checked_add(2)
            .expect("terminal width includes the prompt");
        session.resize(90, width).unwrap();
        std::fs::write(root.join("table-start-release"), "release").unwrap();
        let expected: Vec<_> = case["expected"]
            .as_str()
            .unwrap()
            .lines()
            .map(str::trim_end)
            .collect();
        common::expect_screen(
            &session,
            |_| {
                session.render(|screen| {
                    let rows: Vec<String> = screen
                        .raw()
                        .rows(0, screen.raw().size().1)
                        .map(|row| {
                            row.chars()
                                .skip(2)
                                .collect::<String>()
                                .trim_end()
                                .to_string()
                        })
                        .collect();
                    rows.windows(expected.len())
                        .any(|rows| rows.iter().map(String::as_str).eq(expected.iter().copied()))
                })
            },
            common::DEFAULT_TIMEOUT,
            &format!("exact upstream table snapshot {}", case["name"]),
        );
        wait_file(&root.join("prose-ready"));
        std::fs::write(root.join("prose-release"), "release").unwrap();
        session.resize(90, 100).unwrap();
        common::expect_turn_complete_after(
            &session,
            &marker,
            common::DEFAULT_TIMEOUT,
            "table turn completed",
        );
        session.send("/exit").unwrap();
        common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "table exit");
        session.send("\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
    }
}
