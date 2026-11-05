//! Fires due cron jobs.
//!
//! A fire is a message, not a separate execution path: an `Agent` job sends a
//! [`kinds::SCHEDULED`](crate::agent_mailbox::kinds::SCHEDULED) envelope through
//! the host session's own peer mailbox, the one `send` uses, so the target
//! receives it like any peer message. It is steered into a running turn or
//! queued behind it per the job's `delivery`, or it waits in the durable inbox
//! of an agent that is not running. A `NewSession` job starts a headless
//! `scode -p` session in the job's directory.
//!
//! Every long-lived scode process (the REPL, the ACP server, `scode cron
//! daemon`) runs [`spawn`], and they share one store. An exclusive lease file
//! makes exactly one of them fire. The OS releases it when the holder exits,
//! and another process takes over within one tick.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::agent_mailbox::{kinds, MailboxEnvelope};
use crate::cron_registry::{CronEntry, CronTarget};
use crate::cron_schedule;
use crate::cron_service;
use crate::mailbox::Mailbox;

/// How often the lease holder looks for due jobs, and a waiting process tries
/// the lease. `SUDOCODE_CRON_TICK_MS` shortens it for tests.
const DEFAULT_TICK: Duration = Duration::from_secs(30);

/// A run more than this late (the machine was asleep, or nothing was running)
/// is recorded as missed instead of firing long after its time.
const MISSED_GRACE_SECS: u64 = 600;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn tick_interval() -> Duration {
    std::env::var("SUDOCODE_CRON_TICK_MS")
        .ok()
        .and_then(|ms| ms.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_TICK, Duration::from_millis)
}

/// The lease beside the store, so a test store has its own.
fn lease_path() -> PathBuf {
    cron_service::store_path().with_file_name("cron-scheduler.lock")
}

