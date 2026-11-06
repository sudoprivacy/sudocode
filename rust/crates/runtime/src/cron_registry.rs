#![allow(clippy::must_use_candidate)]
//! Persistent registry for Cron lifecycle management.
//!
//! Backs the `/cron` slash command, the CronCreate/List/Delete/Enable/
//! Disable tools, and the scheduler. Entries persist to a JSON store
//! (`<config_home>/crons.json`) so scheduled jobs survive process
//! restarts. A registry created via [`CronRegistry::open`] write-throughs
//! every mutation; [`CronRegistry::new`] stays purely in-memory (tests).
//!
//! The file is the single source of truth. Several processes open it at
//! once (the REPL, the ACP server a desktop host spawns, `scode cron`), so
//! every operation reloads it first and every mutation is a
//! read-modify-write under an exclusive lock on `<store>.lock`. An
//! in-memory copy that is only written back would let one process silently
//! undo another's change.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_max_retries() -> u32 {
    3
}

/// How an entry's `schedule` string is interpreted by the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CronKind {
    /// 5-field cron expression (e.g. `"0 * * * *"`), honouring `tz`.
    #[default]
    Cron,
    /// Fixed interval; `schedule` is the interval in seconds (as text).
    Every,
    /// One-shot at an absolute unix timestamp (`schedule` = the ts as
    /// text). Self-disables after it fires.
    At,
}

/// Where a fire delivers its message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronTarget {
    /// A fresh session in the entry's `cwd`, one per fire.
    #[default]
    NewSession,
    /// The named agent's inbox. A running agent receives it like a peer
    /// message, steered or queued per [`CronEntry::delivery`]; an agent that
    /// is not running finds it in its durable inbox when it starts.
    Agent { name: String },
}

/// Who created a job, shown to the user and kept through migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CronCreator {
    #[default]
    User,
    Agent,
}

/// A scheduled fire queues behind a running turn unless the job asks to
/// steer into it.
fn default_delivery() -> crate::turn_input::InputDelivery {
    crate::turn_input::InputDelivery::Queue
}

/// A scheduled job. New fields carry `#[serde(default)]` so an older
/// `crons.json` (or the tool-create path) round-trips without them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronEntry {
    pub cron_id: String,
    /// Raw schedule value; interpreted per [`CronEntry::kind`].
    pub schedule: String,
    pub prompt: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_run_at: Option<u64>,
    pub run_count: u64,

    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: CronKind,
    /// IANA timezone for `Cron` kind (e.g. `"Asia/Shanghai"`); `None` = local.
    #[serde(default)]
    pub tz: Option<String>,
    /// Working directory the fired agent turn runs in; `None` = current dir.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Next fire time (unix secs), computed by the scheduler.
    #[serde(default)]
    pub next_run_at: Option<u64>,
    /// Outcome of the most recent fire: `ok` | `error` | `skipped` | `missed`.
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Consecutive busy-retry count for the current pending fire.
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub target: CronTarget,
    #[serde(default = "default_delivery")]
    pub delivery: crate::turn_input::InputDelivery,
    #[serde(default)]
    pub created_by: CronCreator,
}

/// Parameters for a full-fidelity create (CLI / scheduler path). The
/// tool path uses the simpler [`CronRegistry::create`].
#[derive(Debug, Clone, Default)]
pub struct CronCreateParams {
    pub schedule: String,
    pub kind: CronKind,
    pub prompt: String,
    pub description: Option<String>,
    pub name: Option<String>,
    pub tz: Option<String>,
    pub cwd: Option<String>,
    pub target: CronTarget,
    /// `None` = the default ([`InputDelivery::Queue`](crate::turn_input::InputDelivery)).
    pub delivery: Option<crate::turn_input::InputDelivery>,
    pub created_by: CronCreator,
}

#[derive(Debug, Clone, Default)]
pub struct CronRegistry {
    inner: Arc<Mutex<CronInner>>,
}

#[derive(Debug, Default)]
struct CronInner {
    entries: HashMap<String, CronEntry>,
    counter: u64,
    /// When set, every mutation is written through to this path.
    store_path: Option<PathBuf>,
}

