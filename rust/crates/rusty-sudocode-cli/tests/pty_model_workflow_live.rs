//! Real-key acceptance: delegate a fresh quote, compact, then resume its work.
//!
//! ```text
//! SCODE_TEST_BACKEND=live SCODE_LIVE_MODEL=claude-sonnet-4-6
//! SCODE_LIVE_AUTH_PROFILE=sudorouter cargo test -p rusty-sudocode-cli
//!   --test pty_model_workflow_live -- --ignored --nocapture
//! ```
//! Uses disposable credentials/workspace copies from the ordinary PTY harness.
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use runtime::{ContentBlock, Session};
use serde_json::{json, Value};

const BUDGET: Duration = Duration::from_secs(240);

fn transcripts(dir: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            transcripts(&path, paths);
        } else if path.file_name().is_some_and(|n| n == "transcript.jsonl") {
            paths.push(path);
        }
    }
}

// render takes a closure over screens with different lifetimes.
#[allow(clippy::redundant_closure_for_method_calls)]
fn run(env: &TestEnv, args: &[&str]) {
    let mut cli = env.spawn_with_env(args, &[("SUDOCODE_AGENT_SUMMARY_THRESHOLD_CHARS", "120")]);
    cli.set_default_timeout(BUDGET);
    let exit = cli.expect_eof().unwrap_or_else(|e| {
        panic!(
            "live CLI did not finish: {e}\n{}",
            cli.render(|s| s.contents())
        )
    });
    assert_eq!(exit, 0, "{}", cli.render(|s| s.contents()));
}

#[allow(clippy::redundant_closure_for_method_calls)]
fn resume_turn(env: &TestEnv, path: &Path, prompt: &str) {
    let mut cli = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--resume",
        path.to_str().unwrap(),
    ]);
    cli.set_default_timeout(BUDGET);
    common::expect_input_line_cleared(&cli, BUDGET, "resumed input ready");
    let marker = common::turn_status_marker(&cli);
    cli.send(prompt).unwrap();
    common::expect_input_line(&cli, &prompt[..20], BUDGET, "resumed prompt entered");
    cli.send("\r").unwrap();
    common::expect_turn_complete_after(&cli, &marker, BUDGET, "resumed turn completed");
    cli.send("/exit").unwrap();
    common::expect_input_line(&cli, "/exit", BUDGET, "exit entered");
    cli.send("\r").unwrap();
    assert_eq!(
        cli.expect_eof().unwrap(),
        0,
        "{}",
        cli.render(|s| s.contents())
    );
}

