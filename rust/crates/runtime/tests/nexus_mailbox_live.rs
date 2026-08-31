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
use std::thread;
use std::time::{Duration, Instant};

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
    let (_history, start) = poll_new(&client, me, 0, &auth, 0).expect("seek to tail");

    // A "peer" writes into our inbox (simulates the receive direction), then we
    // poll it back — proving send + poll against the real DT_STREAM.
    let body = "hello over a real dt_stream";
    send(&client, "peer-x", me, body, &auth).expect("send to inbox");

    let (msgs, next) = poll_new(&client, me, start, &auth, 0).expect("poll new");
    assert!(next >= start, "cursor must not regress");
    assert!(
        msgs.iter().any(|m| m.from == "peer-x" && m.body == body),
        "expected the sent envelope back, got {msgs:?}"
    );
}

/// Prove the receive loop's blocking tail (`poll_new` with `block_ms > 0`,
/// backed by `stream_read_at(blocking=true)`) is an event-driven wakeup, not a
/// poll: a receiver parked on an idle inbox wakes as soon as a peer writes, and
/// on an idle inbox returns empty at the deadline instead of hanging. This is
/// the exact wait the standalone `scode` receiver now uses in place of a
/// `sleep` poll loop — the cursor-aware DT_STREAM tail primitive
/// (`read_at_blocking`): one RPC that returns the next frame at the cursor,
/// versus `sys_watch`'s change-event-then-separate-read. Run against a
/// plaintext `serve-local` daemon:
///
/// ```text
/// NEXUS_A2A_TEST_ENDPOINT=127.0.0.1:12022 \
///   cargo test -p runtime --test nexus_mailbox_live live_blocking_read_wakes_on_write -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT"]
fn live_blocking_read_wakes_on_write() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let auth = std::env::var("NEXUS_API_KEY").unwrap_or_default();
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial daemon"));

    let me = "scode-blocking-read-probe";
    ensure_inbox(&client, me, &auth).expect("ensure inbox");
    // Fix the cursor at the current tail so the assertions see only what we
    // write below, not residue from a previous run.
    let (_history, tail) = poll_new(&client, me, 0, &auth, 0).expect("seek to tail");

    // Negative path: an idle inbox with no writer must block for the whole
    // timeout and return EMPTY at the deadline — never hang, never early-return.
    let t0 = Instant::now();
    let (idle_msgs, idle_next) =
        poll_new(&client, me, tail, &auth, 800).expect("idle blocking read");
    let idle_elapsed = t0.elapsed();
    assert!(
        idle_msgs.is_empty(),
        "idle read must surface nothing, got {idle_msgs:?}"
    );
    assert_eq!(idle_next, tail, "idle read must not advance the cursor");
    assert!(
        idle_elapsed >= Duration::from_millis(700),
        "idle read returned too early ({idle_elapsed:?}) — it did not park on the tail"
    );

    // Positive path: park the blocking read first, then a peer writes ~400ms
    // later. The read must wake on that write well before its 5s deadline AND
    // return the exact envelope — proving event-driven (not timeout) delivery.
    let body = "wake up over the blocking tail";
    let writer = {
        // A SEPARATE connection: the receiver's blocking read monopolises its
        // own client's single worker task, so the writer must not share it (a
        // shared client would queue the write behind the 5s blocking read).
        let endpoint = endpoint.clone();
        let auth = auth.clone();
        thread::spawn(move || {
            let wclient = NexusVfsClient::connect(&endpoint).expect("writer dial");
            thread::sleep(Duration::from_millis(400));
            send(&wclient, "peer-block", me, body, &auth).expect("peer write");
        })
    };

    let t1 = Instant::now();
    let (msgs, next) = poll_new(&client, me, tail, &auth, 5_000).expect("armed blocking read");
    let woke = t1.elapsed();
    writer.join().expect("writer thread");

    assert!(
        msgs.iter()
            .any(|m| m.from == "peer-block" && m.body == body),
        "blocking read must surface the peer envelope, got {msgs:?}"
    );
    assert!(next > tail, "cursor must advance past the consumed frame");
    assert!(
        woke < Duration::from_millis(4_000),
        "read woke on the timeout ({woke:?}), not the write event"
    );
    assert!(
        woke >= Duration::from_millis(300),
        "read returned before the write was issued ({woke:?}) — stale/instant wake"
    );
    println!("blocking read woke on write after {woke:?} (idle timeout was {idle_elapsed:?})");
}

