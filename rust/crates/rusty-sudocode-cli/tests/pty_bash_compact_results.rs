//! Assert model-visible bash results through the real CLI, including save/resume.
mod common;

use common::TestEnv;
use serde_json::Value;
use std::collections::BTreeMap;

fn results(request: &Value) -> BTreeMap<String, String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            let text = b["content"].as_str().map_or_else(
                || {
                    b["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|c| c["text"].as_str())
                        .collect::<String>()
                },
                str::to_owned,
            );
            (b["tool_use_id"].as_str().unwrap().to_owned(), text)
        })
        .collect()
}

fn assert_shell_statuses(sent: &BTreeMap<String, String>) {
    assert_eq!(
        sent.len(),
        9,
        "eight shell runs plus reading offloaded output"
    );
    let parsed: Vec<Value> = (0..5)
        .map(|i| serde_json::from_str(&sent[&format!("compact_bash_{i}")]).unwrap())
        .collect();
    assert_eq!(
        parsed[0],
        serde_json::json!({"stdout":"compact stdout", "exit_code":0})
    );
    assert_eq!(parsed[1]["exit_code"], 7);
    assert_eq!(parsed[1]["stderr"], "failure detail");
    assert_eq!(parsed[1]["returnCodeInterpretation"], "exit_code:7");
    assert_eq!(parsed[2]["returnCodeInterpretation"], "timeout");
    assert_eq!(parsed[2]["interrupted"], true);
    assert!(parsed[2].get("exit_code").is_none());
    assert!(parsed[3]["backgroundTaskId"].as_str().is_some());
    assert!(parsed[3].get("noOutputExpected").is_none());
    assert!(parsed[3]["rawOutputPath"].as_str().is_some());
    assert!(parsed[3].get("exit_code").is_none());
    if cfg!(unix) {
        assert!(parsed[4]["returnCodeInterpretation"]
            .as_str()
            .unwrap()
            .contains("signal"));
        assert!(parsed[4].get("exit_code").is_none());
    } else {
        assert_eq!(parsed[4]["exit_code"], 143);
    }
    let empty: Value = serde_json::from_str(&sent["compact_bash_6"]).unwrap();
    assert_eq!(empty, serde_json::json!({"stdout":"", "exit_code":0}));
    let diagnostic: Value = serde_json::from_str(&sent["compact_bash_7"]).unwrap();
    assert_eq!(
        diagnostic,
        serde_json::json!({"stdout":"", "stderr":"diagnostic only", "exit_code":0})
    );
    for result in &parsed {
        // Public Rust consumers can still deserialize the compact wire result.
        serde_json::from_value::<runtime::BashCommandOutput>(result.clone())
            .expect("compact output remains readable by the public result type");
        assert!(result.get("sandboxStatus").is_none());
        assert!(result.as_object().unwrap().values().all(|v| !v.is_null()));
    }
    assert!(sent["compact_bash_5"].contains("<persisted-output"));
    assert!(sent["compact_bash_8"].contains("large output line"));
}

#[test]
fn compact_bash_results_reach_model_and_survive_resume() {
    let env = TestEnv::new("compact-bash-results");
    if !env.is_mock() {
        return;
    }
    let prompt = env.prompt("Exercise shell result statuses", "bash_compact_results");
    let sent = shell_roundtrip(&env, &prompt, "compact bash results verified");
    assert_shell_statuses(&sent);
}

