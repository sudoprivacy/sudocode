//! Golden files for the instruction surface.
//!
//! The instruction surface is everything `scode` sends to the model as
//! instruction: the `system` blocks, every tool definition, and the text it
//! places in messages on its own behalf. These tests drive the real CLI
//! through a PTY against the mock provider, read the request bodies it
//! actually sent, and compare them with `tests/fixtures/prompt_surface/*.txt`.
//!
//! A change to any of that text shows up as a diff of a golden file in the
//! same pull request, so prompt edits are reviewed as text. After an intended
//! change, regenerate the files and review their diff:
//!
//! ```text
//! UPDATE_PROMPT_GOLDENS=1 cargo test -p rusty-sudocode-cli --test pty_prompt_surface
//! ```
//!
//! `main_loop.txt` holds a whole request. The other files hold a request as
//! its difference from the main loop's: a `system` block or tool definition
//! that is identical there is named, not repeated, so each piece of text
//! lives in one file.
//!
//! The goldens pin bytes, so these tests always use the mock backend: a live
//! provider has no part in what scode sends.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(90);

/// A line that starts a part of a golden file. Prompt text uses Markdown
/// headings and XML-like tags, never this.
const PART: &str = "@@@ ";

const SAME: &str = "(same as the main loop)";

fn requests(env: &TestEnv) -> Vec<Value> {
    env.captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).expect("request body is JSON"))
        .collect()
}

