//! A co-host with no sudocode configuration REFUSES, and says what is missing.
//!
//! This is the first thing a fresh daemon hits: an agent resolves its model and
//! credentials the way the CLI does, from the config home plus the daemon's working
//! directory, and a daemon that has neither cannot run one. That used to be an
//! `expect` — so `start_session_v1` killed a daemon thread and printed a backtrace
//! about a missing file, where an RPC error naming it belongs.
//!
//! Its own test binary on purpose: `SCODE_GLOBAL_CONFIG_DIR` is process-global, so a
//! test that needs it EMPTY cannot share a process with the ones that need it
//! populated.

mod common;

use std::sync::Arc;

use common::{make_desc, mount_agent_world};
use engine_host::managed_agent::spawn_managed_agent;
use kernel::kernel::Kernel;

#[test]
fn a_cohost_without_sudocode_configuration_refuses_and_names_what_is_missing() {
    let empty_home = tempfile::Builder::new()
        .prefix("cohost-no-config-")
        .tempdir()
        .expect("config home");
    // SAFETY-ish: this binary runs exactly one test, so nothing else observes the
    // process environment while it is set.
    std::env::set_var("SCODE_GLOBAL_CONFIG_DIR", empty_home.path());

    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);

    // `SpawnHandle` is not `Debug`, so match rather than `expect_err`.
    let refusal = match spawn_managed_agent(
        Arc::clone(&kernel),
        make_desc("pid-no-config", "unconfigured-agent", "claude-sonnet"),
        |_, _| {},
    ) {
        Err(reason) => reason,
        Ok(_) => panic!("a host with no sudocode configuration must refuse to spawn an agent"),
    };

    // What an operator needs from it: which side is misconfigured, and where the
    // configuration is looked for. Asserting the substance rather than the sentence.
    assert!(
        refusal.contains("co-host"),
        "the refusal must say which side could not start: {refusal}"
    );
    assert!(
        refusal.contains("SCODE_GLOBAL_CONFIG_DIR"),
        "the refusal must name where configuration is read from: {refusal}"
    );
    assert!(
        !refusal.to_lowercase().contains("panic"),
        "it must be a refusal, not a panic report: {refusal}"
    );
}
