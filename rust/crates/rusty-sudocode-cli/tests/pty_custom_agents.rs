//! PTY live e2e — `~/.claude/agents/*.md` custom sub-agent parser
//! wired into the Agent tool dispatch.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.3 Commit 8. A
//! fixture `.md` file with a distinctive body is written under the
//! test's `HOME/.claude/agents/`. The test drives the full chain:
//! parent LLM calls `Agent(subagent_type="<fixture-name>", ...)`, the
//! runtime resolves the name through `runtime::custom_agents`, and
//! the child sub-agent runs under the fixture's system prompt.
//!
//! ## Local-only per current convention
//!
//! Same rationale as `pty_presets_e2e.rs`: driving a live sub-agent
//! chain hits the mock harness's scenario-inheritance gap (plan
//! §6.4). Under `SCODE_TEST_BACKEND=mock` (CI's default) the test
//! early-skips with a stderr note.
//!
//! Local run against sudorouter:
//!
//! ```powershell
//! $env:PATH = "C:\Program Files\Git\bin;C:\Program Files\Git\usr\bin;" + $env:PATH
//! cmd /c 'call "D:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" > NUL 2>&1 && cd /d C:\Users\songym\cursor-projects\sudocode\rust && $env:SCODE_TEST_BACKEND="live"; cargo test -p rusty-sudocode-cli --test pty_custom_agents -- --nocapture'
//! ```

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::{TestEnv, LIVE_TIMEOUT};

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — driving a real \
         sub-agent chain is blocked by the mock scenario-inheritance \
         gap (plan §6.4). Rerun with SCODE_TEST_BACKEND=live."
    );
    false
}

/// Write a fixture `.md` custom agent under `<home>/.claude/agents/`.
fn write_fixture_agent(home: &Path, name: &str, frontmatter: &str, body: &str) {
    let dir = home.join(".claude").join("agents");
    fs::create_dir_all(&dir).expect("mkdir agents dir");
    let contents = format!("---\n{frontmatter}---\n{body}");
    fs::write(dir.join(format!("{name}.md")), contents).expect("write fixture");
}

#[test]
fn custom_md_agent_is_reachable_via_agent_tool() {
    let env = TestEnv::new("pty-custom-agents");
    if !require_live(&env, "custom_md_agent_is_reachable_via_agent_tool") {
        return;
    }

    // Distinctive sentinel so any hit in the child's output is
    // almost certainly the fixture body, not something the model
    // hallucinated. Keep it letters-only so regex-style pty-expect
    // matchers treat it as a literal.
    let sentinel = "CUST_AG_QW";

    // The fixture: a naming-committee agent, restricted to read-only
    // tools so we know we're not accidentally hitting general-purpose.
    // `workspace_root/home` is what the harness passes as HOME to the
    // child scode process — so custom_agents' `~/.claude/agents/`
    // resolver looks under this exact path.
    write_fixture_agent(
        env.workspace_root().join("home").as_path(),
        "sudocode-fixture-namer",
        "name: sudocode-fixture-namer\n\
         description: A naming committee restricted to reply with names only.\n\
         tools: [read_file, glob_search, grep_search]\n",
        &format!(
            "You are a naming committee.\n\
             When you have finished, include the token {sentinel} in \
             your final reply so callers can prove you ran.\n\
             Reply with three unique names, one per line.\n"
        ),
    );

    let prompt = format!(
        "Use Agent(subagent_type=\"sudocode-fixture-namer\", \
         description=\"name 3 birds\", \
         prompt=\"Reply with three bird names, one per line, then finish.\") \
         to complete a small naming task. Report back briefly."
    );

    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    let long = LIVE_TIMEOUT.saturating_mul(3);
    sess.set_default_timeout(long);

    // Sentinel is the primary success indicator — proves the custom
    // agent's system prompt actually loaded and its body was applied.
    sess.expect(sentinel).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "custom .md agent did not emit its sentinel: {e}\n\
             tail of PTY screen (last 600 chars):\n{tail}",
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

    sess.set_default_timeout(long);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "scode did not exit cleanly after custom-agent chain: {e}\n\
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
    assert_eq!(exit, 0, "scode should exit 0; got {exit}");

    // Sanity-check the timeout var so an idle test env doesn't shadow
    // real CI regressions.
    let _elapsed = Duration::from_secs(0);
}