fn part(out: &mut String, title: &str, body: &str) {
    writeln!(out, "{PART}{title}").unwrap();
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

/// The `system` field as `(title suffix, text)` per block. The suffix carries
/// the block's cache marker: moving text across a marker changes what is cached.
fn system_blocks(request: &Value) -> Vec<(String, String)> {
    match &request["system"] {
        Value::String(text) => vec![(String::new(), text.clone())],
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| {
                let cache = block
                    .get("cache_control")
                    .filter(|cache| !cache.is_null())
                    .map_or_default(|cache| format!(" cache_control={cache}"));
                (cache, block["text"].as_str().unwrap_or_default().to_owned())
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn system_text(request: &Value) -> String {
    let blocks: Vec<String> = system_blocks(request)
        .into_iter()
        .map(|block| block.1)
        .collect();
    blocks.join("\n")
}

fn tools(request: &Value) -> &[Value] {
    request["tools"].as_array().map_or(&[], Vec::as_slice)
}

fn tool_name(tool: &Value) -> &str {
    tool["name"].as_str().unwrap_or("<unnamed>")
}

/// A tool's definition without `defer_loading`, which says when the
/// definition is loaded, not what it is. The tool list records that flag.
fn definition(tool: &Value) -> Value {
    let mut definition = tool.clone();
    if let Some(fields) = definition.as_object_mut() {
        fields.remove("defer_loading");
    }
    definition
}

/// The `system` blocks and tool definitions of `request`. With a `main_loop`
/// request to compare against, whatever is identical there is only named.
fn render_definitions(out: &mut String, request: &Value, main_loop: Option<&Value>) {
    let reference = main_loop.map_or_default(system_blocks);
    for (index, block) in system_blocks(request).iter().enumerate() {
        let title = format!("system[{index}]{}", block.0);
        let body = if reference.get(index) == Some(block) {
            SAME
        } else {
            &block.1
        };
        part(out, &title, body);
    }

    let unchanged = |tool: &Value| {
        main_loop.is_some_and(|main_loop| {
            tools(main_loop)
                .iter()
                .any(|other| definition(other) == definition(tool))
        })
    };
    let mut order = String::new();
    for tool in tools(request) {
        order.push_str(tool_name(tool));
        if tool["defer_loading"].as_bool().unwrap_or(false) {
            order.push_str(" (deferred)");
        }
        if main_loop.is_some() && !unchanged(tool) {
            order.push_str(" (defined below)");
        }
        order.push('\n');
    }
    let title = if main_loop.is_some() {
        "tools in request order; those not marked (defined below) are defined as in the main loop"
    } else {
        "tools in request order"
    };
    part(out, title, &order);
    if let Some(main_loop) = main_loop {
        let dropped: Vec<&str> = tools(main_loop)
            .iter()
            .map(tool_name)
            .filter(|name| tools(request).iter().all(|tool| tool_name(tool) != *name))
            .collect();
        part(
            out,
            "main-loop tools this request does not offer",
            &dropped.join("\n"),
        );
    }
    for tool in tools(request).iter().filter(|tool| !unchanged(tool)) {
        let name = tool_name(tool);
        part(
            out,
            &format!("tool {name}: description"),
            tool["description"].as_str().unwrap_or_default(),
        );
        // The rest of the definition, whose parameter descriptions the model
        // reads too.
        let mut rest = definition(tool);
        if let Some(fields) = rest.as_object_mut() {
            fields.remove("description");
        }
        part(
            out,
            &format!("tool {name}: definition without its description"),
            &serde_json::to_string_pretty(&rest).unwrap(),
        );
    }
}

/// One message's text blocks. Other block kinds are named, not dumped: their
/// content is the conversation, not instruction.
fn render_message(out: &mut String, index: usize, message: &Value) {
    let role = message["role"].as_str().unwrap_or("<no role>");
    match &message["content"] {
        Value::String(text) => part(out, &format!("message[{index}] {role}"), text),
        Value::Array(blocks) => {
            for (block_index, block) in blocks.iter().enumerate() {
                let kind = block["type"].as_str().unwrap_or("<no type>");
                let title = format!("message[{index}] {role} block[{block_index}] {kind}");
                part(out, &title, block["text"].as_str().unwrap_or_default());
            }
        }
        _ => {}
    }
}

fn render_messages(out: &mut String, request: &Value) {
    for (index, message) in messages(request).iter().enumerate() {
        render_message(out, index, message);
    }
}

fn messages(request: &Value) -> &[Value] {
    request["messages"].as_array().map_or(&[], Vec::as_slice)
}

/// All text of one message, for recognising a request by what it carries.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Replace what differs between runs and machines with placeholders: the
/// fixture's directories, the path separator after them, the operating
/// system, today's date, and the identifiers and timings of this run.
fn normalize(text: &str, env: &TestEnv) -> String {
    let mut text = text.to_owned();
    let mut roots = vec![
        (env.config_home().to_path_buf(), "<CONFIG_HOME>"),
        (env.workspace_root().to_path_buf(), "<WORKSPACE>"),
    ];
    // Longest first, so a directory inside another is not half-replaced.
    roots.sort_by_key(|(path, _)| std::cmp::Reverse(path.as_os_str().len()));
    for (path, placeholder) in &roots {
        for spelling in path_spellings(path) {
            text = text.replace(&spelling, placeholder);
        }
    }
    for (_, placeholder) in &roots {
        text = forward_slashes_after(&text, placeholder);
    }
    text = text.replace(
        &format!("Platform: {}", std::env::consts::OS),
        "Platform: <OS>",
    );
    for (lead, placeholder) in [
        ("agent-", "<ID>"),
        ("tool-batch-", "<N>"),
        ("<duration_ms>", "<N>"),
    ] {
        text = replace_digits_after(&text, lead, placeholder);
    }
    // A sub-agent's colour is derived from its identifier.
    text = replace_between(&text, "<color>", "</color>", "<COLOR>");
    replace_date(&text)
}

/// The ways one directory can be written in a request: as given, canonical,
/// with the Windows extended-length prefix, with forward slashes, and with
/// its backslashes escaped as in JSON.
fn path_spellings(path: &Path) -> Vec<String> {
    let mut spellings = Vec::new();
    let mut push = |candidate: String| {
        if !candidate.is_empty() && !spellings.contains(&candidate) {
            spellings.push(candidate);
        }
    };
    let given = path.display().to_string();
    let canonical =
        fs::canonicalize(path).map_or_default(|resolved| resolved.display().to_string());
    for spelling in [canonical, given] {
        let bare = spelling
            .strip_prefix(r"\\?\")
            .unwrap_or(&spelling)
            .to_owned();
        if bare.is_empty() {
            continue;
        }
        push(format!(r"\\?\{bare}"));
        push(bare.replace('\\', "/"));
        push(bare.replace('\\', r"\\"));
        push(bare);
    }
    // Longest first: a longer spelling contains a shorter one.
    spellings.sort_by_key(|spelling| std::cmp::Reverse(spelling.len()));
    spellings
}

/// Rewrite `\` to `/` in the path that follows `placeholder`, so one golden
/// file serves Windows and Unix.
fn forward_slashes_after(text: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(placeholder) {
        let after = start + placeholder.len();
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        let end = rest
            .find(|c: char| c.is_whitespace() || "`\"'<>),".contains(c))
            .unwrap_or(rest.len());
        out.push_str(&rest[..end].replace(r"\\", "/").replace('\\', "/"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Replace the run of digits that follows each `lead` with `placeholder`.
fn replace_digits_after(text: &str, lead: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(lead) {
        let after = start + lead.len();
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            out.push_str(placeholder);
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

/// Replace whatever stands between each `open` and the `close` after it.
fn replace_between(text: &str, open: &str, close: &str, placeholder: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after = start + open.len();
        let Some(length) = rest[after..].find(close) else {
            break;
        };
        out.push_str(&rest[..after]);
        out.push_str(placeholder);
        rest = &rest[after + length..];
    }
    out.push_str(rest);
    out
}

fn replace_date(text: &str) -> String {
    const LEAD: &str = "Today's date is ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(LEAD) {
        let after = start + LEAD.len();
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        let is_date = rest.len() >= 10
            && rest
                .bytes()
                .take(10)
                .enumerate()
                .all(|(index, byte)| match index {
                    4 | 7 => byte == b'-',
                    _ => byte.is_ascii_digit(),
                });
        if is_date {
            out.push_str("<DATE>");
            rest = &rest[10..];
        }
    }
    out.push_str(rest);
    out
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/prompt_surface")
        .join(format!("{name}.txt"))
}

fn updating() -> bool {
    std::env::var_os("UPDATE_PROMPT_GOLDENS").is_some()
}

fn read_golden(name: &str) -> String {
    let path = golden_path(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{name}: cannot read {}: {error}. Create it with UPDATE_PROMPT_GOLDENS=1.",
            path.display()
        )
    });
    // A checkout with CRLF conversion rewrites the golden file's line endings.
    text.replace("\r\n", "\n")
}

/// Compare with the golden file, or rewrite it under `UPDATE_PROMPT_GOLDENS`.
fn assert_golden(name: &str, actual: &str) {
    assert!(
        !actual.contains('\r'),
        "{name}: the request carries carriage returns, so prompt text was compiled from a file \
         with CRLF line endings"
    );
    if updating() {
        let path = golden_path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, actual).unwrap();
        return;
    }
    let expected = read_golden(name);
    if expected == actual {
        return;
    }
    let line = expected
        .lines()
        .zip(actual.lines())
        .position(|(want, got)| want != got)
        .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
    panic!(
        "{name}: the instruction surface differs from {} at line {}.\n  golden: {:?}\n  actual: {:?}\n\
         If the change is intended, rerun with UPDATE_PROMPT_GOLDENS=1 and review the diff.",
        golden_path(name).display(),
        line + 1,
        expected.lines().nth(line).unwrap_or("<end of file>"),
        actual.lines().nth(line).unwrap_or("<end of file>"),
    );
}

/// The other goldens are written relative to the main loop. A request used as
/// that reference must therefore carry exactly what `main_loop.txt` records.
fn assert_is_main_loop(reference: &Value, env: &TestEnv) {
    if updating() {
        return;
    }
    let mut definitions = String::new();
    render_definitions(&mut definitions, reference, None);
    assert!(
        read_golden("main_loop").starts_with(&normalize(&definitions, env)),
        "this session's own system blocks or tools differ from main_loop.txt, so it cannot serve \
         as the reference for a relative golden"
    );
}

fn relative_golden(name: &str, request: &Value, reference: &Value, env: &TestEnv) {
    let mut out = String::new();
    render_definitions(&mut out, request, Some(reference));
    render_messages(&mut out, request);
    assert_golden(name, &normalize(&out, env));
}

/// Pin the session's agent name, which the agent-to-agent section quotes.
fn pin_agent_name(env: &TestEnv) {
    let dir = env.workspace_root().join(".nexus/sudocode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.json"),
        json!({ "agentName": "prompt-surface" }).to_string(),
    )
    .unwrap();
}

fn fixture(label: &str) -> TestEnv {
    let env = TestEnv::new_mock(label);
    pin_agent_name(&env);
    env
}

/// Run `scode` to completion and return every request it has sent so far.
fn run(env: &TestEnv, args: &[&str]) -> Vec<Value> {
    let mut session = env.spawn(args);
    session.set_default_timeout(WAIT);
    assert_eq!(session.expect_eof().unwrap(), 0);
    requests(env)
}

fn start(env: &TestEnv, args: &[&str], vars: &[(&str, &str)]) -> PtySession {
    let mut session = env.spawn_with_env(args, vars);
    session.set_default_timeout(WAIT);
    common::expect_input_line_cleared(&session, WAIT, "ready for input");
    session
}

/// Submit `prompt` and wait for the turn it starts to finish.
fn turn(session: &mut PtySession, prompt: &str) {
    let before = common::turn_status_marker(session);
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(session, &before, WAIT, "the turn completes");
}

fn leave(mut session: PtySession) {
    session.send("/exit").unwrap();
    common::expect_input_line(&session, "/exit", WAIT, "exit entered");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

/// One interactive turn; returns every request `scode` has sent so far.
fn interactive_turn(
    env: &TestEnv,
    prompt: &str,
    args: &[&str],
    vars: &[(&str, &str)],
) -> Vec<Value> {
    let mut session = start(env, args, vars);
    turn(&mut session, prompt);
    leave(session);
    requests(env)
}

/// The request whose first message carries `marker`.
fn request_opening_with<'a>(sent: &'a [Value], marker: &str) -> &'a Value {
    sent.iter()
        .find(|request| {
            messages(request)
                .first()
                .is_some_and(|first| message_text(first).contains(marker))
        })
        .unwrap_or_else(|| panic!("no request opens with {marker:?}"))
}

/// One ordinary interactive turn; its request is the main loop's surface.
fn main_loop(env: &TestEnv) -> Value {
    let prompt = env.prompt("MAIN_LOOP_PROBE", "single_turn_text");
    let sent = interactive_turn(env, &prompt, &[], &[]);
    request_opening_with(&sent, "MAIN_LOOP_PROBE").clone()
}

/// A saved conversation long enough for compaction to have something to do.
fn saved_history(env: &TestEnv) -> PathBuf {
    let mut history = runtime::Session::new().with_workspace_root(env.workspace_root());
    let opening = env.prompt("History to summarise.", "llm_compaction_roundtrip");
    for index in 0..10 {
        let lead = if index == 0 {
            opening.clone()
        } else {
            format!("Historical implementation log {index}. ")
        };
        let text = lead.clone() + &"x".repeat(5_997 - lead.len());
        history
            .push_message(if index % 2 == 0 {
                runtime::ConversationMessage::user_text(text)
            } else {
                runtime::ConversationMessage::assistant(vec![runtime::ContentBlock::Text { text }])
            })
            .unwrap();
    }
    let path = env.workspace_root().join("history.jsonl");
    history.save_to_path(&path).unwrap();
    path
}

/// The request that asks for a summary: its `system` and tools relative to
/// the main loop, and its instruction, which is the last message.
fn compaction_golden(name: &str, sent: &[Value], reference: &Value, env: &TestEnv) {
    let (request, instruction) = sent
        .iter()
        .filter_map(|request| Some((request, messages(request).last()?)))
        .find(|(_, last)| message_text(last).contains("Respond with TEXT ONLY"))
        .expect("scode sent a compaction request");
    let mut out = String::new();
    render_definitions(&mut out, request, Some(reference));
    part(&mut out, "last message", &message_text(instruction));
    assert_golden(name, &normalize(&out, env));
}

/// The request of an ordinary interactive session, whole.
#[test]
fn main_loop_request() {
    let env = fixture("prompt-surface-main");
    let request = main_loop(&env);
    let mut out = String::new();
    render_definitions(&mut out, &request, None);
    render_messages(&mut out, &request);
    assert_golden("main_loop", &normalize(&out, &env));
}

/// Print mode (`-p`).
#[test]
fn print_mode_request() {
    let env = fixture("prompt-surface-print");
    let reference = main_loop(&env);
    assert_is_main_loop(&reference, &env);

    let prompt = env.prompt("PRINT_MODE_PROBE", "single_turn_text");
    let sent = run(&env, &["-p", &prompt]);
    let request = request_opening_with(&sent, "PRINT_MODE_PROBE");
    relative_golden("print_mode", request, &reference, &env);
}

/// Coordinator mode.
#[test]
fn coordinator_request() {
    let env = fixture("prompt-surface-coordinator");
    let reference = main_loop(&env);
    assert_is_main_loop(&reference, &env);

    let prompt = env.prompt("COORDINATOR_PROBE", "single_turn_text");
    let sent = interactive_turn(&env, &prompt, &[], &[("SUDOCODE_COORDINATOR_MODE", "1")]);
    let request = request_opening_with(&sent, "COORDINATOR_PROBE");
    relative_golden("coordinator", request, &reference, &env);
}

/// The request that asks the model to summarise history, here started with
/// `scode --resume <session> /compact`. Against this provider `/compact`
/// typed in a session sends the same `system` block and instruction.
#[test]
fn compaction_request() {
    let env = fixture("prompt-surface-compaction");
    let reference = main_loop(&env);
    assert_is_main_loop(&reference, &env);

    let history = saved_history(&env);
    let sent = run(&env, &["--resume", history.to_str().unwrap(), "/compact"]);
    compaction_golden("compaction", &sent, &reference, &env);
}

/// One way to start a sub-agent, and how to recognise its requests.
struct SubAgent {
    golden: &'static str,
    /// Text only this kind of sub-agent has in its `system` field.
    identity: &'static str,
    tool: &'static str,
    agent: Option<&'static str>,
    /// The parent lists a custom agent, so it is not the plain main loop.
    plain_parent: bool,
}

const SUB_AGENTS: &[SubAgent] = &[
    SubAgent {
        golden: "sub_agent_general_purpose",
        identity: "sub-agent of type `general-purpose`",
        tool: "agent_spawn",
        agent: Some("general-purpose"),
        plain_parent: true,
    },
    SubAgent {
        golden: "sub_agent_explore",
        identity: "sub-agent of type `Explore`",
        tool: "agent_spawn",
        agent: Some("Explore"),
        plain_parent: true,
    },
    SubAgent {
        golden: "sub_agent_plan",
        identity: "sub-agent of type `Plan`",
        tool: "agent_spawn",
        agent: Some("Plan"),
        plain_parent: true,
    },
    SubAgent {
        golden: "sub_agent_verification",
        identity: "sub-agent of type `Verification`",
        tool: "agent_spawn",
        agent: Some("Verification"),
        plain_parent: true,
    },
    SubAgent {
        golden: "sub_agent_custom",
        identity: "custom sub-agent `surface-archivist`",
        tool: "agent_spawn",
        agent: Some("surface-archivist"),
        plain_parent: false,
    },
    SubAgent {
        golden: "sub_agent_fork",
        identity: "You are a fork subagent",
        tool: "pid_fork",
        agent: None,
        plain_parent: true,
    },
];

/// A custom agent that declares a memory scope, which is what selects the
/// full edition of the memory instructions.
fn write_custom_agent(env: &TestEnv) {
    let dir = env.workspace_root().join(".nexus/sudocode/agents");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("surface-archivist.md"),
        "---\nname: surface-archivist\ndescription: Prompt surface probe.\n\
         tools: [read_file]\nmemory: project\n---\nCUSTOM_AGENT_BODY\n",
    )
    .unwrap();
}

/// The first request of each kind of sub-agent.
#[test]
fn sub_agent_requests() {
    for kind in SUB_AGENTS {
        let env = fixture(&format!("prompt-surface-{}", kind.golden));
        if !kind.plain_parent {
            write_custom_agent(&env);
        }
        let task = env.prompt("SUB_AGENT_PROBE", "single_turn_text");
        let mut input = json!({
            "description": "Prompt surface probe",
            "prompt": task,
            "run_in_background": false,
        });
        if let Some(agent) = kind.agent {
            input["agent"] = json!(agent);
            input["context"] = json!("CONTEXT_FIELD");
            input["constraints"] = json!("CONSTRAINTS_FIELD");
            input["acceptance"] = json!("ACCEPTANCE_FIELD");
        }
        let spawn = env.tool_batch_prompt(&json!([{
            "id": "toolu_prompt_surface_spawn",
            "name": kind.tool,
            "input": input,
        }]));
        let sent = interactive_turn(
            &env,
            &spawn,
            &["--permission-mode", "danger-full-access"],
            &[],
        );
        let parent = request_opening_with(&sent, "TOOL_BATCH:");
        if kind.plain_parent {
            assert_is_main_loop(parent, &env);
        }
        let child = sent
            .iter()
            .find(|request| system_text(request).contains(kind.identity))
            .unwrap_or_else(|| panic!("{}: the sub-agent sent no request", kind.golden));
        relative_golden(kind.golden, child, parent, &env);
    }
}

/// What the parent is told when a background sub-agent finishes.
#[test]
fn background_completion_notice() {
    let env = fixture("prompt-surface-background");
    let wait = env.tool_batch_prompt(&json!([{
        "id": "toolu_prompt_surface_wait",
        "name": "bash",
        "input": {
            "command": "while [ ! -f release ]; do sleep 0.2; done; echo BACKGROUND_PROBE_DONE",
            "timeout": 60000,
        },
    }]));
    let spawn = env.tool_batch_prompt(&json!([{
        "id": "toolu_prompt_surface_spawn",
        "name": "agent_spawn",
        "input": {
            "description": "Prompt surface probe",
            "agent": "Verification",
            "prompt": wait,
            "run_in_background": true,
        },
    }]));
    let mut session = start(&env, &["--permission-mode", "danger-full-access"], &[]);
    turn(&mut session, &spawn);
    // The launch turn is over and the child is parked on the file. Releasing
    // it now makes the notice start a turn of its own.
    let before = common::turn_status_marker(&session);
    fs::write(env.workspace_root().join("release"), "released").unwrap();
    common::expect_turn_complete_after(&session, &before, WAIT, "the notice is delivered");
    leave(session);

    let sent = requests(&env);
    let notice = sent
        .iter()
        .flat_map(messages)
        .filter(|message| message["role"] == "user")
        .map(message_text)
        .find(|text| text.contains("<task-notification>"))
        .expect("the parent was sent a completion notice");
    let mut out = String::new();
    part(&mut out, "user message carrying the notice", &notice);
    assert_golden("background_completion", &normalize(&out, &env));
}
