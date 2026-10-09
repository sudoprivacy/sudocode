//! Live regression: cancelling a foreground tool must stop its descendants.
//! Run with `SCODE_TEST_BACKEND=live` and Node.js on PATH.
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
/// just the shell can satisfy the assertions. The next turn reads its run ID.
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
    let run_id = root
        .file_name()
        .expect("unique workspace")
        .to_string_lossy()
        .into_owned();
    let run_id_json = serde_json::to_string(&run_id).expect("run ID JSON");
    fs::write(
        root.join("descendant.cjs"),
        format!("const fs=require('fs'); fs.writeFileSync('started.json', JSON.stringify({{pid:process.pid,run_id:{run_id_json}}})); setInterval(()=>{{}},1000);"),
    )
    .expect("write descendant");
    fs::write(
        root.join("parent.cjs"),
        "require('child_process').spawn(process.execPath,['descendant.cjs'],{stdio:'inherit'});",
    )
    .expect("write parent");
    fs::write(
        root.join("recover.cjs"),
        "const fs=require('fs'); const marker=JSON.parse(fs.readFileSync('started.json','utf8')); fs.writeFileSync('recovered.txt',marker.run_id);",
    )
    .expect("write recovery");
    (node, run_id)
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
    let (node, run_id) = write_fixture(root);
    let command = format!("\"{node}\" parent.cjs");
    let timeout_ms = if is_cancel { 120_000 } else { 5_000 };
    let prompt = format!("This workspace contains a local process-cancellation regression fixture. parent.cjs starts descendant.cjs, which records its PID and a synthetic run_id in started.json. Use the bash tool to run exactly: {command}. Run once in the foreground with timeout={timeout_ms}. Do not retry, read or edit either script.");
    let mut session = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
    ]);
    session.set_default_timeout(Duration::from_secs(90));
    session.expect("❯").expect("initial prompt");
    let first_marker = common::turn_status_marker(&session);
    session.send(&format!("{prompt}\r")).expect("start tool");
    let started = root.join("started.json");
    assert!(
        wait_for(|| started.exists(), Duration::from_secs(90)),
        "real grandchild did not start: {}",
        common::screen_tail(&session, 4000)
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
        common::screen_tail(&session, 4000)
    );

    if !is_cancel {
        // A tool timeout still lets the model finish its reply. Wait for that
        // turn, otherwise the follow-up can arrive while input is disabled.
        common::expect_turn_complete_after(
            &session,
            &first_marker,
            common::LIVE_TURN_BUDGET,
            "initial tool turn completed",
        );
    }
    common::expect_input_line_cleared(
        &session,
        Duration::from_secs(90),
        "ready after cancellation",
    );
    let marker = common::turn_status_marker(&session);
    // Keep shell syntax out of the model's recovery choice: this acceptance
    // checks that a new real tool call works after cancellation or timeout.
    // The script still reads the fresh grandchild run ID and writes the result.
    let follow_up = format!(
        "Continue the local process-cancellation regression fixture. Use the bash tool to run exactly: \"{node}\" recover.cjs. Run once in the foreground. This reads the synthetic run_id recorded by descendant.cjs in started.json and writes that run ID to recovered.txt. Execute it; do not just describe it."
    );
    session
        .send(&format!("{follow_up}\r"))
        .expect("follow-up turn");
    assert!(
        wait_for(
            || {
                fs::read_to_string(root.join("recovered.txt"))
                    .is_ok_and(|text| text.trim() == run_id)
            },
            Duration::from_secs(90),
        ),
        "next turn did not write the recovered run ID: {}",
        common::screen_tail(&session, 4000)
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
