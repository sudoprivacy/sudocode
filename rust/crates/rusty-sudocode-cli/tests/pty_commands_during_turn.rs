//! Slash commands typed while a turn runs. Settings are chosen during the turn
//! and apply when it ends; commands that need the session are queued and run
//! as commands when it ends; none of them reaches the model.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

const SWITCHED_MODEL: &str = "mock-switched-model";

/// Releases the held tool when the test ends, pass or fail, so a failed
/// assertion never leaves the turn waiting.
struct Release(PathBuf);

impl Release {
    fn now(&self) {
        fs::write(self.0.join("release-tool"), "release").unwrap();
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release-tool"), "release");
    }
}

/// A REPL whose turn is held open on a shell tool until `release-tool`
/// exists, with one more model configured to switch to.
fn start_held_turn(label: &str) -> (TestEnv, PtySession, Release) {
    let env = TestEnv::new_mock(label);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["models"][SWITCHED_MODEL] = json!({
        "alias": SWITCHED_MODEL, "name": "Switched model fixture", "input": ["text"],
        "providers": {"api-key": {"provider": "anthropic", "model": SWITCHED_MODEL}}
    });
    fs::write(config_path, config.to_string()).unwrap();
    fs::write(
        env.workspace_root().join("wait.sh"),
        "printf started > tool-started; while test ! -f release-tool; do sleep 0.1; done; printf 'HELD_TOOL_DONE\\n'",
    )
    .unwrap();
    let release = Release(env.workspace_root().to_path_buf());
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 120).unwrap();
    common::expect_input_line_cleared(&session, env.timeout(), "initial prompt");
    let call = json!({
        "id": "held-tool", "name": "Bash",
        "input": {"command": "sh wait.sh", "timeout": common::LIVE_TURN_BUDGET.as_millis() * 3},
    });
    paste(
        &mut session,
        &env,
        &format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([call])
        ),
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |_| env.workspace_root().join("tool-started").exists(),
        env.timeout(),
        "the held tool started",
    );
    (env, session, release)
}

fn paste(session: &mut PtySession, env: &TestEnv, text: &str) {
    session.send(&format!("\x1b[200~{text}\x1b[201~")).unwrap();
    common::expect_screen(
        session,
        |view| view.contains("Pasted") || common::screen_contains(view, text),
        env.timeout(),
        "draft arrived",
    );
}

/// Type `command` and press Enter while the turn is held.
fn enter(session: &mut PtySession, env: &TestEnv, command: &str) {
    session.send(command).unwrap();
    common::expect_input_line(session, command, env.timeout(), "command typed");
    session.send("\r").unwrap();
}

