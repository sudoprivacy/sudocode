//! Resizing inline chrome must not commit old frames into terminal history.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session, TokenUsage};

#[test]
#[ignore = "requires the pinned real terminal host; CI runs this test explicitly"]
fn resize_keeps_one_status_and_todo_without_erasing_history_or_input() {
    let env = TestEnv::new("chrome-resize");
    let store = env.workspace_root().join("todos.json");
    std::fs::write(
        &store,
        r#"[{"content":"ResizeCompletedTask","activeForm":"Checking resize","status":"completed"}]"#,
    )
    .unwrap();
    let mut saved = Session::new().with_workspace_root(env.workspace_root());
    saved.push_user_text("Saved resize conversation").unwrap();
    let mut body = (0..70)
        .map(|i| format!("Earlier history line {i}\n"))
        .collect::<String>();
    body.push_str("ResizeHistorySentinel");
    let mut message = ConversationMessage::assistant(vec![ContentBlock::Text { text: body }]);
    message.usage = Some(TokenUsage {
        cache_read_input_tokens: 405_405,
        cache_creation_input_tokens: 4_095,
        output_tokens: 900,
        ..TokenUsage::default()
    });
    message.duration_ms = Some(61_000);
    saved.push_message(message).unwrap();
    let path = env.workspace_root().join("resize-session.jsonl");
    saved.save_to_path(&path).unwrap();
    let output = run_terminal(
        &env,
        &[
            "--resume",
            path.to_str().unwrap(),
            "--permission-mode",
            "read-only",
        ],
        "history",
        serde_json::json!({"todos": store}),
    );
    assert_terminal_output(output);
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            0,
            "resize must not run a turn"
        );
    }
}

#[test]
#[ignore = "requires the pinned real terminal host; CI runs this test explicitly"]
fn queued_peer_preview_resizes_and_flushes_its_complete_body_once() {
    use runtime::agent_mailbox::{self, MailboxEnvelope};
    use runtime::mailbox::{local_pair_root_in, Mailbox};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let env = TestEnv::new("a2a-real-resize");
    let first = format!("A2A-BUSY-MARKER {}", "preview ".repeat(12));
    let body = format!(
        "{first}\n\nQUEUED-BODY-END\n\n{}",
        env.prompt(
            "Reply only with A2A-ACK. Do not call tools.",
            "single_turn_text"
        )
    );
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: printf 'ready' > cancel-ready; printf 'interrupt-start'; sleep 30; printf 'interrupt-done'",
        "bash_interrupt_long_running",
    );
    let pair = local_pair_root_in(env.config_home());
    let ready = env.workspace_root().join("cancel-ready");
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let peer = std::thread::spawn(move || -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline && !stopped.load(Ordering::Relaxed) {
            if ready.is_file() {
                let receiver = std::fs::read_dir(pair.join("agents"))
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| path.join("conversations").is_dir());
                if let Some(receiver) = receiver {
                    return Mailbox::workspace_local(&pair, "mac-ai".into()).send(
                        MailboxEnvelope {
                            from: "mac-ai".into(),
                            to: receiver.file_name().unwrap().to_string_lossy().into_owned(),
                            body,
                            summary: Some("queued resize acceptance".into()),
                            timestamp: 0,
                            color: None,
                            kind: agent_mailbox::kinds::MESSAGE.into(),
                            request_id: None,
                        },
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err("real Bash readiness and mailbox receiver were not observed".into())
    });
    let output = run_terminal(
        &env,
        &["--permission-mode", "danger-full-access"],
        "a2a",
        serde_json::json!({
            "prompt": prompt,
            "firstLine": first.trim_end(),
            "expectedReply": if env.is_live() { "A2A-ACK" } else { "The answer is 4" },
        }),
    );
    done.store(true, Ordering::Relaxed);
    let delivered = peer.join().expect("mailbox producer");
    assert_terminal_output(output);
    delivered.expect("real peer delivery");
    if env.is_mock() {
        assert!(
            env.captured_message_count() >= 2,
            "both turns must reach the provider"
        );
    }
}

#[cfg(unix)]
#[test]
#[ignore = "requires the pinned real terminal host; CI runs this test explicitly"]
fn parallel_bash_cards_survive_narrow_and_wide_resize() {
    use nix::{libc, sys::stat::Mode, unistd::mkfifo};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let env = TestEnv::new("parallel-real-resize");
    for name in ["one.fifo", "two.fifo"] {
        mkfifo(
            &env.workspace_root().join(name),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .unwrap();
    }
    let calls = serde_json::json!([
        {"id": "one", "name": "Bash", "input": {"command":
            "cat one.fifo # 一起验证 long title keeps the complete command and resizes_END_ONE"}},
        {"id": "two", "name": "Bash", "input": {"command":
            "cat two.fifo # 一起验证 long title keeps the complete command and resizes_END_TWO"}},
    ]);
    let prompt = if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        format!("In one assistant message issue all these tool calls together, using their names and inputs exactly (ids are fixture labels). Do not run extra tools or wait for one result before requesting the next: {calls}. After all results, reply exactly: Concurrency batch done.")
    };
    let root = env.workspace_root().to_owned();
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let producer = std::thread::spawn(move || -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut writers = Vec::new();
        for name in ["one.fifo", "two.fifo"] {
            loop {
                if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    return Err(format!("both FIFO readers did not start: {name}"));
                }
                match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(root.join(name))
                {
                    Ok(file) => {
                        writers.push(file);
                        break;
                    }
                    Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(format!("open {name}: {error}")),
                }
            }
        }
        // A serial scheduler cannot reach this marker. Keep both readers
        // blocked until the real terminal has checked every width and input.
        std::fs::write(root.join("parallel-ready"), "both readers open").unwrap();
        while !root.join("parallel-release").is_file()
            && !stopped.load(Ordering::Relaxed)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let released = root.join("parallel-release").is_file();
        for (mut writer, output) in writers.into_iter().zip(["FIRST_OK", "SECOND_OK"]) {
            writeln!(writer, "{output}").map_err(|error| error.to_string())?;
        }
        if released {
            Ok(())
        } else {
            Err("terminal did not finish resize assertions".into())
        }
    });
    let output = run_terminal(
        &env,
        &["--permission-mode", "danger-full-access"],
        "parallel",
        serde_json::json!({"prompt": prompt}),
    );
    done.store(true, Ordering::Relaxed);
    let released = producer.join().expect("FIFO producer");
    assert_terminal_output(output);
    released.expect("both tools released after terminal assertions");
    if env.is_mock() {
        assert!(
            env.captured_message_count() >= 2,
            "tool results must reach the provider"
        );
    }
}

