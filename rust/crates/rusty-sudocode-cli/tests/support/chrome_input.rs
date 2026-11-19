//! Shared real-CLI resize/input journeys for workspace and native-host acceptance.

use super::common;
use common::TestEnv;

/// A no-op F6 follows the tested burst in the same native input stream.
/// Seeing its local observation establishes delivery, unlike seeing a repaint.
struct InputDelivery {
    path: std::path::PathBuf,
    acknowledged: u64,
}

impl InputDelivery {
    fn new(env: &TestEnv) -> Self {
        Self {
            path: env.workspace_root().join("input-observations.jsonl"),
            acknowledged: 0,
        }
    }

    fn mark(&mut self) {
        self.acknowledged = std::fs::read_to_string(&self.path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|record| record["sequence"].as_u64())
            .max()
            .expect("the UI observed the preparatory input");
    }

    fn expect_blocked_burst(
        &mut self,
        sess: &mut pty_expect::PtySession,
        event: &str,
        count: usize,
    ) {
        sess.send("\x1b[17~").unwrap();
        let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
        loop {
            let observations = std::fs::read_to_string(&self.path).unwrap_or_default();
            let acknowledgment = observations
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|record| {
                    record["event"] == "function"
                        && record["function"] == 6
                        && record["sequence"]
                            .as_u64()
                            .is_some_and(|sequence| sequence > self.acknowledged)
                });
            if let Some(record) = acknowledgment {
                let sequence = record["sequence"].as_u64().unwrap();
                let delivered: Vec<_> = observations
                    .lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .filter(|record| {
                        record["event"] == event
                            && record["sequence"].as_u64().is_some_and(|observed| {
                                observed > self.acknowledged && observed < sequence
                            })
                    })
                    .collect();
                assert_eq!(
                    delivered.len(),
                    count,
                    "the actual burst must precede its acknowledgment: {observations}"
                );
                assert!(
                    delivered.iter().chain(std::iter::once(&record)).all(|record|
                        record["reviewable"] == false || record["frame_current"] == false),
                    "input delivery must be observed before restoring a reviewable frame: {observations}"
                );
                assert!(
                    !observations.contains("MustNotAppear")
                        && !observations.contains("DraftHeadTail"),
                    "input diagnostics must omit typed and pasted contents"
                );
                self.acknowledged = sequence;
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native input was not observed within {:?}\nObservations:\n{observations}\nPTY:\n{}",
                common::DEFAULT_TIMEOUT,
                sess.render(common::screen_contents)
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

pub(super) fn exit(sess: &mut pty_expect::PtySession) {
    // Both workflows finish on an empty input. Do not send a redundant
    // parent-owned Ctrl-U in the same key burst as TextInput-owned typing.
    common::expect_screen_settled(
        sess,
        |screen| screen.contains('❯') && common::input_line_of(screen).is_empty(),
        common::DEFAULT_TIMEOUT,
        "empty input before exit",
    );
    sess.send("/exit").unwrap();
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

pub(super) fn assert_resize_confirmation_rejected() {
    let env = TestEnv::new("chrome-budget-resize-confirm-burst");
    let mut delivery = InputDelivery::new(&env);
    let mut sess = env.spawn_with_env(
        &[],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SCODE_TRACE_INPUT", delivery.path.to_str().unwrap()),
        ],
    );
    common::expect_screen(&sess, |s| s.contains('❯'), common::DEFAULT_TIMEOUT, "input");
    sess.send("/model\r").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Select model"),
        common::DEFAULT_TIMEOUT,
        "picker before resize",
    );
    for _ in 0..3 {
        delivery.mark();
        sess.resize(6, 80).unwrap();
        // No paint, sleep or polling between resize and confirmation: admission
        // must use the new geometry even when events arrive in the same batch.
        sess.send("\r").unwrap();
        delivery.expect_blocked_burst(&mut sess, "enter", 1);
        common::expect_screen_settled(
            &sess,
            |s| s.contains("Enlarge terminal to review"),
            common::DEFAULT_TIMEOUT,
            "warning after resize/confirm burst",
        );
        sess.resize(40, 80).unwrap();
        common::expect_screen_settled(
            &sess,
            |s| {
                common::input_and_footer_of(s).is_some_and(|area| {
                    area.contains("Select model") && !area.contains("Enlarge terminal to review")
                })
            },
            common::DEFAULT_TIMEOUT,
            "unconfirmed picker after immediate resize/Enter",
        );
    }
    sess.send("\x1b").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains('❯') && !s.contains("Select model"),
        common::DEFAULT_TIMEOUT,
        "cancel picker",
    );
    exit(&mut sess);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

pub(super) fn assert_resize_edit_burst_rejected(burst: &str, name: &str) {
    let env = TestEnv::new(name);
    let mut delivery = InputDelivery::new(&env);
    let mut sess = env.spawn_with_env(
        &[],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SCODE_TRACE_INPUT", delivery.path.to_str().unwrap()),
        ],
    );
    common::expect_screen(&sess, |s| s.contains('❯'), common::DEFAULT_TIMEOUT, "input");
    sess.send("DraftHeadTail").unwrap();
    common::expect_input_line(&sess, "DraftHeadTail", common::DEFAULT_TIMEOUT, "draft");
    sess.send("\x1b[H\x1b[C\x1b[C\x1b[C\x1b[C\x1b[CX").unwrap();
    common::expect_input_line(
        &sess,
        "DraftXHeadTail",
        common::DEFAULT_TIMEOUT,
        "cursor probe",
    );
    sess.send("\x7f").unwrap();
    common::expect_input_line(
        &sess,
        "DraftHeadTail",
        common::DEFAULT_TIMEOUT,
        "remove probe",
    );
    delivery.mark();
    sess.resize(8, 12).unwrap();
    // Deliberately do not wait for the warning before this input burst.
    sess.send(burst).unwrap();
    let (event, count) = if burst.starts_with("\x1b[200~") {
        ("paste", 1)
    } else {
        ("character", burst.chars().count())
    };
    delivery.expect_blocked_burst(&mut sess, event, count);
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Enlarge") && s.contains("terminal"),
        common::DEFAULT_TIMEOUT,
        "warning after resize/typing burst",
    );
    sess.resize(24, 80).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| common::input_line_of(s) == "DraftHeadTail",
        common::DEFAULT_TIMEOUT,
        "draft unchanged after immediate resize/typing",
    );
    sess.send("Kept").unwrap();
    common::expect_input_line(
        &sess,
        "DraftKeptHeadTail",
        common::DEFAULT_TIMEOUT,
        "cursor preserved after immediate resize/typing",
    );
    sess.send_ctrl('u').unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains('❯') && common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "cleared draft",
    );
    exit(&mut sess);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}
