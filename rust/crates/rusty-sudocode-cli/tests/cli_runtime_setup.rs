//! Real CLI setup must not provision services or submit inference in local mode.
mod common;

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn command(root: &Path) -> Command {
    let mut command = Command::new(common::scode_bin());
    command
        .current_dir(root)
        .env("SCODE_GLOBAL_CONFIG_DIR", root.join("config"))
        .env("SCODE_PROJECT_CONFIG_DIR", root.join("project-config"))
        .env("SUDO_CODE_CONFIG_HOME", root.join("legacy-config"))
        .env("SCODE_SERVICE_ROOT", root.join("services"))
        .env("SCODE_CHECK_UPDATES", "0")
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .stdin(Stdio::null());
    command
}

fn output_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn inspection_preserves_a_fresh_profile_and_absent_runtime() {
    let root = tempfile::tempdir().unwrap();
    let result = output_json(
        &command(root.path())
            .args(["setup", "--check", "--output-format", "json"])
            .output()
            .unwrap(),
    );
    assert!(result["mode"].is_null());
    assert_eq!(result["services"]["running"], false);
    assert_eq!(result["services"]["installation"]["installed"], false);
    assert_eq!(result["domainJoinAccepted"], false);
    assert!(!root.path().join("config").exists());
    assert!(!root.path().join("services").exists());
}

#[test]
fn help_exposes_runtime_setup_without_preparing_services() {
    let root = tempfile::tempdir().unwrap();
    let result = command(root.path()).arg("help").output().unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("scode setup [--mode local|shared]"));
    let setup_help = command(root.path())
        .args(["setup", "--help"])
        .output()
        .unwrap();
    assert!(setup_help.status.success());
    assert!(String::from_utf8_lossy(&setup_help.stdout).contains("--mode"));
    assert!(String::from_utf8_lossy(&setup_help.stdout).contains("--check"));
    assert!(!root.path().join("services").exists());
}

