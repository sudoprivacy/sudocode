//! Host pause/return over ACP in a real terminal, followed by a real shell.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::{
    fs,
    time::{Duration, Instant},
};

fn screen_text(screen: &pty_expect::Screen<'_>) -> String {
    screen.contents()
}

fn request(terminal: &mut PtySession, id: u64, method: &str, params: Value) -> String {
    let newline = if cfg!(windows) { "\r" } else { "\n" };
    let mut message = json!({"jsonrpc":"2.0","id":id,"method":method});
    message["params"] = params;
    terminal
        .send(&format!("{message}{newline}"))
        .expect("send ACP request");
    terminal
        .expect(&format!(r#"(?s)"(?:result|error)":.*?"id":{id}\}}"#))
        .unwrap_or_else(|error| {
            panic!(
                "actual ACP response: {error}; terminal: {}",
                terminal.render(screen_text)
            )
        })
}

struct ReleaseOnDrop(std::path::PathBuf);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release.txt"), "owned cleanup");
    }
}

#[test]
fn host_takeover_stops_a_foreground_shell_before_human_edit() {
    let env = TestEnv::new("pty-tool-pause-active");
    let _release = ReleaseOnDrop(env.workspace_root().to_path_buf());
    fs::write(env.workspace_root().join("main-loop.sh"), "i=0\nwhile [ $i -lt 1200 ] && [ ! -f release.txt ]; do\n printf x >> main-ticks.txt\n i=$((i+1))\n sleep 0.1\ndone\n").unwrap();
    let mut terminal = env.spawn(&["--dangerously-skip-permissions", "acp"]);
    terminal.resize(40, 1000).unwrap();
    terminal.set_default_timeout(Duration::from_secs(45));
    request(&mut terminal, 1, "initialize", json!({"protocolVersion":1}));
    let created = request(
        &mut terminal,
        2,
        "session/new",
        json!({"cwd":env.workspace_root().to_string_lossy(),"mcpServers":[]}),
    );
    let session = created
        .split(r#""sessionId":""#)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let prompt = if env.is_mock() {
        let batch = json!([{"id":"pause-shell","name":"Bash","input":{"command":"sh main-loop.sh","timeout":180_000,"run_in_background":false}}]);
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}")
    } else {
        "Run Bash with exactly command 'sh main-loop.sh', timeout180000 and run_in_background false. This is a disposable acceptance fixture bounded to120 seconds and1200 bytes. The harness pauses the session after it starts and releases it during cleanup. Do not edit the script or create a separate background job.".to_string()
    };
    let newline = if cfg!(windows) { "\r" } else { "\n" };
    terminal.send(&format!("{}{newline}", json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":prompt}]}}))).unwrap();
    let receipt = env.workspace_root().join("main-ticks.txt");
    let deadline = Instant::now() + Duration::from_mins(3);
    while !receipt.exists() {
        assert!(
            Instant::now() < deadline,
            "actual foreground shell never started: {}",
            terminal.render(screen_text)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let paused = request(
        &mut terminal,
        4,
        "_sudocode/session/pause_tools",
        json!({"sessionId":session}),
    );
    assert!(paused.contains(r#""isQuiescent":true"#), "{paused}");
    let epoch = number(&paused, "pauseEpoch");
    let bytes = fs::read(&receipt).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fs::read(&receipt).unwrap(),
        bytes,
        "shell wrote after takeover ACK"
    );
    fs::write(
        env.workspace_root().join("human-note.txt"),
        "HUMAN_AFTER_STOP",
    )
    .unwrap();
    let resumed = request(
        &mut terminal,
        5,
        "_sudocode/session/resume_tools",
        json!({"sessionId":session,"pauseEpoch":epoch}),
    );
    assert!(!resumed.contains(r#""error":"#), "{resumed}");
    request(
        &mut terminal,
        6,
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"! cat human-note.txt > observed-after-return.txt"}]}),
    );
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("observed-after-return.txt")).unwrap(),
        "HUMAN_AFTER_STOP"
    );
    assert_eq!(
        fs::read(&receipt).unwrap(),
        bytes,
        "cancelled shell revived after return"
    );
    request(
        &mut terminal,
        7,
        "session/close",
        json!({"sessionId":session}),
    );
}

fn number(response: &str, key: &str) -> u64 {
    let suffix = response
        .split(&format!(r#""{key}":"#))
        .nth(1)
        .expect("response number");
    suffix
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn host_pause_blocks_tools_until_the_matching_return() {
    let env = TestEnv::new("pty-tool-pause");
    let mut terminal = env.spawn(&["--dangerously-skip-permissions", "acp"]);
    terminal.resize(40, 1000).expect("wide protocol terminal");
    terminal.set_default_timeout(Duration::from_secs(45));
    let initialized = request(&mut terminal, 1, "initialize", json!({"protocolVersion":1}));
    assert!(initialized.contains("toolPause"), "{initialized}");
    let created = request(
        &mut terminal,
        2,
        "session/new",
        json!({"cwd":env.workspace_root().to_string_lossy(),"mcpServers":[]}),
    );
    let session = created
        .split(r#""sessionId":""#)
        .nth(1)
        .expect("created session")
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let paused = request(
        &mut terminal,
        3,
        "_sudocode/session/pause_tools",
        json!({"sessionId":session}),
    );
    assert!(paused.contains(r#""isQuiescent":true"#), "{paused}");
    let epoch = number(&paused, "pauseEpoch");
    let denied = request(
        &mut terminal,
        4,
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"! printf forbidden > forbidden.txt"}]}),
    );
    assert!(denied.contains(r#""error":"#), "{denied}");
    assert!(!env.workspace_root().join("forbidden.txt").exists());
    let stale = request(
        &mut terminal,
        5,
        "_sudocode/session/resume_tools",
        json!({"sessionId":session,"pauseEpoch":epoch+1}),
    );
    assert!(stale.contains(r#""error":"#), "{stale}");
    let resumed = request(
        &mut terminal,
        6,
        "_sudocode/session/resume_tools",
        json!({"sessionId":session,"pauseEpoch":epoch}),
    );
    assert!(!resumed.contains(r#""error":"#), "{resumed}");
    let completed = request(
        &mut terminal,
        7,
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"! printf actual-returned-shell > returned.txt"}]}),
    );
    assert!(!completed.contains(r#""error":"#), "{completed}");
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("returned.txt")).expect("actual shell output"),
        "actual-returned-shell"
    );
    request(
        &mut terminal,
        8,
        "session/close",
        json!({"sessionId":session}),
    );
}