fn finish(mut session: PtySession, env: &TestEnv) {
    common::expect_input_line_cleared(&session, env.timeout(), "idle before exit");
    session.send("/exit").unwrap();
    common::expect_input_line(&session, "/exit", env.timeout(), "exit typed");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

fn requests(env: &TestEnv) -> Vec<Value> {
    env.captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect()
}

/// The first request that carries `marker`.
fn request_with(env: &TestEnv, marker: &str) -> Option<String> {
    requests(env)
        .into_iter()
        .map(|body| body.to_string())
        .find(|body| body.contains(marker))
}

/// Queue a message behind the held turn with Tab.
fn queue_message(session: &mut PtySession, env: &TestEnv, marker: &str) {
    paste(session, env, &env.prompt(marker, "single_turn_text"));
    session.send("\t").unwrap();
    common::expect_screen(
        session,
        |view| common::input_line_of(view).is_empty() && view.contains(marker),
        env.timeout(),
        "the message waits in the queue",
    );
}

/// Press Enter on `command` during the turn; it waits in the pending area.
fn queue_command(session: &mut PtySession, env: &TestEnv, command: &str) {
    enter(session, env, command);
    common::expect_screen(
        session,
        |view| view.contains(&format!("queued: {command}")),
        env.timeout(),
        "the command waits in the queue",
    );
}

#[test]
fn a_model_chosen_during_a_turn_applies_when_the_turn_ends() {
    let (env, mut session, release) = start_held_turn("turn-commands-model");
    enter(&mut session, &env, &format!("/model {SWITCHED_MODEL}"));
    common::expect_screen(
        &session,
        |view| {
            view.contains(&format!(
                "Model switches to {SWITCHED_MODEL} after this turn."
            ))
        },
        env.timeout(),
        "the switch waits for the turn",
    );
    // A message queued behind the turn goes out after the switch.
    queue_message(&mut session, &env, "QUEUED_AFTER_SWITCH");
    let marker = common::turn_status_marker(&session);
    release.now();
    common::expect_turn_complete_after(&session, &marker, env.timeout(), "the held turn ends");
    common::expect_screen(
        &session,
        |_| {
            requests(&env)
                .iter()
                .any(|body| body.to_string().contains("QUEUED_AFTER_SWITCH"))
        },
        env.timeout(),
        "the queued message is sent",
    );
    let screen = common::expect_screen(
        &session,
        |view| view.contains("Model updated") && view.contains(SWITCHED_MODEL),
        env.timeout(),
        "the switch is reported at the turn boundary",
    );
    // The status line names the model of the last completed turn: the queued
    // message ran on the new one.
    common::expect_screen(
        &session,
        |view| view.contains(&format!("[{SWITCHED_MODEL}] · turn")),
        env.timeout(),
        "the queued turn completes on the new model",
    );
    let bodies = requests(&env);
    let queued = bodies
        .iter()
        .position(|body| body.to_string().contains("QUEUED_AFTER_SWITCH"))
        .unwrap();
    for body in &bodies[..queued] {
        assert_ne!(
            body["model"], SWITCHED_MODEL,
            "the held turn keeps its model"
        );
    }
    assert_eq!(
        bodies[queued]["model"], SWITCHED_MODEL,
        "the queued message runs on the new model; screen:\n{screen}"
    );
    assert!(
        bodies
            .iter()
            .all(|body| !body.to_string().contains("/model ")),
        "the command never reaches the model"
    );
    finish(session, &env);
}

#[test]
fn model_picker_and_confirmation_have_separate_history_from_streaming_thinking() {
    let env = TestEnv::new_mock("thinking-model-picker-spacing");
    let gate = env.workspace_root().join("release-tool");
    let release = Release(env.workspace_root().to_path_buf());
    let batch = json!([{
        "id": "thinking-model-tool", "name": "Bash",
        "input": {"command": "printf 'MODEL_SPACING_TOOL_DONE\\n'"},
        "stream_wait_for": gate,
        "stream_pause_at": "thinking",
        "stream_thinking": [
            {"thinking": "First reasoning marker.\n", "signature": "first-signature"},
            {"thinking": "Second reasoning marker.\n", "signature": "second-signature"}
        ]
    }]);
    let mut session = env.spawn_with_env_at_size(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
        60,
        120,
    );
    common::expect_input_line_cleared(&session, env.timeout(), "initial prompt");
    paste(
        &mut session,
        &env,
        &env.prompt(
            &format!("Use the fixture. TOOL_BATCH:{batch}"),
            "tool_concurrency",
        ),
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |screen| {
            screen
                .lines()
                .any(|row| row.trim() == "✻ First reasoning marker.")
        },
        env.timeout(),
        "thinking before the model picker",
    );
    enter(&mut session, &env, "/model");
    common::expect_screen(
        &session,
        |screen| screen.contains("Select model") && screen.contains("Current:"),
        env.timeout(),
        "model picker opened",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |screen| {
            screen
                .lines()
                .any(|row| row.trim().starts_with("Model switches to "))
        },
        env.timeout(),
        "choice announced while thinking is still gated",
    );
    release.now();
    let screen = common::expect_screen_rows_settled(
        &session,
        |screen| {
            screen
                .lines()
                .any(|row| row.trim() == "Second reasoning marker.")
                && screen.contains("Concurrency batch done.")
                && screen.contains("Select effort")
        },
        env.timeout(),
        "the streamed turn ended and opened the selected model's effort picker",
    );
    let rows: Vec<_> = screen.lines().collect();
    let boundaries = [
        "✻ First reasoning marker.",
        "❯ /model",
        "Current:",
        "Model switches to ",
        "Second reasoning marker.",
    ]
    .map(|marker| {
        rows.iter()
            .position(|row| row.trim_start().starts_with(marker))
            .unwrap_or_else(|| panic!("missing {marker}:\n{screen}"))
    });
    for (before, after) in boundaries.into_iter().zip(boundaries.into_iter().skip(1)) {
        assert_eq!(after, before + 2, "{screen}");
        assert!(rows[before + 1].trim().is_empty(), "{screen}");
    }
    session.send("\x1b").unwrap();
    finish(session, &env);
}

#[test]
fn a_session_command_queued_during_a_turn_runs_between_the_messages_around_it() {
    let (env, mut session, release) = start_held_turn("turn-commands-queued");
    queue_message(&mut session, &env, "QUEUED_BEFORE_CLEAR");
    queue_command(&mut session, &env, "/clear --confirm");
    queue_message(&mut session, &env, "QUEUED_AFTER_CLEAR");
    // `/help` needs no session, so it answers while the tool is still held.
    // Its last section is the Debug commands.
    enter(&mut session, &env, "/help");
    common::expect_screen(
        &session,
        |view| view.lines().any(|line| line.trim_end() == "Debug"),
        env.timeout(),
        "help answers during the turn",
    );
    let screen = session.render(common::screen_contents);
    assert!(
        !screen.contains("Session cleared"),
        "the command waited for the turn:\n{screen}"
    );
    release.now();
    common::expect_screen(
        &session,
        |_| request_with(&env, "QUEUED_AFTER_CLEAR").is_some(),
        env.timeout(),
        "the message queued after the command is sent",
    );
    common::expect_screen(
        &session,
        |view| view.contains("Session cleared"),
        env.timeout(),
        "the command ran",
    );
    let before = request_with(&env, "QUEUED_BEFORE_CLEAR").unwrap();
    let after = request_with(&env, "QUEUED_AFTER_CLEAR").unwrap();
    assert!(
        before.contains("held-tool") && !before.contains("QUEUED_AFTER_CLEAR"),
        "the first message ran alone, in the held session: {before}"
    );
    assert!(
        !after.contains("held-tool") && !after.contains("QUEUED_BEFORE_CLEAR"),
        "the second message ran in the cleared session: {after}"
    );
    assert!(
        requests(&env).iter().all(|body| {
            let body = body.to_string();
            !body.contains("/clear") && !body.contains("/help")
        }),
        "no command reaches the model"
    );
    finish(session, &env);
}

/// `/compact` runs as its own foreground work. The message queued after it
/// must wait for that work to finish, not start beside it.
#[test]
fn a_queued_compact_finishes_before_the_message_queued_after_it() {
    let (env, mut session, release) = start_held_turn("turn-commands-compact");
    queue_command(&mut session, &env, "/compact");
    queue_message(&mut session, &env, "QUEUED_AFTER_COMPACT");
    release.now();
    common::expect_screen(
        &session,
        |_| request_with(&env, "QUEUED_AFTER_COMPACT").is_some(),
        env.timeout(),
        "the message queued after /compact is sent",
    );
    let screen = common::expect_screen(
        &session,
        |view| view.contains("Estimated tokens") && !view.contains("queued: "),
        env.timeout(),
        "the compaction report is shown and nothing is left queued",
    );
    let report = screen.find("Estimated tokens").unwrap();
    let message = screen.find("QUEUED_AFTER_COMPACT");
    assert!(
        message.is_some_and(|message| message > report),
        "the message ran after the compaction finished:\n{screen}"
    );
    assert!(
        requests(&env)
            .iter()
            .all(|body| !body.to_string().contains("/compact")),
        "the command never reaches the model"
    );
    finish(session, &env);
}

#[test]
fn esc_steers_queued_messages_and_keeps_a_queued_command_for_the_turn_end() {
    let (env, mut session, release) = start_held_turn("turn-commands-esc");
    queue_command(&mut session, &env, "/clear --confirm");
    queue_message(&mut session, &env, "STEERED_BY_ESC");
    session.send("\x1b").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("steering: ") && view.contains("queued: /clear --confirm"),
        env.timeout(),
        "Esc steers the message and leaves the command queued",
    );
    release.now();
    common::expect_screen(
        &session,
        |view| view.contains("Session cleared"),
        env.timeout(),
        "the command runs when the turn ends",
    );
    let steered = request_with(&env, "STEERED_BY_ESC").unwrap();
    assert!(
        steered.contains("held-tool"),
        "the message joined the running turn: {steered}"
    );
    assert!(
        requests(&env)
            .iter()
            .all(|body| !body.to_string().contains("/clear")),
        "the command is never steered into the turn"
    );
    finish(session, &env);
}

