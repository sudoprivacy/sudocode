//! The real CLI exposes startup phases only when explicitly enabled.
mod common;

#[test]
fn shared_fixtures_keep_lazy_caller_binaries_and_isolated_peer_workspaces() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIRST_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SECOND_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn first_binary() -> std::path::PathBuf {
        FIRST_CALLS.fetch_add(1, Ordering::SeqCst);
        common::scode_bin()
    }

    fn second_binary() -> std::path::PathBuf {
        SECOND_CALLS.fetch_add(1, Ordering::SeqCst);
        common::scode_bin()
    }

    let first = scode_test_support::TestEnv::new_mock("shared-fixture-first", first_binary);
    let peer = first.peer("shared-fixture-peer");
    let second = scode_test_support::TestEnv::new_mock("shared-fixture-second", second_binary);
    assert_eq!(FIRST_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(SECOND_CALLS.load(Ordering::SeqCst), 0);

    let marker = "fixture-owned-state";
    std::fs::write(first.config_home().join(marker), "first only").unwrap();
    for other in [&peer, &second] {
        assert_ne!(first.workspace_root(), other.workspace_root());
        assert_ne!(first.config_home(), other.config_home());
        assert!(!other.config_home().join(marker).exists());
    }
    assert_ne!(peer.workspace_root(), second.workspace_root());

    for env in [&first, &peer, &second] {
        let mut child = env.spawn_with_env(&["version"], &[]);
        child.expect("Sudo Code").unwrap();
        assert_eq!(child.expect_eof().unwrap(), 0);
        assert_eq!(env.captured_message_count(), 0);
    }
    assert_eq!(FIRST_CALLS.load(Ordering::SeqCst), 2);
    assert_eq!(SECOND_CALLS.load(Ordering::SeqCst), 1);
}

#[cfg(windows)]
#[test]
fn native_startup_answers_conpty_before_its_device_attributes_timeout() {
    let env = common::TestEnv::new("startup-conpty-handshake");
    let started = std::time::Instant::now();
    let mut child = env.spawn_with_env(&["version"], &[]);
    child.expect("Sudo Code").unwrap();
    let elapsed = started.elapsed();
    assert_eq!(child.expect_eof().unwrap(), 0);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "ConPTY device attributes handshake delayed native startup: {elapsed:?}"
    );
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn version_reports_opt_in_startup_phases_without_loading_an_agent() {
    let env = common::TestEnv::new("startup-timing");
    let mut child = env.spawn_with_env(&["version"], &[("SCODE_TRACE_STARTUP", "1")]);
    child.expect("startup phase=console elapsed_us=").unwrap();
    child.expect("startup phase=clap elapsed_us=").unwrap();
    child
        .expect("startup phase=permission_config elapsed_us=")
        .unwrap();
    child
        .expect("startup phase=legacy_config elapsed_us=")
        .unwrap();
    child
        .expect("startup phase=version_output elapsed_us=")
        .unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}
