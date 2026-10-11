//! Real Opus 5.5 acceptance; mock payload assertions live in pty_cache_prefix.
//! SCODE_TEST_BACKEND=live SCODE_LIVE_MODEL=claude-opus-5-5
//! SCODE_LIVE_AUTH_PROFILE=<account> cargo test --test pty_adaptive_cache_live -- --nocapture
//! Set SCODE_ADAPTIVE_DISPLAY=omitted for the hidden-summary acceptance arm.
mod common;
#[path = "support/request_evidence.rs"]
mod request_evidence;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(180);

fn fingerprint(value: &Value) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap()))
}

fn message_shapes(body: &Value) -> Vec<Value> {
    body["messages"].as_array().into_iter().flatten().map(|message| {
        let blocks: Vec<_> = message["content"].as_array().into_iter().flatten().map(|block| {
            json!({"type":block["type"], "hash":fingerprint(&without_cache_markers(block.clone())),
                "cache_control":block["cache_control"],
                "thinking_bytes":block["thinking"].as_str().map(str::len),
                "signature_bytes":block["signature"].as_str().map(str::len)})
        }).collect();
        json!({"role":message["role"], "hash":fingerprint(&without_cache_markers(message.clone())), "blocks":blocks})
    }).collect()
}

/// Keep credential-free evidence even when a live assertion fails. The fixture
/// itself contains copied credentials and must still be removed by TestEnv.
struct Evidence {
    log: PathBuf,
    cache: PathBuf,
}

impl Drop for Evidence {
    fn drop(&mut self) {
        let Ok(destination) = std::env::var("SCODE_ADAPTIVE_REPORT") else {
            return;
        };
        let raw = std::fs::read_to_string(&self.log).unwrap_or_default();
        let events: Vec<Value> = raw
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let shape: Vec<_> = events.iter().filter(|r| r["event"] == "request_debug").map(|r| {
            let body = &r["attributes"]["body"];
            json!({"request_id":r["attributes"]["request_id"], "model":body["model"], "thinking":body["thinking"], "effort":body["output_config"]["effort"],
                "max_tokens":body["max_tokens"], "messages":body["messages"].as_array().map(Vec::len),
                "compaction":body.to_string().contains("Create a concise checkpoint"),
                "metadata_hash":fingerprint(&body["metadata"]),
                "system_hash":fingerprint(&body["system"]), "tools_hash":fingerprint(&body["tools"]),
                "message_shapes":message_shapes(body)})
        }).collect();
        let mut usage = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.cache) {
            for entry in entries.flatten() {
                let rows = std::fs::read_to_string(entry.path().join("requests.jsonl"))
                    .unwrap_or_default();
                for row in rows
                    .lines()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                {
                    usage.push(json!({"read":row["cache_read_input_tokens"], "write":row["cache_creation_input_tokens"], "input":row["input_tokens"],
                        "at_unix_secs":row["at_unix_secs"], "gateway_request_id":row["gateway_request_id"],
                        "provider_request_id":row["provider_request_id"], "break_reason":row["break_reason"]}));
                }
            }
        }
        let lifecycle: Vec<_> = events.iter().filter(|event| {
            matches!(event["event"].as_str(), Some("request_started" | "request_succeeded" | "request_failed" | "response_usage"))
        }).map(|event| {
            let attributes = &event["attributes"];
            json!({"event":event["event"], "request_id":attributes["request_id"],
                "attempt":attributes["attempt"], "status":attributes["status"], "retryable":attributes["retryable"]})
        }).collect();
        let report = json!({"requests":shape, "lifecycle":lifecycle, "usage":usage});
        let _ = std::fs::write(destination, serde_json::to_vec_pretty(&report).unwrap());
    }
}

fn transcript(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = transcript(&path) {
                return Some(found);
            }
        } else if path
            .file_name()
            .is_some_and(|name| name == "transcript.jsonl")
        {
            return Some(path);
        }
    }
    None
}

fn requests(path: &Path) -> Vec<Value> {
    request_evidence::accepted_messages(path)
}

fn turn(cli: &mut pty_expect::PtySession, prompt: &str) {
    let marker = common::turn_status_marker(cli);
    cli.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(cli, &marker, WAIT, prompt);
    common::expect_input_line_cleared(cli, WAIT, "live turn complete");
}

fn without_cache_markers(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("cache_control");
            for child in object.values_mut() {
                *child = without_cache_markers(std::mem::take(child));
            }
        }
        Value::Array(array) => {
            for child in array {
                *child = without_cache_markers(std::mem::take(child));
            }
        }
        _ => {}
    }
    value
}

