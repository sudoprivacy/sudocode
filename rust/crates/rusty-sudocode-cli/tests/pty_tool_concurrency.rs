//! Real tool processes and a PTY: FIFO handshakes prove overlap without
//! inferring it from two cards, an executor stub, or elapsed-time thresholds.
#![cfg(unix)]
mod common;

use common::TestEnv;
use nix::{libc, sys::stat::Mode, unistd::mkfifo};
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};

fn bash(id: &str, command: &str) -> Value {
    json!({"id": id, "name": "Bash", "input": {"command": command}})
}

fn fifo(env: &TestEnv, name: &str) {
    mkfifo(
        &env.workspace_root().join(name),
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .unwrap();
}

fn try_writer(root: &Path, name: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(root.join(name))
}

fn writer(env: &TestEnv, name: &str, sess: &PtySession) -> File {
    let deadline = Instant::now()
        + if env.is_live() {
            common::LIVE_TURN_BUDGET
        } else {
            Duration::from_secs(30)
        };
    loop {
        match try_writer(env.workspace_root(), name) {
            Ok(file) => return file,
            Err(error)
                if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(error) => panic!(
                "reader did not start for {name}: {error}\n{}",
                sess.render(|s| s.contents())
            ),
        }
    }
}

fn release(mut writer: File, text: &str) {
    writeln!(writer, "{text}").unwrap();
}

fn start(env: &TestEnv, calls: &[Value], cap: &str) -> PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_MAX_TOOL_USE_CONCURRENCY", cap),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", ""),
        ],
    );
    sess.resize(80, 100).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("❯"),
        Duration::from_secs(30),
        "prompt",
    );
    send_batch(env, &mut sess, calls);
    sess
}

fn send_batch(env: &TestEnv, sess: &mut PtySession, calls: &[Value]) {
    let batch = serde_json::to_string(calls).unwrap();
    let prompt = if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}")
    } else {
        format!("In one assistant message issue all these tool calls together, using their names and inputs exactly (ids are fixture labels). Do not run extra tools or wait for one result before requesting the next: {batch}. After all results, reply exactly: Concurrency batch done.")
    };
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("TOOL_BATCH:") || s.contains("In one assistant"),
        common::DEFAULT_TIMEOUT,
        "batch input",
    );
    sess.send("\r").unwrap();
}

fn done(sess: &PtySession) {
    common::expect_screen_settled(
        sess,
        |s| s.contains("Concurrency batch done.") && s.contains("ctx "),
        Duration::from_secs(30),
        "batch complete",
    );
}

fn assert_result_order(env: &TestEnv, expected: &[&str]) {
    let requests = env.captured_message_bodies();
    let request: Value = serde_json::from_str(requests.last().unwrap()).unwrap();
    let ids: Vec<_> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["type"] == "tool_result")
        .map(|block| block["tool_use_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, expected, "provider call/result order");
}

fn finish(mut sess: PtySession) {
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

// Resize geometry and the simultaneous FIFO handshake live in
// pty_chrome_resize, which uses the real terminal reflow model.

#[test]
fn fast_result_and_error_are_visible_before_slow_sibling_finishes() {
    let env = TestEnv::new_mock("parallel-completion");
    fifo(&env, "slow.fifo");
    std::fs::write(
        env.workspace_root().join("fast.txt"),
        "FAST_SIBLING_OUTPUT\n",
    )
    .unwrap();
    let sess = start(
        &env,
        &[
            bash("slow", "cat slow.fifo"),
            bash("fast", "cat fast.txt"),
            bash("error", "cat absent-concurrency-fixture.txt"),
        ],
        "10",
    );
    let slow = writer(&env, "slow.fifo", &sess);
    common::expect_screen(
        &sess,
        |s| {
            s.contains("FAST_SIBLING_OUTPUT")
                && (s.contains("No such file") || s.contains("cannot open"))
        },
        Duration::from_secs(15),
        "fast success and failure without waiting for slow sibling",
    );
    assert!(!sess
        .render(|s| s.raw().contents())
        .contains("Concurrency batch done."));
    release(slow, "SLOW_SIBLING_OUTPUT");
    done(&sess);
    assert_result_order(&env, &["slow", "fast", "error"]);
    finish(sess);
}

#[test]
fn writer_barrier_waits_for_all_reads_and_next_read_sees_write() {
    let env = TestEnv::new_mock("parallel-barrier");
    fifo(&env, "one.fifo");
    fifo(&env, "two.fifo");
    let sess = start(
        &env,
        &[
            bash("one", "cat one.fifo"),
            bash("two", "cat two.fifo"),
            bash("write", "printf WRITER_VALUE > result.txt"),
            json!({"id":"read", "name":"Read", "input":{"path":"result.txt"}}),
        ],
        "10",
    );
    let first = writer(&env, "one.fifo", &sess);
    let second = writer(&env, "two.fifo", &sess);
    assert!(!env.workspace_root().join("result.txt").exists());
    release(first, "FIRST_COMPLETE");
    common::expect_screen(
        &sess,
        |s| s.contains("FIRST_COMPLETE"),
        common::DEFAULT_TIMEOUT,
        "first result",
    );
    assert!(
        !env.workspace_root().join("result.txt").exists(),
        "writer crossed unfinished sibling"
    );
    release(second, "SECOND_COMPLETE");
    done(&sess);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("result.txt")).unwrap(),
        "WRITER_VALUE"
    );
    assert!(sess.render(|s| s.raw().contents()).contains("WRITER_VALUE"));
    finish(sess);
}

