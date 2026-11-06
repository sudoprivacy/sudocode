//! A scheduled job fires back into the running REPL of the agent it targets,
//! through that agent's inbox and message queue, like a peer message.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::TestEnv;
use runtime::cron_registry::{CronCreateParams, CronKind, CronRegistry, CronTarget};

const AGENT: &str = "cron-target";

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Name the REPL's agent, so a job can address it.
fn name_agent(env: &TestEnv) {
    let path = env.config_home().join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["agentName"] = serde_json::json!(AGENT);
    std::fs::write(&path, settings.to_string()).unwrap();
}

#[test]
fn a_scheduled_job_is_delivered_into_the_running_repl_of_its_agent() {
    let env = TestEnv::new("cron-delivery");
    if env.is_live() {
        return;
    }
    name_agent(&env);
    let registry = CronRegistry::open(env.config_home().join("crons.json"));
    let due = now_secs() + 2;
    let job = registry.create_full(CronCreateParams {
        schedule: due.to_string(),
        kind: CronKind::At,
        prompt: env.prompt(
            "Calculate 2+2 and reply with 'The answer is N'.",
            "single_turn_text",
        ),
        name: Some("nightly check".into()),
        target: CronTarget::Agent { name: AGENT.into() },
        ..Default::default()
    });
    registry.set_next_run(&job.cron_id, Some(due)).unwrap();

    let mut repl = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_CRON_TICK_MS", "200")],
    );
    common::expect_input_line_cleared(&repl, env.timeout(), "repl ready");

    // While this REPL holds the scheduler lease, another process cannot fire.
    let mut tick = env.spawn_with_env(&["cron", "tick"], &[]);
    tick.expect("another scode process is running the scheduler")
        .expect("tick refuses while the REPL schedules");
    let _ = tick.expect_eof();

    common::expect_screen(
        &repl,
        |screen| {
            screen.contains("· scheduled")
                && screen.contains("nightly check")
                && screen.contains("The answer is 4")
        },
        Duration::from_secs(30),
        "the job arrives as a scheduled message and is answered",
    );
    let request = env
        .captured_message_bodies()
        .into_iter()
        .find(|body| body.contains("Calculate 2+2"))
        .expect("the scheduled message reached the model");
    assert!(request.contains("scheduled-message"), "{request}");

    let fired = registry.get(&job.cron_id).unwrap();
    assert_eq!(fired.run_count, 1);
    assert_eq!(fired.last_status.as_deref(), Some("ok"));
    assert!(!fired.enabled, "a one-shot job disables after it fires");

    common::expect_input_line_cleared(&repl, env.timeout(), "turn finished");
    repl.send("/exit\r").unwrap();
    assert_eq!(repl.expect_eof().unwrap(), 0);
}
