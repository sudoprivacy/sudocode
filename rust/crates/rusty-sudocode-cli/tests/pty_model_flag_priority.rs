//! Explicit default model -> actual file read -> resume/headless wire requests.
//! CI checks CLI precedence with a deterministic transport on all platforms.
//! SCODE_TEST_BACKEND=live additionally probes the compiled-in default provider.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use engine_host::config::DEFAULT_MODEL;
use pty_expect::PtySession;
use runtime::{ContentBlock, MessageRole, Session, SessionStore};
use serde_json::{json, Value};

const BUDGET: Duration = Duration::from_secs(120);

fn setup(label: &str, model: &str) -> TestEnv {
    let env = TestEnv::new(label);
    if env.is_mock() {
        let path = env.config_home().join("sudocode.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["models"][model] = json!({
            "alias":model,"name":"Explicit model fixture","input":["text"],
            "providers":{"api-key":{"provider":"anthropic","model":model}}
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

fn task(env: &TestEnv, resume: bool) -> (String, String) {
    let nonce = format!(
        "MODEL_FILE_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = env.workspace_root().join("fixture.txt");
    fs::write(&path, &nonce).unwrap();
    // Anchor the task to this fixture, including on Windows. A relative name
    // left the live model guessing an unrelated workspace despite a correct
    // working directory in the request's environment context.
    let path = path.to_string_lossy().replace('\\', "/");
    let update = if resume {
        "The local file has been updated since the previous turn. "
    } else {
        ""
    };
    let prompt = env.prompt(
        &format!(
            "{update}Use read_file to read the local file at {path:?}. Reply with its current contents."
        ),
        "read_file_roundtrip",
    );
    (prompt, nonce)
}

fn assert_requests(env: &TestEnv, model: &str, minimum: usize, nonce: &str) {
    let requests: Vec<Value> = fs::read_dir(request_directory(env))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| wire_requests::is_inference_request_dump(path))
        .map(|path| serde_json::from_slice(&fs::read(path).unwrap()).unwrap())
        .collect();
    assert!(
        requests.len() >= minimum,
        "no actual model roundtrip captured"
    );
    let mut results = std::collections::BTreeSet::new();
    let mut has_fixture_result = false;
    for request in requests {
        assert_eq!(request["model"], model, "explicit flag changed on the wire");
        for block in wire_requests::request_tool_blocks(&request)
            .iter()
            .filter(|block| block["type"] == "tool_result")
        {
            assert_ne!(
                block["is_error"], true,
                "file workflow recovered from a failed tool"
            );
            results.insert(block["tool_use_id"].to_string());
            has_fixture_result |= block["content"].to_string().contains(nonce);
        }
    }
    assert!(!results.is_empty(), "the model never read the actual file");
    assert!(
        has_fixture_result,
        "no successful tool result contains this turn's fixture contents"
    );
}

fn assistant_answer(env: &TestEnv) -> Option<String> {
    let store = SessionStore::from_cwd(env.workspace_root()).ok()?;
    // Resume startup also creates an empty placeholder session. Select the
    // conversation instead of relying on filesystem enumeration order.
    let transcript = store
        .list_sessions()
        .ok()?
        .into_iter()
        .find(|session| session.message_count > 0)?
        .path;
    // The live child can still be appending a record while this is polled.
    let session = Session::load_from_path(transcript).ok()?;
    let answer = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)?;
    Some(
        answer
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>(),
    )
}

fn assert_answer(env: &TestEnv, nonce: &str) {
    let text = assistant_answer(env).expect("the CLI must persist an assistant answer");
    assert!(
        text.contains(nonce),
        "persisted assistant answer lacks this turn's file contents: {text}"
    );
}

fn turn(env: &TestEnv, model: &str, resume: bool) -> String {
    let auth = if env.is_live() { "proxy" } else { "api-key" };
    let mut args = vec![
        "--auth",
        auth,
        "--model",
        model,
        "--permission-mode",
        "read-only",
    ];
    if resume {
        args.push("--resume");
    }
    let (prompt, nonce) = task(env, resume);
    let mut cli = spawn(env, &args);
    // The input helper checks one prompt row. Keep the full absolute path and
    // the resume instruction on that row instead of timing out on a wrap.
    let columns = u16::try_from(prompt.chars().count() + 8).unwrap().max(200);
    cli.resize(48, columns).unwrap();
    common::expect_input_line_cleared(&cli, BUDGET, "interactive prompt ready after replay");
    cli.send(&prompt).unwrap();
    common::expect_input_line(&cli, &prompt, BUDGET, "task input");
    cli.send("\r").unwrap();
    // A replayed status line can arrive after input is submitted on resume.
    // Only this turn's persisted answer proves that the new file was read.
    // A refusal is a failure, even if the tool card already shows the nonce.
    common::expect_screen(
        &cli,
        |screen| {
            assistant_answer(env).is_some_and(|answer| answer.contains(&nonce))
                || common::screen_contains(screen, "provider refused")
        },
        BUDGET,
        "file read result or provider refusal",
    );
    let screen = common::screen_tail(&cli, 12000);
    assert!(
        !common::screen_contains(&screen, "provider refused"),
        "live provider refused the file-read task: {screen}"
    );
    assert_answer(env, &nonce);
    common::expect_input_line_cleared(&cli, BUDGET, "file read completed and prompt ready");
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
    assert_answer(env, &nonce);
    nonce
}

#[test]
fn explicit_default_model_survives_repl_and_resume_with_conflicting_defaults() {
    let env = setup("model-flag-resume", DEFAULT_MODEL);
    let nonce = turn(&env, DEFAULT_MODEL, false);
    assert_requests(&env, DEFAULT_MODEL, 2, &nonce);
    let nonce = turn(&env, DEFAULT_MODEL, true);
    assert_requests(&env, DEFAULT_MODEL, 4, &nonce);
}

#[test]
fn configured_model_reads_updated_file_after_resume() {
    // Use a canonical wire model so the request assertion also checks routing.
    // The compiled-default tests above remain independent of this live pin.
    let model = std::env::var("SCODE_LIVE_MODEL")
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
    let env = setup("configured-model-resume", &model);
    let nonce = turn(&env, &model, false);
    assert_requests(&env, &model, 2, &nonce);
    let nonce = turn(&env, &model, true);
    assert_requests(&env, &model, 4, &nonce);
}

#[test]
fn explicit_default_model_reaches_headless_file_tools_with_conflicting_defaults() {
    let env = setup("model-flag-headless", DEFAULT_MODEL);
    let auth = if env.is_live() { "proxy" } else { "api-key" };
    let (prompt, nonce) = task(&env, false);
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
    assert_answer(&env, &nonce);
    assert_requests(&env, DEFAULT_MODEL, 2, &nonce);
}
