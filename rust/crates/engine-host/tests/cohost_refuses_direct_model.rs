//! A co-host cannot silently retain an HTTP model route.
mod common;

use std::sync::Arc;

#[test]
fn direct_model_configuration_is_refused_before_an_agent_starts() {
    let home = tempfile::tempdir().unwrap();
    let config = serde_json::json!({
        "auth_modes": {"api-key": {"anthropic": {"baseUrl": "http://127.0.0.1:1", "apiKey": "unused"}}},
        "models": {"test": {"alias": "test", "name": "test", "input": ["text"],
            "providers": {"api-key": {"provider": "anthropic", "model": "claude-sonnet-4-6"}}}}
    });
    std::fs::write(
        home.path().join("sudocode.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    // This test owns its process and config home.
    std::env::set_var("SUDO_CODE_CONFIG_HOME", home.path());
    let kernel = Arc::new(kernel::kernel::Kernel::new());
    common::mount_agent_world(&kernel);
    let result = engine_host::managed_agent::spawn_managed_agent(
        kernel,
        common::make_desc("direct-model", "direct-model", "test"),
        |_, _| {},
    );
    match result {
        Err(message) => assert!(message.contains("nexus:///mount"), "{message}"),
        Ok(handle) => {
            handle.abort_signal.abort();
            handle.join.join().unwrap();
            panic!("co-host accepted an HTTP model route");
        }
    }
}
