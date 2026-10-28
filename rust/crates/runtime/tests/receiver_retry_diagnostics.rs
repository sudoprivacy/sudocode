//! Real receiver threads, backend locks, reader-register writes and delivery.
//! Only the two register writes are faulted, at the existing backend boundary.
//! Unlike a disk renew() test, this actually enters the claiming-reader Err
//! branch. It tests diagnostics there, not a real network/daemon outage.

#[allow(dead_code)]
#[path = "common/lease_fault.rs"]
mod lease_fault;

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lease_fault::{FaultyBackend, Probe};
use runtime::agent_mailbox::{kinds, MailboxEnvelope};
use runtime::fs_backend::StdFsBackend;
use runtime::mailbox::{
    spawn_inbox_poller, spawn_inbox_poller_with_notices, InboxConvention, Mailbox,
};
use runtime::HookAbortSignal;

const AGENT: &str = "retry-receiver";

fn wait_for(label: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {label}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn note(peer: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: peer.into(),
        to: AGENT.into(),
        body: format!("original message from {peer}"),
        summary: None,
        timestamp: 0,
        color: None,
        kind: kinds::MESSAGE.into(),
        request_id: None,
    }
}

#[test]
fn child_receiver() {
    let Ok(root) = std::env::var("SCODE_RETRY_FIXTURE_ROOT") else {
        return;
    };
    let convention = InboxConvention::new(root.clone());
    let probe = Probe::new();
    let paths: Vec<_> = ["peer-a", "peer-b"]
        .into_iter()
        .map(|peer| {
            Mailbox::new(Arc::new(StdFsBackend), peer.into(), convention.clone())
                .send(note(peer))
                .unwrap();
            let path = convention.reader_path(AGENT, peer, AGENT);
            probe.fail_register_writes(&path, true);
            path
        })
        .collect();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let abort = HookAbortSignal::new();
    let mailbox = Arc::new(Mailbox::new(
        Arc::new(FaultyBackend::new(
            Arc::new(StdFsBackend),
            Arc::clone(&probe),
        )),
        AGENT.into(),
        convention,
    ));
    let consume = {
        let seen = Arc::clone(&seen);
        move |message: &MailboxEnvelope| {
            seen.lock().unwrap().push(message.body.clone());
            true
        }
    };
    let receiver = if std::env::var_os("SCODE_RETRY_STATUS_ONLY").is_some() {
        spawn_inbox_poller(mailbox, 50, "retry-fixture", abort.clone(), consume)
    } else {
        spawn_inbox_poller_with_notices(
            mailbox,
            50,
            "retry-fixture",
            abort.clone(),
            telemetry::RetryNotices::Terminal,
            consume,
        )
    };
    wait_for("both actual claim paths retried", || {
        paths.iter().all(|p| probe.register_write_attempts(p) >= 4)
    });
    // Retain this observation before the optional real-minute run rolls the
    // bounded ring past the early peer's failures. Assert only after cleanup.
    let initial_events = runtime::mailbox::delivery_events();
    let failure_started = Instant::now();
    let first_failures = probe.fail_register_writes(&paths[0], false);
    wait_for("first peer delivered", || seen.lock().unwrap().len() == 1);
    // Keep the second real reader failing past the recovery quiet period.
    let attempts = probe.register_write_attempts(&paths[1]);
    wait_for("second reader continues retrying", || {
        probe.register_write_attempts(&paths[1]) >= attempts + 50
    });
    if std::env::var_os("SCODE_RETRY_FIXTURE_REMINDER").is_some() {
        // Exercise the real production minute, not a shortened test policy.
        while failure_started.elapsed() < Duration::from_secs(63) {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    eprintln!("FIXTURE_PARTIAL_RECOVERY");
    let second_failures = probe.fail_register_writes(&paths[1], false);
    println!(
        "FIXTURE_EXPECTED_FAILURES {}",
        serde_json::json!({"peer-a":first_failures,"peer-b":second_failures})
    );
    wait_for("both original messages delivered", || {
        seen.lock().unwrap().len() == 2
    });
    for path in &paths {
        wait_for("natural reader commit", || {
            std::fs::read(path)
                .ok()
                .and_then(|raw| {
                    serde_json::from_slice::<runtime::mailbox::ReaderRegister>(&raw).ok()
                })
                .is_some_and(|reader| reader.read_offset > 0)
        });
    }
    // Discovery and empty polls continue making real healthy checks.
    std::thread::sleep(Duration::from_millis(2_300));
    let last_error = runtime::mailbox::last_receiver_error(AGENT)
        .expect("the same real receiver error remains inspectable after recovery");
    assert!(last_error.contains("INJECTED"), "{last_error}");
    println!(
        "FIXTURE_RX_OBSERVATION {}",
        serde_json::json!({"last_error":last_error})
    );
    abort.abort();
    receiver.join().unwrap();
    assert!(
        runtime::mailbox::last_receiver_error(AGENT).is_none(),
        "the existing receiver observation retires with its last owner"
    );
    let mut bodies = seen.lock().unwrap().clone();
    bodies.sort();
    assert_eq!(
        bodies,
        [
            "original message from peer-a",
            "original message from peer-b"
        ]
    );
    for peer in ["peer-a", "peer-b"] {
        assert!(
            initial_events.iter().any(|event| {
                event.peer == peer
                    && event.stall == runtime::mailbox::DeliveryStall::ClaimFailed
                    && event.holder.is_empty()
                    && event.offset.is_none()
                    && event.tail.is_none()
                    && event.loss.is_none()
            }),
            "the same real claim failure must reach the structured consumer: {initial_events:?}"
        );
    }
}

#[test]
fn concurrent_reader_failures_share_one_notice_and_recover_without_lost_messages() {
    let root = std::env::temp_dir().join(format!("receiver-retry-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("diagnostics.jsonl");
    let output = run_fixture(&root, &log);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{stderr}\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        stderr.matches("Message receiving is interrupted;").count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("Message receive checks recovered.").count(),
        1,
        "{stderr}"
    );
    let partial = stderr.find("FIXTURE_PARTIAL_RECOVERY").unwrap();
    assert!(
        !stderr[..partial].contains("checks recovered"),
        "one healthy peer hid another failure: {stderr}"
    );
    assert!(
        !stderr.contains("INJECTED"),
        "raw backend errors leaked to terminal: {stderr}"
    );
    let records = read_records(&log);
    for peer in ["peer-a", "peer-b"] {
        assert!(
            records.iter().any(|record| {
                let props = &record["attributes"];
                props["scope"] == format!("conversation with {peer}")
                    && props["subject"] == AGENT
                    && props["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("write reader register"))
                    && props["context"]["operation"] == "claim"
                    && props["context"]["stall"] == "ClaimFailed"
                    && props["context"]["holder"] == ""
                    && props["context"]["offset"].is_null()
                    && props["context"]["tail"].is_null()
                    && props["context"]["loss"].is_null()
            }),
            "missing real register error for {peer}: {records:?}"
        );
    }
    assert!(records
        .iter()
        .any(|record| record["attributes"]["suppressed_repeats"]
            .as_u64()
            .is_some_and(|n| n >= 3)));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn default_receiver_retains_status_and_log_without_terminal_notices() {
    let root = std::env::temp_dir().join(format!("receiver-status-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("diagnostics.jsonl");
    let output = fixture_command(&root, &log)
        .env("SCODE_RETRY_STATUS_ONLY", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stderr}\n{stdout}");
    assert!(
        !stderr.contains("Message receiving is interrupted;"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Message receive checks recovered."),
        "{stderr}"
    );
    assert!(!stderr.contains("INJECTED"), "{stderr}");
    assert!(stdout.contains("FIXTURE_RX_OBSERVATION"), "{stdout}");
    let records = read_records(&log);
    for peer in ["peer-a", "peer-b"] {
        assert!(
            records.iter().any(|record| {
                let p = &record["attributes"];
                p["context"]["peer"] == peer
                    && p["context"]["operation"] == "claim"
                    && p["error"]
                        .as_str()
                        .is_some_and(|error| error.contains("INJECTED"))
            }),
            "the default receiver must retain the actual cause for {peer}: {records:?}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_unwritable_diagnostic_log_keeps_the_actual_cause_in_one_bounded_notice() {
    let root = std::env::temp_dir().join(format!("receiver-retry-log-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // Opening a directory as the log fails on all three hosts, even as root.
    let output = run_fixture(&root, &root);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("Message receiving is interrupted;").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("Diagnostic log unavailable"), "{stderr}");
    assert!(
        stderr.contains("reader register write transport error"),
        "{stderr}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn a_log_write_failure_does_not_create_a_second_stderr_retry_loop() {
    let root = std::env::temp_dir().join(format!("receiver-retry-full-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // Opening succeeds; the actual write fails. This differs from the directory
    // case and guards against using the sink's per-write stderr fallback here.
    let output = run_fixture(&root, Path::new("/dev/full"));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr.matches("Message receiving is interrupted;").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("Diagnostic log unavailable"), "{stderr}");
    assert!(!stderr.contains("[scode telemetry]"), "{stderr}");
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn a_real_minute_reminder_counts_only_the_actually_suppressed_failures() {
    let root = std::env::temp_dir().join(format!("receiver-retry-minute-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let log = root.join("diagnostics.jsonl");
    let output = fixture_command(&root, &log)
        .env("SCODE_RETRY_FIXTURE_REMINDER", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{stderr}\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        stderr.matches("Message receiving is interrupted;").count(),
        2,
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("Message receive checks recovered.").count(),
        1,
        "{stderr}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (_, expected) = stdout.split_once("FIXTURE_EXPECTED_FAILURES ").unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(expected.lines().next().unwrap()).unwrap();
    let records = read_records(&log);
    for peer in ["peer-a", "peer-b"] {
        let observed: u64 = records
            .iter()
            .filter_map(|record| {
                let props = &record["attributes"];
                (props["scope"] == format!("conversation with {peer}")).then(|| {
                    props["suppressed_repeats"].as_u64().unwrap()
                        + u64::from(props["action"] == "background_retry")
                })
            })
            .sum();
        assert_eq!(
            observed,
            expected[peer].as_u64().unwrap(),
            "{peer}: each failed write must be counted once"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

fn run_fixture(root: &Path, log: &Path) -> std::process::Output {
    fixture_command(root, log).output().unwrap()
}

fn fixture_command(root: &Path, log: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "child_receiver", "--nocapture"])
        .env("SCODE_RETRY_FIXTURE_ROOT", root.to_str().unwrap())
        .env("SCODE_LOG_PATH", log);
    command
}

fn read_records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
