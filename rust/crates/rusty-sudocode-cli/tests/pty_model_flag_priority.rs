//! Explicit default model -> actual file read -> resume/headless wire requests.
//! SCODE_TEST_BACKEND=live uses the real account and the compiled-in default.
mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use engine_host::config::DEFAULT_MODEL;
use pty_expect::PtySession;
use serde_json::{json, Value};

const BUDGET: Duration = Duration::from_secs(120);

fn setup(label: &str) -> TestEnv {
    let env = TestEnv::new(label);
    if env.is_mock() {
        let path = env.config_home().join("sudocode.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["models"][DEFAULT_MODEL] = json!({
            "alias":DEFAULT_MODEL,"name":"Explicit default model fixture","input":["text"],
            "providers":{"api-key":{"provider":"anthropic","model":DEFAULT_MODEL}}
        });
        fs::write(path, config.to_string()).unwrap();
    }
    let path = env.config_home().join("settings.json");
    let mut settings: Value = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    settings["model"] = json!("sonnet");
    settings["thinking"] = json!(false);
    fs::write(path, settings.to_string()).unwrap();
    env
}

fn request_directory(env: &TestEnv) -> std::path::PathBuf {
    std::env::var_os("SCODE_LIVE_ARTIFACTS").map_or_else(
        || env.workspace_root().join("requests"),
        |root| std::path::PathBuf::from(root).join(env.workspace_root().file_name().unwrap()),
    )
}

fn spawn(env: &TestEnv, args: &[&str]) -> PtySession {
    let requests = request_directory(env);
    let home = env.workspace_root().join("home");
    common::spawn_scode_in_dir_with_env(
        env.workspace_root(),
        args,
        BUDGET,
        &[
            ("SUDO_CODE_CONFIG_HOME", env.config_home()),
            ("HOME", &home),
            ("USERPROFILE", &home),
            ("ANTHROPIC_MODEL", Path::new("sonnet")),
            ("SCODE_SKIP_CONFIG_MIGRATION", Path::new("1")),
            ("SUDOCODE_DUMP_REQUESTS", &requests),
        ],
    )
    .unwrap()
}

fn task(env: &TestEnv) -> (String, String) {
    let nonce = format!(
        "MODEL_FILE_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = env.workspace_root().join("fixture.txt");
    fs::write(&path, &nonce).unwrap();
    let prompt = env.prompt(
        "Read fixture.txt with read_file and reply with its contents.",
        "read_file_roundtrip",
    );
    (prompt, nonce)
}

fn assert_requests(env: &TestEnv, minimum: usize) {
    let requests: Vec<Value> = fs::read_dir(request_directory(env))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-messages.json")
        })
        .map(|path| serde_json::from_slice(&fs::read(path).unwrap()).unwrap())
        .collect();
    assert!(
        requests.len() >= minimum,
        "no actual model roundtrip captured"
    );
    let mut results = std::collections::BTreeSet::new();
    for request in requests {
        assert_eq!(
            request["model"], DEFAULT_MODEL,
            "explicit flag changed on the wire"
        );
        for message in request["messages"].as_array().unwrap() {
            let Some(blocks) = message["content"].as_array() else {
                continue;
            };
            for block in blocks.iter().filter(|block| block["type"] == "tool_result") {
                assert_ne!(
                    block["is_error"], true,
                    "file workflow recovered from a failed tool"
                );
                results.insert(block["tool_use_id"].to_string());
            }
        }
    }
    assert!(!results.is_empty(), "the model never read the actual file");
}

fn turn(env: &TestEnv, resume: bool) {
    let auth = if env.is_live() { "proxy" } else { "api-key" };
    let mut args = vec![
        "--auth",
        auth,
        "--model",
        DEFAULT_MODEL,
        "--permission-mode",
        "read-only",
    ];
    if resume {
        args.push("--resume");
    }
    let mut cli = spawn(env, &args);
    cli.resize(48, 200).unwrap();
    common::expect_screen(&cli, |s| s.contains("❯"), BUDGET, "interactive prompt");
    let (prompt, nonce) = task(env);
    let marker = common::turn_status_marker(&cli);
    cli.send(&prompt).unwrap();
    common::expect_input_line(&cli, &prompt, BUDGET, "task input");
    cli.send("\r").unwrap();
    common::expect_turn_complete_after(&cli, &marker, BUDGET, "file read completed");
    common::expect_screen(
        &cli,
        |s| s.contains(&nonce),
        BUDGET,
        "file contents in reply",
    );
    cli.send("/exit").unwrap();
    common::expect_input_line(&cli, "/exit", BUDGET, "exit input");
    cli.send("\r").unwrap();
    assert_eq!(
        cli.expect_eof().unwrap(),
        0,
        "{}",
        common::screen_tail(&cli, 12000)
    );
}

#[test]
fn explicit_default_model_survives_repl_and_resume_with_conflicting_defaults() {
    let env = setup("model-flag-resume");
    turn(&env, false);
    assert_requests(&env, 2);
    turn(&env, true);
    assert_requests(&env, 4);
}

#[test]
fn explicit_default_model_reaches_headless_file_tools_with_conflicting_defaults() {
    let env = setup("model-flag-headless");
    let auth = if env.is_live() { "proxy" } else { "api-key" };
    let (prompt, nonce) = task(&env);
    let mut cli = spawn(
        &env,
        &[
            "--auth",
            auth,
            "--model",
            DEFAULT_MODEL,
            "--permission-mode",
            "read-only",
            "-p",
            "--output-format",
            "stream-json",
            &prompt,
        ],
    );
    assert_eq!(
        cli.expect_eof().unwrap(),
        0,
        "{}",
        common::screen_tail(&cli, 12000)
    );
    let screen = common::screen_tail(&cli, 20000);
    assert!(
        screen.contains(&nonce),
        "headless response lacks the file contents: {screen}"
    );
    assert_requests(&env, 2);
}
