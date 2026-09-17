//! `send` must reach the same destination no matter how the model spells it.
//!
//! This is the regression guard for a live failure. Three tools were advertised
//! at once — `SendMessage`, `send_message`, `send` — differing only in which
//! schema field carried the text, and only the literal name `send` was
//! intercepted for nexus A2A. A model on a cross-machine session picked
//! `send_message`, its reply was written to a local `.sudocode-inbox/` file, the
//! tool answered "Message sent to <peer>'s inbox", and the peer on the other
//! machine waited ten hours for a message that had never left the host.
//!
//! ## What these assert, and why it changed
//!
//! They used to assert that a hook had been called: the host wired an A2A
//! `MailboxSender`, the dispatcher branched on its presence, and the test
//! watched the sink. That branch is gone — it chose the destination from PROCESS
//! state while the recipient went unread, so a session with A2A on addressed a
//! local sub-agent over the network, at a stream no ephemeral agent has.
//!
//! So these assert the PATH the envelope took, which is the thing that was wrong
//! in the first place. A recording backend stands in for the transport: the
//! destination is decided by the convention before any transport is touched, so
//! a daemon would only slow the test down without testing more of the decision.

use std::io;
use std::sync::{Arc, Mutex};

use engine_host::tool_executor::CliToolExecutor;
use runtime::fs_backend::{FsBackend, FsDirEntry, FsMetadata};
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::{ToolDispatchContext, ToolExecutor};
use tools::GlobalToolRegistry;

/// Every append this backend was asked to make, as `(path, bytes)`.
type Appends = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// Every inbox this backend was asked to create.
type Provisions = Arc<Mutex<Vec<String>>>;

/// A backend that records what a send does and refuses everything else.
///
/// `append`, `is_append_stream` and `create_append_log` are what a send
/// reaches. The rest panic rather than return plausible defaults: a send that
/// starts reading or renaming is doing something these tests do not describe,
/// and a silent default would hide it.
struct RecordingBackend {
    appends: Appends,
    provisions: Provisions,
}

impl FsBackend for RecordingBackend {
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.appends
            .lock()
            .expect("appends poisoned")
            .push((path.to_string(), data.to_vec()));
        Ok(())
    }

    /// A send creates the recipient's inbox before writing to it.
    ///
    /// Overridden rather than left to the trait's default, which probes
    /// `exists` and falls back to `write` — the file-backend shape, and not
    /// what a stream backend does. Recording it is also the point: the path a
    /// send provisions has to be the path it then appends to.
    fn create_append_log(&self, path: &str, _retention: u64) -> io::Result<()> {
        self.provisions
            .lock()
            .expect("provisions poisoned")
            .push(path.to_string());
        Ok(())
    }

    /// Mirrors `NexusVfsFsBackend`: the A2A leaf marks a framed stream, so an
    /// envelope crosses as one record rather than a JSONL line.
    fn is_append_stream(&self, path: &str) -> io::Result<bool> {
        Ok(path.ends_with(runtime::mailbox::CHAT_WITH_ME_SUFFIX))
    }

    fn read(&self, _: &str) -> io::Result<Vec<u8>> {
        unreachable!("a send does not read")
    }
    fn write(&self, _: &str, _: &[u8]) -> io::Result<()> {
        unreachable!("a send appends, it does not overwrite")
    }
    fn delete(&self, _: &str) -> io::Result<()> {
        unreachable!("a send does not delete")
    }
    fn stat(&self, _: &str) -> io::Result<FsMetadata> {
        unreachable!("a send does not stat")
    }
    fn readdir(&self, _: &str) -> io::Result<Vec<FsDirEntry>> {
        unreachable!("a send does not list directories")
    }
    fn exists(&self, _: &str) -> io::Result<bool> {
        unreachable!("a send does not probe existence")
    }
    fn create_dir_all(&self, _: &str) -> io::Result<()> {
        unreachable!("a stream inbox has no directory to create")
    }
    fn rename(&self, _: &str, _: &str) -> io::Result<()> {
        unreachable!("a send does not rename")
    }
    fn canonicalize(&self, _: &str) -> io::Result<String> {
        unreachable!("a send does not canonicalize")
    }
    fn symlink_metadata(&self, _: &str) -> io::Result<FsMetadata> {
        unreachable!("a send does not read link metadata")
    }
}

fn executor() -> CliToolExecutor {
    CliToolExecutor::new(None, GlobalToolRegistry::builtin(), None)
}

/// An executor on a nexus-convention mailbox that records every write.
///
/// Given to the dispatcher exactly as the host gives it the A2A session's
/// mailbox, so the test exercises the real handover. Per-executor rather than
/// per-process, so each test gets its own and they need not run in any order.
fn nexus_executor() -> (CliToolExecutor, Appends, Provisions) {
    let appends: Appends = Arc::new(Mutex::new(Vec::new()));
    let provisions: Provisions = Arc::new(Mutex::new(Vec::new()));
    let mut executor = executor();
    executor.set_mailbox(Arc::new(Mailbox::new(
        Arc::new(RecordingBackend {
            appends: Arc::clone(&appends),
            provisions: Arc::clone(&provisions),
        }),
        "win-ai".to_string(),
        InboxConvention::PerRecipient {
            root: String::new(),
        },
    )));
    (executor, appends, provisions)
}

