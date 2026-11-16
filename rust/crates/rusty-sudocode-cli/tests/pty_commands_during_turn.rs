//! Slash commands typed while a turn runs. Settings are chosen during the turn
//! and apply when it ends; commands that need the session are refused by name;
//! none of them reaches the model.
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
    paste(
        &mut session,
        &env,
        &env.prompt("QUEUED_AFTER_SWITCH", "single_turn_text"),
    );
    session.send("\t").unwrap();
    common::expect_screen(
        &session,
        |view| common::input_line_of(view).is_empty() && view.contains("QUEUED_AFTER_SWITCH"),
        env.timeout(),
        "the message waits in the queue",
    );
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
fn a_session_command_during_a_turn_is_refused_by_name_and_help_still_runs() {
    let (env, mut session, release) = start_held_turn("turn-commands-refused");
    enter(&mut session, &env, "/compact");
    let screen = common::expect_screen(
        &session,
        |view| view.contains("'/compact' can't run while a turn is running."),
        env.timeout(),
        "the refusal names the command",
    );
    assert!(
        !screen.contains("finish or cancel before changing session settings"),
        "{screen}"
    );
    // `/help` needs no session, so it answers while the tool is still held.
    // Its last section is the Debug commands.
    enter(&mut session, &env, "/help");
    common::expect_screen(
        &session,
        |view| view.lines().any(|line| line.trim_end() == "Debug"),
        env.timeout(),
        "help answers during the turn",
    );
    assert!(!env.workspace_root().join("release-tool").exists());
    let marker = common::turn_status_marker(&session);
    release.now();
    common::expect_turn_complete_after(&session, &marker, env.timeout(), "the held turn ends");
    assert!(
        requests(&env)
            .iter()
            .all(|body| !body.to_string().contains("/compact")
                && !body.to_string().contains("/help")),
        "no command reaches the model"
    );
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
    paste(
        &mut session,
        &env,
        &env.prompt("QUEUED_BEHIND_PICKER", "single_turn_text"),
    );
    session.send("\t").unwrap();
    common::expect_screen(
        &session,
        |view| common::input_line_of(view).is_empty() && view.contains("QUEUED_BEHIND_PICKER"),
        env.timeout(),
        "the message waits in the queue",
    );
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
