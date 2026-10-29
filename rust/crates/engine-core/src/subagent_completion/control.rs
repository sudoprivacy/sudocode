//! Storage controls, delivery admission and retry policy owned by the engine.
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_events::{CompletionAction, CompletionReceipt, EngineEvent};
use runtime::fs_backend::{FsBackend, StdFsBackend};
use runtime::subagent_completion::{CompletionJournal, TaskCompletion};
use serde_json::Value;

use super::{CompletionState, EventSink};

pub(super) struct StorageBinding {
    fs: Arc<dyn FsBackend>,
    path: PathBuf,
    session_id: String,
    cwd: PathBuf,
    store_root: Option<PathBuf>,
}

#[derive(Default)]
pub(super) struct DeliveryState {
    connected: bool,
    tasks: bool,
    sequence: u64,
    flight: Option<(u64, String, Instant)>,
    retry_at: Option<Instant>,
    failures: u32,
}

impl CompletionState {
    #[inline]
    pub(super) fn persistence_diagnostics(&self) -> &telemetry::RetryDiagnostics {
        self.persistence_diagnostics.get_or_init(|| {
            Box::new(telemetry::RetryDiagnostics::new(
                "engine-completion",
                self.binding
                    .as_ref()
                    .map_or("unbound session", |binding| binding.session_id.as_str()),
                "Task completion persistence is pending; retrying in the background.",
                "Task completion persistence checks recovered.",
            ))
        })
    }

    /// Composition only: open the journal shared with this session's writer.
    pub fn open(session: &runtime::Session, cwd: &Path, create: bool) -> Result<Arc<Self>, String> {
        let state = Arc::new(Self {
            binding: Some(StorageBinding {
                fs: session.fs_handle(),
                path: session
                    .persistence_path()
                    .ok_or("session has no transcript")?
                    .into(),
                session_id: session.session_id.clone(),
                cwd: cwd.into(),
                store_root: session.session_store_root().map(Path::to_path_buf),
            }),
            ..Self::default()
        });
        state.install_journal(CompletionJournal::open(session, cwd, create)?);
        Ok(state)
    }

