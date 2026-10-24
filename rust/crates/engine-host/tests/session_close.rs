//! Retained host references must preserve reads while releasing write ownership.
use engine_core::EngineDelegate;
use engine_host::{SessionEngine, SessionLifecycle};

#[test]
fn closed_engine_releases_native_writer_and_fences_retained_snapshots() {
    let workspace = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config.path());
    // Node discovery must use this fixture's home, not the developer's node.
    std::env::set_var("HOME", config.path());
    std::env::set_var("USERPROFILE", config.path());
    std::fs::write(
        config.path().join("sudocode.json"),
        serde_json::json!({
            "auth_modes": {"api-key": {"anthropic": {
                "baseUrl": "http://127.0.0.1:1", "apiKey": "isolated-close-test"
            }}},
            "models": {"claude-sonnet": {
                "alias": "claude-sonnet", "name": "Test Sonnet", "input": ["text"],
                "providers": {"api-key": {"provider": "anthropic", "model": "claude-sonnet-4-6"}}
            }}
        })
        .to_string(),
    )
    .unwrap();
    let engine = SessionEngine::build(
        workspace.path(),
        &Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        "claude-sonnet".into(),
        Some("claude-sonnet".into()),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let handle = engine.session_handle();
    let mut retained = engine.session_snapshot();
    let before = std::fs::read(&handle.path).unwrap();
    assert!(engine_host::session::delete_managed_session(&handle.path)
        .unwrap_err()
        .to_string()
        .contains("session_writer_busy"));
    engine.close();
    engine.close();
    assert!(retained
        .push_user_text("late callback")
        .unwrap_err()
        .to_string()
        .contains("session_closed"));
    assert_eq!(std::fs::read(&handle.path).unwrap(), before);
    engine_host::session::delete_managed_session(&handle.path).unwrap();
    assert!(!handle.path.exists());
    assert!(retained.save_to_path(&handle.path).is_err());
    assert!(
        !handle.path.exists(),
        "a closed snapshot resurrected the transcript"
    );
    drop(engine);
}
