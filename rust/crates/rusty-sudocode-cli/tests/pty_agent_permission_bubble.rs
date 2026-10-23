//! Child Bash approval on the parent terminal -> fresh invoice -> follow-up.
//! Run with `SCODE_TEST_BACKEND=live` and the normal live provider configuration.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::Value;

const BUDGET: Duration = common::LIVE_TURN_BUDGET;
const INVOICE_AGENT: &str = "invoice-bash-fixture";
const INVOICE_SYSTEM_MARKER: &str = "InvoiceBashFixture";
const REPORT_AGENT: &str = "invoice-report-fixture";
const REPORT_SYSTEM_MARKER: &str = "InvoiceReportFixture";

/// Keep execution and report agents on the parent route. Authentication is a
/// fixture input, not a model-selected part of this permission/report journey.
fn agent_input(model: &str, agent: &str, prompt: &str) -> Value {
    serde_json::json!({
        "agent": agent,
        "model": model,
        "auth_mode": null,
        "run_in_background": false,
        "prompt": prompt,
    })
}

fn requests(directory: &Path) -> Vec<Value> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| wire_requests::is_inference_request_dump(path))
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect()
}

fn submit_agent_request(cli: &mut PtySession, prompt: &str) {
    cli.send(prompt).unwrap();
    common::expect_screen(
        cli,
        |screen| common::screen_contains(screen, prompt),
        BUDGET,
        "complete agent task input",
    );
    cli.send("\r").unwrap();
}

fn completed_child_report(directory: &Path, workspace: &Path) -> String {
    let mut reports = BTreeSet::new();
    for request in requests(directory) {
        for block in wire_requests::request_tool_blocks(&request) {
            if block["type"] != "tool_result" {
                continue;
            }
            let Some(result) = block["content"]
                .as_str()
                .and_then(|content| serde_json::from_str::<Value>(content).ok())
            else {
                continue;
            };
            if result["status"] == "completed" {
                assert_ne!(block["is_error"], true);
                reports.insert(
                    result["outputFile"]
                        .as_str()
                        .expect("completed Agent result must name its report")
                        .to_string(),
                );
            }
        }
    }
    assert_eq!(reports.len(), 1, "exactly one invoice child must complete");
    let report = reports.pop_first().unwrap();
    let actual = Path::new(&report).canonicalize().unwrap();
    let root = workspace.canonicalize().unwrap();
    let relative = actual
        .strip_prefix(&root)
        .expect("completed report must belong to the fixture workspace");
    assert_eq!(relative.parent(), Some(Path::new(".sudocode-agents")));
    // The returned absolute path is the producer's tool contract. Preserve it.
    report
}

fn offered_tools(request: &Value) -> BTreeSet<String> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().or(tool["function"]["name"].as_str()))
        .map(tools::canonicalize_tool_name)
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
        r"import csv
from pathlib import Path
root = Path(__file__).resolve().parent
with (root / 'order.csv').open() as f:
    order = next(csv.DictReader(f))
