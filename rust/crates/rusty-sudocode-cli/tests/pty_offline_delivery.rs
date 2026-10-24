//! Offline Agent delivery through the native filesystem, before any receiver
//! runtime exists. The same real PTY journey runs with mock and live providers.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::{local_pair_root_in, Mailbox};
use runtime::{ContentBlock, MessageRole, SessionStore, StdFsBackend};

#[test]
fn persisted_message_is_consumed_when_the_receiver_first_starts() {
    let env = TestEnv::new("offline-first-start");
    let recipient = "offline-recipient".to_string();
    let settings_path = env.config_home().join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["agentName"] = serde_json::json!(recipient);
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    let root = local_pair_root_in(env.config_home());
    let inbox = Mailbox::workspace_local(&root, recipient.clone());
    let body = env.prompt(
        "Reply with exactly OFFLINE_RECEIPT_7319.",
        "single_turn_text",
    );
    let sender = "offline-sender";
    let outbox = Mailbox::workspace_local(&root, sender.into());
    outbox
        .send(MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: sender.into(),
            to: recipient,
            body: body.clone(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.into(),
            request_id: None,
        })
        .expect("persist delivery without a receiver runtime");
    assert!(std::fs::read_to_string(inbox.transcript_path(sender))
        .unwrap()
        .contains("OFFLINE_RECEIPT_7319"));
    assert_eq!(inbox.read_position(sender).unwrap(), None);

    let mut receiver = env.spawn_with_env(
        &["--permission-mode", "workspace-write"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let recorded = inbox.read_position(sender).unwrap().unwrap_or(0);
        if recorded > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "offline delivery was not committed after a successful turn:\n{}",
            receiver.render(|screen| screen.contents())
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let receipt = if env.is_mock() {
        "The answer is 4"
    } else {
        "OFFLINE_RECEIPT_7319"
    };
    // The durable cursor can commit before the terminal renders its next frame.
    common::expect_screen(
        &receiver,
        |screen| screen.contains("Message from offline-sender") && screen.contains(receipt),
        common::DEFAULT_TIMEOUT,
        "the persisted offline turn renders its receipt",
    );
    receiver.send("/exit\r").expect("exit receiver");
    assert_eq!(receiver.expect_eof().expect("receiver exits"), 0);
    let store = SessionStore::from_cwd_with(env.workspace_root(), Arc::new(StdFsBackend)).unwrap();
    let saved = store.latest_session().unwrap();
    let session = store.load_session(&saved.id).unwrap().session;
    assert!(session.messages().iter().any(|message| {
        message.role == MessageRole::User
            && message.blocks.iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("OFFLINE_RECEIPT_7319"))
            })
    }));
    let persisted_reply = outbox
        .poll_conversation("offline-recipient", 0, 0)
        .unwrap()
        .0
        .iter()
        .any(|message| message.from == "offline-recipient" && message.body.trim() == receipt);
    let persisted_assistant_text = session.messages().iter().any(|message| {
        message.role == MessageRole::Assistant
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text.trim() == receipt))
    });
    assert!(
        persisted_reply || persisted_assistant_text,
        "the delivery cursor requires the actual model receipt in durable history; saved messages: {:?}",
        session.messages()
    );
    eprintln!("PASS: durable delivery existed before the receiver PID; successful {} PTY turn committed its cursor", if env.is_mock() { "mock" } else { "live" });
}
