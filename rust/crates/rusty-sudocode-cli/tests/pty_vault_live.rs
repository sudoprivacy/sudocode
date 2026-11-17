//! Live acceptance for the builtin Vault plugin (sudoprivacy/sudocode#1458):
//! the real `scode vault tool` entry against a real `nexusd-cluster serve-user`
//! that loads the real, signed Vault plugin. Skipped unless
//! `SUDOCODE_VAULT_LIVE` points at those artifacts; `e2e/vault-live/run.sh`
//! fetches and checks them, then runs this file in a container, and
//! `e2e/vault-live/run-native.sh` runs it on a host such as macOS arm64.
//!
//! Every case scans what it produced for the canary secret: tool stdout (stderr
//! must be empty), the runtime's log and every process command line.
#![cfg(any(target_os = "linux", vault_live))]

#[path = "support/vault_live.rs"]
mod vault_live;
#[path = "support/vault_tool.rs"]
mod vault_tool;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use vault_live::{
    canary, closed_endpoint, command_lines_containing, contacted, files_containing, record,
    section, target, Live, Runtime,
};

const OWNER: &str = "alice";
const PRESENT: &str = "live/present";
const MISSING: &str = "live/missing";

fn live() -> Option<Live> {
    let live = Live::from_env();
    if live.is_none() {
        eprintln!("SUDOCODE_VAULT_LIVE is unset; skipping the live Vault acceptance");
    }
    live
}

/// The canary must appear nowhere but in the Vault's own store.
fn assert_unseen(runtime: &Runtime, outputs: &[&str], secret: &str) {
    for output in outputs {
        assert!(
            !output.contains(secret),
            "a tool output carried the canary: {output}"
        );
    }
    assert!(
        !runtime.log_text().contains(secret),
        "the runtime log carried the canary"
    );
    assert_eq!(command_lines_containing(secret), Vec::<String>::new());
}

