//! PTY acceptance for the builtin Vault plugin (sudoprivacy/sudocode#1458) in
//! the real REPL with the mock model: enabling the plugin changes the tools
//! the model is offered, `vault_use` in a workspace-write session stops at the
//! native permission prompt, and a `vault` section the agent writes into its
//! own workspace is refused rather than obeyed. None of this needs a runtime:
//! `vault_use` refuses before any credential or network access, and a refused
//! settings layer stops the tool before it dials.
//!
//! ```bash
//! cargo test --test pty_vault_plugin
//! ```

mod common;

use std::fs;

use common::TestEnv;
use serde_json::{json, Value};

const READY: &str = "\u{276f}";

/// Merge `entries` into the config home's (user-layer) settings.json.
fn merge_user_settings(env: &TestEnv, entries: &Value) {
    let path = env.config_home().join("settings.json");
    let mut root: Value = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    for (key, value) in entries.as_object().expect("settings entries") {
        root[key] = value.clone();
    }
    fs::write(&path, root.to_string()).expect("write user settings");
}

/// A user-layer `vault` section with one operation. Nothing listens on its
/// endpoint or target, and its credential directory does not exist: no case
/// here may reach either.
fn vault_section(env: &TestEnv) -> Value {
    json!({
        "endpoint": "https://127.0.0.1:9",
        "owner": "pty-owner",
        "credentialDir": env.workspace_root().join("no-credentials"),
        "operations": [{
            "id": "deploy", "kind": "http", "ref": "deploy/token",
            "method": "POST", "origin": "http://127.0.0.1:9", "path": "/v1/deploy",
            "header": "Authorization", "scheme": "Bearer", "redirect": "deny",
            "response": {"headers": [], "jsonFields": ["/ok"]}
        }]
    })
}

fn enable_vault_in_settings(env: &TestEnv) {
    merge_user_settings(
        env,
        &json!({
            "vault": vault_section(env),
            "plugins": {"enabled": {"vault@builtin": {"enabled": true}}}
        }),
    );
}

/// Send one prompt, wait for its turn to finish, and return the last request
/// body the model received.
fn turn(env: &TestEnv, session: &mut pty_expect::PtySession, prompt: &str) -> String {
    let marker = common::turn_status_marker(session);
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen_settled(
        session,
        |screen| !common::input_line_of(screen).trim().is_empty(),
        env.timeout(),
        "prompt pasted",
    );
    session.send("\r").unwrap();
    common::expect_turn_complete_after(session, &marker, env.timeout(), "turn completed");
    env.captured_message_bodies()
        .last()
        .cloned()
        .expect("the model received a request")
}

fn run_command(env: &TestEnv, session: &mut pty_expect::PtySession, command: &str) {
    session.send(&format!("{command}\r")).unwrap();
    common::expect_input_line_cleared(session, env.timeout(), command);
}

fn exit(session: &mut pty_expect::PtySession) {
    session.send("/exit\r").unwrap();
    let code = session.expect_eof().unwrap();
    let screen = session.render(common::screen_contents);
    assert_eq!(code, 0, "{screen}");
}

#[test]
fn enabling_and_disabling_the_builtin_plugin_changes_the_offered_tools() {
    let env = TestEnv::new_mock("vault-plugin-catalog");
    merge_user_settings(&env, &json!({"vault": vault_section(&env)}));
    let mut session = env.spawn(&[]);
    session.resize(50, 160).unwrap();
    session.expect(READY).unwrap();

    let disabled = turn(&env, &mut session, &env.prompt("say hi", "streaming_text"));
    assert!(!disabled.contains("vault_list"), "offered before enabling");
    assert!(!disabled.contains("vault_use"), "offered before enabling");

    run_command(&env, &mut session, "/plugins enable vault@builtin");
    let after_command = session.render(common::screen_contents);
    let enabled = turn(
        &env,
        &mut session,
        &env.prompt("say hi again", "streaming_text"),
    );
    assert!(
        enabled.contains("vault_list"),
        "not offered after enabling; screen after the command:\n{after_command}"
    );
    assert!(enabled.contains("vault_use"), "not offered after enabling");

    run_command(&env, &mut session, "/plugins disable vault@builtin");
    let again = turn(
        &env,
        &mut session,
        &env.prompt("one more", "streaming_text"),
    );
    assert!(
        !again.contains("vault_list"),
        "still offered after disabling"
    );
    exit(&mut session);
}

#[test]
fn vault_use_stops_at_the_permission_prompt_in_a_workspace_write_session() {
    let env = TestEnv::new_mock("vault-plugin-permission");
    enable_vault_in_settings(&env);
    let mut session = env.spawn(&["--permission-mode", "workspace-write"]);
    session.resize(50, 160).unwrap();
    session.expect(READY).unwrap();

    let marker = common::turn_status_marker(&session);
    let prompt = env.tool_batch_prompt(&json!([
        {"id": "use-deploy", "name": "vault_use", "input": {"operation": "deploy"}}
    ]));
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen_settled(
        &session,
        |screen| common::input_line_of(screen).contains("TOOL_BATCH"),
        env.timeout(),
        "tool batch prompt pasted",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |screen| screen.contains("vault_use") && screen.contains("Allow this tool call?"),
        env.timeout(),
        "the native permission prompt for vault_use",
    );
    session.send("1\r").unwrap();
    common::expect_turn_complete_after(&session, &marker, env.timeout(), "vault_use answered");
    let bodies = env.captured_message_bodies();
    let answer = bodies.last().expect("the tool result reached the model");
    assert!(answer.contains("use_unavailable"), "{answer}");
    exit(&mut session);
}

#[test]
fn a_vault_section_the_agent_writes_into_its_workspace_is_refused() {
    let env = TestEnv::new_mock("vault-plugin-project-config");
    enable_vault_in_settings(&env);
    let mut session = env.spawn(&["--permission-mode", "workspace-write"]);
    session.resize(50, 160).unwrap();
    session.expect(READY).unwrap();

    let planted = env
        .workspace_root()
        .join(".nexus")
        .join("sudocode")
        .join("settings.json");
    let widened = json!({"vault": vault_section(&env)}).to_string();
    let write = env.tool_batch_prompt(&json!([
        {"id": "plant", "name": "write_file", "input": {"path": planted, "content": widened}}
    ]));
    turn(&env, &mut session, &write);
    assert!(
        planted.exists(),
        "the agent could not write its workspace settings"
    );

    let list = env.tool_batch_prompt(&json!([
        {"id": "list", "name": "vault_list", "input": {}}
    ]));
    let answer = turn(&env, &mut session, &list);
    assert!(answer.contains("project_config_refused"), "{answer}");
    exit(&mut session);
}
