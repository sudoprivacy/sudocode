//! Peer-messaging health reaches the terminal as state, not raw errors. An
//! outage that heals within the grace period stays silent. A longer one shows
//! in the footer until receiving recovers. `/status` reports the current
//! health beside the historical last error, which survives recovery.
//!
//! The failure is real: the agent's conversation list is replaced by a regular
//! file, so the receiver's discovery `read_dir` fails on every platform.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use runtime::mailbox::{local_pair_root_in, InboxConvention};

const AGENT: &str = "health-recipient";
/// Long enough that a blip observed and repaired at once stays inside it.
const GRACE: Duration = Duration::from_secs(10);
const SCOPE: &str = "conversation discovery";

/// The receiver's conversation list on disk, breakable and repairable.
struct ConversationList {
    dir: PathBuf,
    aside: PathBuf,
}

impl ConversationList {
    fn of(env: &TestEnv) -> Self {
        let root = local_pair_root_in(env.config_home());
        let dir = PathBuf::from(
            InboxConvention::new(root.to_string_lossy().into_owned()).chat_list_dir(AGENT),
        );
        Self {
            aside: dir.with_extension("aside"),
            dir,
        }
    }

    fn break_listing(&self) {
        if self.dir.is_dir() {
            std::fs::rename(&self.dir, &self.aside).unwrap();
        }
        std::fs::create_dir_all(self.dir.parent().unwrap()).unwrap();
        std::fs::write(&self.dir, b"not a directory").unwrap();
    }

    fn repair(&self) {
        std::fs::remove_file(&self.dir).unwrap();
        if self.aside.exists() {
            std::fs::rename(&self.aside, &self.dir).unwrap();
        }
    }
}

fn start(env: &TestEnv) -> PtySession {
    let settings_path = env.config_home().join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["agentName"] = serde_json::json!(AGENT);
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    let grace = GRACE.as_millis().to_string();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            // The default REPL; the harness otherwise selects the legacy one,
            // which has no footer.
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_A2A_DEGRADED_GRACE_MS", grace.as_str()),
        ],
    );
    session.resize(60, 160).unwrap();
    status_until(&mut session, "the receiver starts healthy", |status| {
        status.receiving && status.health == "ok"
    });
    session
}

struct Status {
    receiving: bool,
    health: String,
    failing_detail: bool,
    history: bool,
    footer: String,
}

/// Run `/status` and read the newest report: output is appended, so the last
/// matching line on the settled screen belongs to this run.
fn status(session: &mut PtySession) -> Status {
    session.send("/status").unwrap();
    common::expect_screen_settled(
        session,
        |screen| common::input_line_of(screen) == "/status",
        common::DEFAULT_TIMEOUT,
        "status command typed",
    );
    session.send("\r").unwrap();
    let screen = common::expect_screen_settled(
        session,
        |screen| {
            screen.contains("Peers")
                && screen.contains("  Health ")
                && common::input_line_of(screen).is_empty()
        },
        common::DEFAULT_TIMEOUT,
        "status report returns to an empty prompt",
    );
    let last = |label: &str| {
        screen
            .lines()
            .rev()
            .find(|line| line.trim_start().starts_with(label))
            .map(|line| line.trim_start()[label.len()..].trim().to_string())
            .unwrap_or_default()
    };
    Status {
        receiving: last("Receiver").starts_with("receiving"),
        health: last("Health"),
        failing_detail: screen.contains(&format!("    {SCOPE}: list conversations at")),
        history: !last("Last RX error").is_empty(),
        footer: common::input_and_footer_of(&screen).unwrap_or_default(),
    }
}

fn status_until(session: &mut PtySession, what: &str, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + GRACE * 4;
    loop {
        let status = status(session);
        if done(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: last status health={:?} footer={:?}\n{}",
            status.health,
            status.footer,
            session.render(common::screen_contents)
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn finish(mut session: PtySession) {
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn outage_shorter_than_the_grace_period_stays_silent() {
    let env = TestEnv::new_mock("a2a-health-blip");
    let mut session = start(&env);
    let list = ConversationList::of(&env);
    let broken = Instant::now();
    list.break_listing();
    // Observed, not merely assumed: the report names the failing scope while
    // health stays ok, because the outage is younger than the grace period.
    let seen = status_until(
        &mut session,
        "the receiver observes the failure",
        |status| status.health.starts_with("ok; retrying") && status.health.contains(SCOPE),
    );
    assert!(
        broken.elapsed() < GRACE,
        "the blip outlasted the grace period, so this run proves nothing"
    );
    assert!(!seen.footer.contains("A2A:"), "footer: {}", seen.footer);
    list.repair();
    let healed = status_until(&mut session, "receiving recovers", |status| {
        status.health == "ok"
    });
    assert!(!healed.footer.contains("A2A:"), "footer: {}", healed.footer);
    finish(session);
}

#[test]
fn persistent_outage_is_shown_until_receiving_recovers() {
    let env = TestEnv::new_mock("a2a-health-persistent");
    let mut session = start(&env);
    let list = ConversationList::of(&env);
    list.break_listing();
    let degraded = status_until(
        &mut session,
        "health degrades after the grace period",
        |status| status.health.starts_with("degraded:") && status.footer.contains("A2A:"),
    );
    assert!(
        degraded.health.contains(SCOPE),
        "health: {}",
        degraded.health
    );
    assert!(
        degraded.failing_detail,
        "the failing scope's detail is listed"
    );
    assert!(
        degraded.footer.contains(&format!("A2A: {SCOPE} failing")),
        "footer: {}",
        degraded.footer
    );
    list.repair();
    // No restart: the footer clears itself and the report returns to ok, while
    // the historical last error stays inspectable.
    let healed = status_until(
        &mut session,
        "health recovers without a restart",
        |status| status.health == "ok" && !status.footer.contains("A2A:"),
    );
    assert!(healed.history, "the last receiver error is kept as history");
    finish(session);
}