/// Every case trusts the command-line scan to find the canary if it were
/// there; a scan that saw nothing would pass them all. It must see a process
/// that carries a marker.
#[test]
fn the_command_line_scan_sees_a_marked_process() {
    let Some(_live) = live() else { return };
    let marker = canary();
    // Two commands keep the shell itself alive (a lone command may replace
    // it, marker and all), and `read` blocks without starting a child.
    let mut process = std::process::Command::new("sh")
        .args(["-c", "read -r line; :", &marker])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn a marked process");
    // `spawn` returns before the kernel has published the new image's
    // arguments, so the marked command line appears a moment later.
    let deadline = Instant::now() + Duration::from_secs(10);
    let seen = loop {
        let seen = command_lines_containing(&marker);
        if !seen.is_empty() || Instant::now() >= deadline {
            break seen;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = process.kill();
    let _ = process.wait();
    assert_eq!(seen.len(), 1, "{seen:?}");
}

#[test]
fn a_seeded_secret_is_available_an_unseeded_ref_absent_and_use_touches_nothing() {
    let Some(live) = live() else { return };
    let data = live.path("data");
    let runtime = live.start(OWNER, &data, true);
    let secret = canary();
    live.seed(&runtime, OWNER, "live", "present", &secret);
    let agent = live.agent(&live.path("agent"), "vault-agent", OWNER, 300);
    let (listener, origin) = target();
    let home = live.home("home");
    home.configure(&section(
        &runtime.endpoint(),
        OWNER,
        &agent,
        &origin,
        &[PRESENT, MISSING],
    ));

    let list = home.run("list", "{}");
    assert_eq!(
        list.result["status"], "configured-unqualified",
        "{}",
        list.stdout
    );
    let availability: Vec<&str> = list.result["operations"]
        .as_array()
        .expect("operations")
        .iter()
        .map(|operation| operation["availability"].as_str().expect("availability"))
        .collect();
    assert_eq!(
        availability,
        ["available", "secret_absent"],
        "{}",
        list.stdout
    );

    let used = home.run("use", r#"{"operation":"op-0"}"#);
    assert_eq!(
        used.result["error"]["code"], "use_unavailable",
        "{}",
        used.stdout
    );
    assert!(
        !contacted(&listener),
        "vault_use reached the operation's target"
    );

    assert_unseen(&runtime, &[&list.stdout, &used.stdout], &secret);
    let stored = files_containing(&data, &secret);
    record(
        "b_list_and_use",
        &json!({
            "list": list.result, "use": used.result,
            "target_contacted": false,
            "plaintext_files_in_data_dir": stored,
        }),
    );
    assert!(
        stored.is_empty(),
        "the canary is stored in plaintext: {stored:?}"
    );
}

#[test]
fn the_runtime_itself_refuses_an_agent_that_is_its_own_owner() {
    let Some(live) = live() else { return };
    let runtime = live.start(OWNER, &live.path("data"), true);
    let secret = canary();
    live.seed(&runtime, OWNER, "live", "present", &secret);
    // Owner SAN and server name match, so only the runtime can refuse it:
    // an agent identity must differ from the owner it acts for.
    let owner_itself = live.agent(&live.path("owner-itself"), OWNER, OWNER, 300);
    let (_listener, origin) = target();
    let home = live.home("home");
    home.configure(&section(
        &runtime.endpoint(),
        OWNER,
        &owner_itself,
        &origin,
        &[PRESENT],
    ));
    let list = home.run("list", "{}");
    record("c_owner_as_agent", &list.result);
    assert_eq!(
        list.result["operations"][0]["availability"], "access_denied",
        "{}",
        list.stdout
    );
    assert_unseen(&runtime, &[&list.stdout], &secret);
}

#[test]
fn another_owners_agent_never_reaches_this_owners_vault() {
    let Some(live) = live() else { return };
    let runtime = live.start(OWNER, &live.path("data"), true);
    let secret = canary();
    live.seed(&runtime, OWNER, "live", "present", &secret);
    let bob = live.agent(&live.path("bob"), "bob-agent", "bob", 300);
    let (_listener, origin) = target();
    let home = live.home("home");
    home.configure(&section(
        &runtime.endpoint(),
        "bob",
        &bob,
        &origin,
        &[PRESENT],
    ));
    let list = home.run("list", "{}");
    record("c_other_owner", &list.result);
    // Bob's settings expect bob's runtime; alice's runtime cannot prove that
    // name, so the connection fails before any Vault call.
    assert_eq!(
        list.result["operations"][0]["availability"], "vault_unavailable",
        "{}",
        list.stdout
    );
    assert_unseen(&runtime, &[&list.stdout], &secret);
}

#[test]
fn a_runtime_without_the_vault_plugin_or_no_runtime_is_vault_unavailable() {
    let Some(live) = live() else { return };
    let runtime = live.start(OWNER, &live.path("data"), false);
    let agent = live.agent(&live.path("agent"), "vault-agent", OWNER, 300);
    let (_listener, origin) = target();
    let home = live.home("home");
    home.configure(&section(
        &runtime.endpoint(),
        OWNER,
        &agent,
        &origin,
        &[PRESENT],
    ));
    let without_plugin = home.run("list", "{}");
    home.configure(&section(
        &closed_endpoint(),
        OWNER,
        &agent,
        &origin,
        &[PRESENT],
    ));
    let without_runtime = home.run("list", "{}");
    record(
        "d_service_missing",
        &json!({"without_plugin": without_plugin.result, "without_runtime": without_runtime.result}),
    );
    assert_eq!(
        without_plugin.result["operations"][0]["availability"], "vault_unavailable",
        "{}",
        without_plugin.stdout
    );
    assert_eq!(
        without_runtime.result["operations"][0]["availability"], "vault_unavailable",
        "{}",
        without_runtime.stdout
    );
}

#[test]
fn a_replaced_bundle_is_read_again_on_the_next_call() {
    let Some(live) = live() else { return };
    let runtime = live.start(OWNER, &live.path("data"), true);
    let secret = canary();
    live.seed(&runtime, OWNER, "live", "present", &secret);
    let dir = live.path("rotating");
    let (_listener, origin) = target();
    let home = live.home("home");
    home.configure(&section(
        &runtime.endpoint(),
        OWNER,
        &dir,
        &origin,
        &[PRESENT],
    ));

    live.agent(&dir, "generation-1", OWNER, 300);
    let first = home.availability();
    live.agent(&dir, "generation-2", "bob", 300);
    let foreign = home.availability();
    live.agent(&dir, "generation-3", OWNER, 300);
    let renewed = home.availability();
    record(
        "e_replaced_bundle",
        &json!({"first": first, "foreign_generation": foreign, "renewed": renewed}),
    );
    assert_eq!(first, ["available"]);
    assert_eq!(foreign, ["owner_mismatch"]);
    assert_eq!(renewed, ["available"]);
    assert_unseen(&runtime, &[], &secret);
}

#[test]
fn a_restarted_runtime_still_holds_the_secret() {
    let Some(live) = live() else { return };
    let data = live.path("data");
    let secret = canary();
    let agent = live.agent(&live.path("agent"), "vault-agent", OWNER, 300);
    let (_listener, origin) = target();
    let home = live.home("home");
    let before = {
        let runtime = live.start(OWNER, &data, true);
        live.seed(&runtime, OWNER, "live", "present", &secret);
        home.configure(&section(
            &runtime.endpoint(),
            OWNER,
            &agent,
            &origin,
            &[PRESENT],
        ));
        home.availability()
    };
    let runtime = live.start(OWNER, &data, true);
    home.configure(&section(
        &runtime.endpoint(),
        OWNER,
        &agent,
        &origin,
        &[PRESENT],
    ));
    let after = home.availability();
    record("h_restart", &json!({"before": before, "after": after}));
    assert_eq!(before, ["available"]);
    assert_eq!(after, ["available"]);
    assert_unseen(&runtime, &[], &secret);
    assert_eq!(files_containing(&data, &secret), Vec::<PathBuf>::new());
}
