//! The real CLI exposes startup phases only when explicitly enabled.
mod common;

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
