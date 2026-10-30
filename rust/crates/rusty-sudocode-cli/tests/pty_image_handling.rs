//! Image input and screenshot feedback exercised through the real CLI in a PTY.
//! Assertions inspect captured provider requests, including exact image bytes,
//! tool-result ordering, permission failures, session replay, and text-only rejection.
//! The ignored browser case additionally requires installed ai-dev-browser and Chrome.
//!
//! Run normal coverage with `cargo test --test pty_image_handling`; add
//! `-- --include-ignored` to exercise the local browser as well.

mod common;
#[path = "support/image_fixture.rs"]
mod image_fixture;

use std::fs;

use common::TestEnv;
use image_fixture::{mark_model_text_only, write_fixture_png};

// ──────────────────────────────────────────────────────────────────────
// 1. CLI @image reference — full round trip
// ──────────────────────────────────────────────────────────────────────

/// `scode "@image.png — describe"` reads the PNG through the @-file
/// resolver, ships it to the backend, prints a response, exits 0.
///
/// Regression guard: pre-#258 the CLI silently dropped image blocks
/// when the model was text-only. Now it should either succeed (native
/// pass-through) or route via VLM.
#[test]
fn cli_at_image_reference_completes_turn() {
    let env = TestEnv::new("image-cli-at-ref");
    let fixture = env.workspace_root().join("shapes.png");
    write_fixture_png(&fixture);

    // Sanity — the fixture wrote something sane.
    let png_bytes = fs::read(&fixture).expect("read back fixture");
    assert!(
        png_bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47]),
        "fixture must be a valid PNG (starts with the PNG signature)"
    );
    assert!(
        png_bytes.len() > 100,
        "fixture too small: {}",
        png_bytes.len()
    );

    let prompt = env.prompt(
        &format!(
            "Describe what shapes and colors are in this image in ONE short sentence: @{}",
            fixture.display()
        ),
        "single_turn_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    // Response must land — the specific text varies by backend. In mock
    // mode the `single_turn_text` scenario canned reply is "The answer
    // is 4"; in live mode any assistant text works. We key on either
    // pattern so both modes exercise the same assertion path (DRY).
    // A vision turn — upload plus description — outlasts the default live timeout, so
    // both the content wait and the exit get their own, larger budget. (The
    // content pattern is also satisfied by the echo of the prompt, which names
    // the image and its shapes, so it can return while the turn still runs;
    // the exit is what actually proves the turn finished.)
    sess.set_default_timeout(std::time::Duration::from_mins(3));
    sess.expect("(?i)(answer|image|shape|color|red|blue|yellow|circle|rectangle|square)")
        .expect("scode should produce some assistant content on stdout");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "cli image turn should exit 0; got {exit}");

    // Mock-only: verify the message was actually shipped through the
    // backend. If push_images silently dropped the image block the
    // request would never leave scode — this catches that regression.
    if env.is_mock() {
        use base64::Engine as _;
        let bodies = env.captured_message_bodies();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        assert!(
            bodies.iter().any(|body| {
                let request: serde_json::Value = serde_json::from_str(body).unwrap();
                request["messages"].as_array().unwrap().iter().any(|m| {
                    m["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|b| {
                            b["type"] == "image"
                                && b["source"]["media_type"] == "image/png"
                                && b["source"]["data"] == encoded
                        })
                    })
                })
            }),
            "CLI must send the PNG bytes as an image, not merely a textual @path: {bodies:?}"
        );
    }
}

fn captured_blocks(env: &TestEnv) -> Vec<serde_json::Value> {
    env.captured_message_bodies()
        .iter()
        .map(|body| {
            let value: serde_json::Value = serde_json::from_str(body).unwrap();
            value["messages"].as_array().unwrap().last().unwrap()["content"].clone()
        })
        .collect()
}

fn seed_read_fixture(env: &TestEnv) -> Vec<u8> {
    let path = env.workspace_root().join("screen.png");
    write_fixture_png(&path);
    fs::write(
        env.workspace_root().join("fixture.txt"),
        "ordinary text survives",
    )
    .unwrap();
    fs::read(path).unwrap()
}

