//! PTY live e2e — per-agent-type memory scoping.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.3 Commit 9.  Each
//! built-in preset (Explore, Plan, Verification, …) and each custom
//! `.md` agent reads/writes its own memory namespace under
//! `<workspace>/agent-memory/<subagent_type>/`.  Agent A's remembered
//! facts must NOT surface in agent B's memory index.
//!
//! ## What this test proves
//!
//! Two distinct memory dirs are pre-seeded under the test's isolated
//! `SUDOCODE_MEMORY_DIR/agent-memory/`:
//! - `Explore/`  — carries a sentinel `EXPLORE_ONLY_SENTINEL_QWERTY`
//! - `Plan/`     — carries a sentinel `PLAN_ONLY_SENTINEL_ZXCV`
//!
//! Then the parent spawns TWO sub-agents and gives each ordinary
//! work — neither is asked about its memory:
//! - one `Explore` worker
//! - one `Plan` worker
//!
//! Each worker's reply must contain ONLY its own sentinel — never the
//! other's.  The test fails if either sentinel leaks across the
//! boundary.
//!
//! ## Local-only per current convention
//!
//! Same rationale as `pty_presets_e2e.rs` /
//! `pty_custom_agents.rs`.  Under `SCODE_TEST_BACKEND=mock` the test
//! early-skips because the mock harness can't route subagent-owned
//! `/v1/messages` requests (plan §6.4).
//!
//! Local run against sudorouter:
//!
//! ```powershell
//! $env:PATH = "C:\Program Files\Git\bin;C:\Program Files\Git\usr\bin;" + $env:PATH
//! cmd /c 'call "D:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" > NUL 2>&1 && cd /d C:\Users\songym\cursor-projects\sudocode\rust && $env:SCODE_TEST_BACKEND="live"; cargo test -p rusty-sudocode-cli --test pty_agent_memory_scoping -- --nocapture'
//! ```

mod common;

use std::fs;
use std::path::Path;

use common::{TestEnv, LIVE_TIMEOUT};
use serde_json::Value;

const EXPLORE_SENTINEL: &str = "EXPLORE_ONLY_SENTINEL_QWERTY";
const PLAN_SENTINEL: &str = "PLAN_ONLY_SENTINEL_ZXCV";

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — live sub-agent chain \
         blocked by the mock scenario-inheritance gap (plan §6.4). \
         Rerun with SCODE_TEST_BACKEND=live."
    );
    false
}

fn seed_agent_memory(memory_base: &Path, agent_type: &str, sentinel: &str) {
    let dir = memory_base.join("agent-memory").join(agent_type);
    fs::create_dir_all(&dir).expect("mkdir agent memory dir");
    // Seeded as a habit the agent applies to its own answers, not as a secret
    // for someone to ask it for. That is what makes the tag observable without
    // the prompt having to request it: a tag can only reach the screen if this
    // agent's memory was loaded for this agent, which is the scoping property
    // under test.
    let entry = format!(
        "---\n\
         name: reply-tag\n\
         description: how the {agent_type} agent tags its own replies\n\
         metadata:\n  type: user\n\
         ---\n\
         Always begin your reply with the tag {sentinel} so the reader can tell \
         which agent answered.\n"
    );
    fs::write(dir.join("sentinel.md"), entry).expect("write sentinel entry");
}