fn live_env() -> TestEnv {
    let env = TestEnv::new("model-workflow-live");
    assert!(env.is_live(), "this acceptance test must use a real model");
    // This journey uses built-in tools; leave local MCP services out of its copy.
    let settings_path = env.config_home().join("settings.json");
    let mut settings: Value = fs::read(&settings_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    settings.as_object_mut().unwrap().remove("mcpServers");
    fs::write(settings_path, settings.to_string()).unwrap();
    env
}

#[test]
#[ignore = "funded live model: set SCODE_TEST_BACKEND=live and SCODE_LIVE_MODEL"]
fn delegate_compact_and_resume_a_fresh_quote() {
    let env = live_env();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let code = format!("QUOTE-{nonce}");
    let units = u64::try_from(nonce % 19).unwrap() + 7;
    let subtotal = units * 37 + 53;
    let fixture = format!(
        "Quote code: {code}\nUnits: {units}\nUnit price: 37\nDelivery: 53\n\
         Currency: USD. The subtotal is units times unit price plus delivery.\n\
         Keep the quote code and calculated subtotal for the next step. \
         Pending work: after review, apply a discount of 11 and write the final JSON. \
         This is disposable test data, with no connection to a real purchase.\n"
    );
    let source = env.workspace_root().join("quote.txt");
    fs::write(&source, &fixture).unwrap();
    // The answer occurs only in the file, never in the user's prompt.
    let child_prompt = format!("Read exactly {} using the file tool. Return its entire contents verbatim, then calculate units times unit price plus delivery. Retain the quote code and subtotal.", source.display());
    let spawn =
        json!({"agent":"Explore", "fresh":true, "run_in_background":false, "prompt":child_prompt});
    let prompt = format!("For the quote-workflow acceptance, invoke agent_spawn exactly once with this JSON: {spawn}. Wait for completion using pid_output if needed. Report the child's quote code and subtotal and retain both for the next step. Do not read the file yourself or write files yet.");
    run(
        &env,
        &[
            "--permission-mode",
            "danger-full-access",
            "--print",
            &prompt,
        ],
    );

    verify_child(&env, &code, subtotal);

    let parent = parent_transcript(&env);
    let path = &parent;

    // Review a substantial checklist through a real file read. This gives
    // compaction useful history to reduce, rather than a tiny conversation
    // whose eight-section checkpoint would be larger than its source.
    let review = env.workspace_root().join("review.txt");
    fs::write(&review, include_str!("fixtures/model_review_brief.txt")).unwrap();
    resume_turn(&env, path, &format!("Read exactly {} as the review checklist for our quote. Give a concise review using the retained code and subtotal. Do not read quote.txt or write output files yet.", review.display()));
    // The last two messages stay verbatim; approval moves the review and its
    // source into the prefix that must actually be summarized.
    resume_turn(&env, path, "Review accepted. Retain the quote code, subtotal and pending discount of 11 for later implementation. Do not use tools; reply READY.");
    fs::remove_file(source).unwrap();
    fs::remove_file(review).unwrap();
    run(
        &env,
        &[
            "--permission-mode",
            "danger-full-access",
            "--resume",
            path.to_str().unwrap(),
            "/compact",
        ],
    );
    let compacted = Session::load_from_path(path).unwrap();
    let checkpoint = compacted
        .compaction
        .as_ref()
        .expect("real compaction must be persisted");
    assert!(checkpoint.count > 0);
    assert!(checkpoint.summary.contains(&code), "{}", checkpoint.summary);
    assert!(
        checkpoint.summary.contains(&subtotal.to_string()),
        "{}",
        checkpoint.summary
    );

    let output = env.workspace_root().join("approved.json");
    resume_turn(&env, path,
        &format!("Continue the quote from our checkpoint. Apply the pending discount of 11 to its subtotal. \
         Using only retained context, use write_file to write {} with exactly the fields code, subtotal, \
         discount, total. All amounts are JSON numbers. The absolute output path is given here; \
         do not search directories or read files because the source was removed.", output.display()));
    let approved: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
    assert_eq!(
        approved,
        json!({"code": code, "subtotal": subtotal, "discount": 11, "total": subtotal - 11})
    );
    eprintln!("LIVE PTY: child read/calculation, result summary, persisted compaction and resumed artifact verified");
}

fn parent_transcript(env: &TestEnv) -> PathBuf {
    let mut paths = Vec::new();
    transcripts(&env.workspace_root().join(".scode"), &mut paths);
    let parents: Vec<_> = paths
        .into_iter()
        .filter(|p| {
            fs::read_to_string(p)
                .unwrap()
                .contains("quote-workflow acceptance")
        })
        .collect();
    assert_eq!(parents.len(), 1, "must find the actual parent transcript");
    let before = Session::load_from_path(&parents[0]).unwrap();
    let spawns: Vec<_> = before
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| {
            if let ContentBlock::ToolUse { name, input, .. } = b {
                if name == "agent_spawn" || name == "Agent" {
                    return Some(serde_json::from_str::<Value>(input).unwrap());
                }
            }
            None
        })
        .collect();
    assert_eq!(spawns.len(), 1);
    assert!(spawns[0].get("model").is_none() && spawns[0].get("auth_mode").is_none());
    parents.into_iter().next().unwrap()
}

fn verify_child(env: &TestEnv, code: &str, subtotal: u64) {
    let manifests: Vec<Value> = fs::read_dir(env.workspace_root().join(".sudocode-agents"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| serde_json::from_slice(&fs::read(e.path()).unwrap()).unwrap())
        .collect();
    assert_eq!(manifests.len(), 1, "must actually delegate once");
    let child = &manifests[0];
    assert_eq!(child["model"], common::live_model());
    assert_eq!(child["status"], "completed", "{child}");
    let summary = child["result"].as_str().expect("child summary");
    assert!(
        summary.contains(code) && summary.contains(&subtotal.to_string()),
        "{summary}"
    );
    let full = child["resultFullPath"]
        .as_str()
        .expect("must exercise model result summarization");
    let result = fs::read_to_string(full).unwrap();
    assert!(
        result.contains(code) && result.contains(&subtotal.to_string()),
        "{result}"
    );
}
