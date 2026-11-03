//! Clipboard image paste in both REPLs, exercised through the real CLI in a
//! PTY. The clipboard is the documented `SUDOCODE_CLIPBOARD_IMAGE` source, so
//! these tests never read or replace the developer's clipboard. A pasted image
//! must reach the provider as the fixture's exact bytes, through the same `@`
//! reference a user can type.

mod common;
#[path = "support/image_fixture.rs"]
mod image_fixture;
#[path = "support/sync_input.rs"]
mod sync_input;

use std::fs;
use std::path::{Path, PathBuf};

use common::TestEnv;
use image_fixture::{mark_model_text_only, write_fixture_png};
use pty_expect::PtySession;

const ALT_V: &str = "\x1bv";
/// Alt+V held down: the terminal repeats it before the draft is redrawn.
const HELD_ALT_V: &str = "\x1bv\x1bv\x1bv";
const CTRL_V: &str = "\x16";
/// What a terminal sends for its own paste (e.g. Cmd+V on macOS) when the
/// clipboard holds only an image.
const EMPTY_PASTE: &str = "\x1b[200~\x1b[201~";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repl {
    Default,
    Classic,
}

/// Spawn scode with `clipboard` as its clipboard image. The PTY is wide enough
/// that a pasted `@"path"` reference stays on one input row.
fn start(env: &TestEnv, repl: Repl, clipboard: &Path) -> PtySession {
    let mode = match repl {
        Repl::Default => "queue",
        Repl::Classic => "off",
    };
    let clipboard = clipboard.display().to_string();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", mode),
            ("SUDOCODE_CLIPBOARD_IMAGE", clipboard.as_str()),
        ],
    );
    session.resize(40, 400).unwrap();
    common::expect_input_line_cleared(&session, env.timeout(), "prompt ready");
    session
}

/// The clipboard image, at a path with a space so the reference is quoted.
fn clipboard_image(env: &TestEnv) -> PathBuf {
    let path = env.workspace_root().join("clip board.png");
    write_fixture_png(&path);
    path
}