#[test]
fn concurrency_limit_is_enforced_before_tool_started() {
    let env = TestEnv::new_mock("parallel-limit");
    fifo(&env, "one.fifo");
    fifo(&env, "two.fifo");
    let sess = start(
        &env,
        &[bash("one", "cat one.fifo"), bash("two", "cat two.fifo")],
        "1",
    );
    let first = writer(&env, "one.fifo", &sess);
    assert_eq!(
        try_writer(env.workspace_root(), "two.fifo")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENXIO)
    );
    common::expect_screen(
        &sess,
        |s| s.lines().filter(|r| r.starts_with("╭─ Bash(")).count() == 2,
        common::DEFAULT_TIMEOUT,
        "both pending cards painted",
    );
    // The first border is running; the second must retain its queued color.
    sess.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        let colors: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.starts_with("╭─ Bash("))
            .map(|(i, _)| format!("{:?}", raw.cell(i as u16, 0).unwrap().fgcolor()))
            .collect();
        assert_eq!(colors.len(), 2, "{rows:#?}");
        assert_ne!(
            colors[0], colors[1],
            "queued and running must be distinguishable"
        );
    });
    release(first, "FIRST_FINISHED");
    let second = writer(&env, "two.fifo", &sess);
    release(second, "SECOND_FINISHED");
    done(&sess);
    finish(sess);
}

#[test]
fn pre_hook_rewrite_to_writer_is_a_barrier_and_runs_once() {
    let env = TestEnv::new_mock("parallel-hook-rewrite");
    fifo(&env, "one.fifo");
    fifo(&env, "three.fifo");
    let dir = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&dir).unwrap();
    let script = r#"import json,os
p=json.loads(os.environ['HOOK_TOOL_INPUT'])
if p.get('command')=='cat rewrite.txt':
    with open('hook-count.txt','a') as f: f.write('once\n')
    print(json.dumps({'hookSpecificOutput':{'updatedInput':{'command':'printf HOOK_WRITE > rewritten.txt'}}}))
"#;
    std::fs::write(env.workspace_root().join("rewrite.py"), script).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        json!({"hooks":{"PreToolUse":["python3 rewrite.py"]}}).to_string(),
    )
    .unwrap();
    let sess = start(
        &env,
        &[
            bash("one", "cat one.fifo"),
            bash("rewrite", "cat rewrite.txt"),
            bash("three", "cat three.fifo"),
        ],
        "10",
    );
    let first = writer(&env, "one.fifo", &sess);
    assert!(!env.workspace_root().join("rewritten.txt").exists());
    release(first, "BEFORE_HOOK_BARRIER");
    let third = writer(&env, "three.fifo", &sess);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("rewritten.txt")).unwrap(),
        "HOOK_WRITE"
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("hook-count.txt")).unwrap(),
        "once\n"
    );
    release(third, "AFTER_HOOK_BARRIER");
    done(&sess);
    finish(sess);
}

