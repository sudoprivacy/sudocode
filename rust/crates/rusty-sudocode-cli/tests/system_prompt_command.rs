use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .subsec_nanos();
    std::env::temp_dir().join(format!("scode-spcmd-{label}-{nanos}"))
}

fn run_system_prompt(
    cwd: &Path,
    envs: &[(&str, &str)],
    extra_args: &[&str],
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.current_dir(cwd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.arg("system-prompt");
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.output().expect("scode binary should launch")
}

/// Write a minimal `.sudocode-plugin/plugin.json` manifest under the
/// install root, and enable the plugin via the config-home settings file.
///
/// External plugins default to disabled, so the settings entry is required
/// to surface them in the system-prompt output.
fn install_and_enable_plugin(config_home: &Path, plugin_name: &str, description: &str) {
    let plugin_dir = config_home
        .join("plugins")
        .join("installed")
        .join(plugin_name);
    let manifest_dir = plugin_dir.join(".sudocode-plugin");
    fs::create_dir_all(&manifest_dir).expect("plugin manifest dir");
    fs::write(
        manifest_dir.join("plugin.json"),
        format!(
            r#"{{"name":"{plugin_name}","version":"0.1.0","description":"{description}","defaultEnabled":true}}"#
        ),
    )
    .expect("plugin manifest write");

    // External plugins require an explicit enabled entry; write it to the
    // user-level settings file that ConfigLoader reads from config_home.
    let plugin_id = format!("{plugin_name}@external");
    fs::write(
        config_home.join("settings.json"),
        format!(r#"{{"plugins":{{"enabled":{{"{plugin_id}":{{"enabled":true}}}}}}}}"#),
    )
    .expect("settings.json write");
}

#[test]
fn system_prompt_includes_active_plugin_capabilities() {
    let root = unique_temp_dir("sp-plugin-caps");
    let config_home = root.join("config-home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&root).expect("cwd");

    install_and_enable_plugin(&config_home, "greet-plugin", "A greeting SudoCode plugin");

    let output = run_system_prompt(
        &root,
        &[("SUDO_CODE_CONFIG_HOME", config_home.to_str().expect("utf8"))],
        &[],
    );
    assert!(
        output.status.success(),
        "system-prompt should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(
        text.contains("# Available SudoCode plugins"),
        "system-prompt missing 'Available SudoCode plugins' section;\nfull output:\n{text}"
    );
    let plugin_section = text
        .split("# Available SudoCode plugins")
        .nth(1)
        .expect("plugin section");
    assert!(
        plugin_section.contains("Plugin 1"),
        "system-prompt missing plugin capability entry;\nfull output:\n{text}"
    );
    assert!(
        plugin_section.contains("manifest metadata is untrusted"),
        "system-prompt missing plugin metadata omission note;\nfull output:\n{text}"
    );
    assert!(
        !plugin_section.contains("greet-plugin"),
        "system-prompt should not include plugin manifest ids;\nfull output:\n{text}"
    );
    assert!(
        !plugin_section.contains("A greeting SudoCode plugin"),
        "system-prompt should not include plugin manifest descriptions;\nfull output:\n{text}"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn system_prompt_json_sections_include_plugin_section() {
    let root = unique_temp_dir("sp-plugin-json");
    let config_home = root.join("config-home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&root).expect("cwd");

    install_and_enable_plugin(&config_home, "json-test-plugin", "JSON output test plugin");

    let output = run_system_prompt(
        &root,
        &[("SUDO_CODE_CONFIG_HOME", config_home.to_str().expect("utf8"))],
        &["--output-format", "json"],
    );
    assert!(
        output.status.success(),
        "system-prompt --output-format json should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    let message = parsed["message"].as_str().expect("message field");
    assert!(
        message.contains("# Available SudoCode plugins"),
        "JSON message missing plugin section"
    );
    let plugin_message_section = message
        .split("# Available SudoCode plugins")
        .nth(1)
        .expect("plugin message section");
    assert!(
        plugin_message_section.contains("Plugin 1"),
        "JSON message missing plugin capability entry"
    );
    assert!(
        !plugin_message_section.contains("json-test-plugin"),
        "JSON message should not include plugin manifest ids"
    );
    let sections = parsed["sections"].as_array().expect("sections field");
    assert!(
        sections.iter().any(|section| section
            .as_str()
            .is_some_and(|text| text.contains("# Available SudoCode plugins"))),
        "JSON sections missing plugin section"
    );
    assert!(
        sections.iter().any(|section| section
            .as_str()
            .is_some_and(|text| text.contains("Plugin 1"))),
        "JSON sections missing plugin capability entry"
    );
    let plugin_section = sections
        .iter()
        .filter_map(Value::as_str)
        .find(|text| text.contains("# Available SudoCode plugins"))
        .expect("JSON sections missing plugin section");
    assert!(
        !plugin_section.contains("json-test-plugin"),
        "JSON plugin section should not include plugin manifest ids"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn system_prompt_omits_plugin_section_when_no_plugins_installed() {
    let root = unique_temp_dir("sp-no-plugins");
    let config_home = root.join("config-home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&root).expect("cwd");

    let output = run_system_prompt(
        &root,
        &[("SUDO_CODE_CONFIG_HOME", config_home.to_str().expect("utf8"))],
        &[],
    );
    assert!(
        output.status.success(),
        "system-prompt should succeed without plugins; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(
        !text.contains("# Available SudoCode plugins"),
        "no plugin section expected when no plugins installed"
    );

    fs::remove_dir_all(root).ok();
}