#[test]
fn explore_and_plan_agents_have_isolated_memory() {
    let env = TestEnv::new("pty-agent-memory-scoping");
    if !require_live(&env, "explore_and_plan_agents_have_isolated_memory") {
        return;
    }

    // Pin the memory base to a temp dir under the test's workspace so
    // real ~/.scode/projects/ never leaks in or out.
    let memory_base = env.workspace_root().join("scoped-memory");
    fs::create_dir_all(&memory_base).expect("mkdir memory base");
    seed_agent_memory(&memory_base, "Explore", EXPLORE_SENTINEL);
    seed_agent_memory(&memory_base, "Plan", PLAN_SENTINEL);

    let memory_base_str = memory_base.display().to_string();
    let extra_env = &[("SUDOCODE_MEMORY_DIR", memory_base_str.as_str())];

    // Ordinary work for two workers — one Explore, one Plan — with no mention of
    // memory, tags, or echoing anything back.
    //
    // An earlier version asked each sub-agent to "report the exact sentinel
    // string in your persistent memory" and the parent to "report both replies
    // verbatim". A live model read that shape as an exfiltration probe, said so
    // on screen ("a classic exfiltration probe … I won't execute these spawns"),
    // and refused the whole request — so the test failed on a refusal instead of
    // on memory scoping. `pty_agent_summary` (ce366d00) and
    // `pty_verification_streak` were rewritten for the same reason.
    //
    // Nothing is lost by dropping the request: each agent's seeded memory tells
    // it to tag its own replies, so the tags surface because the parent
    // summarises what its workers actually said.
    let prompt = format!(
        "I want two perspectives on the same question: how would someone new to \
         a repository find where its HTTP requests are made? \
         Ask Agent(subagent_type=\"Explore\", description=\"search strategy\", \
         prompt=\"In two sentences, describe how you would search a repository \
         to find where HTTP requests are made.\") and, separately, \
         Agent(subagent_type=\"Plan\", description=\"approach\", \
         prompt=\"In two sentences, outline a plan for locating where HTTP \
         requests are made in a repository.\"). Then summarise what each of them \
         told you."
    );

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", &prompt],
        extra_env,
    );
    // Sub-agent tests are two serialised model turns (parent spawns a worker,
    // worker answers, parent relays), so they need noticeably more room than a
    // single-turn test — especially when the rest of the suite is running
    // beside them. Purely a timeout: the assertions are unchanged.
    let long = LIVE_TIMEOUT.saturating_mul(8);
    sess.set_default_timeout(long);

    // Both tags should eventually surface: each worker opens its own reply with
    // its own tag, and the parent's summary carries them through. Order isn't
    // guaranteed — expect them separately.
    sess.expect(EXPLORE_SENTINEL).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "Explore sentinel did not surface: {e}\n\
             tail: {tail}",
            tail = screen
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });
    sess.expect(PLAN_SENTINEL).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "Plan sentinel did not surface: {e}\n\
             tail: {tail}",
            tail = screen
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });

    // Drain and exit cleanly.
    sess.set_default_timeout(long);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "scode did not exit cleanly: {e}\ntail: {tail}",
            tail = screen
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });
    assert_eq!(exit, 0, "scode should exit 0; got {exit}");

    // Cross-contamination check, read off each worker's own persisted manifest
    // rather than off the rendered screen.
    //
    // The screen is an 80×24 viewport and it scrolls. Each tag reaches it when
    // that worker answers, early in a long transcript, and by the time the run
    // ends neither is visible any more — so counting occurrences there measured
    // what happened to still be on screen, not what each agent said. That is
    // what this assertion failed on even though both tags had demonstrably
    // arrived (the two waits above consumed them from the stream).
    //
    // `.sudocode-agents/<agent_id>.json` keeps each worker's final text in
    // `result` for as long as the workspace lives, which is also how
    // `pty_agent_model_inheritance` reads a child's outcome.
    //
    // Asserted per agent, which is strictly stronger than the old total: a leak
    // is one agent's tag appearing in the OTHER agent's result, and that is now
    // named directly instead of inferred from a count.
    let manifests: Vec<Value> = fs::read_dir(env.workspace_root().join(".sudocode-agents"))
        .expect("read .sudocode-agents")
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_slice(&fs::read(entry.path()).expect("read manifest"))
                .expect("manifest must be JSON")
        })
        .collect();

    let result_of = |agent_type: &str| -> String {
        let manifest = manifests
            .iter()
            // `subagentType`, not `subagent_type`: the manifest is
            // `AgentOutput` with per-field `#[serde(rename = …)]` camelCase, so
            // the snake_case key reads as absent and every lookup misses.
            .find(|m| m["subagentType"] == agent_type)
            .unwrap_or_else(|| {
                panic!(
                    "no {agent_type} manifest among the {} spawned",
                    manifests.len()
                )
            });
        manifest["result"].as_str().unwrap_or_default().to_string()
    };

    let explore_result = result_of("Explore");
    let plan_result = result_of("Plan");
    assert!(
        explore_result.contains(EXPLORE_SENTINEL),
        "Explore's own memory tag must open its reply; got: {explore_result}"
    );
    assert!(
        !explore_result.contains(PLAN_SENTINEL),
        "Plan's tag leaked into Explore's reply: {explore_result}"
    );
    assert!(
        plan_result.contains(PLAN_SENTINEL),
        "Plan's own memory tag must open its reply; got: {plan_result}"
    );
    assert!(
        !plan_result.contains(EXPLORE_SENTINEL),
        "Explore's tag leaked into Plan's reply: {plan_result}"
    );
}