/// Run one tool call to completion on a throwaway runtime.
fn call(executor: &CliToolExecutor, tool: &str, input: &str) -> Result<String, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
        .block_on(async {
            executor
                .execute_with_context(tool, input, &ToolDispatchContext::default())
                .await
                .map_err(|e| e.to_string())
        })
}

/// Every way a model can spell the call reaches the same inbox.
///
/// `SendMessage` is CC's spelling and `send_message` the A2A tool this replaced —
/// a model reaches for either by habit. `body` is the field the old A2A schema
/// used, and the one the wire envelope still uses, so a model that has seen
/// either will reach for it too.
///
/// None of them may quietly become a local file write: the name and the field
/// are things a model picks, and neither is allowed to decide where a message
/// goes.
#[test]
fn every_spelling_and_shape_reaches_the_same_inbox() {
    for (tool, input) in [
        (
            "send",
            r#"{"to":"mac-ai","message":"canonical name","summary":"s"}"#,
        ),
        (
            "SendMessage",
            r#"{"to":"mac-ai","message":"CC spelling","summary":"s"}"#,
        ),
        (
            "send_message",
            r#"{"to":"mac-ai","message":"superseded A2A name","summary":"s"}"#,
        ),
        // The old A2A input shape, field and all.
        (
            "send_message",
            r#"{"to":"mac-ai","body":"body field","summary":"s"}"#,
        ),
        (
            "send",
            r#"{"to":"mac-ai","body":"body field, new name","summary":"s"}"#,
        ),
    ] {
        let (executor, appends, provisions) = nexus_executor();
        let result =
            call(&executor, tool, input).unwrap_or_else(|e| panic!("`{tool}` must not fail: {e}"));

        let wrote = appends.lock().expect("appends poisoned").clone();
        assert_eq!(
            wrote.len(),
            1,
            "`{tool}` must write exactly one envelope, got {wrote:?}"
        );
        assert_eq!(
            wrote[0].0, "/agents/mac-ai/chat-with-me",
            "`{tool}` addressed the wrong path"
        );

        // The inbox is created before it is written to, at the same path. A
        // recipient that has never run has no stream, and an append to a path
        // that is not one does not fail — it leaves a plain entry there, tells
        // the sender it was delivered, and the inbox can never become a stream
        // again.
        assert_eq!(
            provisions.lock().expect("provisions poisoned").as_slice(),
            [wrote[0].0.clone()],
            "`{tool}` must create exactly the inbox it wrote to"
        );

        // `mailbox_path` is what a human reads to decide whether a message left
        // the host, so it has to be the path the convention resolved.
        assert!(
            result.contains("/agents/mac-ai/chat-with-me"),
            "`{tool}` must report the path it wrote, got: {result}"
        );
        assert!(
            !result.contains(".sudocode-inbox"),
            "`{tool}` reported a workspace write while the session is on nexus: {result}"
        );
    }
}

/// A plain-text send with no summary is refused, over nexus as locally.
///
/// This is the one place unification made a cross-machine send stricter, so it is
/// worth pinning. The old A2A path was a two-string pipe with summary hardcoded
/// away: it accepted `{to, body}` and delivered an envelope whose summary field
/// was a placeholder the recipient then displayed. The registry tool has always
/// required one for plain text, and routing every send through it means the
/// requirement now reaches the wire — an error the model can act on, rather than
/// a delivery that arrives unlabelled.
///
/// Nothing may be written while it is refused: a half-envelope on the peer's
/// stream cannot be taken back.
#[test]
fn a_plain_text_send_without_a_summary_is_refused_and_writes_nothing() {
    let (executor, appends, _provisions) = nexus_executor();
    let error = call(
        &executor,
        "send",
        r#"{"to":"mac-ai","message":"unlabelled"}"#,
    )
    .expect_err("a plain-text send with no summary must be refused");

    assert!(
        error.contains("summary"),
        "the error must name the missing field, got: {error}"
    );
    assert!(
        appends.lock().expect("appends poisoned").is_empty(),
        "a refused send must not reach the peer's stream"
    );
}

