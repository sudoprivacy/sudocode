//! The task browser's Scheduled tab: ← / → switch from Background tasks, and
//! the tab manages the shared cron store (enable/disable, run now, delete).

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use pty_expect::PtySession;
use runtime::cron_registry::{CronCreateParams, CronKind, CronRegistry, CronTarget};

const DOWN: &str = "\x1b[B";
const UP: &str = "\x1b[A";
const RIGHT: &str = "\x1b[C";
const LEFT: &str = "\x1b[D";
const ESC: &str = "\x1b";

fn see(env: &TestEnv, sess: &PtySession, text: &str) -> String {
    common::expect_screen(sess, |screen| screen.contains(text), env.timeout(), text)
}

fn seed(registry: &CronRegistry, name: &str, kind: CronKind, schedule: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let job = registry.create_full(CronCreateParams {
        schedule: schedule.into(),
        kind,
        prompt: format!("{name}: summarize what changed"),
        name: Some(name.into()),
        target: CronTarget::Agent {
            name: "elsewhere".into(),
        },
        ..Default::default()
    });
    registry
        .set_next_run(&job.cron_id, Some(now + 600))
        .unwrap();
    job.cron_id
}

#[test]
fn the_scheduled_tab_manages_jobs_from_the_task_browser() {
    let env = TestEnv::new("cron-tab");
    if env.is_live() {
        return;
    }
    let registry = CronRegistry::open(env.config_home().join("crons.json"));
    let build = seed(&registry, "build check", CronKind::Every, "600");
    let weekly = seed(&registry, "weekly report", CronKind::Cron, "0 9 * * 1");

    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[]);
    sess.resize(40, 120).unwrap();
    common::expect_input_line_cleared(&sess, env.timeout(), "repl ready");
    see(&env, &sess, "2 scheduled");

    sess.send(DOWN).unwrap();
    see(&env, &sess, "Enter to view");
    sess.send("\r").unwrap();
    see(&env, &sess, "→ scheduled");
    sess.send(RIGHT).unwrap();
    see(&env, &sess, "Scheduled · 2 jobs");
    see(&env, &sess, "every 10m");
    see(&env, &sess, "weekly report");

    sess.send("e").unwrap();
    see(&env, &sess, "Disabled build check");
    assert!(!registry.get(&build).unwrap().enabled);

    sess.send(DOWN).unwrap();
    sess.send("r").unwrap();
    see(&env, &sess, "Ran weekly report: ok");
    assert_eq!(registry.get(&weekly).unwrap().run_count, 1);

    sess.send("\r").unwrap();
    see(&env, &sess, "message to elsewhere");
    see(&env, &sess, "weekly report: summarize what changed");
    sess.send(ESC).unwrap();
    see(&env, &sess, "r run now · d delete · Esc back");

    sess.send(UP).unwrap();
    sess.send("d").unwrap();
    see(&env, &sess, "Delete build check? y to confirm");
    sess.send("y").unwrap();
    see(&env, &sess, "Deleted build check");
    see(&env, &sess, "Scheduled · 1 job");
    assert!(registry.get(&build).is_none());
    assert!(registry.get(&weekly).is_some());

    sess.send(LEFT).unwrap();
    see(&env, &sess, "No background tasks in this session");
    sess.send(ESC).unwrap();
    common::expect_input_line_cleared(&sess, Duration::from_secs(30), "back at input");
    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}