    /// A control-only session never builds a model or loads a deleted transcript.
    pub fn stored(session_id: &str, cwd: &Path) -> Result<Arc<Self>, String> {
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || !cwd.is_absolute()
        {
            return Err("task control requires exact sessionId and absolute cwd".into());
        }
        let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
        if !cwd.is_dir() {
            return Err("params.cwd must be a directory".into());
        }
        let store =
            runtime::session_control::SessionStore::from_cwd(&cwd).map_err(|e| e.to_string())?;
        let path = store
            .resolve_completion_path(session_id)
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            binding: Some(StorageBinding {
                fs: Arc::new(StdFsBackend),
                path,
                session_id: session_id.into(),
                cwd,
                store_root: Some(store.sessions_dir().into()),
            }),
            ..Self::default()
        }))
    }

    /// Called by engine composition before attaching the model delegate.
    pub fn attach_session(&self, session: &runtime::Session, cwd: &Path) -> Result<(), String> {
        if let Some(journal) = self.journal() {
            journal.attach_session(session, cwd)?;
        }
        Ok(())
    }

    pub(crate) fn mark_live(&self) {
        self.live.store(true, Ordering::Release);
    }

    pub(crate) fn release(&self) {
        self.live.store(false, Ordering::Release);
        self.journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    fn install_journal(&self, journal: Arc<CompletionJournal>) {
        let wake = Arc::clone(&self.wake);
        journal.set_wake(move || wake.notify_one());
        journal.set_task_protocol(
            self.delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tasks,
        );
        *self
            .journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(journal);
    }

    fn probe(&self) -> Result<Value, String> {
        let b = self
            .binding
            .as_ref()
            .ok_or("session has no durable completion store")?;
        CompletionJournal::probe(b.fs.as_ref(), &b.path, &b.session_id, &b.cwd)
    }

    fn load_journal(&self) -> Result<Arc<CompletionJournal>, String> {
        if let Some(journal) = self.journal() {
            return Ok(journal);
        }
        let b = self
            .binding
            .as_ref()
            .ok_or("session has no durable completion store")?;
        let mut session = runtime::Session::load_from_path_with(b.fs.as_ref(), &b.path)
            .map_err(|e| e.to_string())?
            .with_fs_backend(Arc::clone(&b.fs));
        if let Some(root) = &b.store_root {
            session = session.with_session_store_root(root);
        }
        if session.session_id != b.session_id {
            return Err("completion session identity mismatch".into());
        }
        self.install_journal(CompletionJournal::open(&session, &b.cwd, false)?);
        self.journal()
            .ok_or_else(|| "completion journal not loaded".into())
    }

    pub(crate) fn command(&self, action: CompletionAction) -> Result<Value, String> {
        let is_cleanup = matches!(
            &action,
            CompletionAction::Suppress { .. } | CompletionAction::Purge { .. }
        );
        let result = match action {
            CompletionAction::Configure { tasks, connected } => {
                let mut delivery = self
                    .delivery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                delivery.tasks = tasks;
                delivery.connected = connected;
                delivery.flight = None;
                delivery.retry_at = None;
                delivery.failures = 0;
                if let Some(journal) = self.journal() {
                    journal.set_task_protocol(tasks);
                }
                Ok(Value::Null)
            }
            CompletionAction::CheckBinding { session_id, cwd } => {
                let b = self.binding.as_ref().ok_or("missing completion binding")?;
                if session_id != b.session_id
                    || cwd.canonicalize().map_err(|e| e.to_string())? != b.cwd
                {
                    return Err("completion journal identity mismatch".into());
                }
                Ok(Value::Null)
            }
            CompletionAction::Status => self.journal().map_or_else(
                || self.probe(),
                |journal| {
                    journal.context()?;
                    serde_json::to_value(journal.status()).map_err(|e| e.to_string())
                },
            ),
            CompletionAction::Replay {
                storage_ref,
                after,
                legacy,
            } => self.replay(storage_ref.as_deref(), after.as_deref(), legacy),
            CompletionAction::Suppress { storage_ref, trace } => {
                self.load_journal()?.suppress(&storage_ref, &trace)
            }
            CompletionAction::Purge {
                storage_ref,
                cleanup_id,
            } => {
                if let Some(journal) = self.journal() {
                    journal.purge(&storage_ref, &cleanup_id)
                } else {
                    let b = self.binding.as_ref().ok_or("missing completion binding")?;
                    CompletionJournal::purge_stored(
                        Arc::clone(&b.fs),
                        &b.path,
                        &b.session_id,
                        &b.cwd,
                        &storage_ref,
                        &cleanup_id,
                        b.store_root.as_deref(),
                    )
                }
            }
            CompletionAction::Receipt { attempt, receipt } => {
                self.receipt(attempt, receipt).map(|()| Value::Null)
            }
        };
        if result.is_ok() && is_cleanup {
            let mut delivery = self
                .delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            delivery.flight = None;
            delivery.retry_at = None;
        }
        self.wake.notify_one();
        result
    }

    fn replay(
        &self,
        storage_ref: Option<&str>,
        after: Option<&str>,
        legacy: bool,
    ) -> Result<Value, String> {
        let probe = self.probe()?;
        if !legacy {
            let Some(storage_ref) = storage_ref else {
                return Ok(probe);
            };
            if probe["storageRef"] != storage_ref {
                return Err("completion_storage_mismatch".into());
            }
        }
        if probe["deleted"] == true {
            return Err("session_deleted".into());
        }
        let journal = self.load_journal()?;
        if !self.live.load(Ordering::Acquire) {
            let b = self.binding.as_ref().ok_or("missing completion binding")?;
            journal.reconcile(|path, source| {
                tools::recover_subagent_completion(b.fs.as_ref(), path, source)
            })?;
        }
        let mut value = serde_json::to_value(journal.status()).map_err(|e| e.to_string())?;
        if !legacy {
            let page = journal.terminal_page(after);
            value["terminalSources"] = page["terminalSources"].clone();
            value["nextTaskId"] = page["nextTaskId"].clone();
        }
        Ok(value)
    }

    fn receipt(&self, attempt: u64, receipt: CompletionReceipt) -> Result<(), String> {
        let mut delivery = self
            .delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((current, id, _)) = delivery.flight.as_ref() else {
            return Ok(());
        };
        if *current != attempt {
            return Ok(());
        }
        let id = id.clone();
        let journal = self.journal().ok_or("missing completion journal")?;
        let settled = match receipt {
            CompletionReceipt::Ack {
                completion_id,
                disposition,
                reason,
            } => {
                if completion_id.as_deref() == Some(&id) {
                    match disposition.as_deref() {
                        Some("stored") => journal.acknowledge(&id, "stored"),
                        Some("discarded") if reason.as_deref() == Some("session_deleted") => {
                            journal.acknowledge(&id, "session_deleted")
                        }
                        _ => journal.quarantine(&id, "invalid_ack"),
                    }
                } else {
                    journal.quarantine(&id, "ack_identity_mismatch")
                }
            }
            CompletionReceipt::ProtocolRejected => journal.quarantine(&id, "protocol_rejected"),
            CompletionReceipt::Unavailable => {
                delivery.connected = false;
                Ok(())
            }
            CompletionReceipt::Retry => Err("completion transport retry".into()),
        };
        delivery.flight = None;
        if settled.is_ok() {
            delivery.failures = 0;
            delivery.retry_at = None;
        } else {
            delivery.defer(&id);
        }
        // Persistence errors leave the frozen payload pending for the next attempt.
        Ok(())
    }

    pub(crate) async fn delivery_changed(&self) {
        let notified = self.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let deadline = {
            let delivery = self
                .delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            delivery
                .flight
                .as_ref()
                .map(|(_, _, deadline)| *deadline)
                .or(delivery.retry_at)
        };
        match deadline {
            Some(deadline) => {
                tokio::select! { () = &mut notified => {}, () = tokio::time::sleep_until(deadline.into()) => {} }
            }
            None => notified.await,
        }
    }

    pub(crate) fn deliver(&self, sink: &EventSink) {
        if !self
            .delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connected
        {
            if self.binding.is_some() {
                let _ = sink.send(EngineEvent::CompletionIdle);
            }
            return;
        }
        if self.retry_terminals().is_err() {
            self.delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .defer("persistence");
            return;
        }
        let mut delivery = self
            .delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if let Some((_, id, deadline)) = &delivery.flight {
            if *deadline > now {
                return;
            }
            let id = id.clone();
            delivery.flight = None;
            delivery.defer(&id);
        }
        if delivery.retry_at.is_some_and(|deadline| deadline > now) {
            return;
        }
        delivery.retry_at = None;
        let Some(journal) = self.journal() else {
            let _ = sink.send(EngineEvent::CompletionIdle);
            return;
        };
        let payload = journal
            .pending_tasks()
            .into_iter()
            .find(|p| delivery.tasks || matches!(p, TaskCompletion::Subagent(_)));
        let Some(payload) = payload else {
            let _ = sink.send(EngineEvent::CompletionIdle);
            return;
        };
        delivery.sequence = delivery
            .sequence
            .checked_add(1)
            .expect("completion attempt exhausted");
        let attempt = delivery.sequence;
        delivery.flight = Some((attempt, payload.id().into(), now + Duration::from_secs(30)));
        let _ = sink.send(EngineEvent::CompletionRequest {
            attempt,
            tasks: delivery.tasks,
            payload,
        });
    }
}

impl DeliveryState {
    fn defer(&mut self, id: &str) {
        let seconds = (1_u64 << self.failures.min(5)).min(30);
        self.failures = self.failures.saturating_add(1);
        let jitter = id.as_bytes().first().copied().map_or(0, u64::from) * 3;
        self.retry_at = Some(Instant::now() + Duration::from_millis(seconds * 1000 + jitter));
    }
}
