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

/// The agent-type catalog must reach the model through the prompt, because
/// `agent_spawn`'s description no longer names a single type — if this section
/// goes missing the model has nothing to pass as `agent`.
///
/// Runs the real binary so the whole chain is covered: `print_system_prompt` →
/// `commands::cwd_prompt_sections` → `runtime::agent_types`. That shared helper
/// is also what the live runtime extends, which is what keeps this preview
/// honest about what a session sends.
#[test]
fn system_prompt_carries_the_agent_type_catalog() {
    let root = unique_temp_dir("sp-agent-types");
    let config_home = root.join("config-home");
    fs::create_dir_all(&config_home).expect("config home");
    let agents_dir = root.join(".sudocode").join("agents");
    fs::create_dir_all(&agents_dir).expect("agents dir");
    fs::write(
        agents_dir.join("repo-scout.md"),
        "---\nname: repo-scout\ndescription: Maps an unfamiliar repository.\ntools: [read_file, grep_search]\n---\nBe brief.\n",
    )
    .expect("custom agent write");

    let env = [("SUDO_CODE_CONFIG_HOME", config_home.to_str().expect("utf8"))];
    let output = run_system_prompt(&root, &env, &[]);
    assert!(
        output.status.success(),
        "system-prompt should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("stdout utf8");

    assert!(
        text.contains("<available-agent-types>") && text.contains("</available-agent-types>"),
        "prompt must carry the agent-type section;\nfull output:\n{text}"
    );
    // Built-ins, in CC's `- <type>: <whenToUse> (Tools: …)` line format.
    for needle in [
        "- general-purpose: ",
        "- Explore: ",
        "- Plan: ",
        "- Verification: ",
        "- scode-guide: ",
        "- statusline-setup: ",
        "(Tools: *)",
    ] {
        assert!(
            text.contains(needle),
            "prompt must carry {needle:?};\nfull output:\n{text}"
        );
    }
    // A project-local `.md` agent joins the same catalog, tools column and all.
    assert!(
        text.contains(
            "- repo-scout: Maps an unfamiliar repository. (Tools: grep_search, read_file)"
        ),
        "prompt must list the project's custom agent;\nfull output:\n{text}"
    );
    // `fork` is a spawn mode, not a type to choose between — registered, but
    // never offered in the catalog.
    assert!(
        !text.contains("- fork: "),
        "`fork` must not be listed as an agent type;\nfull output:\n{text}"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn system_prompt_carries_no_plugin_section_and_no_manifest_metadata() {
    // The anonymised `# Available SudoCode plugins` inventory was removed: every
    // capability it summarised now reaches the model through a channel that
    // names it (plugin tools and MCP tools appear in the tool list, skills in
    // the skill listing), so the section only spent tokens restating them.
    //
    // The property it was protecting still matters and is asserted directly:
    // an enabled plugin's manifest name and description must not appear
    // anywhere in the prompt, in either output format.
    let root = unique_temp_dir("sp-plugin-none");
    let config_home = root.join("config-home");
    fs::create_dir_all(&config_home).expect("config home");
    fs::create_dir_all(&root).expect("cwd");

    install_and_enable_plugin(&config_home, "greet-plugin", "A greeting SudoCode plugin");
    let env = [("SUDO_CODE_CONFIG_HOME", config_home.to_str().expect("utf8"))];

    let output = run_system_prompt(&root, &env, &[]);
    assert!(
        output.status.success(),
        "system-prompt should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("stdout utf8");
    for needle in [
        "# Available SudoCode plugins",
        "greet-plugin",
        "A greeting SudoCode plugin",
        "manifest metadata is untrusted",
    ] {
        assert!(
            !text.contains(needle),
            "system-prompt should not contain {needle:?};\nfull output:\n{text}"
        );
    }

    let output = run_system_prompt(&root, &env, &["--output-format", "json"]);
    assert!(
        output.status.success(),
        "system-prompt --output-format json should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    let message = parsed["message"].as_str().expect("message field");
    let sections = parsed["sections"].as_array().expect("sections field");
    for needle in [
        "# Available SudoCode plugins",
        "greet-plugin",
        "A greeting SudoCode plugin",
    ] {
        assert!(!message.contains(needle), "JSON message leaked {needle:?}");
        assert!(
            !sections
                .iter()
                .filter_map(Value::as_str)
                .any(|section| section.contains(needle)),
            "JSON sections leaked {needle:?}"
        );
    }

    fs::remove_dir_all(root).ok();
}