/// The content blocks of the user message in the latest provider request.
fn last_user_content(env: &TestEnv) -> Vec<serde_json::Value> {
    let body = env
        .captured_message_bodies()
        .pop()
        .expect("a provider request");
    let request: serde_json::Value = serde_json::from_str(&body).unwrap();
    request["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap()
        .clone()
}

/// Paste the clipboard image with `trigger` after a typed prompt, then submit
/// with `submit`. The provider must receive the fixture's bytes once, with the
/// typed text and the image's `@` reference in place of the placeholder.
fn paste_and_submit(
    env: &TestEnv,
    session: &mut PtySession,
    repl: Repl,
    image: &Path,
    (trigger, submit): (&str, &str),
) {
    use base64::Engine as _;

    let turn = env.captured_message_count() + 1;
    let prompt = env.prompt(&format!("Describe pasted image {turn}"), "single_turn_text");
    session.send(&prompt).unwrap();
    common::expect_input_line(session, &prompt, env.timeout(), "prompt typed");
    session.send(trigger).unwrap();
    let shown = match repl {
        Repl::Default => "[Image #1]",
        Repl::Classic => "board.png\"",
    };
    common::expect_input_line(
        session,
        shown,
        env.timeout(),
        &format!("image pasted with {trigger:?}"),
    );
    let marker = common::turn_status_marker(session);
    session.send(submit).unwrap();
    if repl == Repl::Classic {
        sync_input::expect_prompt_after_turn(session, &marker, env.timeout(), "pasted image turn");
    } else {
        common::expect_turn_complete_after(session, &marker, env.timeout(), "pasted image turn");
    }

    assert_eq!(
        env.captured_message_count(),
        turn,
        "one request per submitted turn"
    );
    let content = last_user_content(env);
    let images: Vec<_> = content
        .iter()
        .filter(|block| block["type"] == "image")
        .collect();
    let expected = base64::engine::general_purpose::STANDARD.encode(fs::read(image).unwrap());
    assert_eq!(
        images.len(),
        1,
        "the pasted image is attached once: {content:?}"
    );
    assert_eq!(images[0]["source"]["media_type"], "image/png");
    assert_eq!(
        images[0]["source"]["data"], expected,
        "the fixture's exact bytes"
    );
    // The runtime adds its own reminder blocks; find the typed message.
    let typed = format!("Describe pasted image {turn}");
    let text = content
        .iter()
        .filter_map(|block| block["text"].as_str())
        .find(|text| text.contains(&typed))
        .unwrap_or_else(|| panic!("the typed text {typed:?} was sent: {content:?}"));
    let reference = format!("@\"{}\"", image.display());
    assert_eq!(
        text.matches(&reference).count(),
        1,
        "the message keeps one image reference, even for a held key: {text}"
    );
    assert!(
        !text.contains("[Image #"),
        "the placeholder is expanded: {text}"
    );
}

fn every_trigger_attaches_the_clipboard_image(repl: Repl, steps: &[(&str, &str)]) {
    let env = TestEnv::new(match repl {
        Repl::Default => "image-paste-default",
        Repl::Classic => "image-paste-classic",
    });
    if !env.is_mock() {
        return; // The assertions read the deterministic provider's captured requests.
    }
    let image = clipboard_image(&env);
    let mut session = start(&env, repl, &image);
    for &step in steps {
        paste_and_submit(&env, &mut session, repl, &image, step);
    }
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn default_repl_attaches_the_clipboard_image_on_alt_v_ctrl_v_and_empty_paste() {
    every_trigger_attaches_the_clipboard_image(
        Repl::Default,
        // Tab queues the turn: the placeholder expands on that path too.
        &[(HELD_ALT_V, "\r"), (CTRL_V, "\t"), (EMPTY_PASTE, "\r")],
    );
}

#[test]
fn classic_repl_attaches_the_clipboard_image_on_alt_v_ctrl_v_and_empty_paste() {
    every_trigger_attaches_the_clipboard_image(
        Repl::Classic,
        &[
            (HELD_ALT_V, "\r"),
            (CTRL_V, "\r"),
            // The Windows console delivers a paste to rustyline as plain
            // keystrokes, so an empty paste reaches it as nothing.
            #[cfg(unix)]
            (EMPTY_PASTE, "\r"),
        ],
    );
}

/// Without a clipboard image, Alt+V leaves the draft unchanged and says so,
/// and a text paste still inserts its text.
fn no_clipboard_image(repl: Repl) {
    let env = TestEnv::new(match repl {
        Repl::Default => "image-paste-none-default",
        Repl::Classic => "image-paste-none-classic",
    });
    let not_an_image = env.workspace_root().join("clipboard.txt");
    fs::write(&not_an_image, "plain text").unwrap();
    let mut session = start(&env, repl, &not_an_image);
    session.send("hello").unwrap();
    common::expect_input_line(&session, "hello", env.timeout(), "draft typed");
    session.send(ALT_V).unwrap();
    common::expect_screen(
        &session,
        |screen| screen.contains("No image on the clipboard"),
        env.timeout(),
        "no-image notice",
    );
    session.send("\x1b[200~ world\x1b[201~").unwrap();
    common::expect_screen(
        &session,
        |screen| common::input_line_of(screen) == "hello world",
        env.timeout(),
        "the draft is unchanged and a text paste inserts its text",
    );
    session.send("\x03").unwrap();
    common::expect_input_line_cleared(&session, env.timeout(), "draft cleared");
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    assert_eq!(env.captured_message_count(), 0, "nothing was submitted");
}

#[test]
fn default_repl_without_a_clipboard_image_keeps_the_draft() {
    no_clipboard_image(Repl::Default);
}

#[test]
fn classic_repl_without_a_clipboard_image_keeps_the_draft() {
    no_clipboard_image(Repl::Classic);
}

#[test]
fn pasted_image_for_a_text_only_model_fails_before_any_request() {
    let env = TestEnv::new("image-paste-text-only");
    if !env.is_mock() {
        return; // The capability override belongs to the deterministic provider fixture.
    }
    mark_model_text_only(&env);
    let image = clipboard_image(&env);
    let mut session = start(&env, Repl::Default, &image);
    session.send("Describe").unwrap();
    common::expect_input_line(&session, "Describe", env.timeout(), "draft typed");
    session.send(ALT_V).unwrap();
    common::expect_input_line(&session, "[Image #1]", env.timeout(), "image pasted");
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |screen| screen.contains("does not support image input"),
        env.timeout(),
        "the existing capability error",
    );
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    assert_eq!(
        env.captured_message_count(),
        0,
        "an unsupported image never reaches the provider"
    );
}