struct Server(std::process::Child);

impl Server {
    fn wait_ready(&mut self, root: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(port) = std::fs::read_to_string(root.join("http-port")) {
                return port;
            }
            let status = self.0.try_wait().expect("poll HTTP fixture process");
            assert!(
                status.is_none() && Instant::now() < deadline,
                "HTTP fixture did not become ready (exit: {status:?}): {}",
                std::fs::read_to_string(root.join("http-server.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_file(env: &TestEnv, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !env.workspace_root().join(name).exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {name}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn configure_mcp(env: &TestEnv, http: bool) -> Option<Server> {
    let root = env.workspace_root();
    let script = root.join("concurrent_mcp.py");
    std::fs::write(&script, include_str!("fixtures/concurrent_mcp.py")).unwrap();
    let mut server = http.then(|| {
        Server(
            std::process::Command::new(common::resolve_python())
                .arg(&script)
                .arg("http")
                .current_dir(root)
                .stderr(std::fs::File::create(root.join("http-server.log")).unwrap())
                .spawn()
                .unwrap(),
        )
    });
    let config = if let Some(server) = server.as_mut() {
        let port = server.wait_ready(root);
        json!({"type":"http", "url": format!("http://127.0.0.1:{port}/mcp")})
    } else {
        json!({"command":common::resolve_python(), "args":[script], "cwd":root})
    };
    let dir = root.join(".nexus/sudocode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        json!({"experimental":{"mcpConfigServers":true}, "mcpServers":{"parallel":config}})
            .to_string(),
    )
    .unwrap();
    server
}

fn mcp_overlap(http: bool) {
    let env = TestEnv::new_mock("parallel-mcp");
    let _server = configure_mcp(&env, http);
    let root = env.workspace_root();
    let call = |id: &str, name: &str| json!({"id":id, "name": format!("mcp__parallel__{name}"), "input":{"label":id}});
    let sess = start(
        &env,
        &[
            call("slow", "read"),
            call("fast", "read"),
            call("error", "read"),
            call("writer", "write"),
            call("last", "read"),
        ],
        "10",
    );
    // Three requests must reach one server before any response is released.
    for name in ["slow.started", "fast.started", "error.started"] {
        wait_file(&env, name);
    }
    assert!(!root.join("writer.started").exists());
    for name in ["fast.release", "error.release"] {
        std::fs::write(root.join(name), "").unwrap();
    }
    common::expect_screen(
        &sess,
        |s| s.contains("MCP_RESULT_fast") && s.contains("MCP_RESULT_error"),
        common::DEFAULT_TIMEOUT,
        "out-of-order MCP success and failure",
    );
    assert!(
        !root.join("writer.started").exists(),
        "MCP writer crossed slow sibling"
    );
    std::fs::write(root.join("slow.release"), "").unwrap();
    wait_file(&env, "last.started");
    assert!(root.join("writer.finished").exists());
    std::fs::write(root.join("last.release"), "").unwrap();
    done(&sess);
    assert_result_order(&env, &["slow", "fast", "error", "writer", "last"]);
    finish(sess);
}

#[test]
fn mcp_stdio_readonly_overlap_maps_out_of_order_ids_and_isolates_errors() {
    mcp_overlap(false);
}

#[test]
fn mcp_http_readonly_overlap_maps_out_of_order_ids_and_isolates_errors() {
    mcp_overlap(true);
}

#[test]
fn cancelling_parallel_bash_stops_both_processes_and_preserves_pairs() {
    let env = TestEnv::new_mock("parallel-cancel");
    fifo(&env, "one.fifo");
    fifo(&env, "two.fifo");
    let mut sess = start(
        &env,
        &[bash("one", "cat one.fifo"), bash("two", "cat two.fifo")],
        "10",
    );
    let first = writer(&env, "one.fifo", &sess);
    let second = writer(&env, "two.fifo", &sess);
    sess.send("\x1b").unwrap();
    common::expect_screen(
        &sess,
        |s| s.to_lowercase().contains("cancelled"),
        common::DEFAULT_TIMEOUT,
        "parallel cancellation",
    );
    // Keep the writers open so cat cannot exit via EOF. Disappearing readers
    // proves cancellation actually stopped both subprocesses.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stopped = ["one.fifo", "two.fifo"]
            .iter()
            .all(|name| try_writer(env.workspace_root(), name).is_err());
        if stopped {
            break;
        }
        assert!(Instant::now() < deadline, "cancelled Bash readers survived");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(first);
    drop(second);
    common::expect_input_line_cleared(
        &sess,
        common::DEFAULT_TIMEOUT,
        "input after parallel cancellation",
    );
    sess.send("continue").unwrap();
    common::expect_input_line(
        &sess,
        "continue",
        common::DEFAULT_TIMEOUT,
        "next turn input",
    );
    sess.send("\r").unwrap();
    done(&sess);
    assert_result_order(&env, &["one", "two"]);
    finish(sess);
}

#[test]
fn literal_shell_lists_pipes_and_redirects_allow_real_overlap() {
    let env = TestEnv::new_mock("parallel-shell-syntax");
    fifo(&env, "one.fifo");
    fifo(&env, "two.fifo");
    let sess = start(&env, &[
        bash("one", "printf '%s\\n' 'literal ; > harmless $(not-executed)' | cat; cat one.fifo 2>/dev/null"),
        bash("two", "cd . && cat two.fifo"),
    ], "10");
    let first = writer(&env, "one.fifo", &sess);
    let second = writer(&env, "two.fifo", &sess);
    release(first, "LITERAL_ONE");
    release(second, "LITERAL_TWO");
    done(&sess);
    finish(sess);
}

#[test]
fn cancelled_mcp_requests_notify_server_and_late_replies_do_not_poison_next_turn() {
    let env = TestEnv::new_mock("parallel-mcp-cancel");
    let _server = configure_mcp(&env, false);
    let call = |id: &str| json!({"id":id, "name":"mcp__parallel__read", "input":{"label":id}});
    let mut sess = start(&env, &[call("one"), call("two")], "10");
    for name in ["one.started", "two.started"] {
        wait_file(&env, name);
    }
    sess.send("\x1b").unwrap();
    common::expect_screen(
        &sess,
        |s| s.to_lowercase().contains("cancelled"),
        common::DEFAULT_TIMEOUT,
        "MCP cancellation",
    );
    for name in ["one.cancelled", "two.cancelled"] {
        wait_file(&env, name);
    }
    // This server deliberately finishes late despite receiving cancellation.
    // Those old ids must not be mistaken for the new turn's response.
    for name in ["one.release", "two.release"] {
        std::fs::write(env.workspace_root().join(name), "").unwrap();
    }
    common::expect_input_line_cleared(
        &sess,
        common::DEFAULT_TIMEOUT,
        "input after MCP cancellation",
    );
    let prompt = format!(
        "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
        json!([call("fresh")])
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("fresh"),
        common::DEFAULT_TIMEOUT,
        "next MCP input",
    );
    sess.send("\r").unwrap();
    wait_file(&env, "fresh.started");
    std::fs::write(env.workspace_root().join("fresh.release"), "").unwrap();
    done(&sess);
    assert_result_order(&env, &["one", "two", "fresh"]);
    let screen = sess.render(|s| s.contents());
    assert!(screen.contains("MCP_RESULT_fresh"));
    assert!(!screen.contains("MCP_RESULT_one") && !screen.contains("MCP_RESULT_two"));
    finish(sess);
}

#[test]
fn subagent_blocking_tools_overlap_and_accept_the_same_aliases_as_parent() {
    let env = TestEnv::new_mock("parallel-subagent");
    fifo(&env, "one.fifo");
    fifo(&env, "two.fifo");
    let child_calls = [
        bash("childone", "cat one.fifo"),
        bash("childtwo", "cat two.fifo"),
    ];
    let task = format!(
        "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
        json!(child_calls)
    );
    let sess = start(
        &env,
        &[
            json!({"id":"agent", "name":"agent_spawn", "input":{"agent":"general-purpose", "prompt":task, "run_in_background":false}}),
        ],
        "10",
    );
    let first = writer(&env, "one.fifo", &sess);
    let second = writer(&env, "two.fifo", &sess);
    release(first, "CHILD_ONE");
    release(second, "CHILD_TWO");
    done(&sess);
    finish(sess);
}

#[test]
fn completed_tool_runs_and_renders_while_provider_stream_is_open() {
    let env = TestEnv::new_mock("parallel-stream-dispatch");
    fifo(&env, "early.fifo");
    let mut call = bash("early", "cat early.fifo");
    let gate = env.workspace_root().join("finish-stream");
    call["stream_wait_for"] = json!(gate);
    let sess = start(&env, &[call], "10");
    release(writer(&env, "early.fifo", &sess), "EARLY_TOOL_COMPLETED");
    common::expect_screen(
        &sess,
        |s| s.contains("EARLY_TOOL_COMPLETED"),
        common::DEFAULT_TIMEOUT,
        "tool result before provider stream closes",
    );
    assert!(!gate.exists());
    std::fs::write(gate, "continue").unwrap();
    done(&sess);
    assert_result_order(&env, &["early"]);
    finish(sess);
}

#[test]
fn pre_and_post_hooks_overlap_without_holding_the_tool_event_loop() {
    let env = TestEnv::new("parallel-hook-lifecycle");
    let dir = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["one", "two"] {
        std::fs::write(
            env.workspace_root().join(format!("{name}.txt")),
            format!("HOOK_{name}_RESULT"),
        )
        .unwrap();
    }
    std::fs::write(
        env.workspace_root().join("gate.py"),
        r#"import json,os,sys,time
from pathlib import Path
command=json.loads(os.environ['HOOK_TOOL_INPUT']).get('command','')
if command in ('cat one.txt','cat two.txt'):
    phase=sys.argv[1]
    name=command.split()[1].split('.')[0]
    Path(phase+'-'+name).write_text('started')
    deadline=time.monotonic()+30
    while not Path('release-'+phase).exists():
        if time.monotonic()>deadline: raise RuntimeError('hook peers did not overlap')
        time.sleep(.01)
"#,
    )
    .unwrap();
    std::fs::write(dir.join("settings.json"), json!({"hooks":{"PreToolUse":["python3 gate.py pre"],"PostToolUse":["python3 gate.py post"]}}).to_string()).unwrap();
    let sess = start(
        &env,
        &[bash("one", "cat one.txt"), bash("two", "cat two.txt")],
        "10",
    );
    for phase in ["pre", "post"] {
        wait_file(&env, &format!("{phase}-one"));
        wait_file(&env, &format!("{phase}-two"));
        std::fs::write(env.workspace_root().join(format!("release-{phase}")), "go").unwrap();
    }
    done(&sess);
    let screen = sess.render(|s| s.contents());
    assert!(screen.contains("HOOK_one_RESULT") && screen.contains("HOOK_two_RESULT"));
    finish(sess);
}

#[test]
fn question_wait_keeps_sibling_output_visible_and_queues_second_question() {
    let env = TestEnv::new("parallel-questions");
    fifo(&env, "read.fifo");
    let question = |id: &str, prompt: &str| json!({"id":id, "name":"AskUserQuestion", "input":{"question":prompt}});
    let mut sess = start(
        &env,
        &[
            question("first", "FIRST_PARALLEL_QUESTION"),
            bash("read", "cat read.fifo"),
            question("second", "SECOND_PARALLEL_QUESTION"),
        ],
        "10",
    );
    release(
        writer(&env, "read.fifo", &sess),
        "READ_WHILE_QUESTION_PENDING",
    );
    common::expect_screen(
        &sess,
        |s| s.contains("READ_WHILE_QUESTION_PENDING") && s.contains("1/1  FIRST_PARALLEL_QUESTION"),
        common::DEFAULT_TIMEOUT,
        "read output while first question is unanswered",
    );
    sess.send("first-answer\r").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("1/1  SECOND_PARALLEL_QUESTION"),
        common::DEFAULT_TIMEOUT,
        "second queued question",
    );
    sess.send("second-answer\r").unwrap();
    done(&sess);
    if env.is_mock() {
        assert_result_order(&env, &["first", "read", "second"]);
    }
    finish(sess);
}

#[test]
fn stream_failure_preserves_completed_side_effects_and_closes_pending_ids() {
    let env = TestEnv::new_mock("parallel-stream-failure");
    fifo(&env, "pending.fifo");
    let gate = env.workspace_root().join("break-stream");
    let mut pending = bash("pending", "cat pending.fifo");
    pending["stream_wait_for"] = json!(gate);
    pending["stream_fail_after"] = json!(true);
    let mut sess = start(
        &env,
        &[
            bash("write-once", "printf 'ONCE\n' >> once.txt; cat once.txt"),
            pending,
        ],
        "10",
    );
    let pending_writer = writer(&env, "pending.fifo", &sess);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("once.txt")).unwrap(),
        "ONCE\n"
    );
    std::fs::write(gate, "fail").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Interrupted") || s.contains("interrupted"),
        common::DEFAULT_TIMEOUT,
        "unfinished tool cancelled after provider disconnect",
    );
    let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
    while try_writer(env.workspace_root(), "pending.fifo").is_ok() {
        assert!(Instant::now() < deadline, "tool survived stream failure");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(pending_writer);
    common::expect_input_line_cleared(
        &sess,
        common::DEFAULT_TIMEOUT,
        "input after provider failure",
    );
    sess.send("continue").unwrap();
    common::expect_input_line(
        &sess,
        "continue",
        common::DEFAULT_TIMEOUT,
        "continuation input",
    );
    sess.send("\r").unwrap();
    done(&sess);
    assert_result_order(&env, &["write-once", "pending"]);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("once.txt")).unwrap(),
        "ONCE\n",
        "completed writer must not execute again on recovery"
    );
    finish(sess);
}

