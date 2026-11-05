//! One API over the cron store for every surface.
//!
//! The `scode cron` CLI, `/cron`, the `CronCreate`/`CronList`/`CronDelete`
//! tools, the ACP methods and the task-browser tab all go through these
//! functions, so a rule about jobs lives in one place. Before this, the CLI and
//! the tools each validated, created and seeded the first run on their own,
//! and resolved the store path differently (only the tools honored
//! `SUDOCODE_CRON_STORE`).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::cron_registry::{CronCreateParams, CronEntry, CronKind, CronRegistry, CronTarget};
use crate::cron_schedule;

/// The store every process shares: `SUDOCODE_CRON_STORE` when set (tests point
/// it at a temp file), else `<config_home>/crons.json`.
#[must_use]
pub fn store_path() -> PathBuf {
    std::env::var_os("SUDOCODE_CRON_STORE").map_or_else(
        || crate::default_config_home().join("crons.json"),
        PathBuf::from,
    )
}

/// A handle on the shared store. Cheap: every operation reloads the file.
#[must_use]
pub fn registry() -> CronRegistry {
    CronRegistry::open(store_path())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Validate, create, and arm the first run. An invalid schedule or target is
/// refused up front, so no caller can create a job that never fires.
pub fn create(params: CronCreateParams) -> Result<CronEntry, String> {
    cron_schedule::validate(params.kind, &params.schedule, params.tz.as_deref())?;
    if let CronTarget::Agent { name } = &params.target {
        if name.trim().is_empty() {
            return Err("a scheduled message needs an agent name to deliver to".into());
        }
    }
    if params.prompt.trim().is_empty() {
        return Err("a scheduled job needs a message".into());
    }
    let registry = registry();
    let entry = registry.create_full(params);
    arm(&registry, &entry.cron_id)
}

#[must_use]
pub fn list() -> Vec<CronEntry> {
    registry().list(false)
}

#[must_use]
pub fn get(cron_id: &str) -> Option<CronEntry> {
    registry().get(cron_id)
}

pub fn remove(cron_id: &str) -> Result<CronEntry, String> {
    registry().delete(cron_id)
}

/// Enabling re-arms the next run from now; disabling keeps the job.
pub fn set_enabled(cron_id: &str, enabled: bool) -> Result<CronEntry, String> {
    let registry = registry();
    registry.set_enabled(cron_id, enabled)?;
    if enabled {
        arm(&registry, cron_id)
    } else {
        registry
            .get(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))
    }
}

/// What [`update`] may change. Absent fields keep their value. Field names
/// are the stored entry's, so a client edits the shape it lists.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
    pub prompt: Option<String>,
    pub schedule: Option<String>,
    pub kind: Option<CronKind>,
    pub tz: Option<String>,
    pub cwd: Option<String>,
    pub target: Option<CronTarget>,
    pub delivery: Option<crate::turn_input::InputDelivery>,
    pub enabled: Option<bool>,
}

/// Change a job. The result is validated as a whole before anything is
/// written, and a changed schedule re-arms the next run from now.
pub fn update(cron_id: &str, change: CronUpdate) -> Result<CronEntry, String> {
    let registry = registry();
    let current = registry
        .get(cron_id)
        .ok_or_else(|| format!("cron not found: {cron_id}"))?;
    let kind = change.kind.unwrap_or(current.kind);
    let schedule = change
        .schedule
        .clone()
        .unwrap_or_else(|| current.schedule.clone());
    let tz = change.tz.clone().or_else(|| current.tz.clone());
    let rescheduled = kind != current.kind || schedule != current.schedule || tz != current.tz;
    if rescheduled {
        cron_schedule::validate(kind, &schedule, tz.as_deref())?;
    }
    if let Some(CronTarget::Agent { name }) = &change.target {
        if name.trim().is_empty() {
            return Err("a scheduled message needs an agent name to deliver to".into());
        }
    }
    if change
        .prompt
        .as_deref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        return Err("a scheduled job needs a message".into());
    }
    registry.modify(cron_id, |entry| {
        entry.kind = kind;
        entry.schedule = schedule;
        entry.tz = tz;
        if let Some(name) = change.name {
            entry.name = Some(name);
        }
        if let Some(description) = change.description {
            entry.description = Some(description);
        }
        if let Some(prompt) = change.prompt {
            entry.prompt = prompt;
        }
        if let Some(cwd) = change.cwd {
            entry.cwd = Some(cwd);
        }
        if let Some(target) = change.target {
            entry.target = target;
        }
        if let Some(delivery) = change.delivery {
            entry.delivery = delivery;
        }
    })?;
    match change.enabled {
        Some(enabled) => set_enabled(cron_id, enabled),
        None if rescheduled => arm(&registry, cron_id),
        None => registry
            .get(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}")),
    }
}

