//! Print mode must never read approval answers, even from a real terminal.
mod common;

#[test]
fn print_denies_approval_without_reading_the_terminal() {
    let env = common::TestEnv::new("headless-permission");
    if env.is_live() {
        return;
    }
    let prompt = env.prompt("", "bash_permission_prompt_denied");
    let mut child = env.spawn(&[
        "-p",
        &prompt,
        "--permission-mode",
        "workspace-write",
        "--output-format=json",
    ]);
    child.set_default_timeout(std::time::Duration::from_secs(15));
    // Sending no keystrokes is the assertion: the old prompter would hang here.
    child
        .expect("interactive approval unavailable in print mode")
        .unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
}

/// A finite invocation must retain its synchronous child until completion,
/// even when the ordinary REPL would hand that child back as a background job.
#[test]
fn print_waits_for_synchronous_agent_past_auto_background_threshold() {
    let env = common::TestEnv::new("headless-sync-agent");
    if env.is_live() {
        return;
    }
    let prompt = env.prompt("", "subagent_events_sync_slow");
    let mut child = env.spawn_with_env(
        &["-p", &prompt, "--permission-mode", "danger-full-access"],
        &[("SUDOCODE_AGENT_AUTO_BG_SECS", "1")],
    );
    child.expect("subagent_events_sync_slow done").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    let requests: Vec<serde_json::Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    let result = requests
        .iter()
        .flat_map(|request| request["messages"].as_array().unwrap())
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "toolu_events_sync")
        .expect("parent must receive the delegated result");
    let text = result["content"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>()
        });
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(manifest["status"], "completed", "{manifest}");
    assert_eq!(
        manifest["result"],
        mock_anthropic_service::SUBAGENT_CHILD_ANSWER
    );
}

#[test]
fn print_completes_tool_roundtrip_in_each_output_format() {
    for format in ["text", "json", "stream-json"] {
        let env = common::TestEnv::new("headless-output");
        std::fs::write(
            env.workspace_root().join("fixture.txt"),
            "alpha parity line\n",
        )
        .unwrap();
        let prompt = env.prompt(
            "Read fixture.txt and report its exact contents.",
            "read_file_roundtrip",
        );
        let mut child = env.spawn(&[
            "-p",
            &prompt,
            "--permission-mode",
            "read-only",
            "--output-format",
            format,
        ]);
        child.expect("alpha parity line").unwrap_or_else(|error| {
            panic!("{format}: {error}; {}", common::screen_tail(&child, 6000))
        });
        assert_eq!(child.expect_eof().unwrap(), 0);
    }
}
