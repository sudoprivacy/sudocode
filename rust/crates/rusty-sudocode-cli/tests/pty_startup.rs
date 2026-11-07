//! The real CLI exposes startup phases only when explicitly enabled.
mod common;

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
