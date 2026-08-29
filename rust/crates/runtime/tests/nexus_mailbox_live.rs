//! Live A2A round-trip against a RUNNING `nexusd-cluster` (the loopback
//! `serve-local` / auth-off plane). Ignored by default — it needs a real
//! daemon, which unit tests can't provide — so it proves the one thing they
//! can't: that `ensure_stream` + `stream_write` + `stream_read_at` actually
//! move an envelope through a real gRPC server and a real DT_STREAM.
//!
//! Run it with a daemon up (e.g. `nexusd-cluster serve-local --port 12022`):
//!
//! ```text
//! NEXUS_A2A_TEST_ENDPOINT=127.0.0.1:12022 \
//!   cargo test -p runtime --test nexus_mailbox_live -- --ignored --nocapture
//! ```
//!
//! Under auth-off the stamp hook is fail-open, so the authored `from` is
//! preserved and the assertion can pin it exactly.

use std::sync::Arc;

use nexus_vfs_client::NexusVfsClient;
use runtime::nexus_mailbox::{ensure_inbox, poll_new, send};

#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT"]
fn live_inbox_roundtrip() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let auth = std::env::var("NEXUS_API_KEY").unwrap_or_default();
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial daemon"));

    let me = "scode-probe";
    // Provision our own inbox (idempotent) — the standalone self-provision path.
    ensure_inbox(&client, me, &auth).expect("ensure inbox");

    // Snapshot the tail so the assertion sees only the message we send below,
    // not any residue from a previous run of this probe.
    let (_history, start) = poll_new(&client, me, 0, &auth).expect("seek to tail");

    // A "peer" writes into our inbox (simulates the receive direction), then we
    // poll it back — proving send + poll against the real DT_STREAM.
    let body = "hello over a real dt_stream";
    send(&client, "peer-x", me, body, &auth).expect("send to inbox");

    let (msgs, next) = poll_new(&client, me, start, &auth).expect("poll new");
    assert!(next >= start, "cursor must not regress");
    assert!(
        msgs.iter().any(|m| m.from == "peer-x" && m.body == body),
        "expected the sent envelope back, got {msgs:?}"
    );
}
