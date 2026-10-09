//! Child Bash approval on the parent terminal -> fresh invoice -> follow-up.
//! Run with SCODE_TEST_BACKEND=live and the normal live provider configuration.
mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use serde_json::Value;

const BUDGET: Duration = Duration::from_secs(120);

fn requests(directory: &Path) -> Vec<Value> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| common::is_inference_request_dump(path))
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect()
}

#[test]
fn permission_prompt_from_subagent_bubbles_to_parent_terminal() {
    let env = TestEnv::new("pty-agent-perm-bubble");
    if env.is_mock() {
        eprintln!("SKIP: this workflow requires real parent and child model requests");
        return;
    }
    let python = common::resolve_python()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let nonce = format!("BUBBLE_{stamp}");
    let units = 9 + stamp % 17;
    let subtotal = units * 29 + 47;
    std::fs::write(
        env.workspace_root().join("order.csv"),
        format!("batch,units,unit_price,delivery\n{nonce},{units},29,47\n"),
    )
    .unwrap();
    std::fs::write(
        env.workspace_root().join("invoice.py"),
        r#"import csv
from pathlib import Path
root = Path(__file__).resolve().parent
with (root / 'order.csv').open() as f:
    order = next(csv.DictReader(f))
subtotal = int(order['units']) * int(order['unit_price']) + int(order['delivery'])
result = order['batch'] + ' ' + str(subtotal)
(root / 'result.txt').write_text(result)
print(result)
"#,
    )
    .unwrap();
    let captured = std::env::var_os("SCODE_LIVE_ARTIFACTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env.workspace_root().join("requests"))
        .join(format!("bubble-{stamp}"));
    std::fs::create_dir_all(&captured).unwrap();
    let model = common::live_model();
    let mut cli = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "Agent",
        ],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "on"),
            ("SUDOCODE_DUMP_REQUESTS", captured.to_str().unwrap()),
        ],
    );
    cli.resize(48, 260).unwrap();
    common::expect_screen(&cli, |s| s.contains("❯"), BUDGET, "interactive prompt");
    let script = env
        .workspace_root()
        .join("invoice.py")
        .to_string_lossy()
        .replace('\\', "/");
    let command = format!("'{python}' '{script}'");
    let command_words = shell_words::split(&command).unwrap();
    let prompt = format!("Call one Bash Agent with model={model}, run_in_background=false, and this prompt: Run exactly this Bash command once, with no other commands or retries: {command}. Report its actual output. Wait for the child and report its output.");
    let marker = common::turn_status_marker(&cli);
    cli.send(&prompt).unwrap();
    common::expect_screen(
        &cli,
        |s| s.contains("report its output."),
        BUDGET,
        "agent task input",
    );
    cli.send("\r").unwrap();
    common::expect_screen(
        &cli,
        |s| {
            (s.contains("Approve Bash?") || s.contains("Approve bash?"))
                && s.contains("invoice.py")
                && s.contains("Allow this tool call?")
        },
        BUDGET,
        "child Bash approval on the parent terminal",
    );
    assert!(
        !env.workspace_root().join("result.txt").exists(),
        "child wrote the result before approval"
    );
    cli.send("1\r").unwrap();
    common::expect_turn_complete_after(&cli, &marker, BUDGET, "approved child completed");
    common::expect_screen(
        &cli,
        |s| s.contains(&nonce) && s.contains(&subtotal.to_string()),
        BUDGET,
        "actual child data in the parent reply",
    );
    let saved = std::fs::read_to_string(env.workspace_root().join("result.txt")).unwrap();
    assert!(
        saved.contains(&nonce) && saved.contains(&subtotal.to_string()),
        "approved child did not persist the actual result: {saved}"
    );
    let marker = common::turn_status_marker(&cli);
    cli.send("Add 13 to that subtotal and reply with the batch code and final total. Use this conversation without tools.\r").unwrap();
    common::expect_turn_complete_after(&cli, &marker, BUDGET, "follow-up completed");
    common::expect_screen(
        &cli,
        |s| s.contains(&nonce) && s.contains(&(subtotal + 13).to_string()),
        BUDGET,
        "child result used in the next turn",
    );
    let mut agents = BTreeSet::new();
    let mut child_commands = BTreeSet::new();
    for request in requests(&captured) {
        assert_eq!(
            request["model"], model,
            "parent or child used a different model"
        );
        for block in common::request_tool_blocks(&request) {
            if block["type"] == "tool_use" {
                match block["name"].as_str() {
                    Some("Agent" | "agent_spawn") => {
                        agents.insert(block["id"].to_string());
                    }
                    Some("Bash" | "bash") => {
                        let actual = block["input"]["command"]
                            .as_str()
                            .expect("child Bash command must be a string");
                        assert_eq!(
                            shell_words::split(actual).expect("valid child shell command"),
                            command_words,
                            "child ran a different invoice command"
                        );
                        child_commands.insert(block["id"].to_string());
                    }
                    _ => {}
                }
            }
            if block["type"] == "tool_result" {
                assert_ne!(
                    block["is_error"], true,
                    "workflow recovered from a failed tool"
                );
            }
        }
    }
    assert_eq!(agents.len(), 1, "the parent retried or omitted its child");
    assert_eq!(
        child_commands.len(),
        1,
        "the child retried or omitted the invoice command"
    );
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    eprintln!(
        "LIVE CHILD APPROVAL PASS: fresh file, terminal Bash approval and follow-up total verified"
    );
}
