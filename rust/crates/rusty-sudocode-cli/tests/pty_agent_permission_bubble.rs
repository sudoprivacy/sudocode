//! Child Bash approval on the parent terminal -> fresh invoice -> follow-up.
//! Run with `SCODE_TEST_BACKEND=live` and the normal live provider configuration.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use serde_json::Value;

const BUDGET: Duration = common::LIVE_TURN_BUDGET;
const INVOICE_AGENT: &str = "invoice-bash-fixture";
const INVOICE_SYSTEM_MARKER: &str = "InvoiceBashFixture";

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

fn agent_call_prompt(input: &Value, follow_up: &str) -> String {
    format!("Call the Agent tool with exactly this input object, without adding or overriding fields: {input}. {follow_up}")
}

fn requests(directory: &Path) -> Vec<Value> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| wire_requests::is_inference_request_dump(path))
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
        .collect()
}

fn completed_invoice_report(directory: &Path) -> PathBuf {
    let reports: BTreeSet<_> = requests(directory)
        .iter()
        .flat_map(wire_requests::request_tool_blocks)
        .filter_map(|block| {
            if block["type"] != "tool_result" {
                return None;
            }
            let result: Value = serde_json::from_str(block["content"].as_str()?).ok()?;
            if result["name"] != INVOICE_AGENT || result["status"] != "completed" {
                return None;
            }
            Some(PathBuf::from(result["outputFile"].as_str()?))
        })
        .collect();
    assert_eq!(reports.len(), 1, "one completed invoice report on the wire");
    let report = reports.into_iter().next().unwrap();
    assert!(
        report.is_file(),
        "Agent returned a missing report: {}",
        report.display()
    );
    report
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
    seed_invoice_files(&env, &nonce, units);
    let captured = std::env::var_os("SCODE_LIVE_ARTIFACTS")
        .map_or_else(
            || env.workspace_root().join("requests"),
            std::path::PathBuf::from,
        )
        .join(format!("bubble-{stamp}"));
    std::fs::create_dir_all(&captured).unwrap();
    let model = common::live_model();
    let mut cli = spawn_invoice_parent(&env, &captured);
    let script = env
        .workspace_root()
        .join("invoice.py")
        .to_string_lossy()
        .replace('\\', "/");
    let command = format!("'{python}' '{script}'");
    let command_words = shell_words::split(&command).unwrap();
    let child_prompt = format!("Use the bash tool to run exactly this command once, with no other commands or retries: {command}. Report its actual output.");
    let execution = agent_input(&model, INVOICE_AGENT, &child_prompt);
    let prompt = agent_call_prompt(
        &execution,
        "Wait for the child and report its actual output.",
    );
    exercise_invoice_approval(&env, &mut cli, &prompt);
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
    let expected_report = if verify_report {
        Some(verify_invoice_report(&env, &mut cli, &captured, &model))
    } else {
        None
    };
    let marker = common::turn_status_marker(&cli);
    cli.send("Add 13 to that subtotal and reply with the batch code and final total. Use this conversation without tools.\r").unwrap();
    common::expect_turn_complete_after(&cli, &marker, BUDGET, "follow-up completed");
    common::expect_screen(
        &cli,
        |s| s.contains(&nonce) && s.contains(&(subtotal + 13).to_string()),
        BUDGET,
        "child result used in the next turn",
    );
    let expected = InvoiceExpectation {
        env: &env,
        model: &model,
        command_words: &command_words,
        expected_report: &expected_report,
        nonce: &nonce,
        subtotal,
    };
    let mut evidence = InvoiceEvidence::default();
    for request in requests(&captured) {
        evidence.observe_request(&request, &expected);
    }
    evidence.assert_complete(&expected, verify_report);
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    eprintln!(
        "LIVE CHILD APPROVAL PASS: fresh file, terminal Bash approval and follow-up total verified; {} agents, {} invoice command, {} report reads",
        evidence.agents.len(), evidence.child_commands.len(), evidence.report_reads.len()
    );
}
fn seed_invoice_files(env: &TestEnv, nonce: &str, units: u128) {
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
}

fn spawn_invoice_parent(env: &TestEnv, captured: &Path) -> pty_expect::PtySession {
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
    cli
}

fn verify_invoice_report(
    env: &TestEnv,
    cli: &mut pty_expect::PtySession,
    captured: &Path,
    model: &str,
) -> std::path::PathBuf {
    // Its own turn: asked to chain both Agent calls in one turn, a live
    // model sometimes answered from the first child's result and never
    // made the second call. Whether it chains steps unprompted is not
    // what this workflow is about; reading the saved report without
    // re-running the invoice is.
    // The returned outputFile is the source of truth. Resolve it in the
    // fixture rather than asking the model to substitute a placeholder.
    let report = completed_invoice_report(captured).canonicalize().unwrap();
    let workspace = env.workspace_root().canonicalize().unwrap();
    let relative = report
        .strip_prefix(&workspace)
        .expect("the completed report belongs to this fixture workspace");
    let read = serde_json::json!({
        "path": format!("./{}", relative.to_string_lossy().replace('\\', "/")),
    });
    let verification = agent_input(
            model,
            "Explore",
            &format!("Call read_file exactly once with exactly this input object: {read}. Copy the supplied relative path unchanged, including its leading './' and forward slashes. Return the actual invoice output. Only read this report: do not run commands or create another report."),
        );
    let marker = common::turn_status_marker(cli);
    cli.send(&agent_call_prompt(
        &verification,
        "Wait for the child. Report that verified output.",
    ))
    .unwrap();
    common::expect_screen(
        cli,
        |s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("Report that verified output.")
        },
        BUDGET,
        "report verification input",
    );
    cli.send("\r").unwrap();
    common::expect_turn_complete_after(cli, &marker, BUDGET, "report verification completed");
    report
}

