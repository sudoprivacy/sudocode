//! The host-local outbox that holds a send whose delivery could not be
//! confirmed until a retry settles it.
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::send_outbox::{
    SendOutbox, SendOutcome, SettledSend, MAX_AGE_SECS, MAX_ATTEMPTS, MAX_QUEUED,
};

fn envelope(send_id: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        send_id: Some(send_id.into()),
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: "alice".into(),
        to: "bob".into(),
        body: format!("body of {send_id}"),
        summary: None,
        timestamp: 100,
        color: None,
        kind: "message".into(),
        request_id: None,
    }
}

#[test]
fn a_queued_send_is_listed_with_one_attempt_and_queued_once() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path());
    assert!(
        outbox.list().unwrap().is_empty(),
        "a missing outbox is empty"
    );
    outbox.queue(&envelope("first")).unwrap();
    outbox.queue(&envelope("first")).unwrap();
    let queued = outbox.list().unwrap();
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].envelope, envelope("first"));
    assert_eq!(queued[0].attempts, 1);
    assert!(queued[0].first_attempt_secs > 0);
}

#[test]
fn a_send_id_that_cannot_name_a_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path().join("outbox"));
    for unsafe_id in ["../escape", "a/b", ""] {
        let error = outbox.queue(&envelope(unsafe_id)).unwrap_err();
        assert!(error.contains("cannot name a queued send"), "{error}");
    }
    let mut no_id = envelope("x");
    no_id.send_id = None;
    assert!(outbox.queue(&no_id).is_err());
    assert!(!dir.path().join("escape.json").exists());
    assert!(outbox.list().unwrap().is_empty(), "nothing was queued");
}

#[test]
fn a_bump_counts_an_attempt_and_keeps_the_send_queued() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path());
    outbox.queue(&envelope("bumped")).unwrap();
    let entry = outbox.list().unwrap().remove(0);
    let abandoned = outbox.bump(&entry, entry.first_attempt_secs).unwrap();
    assert!(!abandoned);
    let queued = outbox.list().unwrap();
    assert_eq!(queued[0].attempts, 2);
    assert_eq!(queued[0].first_attempt_secs, entry.first_attempt_secs);
    assert!(
        outbox.take_settled().is_empty(),
        "a bumped send has not ended"
    );
}

#[test]
fn a_send_is_abandoned_at_the_attempt_limit() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path());
    outbox.queue(&envelope("tired")).unwrap();
    let now = outbox.list().unwrap()[0].first_attempt_secs;
    for _ in 1..MAX_ATTEMPTS - 1 {
        let entry = outbox.list().unwrap().remove(0);
        assert!(!outbox.bump(&entry, now).unwrap());
    }
    let entry = outbox.list().unwrap().remove(0);
    assert_eq!(entry.attempts, MAX_ATTEMPTS - 1);
    assert!(outbox.bump(&entry, now).unwrap());
    assert!(
        outbox.list().unwrap().is_empty(),
        "an abandoned send leaves the queue"
    );
    assert_eq!(
        outbox.take_settled(),
        [SettledSend {
            to: "bob".into(),
            send_id: "tired".into(),
            outcome: SendOutcome::Abandoned,
            attempts: MAX_ATTEMPTS,
        }]
    );
}

#[test]
fn a_send_is_abandoned_at_the_age_limit() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path());
    outbox.queue(&envelope("old")).unwrap();
    let entry = outbox.list().unwrap().remove(0);
    assert!(!entry.past_limits(entry.first_attempt_secs + MAX_AGE_SECS - 1));
    assert!(outbox
        .bump(&entry, entry.first_attempt_secs + MAX_AGE_SECS)
        .unwrap());
    assert!(
        outbox.list().unwrap().is_empty(),
        "an abandoned send leaves the queue"
    );
    let settled = outbox.take_settled();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].outcome, SendOutcome::Abandoned);
    assert_eq!(settled[0].attempts, 2);
    assert_eq!(
        settled[0].to_string(),
        "Earlier queued send old to bob: abandoned after 2 attempts — resend if still needed"
    );
}

#[test]
fn a_settled_send_leaves_the_queue_and_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path());
    outbox.queue(&envelope("landed")).unwrap();
    outbox.queue(&envelope("waiting")).unwrap();
    let entry = outbox
        .list()
        .unwrap()
        .into_iter()
        .find(|entry| entry.send_id() == "landed")
        .unwrap();
    outbox.settle(&entry, SendOutcome::Delivered).unwrap();
    let left: Vec<String> = outbox
        .list()
        .unwrap()
        .iter()
        .map(|entry| entry.send_id().to_string())
        .collect();
    assert_eq!(left, ["waiting"]);
    let settled = outbox.take_settled();
    assert_eq!(
        settled,
        [SettledSend {
            to: "bob".into(),
            send_id: "landed".into(),
            outcome: SendOutcome::Delivered,
            attempts: 1,
        }]
    );
    assert_eq!(
        settled[0].to_string(),
        "Earlier queued send landed to bob: delivered, its original attempt landed"
    );
    assert!(
        outbox.take_settled().is_empty(),
        "an outcome is reported once"
    );
}

#[test]
fn a_full_outbox_refuses_another_send() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = SendOutbox::new(dir.path());
    for n in 0..MAX_QUEUED {
        outbox.queue(&envelope(&format!("send-{n}"))).unwrap();
    }
    let error = outbox.queue(&envelope("one-too-many")).unwrap_err();
    assert!(
        error.contains(&format!("already holds {MAX_QUEUED} unsent messages")),
        "{error}"
    );
    assert_eq!(outbox.list().unwrap().len(), MAX_QUEUED);
    outbox
        .queue(&envelope("send-0"))
        .expect("a send already queued is not a new one");
}

#[test]
fn mailboxes_that_share_an_agent_name_in_different_namespaces_keep_separate_outboxes() {
    let home = tempfile::tempdir().unwrap();
    let node = SendOutbox::in_config_home(home.path(), "/agents", "alice");
    let workspace = SendOutbox::in_config_home(home.path(), "/work/project/.scode/agents", "alice");
    node.queue(&envelope("queued-on-the-node")).unwrap();
    assert!(
        workspace.list().unwrap().is_empty(),
        "a workspace mailbox must not retry a send queued on the node"
    );
    assert_eq!(
        SendOutbox::in_config_home(home.path(), "/agents", "alice")
            .list()
            .unwrap()
            .len(),
        1,
        "the same namespace and name find the same outbox again"
    );
}