#[test]
fn up_recalls_a_queued_command_and_it_no_longer_runs() {
    let (env, mut session, release) = start_held_turn("turn-commands-recall");
    queue_command(&mut session, &env, "/clear --confirm");
    session.send("\x1b[A").unwrap();
    common::expect_screen(
        &session,
        |view| {
            common::input_line_of(view) == "/clear --confirm"
                && !view.contains("queued: /clear --confirm")
        },
        env.timeout(),
        "↑ brings the command back to the draft",
    );
    session.send("\x15").unwrap();
    // The held turn's spinner keeps the screen moving, so wait on the input
    // line alone rather than for a settled screen.
    common::expect_screen(
        &session,
        |view| common::input_line_of(view).is_empty(),
        env.timeout(),
        "draft dropped",
    );
    let marker = common::turn_status_marker(&session);
    release.now();
    common::expect_turn_complete_after(&session, &marker, env.timeout(), "the held turn ends");
    std::thread::sleep(std::time::Duration::from_secs(1));
    let screen = session.render(common::screen_contents);
    assert!(
        !screen.contains("Session cleared"),
        "a recalled command does not run:\n{screen}"
    );
    finish(session, &env);
}

#[test]
fn an_unknown_command_during_a_turn_is_reported_at_once() {
    let (env, mut session, release) = start_held_turn("turn-commands-unknown");
    enter(&mut session, &env, "/not-a-command");
    let screen = common::expect_screen(
        &session,
        |view| view.contains("Unknown slash command: /not-a-command"),
        env.timeout(),
        "reported while the tool is held",
    );
    assert!(
        !screen.contains("queued: /not-a-command"),
        "an unknown command is not queued:\n{screen}"
    );
    assert!(!env.workspace_root().join("release-tool").exists());
    release.now();
    finish(session, &env);
}