#[test]
fn read_image_and_text_batch_sends_real_pixels_and_survives_resume() {
    use base64::Engine as _;
    let env = TestEnv::new("read-image-batch");
    let bytes = seed_read_fixture(&env);
    let prompt = env.prompt("Read screen.png as an image and fixture.txt as text, then describe the shapes and colors. Use Read for both files.", "image_read_roundtrip");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    if env.is_mock() {
        let requests = captured_blocks(&env);
        let blocks = requests.last().unwrap().as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "image-1");
        assert_eq!(blocks[1]["tool_use_id"], "text-2");
        assert_eq!(blocks[2]["type"], "image");
        assert_eq!(blocks[2]["source"]["media_type"], "image/png");
        assert_eq!(
            blocks[2]["source"]["data"],
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert!(
            !blocks[0].to_string().contains("iVBOR"),
            "base64 must not be tool text"
        );
        assert!(blocks[1].to_string().contains("ordinary text survives"));
    }
    // The next process has no screenshot file to reopen: pixels must replay
    // from the saved transcript, not an ephemeral tool-output side channel.
    fs::remove_file(env.workspace_root().join("screen.png")).unwrap();
    let prompt = env.prompt("Describe the previous image.", "single_turn_text");
    let mut sess = env.spawn(&["--resume", "latest", "--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&sess, env.timeout(), "resumed prompt ready");
    let marker = common::turn_status_marker(&sess);
    sess.send(&prompt).unwrap();
    common::expect_input_line(&sess, &prompt, env.timeout(), "resumed prompt typed");
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(&sess, &marker, env.timeout(), "resumed image turn");
    sess.send("/exit\r").unwrap();
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    if env.is_mock() {
        let body = env.captured_message_bodies().last().unwrap().clone();
        assert!(
            body.contains(&base64::engine::general_purpose::STANDARD.encode(bytes)),
            "resume lost image bytes"
        );
    }
}

#[test]
fn corrupt_image_is_a_tool_error_and_other_batch_results_survive() {
    let env = TestEnv::new("read-corrupt-image");
    seed_read_fixture(&env);
    fs::write(env.workspace_root().join("screen.png"), b"not an image").unwrap();
    let prompt = env.prompt(
        "Read screen.png and fixture.txt using Read. Report the image error and the text contents.",
        "image_read_roundtrip",
    );
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    if env.is_mock() {
        let requests = captured_blocks(&env);
        let blocks = requests.last().unwrap().as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["is_error"], true);
        assert!(blocks[1].to_string().contains("ordinary text survives"));
    }
}

#[test]
fn disabled_read_cannot_attach_pixels() {
    let env = TestEnv::new("image-read-disabled");
    seed_read_fixture(&env);
    let prompt = env.prompt(
        "Describe screen.png if a permitted tool can read it; otherwise explain the limitation.",
        "image_read_roundtrip",
    );
    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--allowedTools",
        "glob_search",
        &prompt,
    ]);
    if env.is_live() {
        sess.set_default_timeout(common::LIVE_TURN_BUDGET);
    }
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    if env.is_mock() {
        let requests = captured_blocks(&env);
        let blocks = requests.last().unwrap().as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert!(blocks
            .iter()
            .all(|b| b["type"] == "tool_result" && b["is_error"] == true));
        assert!(!env.captured_message_bodies().join("").contains("iVBOR"));
    }
}

#[test]
fn quoted_cli_image_path_is_attached_once() {
    let env = TestEnv::new("image-quoted-path");
    let path = env.workspace_root().join("screen shot.png");
    write_fixture_png(&path);
    let prompt = env.prompt("Describe the shapes in @\"screen shot.png\". It is the same picture as @'screen shot.png'.", "single_turn_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    if env.is_live() {
        sess.set_default_timeout(common::LIVE_TURN_BUDGET);
    }
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    if env.is_mock() {
        let requests = captured_blocks(&env);
        let blocks = requests.first().unwrap().as_array().unwrap();
        assert_eq!(blocks.iter().filter(|b| b["type"] == "image").count(), 1);
    }
}

#[test]
fn missing_cli_image_fails_before_model_request() {
    let env = TestEnv::new("missing-image");
    let prompt = env.prompt("Describe @absent.png", "single_turn_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    assert_ne!(sess.expect_eof().unwrap(), 0);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

struct ReleaseImageTool(std::path::PathBuf);

impl Drop for ReleaseImageTool {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
}

fn start_image_tool(env: &TestEnv) -> pty_expect::PtySession {
    use serde_json::json;

    fs::write(
        env.workspace_root().join("image-wait.sh"),
        "printf started > image-tool-started; while test ! -f image-tool-release; do sleep 0.1; done; printf 'IMAGE_TOOL_RESULT\\n'",
    )
    .unwrap();
    let mut session = env.spawn_with_env(
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "Bash",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 160).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "image test input");
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"image-held-tool", "name":"Bash", "input":{"command":"sh image-wait.sh", "timeout":common::LIVE_TURN_BUDGET.as_millis()*3}}]),
        )
    } else {
        "Run sh image-wait.sh exactly once in the foreground. Do not create image-tool-release. When it completes, follow the latest user message without using more tools.".into()
    };
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        &session,
        |view| !common::input_line_of(view).is_empty(),
        common::DEFAULT_TIMEOUT,
        "initial image test request",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |_| env.workspace_root().join("image-tool-started").exists(),
        common::LIVE_TURN_BUDGET,
        "image test tool is running",
    );
    session
}

