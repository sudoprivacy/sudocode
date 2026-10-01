//! Live regression: cancelling a foreground tool must stop its descendants.
//! Run with SCODE_TEST_BACKEND=live and Node.js on PATH.
mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

struct DescendantGuard {
    node: String,
    pid: u64,
}

impl Drop for DescendantGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.node)
            .args([
                "-e",
                &format!("try{{process.kill({},'SIGKILL')}}catch{{}}", self.pid),
            ])
            .status();
    }
}

fn wait_for(check: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn is_exited(node: &str, pid: u64) -> bool {
    Command::new(node)
        .args([
            "-e",
            &format!("try{{process.kill({pid},0);process.exit(1)}}catch{{process.exit(0)}}"),
        ])
        .status()
        .expect("check grandchild")
        .success()
}

/// The marker comes from a grandchild, so neither a canned reply nor killing
/// just the shell can satisfy the assertions. The next turn reads its token.
#[test]
fn live_esc_reaps_bash_descendants_and_next_turn_works() {
    run_bash_tree_workflow(true);
}

#[test]
fn live_timeout_reaps_bash_descendants_and_next_turn_works() {
    run_bash_tree_workflow(false);
}

fn write_fixture(root: &Path) -> (String, String) {
    let node = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("Node.js is required for the live descendant fixture");
    assert!(node.status.success());
    let node = String::from_utf8(node.stdout).expect("node path UTF-8");
    let node = node.trim().replace('\\', "/");
    let token = root
        .file_name()
        .expect("unique workspace")
        .to_string_lossy()
        .into_owned();
    let token_json = serde_json::to_string(&token).expect("token JSON");
    fs::write(
        root.join("descendant.cjs"),
        format!("const fs=require('fs'); fs.writeFileSync('started.json', JSON.stringify({{pid:process.pid,token:{token_json}}})); setInterval(()=>{{}},1000);"),
    )
    .expect("write descendant");
    fs::write(
        root.join("parent.cjs"),
        "require('child_process').spawn(process.execPath,['descendant.cjs'],{stdio:'inherit'});",
    )
    .expect("write parent");
    (node, token)
}

fn spawn_session(env: &common::TestEnv) -> pty_expect::PtySession {
    let mut paths: Vec<_> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if cfg!(windows) {
        let shell = common::resolve_sh();
        paths.insert(
            0,
            Path::new(&shell)
                .parent()
                .expect("Git Bash directory")
                .to_path_buf(),
        );
    }
    let tool_path = std::env::join_paths(paths)
        .expect("tool PATH")
        .to_string_lossy()
        .into_owned();
    env.spawn_with_env(
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
        ],
        &[("PATH", &tool_path)],
    )
}

fn run_bash_tree_workflow(is_cancel: bool) {
    let env = common::TestEnv::new("bash-tree-cancel");
    if !env.is_live() {
        eprintln!(
            "SKIP: requires SCODE_TEST_BACKEND=live; a mock cannot prove model/tool cancellation"
        );
        return;
    }
    let root = env.workspace_root();
    let (node, token) = write_fixture(root);
    let command = format!("\"{node}\" parent.cjs");
    let timeout_ms = if is_cancel { 120_000 } else { 5_000 };
    let prompt = format!("Use the bash tool to run exactly: {command}. Run once in the foreground with timeout={timeout_ms}. Do not retry, read or edit either script.");
    let mut session = spawn_session(&env);
    session.set_default_timeout(Duration::from_secs(90));
    session.expect("❯").expect("initial prompt");
    session.send(&format!("{prompt}\r")).expect("start tool");
    let started = root.join("started.json");
    assert!(
        wait_for(|| started.exists(), Duration::from_secs(90)),
        "real grandchild did not start: {}",
        session.render(|screen| screen.contents())
    );
    let marker: serde_json::Value =
        serde_json::from_slice(&fs::read(&started).expect("marker")).expect("marker JSON");
    let pid = marker["pid"].as_u64().expect("grandchild PID");
    let _descendant = DescendantGuard {
        node: node.clone(),
        pid,
    };
    if is_cancel {
        session.send("\x1b").expect("ESC during tool execution");
        session
            .expect("(?i)(cancelled|interrupted)")
            .expect("visible cancellation");
    } else {
        session
            .expect("(?i)(exceeded|timed out)")
            .expect("visible timeout");
    }

    // Use Node for a portable process-liveness check (works with Windows PIDs).
    assert!(
        wait_for(|| is_exited(&node, pid), Duration::from_secs(10)),
        "cancelled descendant still alive: {}",
        session.render(|screen| screen.contents())
    );

    common::expect_input_line_cleared(
        &session,
        Duration::from_secs(90),
        "ready after cancellation",
    );
    let marker = common::turn_status_marker(&session);
    let follow_up = "Use bash to read started.json, then write its token to recovered.txt. Do the actual file operations.";
    session
        .send(&format!("{follow_up}\r"))
        .expect("follow-up turn");
    assert!(
        wait_for(
            || {
                fs::read_to_string(root.join("recovered.txt"))
                    .is_ok_and(|text| text.trim() == token)
            },
            Duration::from_secs(90),
        ),
        "next turn did not write the recovered token: {}",
        session.render(|screen| screen.contents())
    );
    common::expect_turn_complete_after(
        &session,
        &marker,
        Duration::from_secs(90),
        "recovery turn completed",
    );
    common::expect_input_line_cleared(&session, Duration::from_secs(90), "ready to exit");
    session.send("/exit").expect("type exit");
    common::expect_input_line(
        &session,
        "/exit",
        Duration::from_secs(10),
        "exit command landed",
    );
    session.send("\r").expect("submit exit");
    assert_eq!(session.expect_eof().expect("clean exit"), 0);
}
