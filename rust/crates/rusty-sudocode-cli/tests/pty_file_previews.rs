//! Real file writes, shared colored previews, draft preservation and bounded history.
mod common;

use common::TestEnv;
use serde_json::json;
use unicode_width::UnicodeWidthStr;

fn write_preview(light: bool, no_color: bool) {
    let env = TestEnv::new("file-previews");
    let content =
        "fn main() {\n    let message = \"PreviewWrite中文\";\n    println!(\"{message}\");\n}\n";
    let root = env.workspace_root();
    let config = root.join(".nexus/sudocode");
    std::fs::create_dir_all(&config).unwrap();
    let hook = if cfg!(windows) {
        r#"if /I "%HOOK_TOOL_NAME%"=="write_file" (for /L %i in (1,1,60) do @if not exist write-stage-release (ping -n 2 127.0.0.1 >nul))"#
    } else {
        r#"case "$HOOK_TOOL_NAME" in write_file|Write) while [ ! -f write-stage-release ]; do sleep 0.05; done;; esac"#
    };
    std::fs::write(
        config.join("settings.json"),
        json!({"hooks":{"PreToolUse":[hook]}}).to_string(),
    )
    .unwrap();
    let mut session = env.spawn_with_env(
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "write_file",
        ],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", if light { "0;15" } else { "15;0" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", if no_color { "1" } else { "" }),
        ],
    );
    session.resize(70, 100).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "file preview prompt");
    let marker = common::turn_status_marker(&session);
    let prompt = if env.is_mock() {
        env.tool_batch_prompt(&json!([{"id":"preview-write", "name":"write_file", "input":{"path":"preview.rs", "content":content}}]))
    } else {
        format!("Use write_file exactly once to create preview.rs with this exact content, then stop.\n{content}")
    };
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        &session,
        |screen| {
            screen.contains("TOOL_BATCH:")
                || screen.contains("Pasted")
                || screen.contains("Use write_file")
        },
        common::DEFAULT_TIMEOUT,
        "write prompt consumed",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |screen| {
            screen.contains("Write ")
                && screen.contains("preview.rs")
                && screen.contains("PreviewWrite中文")
        },
        common::LIVE_TURN_BUDGET,
        "pending new-file source preview",
    );
    assert!(
        !root.join("preview.rs").exists(),
        "inspect the actual pre-write preview"
    );
    session.send("draft界").unwrap();
    let mut draft = String::from("draft界");
    for (width, key) in [(66, "1"), (94, "2"), (80, "3")] {
        session.resize(70, width).unwrap();
        session.send(key).unwrap();
        draft.push_str(key);
        common::expect_input_line(
            &session,
            &draft,
            common::DEFAULT_TIMEOUT,
            "typing during write preview resize",
        );
    }
    std::fs::write(root.join("write-stage-release"), "release").unwrap();
    common::expect_turn_complete_with_input(
        &session,
        &marker,
        &draft,
        common::LIVE_TURN_BUDGET,
        "actual write completed",
    );
    assert_eq!(
        std::fs::read_to_string(root.join("preview.rs")).unwrap(),
        content
    );
    common::expect_screen(
        &session,
        |screen| {
            screen.contains("Wrote ")
                && screen.contains("PreviewWrite中文")
                && common::input_line_of(screen) == draft
        },
        common::DEFAULT_TIMEOUT,
        "completed preview preserves draft",
    );
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex_default_colors.json")).unwrap();
    session.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        let heading = rows
            .iter()
            .rposition(|row| row.contains("Wrote ") && row.contains("preview.rs"))
            .unwrap();
        let row = (heading + 1..rows.len())
            .find(|&row| rows[row].contains("PreviewWrite中文"))
            .expect("content belongs to the Write card");
        assert!(
            rows[heading + 1].contains("1 +fn main()"),
            "numbered additions: {rows:?}"
        );
        let col = rows[row][..rows[row].find("PreviewWrite").unwrap()].width();
        let cell = raw
            .cell(row.try_into().unwrap(), col.try_into().unwrap())
            .unwrap();
        assert_eq!(
            format!("{:?}", cell.fgcolor()),
            if no_color {
                "Default"
            } else {
                reference["themes"][usize::from(light)]["diff_added_string"]
                    .as_str()
                    .unwrap()
            }
        );
        assert_eq!(
            format!("{:?}", cell.bgcolor()),
            if no_color {
                "Default"
            } else if light {
                "Rgb(218, 251, 225)"
            } else {
                "Rgb(33, 58, 43)"
            }
        );
    });
    session.send("\x15/exit").unwrap();
    common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "exit preview");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn dark_write_preview_matches_edit_colors_and_preserves_draft() {
    write_preview(false, false);
}
#[test]
fn light_write_preview_matches_edit_colors_and_preserves_draft() {
    write_preview(true, false);
}
#[test]
fn colorless_write_keeps_numbered_source() {
    write_preview(false, true);
}

