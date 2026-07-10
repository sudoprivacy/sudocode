#![allow(clippy::must_use_candidate)]
//! Persistent registry for Cron lifecycle management.
//!
//! Backs the `/cron` slash command, the CronCreate/List/Delete/Enable/
//! Disable tools, and the scheduler. Entries persist to a JSON store
//! (`<config_home>/crons.json`) so scheduled jobs survive process
//! restarts. A registry created via [`CronRegistry::open`] write-throughs
//! every mutation; [`CronRegistry::new`] stays purely in-memory (tests).

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
#[derive(Debug, Serialize, Deserialize)]
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
        let path = path.into();
        let mut inner = CronInner::default();
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(doc) = serde_json::from_str::<CronStoreDoc>(&raw) {
                inner.counter = doc.counter;
                for entry in doc.crons {
                    inner.counter = inner.counter.max(entry_counter_hint(&entry));
                    inner.entries.insert(entry.cron_id.clone(), entry);
                }
            }
        }
        inner.store_path = Some(path);
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
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
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
        };
        inner.entries.insert(cron_id, entry.clone());
        let _ = inner.persist();
        entry
    }

    pub fn get(&self, cron_id: &str) -> Option<CronEntry> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.entries.get(cron_id).cloned()
    }

    pub fn list(&self, enabled_only: bool) -> Vec<CronEntry> {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
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
    }

    pub fn delete(&self, cron_id: &str) -> Result<CronEntry, String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let removed = inner
            .entries
            .remove(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        let _ = inner.persist();
        Ok(removed)
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
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        entry.enabled = enabled;
        if enabled {
            entry.retry_count = 0;
        }
        entry.updated_at = now_secs();
        let _ = inner.persist();
        Ok(())
    }

    /// Update the computed next-run time (scheduler bookkeeping).
    pub fn set_next_run(&self, cron_id: &str, next_run_at: Option<u64>) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        entry.next_run_at = next_run_at;
        let _ = inner.persist();
        Ok(())
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
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        let now = now_secs();
        if status == "ok" || status == "error" {
            entry.last_run_at = Some(now);
            entry.run_count += 1;
            entry.retry_count = 0;
        }
        entry.last_status = Some(status.to_owned());
        entry.last_error = error.map(str::to_owned);
        entry.next_run_at = next_run_at;
        entry.updated_at = now;
        let _ = inner.persist();
        Ok(())
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
        let mut inner = self.inner.lock().expect("cron registry lock poisoned");
        let entry = inner
            .entries
            .get_mut(cron_id)
            .ok_or_else(|| format!("cron not found: {cron_id}"))?;
        entry.retry_count += 1;
        entry.updated_at = now_secs();
        let count = entry.retry_count;
        let _ = inner.persist();
        Ok(count)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("cron registry lock poisoned");
        inner.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
