//! A co-host that was installed without a session driver names the protocol it
//! does not speak.
//!
//! The service supplies a `SessionEndpoint` whenever a spawn provider exists and
//! then checks that the handle reports the same one back, so a runtime with no
//! driver must refuse at the call instead of returning a handle with no
//! endpoint: that path still ends in a refusal, one layer later, worded as the
//! symptom ("runtime did not attach the session mailbox") rather than the cause.
//!
//! Covered here because the live harness cannot reach it - the daemon binary
//! always installs a driver, so only a host built without one exercises this.
mod common;

use std::sync::Arc;

use managed_agent::{SpawnOptions, SpawnTask};

#[test]
fn a_session_request_is_refused_by_name_when_no_driver_is_installed() {
    let kernel = Arc::new(kernel::kernel::Kernel::new());
    common::mount_agent_world(&kernel);

    let endpoint = a2a::session::SessionEndpoint::new(
        "driverless-agent".to_string(),
        "controller".to_string(),
        "generation-1".to_string(),
    );
    let adapter = engine_host::managed_agent::SudoCodeSpawnAdapter::new();
    let result = adapter.spawn_with_options(
        kernel,
        common::make_desc("driverless-agent", "driverless-agent", "test"),
        SpawnOptions {
            resume_session_id: None,
            session_endpoint: Some(endpoint),
        },
        Arc::new(|_, _| {}),
    );
    match result {
        Err(refusal) => assert!(
            refusal.contains("acp-mailbox/1"),
            "the refusal must name the protocol, got: {refusal}"
        ),
        Ok(handle) => {
            handle.abort();
            panic!("a runtime with no session driver accepted an attachment");
        }
    }
}