/// Drive shell results through the terminal, provider request, saved transcript
/// and cold resume. Assertions below use only these observed wire results.
fn shell_roundtrip(env: &TestEnv, prompt: &str, answer_marker: &str) -> BTreeMap<String, String> {
    let mut cli = env.spawn(&["--permission-mode", "danger-full-access", prompt]);
    cli.expect(answer_marker).expect("all tool steps complete");
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let requests: Vec<Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    let sent = results(requests.last().unwrap());
    let path = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        .expect("saved transcript");
    let saved = runtime::Session::load_from_path(&path).unwrap();
    let saved_results: BTreeMap<_, _> = saved
        .messages()
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            runtime::ContentBlock::ToolResult {
                tool_use_id,
                output,
                ..
            } => Some((tool_use_id.clone(), output.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(saved_results, sent, "persist exactly what the model saw");
    let mut resumed = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--resume",
        path.to_str().unwrap(),
    ]);
    resumed.expect("❯").expect("resumed prompt");
    let budget = std::time::Duration::from_secs(15);
    common::expect_input_line_cleared(&resumed, budget, "resume ready");
    let turn_marker = common::turn_status_marker(&resumed);
    let request_count = env.captured_message_count();
    resumed.send(&format!("{prompt}\r")).unwrap();
    resumed.expect(answer_marker).expect("resume completes");
    common::expect_turn_complete_after(&resumed, &turn_marker, budget, "resumed turn");
    common::expect_input_line_cleared(&resumed, budget, "ready to exit");
    assert!(
        env.captured_message_count() > request_count,
        "resume must actually call the provider"
    );
    resumed.send("/exit\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);
    assert_eq!(
        results(
            &serde_json::from_str::<Value>(env.captured_message_bodies().last().unwrap()).unwrap()
        ),
        sent,
        "resume preserves every result byte-for-byte"
    );
    sent
}

#[test]
fn native_shell_statuses_reach_model_and_survive_resume() {
    let env = TestEnv::new("native-shell-results");
    if !env.is_mock() {
        return;
    }
    let shells: &[&str] = if cfg!(windows) {
        &["bash", "PowerShell"]
    } else {
        &["bash"]
    };
    let mut calls = Vec::new();
    let mut expected = Vec::new();
    for &shell in shells {
        let codes: &[i32] = if shell == "PowerShell" {
            &[0, 7, -1_073_741_510]
        } else {
            &[0, 7]
        };
        for &code in codes {
            let id = format!("native_{}_{code}", shell.to_ascii_lowercase());
            let stdout = format!("preserved-stdout:{id}");
            let stderr = format!("preserved-stderr:{id}");
            let command = if shell == "PowerShell" {
                format!("[Console]::Out.WriteLine('{stdout}'); [Console]::Error.WriteLine('{stderr}'); exit {code}")
            } else {
                format!("printf '{stdout}\\n'; printf '{stderr}\\n' >&2; exit {code}")
            };
            calls.push(serde_json::json!({
                "id": id, "name": shell, "input": {"command": command},
            }));
            expected.push((id, code, stdout, stderr));
        }
    }
    let prompt = env.prompt(
        "Run the supplied shell commands and report their observed statuses.",
        &format!("tool_concurrency TOOL_BATCH:{}", serde_json::json!(calls)),
    );
    let sent = shell_roundtrip(&env, &prompt, "Concurrency batch done.");
    assert_eq!(sent.len(), expected.len(), "every real shell must report");
    for (id, code, stdout, stderr) in expected {
        let result: Value = serde_json::from_str(&sent[&id]).unwrap();
        assert_eq!(result["stdout"].as_str().unwrap().trim_end(), stdout);
        let observed_stderr = result["stderr"].as_str().unwrap();
        if code == -1_073_741_510 {
            assert_eq!(result["interrupted"], true, "{id}: {result}");
            assert!(result["exit_code"].is_null(), "{id}: {result}");
            assert_eq!(result["returnCodeInterpretation"], "interrupted");
            assert_eq!(
                observed_stderr.lines().collect::<Vec<_>>(),
                vec![stderr.as_str(), "Command interrupted by user"],
                "native cancellation preserves the actual stderr",
            );
        } else {
            assert_ne!(result["interrupted"], true, "{id}: {result}");
            assert_eq!(result["exit_code"], code, "{id}: {result}");
            assert_eq!(observed_stderr.trim_end(), stderr);
            assert_eq!(
                result["returnCodeInterpretation"],
                if code == 0 {
                    Value::Null
                } else {
                    serde_json::json!(format!("exit_code:{code}"))
                },
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn cancelled_bash_persists_a_compact_interruption() {
    let env = TestEnv::new("compact-bash-cancel");
    if !env.is_mock() {
        return;
    }
    let prompt = env.prompt("Run a long command", "bash_interrupt_long_running");
    let mut cli = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    cli.expect("❯").expect("initial prompt");
    cli.send(&format!("{prompt}\r")).unwrap();
    cli.expect("interrupt-start")
        .expect("running bash call is visible");
    cli.send("\x1b").unwrap();
    cli.expect("(?i)(cancelled|interrupted)")
        .expect("turn cancelled");
    common::expect_input_line_cleared(
        &cli,
        std::time::Duration::from_secs(15),
        "cancel ready to exit",
    );
    cli.send("/exit\r").unwrap();
    cli.set_default_timeout(std::time::Duration::from_secs(15));
    cli.expect_eof().expect("cancel exits promptly");
    let path = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        .expect("saved transcript");
    let saved = runtime::Session::load_from_path(&path).unwrap();
    let output = saved
        .messages()
        .iter()
        .flat_map(|m| &m.blocks)
        .find_map(|b| match b {
            runtime::ContentBlock::ToolResult {
                tool_name, output, ..
            } if tool_name == "bash" => Some(output),
            _ => None,
        })
        .expect("cancelled tool must have a result");
    let result: Value = serde_json::from_str(output).unwrap();
    assert_eq!(result["interrupted"], true);
    assert_eq!(result["returnCodeInterpretation"], "interrupted");
    assert!(result.get("exit_code").is_none());
    assert!(result.get("sandboxStatus").is_none());
    assert!(result.as_object().unwrap().values().all(|v| !v.is_null()));
}

/// What a failing PowerShell call shows the model and the user. A script that
/// cannot be parsed reads as a parse error — nothing in it ran — rather than an
/// exit code, with the parser's own message on screen. Text a console program
/// wrote in the console's code page is decoded, not replaced. The model gets
/// the same compact result the card is drawn from.
#[cfg(windows)]
#[test]
fn powershell_failures_read_as_written() {
    const DECODED: &str = "编码错误：配置无效";
    let env = TestEnv::new("powershell-failures");
    if !env.is_mock() {
        return;
    }
    // Console programs write in the console's code page. Switching the shared
    // console to CP936 from inside the run makes this call write GBK bytes on
    // any Windows locale, and tells the reader in this process the same. The
    // text is joined at run time, so only the decoded output can show it whole:
    // the command itself is on screen too.
    let calls = serde_json::json!([
        {"id": "ps_parse", "name": "PowerShell", "input": {"command": "Write-Output 'unclosed' ("}},
        {"id": "ps_cp936", "name": "PowerShell", "input": {"command":
            "[Console]::OutputEncoding = [Text.Encoding]::GetEncoding(936); [Console]::Error.WriteLine('编码错误' + '：配置无效'); exit 1"
        }},
    ]);
    let prompt = env.prompt(
        "Run the supplied PowerShell commands.",
        &format!("tool_concurrency TOOL_BATCH:{calls}"),
    );
    let mut cli = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    cli.resize(60, 200).unwrap();
    let screen = common::expect_screen(
        &cli,
        |s| s.contains("Concurrency batch done."),
        std::time::Duration::from_secs(90),
        "both PowerShell calls complete",
    );
    assert!(
        screen.contains("parse error"),
        "the unparsable script is named a parse error:\n{screen}"
    );
    assert!(
        screen.contains("ParserError"),
        "the parser's own message is on screen, not behind a line count:\n{screen}"
    );
    assert!(
        screen.contains(DECODED),
        "code-page output is decoded on screen:\n{screen}"
    );
    assert!(!screen.contains('\u{FFFD}'), "nothing replaced:\n{screen}");
    assert_eq!(cli.expect_eof().unwrap(), 0);

    let request: Value =
        serde_json::from_str(env.captured_message_bodies().last().unwrap()).unwrap();
    let sent = results(&request);
    let parse: Value = serde_json::from_str(&sent["ps_parse"]).unwrap();
    assert_eq!(parse["returnCodeInterpretation"], "parse_error", "{parse}");
    assert!(
        parse["stderr"].as_str().unwrap().contains("ParserError"),
        "{parse}"
    );
    let decoded: Value = serde_json::from_str(&sent["ps_cp936"]).unwrap();
    assert_eq!(
        decoded["stderr"].as_str().unwrap().trim_end(),
        DECODED,
        "{decoded}"
    );
    assert_eq!(decoded["returnCodeInterpretation"], "exit_code:1");
    for result in [&parse, &decoded] {
        assert!(result.get("lossyOutput").is_none(), "{result}");
        assert!(
            result
                .as_object()
                .unwrap()
                .values()
                .all(|v| !v.is_null() && *v != Value::Bool(false)),
            "only fields that say something reach the model: {result}"
        );
    }
}