#[test]
fn choosing_local_never_installs_starts_or_calls_a_model() {
    let root = tempfile::tempdir().unwrap();
    let model = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    model.set_nonblocking(true).unwrap();
    let address = format!("http://{}", model.local_addr().unwrap());
    let result = output_json(
        &command(root.path())
            .env("ANTHROPIC_BASE_URL", &address)
            .env("OPENAI_BASE_URL", &address)
            .args(["setup", "--mode", "local", "--output-format", "json"])
            .output()
            .unwrap(),
    );
    assert_eq!(result["mode"], "local");
    assert!(result["service"].is_null());
    assert_eq!(result["domainJoinAccepted"], false);
    let path = root.path().join("config/runtime-profile.json");
    let before = std::fs::read(&path).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(
        saved,
        serde_json::json!({"schemaVersion": 1, "mode": "local"})
    );
    let checked = output_json(
        &command(root.path())
            .args(["setup", "--check", "--output-format", "json"])
            .output()
            .unwrap(),
    );
    assert_eq!(checked["mode"], "local");
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(!root.path().join("services").exists());
    assert!(matches!(model.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn noninteractive_setup_requires_an_explicit_mode() {
    let root = tempfile::tempdir().unwrap();
    let result = command(root.path()).arg("setup").output().unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("Noninteractive setup requires --mode")
    );
    assert!(!root.path().join("services").exists());
    assert!(!root.path().join("config").exists());
}

#[test]
fn unsupported_preferences_are_preserved_without_installation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config/runtime-profile.json");
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    let original = br#"{"schemaVersion":2,"mode":"shared"}"#;
    std::fs::write(&path, original).unwrap();
    let result = command(root.path())
        .args(["setup", "--check"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Unsupported runtime profile version"));
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(!root.path().join("services").exists());
}

fn configured_box(root: &Path, record: &serde_json::Value) -> std::path::PathBuf {
    let path = root.join("home/.nexus/run/node.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, record.to_string()).unwrap();
    path
}

#[test]
fn multi_agent_setup_keeps_an_offline_box_without_installing_a_replacement() {
    let root = tempfile::tempdir().unwrap();
    let record = configured_box(
        root.path(),
        &serde_json::json!({"version": 1, "endpoint": "127.0.0.1:1",
            "agents_dir": root.path().join("existing-agents")}),
    );
    let original = std::fs::read(&record).unwrap();
    let model = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    model.set_nonblocking(true).unwrap();
    let address = format!("http://{}", model.local_addr().unwrap());
    for args in [
        vec!["setup", "--mode", "shared", "--output-format", "json"],
        vec!["setup", "--check", "--output-format", "json"],
    ] {
        let result = output_json(
            &command(root.path())
                .env("ANTHROPIC_BASE_URL", &address)
                .env("OPENAI_BASE_URL", &address)
                .args(args)
                .output()
                .unwrap(),
        );
        assert_eq!(result["mode"], "shared");
        assert_eq!(result["box"]["endpoint"], "127.0.0.1:1");
        assert_eq!(result["box"]["agentAuthentication"], "pending");
        assert_eq!(result["domainJoinAccepted"], false);
        assert!(result["service"].is_null());
        assert!(!root.path().join("services").exists());
        assert_eq!(std::fs::read(&record).unwrap(), original);
    }
    // Setup has no Agent yet: it must neither mint nor borrow another credential.
    assert!(!root.path().join("existing-agents").exists());
    assert!(matches!(model.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn multi_agent_setup_preserves_invalid_box_records_without_provisioning() {
    let root = tempfile::tempdir().unwrap();
    let path = configured_box(root.path(), &serde_json::json!({}));
    for (record, expected) in [
        ("{broken", "parse node record"),
        (
            r#"{"version":2,"endpoint":"127.0.0.1:1","agents_dir":"unused"}"#,
            "this scode reads version 1",
        ),
        (
            r#"{"version":1,"endpoint":"127.0.0.1:1","agents_dir":"relative"}"#,
            "agents_dir must be an absolute path",
        ),
    ] {
        std::fs::write(&path, record).unwrap();
        let result = command(root.path())
            .args(["setup", "--mode", "shared", "--output-format", "json"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        let failure: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert!(
            failure["error"].as_str().unwrap().contains(expected),
            "expected {expected:?}, received {failure}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), record.as_bytes());
        assert!(!root.path().join("config/runtime-profile.json").exists());
        assert!(!root.path().join("services").exists());
    }
}

#[test]
fn interactive_multi_agent_choice_reuses_the_configured_box() {
    let env = common::TestEnv::new("runtime-existing-box");
    let services = env.workspace_root().join("setup-services");
    configured_box(
        env.workspace_root(),
        &serde_json::json!({"version": 1, "endpoint": "127.0.0.1:1",
            "agents_dir": env.workspace_root().join("existing-agents")}),
    );
    let home = env.workspace_root().join("home");
    let mut child = env.spawn_with_env(
        &["setup"],
        &[
            ("SCODE_SERVICE_ROOT", &services.to_string_lossy()),
            ("HOME", &home.to_string_lossy()),
            ("USERPROFILE", &home.to_string_lossy()),
        ],
    );
    child.expect("Choose how to run scode").unwrap();
    child.send("\u{1b}[B\r").unwrap();
    child.expect("Using configured Box at 127.0.0.1:1").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    let preference: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.config_home().join("runtime-profile.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(preference["mode"], "shared");
    assert!(!services.exists());
}

#[test]
fn interactive_setup_defaults_to_local_without_installing_services() {
    let env = common::TestEnv::new("runtime-setup-choice");
    let services = env.workspace_root().join("setup-services");
    let mut child = env.spawn_with_env(
        &["setup"],
        &[("SCODE_SERVICE_ROOT", &services.to_string_lossy())],
    );
    child.expect("Choose how to run scode").unwrap();
    child.send("\r").unwrap();
    child.expect("Runtime preference saved").unwrap();
    let preference: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.config_home().join("runtime-profile.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(preference["mode"], "local");
    assert!(!services.exists());
}

#[test]
fn first_interactive_launch_offers_setup_before_a_session_is_started() {
    let env = common::TestEnv::new("runtime-first-launch");
    let config = env.workspace_root().join("fresh-configuration");
    let services = env.workspace_root().join("first-launch-services");
    let mut child = env.spawn_with_env(
        &[],
        &[
            ("SCODE_GLOBAL_CONFIG_DIR", &config.to_string_lossy()),
            ("SCODE_SERVICE_ROOT", &services.to_string_lossy()),
            ("SCODE_CHECK_UPDATES", "0"),
        ],
    );
    child.expect("Choose how to run scode").unwrap();
    child.send("\r").unwrap();
    child.expect("Runtime preference saved").unwrap();
    let preference: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("runtime-profile.json")).unwrap())
            .unwrap();
    assert_eq!(preference["mode"], "local");
    assert!(!services.exists());
}

#[test]
fn interactive_resume_checks_the_saved_owner_before_opening_a_session() {
    let env = common::TestEnv::new("runtime-resume-owner");
    let services = env.workspace_root().join("unregistered-owner");
    let home = env.workspace_root().join("home");
    configured_box(
        env.workspace_root(),
        &serde_json::json!({"version": 1, "endpoint": "127.0.0.1:1",
            "agents_dir": env.workspace_root().join("existing-agents")}),
    );
    let preference =
        serde_json::json!({"schemaVersion": 1, "mode": "shared", "ownerRoot": services});
    let preference_path = env.config_home().join("runtime-profile.json");
    std::fs::write(&preference_path, preference.to_string()).unwrap();
    let before = std::fs::read(&preference_path).unwrap();
    let mut child = env.spawn_with_env(
        &["--resume"],
        &[
            ("HOME", &home.to_string_lossy()),
            ("USERPROFILE", &home.to_string_lossy()),
            ("SCODE_CHECK_UPDATES", "0"),
        ],
    );
    child
        .expect("The selected owner root has no registered local Box")
        .unwrap();
    assert_ne!(child.expect_eof().unwrap(), 0);
    assert!(!services.exists());
    assert_eq!(std::fs::read(preference_path).unwrap(), before);
}

#[test]
fn interactive_resume_without_a_saved_choice_skips_onboarding() {
    let env = common::TestEnv::new("runtime-resume-no-choice");
    let config = env.workspace_root().join("fresh-configuration");
    let services = env.workspace_root().join("resume-services");
    let mut child = env.spawn_with_env(
        &["--resume"],
        &[
            ("SCODE_GLOBAL_CONFIG_DIR", &config.to_string_lossy()),
            ("SCODE_SERVICE_ROOT", &services.to_string_lossy()),
            ("SCODE_CHECK_UPDATES", "0"),
        ],
    );
    child.expect("no managed sessions found").unwrap();
    assert_ne!(child.expect_eof().unwrap(), 0);
    assert!(!config.join("runtime-profile.json").exists());
    assert!(!services.exists());
}