impl CronInner {
    /// Replace the in-memory entries with the store's current contents. A
    /// missing file is an empty store; an unreadable one keeps what is held
    /// (logged by the caller) rather than dropping every job.
    fn reload(&mut self) {
        let Some(path) = &self.store_path else {
            return;
        };
        let doc = match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<CronStoreDoc>(&raw) {
                Ok(doc) => doc,
                Err(_) => return,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CronStoreDoc::default(),
            Err(_) => return,
        };
        self.entries.clear();
        self.counter = doc.counter;
        for entry in doc.crons {
            self.counter = self.counter.max(entry_counter_hint(&entry));
            self.entries.insert(entry.cron_id.clone(), entry);
        }
    }

    /// Exclusive cross-process lock for a read-modify-write of the store.
    /// Released when the returned file is dropped (or the process dies).
    fn lock_store(&self) -> Option<std::fs::File> {
        let path = self.store_path.as_ref()?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_extension("json.lock"))
            .ok()?;
        file.lock().ok()?;
        Some(file)
    }

    /// Write the current entries to the store path, if configured.
    /// Best-effort: a failed write is surfaced to the caller so the
    /// tool/CLI can report it, but never poisons the in-memory state.
    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.store_path else {
            return Ok(());
        };
        let mut entries: Vec<&CronEntry> = self.entries.values().collect();
        entries.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.cron_id.cmp(&b.cron_id))
        });
        let doc = CronStoreDoc {
            version: 1,
            counter: self.counter,
            crons: entries.into_iter().cloned().collect(),
        };
        let json =
            serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize crons.json: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create crons.json dir: {e}"))?;
        }
        // Atomic-ish: write to a temp sibling then rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write crons.json: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename crons.json: {e}"))?;
        Ok(())
    }
}

/// On-disk shape of `crons.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CronStoreDoc {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    crons: Vec<CronEntry>,
}

impl CronRegistry {
    /// In-memory registry (no persistence). Used by tests and any
    /// caller that does not want a durable store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a persistent registry backed by `path`. Loads existing
    /// entries if the file is present; an unreadable/corrupt file is
    /// treated as empty (logged by the caller) rather than fatal, so a
    /// bad store never bricks startup.
    #[must_use]
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let mut inner = CronInner {
            store_path: Some(path.into()),
            ..CronInner::default()
        };
        inner.reload();
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Simple create (tool path): a 5-field cron expression schedule.
    pub fn create(&self, schedule: &str, prompt: &str, description: Option<&str>) -> CronEntry {
        self.create_full(CronCreateParams {
            schedule: schedule.to_owned(),
            kind: CronKind::Cron,
            prompt: prompt.to_owned(),
            description: description.map(str::to_owned),
            ..Default::default()
        })
    }

    /// Full-fidelity create (CLI / scheduler path).
    pub fn create_full(&self, params: CronCreateParams) -> CronEntry {
        self.mutate(|inner| {
            inner.counter += 1;
            let ts = now_secs();
            let cron_id = format!("cron_{:08x}_{}", ts, inner.counter);
            let entry = CronEntry {
                cron_id: cron_id.clone(),
                schedule: params.schedule,
                prompt: params.prompt,
                description: params.description,
                enabled: true,
                created_at: ts,
                updated_at: ts,
                last_run_at: None,
                run_count: 0,
                name: params.name,
                kind: params.kind,
                tz: params.tz,
                cwd: params.cwd,
                next_run_at: None,
                last_status: None,
                last_error: None,
                retry_count: 0,
                max_retries: default_max_retries(),
                target: params.target,
                delivery: params.delivery.unwrap_or_else(default_delivery),
                created_by: params.created_by,
            };
            inner.entries.insert(cron_id, entry.clone());
            Ok(entry)
        })
        .expect("create cannot fail")
    }

    pub fn get(&self, cron_id: &str) -> Option<CronEntry> {
        self.read(|inner| inner.entries.get(cron_id).cloned())
    }

