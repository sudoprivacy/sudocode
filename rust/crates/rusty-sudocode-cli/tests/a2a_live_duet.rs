//! Two real `scode` processes converse over the PRODUCTION broker, auth-on.
//!
//! ## Why this exists next to `pty_agent_duet.rs`
//!
//! That test is the workflow's home and covers three transports — but it is
//! auth-OFF by construction. Its sender takes a throwaway name
//! (`scode-sender-<pid>-<n>`) and it asserts the envelope arrives carrying that
//! name. Against a real cluster neither holds: the node overwrites `from` with
//! the identity in the presented certificate, so an invented sender name cannot
//! exist and the assertion would look for a name that can never appear.
//!
//! That difference is the whole point of running this one. What production
//! actually does — the thing no auth-off run can show — is:
//!
//!   * a client is admitted only if its certificate is, over real mTLS;
//!   * the `from` a receiver sees is the one the NODE stamped, not the one the
//!     sender claimed;
//!   * the receiver's REPL surfaces it and starts a turn on its own.
//!
//! ## Non-invasive on purpose
//!
//! Both sides are the SHIPPED binary with the SHIPPED prompts. Nothing here
//! tells the receiver how to behave: the inbound envelope reaches it through
//! `compose_next_turn_from_envelopes`, and whether it answers is its own
//! decision under its own system prompt. So the assertion is on what the
//! runtime prints — `📨 A2A from <sender>` — and NOT on the model choosing to
//! reply. A test that demanded a reply would be grading the model, and would
//! fail for reasons that have nothing to do with the transport.
//!
//! ## Running it
//!
//! Needs real credentials and a reachable broker, so it is opt-in twice over
//! and skips loudly rather than pretending:
//!
//! ```text
//! A2A_LIVE_BROKER=1 SCODE_TEST_BACKEND=live SCODE_LIVE_MODEL=claude-sonnet-4-6 \
//!   cargo test -p rusty-sudocode-cli --test a2a_live_duet -- --nocapture
//! ```
//!
//! `SCODE_LIVE_MODEL` is not optional in practice: the harness pins `sonnet`,
//! and this machine's config has no model by that bare name.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::TestEnv;

/// The production broker (Tailscale address of the Tencent-cloud node).
const BROKER: &str = "100.64.0.1:8443";
/// Receives. Its REPL is what the assertion reads.
const RECEIVER: &str = "cloud-probe";
/// Sends. Its certificate is what the node stamps into `from`.
const SENDER: &str = "sudocloud-worker";
/// Agent bundles minted off the broker's CA. Windows form deliberately: the
/// binary cannot read an MSYS `/c/...` path.
const CERTS: &str = r"C:\Users\songym\authon-certs";
/// A live turn against a real gateway, plus a real raft commit.
const BUDGET: Duration = Duration::from_secs(180);

fn cert(file: &str) -> String {
    format!(r"{CERTS}\{file}")
}

/// The identity files for one agent, as the binary expects them.
fn tls_env(agent: &str) -> Vec<(&'static str, String)> {
    vec![
        ("NEXUS_CA_PEM", cert(&format!("{agent}-ca.pem"))),
        ("NEXUS_CLIENT_CERT", cert(&format!("{agent}-agent.pem"))),
        ("NEXUS_CLIENT_KEY", cert(&format!("{agent}-agent-key.pem"))),
    ]
}

