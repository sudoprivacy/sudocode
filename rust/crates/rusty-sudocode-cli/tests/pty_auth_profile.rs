//! PTY test: `/config set auth_profile <name>` persists the per-project account
//! selector through the single, unified SSOT config writer.
//!
//! Real user journey (data flows across steps): in a project the user selects
//! which named account (defined once in the global `sudocode.json`) this project
//! should use. The selection is written to the project's
//! `.nexus/sudocode/settings.local.json` via the one scope-aware, file-backed
//! writer (`tools::set_config_setting`) — not a session-only, divergent
//! in-memory toggle. We then read the file back to prove it truly landed on disk.
//!
//! This is PTY-only: `/config set` runs in the interactive async REPL. It uses
//! the mock backend, so it needs no real credentials.
//!
//! ```bash
//! cargo test --test pty_auth_profile
//! ```

mod common;

use std::fs;
use std::time::Duration;

use common::TestEnv;

/// `/config set auth_profile <name>` confirms in the REPL AND persists the value
/// to the project's `settings.local.json` (the scope-appropriate SSOT file).
#[test]
fn config_set_auth_profile_persists_to_settings_local() {
    let env = TestEnv::new("auth-profile-set");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(Duration::from_secs(20));

    sess.expect("❯").expect("async REPL prompt");

    // Step 1: select a named account for this project.
    sess.send("/config set auth_profile client-acct\r")
        .expect("send /config set auth_profile");

    // The unified SSOT writer echoes the persisted `key = value`. A session-only
    // toggle or unknown-key path would instead say "Unknown config key" / error,
    // so this line is proof the write went through `tools::set_config_setting`.
    sess.expect("auth_profile = client-acct").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should confirm auth_profile persisted: {e}\nPTY screen:\n{screen}");
    });

    sess.expect("❯").expect("prompt after set");
    sess.send("/exit\r").expect("send exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);

    // Step 2: the value must actually be on disk in the scope-appropriate file,
    // proving it went through the file-backed SSOT writer (not memory-only).
    let settings_local = env
        .workspace_root()
        .join(".nexus")
        .join("sudocode")
        .join("settings.local.json");
    let content = fs::read_to_string(&settings_local).unwrap_or_else(|e| {
        panic!(
            "settings.local.json should exist at {}: {e}",
            settings_local.display()
        )
    });
    assert!(
        content.contains("auth_profile") && content.contains("client-acct"),
        "settings.local.json must contain the persisted auth_profile selector; got:\n{content}"
    );
}

/// With `auth_profile` set, the resolver — surfaced via `scode doctor`'s Account
/// check — selects the *named* proxy account, not the default first one. This is
/// the payoff of the whole feature: a project points at its own account by name,
/// and the credential still lives once in the global `sudocode.json`.
#[test]
fn doctor_resolves_selected_account_when_auth_profile_set() {
    let env = TestEnv::new("auth-profile-resolve-selected");
    write_two_account_config(&env);
    // The project selects its own account by name.
    write_auth_profile(&env, "team-b");

    let mut sess = env.spawn(&["doctor"]);
    sess.set_default_timeout(Duration::from_secs(30));

    // Only the Account check prints `account=<resolved>`, so matching
    // `account=team-b` proves the selector actually drove resolution.
    sess.expect("account=team-b").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("doctor should resolve auth_profile=team-b: {e}\nPTY screen:\n{screen}");
    });
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("doctor exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

/// Without `auth_profile`, the resolver keeps the pre-existing behavior — the
/// first configured proxy account — so the default path is unchanged
/// (regression guard for the "credentials untouched by default" promise).
#[test]
fn doctor_defaults_to_first_account_without_auth_profile() {
    let env = TestEnv::new("auth-profile-resolve-default");
    write_two_account_config(&env);
    // No auth_profile persisted — exercise the default resolution path.

    let mut sess = env.spawn(&["doctor"]);
    sess.set_default_timeout(Duration::from_secs(30));

    // First account is `sudorouter` (from the sample); its base_url is distinct
    // from team-b's, so this proves the default did NOT pick the added account.
    sess.expect("account=sudorouter").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("doctor should default to the first account: {e}\nPTY screen:\n{screen}");
    });
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("doctor exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

/// Write a global `sudocode.json` with two named proxy accounts (the real sample
/// plus an added `team-b`) into the test's config home. Using the sample keeps
/// the file valid; we only add one account so selection has something to choose.
fn write_two_account_config(env: &TestEnv) {
    let mut config: serde_json::Value =
        serde_json::from_str(runtime::SAMPLE_SUDOCODE_JSON).expect("sample sudocode.json parses");
    config["auth_modes"]["proxy"]["team-b"] = serde_json::json!({
        "baseUrl": "http://team-b.test",
        "apiKey": "test-key-team-b",
    });
    let serialized = serde_json::to_string_pretty(&config).expect("serialize sudocode.json");
    fs::write(env.config_home().join("sudocode.json"), serialized)
        .expect("write two-account sudocode.json");
}

/// Persist a project-scoped `auth_profile` selector to the same
/// `settings.local.json` that `/config set auth_profile` writes to.
fn write_auth_profile(env: &TestEnv, profile: &str) {
    let dir = env.workspace_root().join(".nexus").join("sudocode");
    fs::create_dir_all(&dir).expect("create project config dir");
    fs::write(
        dir.join("settings.local.json"),
        format!("{{\n  \"auth_profile\": \"{profile}\"\n}}\n"),
    )
    .expect("write settings.local.json");
}