subtotal = int(order['units']) * int(order['unit_price']) + int(order['delivery'])
result = order['batch'] + ' ' + str(subtotal)
(root / 'result.txt').write_text(result)
print(result)
",
    )
    .unwrap();
    // Constrain the execution fixture through the same custom-agent tool pool
    // the product uses. Windows models may otherwise choose PowerShell while
    // this journey deliberately verifies a Bash command and its approval.
    let agents_dir = env.workspace_root().join(".sudocode/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{INVOICE_AGENT}.md")),
        format!(
            "---\nname: {INVOICE_AGENT}\ndescription: Execute one approved invoice Bash command.\ntools: [bash]\n---\n{INVOICE_SYSTEM_MARKER}: run the supplied Bash command once and report its actual output.\n"
        ),
    )
    .unwrap();
    if verify_report {
        std::fs::write(
            agents_dir.join(format!("{REPORT_AGENT}.md")),
            format!(
                "---\nname: {REPORT_AGENT}\ndescription: Read a completed invoice report.\ntools: [read_file]\n---\n{REPORT_SYSTEM_MARKER}: read exactly the supplied report file and return its contents.\n"
            ),
        )
        .unwrap();
    }
    let captured = std::env::var_os("SCODE_LIVE_ARTIFACTS")
        .map_or_else(
            || env.workspace_root().join("requests"),
            std::path::PathBuf::from,
        )
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
    // Parent and child share the workspace; fixture commands use that namespace.
    let command = format!("'{python}' 'invoice.py'");
    let command_words = shell_words::split(&command).unwrap();
    let bash_input = serde_json::json!({ "command": command });
    let child_prompt = format!("Call the bash tool exactly once with this input object: {bash_input}. Run no other commands or retries. Report its actual output.");
    let execution = agent_input(&model, INVOICE_AGENT, &child_prompt);
    let prompt = format!("Call the Agent tool with exactly this input object, without adding or overriding fields: {execution}. Wait for the child and report its actual output.");
    let marker = common::turn_status_marker(&cli);
    submit_agent_request(&mut cli, &prompt);
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
    let saved =
        std::fs::read_to_string(env.workspace_root().join("result.txt")).unwrap_or_else(|error| {
            panic!(
                "approved child did not persist its result: {error}\n{}",
                common::screen_tail(&cli, 6000)
            )
        });
    assert!(
        saved.contains(&nonce) && saved.contains(&subtotal.to_string()),
        "approved child did not persist the actual result: {saved}"
    );
    common::expect_screen(
        &cli,
        |s| s.contains(&nonce) && s.contains(&subtotal.to_string()),
        BUDGET,
        "actual child data in the parent reply",
    );
    if verify_report {
        // Make report verification its own user turn. The actual completed
        // child result owns the report path; no model has to infer a second
        // task or locate a report from a placeholder inside a long request.
        let report = completed_child_report(&captured, env.workspace_root());
        let read_input = serde_json::json!({ "path": report });
        let verification = agent_input(
            &model,
            REPORT_AGENT,
            &format!("Call read_file exactly once with this input object: {read_input}. Return the actual invoice output. Only read this report: do not run commands or create another report."),
        );
        let prompt = format!("Call the Agent tool with exactly this input object, without adding or overriding fields: {verification}. Wait for this report verification child and report its actual output.");
        let marker = common::turn_status_marker(&cli);
        submit_agent_request(&mut cli, &prompt);
        common::expect_turn_complete_after(&cli, &marker, BUDGET, "report verification completed");
    }
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
    let mut restricted_child_requests = 0;
    let mut report_child_requests = 0;
    for request in requests(&captured) {
        assert_eq!(
            request["model"], model,
            "parent or child used a different model"
        );
        let mut system = request["system"].to_string();
        for message in request["messages"].as_array().into_iter().flatten() {
            if message["role"] == "system" {
                system.push_str(&message["content"].to_string());
            }
        }
        if system.contains(INVOICE_SYSTEM_MARKER) {
            assert_eq!(
                offered_tools(&request),
                BTreeSet::from(["bash".to_string()])
            );
            restricted_child_requests += 1;
        }
        if system.contains(REPORT_SYSTEM_MARKER) {
            assert_eq!(
                offered_tools(&request),
                BTreeSet::from(["read_file".to_string()]),
            );
            report_child_requests += 1;
        }
        for block in wire_requests::request_tool_blocks(&request) {
            if block["type"] == "tool_use" {
                let name = tools::canonicalize_tool_name(block["name"].as_str().unwrap());
                match name.as_str() {
                    "agent_spawn" => {
                        assert_eq!(block["input"]["model"], model, "child model override");
                        assert!(
                            block["input"]["auth_mode"].is_null(),
                            "child must inherit the parent credential path: {}",
                            block["input"]
                        );
                        assert_eq!(block["input"]["run_in_background"], false);
                        agents.insert(block["id"].to_string());
                    }
                    "bash" => {
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
                    "read_file" => {
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
                        let actual = path.canonicalize().unwrap_or_else(|error| {
                            panic!("observed read path {path:?} must exist in the fixture: {error}")
                        });
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
    assert!(
        restricted_child_requests > 0,
        "invoice child did not use its fixture tool pool"
    );
    assert!(!agents.is_empty(), "the parent omitted its child");
    assert_eq!(
        child_commands.len(),
        1,
        "the child retried or omitted the invoice command"
    );
    if verify_report {
        assert!(
            report_child_requests > 0,
            "report child did not use its fixture tool pool"
        );
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
