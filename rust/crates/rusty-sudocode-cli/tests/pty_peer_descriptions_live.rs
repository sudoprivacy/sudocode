//! Real-provider compaction acceptance and cache-usage observations.
mod common;

use std::time::Duration;

use common::TestEnv;
use runtime::{ConversationMessage, Session};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(30);

#[test]
fn live_peer_description_reuses_compaction_and_retains_provider_cache_observations() {
    let env = TestEnv::new("peer-description-live");
    if env.is_mock() {
        return;
    }
    let mut observations = Vec::new();
    for enabled in [false, true] {
        let settings_path = env.config_home().join("settings.json");
        let mut settings: Value = std::fs::read(&settings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| json!({}));
        settings["agentName"] = json!("peercompass-author");
        settings["experimental"]["peerDescriptions"] = json!(enabled);
        settings["experimental"]["mcpConfigServers"] = json!(false);
        settings["experimental"]["coordinatorMode"] = json!(false);
        std::fs::write(settings_path, settings.to_string()).unwrap();
        let path = live_history(&env, enabled);
        let mut cli = env.spawn(&[
            "--resume",
            path.to_str().unwrap(),
            "--permission-mode",
            "read-only",
        ]);
        common::expect_input_line_cleared(&cli, WAIT, "live peer ready");
        let marker = common::turn_status_marker(&cli);
        cli.send("This agent's ongoing task is peer description discovery for the PeerCompass project. We have changed focus from the old benchmark work. Reply with READY only; do not use tools.\r").unwrap();
        common::expect_turn_complete_after(&cli, &marker, WAIT, "live prefix warmup completed");
        cli.send("/compact\r").unwrap();
        cli.expect("Messages removed").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "live description checkpoint committed");
        cli.send("/exit\r").unwrap();
        assert_eq!(cli.expect_eof().unwrap(), 0);
        let session = Session::load_from_path(&path).unwrap();
        let report = session.last_compaction_report.as_ref().unwrap();
        assert_eq!(
            report.completed_responses, 1,
            "metadata requires no separate model response"
        );
        let metadata_path = runtime::mailbox::local_pair_root_in(env.config_home())
            .join("agents/peercompass-author/description.json");
        let description = std::fs::read(metadata_path)
            .ok()
            .map(|bytes| serde_json::from_slice::<Value>(&bytes).unwrap());
        let usage: Vec<_> = session.maintenance_usage.iter().map(|receipt| json!({
            "attempt_id": receipt.attempt_id,
            "input_tokens": receipt.usage.map(|usage| usage.input_tokens),
            "output_tokens": receipt.usage.map(|usage| usage.output_tokens),
            "cache_read_input_tokens": receipt.usage.map(|usage| usage.cache_read_input_tokens),
            "cache_creation_input_tokens": receipt.usage.map(|usage| usage.cache_creation_input_tokens),
        })).collect();
        observations.push(json!({"enabled":enabled, "model":session.model,
            "description":description, "maintenance_usage":usage,
            "attempts":report.attempts, "completed_responses":report.completed_responses,
            "checkpoint":session.compaction.as_ref().map(|value| &value.summary),
            "live_discovery_verified":false}));
        write_observations(&observations);
        if enabled {
            let description =
                description.expect("the live model emits the requested peer metadata");
            assert!(
                description["description"]
                    .as_str()
                    .unwrap()
                    .to_lowercase()
                    .contains("peer"),
                "description must reflect the latest preserved assignment: {description}"
            );
            assert_eq!(description["source_session_id"], session.session_id);
            verify_live_peer_discovery(&env, &description);
            observations.last_mut().unwrap()["live_discovery_verified"] = json!(true);
        } else {
            assert!(
                description.is_none(),
                "disabled experiment retains production behavior"
            );
        }
        write_observations(&observations);
    }
}

fn write_observations(observations: &[Value]) {
    if let Ok(destination) = std::env::var("SCODE_PEER_DESCRIPTION_REPORT") {
        std::fs::write(destination, serde_json::to_vec_pretty(&json!({
            "observations":observations,
            "scope":"Two live samples of synthetic tasks; provider-reported usage is not a statistical cache-efficiency comparison.",
        })).unwrap()).unwrap();
    }
}

fn verify_live_peer_discovery(env: &TestEnv, description: &Value) {
    let settings_path = env.config_home().join("settings.json");
    let mut settings: Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    settings["agentName"] = json!("peercompass-observer");
    std::fs::write(settings_path, settings.to_string()).unwrap();
    let observer_path = env.workspace_root().join("live-observer.jsonl");
    let mut observer = Session::new().with_workspace_root(env.workspace_root());
    observer.model = Some(common::live_model());
    observer.save_to_path(&observer_path).unwrap();
    drop(observer);
    let mut cli = env.spawn(&[
        "--resume",
        observer_path.to_str().unwrap(),
        "--permission-mode",
        "read-only",
    ]);
    common::expect_input_line_cleared(&cli, WAIT, "live observer ready");
    let marker = common::turn_status_marker(&cli);
    cli.send("Call agent_list to discover the reachable peers. Read the description returned for peercompass-author and quote it in your answer. You may use ToolSearch to locate agent_list if needed. Do not use shell or file tools.\r").unwrap();
    common::expect_turn_complete_after(&cli, &marker, WAIT, "live peer discovery completed");
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let session = Session::load_from_path(&observer_path).unwrap();
    let listing: Value = session
        .messages()
        .iter()
        .flat_map(|message| &message.blocks)
        .find_map(|block| match block {
            runtime::ContentBlock::ToolResult {
                tool_name,
                output,
                is_error: false,
                ..
            } if tool_name == "agent_list" => serde_json::from_str(output).ok(),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "the live observer must call agent_list successfully: {:?}",
                session.messages()
            )
        });
    let author = listing["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "peercompass-author")
        .expect("the author remains discoverable after exit");
    assert_eq!(author["description"], description["description"]);
    assert_eq!(author["description_updated_at"], description["updated_at"]);
}

fn live_history(env: &TestEnv, enabled: bool) -> std::path::PathBuf {
    let mut session = Session::new().with_workspace_root(env.workspace_root());
    session.model = Some(common::live_model());
    for index in 0..12 {
        session
            .push_user_text(format!(
                "Old database benchmark decision {index}: {}",
                "Keep benchmark fixtures isolated and retain deterministic results. ".repeat(80)
            ))
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![
                runtime::ContentBlock::Text {
                    text: format!("Recorded benchmark decision {index}; synthetic fixture only."),
                },
            ]))
            .unwrap();
    }
    let path = env
        .workspace_root()
        .join(format!("live-description-{enabled}.jsonl"));
    session.save_to_path(&path).unwrap();
    path
}