/// Guard the concurrent-dispatch property that lets the receiver share the ONE
/// `NexusVfsClient` with the send half: a blocking tail read parked on the
/// client must NOT stall other ops on the SAME client. Each op runs on its own
/// task (see `nexus-vfs-client`), so a quick op issued while a 1.5s blocking
/// read is parked still returns promptly. If someone reverts the client to a
/// serial single-worker loop, this fails — that regression is exactly what
/// would starve an agent's sends behind its own receive.
///
/// ```text
/// NEXUS_A2A_TEST_ENDPOINT=127.0.0.1:12022 \
///   cargo test -p runtime --test nexus_mailbox_live live_blocking_read_does_not_stall_shared_client -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT"]
fn live_blocking_read_does_not_stall_shared_client() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let auth = std::env::var("NEXUS_API_KEY").unwrap_or_default();

    let me = "scode-starve-probe";
    let shared = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial shared"));
    ensure_inbox(&shared, me, &auth).expect("ensure inbox");
    let (_history, tail) = poll_new(&shared, me, 0, &auth, 0).expect("seek to tail");

    // Park a 1.5s blocking read on the SHARED client (no writer → it holds its
    // task the whole time).
    let blocker = {
        let shared = Arc::clone(&shared);
        let auth = auth.clone();
        thread::spawn(move || {
            let _ = poll_new(&shared, me, tail, &auth, 1_500);
        })
    };
    thread::sleep(Duration::from_millis(150)); // let the blocking read park

    // A quick op on the SAME client must still return promptly — concurrent
    // dispatch means it does not queue behind the 1.5s block.
    let t = Instant::now();
    let _ = shared.stat("/", &auth);
    let shared_op = t.elapsed();
    blocker.join().expect("blocker thread");

    println!("shared_op while a 1.5s blocking read was parked: {shared_op:?}");
    assert!(
        shared_op < Duration::from_millis(500),
        "a shared-client op must NOT be stalled behind the blocking read \
         ({shared_op:?}) — the client is no longer dispatching ops concurrently"
    );
}

/// Provision an inbox for `NEXUS_A2A_TEST_INBOX` (idempotent) — a duet setup
/// helper. A co-host responder's own loop provisions its inbox lazily / relies
/// on the first writer to auto-create it; a standalone `scode` sender's
/// `stream_write` does NOT auto-create (it fails loud with `StreamNotFound`),
/// so this seeds the responder's `/agents/<name>/chat-with-me` up front.
#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT + NEXUS_A2A_TEST_INBOX"]
fn live_ensure_inbox() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let inbox = std::env::var("NEXUS_A2A_TEST_INBOX").expect("set NEXUS_A2A_TEST_INBOX=<agent>");
    let auth = std::env::var("NEXUS_API_KEY").unwrap_or_default();
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial daemon"));
    ensure_inbox(&client, &inbox, &auth).expect("ensure inbox");
    println!("ensured /agents/{inbox}/chat-with-me");
}

/// Spawn a co-host responder agent in a running `nexusd-cluster-cohost` daemon
/// (the control-plane `managed_agent.start_session_v1`), so a standalone
/// `scode` has a REAL LLM partner to converse with over A2A. The responder
/// binds its replicated inbox `/agents/<name>/chat-with-me` and auto-replies to
/// whoever messages it. Drives the full duet:
///
/// ```text
/// NEXUS_A2A_TEST_ENDPOINT=127.0.0.1:2126 NEXUS_A2A_TEST_SPAWN=mac-ai \
///   cargo test -p runtime --test nexus_mailbox_live live_spawn_cohost -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a running nexusd-cluster-cohost daemon + funded key; set NEXUS_A2A_TEST_ENDPOINT + NEXUS_A2A_TEST_SPAWN"]
fn live_spawn_cohost() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let agent = std::env::var("NEXUS_A2A_TEST_SPAWN").expect("set NEXUS_A2A_TEST_SPAWN=<agent>");
    let model =
        std::env::var("NEXUS_A2A_TEST_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string());
    let auth = std::env::var("NEXUS_API_KEY").unwrap_or_default();
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial daemon"));

    // String-only params -> rpc_codec is plain JSON, so a raw JSON payload works.
    let payload =
        format!(r#"{{"agent_id":"{agent}","model":"{model}","owner_id":"root","zone_id":"root"}}"#);
    let resp = client
        .call("managed_agent.start_session_v1", payload.as_bytes(), &auth)
        .expect("start_session_v1 call");
    let body = String::from_utf8_lossy(&resp);
    println!("start_session_v1 -> {body}");
    assert!(
        body.contains("session_id"),
        "expected a session_id in the spawn response, got {body}"
    );
}

/// Read every message in an inbox and print it — the receive-side verify tool
/// (the analog of the nexus-vfs `mailbox_cli collect`). Point it at another
/// agent's inbox to confirm a *separate* writer's envelope actually landed on
/// the wire, e.g. after a real `scode` turn calls `send_message`:
///
/// ```text
/// NEXUS_A2A_TEST_ENDPOINT=127.0.0.1:12055 NEXUS_A2A_TEST_INBOX=scode-probe \
///   cargo test -p runtime --test nexus_mailbox_live live_collect_inbox -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT + NEXUS_A2A_TEST_INBOX"]
fn live_collect_inbox() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let inbox = std::env::var("NEXUS_A2A_TEST_INBOX").expect("set NEXUS_A2A_TEST_INBOX=<agent>");
    let auth = std::env::var("NEXUS_API_KEY").unwrap_or_default();
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial daemon"));

    // poll_new reads inbox_path(self_agent), so pass the target inbox name.
    // Its self-filter only drops the inbox owner's OWN writes (none here) —
    // a peer's stamped envelope (e.g. from a real scode send) still surfaces.
    let (msgs, next) = poll_new(&client, &inbox, 0, &auth, 0).expect("collect");
    println!(
        "inbox /agents/{inbox}/chat-with-me — {} message(s), tail={next}",
        msgs.len()
    );
    for m in &msgs {
        println!("  from={:?} body={:?}", m.from, m.body);
    }
}