#[test]
fn the_model_picker_opens_during_a_turn_and_esc_leaves_the_turn_running() {
    let (env, mut session, release) = start_held_turn("turn-commands-picker");
    enter(&mut session, &env, "/model");
    common::expect_screen(
        &session,
        |view| view.contains("Select model"),
        env.timeout(),
        "the picker opens while the turn holds the session",
    );
    session.send("\x1b").unwrap();
    common::expect_screen(
        &session,
        |view| !view.contains("Select model"),
        env.timeout(),
        "Esc closes the picker",
    );
    release.now();
    let screen = common::expect_screen(
        &session,
        |view| view.contains("HELD_TOOL_DONE"),
        env.timeout(),
        "the turn ran on after Esc",
    );
    assert!(
        !screen.contains("Cancelled"),
        "Esc cancelled the turn:\n{screen}"
    );
    finish(session, &env);
}

#[test]
fn a_picker_open_when_the_turn_ends_stays_and_holds_the_queue() {
    let (env, mut session, release) = start_held_turn("turn-commands-held-queue");
    queue_message(&mut session, &env, "QUEUED_BEHIND_PICKER");
    enter(&mut session, &env, "/model");
    common::expect_screen(
        &session,
        |view| view.contains("Select model"),
        env.timeout(),
        "the picker opens during the turn",
    );
    release.now();
    common::expect_screen(
        &session,
        |view| view.contains("Concurrency batch done.") && view.contains("] · turn "),
        env.timeout(),
        "the held turn ends",
    );
    // The queued turn cannot start while the picker is open, so the next
    // completion after this marker is the queued one.
    let marker = common::turn_status_marker(&session);
    let is_sent = || {
        requests(&env)
            .iter()
            .any(|body| body.to_string().contains("QUEUED_BEHIND_PICKER"))
    };
    // The turn's own cleanup must not take the picker with it, and what is
    // queued waits for the choice.
    for _ in 0..10 {
        let screen = session.render(common::screen_contents);
        assert!(
            screen.contains("Select model"),
            "the picker closed with the turn:\n{screen}"
        );
        assert!(!is_sent(), "the queue ran while the picker was open");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    session.send("\x1b").unwrap();
    common::expect_screen(
        &session,
        |_| is_sent(),
        env.timeout(),
        "closing the picker lets the queue run",
    );
    common::expect_turn_complete_after(&session, &marker, env.timeout(), "the queued turn ends");
    finish(session, &env);
}