/// Held for as long as this process is the one that fires.
pub struct Lease(#[allow(dead_code)] std::fs::File);

/// Take the scheduler lease if no other process holds it.
#[must_use]
pub fn try_lease() -> Option<Lease> {
    let path = lease_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.try_lock().ok()?;
    Some(Lease(file))
}

/// Starts a fresh session for a `NewSession` job.
type StartSession = Arc<dyn Fn(&CronEntry) -> Result<(), String> + Send + Sync>;

/// What a fire needs from the process it runs in.
#[derive(Clone)]
pub struct Firing {
    mailbox: Arc<Mailbox>,
    new_session: StartSession,
}

impl Firing {
    /// Deliver through `mailbox`, this session's peer namespace; start new
    /// sessions as headless children of this executable.
    #[must_use]
    pub fn new(mailbox: Arc<Mailbox>) -> Self {
        Self {
            mailbox,
            new_session: Arc::new(start_headless_session),
        }
    }

    #[must_use]
    pub fn with_new_session(
        mut self,
        start: impl Fn(&CronEntry) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.new_session = Arc::new(start);
        self
    }

    fn fire(&self, entry: &CronEntry) -> Result<(), String> {
        match &entry.target {
            CronTarget::Agent { name } => self.mailbox.send(scheduled_envelope(entry, name)),
            CronTarget::NewSession => (self.new_session)(entry),
        }
    }
}

/// The message a fire sends. `from` is left to the mailbox, which stamps the
/// sending session; the job's name travels as the summary.
#[must_use]
pub fn scheduled_envelope(entry: &CronEntry, to: &str) -> MailboxEnvelope {
    let label = entry
        .name
        .as_deref()
        .or(entry.description.as_deref())
        .filter(|label| !label.trim().is_empty())
        .map_or_else(
            || entry.cron_id.clone(),
            |label| format!("{label} ({})", entry.cron_id),
        );
    MailboxEnvelope {
        delivery: entry.delivery,
        from: String::new(),
        to: to.to_string(),
        body: entry.prompt.clone(),
        summary: Some(label),
        timestamp: 0,
        color: None,
        kind: kinds::SCHEDULED.to_string(),
        request_id: None,
    }
}

/// A fresh `scode -p` session for the job's message, in its directory.
fn start_headless_session(entry: &CronEntry) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("locate scode: {e}"))?;
    let mut command = std::process::Command::new(exe);
    command
        .args([
            "-p",
            &entry.prompt,
            "--permission-mode",
            "danger-full-access",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // The session is the job's own, not a continuation of the process
        // that happened to hold the lease.
        .env_remove("SUDOCODE_PLAN_FILE");
    if let Some(cwd) = &entry.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("start session for {}: {e}", entry.cron_id))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Advance `entry` past this run before firing it, so a process that dies
/// after the send cannot fire the same run twice. One-shot jobs disable.
fn claim(entry: &CronEntry, now: u64) -> Option<u64> {
    let registry = cron_service::registry();
    let next = if cron_schedule::is_one_shot(entry) {
        None
    } else {
        cron_schedule::next_run_after(entry, now)
    };
    let _ = registry.set_next_run(&entry.cron_id, next);
    if cron_schedule::is_one_shot(entry) {
        let _ = registry.disable(&entry.cron_id);
    }
    next
}

fn record(entry: &CronEntry, outcome: Result<(), String>, next: Option<u64>) -> String {
    let registry = cron_service::registry();
    match outcome {
        Ok(()) => {
            let _ = registry.record_result(&entry.cron_id, "ok", None, next);
            "ok".to_string()
        }
        Err(error) => {
            let _ = registry.record_result(&entry.cron_id, "error", Some(&error), next);
            format!("error: {error}")
        }
    }
}

/// Fire one job now, whatever its schedule, and record the outcome.
pub fn trigger(cron_id: &str, firing: &Firing) -> Result<String, String> {
    let entry = cron_service::get(cron_id).ok_or_else(|| format!("cron not found: {cron_id}"))?;
    let next = if cron_schedule::is_one_shot(&entry) {
        None
    } else {
        entry.next_run_at
    };
    Ok(record(&entry, firing.fire(&entry), next))
}

/// One pass over the store at `now`: every due job is claimed, then fired, or
/// recorded as missed when it is more than the grace period late. Returns
/// `(cron_id, status)` for each job it handled.
#[must_use = "the statuses report what fired"]
pub fn tick(now: u64, firing: &Firing) -> Vec<(String, String)> {
    let due: Vec<CronEntry> = cron_service::list()
        .into_iter()
        .filter(|entry| cron_schedule::is_due(entry, now))
        .collect();
    let mut handled = Vec::with_capacity(due.len());
    for entry in due {
        let late = now.saturating_sub(entry.next_run_at.unwrap_or(now));
        let next = claim(&entry, now);
        let status = if late > MISSED_GRACE_SECS {
            let _ = cron_service::registry().record_result(&entry.cron_id, "missed", None, next);
            "missed".to_string()
        } else {
            record(&entry, firing.fire(&entry), next)
        };
        handled.push((entry.cron_id, status));
    }
    handled
}

/// Start this process's scheduler unless one is already running. A process
/// hosts several sessions (ACP) or restarts its session (the REPL); it still
/// fires each job once.
pub fn start_once(firing: impl FnOnce() -> Firing) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        let _scheduler = spawn(firing());
    });
}