/// Block until the receiver records its read position.
///
/// `spawn_inbox_poller` writes it the moment its first seek-to-tail succeeds,
/// so the file appearing means "connected, inbox provisioned, now listening".
/// Its ABSENCE has two causes and they are not distinguishable from the
/// filesystem: the poller never started (no A2A session — bad env, refused
/// certificate), or its first seek failed and it printed
/// `initial inbox seek failed` instead.
///
/// Both leave their trace on the receiver's screen, so the screen is what a
/// failure here must show. Dumping it is not decoration: the harness deletes
/// the temp workspace when the test unwinds, so anything not captured HERE is
/// gone before it can be read.
fn wait_for_cursor(sess: &mut pty_expect::PtySession, path: &Path, config_home: &Path) {
    let deadline = Instant::now() + BUDGET;
    while !path.exists() {
        if Instant::now() >= deadline {
            let screen = sess.render(|s| s.contents());
            let listing = std::fs::read_dir(config_home)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|e| format!("  {}", e.file_name().to_string_lossy()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|e| format!("  <unreadable: {e}>"));
            panic!(
                "the receiver never recorded a read position at {}\n\
                 — so either no A2A session was built, or the first inbox seek failed.\n\n\
                 === receiver screen ===\n{screen}\n\n\
                 === config home ({}) ===\n{listing}",
                path.display(),
                config_home.display()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Poll the receiver's screen for `needle`.
fn expect_on_screen(sess: &mut pty_expect::PtySession, needle: &str) {
    let deadline = Instant::now() + BUDGET;
    loop {
        let screen = sess.render(|s| s.contents());
        if screen.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the receiver's screen never showed {needle:?} within {BUDGET:?}\nPTY:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn cursor_path(config_home: &Path, agent: &str) -> PathBuf {
    config_home.join(format!("a2a-cursor-{agent}"))
}

#[test]
fn two_real_scode_processes_converse_over_the_production_broker() {
    if std::env::var("A2A_LIVE_BROKER").is_err() {
        eprintln!(
            "SKIP(a2a-live): set A2A_LIVE_BROKER=1 (plus SCODE_TEST_BACKEND=live and \
             SCODE_LIVE_MODEL) — this one talks to a real cluster with real certificates"
        );
        return;
    }
    assert_eq!(
        std::env::var("SCODE_TEST_BACKEND").unwrap_or_default(),
        "live",
        "this test is meaningless against the mock: the mock's sender writes a fixed \
         recipient and never opens a connection to the broker"
    );
    for agent in [RECEIVER, SENDER] {
        for (_, path) in tls_env(agent) {
            assert!(
                Path::new(&path).exists(),
                "missing identity file {path} — mint it with `nexusd-cluster auth mint`"
            );
        }
    }

    let env = TestEnv::new("a2a-live");
    let config_home = env.config_home().to_path_buf();

    // ── The RECEIVER: a real REPL, parked on its own inbox ──────────────────
    let recv_tls = tls_env(RECEIVER);
    let mut receiver_env: Vec<(&str, &str)> = vec![
        // NOT optional, and nothing says so at the failure site. The A2A
        // receiver lives on the coordinator loop, and that loop only exists in
        // the ASYNC repl: `main.rs` reads `QueueMode::from_env()` and dispatches
        // to `run_repl_iocraft_dispatch` for anything but `off`, else to
        // `run_repl_loop` — which has no coordinator, so no poller, so no
        // inbox. The harness defaults this to `off` (the sync repl reprints
        // `❯`, which `expect` depends on), so a receiver that does not override
        // it connects to nothing and says nothing about it.
        ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        ("NEXUS_A2A_ENDPOINT", BROKER),
        ("NEXUS_A2A_AGENT", RECEIVER),
        ("NEXUS_A2A_PEER", SENDER),
    ];
    receiver_env.extend(recv_tls.iter().map(|(k, v)| (*k, v.as_str())));

    let mut receiver = env.spawn_with_env(&["--permission-mode", "read-only"], &receiver_env);
    receiver.set_default_timeout(BUDGET);
    receiver
        .expect("❯")
        .expect("the receiver's REPL should start");
    // It provisions its own inbox and seeks to the tail; the cursor file is the
    // observable edge of "now listening".
    wait_for_cursor(
        &mut receiver,
        &cursor_path(&config_home, RECEIVER),
        &config_home,
    );

    // ── The SENDER: another real scode, whose model decides to call `send` ──
    let send_tls = tls_env(SENDER);
    let mut sender_env: Vec<(&str, &str)> = vec![
        ("NEXUS_A2A_ENDPOINT", BROKER),
        ("NEXUS_A2A_AGENT", SENDER),
        ("NEXUS_A2A_PEER", RECEIVER),
    ];
    sender_env.extend(send_tls.iter().map(|(k, v)| (*k, v.as_str())));

    let prompt = env.prompt(
        &format!(
            "Send a message to {RECEIVER} saying LIVE-DUET-PROBE over the nexus A2A network. \
             Use the send tool. Do not do anything else."
        ),
        "unified_send_roundtrip",
    );
    let mut sending = env.spawn_with_env(
        &["--permission-mode", "workspace-write", &prompt],
        &sender_env,
    );
    sending.set_default_timeout(BUDGET);
    // Capture BEFORE unwrapping: `expect_eof` failing is exactly the case whose
    // screen explains why, and an unwrap here would throw it away. An exit code
    // of 0 says the process ended cleanly, not that a message left the machine —
    // a turn that decided to do nothing exits 0 too. The screen is where
    // `message delivered to <peer>` (or a refusal, or a permission prompt) shows
    // up, so it is printed either way.
    let exit = sending.expect_eof();
    let sender_screen = sending.render(|s| s.contents());
    eprintln!(
        "=== SENDER screen ===\n{}\n=== end SENDER screen (exit: {exit:?}) ===",
        sender_screen.trim_end()
    );
    let exit = exit.expect("the sender should exit");
    eprintln!("A2A-LIVE sender exit: {exit:?}");

    // ── What crossed ───────────────────────────────────────────────────────
    // The node stamps `from` from the presented certificate, so the name on the
    // receiver's screen is the SENDER's true identity — never what the sender
    // claimed. That is the auth-on property this test exists for.
    expect_on_screen(&mut receiver, &format!("A2A from {SENDER}"));
    expect_on_screen(&mut receiver, "LIVE-DUET-PROBE");

    // Print the receiving end on SUCCESS too, not only when it fails. What this
    // test exists to demonstrate is a node-stamped identity appearing on a real
    // peer's screen; a green line asserts that happened but shows nothing, and
    // evidence nobody can read is evidence nobody checks.
    eprintln!(
        "=== RECEIVER screen ===\n{}\n=== end RECEIVER screen ===",
        receiver.render(|s| s.contents()).trim_end()
    );

    // And the receiver recorded that it took the message: a receiver that
    // surfaces without recording re-delivers the same message forever.
    let cursor = cursor_path(&config_home, RECEIVER);
    let deadline = Instant::now() + BUDGET;
    loop {
        let recorded = std::fs::read_to_string(&cursor)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if recorded > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the receiver surfaced the message but never advanced {}",
            cursor.display()
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}
