//! Large model/context tool replies must reach the next turn without pipe deadlock.
#![cfg(windows)]
mod common;

use std::fs;
use std::process::Command;
use std::time::Duration;

#[test]
fn powershell_drains_both_streams_before_exit_and_next_turn_reads_the_result() {
    let env = common::TestEnv::new("powershell-large-output");
    let node = Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("Node.js is required for the shell fixture");
    assert!(node.status.success());
    let node = String::from_utf8(node.stdout).unwrap();
    let node = node.trim().replace('\\', "/").replace('\'', "''");
    let run_id = env.workspace_root().file_name().unwrap().to_string_lossy();
    fs::write(
        env.workspace_root().join("emit.cjs"),
        format!(
            "const fs=require('fs');let out=0,err=0;for(let i=0;i<128;i++){{out+=fs.writeSync(1,'O'.repeat(8192));err+=fs.writeSync(2,'E'.repeat(8192));}}fs.writeFileSync('completed.json',JSON.stringify({{run_id:{},stdout_bytes:out,stderr_bytes:err}}));",
            serde_json::to_string(&run_id).unwrap()
        ),
    )
    .unwrap();
    let command = format!("& '{node}' emit.cjs");
    let prompt = if env.is_mock() {
        env.tool_batch_prompt(&serde_json::json!([{
            "id":"large-powershell", "name":"PowerShell",
            "input":{"command":command,"timeout":10000,"run_in_background":false}
        }]))
    } else {
        format!("Run this local output regression once using the PowerShell tool in the foreground with timeout=10000: {command}. Do not edit or retry the script. It writes both stdout and stderr before recording its completion.")
    };
    let mut cli = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "PowerShell",
    ]);
    cli.expect("❯").expect("initial prompt");
    let first = common::turn_status_marker(&cli);
    cli.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(&cli, &first, env.timeout(), "large-output turn completes");
    let completed: serde_json::Value = serde_json::from_slice(
        &fs::read(env.workspace_root().join("completed.json"))
            .expect("both OS pipes drained before the command's completion marker"),
    )
    .unwrap();
    assert_eq!(completed["run_id"], run_id.as_ref());
    assert_eq!(completed["stdout_bytes"], 1_048_576);
    assert_eq!(completed["stderr_bytes"], 1_048_576);
    common::expect_input_line_cleared(&cli, Duration::from_secs(15), "ready for follow-up");
    let second = common::turn_status_marker(&cli);
    let command = "Get-Content -Raw completed.json";
    let prompt = if env.is_mock() {
        env.tool_batch_prompt(&serde_json::json!([{
            "id":"read-completed", "name":"PowerShell",
            "input":{"command":command,"timeout":10000,"run_in_background":false}
        }]))
    } else {
        format!(
            "Read the actual result with PowerShell: {command}. Report its run_id and byte counts."
        )
    };
    cli.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(
        &cli,
        &second,
        env.timeout(),
        "follow-up reads completed result",
    );
    common::expect_input_line_cleared(&cli, Duration::from_secs(15), "ready to exit");
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().expect("clean exit"), 0);
    let transcript = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        .expect("saved transcript");
    let session = runtime::Session::load_from_path(&transcript).unwrap();
    let results: Vec<serde_json::Value> = session
        .messages()
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            runtime::ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
                ..
            } if tool_name == "PowerShell" => {
                assert!(!is_error, "shell result must succeed");
                // Large results are persisted separately; the transcript keeps
                // their bounded preview and opaque read-more identity.
                let full = if output.starts_with("<persisted-output ") {
                    fs::read_to_string(
                        session
                            .tool_results_dir()
                            .expect("persisted tool results")
                            .join(session.tool_output_id(tool_use_id)),
                    )
                    .expect("complete persisted shell result")
                } else {
                    output.clone()
                };
                Some(serde_json::from_str(&full).expect("structured shell result"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["exit_code"], 0);
    assert_eq!(results[1]["exit_code"], 0);
    for (field, byte) in [("stdout", b'O'), ("stderr", b'E')] {
        let stream = results[0][field]
            .as_str()
            .expect("drained output stream")
            .trim_end_matches(['\r', '\n']);
        assert_eq!(stream.len(), 1_048_576, "complete {field}");
        assert!(stream.bytes().all(|value| value == byte), "exact {field}");
    }
    let recovered: serde_json::Value =
        serde_json::from_str(results[1]["stdout"].as_str().expect("follow-up stdout")).unwrap();
    assert_eq!(
        recovered, completed,
        "the next model turn receives the real prior command result"
    );
}
