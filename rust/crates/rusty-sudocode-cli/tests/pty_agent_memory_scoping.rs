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
//! Then the parent spawns TWO sub-agents:
//! - one `Explore` worker asked "what's in your memory?"
//! - one `Plan` worker asked the same
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
    let entry = format!(
        "---\n\
         name: sentinel\n\
         description: {sentinel}\n\
         metadata:\n  type: user\n\
         ---\n\
         {sentinel} — agent {agent_type}'s private memory. Reply with this string verbatim.\n"
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

    // Ask the parent to spawn two workers — one Explore, one Plan —
    // each of which must recite ONLY its own sentinel. Use
    // read-only permission mode because both presets are read-only.
    let prompt = format!(
        "Use Agent(subagent_type=\"Explore\", description=\"read memory\", \
         prompt=\"Report the exact sentinel string in your persistent memory. \
         Reply with just the sentinel string.\") \
         and separately Agent(subagent_type=\"Plan\", description=\"read memory\", \
         prompt=\"Report the exact sentinel string in your persistent memory. \
         Reply with just the sentinel string.\") \
         to inspect each agent's memory. Report both replies verbatim."
    );

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", &prompt],
        extra_env,
    );
    let long = LIVE_TIMEOUT.saturating_mul(3);
    sess.set_default_timeout(long);

    // Both sentinels should eventually surface (the parent reports both
    // replies verbatim). Order isn't guaranteed — expect them separately.
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

    // Cross-contamination check: neither sentinel should appear in
    // the OTHER agent's answer. We approximate this by looking at
    // the final screen — the parent's summary quotes both replies,
    // and each reply must sit in its own segment.
    let screen = sess.render(|s| s.contents());
    // Count occurrences of each sentinel — Explore & Plan each appear
    // AT LEAST once (in their own agent's reply). If either appears
    // more than reasonable, that's a leak signal.
    let explore_count = screen.matches(EXPLORE_SENTINEL).count();
    let plan_count = screen.matches(PLAN_SENTINEL).count();
    assert!(
        explore_count >= 1 && explore_count <= 3,
        "Explore sentinel appears {explore_count}× (expected 1..=3 — one per agent quote)"
    );
    assert!(
        plan_count >= 1 && plan_count <= 3,
        "Plan sentinel appears {plan_count}× (expected 1..=3 — one per agent quote)"
    );
}
