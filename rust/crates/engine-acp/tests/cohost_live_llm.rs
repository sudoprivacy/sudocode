#![cfg(feature = "mailbox")]

//! LIVE co-host proof — the same agent, answered by a real model.
//!
//! Drives `managed_harness::spawn_managed_agent` (the EXACT factory
//! the nexusd `SudoCodeSpawnAdapter` calls) on a real in-process `Kernel`:
//! provisions the pair's conversation exactly as a sender would, spawns the
//! managed-agent loop, writes a user prompt into the shared transcript, and
//! reads the agent's reply back out of it. The LLM turn is a real network call
//! through the configured provider.
//!
//! This is the co-host end-to-end minus the gRPC `StartSession` entry + the
//! adapter's enum mapping (both compile+link-verified in nexusd).
//!
//! `#[ignore]` — opt-in, needs a live LLM. The CI gate for the same path is
//! `cohost_mock_llm`, which runs this harness against a scripted model; the two
//! share `common` so a pass there is a statement about this one. Run with:
//!   ANTHROPIC_API_KEY=<sudorouter sk-…> \
//!   ANTHROPIC_BASE_URL=https://napi.sudorouter.ai \
//!   cargo test -p engine-acp --features mailbox --test cohost_live_llm -- --ignored --nocapture

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;
mod managed_harness;

use std::sync::Arc;
use std::time::Duration;

use common::{
    make_desc, mount_agent_world, provision_stream_transcript, send_prompt, user_ctx,
    wait_for_agent_reply,
};
use kernel::kernel::Kernel;
use managed_harness::spawn_managed_agent;
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::{FsBackend, KernelFsBackend};

/// Model the agent runs, read from its descriptor `model` label. SudoRouter
/// serves `claude-sonnet-4-6`; override via `SUDOCODE_TEST_MODEL`.
fn test_model() -> String {
    std::env::var("SUDOCODE_TEST_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string())
}

#[test]
#[ignore = "live LLM: set ANTHROPIC_API_KEY + ANTHROPIC_BASE_URL (sudorouter)"]
fn cohost_agent_replies_via_mailbox_with_real_llm() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("set ANTHROPIC_API_KEY for the live mount");
    let base_url =
        std::env::var("ANTHROPIC_BASE_URL").expect("set ANTHROPIC_BASE_URL for the live mount");
    let config_home = tempfile::tempdir().expect("isolated config home");
    let model = test_model();
    let config = serde_json::json!({
        "auth_modes": {"api-key": {"anthropic": {"baseUrl": "nexus:///model"}}},
        "models": {model.clone(): {"alias": model, "name": "live model", "input": ["text"],
            "providers": {"api-key": {"provider": "anthropic", "model": test_model()}}}}
    });
    std::fs::write(
        config_home.path().join("sudocode.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config_home.path());
    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);
    let _model_storage = common::mount_model(&kernel, "anthropic", &base_url, &key);
    let pid = "cohost-live-1";
    let agent_id = "scode-live";
    let user = "user-test";
    let desc = make_desc(pid, agent_id, &test_model());

    // Provision the conversation the way a real sender does — through a
    // Mailbox over this same kernel — rather than planting entries by hand.
    // Hand-planting is how a test ends up exercising a shape production never
    // creates, and this one is here precisely to exercise the production path.
    let user_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        user,
        "/".to_string(),
    ));
    let transcript = InboxConvention::new(String::new()).transcript_path(user, agent_id);
    provision_stream_transcript(&kernel, &transcript);
    let user_mb = Arc::new(Mailbox::daemon_absolute(user_fs, user.to_string()));
    user_mb
        .ensure_conversation(agent_id)
        .expect("provision the conversation with the co-host");

    // Spawn the REAL managed-agent loop — the same factory nexusd's
    // SudoCodeSpawnAdapter calls, and with the mailbox the factory builds from
    // the descriptor, so there is no second place a test could disagree with
    // the daemon about what the agent listens on. State transitions are
    // printed, so the agent's lifecycle is observable.
    let handle = spawn_managed_agent(Arc::clone(&kernel), desc, |state, reason| {
        eprintln!("[agent state] {state:?} reason={reason:?}");
    })
    .expect("this host can run an agent: the live test supplies a real config home");

    let ctx = user_ctx();
    let prompt = "You are being tested over a nexus A2A mailbox. \
                  Reply with exactly one word: PONG";
    eprintln!("[user → agent] {prompt}");
    send_prompt(&user_mb, agent_id, prompt);

    let reply = wait_for_agent_reply(
        &kernel,
        &transcript,
        &ctx,
        agent_id,
        Duration::from_secs(90),
    );
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("no agent reply within 90s — LLM turn did not complete");
    let body = reply
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    eprintln!("[agent → user] {body}");
    assert!(
        body.to_ascii_uppercase().contains("PONG"),
        "the agent should have answered the prompt; got: {body}"
    );
}
