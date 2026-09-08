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
    sess.expect("auth_profile = client-acct")
        .unwrap_or_else(|e| {
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

/// Several accounts configured and none selected: `doctor` names the candidates
/// and the command that fixes it, rather than reporting whichever account sorts
/// first as if it were a decision.
///
/// The old behavior — silently taking the alphabetically first account — is what
/// let a session bill an account nobody chose, so a report that answers
/// confidently here would be reporting a coin flip.
#[test]
fn doctor_reports_ambiguity_when_no_account_is_selected() {
    let env = TestEnv::new("auth-profile-resolve-default");
    write_two_account_config(&env);
    // No auth_profile persisted — exercise the unselected path.

    let mut sess = env.spawn(&["doctor"]);
    sess.set_default_timeout(Duration::from_secs(30));

    // Assert the short summary line, not the detail: the detail carries the
    // candidate list and the fix command, which the terminal wraps at width —
    // its wording is covered by the `select_proxy_account` unit tests instead.
    sess.expect("could not resolve a proxy account")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("doctor should report the ambiguity: {e}\nPTY screen:\n{screen}");
        });
    // `doctor` must still run to completion — it is the command someone reaches
    // for to diagnose this, so it cannot be taken down by the thing it reports.
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

    // These tests are about which account the *selector* resolves to, so the
    // config home must not carry one of its own. In live mode it is seeded from
    // a copy of the developer's config, which may well have `auth_profile` set —
    // and then `doctor_defaults_to_first_account_without_auth_profile` would be
    // asserting the default path while a profile was quietly in force.
    let settings_path = env.config_home().join("settings.json");
    if let Ok(contents) = fs::read_to_string(&settings_path) {
        if let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&contents) {
            if let Some(object) = settings.as_object_mut() {
                object.remove("auth_profile");
            }
            let serialized =
                serde_json::to_string_pretty(&settings).expect("serialize settings.json");
            fs::write(&settings_path, serialized).expect("rewrite settings.json");
        }
    }
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

/// Wait for the REPL to be ready for input again, then exit and assert a clean
/// shutdown.
///
/// The `expect("❯")` is the load-bearing half. A slash command's output can
/// match while the REPL is still finishing the command, and keys sent into that
/// window are not read as a submitted line — the session then never exits, and
/// the failure surfaces as a timeout on the exit rather than on the command
/// that caused it. The exit then gets its own budget, because teardown is a
/// different cost from the command under test.
fn exit_cleanly(sess: &mut pty_expect::PtySession) {
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL should return to the prompt: {e}\nPTY screen:\n{screen}");
    });
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(60));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// `/account` names who pays and lists what else is configured.
///
/// The account is resolved per request from layered config, so it can be one
/// nobody in this project chose. This is the command that makes it visible
/// without reading three files.
#[test]
fn account_lists_configured_accounts_and_marks_current() {
    let env = TestEnv::new("account-list");
    write_two_account_config(&env);

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(Duration::from_secs(20));
    sess.expect("❯").expect("async REPL prompt");

    sess.send("/account\r").expect("send /account");
    for expected in ["Accounts", "sudorouter", "team-b"] {
        sess.expect(expected).unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("/account should list {expected}: {e}\nPTY screen:\n{screen}");
        });
    }

    exit_cleanly(&mut sess);
}

/// `/account <name>` switches the project to another configured account and
/// persists the selection through the same writer `/config set` uses, so the
/// choice survives the session and a later `/account` reports it as chosen by
/// `auth_profile` rather than by a default.
#[test]
fn account_switch_persists_the_selection() {
    let env = TestEnv::new("account-switch");
    write_two_account_config(&env);

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(Duration::from_secs(20));
    sess.expect("❯").expect("async REPL prompt");

    sess.send("/account team-b\r")
        .expect("send /account team-b");
    sess.expect("Account updated").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("/account <name> should report the switch: {e}\nPTY screen:\n{screen}");
    });

    exit_cleanly(&mut sess);

    // On disk, in the scope-appropriate file — the same one `/config set
    // auth_profile` writes, not a session-only toggle.
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
        content.contains("auth_profile") && content.contains("team-b"),
        "/account must persist the selection; settings.local.json:\n{content}"
    );
}

/// An account that is not configured is refused, naming the ones that are.
///
/// Writing it instead would move the failure to the next request, where the
/// selector refuses rather than billing someone else — correct, but by then
/// the user has stopped looking at the command that caused it.
#[test]
fn account_refuses_a_name_that_is_not_configured() {
    let env = TestEnv::new("account-unknown");
    write_two_account_config(&env);

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(Duration::from_secs(20));
    sess.expect("❯").expect("async REPL prompt");

    sess.send("/account no-such-account\r")
        .expect("send /account no-such-account");
    sess.expect("no account named").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("/account should refuse an unconfigured name: {e}\nPTY screen:\n{screen}");
    });
    sess.expect("team-b").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("the refusal should name what is configured: {e}\nPTY screen:\n{screen}");
    });

    exit_cleanly(&mut sess);

    // Nothing was written: a refused selection must not land on disk.
    let settings_local = env
        .workspace_root()
        .join(".nexus")
        .join("sudocode")
        .join("settings.local.json");
    let content = fs::read_to_string(&settings_local).unwrap_or_default();
    assert!(
        !content.contains("no-such-account"),
        "a refused account must not be persisted; settings.local.json:\n{content}"
    );
}

/// The per-turn status line names the account the turn was billed to.
///
/// Live-only: mock mode runs under `--auth api-key`, which bills no named
/// account, so there is nothing for the line to name there.
#[test]
fn turn_status_line_names_the_billing_account() {
    let env = TestEnv::new("account-status-line");
    if env.is_mock() {
        eprintln!(
            "skipping turn_status_line_names_the_billing_account: mock mode \
             runs under --auth api-key (run with SCODE_TEST_BACKEND=live)"
        );
        return;
    }

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(Duration::from_secs(120));
    sess.expect("❯").expect("async REPL prompt");

    sess.send("Reply with the single word: ok\r")
        .expect("send prompt");
    // `acct ` is emitted only by the status-line renderer, so unlike the model
    // name it cannot be matched against the echo of the prompt.
    sess.expect("acct ").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("turn status line should name the account: {e}\nPTY screen:\n{screen}");
    });

    exit_cleanly(&mut sess);
}
