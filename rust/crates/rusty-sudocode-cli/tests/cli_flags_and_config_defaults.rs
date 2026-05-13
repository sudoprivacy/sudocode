use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::Session;
use serde_json;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn status_command_applies_model_and_permission_mode_flags() {
    // given
    let temp_dir = unique_temp_dir("status-flags");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");

    // when
    let output = Command::new(env!("CARGO_BIN_EXE_scode"))
        .current_dir(&temp_dir)
        .args([
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
            "status",
        ])
        .output()
        .expect("scode should launch");

    // then
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Status"));
    assert!(stdout.contains("Model            claude-sonnet-4-6"));
    assert!(stdout.contains("Permission mode  read-only"));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

#[test]
fn resume_flag_loads_a_saved_session_and_dispatches_status() {
    // given
    let temp_dir = unique_temp_dir("resume-status");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let session_path = write_session(&temp_dir, "resume-status");

    // when
    let output = Command::new(env!("CARGO_BIN_EXE_scode"))
        .current_dir(&temp_dir)
        .args([
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/status",
        ])
        .output()
        .expect("scode should launch");

    // then
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Status"));
    assert!(stdout.contains("Messages         1"));
    assert!(stdout.contains("Session          "));
    assert!(stdout.contains(session_path.to_str().expect("utf8 path")));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

#[test]
fn slash_command_names_match_known_commands_and_suggest_nearby_unknown_ones() {
    // given
    let temp_dir = unique_temp_dir("slash-dispatch");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");

    // when
    let help_output = Command::new(env!("CARGO_BIN_EXE_scode"))
        .current_dir(&temp_dir)
        .arg("/help")
        .output()
        .expect("scode should launch");
    let unknown_output = Command::new(env!("CARGO_BIN_EXE_scode"))
        .current_dir(&temp_dir)
        .arg("/zstats")
        .output()
        .expect("scode should launch");

    // then
    assert_success(&help_output);
    let help_stdout = String::from_utf8(help_output.stdout).expect("stdout should be utf8");
    assert!(help_stdout.contains("Interactive slash commands:"));
    assert!(help_stdout.contains("/status"));

    assert!(
        !unknown_output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&unknown_output.stdout),
        String::from_utf8_lossy(&unknown_output.stderr)
    );
    let stderr = String::from_utf8(unknown_output.stderr).expect("stderr should be utf8");
    assert!(stderr.contains("unknown slash command outside the REPL: /zstats"));
    assert!(stderr.contains("Did you mean"));
    assert!(stderr.contains("/status"));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

#[test]
fn omc_namespaced_slash_commands_surface_a_targeted_compatibility_hint() {
    let temp_dir = unique_temp_dir("slash-dispatch-omc");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");

    let output = Command::new(env!("CARGO_BIN_EXE_scode"))
        .current_dir(&temp_dir)
        .arg("/oh-my-claudecode:hud")
        .output()
        .expect("scode should launch");

    assert!(
        !output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr should be utf8");
    assert!(stderr.contains("unknown slash command outside the REPL: /oh-my-claudecode:hud"));
    assert!(stderr.contains("Sudo Code/OMC plugin command"));
    assert!(stderr.contains("does not yet load plugin slash commands"));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

#[test]
fn config_command_loads_defaults_from_standard_config_locations() {
    // given
    let temp_dir = unique_temp_dir("config-defaults");
    let config_home = temp_dir.join("home").join(".nexus").join("sudocode");
    fs::create_dir_all(temp_dir.join(".nexus").join("sudocode"))
        .expect("project config dir should exist");
    fs::create_dir_all(&config_home).expect("home config dir should exist");

    fs::write(config_home.join("settings.json"), r#"{"model":"haiku"}"#)
        .expect("write user settings");
    fs::write(temp_dir.join(".scode.json"), r#"{"model":"sonnet"}"#)
        .expect("write project settings");
    fs::write(
        temp_dir
            .join(".nexus")
            .join("sudocode")
            .join("settings.local.json"),
        r#"{"model":"opus"}"#,
    )
    .expect("write local settings");
    let session_path = write_session(&temp_dir, "config-defaults");

    // when
    let output = command_in(&temp_dir)
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .args([
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/config",
            "model",
        ])
        .output()
        .expect("scode should launch");

    // then
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Config"));
    assert!(stdout.contains("Loaded files      3"));
    assert!(stdout.contains("Merged section: model"));
    assert!(stdout.contains("opus"));
    assert!(stdout.contains(
        config_home
            .join("settings.json")
            .to_str()
            .expect("utf8 path")
    ));
    assert!(stdout.contains(temp_dir.join(".scode.json").to_str().expect("utf8 path")));
    assert!(stdout.contains(
        temp_dir
            .join(".nexus")
            .join("sudocode")
            .join("settings.local.json")
            .to_str()
            .expect("utf8 path")
    ));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

#[test]
fn doctor_command_runs_as_a_local_shell_entrypoint() {
    // given
    let temp_dir = unique_temp_dir("doctor-entrypoint");
    let config_home = temp_dir.join("home").join(".nexus").join("sudocode");
    fs::create_dir_all(&config_home).expect("config home should exist");

    // when
    let output = command_in(&temp_dir)
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .arg("doctor")
        .output()
        .expect("scode doctor should launch");

    // then
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf8");
    assert!(stdout.contains("Doctor"));
    assert!(stdout.contains("Auth"));
    assert!(stdout.contains("Config"));
    assert!(stdout.contains("Workspace"));
    assert!(stdout.contains("Sandbox"));
    assert!(!stdout.contains("Thinking"));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

#[test]
fn local_subcommand_help_does_not_fall_through_to_runtime_or_provider_calls() {
    let temp_dir = unique_temp_dir("subcommand-help");
    let config_home = temp_dir.join("home").join(".nexus").join("sudocode");
    fs::create_dir_all(&config_home).expect("config home should exist");

    let doctor_help = command_in(&temp_dir)
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .args(["doctor", "--help"])
        .output()
        .expect("doctor help should launch");
    let status_help = command_in(&temp_dir)
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
        .args(["status", "--help"])
        .output()
        .expect("status help should launch");

    // clap handles --help: it prints to stdout and exits 0.
    assert_success(&doctor_help);
    let doctor_stdout = String::from_utf8(doctor_help.stdout).expect("stdout should be utf8");
    assert!(
        doctor_stdout.contains("local-only health report"),
        "doctor --help should contain subcommand description: {doctor_stdout}"
    );
    assert!(!doctor_stdout.contains("Thinking"));

    assert_success(&status_help);
    let status_stdout = String::from_utf8(status_help.stdout).expect("stdout should be utf8");
    assert!(
        status_stdout.contains("Status"),
        "status --help should contain help topic header: {status_stdout}"
    );
    assert!(!status_stdout.contains("Thinking"));

    let doctor_stderr = String::from_utf8(doctor_help.stderr).expect("stderr should be utf8");
    let status_stderr = String::from_utf8(status_help.stderr).expect("stderr should be utf8");
    assert!(!doctor_stderr.contains("auth_unavailable"));
    assert!(!status_stderr.contains("auth_unavailable"));

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

/// Verify that informational commands (help, version, config) succeed without
/// any credentials present. This guards against regressions where an
/// `ensure_authenticated()` call is accidentally placed before the informational
/// dispatch in `run()`.
#[test]
fn informational_commands_bypass_credential_check() {
    let temp_dir = unique_temp_dir("informational-no-creds");
    let config_home = temp_dir.join("home").join(".nexus").join("sudocode");
    fs::create_dir_all(&config_home).expect("config home should exist");

    // A helper that strips credentials from the environment and points the
    // network at a port that refuses connections (127.0.0.1:9), so any
    // accidental auth attempt either errors immediately or hangs (which the
    // test process timeout would surface).
    let no_creds = |args: &[&str]| {
        command_in(&temp_dir)
            .env("SUDO_CODE_CONFIG_HOME", &config_home)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
            .env_remove("PROXY_AUTH_TOKEN")
            .env("ANTHROPIC_BASE_URL", "http://127.0.0.1:9")
            .args(args)
            .output()
            .expect("scode should launch")
    };

    // scode help --output-format json
    let help_json = no_creds(&["help", "--output-format", "json"]);
    assert_success(&help_json);
    let help_stdout = String::from_utf8(help_json.stdout).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&help_stdout)
        .expect("help --output-format json should emit valid JSON");
    assert_eq!(parsed["kind"], "help");

    // scode version --output-format json
    let version_json = no_creds(&["version", "--output-format", "json"]);
    assert_success(&version_json);
    let version_stdout = String::from_utf8(version_json.stdout).expect("utf8");
    let parsed: serde_json::Value = serde_json::from_str(&version_stdout)
        .expect("version --output-format json should emit valid JSON");
    assert_eq!(parsed["kind"], "version");

    // scode config --output-format json
    let config_json = no_creds(&["config", "--output-format", "json"]);
    assert_success(&config_json);
    let config_stdout = String::from_utf8(config_json.stdout).expect("utf8");
    let _parsed: serde_json::Value = serde_json::from_str(&config_stdout)
        .expect("config --output-format json should emit valid JSON");

    fs::remove_dir_all(temp_dir).expect("cleanup temp dir");
}

fn command_in(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scode"));
    command.current_dir(cwd);
    command
}

fn write_session(root: &Path, label: &str) -> PathBuf {
    let session_path = root.join(format!("{label}.jsonl"));
    let mut session = Session::new().with_workspace_root(root.to_path_buf());
    session
        .push_user_text(format!("session fixture for {label}"))
        .expect("session write should succeed");
    session
        .save_to_path(&session_path)
        .expect("session should persist");
    session_path
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