fn run_terminal(
    env: &TestEnv,
    args: &[&str],
    scenario: &str,
    extra: serde_json::Value,
) -> std::process::Output {
    let auth = if env.is_live() {
        std::env::var("SCODE_LIVE_AUTH_MODE").unwrap_or_else(|_| "proxy".into())
    } else {
        "api-key".into()
    };
    let model = if env.is_live() {
        common::live_model()
    } else {
        "sonnet".into()
    };
    let mut arguments = vec!["--auth", auth.as_str(), "--model", model.as_str()];
    arguments.extend_from_slice(args);
    let log_root = std::env::var_os("SCODE_TERMINAL_LOG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env.workspace_root().join("terminal-logs"))
        .join(scenario);
    std::fs::create_dir_all(&log_root).unwrap();
    let mut config = serde_json::json!({
        "binary": common::scode_bin(), "args": arguments, "scenario": scenario,
        "root": env.workspace_root(), "configHome": env.config_home(),
        "todos": env.workspace_root().join("todos.json"), "logRoot": log_root,
        "backend": std::env::var("SCODE_CONPTY_BACKEND").unwrap_or_else(|_| "bundled".into()),
    });
    config
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let manifest = env.workspace_root().join("terminal.json");
    std::fs::write(&manifest, serde_json::to_vec(&config).unwrap()).unwrap();
    let host = std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into());
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../e2e/terminal-resize/run.cjs");
    std::process::Command::new(host)
        .arg(script)
        .arg(manifest)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .output()
        .expect("start real terminal host; see e2e/terminal-resize/README.md")
}

fn assert_terminal_output(output: std::process::Output) {
    assert!(
        output.status.success(),
        "real terminal resize failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
}
