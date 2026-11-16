//! PTY acceptance for the builtin Vault plugin (sudoprivacy/sudocode#1458) in
//! the real REPL with the mock model: enabling the plugin changes the tools
//! the model is offered, `vault_use` in a workspace-write session stops at the
//! native permission prompt, and a `vault` section the agent writes into its
//! own workspace, including a user settings file that lies inside it, is
//! refused rather than obeyed. None of this needs a runtime: `vault_use`
//! refuses before any credential or network access, and a refused settings
//! layer stops the tool before it dials.
//!
//! ```bash
//! cargo test --test pty_vault_plugin
//! ```

mod common;

use std::fs;
use std::path::Path;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};
use tempfile::TempDir;

const READY: &str = "\u{276f}";

/// The harness keeps its config home inside the workspace root, the layout in
/// which a `vault` section is refused. These sessions use a copy of it outside
/// the workspace, as `~/.nexus/sudocode` is for a project under `~`.
fn config_home_outside(env: &TestEnv) -> TempDir {
    let dir = tempfile::tempdir().expect("config home");
    copy_config(env.config_home(), dir.path());
    dir
}

fn copy_config(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("config home");
    for entry in fs::read_dir(from).expect("isolated config home") {
        let entry = entry.expect("config entry");
        if entry.file_type().expect("entry type").is_file() {
            fs::copy(entry.path(), to.join(entry.file_name())).expect("copy config");
        }
    }
}

fn read_settings(config_home: &Path) -> Value {
    fs::read_to_string(config_home.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}))
}

/// Merge `entries` into `config_home`'s (user-layer) settings.json.
fn merge_user_settings(config_home: &Path, entries: &Value) {
    let mut root = read_settings(config_home);
    for (key, value) in entries.as_object().expect("settings entries") {
        root[key] = value.clone();
    }
    fs::write(config_home.join("settings.json"), root.to_string()).expect("write user settings");
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

const ENABLED: &str = r#"{"enabled": {"vault@builtin": {"enabled": true}}}"#;

fn enabled_plugin() -> Value {
    serde_json::from_str(ENABLED).expect("plugins section")
}

/// Start the REPL in the workspace with `config_home` as the user config home.
fn spawn(env: &TestEnv, config_home: &Path, args: &[&str]) -> PtySession {
    let config_home = config_home.to_str().expect("UTF-8 config home");
    let mut session = env.spawn_with_env(args, &[("SUDO_CODE_CONFIG_HOME", config_home)]);
    session.resize(50, 160).unwrap();
    session.expect(READY).unwrap();
    session
}

/// Send one prompt, wait for its turn to finish, and return the last request
/// body the model received.
fn turn(env: &TestEnv, session: &mut PtySession, prompt: &str) -> String {
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

/// Have the agent write `content` to `path` with `write_file`, then call
/// `vault_list` and return what the model received back.
fn write_then_list(env: &TestEnv, session: &mut PtySession, path: &Path, content: &str) -> String {
    let write = env.tool_batch_prompt(&json!([
        {"id": "plant", "name": "write_file", "input": {"path": path, "content": content}}
    ]));
    turn(env, session, &write);
    assert!(
        fs::read_to_string(path).is_ok_and(|text| text.contains("\"vault\"")),
        "the agent could not write {}",
        path.display()
    );
    let list = env.tool_batch_prompt(&json!([
        {"id": "list", "name": "vault_list", "input": {}}
    ]));
    turn(env, session, &list)
}

fn run_command(env: &TestEnv, session: &mut PtySession, command: &str) {
    session.send(&format!("{command}\r")).unwrap();
    common::expect_input_line_cleared(session, env.timeout(), command);
}

fn exit(session: &mut PtySession) {
    session.send("/exit\r").unwrap();
    let code = session.expect_eof().unwrap();
    let screen = session.render(common::screen_contents);
    assert_eq!(code, 0, "{screen}");
}

#[test]
fn enabling_and_disabling_the_builtin_plugin_changes_the_offered_tools() {
    let env = TestEnv::new_mock("vault-plugin-catalog");
    let config_home = config_home_outside(&env);
    merge_user_settings(config_home.path(), &json!({"vault": vault_section(&env)}));
    let mut session = spawn(&env, config_home.path(), &[]);

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
    let config_home = config_home_outside(&env);
    merge_user_settings(
        config_home.path(),
        &json!({"vault": vault_section(&env), "plugins": enabled_plugin()}),
    );
    let mut session = spawn(
        &env,
        config_home.path(),
        &["--permission-mode", "workspace-write"],
    );

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
    let config_home = config_home_outside(&env);
    merge_user_settings(
        config_home.path(),
        &json!({"vault": vault_section(&env), "plugins": enabled_plugin()}),
    );
    let mut session = spawn(
        &env,
        config_home.path(),
        &["--permission-mode", "workspace-write"],
    );

    let planted = env
        .workspace_root()
        .join(".nexus")
        .join("sudocode")
        .join("settings.json");
    let widened = json!({"vault": vault_section(&env)}).to_string();
    let answer = write_then_list(&env, &mut session, &planted, &widened);
    assert!(answer.contains("project_config_refused"), "{answer}");
    exit(&mut session);
}

/// A session started in the directory that holds the user config home (scode
/// run in `~`, config home `~/.nexus/sudocode`) has its user settings file
/// inside the workspace, where workspace-write tools reach it. A `vault`
/// section the agent writes there is refused like any workspace file's.
#[test]
fn a_vault_section_the_agent_writes_into_a_user_settings_file_inside_the_workspace_is_refused() {
    let env = TestEnv::new_mock("vault-plugin-home-workspace");
    let config_home = env.workspace_root().join(".nexus").join("sudocode");
    copy_config(env.config_home(), &config_home);
    merge_user_settings(&config_home, &json!({"plugins": enabled_plugin()}));
    let mut session = spawn(
        &env,
        &config_home,
        &["--permission-mode", "workspace-write"],
    );

    let mut widened = read_settings(&config_home);
    widened["vault"] = vault_section(&env);
    let answer = write_then_list(
        &env,
        &mut session,
        &config_home.join("settings.json"),
        &widened.to_string(),
    );
    assert!(answer.contains("project_config_refused"), "{answer}");
    exit(&mut session);
}