fn arm(registry: &CronRegistry, cron_id: &str) -> Result<CronEntry, String> {
    let entry = registry
        .get(cron_id)
        .ok_or_else(|| format!("cron not found: {cron_id}"))?;
    registry.set_next_run(cron_id, cron_schedule::first_run_at(&entry, now_secs()))?;
    registry
        .get(cron_id)
        .ok_or_else(|| format!("cron not found: {cron_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron_registry::CronKind;

    fn with_store<T>(run: impl FnOnce() -> T) -> T {
        let _guard = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SUDOCODE_CRON_STORE", dir.path().join("crons.json"));
        let out = run();
        std::env::remove_var("SUDOCODE_CRON_STORE");
        out
    }

    fn every(prompt: &str, target: CronTarget) -> CronCreateParams {
        CronCreateParams {
            schedule: "60".into(),
            kind: CronKind::Every,
            prompt: prompt.into(),
            target,
            ..Default::default()
        }
    }

    #[test]
    fn create_arms_the_first_run_in_the_shared_store() {
        with_store(|| {
            let entry = create(every("check in", CronTarget::NewSession)).unwrap();
            assert!(entry.next_run_at.is_some(), "armed");
            assert_eq!(
                registry().get(&entry.cron_id).unwrap().next_run_at,
                entry.next_run_at
            );
        });
    }

    #[test]
    fn create_refuses_what_could_never_fire() {
        with_store(|| {
            let bad_schedule = CronCreateParams {
                schedule: "not a cron".into(),
                prompt: "x".into(),
                ..Default::default()
            };
            assert!(create(bad_schedule).is_err());
            let no_agent = every("x", CronTarget::Agent { name: " ".into() });
            assert!(create(no_agent).unwrap_err().contains("agent name"));
            assert!(create(every(" ", CronTarget::NewSession)).is_err());
            assert!(list().is_empty(), "nothing half-created");
        });
    }

    #[test]
    fn update_validates_the_result_and_re_arms_a_new_schedule() {
        with_store(|| {
            let entry = create(every("x", CronTarget::NewSession)).unwrap();
            let bad = CronUpdate {
                schedule: Some("not a number".into()),
                ..Default::default()
            };
            assert!(update(&entry.cron_id, bad).is_err());
            assert_eq!(
                get(&entry.cron_id).unwrap().schedule,
                "60",
                "nothing written"
            );

            registry().set_next_run(&entry.cron_id, None).unwrap();
            let changed = update(
                &entry.cron_id,
                CronUpdate {
                    schedule: Some("120".into()),
                    prompt: Some("check again".into()),
                    target: Some(CronTarget::Agent {
                        name: "worker".into(),
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(changed.schedule, "120");
            assert_eq!(changed.prompt, "check again");
            assert_eq!(
                changed.target,
                CronTarget::Agent {
                    name: "worker".into()
                }
            );
            assert!(changed.next_run_at.is_some(), "re-armed");

            let paused = update(
                &entry.cron_id,
                CronUpdate {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(!paused.enabled);
        });
    }

    #[test]
    fn re_enabling_re_arms_from_now() {
        with_store(|| {
            let entry = create(every("x", CronTarget::NewSession)).unwrap();
            registry().set_next_run(&entry.cron_id, None).unwrap();
            set_enabled(&entry.cron_id, false).unwrap();
            let enabled = set_enabled(&entry.cron_id, true).unwrap();
            assert!(enabled.enabled && enabled.next_run_at.is_some());
        });
    }
}