/// A local-looking recipient resolves through the same convention as any other.
///
/// This is the case the old dispatcher got wrong in the other direction: with
/// A2A configured it shipped a local sub-agent's message over the network, to a
/// stream no ephemeral agent has, while the sub-agent read a workspace file and
/// heard nothing. There is one namespace now, so what matters is that the path
/// is DERIVED from the name rather than branched on.
#[test]
fn a_local_looking_recipient_resolves_through_the_same_convention() {
    let (executor, appends, _provisions) = nexus_executor();
    call(
        &executor,
        "send",
        r#"{"to":"sub-agent-1","message":"to a sub-agent","summary":"hand-off"}"#,
    )
    .expect("send must succeed");

    let wrote = appends.lock().expect("appends poisoned").clone();
    assert_eq!(wrote.len(), 1, "expected one envelope, got {wrote:?}");
    assert_eq!(
        wrote[0].0, "/agents/sub-agent-1/chat-with-me",
        "a recipient's name resolves through the session's convention like any other"
    );
}

/// The envelope keeps the fields the tool accepted.
///
/// The A2A path used to be a two-string pipe — `to` and `message`, with summary
/// and kind hardcoded away — so a cross-machine send silently dropped everything
/// the local one carried. One send means one envelope shape.
#[test]
fn the_envelope_carries_its_summary_over_nexus() {
    let (executor, appends, _provisions) = nexus_executor();
    call(
        &executor,
        "send",
        r#"{"to":"mac-ai","message":"the body","summary":"the summary"}"#,
    )
    .expect("send must succeed");

    let wrote = appends.lock().expect("appends poisoned").clone();
    let envelope: serde_json::Value =
        serde_json::from_slice(&wrote[0].1).expect("the record is a JSON envelope");
    assert_eq!(envelope["body"], "the body");
    assert_eq!(
        envelope["summary"], "the summary",
        "a summary the tool accepted must survive the crossing: {envelope}"
    );
    assert_eq!(envelope["to"], "mac-ai");
}

/// The envelope carries the time it was sent, over nexus too.
///
/// Delivery into an inbox is at-least-once by design: `spawn_inbox_poller`
/// advances ONE cursor for a whole batch, so a batch the consumer did not fully
/// accept is read again, and the frames it re-hands over are byte-identical to
/// ones the receiving model has already answered. The send time is the only
/// field that separates the two cases a receiver must tell apart — the same
/// bytes handed over twice carry the SAME timestamp, a peer genuinely repeating
/// itself carries a later one.
///
/// Pinned on the FRAMED path specifically, because that is where it was missing:
/// the JSONL branch has always stamped inside `append_envelope_to_path`, while
/// the stream branch serialises the envelope as-is, so every message that ever
/// crossed a DT_STREAM arrived with `timestamp` absent. A `StdFsBackend` pair
/// root cannot show this — its `is_append_stream` is a hardcoded `false`, so it
/// only ever exercises the branch that already worked.
///
/// `skip_serializing_if = "is_zero"` is why the presence check is the assertion:
/// an unstamped envelope omits the field rather than sending a zero.
#[test]
fn the_envelope_carries_a_send_time_over_nexus() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();

    let (executor, appends, _provisions) = nexus_executor();
    call(
        &executor,
        "send",
        r#"{"to":"mac-ai","message":"the body","summary":"the summary"}"#,
    )
    .expect("send must succeed");

    let wrote = appends.lock().expect("appends poisoned").clone();
    assert_eq!(wrote.len(), 1, "expected one envelope, got {wrote:?}");
    assert_eq!(
        wrote[0].0, "/agents/mac-ai/chat-with-me",
        "the framed A2A path is the one this pins"
    );

    let envelope: serde_json::Value =
        serde_json::from_slice(&wrote[0].1).expect("the record is a JSON envelope");
    let sent_at = envelope["timestamp"]
        .as_u64()
        .unwrap_or_else(|| panic!("a crossing envelope must carry its send time, got: {envelope}"));
    assert!(
        sent_at >= before,
        "the stamp must be the current unix seconds, got {sent_at} (before={before})"
    );
}

/// With no mailbox given to the executor, the same tool writes the workspace.
///
/// The contract, not a fallback: one tool, and the session chooses the
/// destination. A plain `scode` with no nexus configured has always written
/// `.sudocode-inbox/`, and it still does — through the same call, resolved one
/// level down.
#[test]
fn with_no_session_mailbox_the_same_tool_writes_the_workspace() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("chdir into workspace");

    let result = call(
        &executor(),
        "send",
        r#"{"to":"worker","message":"local hand-off","summary":"local hand-off"}"#,
    );

    std::env::set_current_dir(previous).expect("restore cwd");
    let result = result.expect("workspace delivery must succeed");
    assert!(
        result.contains("chat-with-me"),
        "with no session mailbox the tool must write the workspace, got: {result}"
    );
    // Assert the envelope landed in THIS workspace, not just that the answer
    // named the convention. The reverse of this test is what the broken state
    // looked like: the three nexus cases above, run without the mailbox reaching
    // the dispatch thread, resolved to the ambient workspace and wrote real
    // envelopes into the crate directory while reporting success. A string check
    // alone is satisfied by that.
    let delivered = runtime::agent_mailbox::inbox_path_under(workspace.path(), "worker");
    assert!(
        delivered.is_file(),
        "the envelope must be in the workspace that was current, expected {}",
        delivered.display()
    );
}