#[test]
fn adaptive_live_tool_chain_compaction_and_resume_read_cache() {
    let env = common::TestEnv::new("adaptive-cache-live");
    if env.is_mock() || common::live_model() != "claude-opus-5-5" {
        eprintln!("SKIP: requires live backend and SCODE_LIVE_MODEL=claude-opus-5-5");
        return;
    }
    let display =
        std::env::var("SCODE_ADAPTIVE_DISPLAY").unwrap_or_else(|_| "summarized".to_string());
    assert!(matches!(display.as_str(), "summarized" | "omitted"));
    let settings_path = env.config_home().join("settings.json");
    let mut settings: Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    settings["thinking"] = json!(display == "summarized");
    std::fs::write(&settings_path, serde_json::to_vec(&settings).unwrap()).unwrap();
    // Each next filename is available only in the preceding file. This forces
    // real tool round trips rather than a response that happens to say 'OK'.
    let files: Vec<_> = (0..8)
        .map(|_| {
            tempfile::NamedTempFile::new_in(env.workspace_root())
                .unwrap()
                .into_temp_path()
        })
        .collect();
    let names: Vec<_> = files
        .iter()
        .map(|path| path.file_name().unwrap().to_str().unwrap())
        .collect();
    for (index, name) in names.iter().enumerate() {
        let next = names.get(index + 1).copied().unwrap_or("END");
        std::fs::write(
            env.workspace_root().join(name),
            format!(
                "value={}\nnext={next}\n{}",
                37 + index,
                "Fixture note: preserve the numbered values and follow the next file.\n".repeat(64)
            ),
        )
        .unwrap();
    }
    let log = env.workspace_root().join("requests.jsonl");
    let _evidence = Evidence {
        log: log.clone(),
        cache: env.config_home().join("cache/prompt-cache"),
    };
    let extra_env = [("SCODE_LOG_PATH", log.to_str().unwrap())];
    let mut cli = env.spawn_with_env(
        &[
            "--reasoning-effort",
            "high",
            "--permission-mode",
            "read-only",
        ],
        &extra_env,
    );
    cli.set_default_timeout(WAIT);
    common::expect_input_line_cleared(&cli, WAIT, "live adaptive ready");
    turn(&mut cli, &format!("Read {} with read_file, follow each next filename using read_file until END, sum only the value fields. Do not use shell commands. Reply ADAPTIVE_CHAIN_OK:<sum>.", names[0]));
    let path = transcript(&env.workspace_root().join(".scode")).expect("persisted transcript");
    let session = runtime::Session::load_from_path(&path).unwrap();
    assert!(session
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .any(|b| matches!(b,
        runtime::ContentBlock::Text { text } if text.contains("ADAPTIVE_CHAIN_OK:324"))));
    assert!(
        session
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .any(|b| matches!(b,
        runtime::ContentBlock::Thinking { thinking, signature: Some(signature) }
        if !signature.is_empty() && (display == "omitted" || !thinking.is_empty()))),
        "live route must preserve signed thinking, including hidden summaries"
    );
    if display == "omitted" {
        assert!(session.messages.iter().flat_map(|m| &m.blocks).all(|b| {
            !matches!(b, runtime::ContentBlock::Thinking { thinking, .. } if !thinking.is_empty())
        }), "hidden mode must omit readable summaries, not the signed blocks");
    }
    let ordinary = requests(&log);
    assert!(ordinary.len() >= 9, "must exercise the actual tool chain");
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "live compaction complete");
    turn(
        &mut cli,
        "What was the sum of the value fields? Reply ADAPTIVE_RESUME_OK:<sum>.",
    );
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let mut cli = env.spawn_with_env(
        &[
            "--resume",
            path.to_str().unwrap(),
            "--reasoning-effort",
            "high",
            "--permission-mode",
            "read-only",
        ],
        &extra_env,
    );
    cli.set_default_timeout(WAIT);
    common::expect_input_line_cleared(&cli, WAIT, "live adaptive resumed");
    turn(
        &mut cli,
        "Confirm the earlier sum without tools. Reply ADAPTIVE_RESUME_OK:<sum>.",
    );
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let resumed = runtime::Session::load_from_path(&path).unwrap();
    assert!(resumed
        .messages
        .last()
        .unwrap()
        .blocks
        .iter()
        .any(|b| matches!(b,
        runtime::ContentBlock::Text { text } if text.contains("ADAPTIVE_RESUME_OK:324"))));
    let bodies = requests(&log);
    let baseline = &bodies[0];
    for body in &bodies {
        assert_eq!(
            body["thinking"],
            json!({"type":"adaptive","display":display})
        );
        assert_eq!(body["output_config"]["effort"], "high");
        for key in [
            "model",
            "thinking",
            "output_config",
            "system",
            "tools",
            "metadata",
        ] {
            assert_eq!(
                body[key], baseline[key],
                "cache-relevant field changed: {key}"
            );
        }
    }
    let ledger = env
        .config_home()
        .join("cache/prompt-cache")
        .join(&session.session_id)
        .join("requests.jsonl");
    let rows: Vec<Value> = std::fs::read_to_string(ledger)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let report = json!({"model":baseline["model"], "request_count":bodies.len(), "usage":rows.iter().map(|r| json!({
        "read":r["cache_read_input_tokens"], "write":r["cache_creation_input_tokens"], "input":r["input_tokens"]
    })).collect::<Vec<_>>()});
    eprintln!("{report}");
    if let Ok(path) = std::env::var("SCODE_ADAPTIVE_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert_eq!(
        rows.len(),
        bodies.len(),
        "every real request must have usage evidence"
    );
    for (index, row) in rows.iter().enumerate().skip(1) {
        let current = without_cache_markers(bodies[index]["messages"].clone());
        let current = current.as_array().unwrap();
        let expected_read = bodies[..index]
            .iter()
            .zip(&rows[..index])
            .filter_map(|(body, usage)| {
                let previous = without_cache_markers(body["messages"].clone());
                let previous = previous.as_array().unwrap();
                current.starts_with(previous).then(|| {
                    usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
                        + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                })
            })
            .max()
            .unwrap_or(0);
        assert!(
            row["cache_read_input_tokens"].as_u64().unwrap_or(0) >= expected_read.max(1),
            "request {index} did not reuse the previously cached prefix (expected at least {expected_read}); a tools-only partial hit is not acceptance"
        );
    }
}