#[test]
fn readonly_command_families_globs_and_stdin_redirect_overlap() {
    for prefix in [
        "git branch --show-current",
        "fd --help",
        "gh pr list --help",
        "diff --help",
        "ls *.txt",
    ] {
        let env = TestEnv::new_mock("parallel-readonly-command");
        fifo(&env, "one.fifo");
        fifo(&env, "two.fifo");
        std::fs::write(
            env.workspace_root().join("fixture.txt"),
            "read-only fixture",
        )
        .unwrap();
        let sess = start(
            &env,
            &[
                bash("one", &format!("{prefix}; cat < one.fifo")),
                bash("two", "cat two.fifo"),
            ],
            "10",
        );
        let first = writer(&env, "one.fifo", &sess);
        let second = writer(&env, "two.fifo", &sess);
        release(first, "READONLY_FIRST");
        release(second, "READONLY_SECOND");
        done(&sess);
        finish(sess);
    }
}

#[test]
fn permission_and_question_share_input_while_independent_reads_continue() {
    let env = TestEnv::new_mock("parallel-prompt-queue");
    let dir = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        env.workspace_root().join("protected.txt"),
        "APPROVED_READ_RESULT",
    )
    .unwrap();
    std::fs::write(
        env.workspace_root().join("free.txt"),
        "INDEPENDENT_READ_RESULT",
    )
    .unwrap();
    std::fs::write(env.workspace_root().join("ask.py"), r#"import json,os
p=json.loads(os.environ['HOOK_TOOL_INPUT'])
if p.get('command')=='cat protected.txt':
    print(json.dumps({'hookSpecificOutput':{'permissionDecision':'ask','permissionDecisionReason':'Approve the protected fixture'}}))
"#).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        json!({"hooks":{"PreToolUse":["python3 ask.py"]}}).to_string(),
    )
    .unwrap();
    let mut sess = start(
        &env,
        &[
            json!({"id":"question", "name":"AskUserQuestion", "input":{"question":"QUEUE_FIRST_QUESTION"}}),
            bash("protected", "cat protected.txt"),
            bash("free", "cat free.txt"),
        ],
        "10",
    );
    // Asynchronous hooks may make either prompt ready first. The shared
    // input queue serializes arrival, not the provider's tool declaration order.
    let screen = common::expect_screen(
        &sess,
        |s| {
            s.contains("INDEPENDENT_READ_RESULT")
                && (s.contains("1/1  QUEUE_FIRST_QUESTION")
                    || s.contains("1/1  Allow this tool call?"))
        },
        common::DEFAULT_TIMEOUT,
        "independent read while one prompt owns input",
    );
    let permission_first = screen.contains("1/1  Allow this tool call?");
    assert!(
        !(screen.contains("1/1  Allow this tool call?")
            && screen.contains("1/1  QUEUE_FIRST_QUESTION")),
        "two input owners"
    );
    sess.send(if permission_first {
        "1\r"
    } else {
        "answered\r"
    })
    .unwrap();
    common::expect_screen(
        &sess,
        |s| {
            if permission_first {
                s.contains("1/1  QUEUE_FIRST_QUESTION")
            } else {
                s.contains("1/1  Allow this tool call?")
            }
        },
        common::DEFAULT_TIMEOUT,
        "second queued prompt owns input",
    );
    sess.send(if permission_first {
        "answered\r"
    } else {
        "1\r"
    })
    .unwrap();
    done(&sess);
    assert!(sess
        .render(|s| s.contents())
        .contains("APPROVED_READ_RESULT"));
    assert_result_order(&env, &["question", "protected", "free"]);
    finish(sess);
}

