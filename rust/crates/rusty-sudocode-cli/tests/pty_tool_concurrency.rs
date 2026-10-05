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
    sess
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

#[test]
fn parallel_bash_cards_survive_narrow_and_wide_resize() {
    let env = TestEnv::new("parallel-resize");
    fifo(&env, "one.fifo");
    fifo(&env, "two.fifo");
    let calls = [
        bash(
            "one",
            "cat one.fifo # 一起验证 long title keeps the complete command and resizes_END_ONE",
        ),
        bash(
            "two",
            "cat two.fifo # 一起验证 long title keeps the complete command and resizes_END_TWO",
        ),
    ];
    let mut sess = start(&env, &calls, "10");
    // Both FIFO opens must succeed before either is released. A serial
    // scheduler cannot pass this handshake.
    let first = writer(&env, "one.fifo", &sess);
    let second = writer(&env, "two.fifo", &sess);
    for width in [52, 170, 66, 100] {
        sess.resize(80, width).unwrap();
        common::expect_screen(
            &sess,
            |s| {
                let rows: Vec<_> = s.lines().collect();
                let starts: Vec<_> = rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row.starts_with("╭─ Bash("))
                    .map(|(i, _)| i)
                    .collect();
                starts.len() == 2
                    && rows.iter().filter(|r| r.trim() == "╰─").count() == 2
                    && starts.iter().all(|&row| rows.get(row + 1).is_some_and(|next| next.trim() == "╰─"))
                    // A resized VT screen may briefly contain the old canvas
                    // clipped at its new width. Wait for freshly laid-out
                    // titles, not text in the echoed user prompt.
                    && starts.iter().all(|&row| if width < 100 { rows[row].ends_with('…') } else { rows[row].contains("resizes_END_") })
            },
            common::DEFAULT_TIMEOUT,
            "two whole pending frames laid out at the new width",
        );
        sess.send("x").unwrap();
        common::expect_input_line(
            &sess,
            "x",
            common::DEFAULT_TIMEOUT,
            "input while two tools run",
        );
        sess.send("\x15").unwrap();
        common::expect_screen(
            &sess,
            |s| common::input_line_of(s).is_empty(),
            common::DEFAULT_TIMEOUT,
            "draft cleared before next resize",
        );
    }
    release(first, "FIRST_OK");
    release(second, "SECOND_OK");
    done(&sess);
    finish(sess);
}

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
            json!({"id":"read", "name":"Read", "input":{"file_path":"result.txt"}}),
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
