//! Terminal acceptance for shared column layout and structured styles.
mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use unicode_width::UnicodeWidthStr;

fn start(env: &TestEnv, background: &str) -> PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", background),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(60, 40).expect("resize");
    sess.expect("❯").expect("prompt");
    sess
}

fn finish(sess: &mut PtySession) {
    sess.send("/exit").expect("exit text");
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").expect("submit exit");
    assert_eq!(sess.expect_eof().expect("exit"), 0);
}

fn showcase(background: &str, link_color: &str, code_color: &str) {
    let env = TestEnv::new("unicode-layout");
    let mut sess = start(&env, background);
    // Live mode asks for the exact document too; both paths exercise the
    // production engine, Markdown renderer, inline history and terminal.
    let prompt = env.prompt(
        &format!(
            "Reply with exactly this Markdown, without an enclosing code fence:\n{}",
            mock_anthropic_service::UNICODE_SHOWCASE_DOC
        ),
        "unicode_rendering_showcase",
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .expect("paste prompt");
    // Wait for paste consumption before Enter, including its collapsed preview.
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("Reply with exactly"),
        common::DEFAULT_TIMEOUT,
        "prompt paste",
    );
    sess.send("\r").expect("submit");
    sess.set_default_timeout(if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        common::DEFAULT_TIMEOUT
    });
    // A live model may emit a thinking block before the requested text.
    sess.expect("•").expect("assistant response");
    sess.expect("CJK:").expect("CJK block after response start");
    sess.expect("👩🏽‍💻END")
        .expect("emoji cluster must not be split by a line break");
    common::expect_screen(
        &sess,
        |s| s.contains("Unicode done.") && s.contains("ctx "),
        Duration::from_secs(90),
        "completed unicode turn",
    );
    sess.render(|screen| {
        let contents = screen.raw().contents();
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let first = rows
            .iter()
            .rposition(|r| r.contains("CJK:"))
            .expect("response start");
        let prefix = if env.is_mock() { "• " } else { "  " };
        // In live mode thinking may own the initial bullet.
        assert!(
            rows[first] == format!("{prefix}CJK:{}", "界".repeat(17))
                || (env.is_live() && rows[first] == format!("• CJK:{}", "界".repeat(17)))
        );
        assert_eq!(
            rows[first + 1],
            format!("  {}", "界".repeat(13)),
            "wide continuation must retain its margin: {contents}"
        );
        let combining = rows
            .iter()
            .rposition(|r| r.contains("Combining:"))
            .expect("combining line");
        assert!(
            rows[combining + 1].starts_with("  e\u{301}END")
                || (env.is_live() && rows[combining + 1].starts_with("  éEND")),
            "combining sequence must stay attached: {contents}"
        );
        let table: Vec<_> = rows[first..]
            .iter()
            .filter(|r| {
                r.contains('│') && (r.contains("Key") || r.contains("中文") || r.contains("ASCII"))
            })
            .collect();
        assert_eq!(table.len(), 3, "table rows: {contents}");
        let borders: Vec<Vec<usize>> = table
            .iter()
            .map(|row| {
                row.match_indices('│')
                    .map(|(index, _)| row[..index].width())
                    .collect()
            })
            .collect();
        assert!(
            borders.windows(2).all(|pair| pair[0] == pair[1]),
            "table borders must align in columns: {borders:?}"
        );
        for (label, color) in [("LINK", link_color), ("CODE", code_color)] {
            let (row, text) = rows
                .iter()
                .enumerate()
                .rfind(|(_, r)| r.contains(label))
                .expect("styled label");
            let col = text[..text.find(label).unwrap()].width();
            let cell = screen
                .raw()
                .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                .unwrap();
            assert_eq!(format!("{:?}", cell.fgcolor()), color, "{label} color");
        }
    });
    finish(&mut sess);
}

#[test]
fn unicode_columns_and_semantic_colors_dark() {
    showcase("15;0", "Rgb(99, 168, 248)", "Rgb(166, 227, 161)");
}

#[test]
fn unicode_columns_and_semantic_colors_light() {
    showcase("0;15", "Rgb(28, 100, 200)", "Rgb(64, 160, 43)");
}

#[test]
fn wrapped_read_card_keeps_highlight_on_continuations() {
    let env = TestEnv::new("wrapped-read-style");
    std::fs::write(env.workspace_root().join("fixture.txt"), "X".repeat(160)).expect("fixture");
    let mut sess = start(&env, "15;0");
    let prompt = env.prompt("Read fixture.txt with the Read tool", "read_file_roundtrip");
    sess.send(&format!("{prompt}\r")).expect("submit read");
    common::expect_screen(
        &sess,
        |s| s.contains("XXXXX") && s.contains("ctx "),
        Duration::from_secs(90),
        "completed read",
    );
    sess.render(|screen| {
        let contents = screen.raw().contents();
        let mut colors = Vec::new();
        for (row, text) in screen.raw().rows(0, screen.raw().size().1).enumerate() {
            if text.starts_with("│ ") {
                if let Some(col) = text.find("XXXXX") {
                    let col = text[..col].width();
                    colors.push(format!(
                        "{:?}",
                        screen
                            .raw()
                            .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                            .unwrap()
                            .fgcolor()
                    ));
                }
            }
        }
        assert!(colors.len() >= 4, "wrapped card rows: {contents}");
        assert!(
            colors
                .iter()
                .all(|color| color != "Default" && color == &colors[0]),
            "continuation styles: {colors:?}"
        );
    });
    finish(&mut sess);
}

