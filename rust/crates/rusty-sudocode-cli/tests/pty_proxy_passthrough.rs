//! PTY tests for proxy model passthrough — verifies that models
//! NOT registered in sudocode.json can be used via proxy (sudorouter)
//! when proxy auth is configured.
//!
//! These are live-only tests — passthrough requires a real proxy
//! provider to route unknown model IDs.
//!
//! ```bash
//! SCODE_TEST_BACKEND=live cargo test --test pty_proxy_passthrough
//! ```

mod common;

use std::time::Duration;

use common::{spawn_scode_in_dir, HarnessWorkspace, TestEnv, DEFAULT_TIMEOUT};

/// Returns true if a proxy provider is configured (live mode with
/// sudorouter). Passthrough tests only make sense in this mode.
fn has_proxy_config() -> bool {
    let env = TestEnv::new("proxy-check");
    env.is_live()
}

// ──────────────────────────────────────────────────────────────────────
// 1. Unconfigured model works via proxy passthrough
// ──────────────────────────────────────────────────────────────────────

/// `scode --model doubao-seed-1-6-251015 --auth proxy "What is 2+2?"` — a model
/// NOT in sudocode.json. Proxy passthrough sends it to sudorouter.
#[test]
fn unconfigured_model_works_via_proxy() {
    if !has_proxy_config() {
        return;
    }

    let workspace = HarnessWorkspace::new("passthrough-qwen");
    let mut sess = spawn_scode_in_dir(
        &workspace.root,
        &[
            "--model",
            "doubao-seed-1-6-251015",
            "--auth",
            "proxy",
            "--compact",
            "--permission-mode",
            "read-only",
            "What is 2+2? Answer with just the number.",
        ],
        Duration::from_secs(60),
    )
    .expect("spawn scode with unconfigured model");

    sess.set_default_timeout(Duration::from_secs(60));
    sess.expect("4").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("doubao-seed-1-6-251015 should respond with 4: {e}\nPTY screen:\n{screen}");
    });

    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 2. Another unconfigured model (o3-mini)
// ──────────────────────────────────────────────────────────────────────

/// Same passthrough test with a different model family.
#[test]
fn another_unconfigured_model_via_proxy() {
    if !has_proxy_config() {
        return;
    }

    let workspace = HarnessWorkspace::new("passthrough-o3");
    let mut sess = spawn_scode_in_dir(
        &workspace.root,
        &[
            "--model",
            "o3-mini",
            "--auth",
            "proxy",
            "--compact",
            "--permission-mode",
            "read-only",
            "What is 3+3? Answer with just the number.",
        ],
        Duration::from_secs(60),
    )
    .expect("spawn scode with o3-mini");

    sess.set_default_timeout(Duration::from_secs(60));
    sess.expect("6").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("o3-mini should respond with 6: {e}\nPTY screen:\n{screen}");
    });

    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 3. Configured model still works (no regression)
// ──────────────────────────────────────────────────────────────────────

/// Models in sudocode.json still route through config, not passthrough.
#[test]
fn configured_model_still_works() {
    let env = TestEnv::new("proxy-configured");

    let prompt = env.prompt(
        "What is 2+2? Answer with just the number.",
        "single_turn_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    sess.set_default_timeout(Duration::from_secs(30));
    // Mock returns "4" (SingleTurnText scenario), live returns "4" too.
    sess.expect("4")
        .expect("configured model should respond with 4");

    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}