/// Run the scheduler on a background thread for the life of the process:
/// take the lease when it is free, and tick while holding it.
#[must_use = "join it to run the scheduler in the foreground"]
pub fn spawn(firing: Firing) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("cron-scheduler".into())
        .spawn(move || {
            let interval = tick_interval();
            let mut lease = None;
            loop {
                if lease.is_none() {
                    lease = try_lease();
                }
                if lease.is_some() {
                    let _ = tick(now_secs(), &firing);
                }
                std::thread::sleep(interval);
            }
        })
        .expect("spawn cron scheduler thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron_registry::{CronCreateParams, CronKind};
    use std::sync::Mutex;

    struct Store(tempfile::TempDir);

    fn store() -> (std::sync::MutexGuard<'static, ()>, Store) {
        let guard = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SUDOCODE_CRON_STORE", dir.path().join("crons.json"));
        (guard, Store(dir))
    }

    impl Drop for Store {
        fn drop(&mut self) {
            std::env::remove_var("SUDOCODE_CRON_STORE");
        }
    }

    fn mailbox(root: &std::path::Path, me: &str) -> Arc<Mailbox> {
        Arc::new(Mailbox::workspace_local(root, me.to_string()))
    }

    fn job(target: CronTarget) -> CronEntry {
        cron_service::create(CronCreateParams {
            schedule: "60".into(),
            kind: CronKind::Every,
            prompt: "check the build".into(),
            name: Some("build check".into()),
            target,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn a_due_agent_job_is_delivered_once_and_re_armed() {
        let (_guard, store) = store();
        let me = mailbox(store.0.path(), "alpha");
        let entry = job(CronTarget::Agent {
            name: "alpha".into(),
        });
        let due_at = entry.next_run_at.unwrap();
        let firing = Firing::new(Arc::clone(&me));

        assert!(tick(due_at - 1, &firing).is_empty(), "not due yet");
        assert_eq!(
            tick(due_at, &firing),
            [(entry.cron_id.clone(), "ok".into())]
        );
        assert!(tick(due_at, &firing).is_empty(), "claimed: not fired twice");

        let (got, _) = me.poll_conversation("alpha", 0, 0).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, kinds::SCHEDULED);
        assert_eq!(got[0].body, "check the build");
        assert_eq!(got[0].delivery, crate::turn_input::InputDelivery::Queue);
        assert_eq!(
            got[0].summary.as_deref(),
            Some(&*format!("build check ({})", entry.cron_id))
        );
        let after = cron_service::get(&entry.cron_id).unwrap();
        assert_eq!((after.run_count, after.next_run_at), (1, Some(due_at + 60)));
    }

    #[test]
    fn a_run_far_past_its_time_is_missed_not_fired() {
        let (_guard, store) = store();
        let me = mailbox(store.0.path(), "alpha");
        let entry = job(CronTarget::Agent {
            name: "alpha".into(),
        });
        let late = entry.next_run_at.unwrap() + MISSED_GRACE_SECS + 1;
        let firing = Firing::new(Arc::clone(&me));
        assert_eq!(
            tick(late, &firing),
            [(entry.cron_id.clone(), "missed".into())]
        );
        let (delivered, _) = me.poll_conversation("alpha", 0, 0).unwrap();
        assert_eq!(delivered.len(), 0, "a missed run sends nothing");
        let after = cron_service::get(&entry.cron_id).unwrap();
        assert_eq!(after.last_status.as_deref(), Some("missed"));
        assert!(after.next_run_at.unwrap() > late, "armed for the next run");
    }

    #[test]
    fn a_new_session_job_starts_a_session_and_a_failure_is_recorded() {
        let (_guard, store) = store();
        let started = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&started);
        let firing = Firing::new(mailbox(store.0.path(), "alpha")).with_new_session(move |entry| {
            seen.lock().unwrap().push(entry.prompt.clone());
            Err("no model configured".into())
        });
        let entry = job(CronTarget::NewSession);
        let status = trigger(&entry.cron_id, &firing).unwrap();
        assert_eq!(status, "error: no model configured");
        assert_eq!(*started.lock().unwrap(), ["check the build"]);
        let after = cron_service::get(&entry.cron_id).unwrap();
        assert_eq!(after.last_error.as_deref(), Some("no model configured"));
    }

    #[test]
    fn only_one_process_holds_the_lease() {
        let (_guard, _store) = store();
        let first = try_lease().expect("free");
        assert!(try_lease().is_none(), "held");
        drop(first);
        assert!(try_lease().is_some(), "released with its holder");
    }
}