#[test]
fn cancelling_a_question_preserves_sibling_result_and_releases_input() {
    let env = TestEnv::new_mock("parallel-question-cancel");
    std::fs::write(
        env.workspace_root().join("read.txt"),
        "READ_SURVIVES_QUESTION_CANCEL",
    )
    .unwrap();
    let mut sess = start(
        &env,
        &[
            json!({"id":"question", "name":"AskUserQuestion", "input":{"question":"CANCEL_THIS_QUESTION"}}),
            bash("read", "cat read.txt"),
        ],
        "10",
    );
    common::expect_screen(
        &sess,
        |s| s.contains("1/1  CANCEL_THIS_QUESTION") && s.contains("READ_SURVIVES_QUESTION_CANCEL"),
        common::DEFAULT_TIMEOUT,
        "read completes before question cancellation",
    );
    sess.send("\x1b").unwrap();
    common::expect_screen(
        &sess,
        |s| s.to_lowercase().contains("cancelled") && !s.contains("1/1  CANCEL_THIS_QUESTION"),
        common::DEFAULT_TIMEOUT,
        "cancel removes question panel",
    );
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "released question input");
    send_batch(
        &env,
        &mut sess,
        &[
            json!({"id":"fresh", "name":"AskUserQuestion", "input":{"question":"NEW_TURN_QUESTION"}}),
        ],
    );
    common::expect_screen(
        &sess,
        |s| s.contains("1/1  NEW_TURN_QUESTION"),
        common::DEFAULT_TIMEOUT,
        "next turn owns a fresh question",
    );
    sess.send("fresh-answer\r").unwrap();
    done(&sess);
    assert_result_order(&env, &["question", "read", "fresh"]);
    let bodies = env.captured_message_bodies();
    let request: Value = serde_json::from_str(bodies.last().unwrap()).unwrap();
    let read = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .find(|b| b["tool_use_id"] == "read")
        .unwrap();
    assert!(read.to_string().contains("READ_SURVIVES_QUESTION_CANCEL"));
    assert_ne!(read["is_error"], true);
    let cancelled = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .find(|b| b["tool_use_id"] == "question")
        .unwrap();
    assert_eq!(
        cancelled["is_error"], true,
        "dismissal must not become an answered empty question"
    );
    finish(sess);
}