fn steering_image_reference(queue_before_flush: bool) {
    use base64::Engine as _;

    let env = TestEnv::new(if queue_before_flush {
        "image-queue-flush"
    } else {
        "image-enter-steer"
    });
    let image = env.workspace_root().join("image fixture.png");
    write_fixture_png(&image);
    let expected = base64::engine::general_purpose::STANDARD.encode(fs::read(&image).unwrap());
    let release = env.workspace_root().join("image-tool-release");
    let _release = ReleaseImageTool(release.clone());
    let mut session = start_image_tool(&env);
    let input =
        "Describe this image in one short English sentence: @\"image fixture.png\" @\"image fixture.png\"";
    session.send(&format!("\x1b[200~{input}\x1b[201~")).unwrap();
    common::expect_input_line(
        &session,
        input,
        common::DEFAULT_TIMEOUT,
        "image steering draft",
    );
    session
        .send(if queue_before_flush { "\t" } else { "\r" })
        .unwrap();
    if queue_before_flush {
        common::expect_screen(
            &session,
            |view| view.contains("queued:"),
            common::DEFAULT_TIMEOUT,
            "image explicitly queued",
        );
        session.send("\x1b").unwrap();
    }
    common::expect_screen(
        &session,
        |view| view.contains("steering:"),
        common::DEFAULT_TIMEOUT,
        "image steered during the original tool",
    );
    fs::write(&release, "release").unwrap();
    common::expect_screen(
        &session,
        |view| {
            view.contains("IMAGE_TOOL_RESULT")
                && view.contains("ctx ")
                && !view.contains("steering:")
        },
        common::LIVE_TURN_BUDGET,
        "image follow-up completes without cancelling the original tool",
    );
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 2);
        let content = captured_blocks(&env).pop().unwrap();
        let images: Vec<_> = content
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block["type"] == "image")
            .collect();
        assert_eq!(
            images.len(),
            1,
            "steering must attach native pixels once: {content}"
        );
        assert_eq!(images[0]["source"]["data"], expected);
    } else {
        common::expect_screen(
            &session,
            |view| {
                view.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
                    ["red", "cyan", "rectangle", "circle", "yellow"]
                        .iter()
                        .any(|expected| word.eq_ignore_ascii_case(expected))
                })
            },
            common::LIVE_TURN_BUDGET,
            "the live model describes the actual pixels without Read",
        );
    }
    session.render(|screen| assert!(!screen.contents().contains("Cancelled")));
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn steering_image_on_enter_preserves_native_pixels() {
    steering_image_reference(false);
}

#[test]
fn steering_image_on_queue_flush_preserves_native_pixels() {
    steering_image_reference(true);
}

#[test]
fn steering_image_rejects_text_only_models_before_another_provider_request() {
    let env = TestEnv::new("steering-image-text-only");
    if !env.is_mock() {
        return; // The capability override belongs to the deterministic provider fixture.
    }
    mark_model_text_only(&env);
    write_fixture_png(&env.workspace_root().join("image fixture.png"));
    let release = env.workspace_root().join("image-tool-release");
    let _release = ReleaseImageTool(release.clone());
    let mut session = start_image_tool(&env);
    let input = "Describe @\"image fixture.png\"";
    session.send(&format!("\x1b[200~{input}\x1b[201~")).unwrap();
    common::expect_input_line(
        &session,
        input,
        common::DEFAULT_TIMEOUT,
        "text-only image draft",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("steering:"),
        common::DEFAULT_TIMEOUT,
        "text-only image is staged",
    );
    fs::write(&release, "release").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("does not support image input"),
        common::DEFAULT_TIMEOUT,
        "steering shares initial-input capability rejection",
    );
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    assert_eq!(
        env.captured_message_count(),
        1,
        "an unsupported image must not reach the provider"
    );
}

#[path = "common/openai_compat_mock.rs"]
mod image_vlm_mock;