struct InvoiceExpectation<'a> {
    env: &'a TestEnv,
    model: &'a str,
    command_words: &'a [String],
    expected_report: &'a Option<std::path::PathBuf>,
    nonce: &'a str,
    subtotal: u128,
}

#[derive(Default)]
struct InvoiceEvidence {
    agents: BTreeSet<String>,
    child_commands: BTreeSet<String>,
    report_reads: BTreeSet<String>,
    fixture_reads: BTreeMap<String, String>,
    results: BTreeMap<String, Value>,
    restricted_child_requests: usize,
}
impl InvoiceEvidence {
    fn observe_request(&mut self, request: &Value, expected: &InvoiceExpectation<'_>) {
        let model = expected.model;

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
            let offered: BTreeSet<_> = request["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| tool["name"].as_str().or(tool["function"]["name"].as_str()))
                .map(tools::canonicalize_tool_name)
                .collect();
            assert_eq!(offered, BTreeSet::from(["bash".to_string()]));
            self.restricted_child_requests += 1;
        }
        for block in wire_requests::request_tool_blocks(request) {
            self.observe_tool_block(&block, expected);
        }
    }
    fn observe_tool_block(&mut self, block: &Value, expected: &InvoiceExpectation<'_>) {
        let env = expected.env;
        let model = expected.model;
        let command_words = expected.command_words;
        let expected_report = expected.expected_report;
        let nonce = expected.nonce;
        let subtotal = expected.subtotal;

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
                    self.agents.insert(block["id"].to_string());
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
                    self.child_commands.insert(block["id"].to_string());
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
                        panic!("report file {} must exist: {error}", path.display())
                    });
                    let reports = env
                        .workspace_root()
                        .join(".sudocode-agents")
                        .canonicalize()
                        .unwrap();
                    let is_report = actual.parent() == Some(reports.as_path())
                        && actual.extension().and_then(|ext| ext.to_str()) == Some("md");
                    let is_fixture = ["invoice.py", "order.csv", "result.txt"]
                        .iter()
                        .any(|name| {
                            env.workspace_root().join(name).canonicalize().unwrap() == actual
                        });
                    assert!(
                        is_report || is_fixture,
                        "child read outside the invoice fixture"
                    );
                    let contents = std::fs::read_to_string(&actual).unwrap();
                    if is_report {
                        if let Some(expected) = expected_report {
                            assert_eq!(
                                &actual, expected,
                                "the verification child must read the first child's actual report"
                            );
                        }
                        assert!(
                            contents.contains(nonce) && contents.contains(&subtotal.to_string())
                        );
                        self.report_reads.insert(block["id"].to_string());
                    }
                    self.fixture_reads.insert(block["id"].to_string(), contents);
                }
                other => panic!("unexpected tool in child approval workflow: {other:?}"),
            }
        }
        if block["type"] == "tool_result" {
            assert_ne!(
                block["is_error"], true,
                "workflow recovered from a failed tool"
            );
            self.results
                .insert(block["tool_use_id"].to_string(), block["content"].clone());
        }
    }
    fn assert_complete(&self, expected: &InvoiceExpectation<'_>, verify_report: bool) {
        let model = expected.model;
        let nonce = expected.nonce;
        let subtotal = expected.subtotal;
        assert!(
            self.restricted_child_requests > 0,
            "invoice child did not use its fixture tool pool"
        );
        assert!(!self.agents.is_empty(), "the parent omitted its child");
        assert_eq!(
            self.child_commands.len(),
            1,
            "the child retried or omitted the invoice command"
        );
        if verify_report {
            assert!(
                self.agents.len() >= 2,
                "the report verification child was omitted"
            );
            assert!(!self.report_reads.is_empty(), "no child report was read");
        }
        for id in &self.agents {
            let result: Value = serde_json::from_str(
                self.results
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
            self.agents.iter().any(|id| {
                let output = self.results[id].to_string();
                output.contains(nonce) && output.contains(&subtotal.to_string())
            }),
            "no completed child returned the actual invoice result"
        );
        for id in self.child_commands.iter().chain(&self.report_reads) {
            let output = self
                .results
                .get(id)
                .expect("every observed tool must complete");
            let output = output.to_string();
            assert!(
                output.contains(nonce) && output.contains(&subtotal.to_string()),
                "a tool did not return the actual child result"
            );
        }
        for (id, contents) in &self.fixture_reads {
            let result: Value = serde_json::from_str(
                self.results
                    .get(id)
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
    }
}

fn exercise_invoice_approval(env: &TestEnv, cli: &mut pty_expect::PtySession, prompt: &str) {
    let marker = common::turn_status_marker(cli);
    cli.send(prompt).unwrap();
    common::expect_screen(
        cli,
        |s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("report its actual output.")
        },
        BUDGET,
        "agent task input",
    );
    cli.send("\r").unwrap();
    common::expect_screen(
        cli,
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
    common::expect_turn_complete_after(cli, &marker, BUDGET, "approved child completed");
}