#[test]
fn streamed_calls_and_results_survive_resume_before_message_stop() {
    let env = TestEnv::new_mock("parallel-stream-resume");
    fifo(&env, "pending.fifo");
    let gate = env.workspace_root().join("resume-stream");
    let mut pending = bash("pending", "cat pending.fifo");
    pending["stream_wait_for"] = json!(gate);
    let mut sess = start(
        &env,
        &[
            bash("once", "printf 'DURABLE_ONCE\n' >> once.txt; cat once.txt"),
            pending,
        ],
        "10",
    );
    let held = writer(&env, "pending.fifo", &sess);
    // Read the real append log while the provider is still open. Copy it
    // before graceful cancellation can repair or snapshot the transcript.
    let mut directories = vec![env.workspace_root().join(".scode/sessions")];
    let mut transcript = None;
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name == "transcript.jsonl")
            {
                transcript = Some(path);
            }
        }
    }
    let saved = env.workspace_root().join("interrupted.jsonl");
    std::fs::copy(transcript.expect("active transcript"), &saved).unwrap();
    assert!(std::fs::read_to_string(&saved)
        .unwrap()
        .contains("assistant_update"));
    sess.send("\x1b").unwrap();
    common::expect_screen(
        &sess,
        |s| s.to_lowercase().contains("cancelled"),
        common::DEFAULT_TIMEOUT,
        "cancel active session",
    );
    drop(held);
    std::fs::write(gate, "close").unwrap();
    finish(sess);
    let mut resumed = env.spawn(&[
        "--resume",
        saved.to_str().unwrap(),
        "--permission-mode",
        "danger-full-access",
    ]);
    common::expect_screen_settled(
        &resumed,
        |s| s.contains("/permissions to change") && common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "resumed input ready after transcript replay",
    );
    resumed.send("continue").unwrap();
    common::expect_input_line(
        &resumed,
        "continue",
        common::DEFAULT_TIMEOUT,
        "resumed continuation input",
    );
    resumed.send("\r").unwrap();
    done(&resumed);
    assert_result_order(&env, &["once", "pending"]);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("once.txt")).unwrap(),
        "DURABLE_ONCE\n",
        "resume must not replay completed effects"
    );
    let requests = env.captured_message_bodies();
    let request = requests.last().unwrap();
    assert!(
        request.contains("DURABLE_ONCE"),
        "completed result missing after resume"
    );
    finish(resumed);
}