/// Run explicitly against each immutable release binary, outside a busy build.
/// This measures key -> parsed terminal cell, not physical display latency.
#[test]
#[ignore = "manual release A/B measurement"]
fn release_render_input_measurement() {
    let env = TestEnv::new("render-measurement");
    let mut sess = start(&env, "15;0");
    sess.resize(40, 120).expect("benchmark size");
    measure_keys(&mut sess, "idle");
    report_resources("idle");
    let prompt = env.prompt("Say hello", "delayed_text");
    sess.send(&format!("{prompt}\r")).expect("busy turn");
    // Wait until the request is in flight before measuring typing beside it.
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "submitted input");
    measure_keys(&mut sess, "request_in_flight");
    common::expect_screen(
        &sess,
        |s| s.contains("· turn 1"),
        Duration::from_secs(30),
        "first turn",
    );
    let started = Instant::now();
    for _ in 0..10 {
        let marker = common::turn_status_marker(&sess);
        let prompt = env.prompt(
            "Show a short Rust program in a code fence",
            "syntax_highlight_showcase",
        );
        sess.send(&format!("{prompt}\r")).expect("highlight turn");
        common::expect_turn_complete_after(
            &sess,
            &marker,
            Duration::from_secs(30),
            "highlight turn",
        );
    }
    println!(
        "RENDER_TURNS ten_highlight_turns_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    report_resources("after_ten_highlights");
    measure_keys(&mut sess, "after_ten_highlights");
    finish(&mut sess);
}

fn measure_keys(sess: &mut PtySession, phase: &str) {
    let mut expected = String::new();
    let mut samples = Vec::new();
    for _ in 0..60 {
        expected.push('x');
        let started = Instant::now();
        sess.send("x").expect("key");
        while !sess.render(|s| common::input_line_of(&s.raw().contents()).contains(&expected)) {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "input did not echo"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "RENDER_INPUT phase={phase} samples={} median_ms={:.3} p95_ms={:.3}",
        samples.len(),
        samples[30],
        samples[56]
    );
    sess.send("\x15").expect("clear draft");
    common::expect_screen(
        sess,
        |s| common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "draft cleared",
    );
}

#[cfg(unix)]
fn report_resources(phase: &str) {
    let output = std::process::Command::new("ps")
        .args(["-axo", "ppid=,rss=,time=,comm="])
        .output()
        .expect("process accounting");
    let parent = std::process::id().to_string();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 4 && fields[0] == parent && fields[3].contains("scode") {
            println!(
                "RENDER_RESOURCES phase={phase} rss_kib={} cpu_time={}",
                fields[1], fields[2]
            );
        }
    }
}

#[cfg(not(unix))]
fn report_resources(_phase: &str) {}

#[test]
#[ignore = "manual release A/B measurement"]
fn release_pending_diff_measurement() {
    let env = TestEnv::new("diff-measurement");
    std::fs::write(
        env.workspace_root().join("colors.rs"),
        format!(
            "fn main() {{\n{}\n}}\n",
            mock_anthropic_service::CODEX_DIFF_OLD
        ),
    )
    .unwrap();
    let config = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&config).unwrap();
    let hook = if cfg!(windows) {
        r#"if /I "%HOOK_TOOL_NAME%"=="edit_file" (for /L %i in (1,1,60) do @if not exist color-diff-release (ping -n 2 127.0.0.1 >nul))"#
    } else {
        r#"case "$HOOK_TOOL_NAME" in edit_file|Edit) while [ ! -f color-diff-release ]; do sleep 0.05; done;; esac"#
    };
    std::fs::write(
        config.join("settings.json"),
        serde_json::json!({"hooks":{"PreToolUse":[hook]}}).to_string(),
    )
    .unwrap();
    let started = Instant::now();
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", "15;0"),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(50, 120).unwrap();
    sess.expect("❯").unwrap();
    println!(
        "RENDER_STARTUP no_palette_reply_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let prompt = env.prompt(
        "Replace old_message with new_message in colors.rs and say Color diff done.",
        "codex_diff_showcase",
    );
    sess.send(&format!("{prompt}\r")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Editing colors.rs") && s.contains("NEW_MARKER"),
        common::LIVE_TURN_BUDGET,
        "pending diff",
    );
    report_resources("pending_diff_before_input");
    measure_keys(&mut sess, "pending_diff");
    report_resources("pending_diff_after_input");
    std::fs::write(env.workspace_root().join("color-diff-release"), "release").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Color diff done.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "edit finished",
    );
    finish(&mut sess);
}
