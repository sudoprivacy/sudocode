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