#[test]
fn text_only_model_rejects_images_without_calling_configured_vlm() {
    let env = TestEnv::new("read-image-text-only");
    if !env.is_mock() {
        return; // This test needs deterministic capability and provider fixtures.
    }
    seed_read_fixture(&env);
    mark_model_text_only(&env);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let vlm = rt
        .block_on(image_vlm_mock::OpenAiCompatMock::spawn(
            "UNEXPECTED_VLM_CALL",
        ))
        .unwrap();
    let path = env.config_home().join("sudocode.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    config["auth_modes"]["proxy"]["sudorouter"] =
        serde_json::json!({"baseUrl": vlm.base_url(), "apiKey": "test-image-key"});
    fs::write(path, serde_json::to_string(&config).unwrap()).unwrap();

    // Both the serial and parallel tool paths must return an explicit tool
    // error without pixels or a hidden request to the configured vision service.
    for marker in ["", " IMAGE_SINGLE"] {
        let prompt = env.prompt(
            &format!("Read screen.png and fixture.txt, then describe the image.{marker}"),
            "image_read_roundtrip",
        );
        let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
        assert_eq!(
            sess.expect_eof().unwrap(),
            0,
            "{}",
            sess.render(common::screen_contents)
        );
        let requests = captured_blocks(&env);
        let blocks = requests.last().unwrap().as_array().unwrap();
        assert!(blocks.iter().all(|b| b["type"] != "image"));
        let result = blocks
            .iter()
            .find(|b| b["tool_use_id"] == "image-1")
            .unwrap();
        assert_eq!(result["is_error"], true);
        assert!(result
            .to_string()
            .contains("Switch to a vision-capable model"));
        if marker.is_empty() {
            assert!(blocks.iter().any(|b| b["tool_use_id"] == "text-2"
                && b.to_string().contains("ordinary text survives")));
        }
    }

    let before = env.captured_message_count();
    let prompt = env.prompt("Describe @screen.png", "single_turn_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    sess.expect("Switch to a vision-capable model").unwrap();
    sess.expect_eof().unwrap();
    assert_eq!(
        env.captured_message_count(),
        before,
        "rejected CLI image must not reach the main model"
    );
    let requests = rt.block_on(vlm.captured_requests());
    assert!(
        requests.iter().all(|r| r.method != "POST"),
        "no vision side call is allowed: {requests:?}"
    );
}

#[test]
fn serial_read_attaches_image() {
    let env = TestEnv::new("image-serial");
    seed_read_fixture(&env);
    let prompt = env.prompt(
        "Use Read to inspect screen.png and describe its shapes. IMAGE_SINGLE",
        "image_read_roundtrip",
    );
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    if env.is_mock() {
        let requests = captured_blocks(&env);
        let blocks = requests.last().unwrap().as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "image");
    }
}

/// External prerequisites are optional for a scode installation. An explicit
/// run fails if they are absent; it never silently skips the actual browser.
#[test]
#[ignore = "requires ai-dev-browser and Chrome"]
fn real_browser_screenshot_reaches_model_as_pixels() {
    use base64::Engine as _;
    let env = TestEnv::new("browser-image-feedback");
    let python = std::env::var("SCODE_BROWSER_PYTHON").unwrap_or_else(|_| "python3".into());
    let lookup = std::process::Command::new(&python)
        .args(["-c", "import ai_dev_browser"])
        .output()
        .expect("set SCODE_BROWSER_PYTHON to an interpreter with ai-dev-browser installed");
    assert!(
        lookup.status.success(),
        "install ai-dev-browser in SCODE_BROWSER_PYTHON before explicitly running this test"
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let python = if cfg!(windows) {
        python.replace('\\', "/")
    } else {
        python
    };
    let quoted = format!("'{}'", python.replace('\'', "'\\''"));
    fs::write(env.workspace_root().join("capture.sh"), format!(
        "set -eu\ntrap \"{quoted} -m ai_dev_browser.tools.browser_stop --port {port}\" EXIT\n{quoted} -m ai_dev_browser.tools.browser_start --headless true --silent-stderr --port {port} --url 'data:text/html,<title>Pixel feedback</title><h1 style=\"color:red\">SCREENSHOT-917</h1>'\n{quoted} -m ai_dev_browser.tools.page_wait_ready --port {port}\n{quoted} -m ai_dev_browser.tools.page_screenshot --port {port} --path screen.png\n"
    )).unwrap();
    let prompt = env.prompt("Run bash capture.sh to take a real browser screenshot, then use Read on screen.png to inspect its pixels and describe what you see. Do not inspect capture.sh or page HTML.", "browser_image_roundtrip");
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    sess.set_default_timeout(std::time::Duration::from_mins(2));
    assert_eq!(
        sess.expect_eof().unwrap(),
        0,
        "{}",
        sess.render(common::screen_contents)
    );
    let png = fs::read(env.workspace_root().join("screen.png")).unwrap_or_else(|error| {
        panic!(
            "actual screenshot missing: {error}; PTY: {}",
            sess.render(common::screen_contents)
        )
    });
    assert!(png.starts_with(b"\x89PNG"));
    if env.is_mock() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let bodies = env.captured_message_bodies();
        assert_eq!(bodies.len(), 3, "capture, Read, then visual follow-up");
        let request: serde_json::Value = serde_json::from_str(bodies.last().unwrap()).unwrap();
        let image = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().unwrap())
            .find(|b| b["type"] == "image")
            .expect("image block on model wire");
        assert_eq!(
            image["source"]["data"], encoded,
            "must be the browser's exact pixels"
        );
    }
}
