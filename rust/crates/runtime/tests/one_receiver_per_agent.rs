//! An agent has one receiver at a time, however many instances of it run.
//!
//! Before the agent-level receiver seat, two instances of one agent each
//! claimed whichever conversations they reached first, so a single agent's
//! messages were answered by two sessions that knew nothing of each other. Here
//! two receivers for the same agent share one pair root: the first takes every
//! conversation, the second stands by, and when the first stops the second
//! takes over from where the first left off.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::StdFsBackend;
use runtime::mailbox::{
    receiver_state, spawn_inbox_poller, InboxConvention, Mailbox, ReceiverState,
};
use runtime::HookAbortSignal;

const AGENT: &str = "one-receiver-probe";

fn tmp_root() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root = std::env::temp_dir().join(format!("one-receiver-{nanos}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn mailbox(root: &Path, self_id: &str) -> Arc<Mailbox> {
    Arc::new(Mailbox::new(
        Arc::new(StdFsBackend),
        self_id.to_string(),
        InboxConvention::new(root.to_string_lossy().into_owned()),
    ))
}

fn note(from: &str, body: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: from.to_string(),
        to: AGENT.to_string(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
        request_id: None,
    }
}

type Seen = Arc<Mutex<Vec<(&'static str, String)>>>;

fn start(
    root: &Path,
    who: &'static str,
    seen: &Seen,
) -> (HookAbortSignal, std::thread::JoinHandle<()>) {
    let abort = HookAbortSignal::new();
    let seen = Arc::clone(seen);
    let handle = spawn_inbox_poller(mailbox(root, AGENT), 200, who, abort.clone(), move |m| {
        seen.lock().unwrap().push((who, m.body.clone()));
        true
    });
    (abort, handle)
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn bodies_by(seen: &Seen, who: &str) -> Vec<String> {
    let mut bodies: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(by, _)| *by == who)
        .map(|(_, body)| body.clone())
        .collect();
    bodies.sort();
    bodies
}

#[test]
fn a_second_instance_of_an_agent_stands_by_and_takes_over_without_replay() {
    let root = tmp_root();
    let alice = mailbox(&root, "alice");
    let bob = mailbox(&root, "bob");
    let seen: Seen = Arc::default();

    alice.send(note("alice", "a-1")).unwrap();
    bob.send(note("bob", "b-1")).unwrap();

    // Two instances of the same agent, started half a discovery interval
    // apart so each finds a new conversation first about half the time. That
    // is what would split an agent's conversations between them.
    let (first_stop, first) = start(&root, "first", &seen);
    std::thread::sleep(Duration::from_millis(500));
    let (second_stop, second) = start(&root, "second", &seen);
    wait_until("the first receiver to take both conversations", || {
        seen.lock().unwrap().len() == 2
    });
    assert_eq!(receiver_state(AGENT), Some(ReceiverState::Receiving));

    alice.send(note("alice", "a-2")).unwrap();
    bob.send(note("bob", "b-2")).unwrap();
    for peer in ["carol", "dave", "erin", "frank"] {
        mailbox(&root, peer)
            .send(note(peer, &format!("{peer}-1")))
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));
    }
    wait_until("every new message", || seen.lock().unwrap().len() == 8);
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(
        bodies_by(&seen, "first"),
        ["a-1", "a-2", "b-1", "b-2", "carol-1", "dave-1", "erin-1", "frank-1"],
        "the agent's one receiver takes every conversation, new ones included"
    );
    assert!(
        bodies_by(&seen, "second").is_empty(),
        "a standing-by instance must not receive: {seen:?}"
    );

    // The first stops; the second takes over, from where the first left off.
    first_stop.abort();
    first.join().unwrap();
    alice.send(note("alice", "a-3")).unwrap();
    bob.send(note("bob", "b-3")).unwrap();
    wait_until("the second receiver to take over", || {
        seen.lock().unwrap().len() == 10
    });
    std::thread::sleep(Duration::from_millis(1_500));
    assert_eq!(
        bodies_by(&seen, "second"),
        ["a-3", "b-3"],
        "the instance taking over resumes at the read position, without replay"
    );
    assert_eq!(receiver_state(AGENT), Some(ReceiverState::Receiving));

    second_stop.abort();
    second.join().unwrap();
    assert_eq!(receiver_state(AGENT), None);
    let _ = std::fs::remove_dir_all(&root);
}