    pub fn list(&self, enabled_only: bool) -> Vec<CronEntry> {
        self.read(|inner| {
            let mut out: Vec<CronEntry> = inner
                .entries
                .values()
                .filter(|e| !enabled_only || e.enabled)
                .cloned()
                .collect();
            out.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then(a.cron_id.cmp(&b.cron_id))
            });
            out
        })
    }

    pub fn delete(&self, cron_id: &str) -> Result<CronEntry, String> {
        self.mutate(|inner| {
            inner
                .entries
                .remove(cron_id)
                .ok_or_else(|| format!("cron not found: {cron_id}"))
        })
    }

    /// Enable a cron entry. Clears the retry counter so a re-enabled job
    /// starts its backoff fresh.
    pub fn enable(&self, cron_id: &str) -> Result<(), String> {
        self.set_enabled(cron_id, true)
    }

    /// Disable a cron entry without removing it.
    pub fn disable(&self, cron_id: &str) -> Result<(), String> {
        self.set_enabled(cron_id, false)
    }

    pub fn set_enabled(&self, cron_id: &str, enabled: bool) -> Result<(), String> {
        self.modify(cron_id, |entry| {
            entry.enabled = enabled;
            if enabled {
                entry.retry_count = 0;
            }
        })
    }

    /// Update the computed next-run time (scheduler bookkeeping).
    pub fn set_next_run(&self, cron_id: &str, next_run_at: Option<u64>) -> Result<(), String> {
        self.modify(cron_id, |entry| entry.next_run_at = next_run_at)
    }

    /// Record a completed fire with its outcome and the recomputed next
    /// run. `status` is `ok` | `error` | `skipped` | `missed`. Only `ok`
    /// and `error` bump `run_count` / `last_run_at` (an actual fire);
    /// `skipped`/`missed` just annotate state.
    pub fn record_result(
        &self,
        cron_id: &str,
        status: &str,
        error: Option<&str>,
        next_run_at: Option<u64>,
    ) -> Result<(), String> {
        self.modify(cron_id, |entry| {
            if status == "ok" || status == "error" {
                entry.last_run_at = Some(entry.updated_at);
                entry.run_count += 1;
                entry.retry_count = 0;
            }
            entry.last_status = Some(status.to_owned());
            entry.last_error = error.map(str::to_owned);
            entry.next_run_at = next_run_at;
        })
    }

    /// Record a cron run (legacy helper): marks an `ok` fire, bumping
    /// `run_count` / `last_run_at`. Retained for the existing tool path
    /// and tests; new code should prefer [`record_result`].
    pub fn record_run(&self, cron_id: &str) -> Result<(), String> {
        self.record_result(cron_id, "ok", None, None)
    }

    /// Increment the busy-retry counter; returns the new count so the
    /// scheduler can compare against `max_retries`.
    pub fn bump_retry(&self, cron_id: &str) -> Result<u32, String> {
        self.mutate(|inner| {
            let entry = inner
                .entries
                .get_mut(cron_id)
                .ok_or_else(|| format!("cron not found: {cron_id}"))?;
            entry.retry_count += 1;
            entry.updated_at = now_secs();
            Ok(entry.retry_count)
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.read(|inner| inner.entries.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Change one entry under the store lock, stamping `updated_at` first so
    /// the change can use it as "now".
    pub fn modify(&self, cron_id: &str, change: impl FnOnce(&mut CronEntry)) -> Result<(), String> {
        self.mutate(|inner| {
            let entry = inner
                .entries
                .get_mut(cron_id)
                .ok_or_else(|| format!("cron not found: {cron_id}"))?;
            entry.updated_at = now_secs();
            change(entry);
            Ok(())
        })
    }

    fn read<T>(&self, read: impl FnOnce(&CronInner) -> T) -> T {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.reload();
        read(&inner)
    }

    /// Read-modify-write under the store lock. A failed write is not
    /// returned: callers report the change, and the next reload shows what
    /// actually persisted.
    fn mutate<T>(
        &self,
        change: impl FnOnce(&mut CronInner) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let _store = inner.lock_store();
        inner.reload();
        let out = change(&mut inner)?;
        let _ = inner.persist();
        Ok(out)
    }
}

/// Best-effort counter hint from a loaded entry's id (`cron_<ts>_<n>`),
/// so a reloaded registry keeps minting monotonically-increasing ids.
fn entry_counter_hint(entry: &CronEntry) -> u64 {
    entry
        .cron_id
        .rsplit('_')
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("crons.json")
    }

    #[test]
    fn v1_store_loads_with_v2_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            store(&dir),
            r#"{"version":1,"counter":1,"crons":[{"cron_id":"cron_00000001_1","schedule":"0 * * * *","prompt":"hourly","description":null,"enabled":true,"created_at":1,"updated_at":1,"last_run_at":null,"run_count":0}]}"#,
        )
        .unwrap();
        let entry = CronRegistry::open(store(&dir))
            .get("cron_00000001_1")
            .unwrap();
        assert_eq!(entry.target, CronTarget::NewSession);
        assert_eq!(entry.delivery, crate::turn_input::InputDelivery::Queue);
        assert_eq!(entry.created_by, CronCreator::User);
    }

    #[test]
    fn create_full_carries_target_delivery_and_creator() {
        let entry = CronRegistry::new().create_full(CronCreateParams {
            schedule: "60".into(),
            kind: CronKind::Every,
            prompt: "check in".into(),
            target: CronTarget::Agent {
                name: "worker".into(),
            },
            delivery: Some(crate::turn_input::InputDelivery::Steer),
            created_by: CronCreator::Agent,
            ..Default::default()
        });
        assert_eq!(
            entry.target,
            CronTarget::Agent {
                name: "worker".into()
            }
        );
        assert_eq!(entry.delivery, crate::turn_input::InputDelivery::Steer);
        assert_eq!(entry.created_by, CronCreator::Agent);
    }

    /// Two processes hold the same store. Neither may undo the other's
    /// change by writing back a stale copy.
    #[test]
    fn registries_sharing_a_store_keep_each_others_changes() {
        let dir = tempfile::tempdir().unwrap();
        let first = CronRegistry::open(store(&dir));
        let second = CronRegistry::open(store(&dir));
        let a = first.create("0 * * * *", "from first", None);
        let b = second.create("0 * * * *", "from second", None);
        first.disable(&b.cron_id).unwrap();
        second.record_run(&a.cron_id).unwrap();

        let reopened = CronRegistry::open(store(&dir));
        assert_eq!(reopened.len(), 2, "both creates survive");
        assert!(
            !reopened.get(&b.cron_id).unwrap().enabled,
            "first's change to second's job"
        );
        assert_eq!(
            reopened.get(&a.cron_id).unwrap().run_count,
            1,
            "second's change to first's job"
        );
        assert_ne!(a.cron_id, b.cron_id, "ids stay unique across processes");
        assert_eq!(
            first.list(false).len(),
            2,
            "a live registry sees the other's job"
        );
    }

    #[test]
    fn creates_and_retrieves_cron() {
        let registry = CronRegistry::new();
        let entry = registry.create("0 * * * *", "Check status", Some("hourly check"));
        assert_eq!(entry.schedule, "0 * * * *");
        assert_eq!(entry.prompt, "Check status");
        assert!(entry.enabled);
        assert_eq!(entry.kind, CronKind::Cron);
        assert_eq!(entry.run_count, 0);
        assert!(entry.last_run_at.is_none());

        let fetched = registry.get(&entry.cron_id).expect("cron should exist");
        assert_eq!(fetched.cron_id, entry.cron_id);
    }

    #[test]
    fn lists_with_enabled_filter() {
        let registry = CronRegistry::new();
        let c1 = registry.create("* * * * *", "Task 1", None);
        let c2 = registry.create("0 * * * *", "Task 2", None);
        registry
            .disable(&c1.cron_id)
            .expect("disable should succeed");

        let all = registry.list(false);
        assert_eq!(all.len(), 2);

        let enabled_only = registry.list(true);
        assert_eq!(enabled_only.len(), 1);
        assert_eq!(enabled_only[0].cron_id, c2.cron_id);
    }

    #[test]
    fn enable_reenables_and_clears_retry() {
        let registry = CronRegistry::new();
        let c = registry.create("* * * * *", "Task", None);
        registry.bump_retry(&c.cron_id).unwrap();
        registry.disable(&c.cron_id).unwrap();
        registry.enable(&c.cron_id).unwrap();
        let fetched = registry.get(&c.cron_id).unwrap();
        assert!(fetched.enabled);
        assert_eq!(fetched.retry_count, 0);
    }

    #[test]
    fn deletes_cron_entry() {
        let registry = CronRegistry::new();
        let entry = registry.create("* * * * *", "To delete", None);
        let deleted = registry
            .delete(&entry.cron_id)
            .expect("delete should succeed");
        assert_eq!(deleted.cron_id, entry.cron_id);
        assert!(registry.get(&entry.cron_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn records_cron_runs() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);
        registry.record_run(&entry.cron_id).unwrap();
        registry.record_run(&entry.cron_id).unwrap();

        let fetched = registry.get(&entry.cron_id).unwrap();
        assert_eq!(fetched.run_count, 2);
        assert!(fetched.last_run_at.is_some());
        assert_eq!(fetched.last_status.as_deref(), Some("ok"));
    }

    #[test]
    fn record_result_error_sets_status_without_double_counting() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);
        registry
            .record_result(&entry.cron_id, "error", Some("boom"), Some(now_secs() + 60))
            .unwrap();
        let fetched = registry.get(&entry.cron_id).unwrap();
        assert_eq!(fetched.run_count, 1);
        assert_eq!(fetched.last_status.as_deref(), Some("error"));
        assert_eq!(fetched.last_error.as_deref(), Some("boom"));
        assert!(fetched.next_run_at.is_some());
    }

    #[test]
    fn skipped_status_does_not_count_as_run() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/5 * * * *", "Recurring", None);
        registry
            .record_result(&entry.cron_id, "skipped", None, None)
            .unwrap();
        let fetched = registry.get(&entry.cron_id).unwrap();
        assert_eq!(fetched.run_count, 0);
        assert_eq!(fetched.last_status.as_deref(), Some("skipped"));
    }

    #[test]
    fn rejects_missing_cron_operations() {
        let registry = CronRegistry::new();
        assert!(registry.delete("nonexistent").is_err());
        assert!(registry.disable("nonexistent").is_err());
        assert!(registry.enable("nonexistent").is_err());
        assert!(registry.record_run("nonexistent").is_err());
        assert!(registry.bump_retry("nonexistent").is_err());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn cron_create_without_description() {
        let registry = CronRegistry::new();
        let entry = registry.create("*/15 * * * *", "Check health", None);
        assert!(entry.cron_id.starts_with("cron_"));
        assert_eq!(entry.description, None);
        assert!(entry.enabled);
    }

    #[test]
    fn new_cron_registry_is_empty() {
        let registry = CronRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.list(true).is_empty());
        assert!(registry.list(false).is_empty());
    }

    #[test]
    fn create_full_carries_kind_tz_cwd() {
        let registry = CronRegistry::new();
        let entry = registry.create_full(CronCreateParams {
            schedule: "3600".to_owned(),
            kind: CronKind::Every,
            prompt: "hourly".to_owned(),
            tz: Some("Asia/Shanghai".to_owned()),
            cwd: Some("/tmp/work".to_owned()),
            name: Some("hourly-job".to_owned()),
            ..Default::default()
        });
        assert_eq!(entry.kind, CronKind::Every);
        assert_eq!(entry.tz.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(entry.cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(entry.name.as_deref(), Some("hourly-job"));
    }

    #[test]
    fn persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!("cron-store-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("crons.json");
        let _ = std::fs::remove_file(&path);

        let created_id;
        {
            let reg = CronRegistry::open(&path);
            let e = reg.create("0 9 * * *", "Daily standup", Some("9am"));
            created_id = e.cron_id.clone();
            reg.record_run(&created_id).unwrap();
        }
        // Fresh registry from the same path must see the persisted entry.
        let reg2 = CronRegistry::open(&path);
        assert_eq!(reg2.len(), 1);
        let loaded = reg2.get(&created_id).expect("entry should persist");
        assert_eq!(loaded.prompt, "Daily standup");
        assert_eq!(loaded.run_count, 1);
        // A new create keeps minting unique ids after reload.
        let e2 = reg2.create("0 10 * * *", "Second", None);
        assert_ne!(e2.cron_id, created_id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_store_loads_as_empty() {
        let dir = std::env::temp_dir().join(format!("cron-bad-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("crons.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let reg = CronRegistry::open(&path);
        assert!(reg.is_empty());
        // still usable — create works and overwrites the bad file.
        reg.create("* * * * *", "ok now", None);
        assert_eq!(reg.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