#[test]
fn markdown_source_and_long_file_preview_are_bounded() {
    let env = TestEnv::new_mock("bounded-write-preview");
    let markdown = "# LiteralHeading\n- `literal_code`\n[source](https://example.com/source)\n";
    let long = format!(
        "{}\n{}\nHiddenWriteTail",
        "界👩🏽‍💻".repeat(2000),
        "visible_line\n".repeat(1000)
    );
    std::fs::create_dir(env.workspace_root().join("not-a-file")).unwrap();
    let calls = json!([
        {"id":"markdown-write", "name":"write_file", "input":{"path":"source.md", "content":markdown}},
        {"id":"long-write", "name":"write_file", "input":{"path":"long.txt", "content":long}},
        {"id":"empty-write", "name":"write_file", "input":{"path":"empty.txt", "content":""}},
        {"id":"failed-write", "name":"write_file", "input":{"path":"not-a-file", "content":"NotCommittedSource"}}
    ]);
    let mut session = env.spawn(&["--permission-mode", "danger-full-access"]);
    session.resize(100, 90).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "bounded write prompt");
    let marker = common::turn_status_marker(&session);
    let prompt = env.tool_batch_prompt(&calls);
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        &session,
        |screen| screen.contains("TOOL_BATCH:"),
        common::DEFAULT_TIMEOUT,
        "tool fixture prompt consumed",
    );
    session.send("\r").unwrap();
    common::expect_turn_complete_after(
        &session,
        &marker,
        common::DEFAULT_TIMEOUT,
        "bounded file writes",
    );
    session.render(|screen| {
        let text = screen.contents();
        assert!(
            text.contains("# LiteralHeading") && text.contains("`literal_code`"),
            "Markdown files show their source: {text}"
        );
        assert!(
            text.contains("more lines") && text.contains("truncated for display"),
            "explicit line and character limits: {text}"
        );
        assert!(
            text.contains("Input preview") && !text.contains("long.txt (0 lines)"),
            "offloaded tool results retain a labelled, nonempty input preview: {text}"
        );
        assert!(
            !text.contains("HiddenWriteTail"),
            "hidden source must not become scrollback: {text}"
        );
        assert!(
            text.contains("Write failed") && text.contains("not-a-file"),
            "failed writes retain their identity without claiming success: {text}"
        );
        assert!(
            !text.contains("NotCommittedSource"),
            "a failed write must not commit its proposed source to history: {text}"
        );
    });
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("source.md")).unwrap(),
        markdown
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("long.txt")).unwrap(),
        long
    );
    assert_eq!(
        std::fs::read(env.workspace_root().join("empty.txt")).unwrap(),
        b""
    );
    assert!(env.workspace_root().join("not-a-file").is_dir());
    session.send("/exit").unwrap();
    common::expect_input_line(
        &session,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "exit bounded previews",
    );
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}
