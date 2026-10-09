//! Child Bash approval on the parent terminal -> fresh invoice -> follow-up.
//! Run with SCODE_TEST_BACKEND=live and the normal live provider configuration.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use serde_json::Value;

const BUDGET: Duration = Duration::from_secs(120);

fn requests(directory: &Path) -> Vec<Value> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| wire_requests::is_inference_request_dump(path))
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect()
}

#[test]
fn permission_prompt_from_subagent_bubbles_to_parent_terminal() {
    child_approval_workflow(false);
}

#[test]
fn child_report_verification_keeps_one_approved_invoice_execution() {
    child_approval_workflow(true);
}

fn child_approval_workflow(verify_report: bool) {
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
    let mut prompt = format!("Call one Bash Agent with model={model}, run_in_background=false, and this prompt: Run exactly this Bash command once, with no other commands or retries: {command}. Report its actual output. Wait for the child and report its output.");
    if verify_report {
        prompt.push_str(" After it completes, have a second read-only Agent read the completed child's saved Markdown report and return the actual invoice output. It must only read that report, without running any commands. Report that verified output.");
    }
    let marker = common::turn_status_marker(&cli);
    cli.send(&prompt).unwrap();
    common::expect_screen(
        &cli,
        |s| {
            s.contains(if verify_report {
                "Report that verified output."
            } else {
                "report its output."
            })
        },
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
    let mut report_reads = BTreeSet::new();
    let mut fixture_reads = BTreeMap::new();
    let mut results = BTreeMap::new();
    for request in requests(&captured) {
        assert_eq!(
            request["model"], model,
            "parent or child used a different model"
        );
        for block in wire_requests::request_tool_blocks(&request) {
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
                    Some("Read" | "read_file") => {
                        // A parent may inspect its completed child's report through
                        // a read-only agent, or inspect the supplied script and data.
                        // None of those reads is another invoice execution.
                        let path = block["input"]["path"]
                            .as_str()
                            .or_else(|| block["input"]["file_path"].as_str())
                            .expect("report read must name a file");
                        let path = Path::new(path);
                        let path = if path.is_absolute() {
                            path.to_path_buf()
                        } else {
                            env.workspace_root().join(path)
                        };
                        let actual = path.canonicalize().expect("report file must exist");
                        let reports = env
                            .workspace_root()
                            .join(".sudocode-agents")
                            .canonicalize()
                            .unwrap();
                        let is_report = actual.parent() == Some(reports.as_path())
                            && actual.extension().and_then(|ext| ext.to_str()) == Some("md");
                        let is_fixture =
                            ["invoice.py", "order.csv", "result.txt"]
                                .iter()
                                .any(|name| {
                                    env.workspace_root().join(name).canonicalize().unwrap()
                                        == actual
                                });
                        assert!(
                            is_report || is_fixture,
                            "child read outside the invoice fixture"
                        );
                        let contents = std::fs::read_to_string(actual).unwrap();
                        if is_report {
                            assert!(
                                contents.contains(&nonce)
                                    && contents.contains(&subtotal.to_string())
                            );
                            report_reads.insert(block["id"].to_string());
                        }
                        fixture_reads.insert(block["id"].to_string(), contents);
                    }
                    other => panic!("unexpected tool in child approval workflow: {other:?}"),
                }
            }
            if block["type"] == "tool_result" {
                assert_ne!(
                    block["is_error"], true,
                    "workflow recovered from a failed tool"
                );
                results.insert(block["tool_use_id"].to_string(), block["content"].clone());
            }
        }
    }
    assert!(!agents.is_empty(), "the parent omitted its child");
    assert_eq!(
        child_commands.len(),
        1,
        "the child retried or omitted the invoice command"
    );
    if verify_report {
        assert!(
            agents.len() >= 2,
            "the report verification child was omitted"
        );
        assert!(!report_reads.is_empty(), "no child report was read");
    }
    for id in &agents {
        let result: Value = serde_json::from_str(
            results
                .get(id)
                .expect("every child must complete")
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["status"], "completed",
            "child did not finish successfully"
        );
        assert_eq!(result["model"], model, "child used a different model");
    }
    assert!(
        agents.iter().any(|id| {
            let output = results[id].to_string();
            output.contains(&nonce) && output.contains(&subtotal.to_string())
        }),
        "no completed child returned the actual invoice result"
    );
    for id in child_commands.iter().chain(&report_reads) {
        let output = results.get(id).expect("every observed tool must complete");
        let output = output.to_string();
        assert!(
            output.contains(&nonce) && output.contains(&subtotal.to_string()),
            "a tool did not return the actual child result"
        );
    }
    for (id, contents) in fixture_reads {
        let result: Value = serde_json::from_str(
            results
                .get(&id)
                .expect("every read must complete")
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["file"]["content"]
                .as_str()
                .unwrap()
                .replace("\r\n", "\n")
                .trim_end(),
            contents.replace("\r\n", "\n").trim_end(),
            "read did not return the actual fixture contents"
        );
    }
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    eprintln!(
        "LIVE CHILD APPROVAL PASS: fresh file, terminal Bash approval and follow-up total verified; {} agents, {} invoice command, {} report reads",
        agents.len(), child_commands.len(), report_reads.len()
    );
}