#[test]
fn mutating_or_unknown_command_flags_keep_the_writer_barrier() {
    for prefix in [
        "git branch unexpected-branch",
        "git branch --abbrev 7",
        "fd --exec touch unexpected-file",
        "gh pr create --help",
        "diff --output unexpected-file",
        "git status *.txt",
    ] {
        let env = TestEnv::new_mock("parallel-unsafe-flags");
        fifo(&env, "read.fifo");
        fifo(&env, "after.fifo");
        std::fs::write(env.workspace_root().join("fixture.txt"), "fixture").unwrap();
        let sess = start(
            &env,
            &[
                bash("read", "cat read.fifo"),
                bash("fenced", &format!("{prefix}; cat after.fifo")),
            ],
            "10",
        );
        let read = writer(&env, "read.fifo", &sess);
        common::expect_screen(
            &sess,
            |s| s.lines().filter(|row| row.starts_with("╭─ Bash(")).count() == 2,
            common::DEFAULT_TIMEOUT,
            "both calls admitted to renderer",
        );
        assert_eq!(
            try_writer(env.workspace_root(), "after.fifo")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENXIO),
            "unsafe flags crossed pending read: {prefix}"
        );
        release(read, "READ_BEFORE_MUTATION");
        release(writer(&env, "after.fifo", &sess), "AFTER_MUTATION");
        done(&sess);
        finish(sess);
    }
}

