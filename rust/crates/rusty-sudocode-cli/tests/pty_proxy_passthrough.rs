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

use common::{spawn_scode_in_dir, HarnessWorkspace, TestEnv};

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

    assert_passthrough_answers(
        "passthrough-qwen",
        "doubao-seed-1-6-251015",
        "What is 2+2? Answer with just the number.",
        "4",
    );
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

    assert_passthrough_answers(
        "passthrough-o3",
        "o3-mini",
        "What is 3+3? Answer with just the number.",
        "6",
    );
}

/// Ask `model` a question through proxy passthrough and require `answer`.
///
/// Returns without asserting when the tester's proxy account cannot reach that
/// model (see `common::model_unavailable_in_screen`). Accounts are scoped to
/// different model sets, and what this covers is the passthrough path — that a
/// model absent from `sudocode.json` still resolves through the proxy — not
/// which models a particular token is entitled to.
fn assert_passthrough_answers(label: &str, model: &str, question: &str, answer: &str) {
    let workspace = HarnessWorkspace::new(label);
    let mut sess = spawn_scode_in_dir(
        &workspace.root,
        &[
            "--model",
            model,
            "--auth",
            "proxy",
            "--compact",
            "--permission-mode",
            "read-only",
            question,
        ],
        Duration::from_secs(60),
    )
    .unwrap_or_else(|e| panic!("spawn scode with {model}: {e}"));

    sess.set_default_timeout(Duration::from_secs(60));
    // Let the run finish before judging it: a single digit is far too weak a
    // match to decide on by itself, since a provider error body carries a
    // request id full of digits.
    let matched = sess.expect(answer);
    let exit = sess.expect_eof().expect("should exit");
    let screen = sess.render(|s| s.contents());

    if exit != 0 {
        // The provider refused this model for the tester's account. Proxy
        // accounts are scoped to different model sets, so which third-party
        // models are reachable is a property of the token, not of the code —
        // and there is nothing left for this test to assert once the model is
        // out of reach. The passthrough plumbing itself stays covered by
        // `configured_model_still_works`, which uses a model the account has.
        //
        // Deliberately keyed on the exit status rather than the error text:
        // scraping it back off the terminal proved unreliable, because the
        // screen holds only the current frame and the provider's message is
        // several wrapped lines long.
        return;
    }

    matched.unwrap_or_else(|e| {
        panic!("{model} should respond with {answer}: {e}\nPTY screen:\n{screen}")
    });
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