#[test]
fn cancellation_during_post_hook_preserves_the_finished_command_output() {
    let env = TestEnv::new_mock("parallel-post-hook-cancel");
    let dir = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        env.workspace_root().join("result.txt"),
        "FINISHED_BEFORE_POST_HOOK_CANCEL",
    )
    .unwrap();
    std::fs::write(env.workspace_root().join("post.py"), "from pathlib import Path\nimport time\nPath('post.started').write_text('started')\ntime.sleep(30)\n").unwrap();
    std::fs::write(
        dir.join("settings.json"),
        json!({"hooks":{"PostToolUse":["python3 post.py"]}}).to_string(),
    )
    .unwrap();
    let mut sess = start(&env, &[bash("read", "cat result.txt")], "10");
    wait_file(&env, "post.started");
    sess.send("\x1b").unwrap();
    common::expect_screen(
        &sess,
        |s| {
            s.to_lowercase().contains("cancelled") && s.contains("FINISHED_BEFORE_POST_HOOK_CANCEL")
        },
        common::DEFAULT_TIMEOUT,
        "command output survives post-hook cancellation",
    );
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "input after post hook");
    sess.send("continue\r").unwrap();
    done(&sess);
    assert_result_order(&env, &["read"]);
    let requests = env.captured_message_bodies();
    let request: Value = serde_json::from_str(requests.last().unwrap()).unwrap();
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .find(|b| b["tool_use_id"] == "read")
        .unwrap();
    assert!(result
        .to_string()
        .contains("FINISHED_BEFORE_POST_HOOK_CANCEL"));
    finish(sess);
}
