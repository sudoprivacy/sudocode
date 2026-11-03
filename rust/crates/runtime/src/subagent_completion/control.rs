//! Storage-bound control operations. A deleted transcript is never loaded again.
use super::{CompletionJournal, Completions};
use crate::fs_backend::FsBackend;
use crate::session_writer::SessionWriter;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

impl CompletionJournal {
    /// Resolve durable identity from a bound journal or its original transcript.
    pub fn storage_identity(
        fs: &dyn FsBackend,
        transcript: &Path,
    ) -> Result<(String, std::path::PathBuf), String> {
        super::storage::identity(fs, transcript)
    }

    pub fn terminal_page(&self, after: Option<&str>) -> Value {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut page = state.sources.iter().filter(|(tool, s)| {
            s.terminal.is_some() && after.is_none_or(|after| tool.as_str() > after)
        });
        let sources: Vec<_> = page.by_ref().take(50).collect();
        let next = page
            .next()
            .and_then(|_| sources.last().map(|(tool, _)| (*tool).clone()));
        json!({"terminalSources":sources.into_iter().filter_map(|(_,s)| Self::terminal_view(s)).collect::<Vec<_>>(),"nextTaskId":next})
    }

    pub fn storage_ref(&self) -> String {
        self.status().storage_ref
    }

    pub fn check_binding(&self, storage_ref: &str) -> Result<(), String> {
        if self.storage_ref() != storage_ref {
            return Err("completion_storage_mismatch".into());
        }
        Ok(())
    }

    pub fn suppress(&self, storage_ref: &str, trace: &str) -> Result<Value, String> {
        self.check_binding(storage_ref)?;
        self.change(|state| {
            if state.active_trace.as_deref() == Some(trace) {
                return Err("trace_still_active".into());
            }
            if !state.closed_traces.contains(trace) {
                return Err("trace_not_closed".into());
            }
            for (tool, source) in &mut state.sources {
                if source.trace_id != trace {
                    continue;
                }
                source.suppressed = true;
                source.settled = true;
                source.blocked = None;
                source.ack = Some("suppressed".into());
                source.payload = None;
                if let Some(lifecycle) = &mut source.lifecycle {
                    lifecycle.raw_output = None;
                    lifecycle.completion_notification = None;
                }
                state.reducer.workers.remove(tool);
            }
            Ok(())
        })?;
        Ok(
            json!({"originTraceId": trace, "disposition": "suppressed", "storageRef":storage_ref, "watermark": self.status().watermark}),
        )
    }

    pub fn purge(&self, storage_ref: &str, cleanup_id: &str) -> Result<Value, String> {
        self.check_binding(storage_ref)?;
        if cleanup_id.is_empty() || cleanup_id.len() > 512 {
            return Err("invalid cleanup identity".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "completion journal poisoned")?;
        if state.deleted.is_none() {
            let shared_history = self.writer.has_history()?;
            let _write = if shared_history {
                self.writer.write()?
            } else {
                self.writer.purge_write()?
            };
            if !shared_history && state.active_trace.is_some() {
                return Err("trace_still_active".into());
            }
            let deletion = super::storage::Deletion {
                session_id: &state.session_id,
                cwd: &state.cwd,
                storage_ref: &state.storage_ref,
                cleanup_id,
                store_root: self.store_root.as_deref(),
            };
            if shared_history {
                super::storage::mark_deleted(self.fs.as_ref(), &self.transcript, &deletion)?;
            } else {
                super::storage::purge(
                    self.fs.as_ref(),
                    &self.transcript,
                    &deletion,
                    Some(&self.path),
                )?;
            }

            // Retire before releasing the same mutex used by terminal callbacks.
            self.writer.retire();
            state.sources.clear();
            state.reducer = Completions::default();
            state.context = None;
            state.active_trace = None;
            state.closed_traces.clear();
            state.deleted = Some(cleanup_id.into());
        }
        drop(state);
        if let Some(clear) = self
            .clear_cache
            .lock()
            .map_err(|_| "completion cache poisoned")?
            .as_ref()
        {
            clear();
        }
        crate::session_cleanup::resume(Arc::clone(&self.fs), &self.transcript)?;
        Ok(json!({"cleanupId": cleanup_id, "disposition": "purged", "storageRef": storage_ref}))
    }

    /// Read-only probe, including minimal tombstones. No engine, lock or mutation.
    pub fn probe(
        fs: &dyn FsBackend,
        transcript: &Path,
        session_id: &str,
        cwd: &Path,
    ) -> Result<Value, String> {
        super::storage::probe(fs, transcript, session_id, cwd)
    }

    pub fn purge_stored(
        fs: Arc<dyn FsBackend>,
        transcript: &Path,
        session_id: &str,
        cwd: &Path,
        storage_ref: &str,
        cleanup_id: &str,
        store_root: Option<&Path>,
    ) -> Result<Value, String> {
        let probe = Self::probe(fs.as_ref(), transcript, session_id, cwd)?;
        if probe["storageRef"] != storage_ref {
            return Err("completion_storage_mismatch".into());
        }
        if cleanup_id.is_empty() || cleanup_id.len() > 512 {
            return Err("invalid cleanup identity".into());
        }
        {
            if !fs.fenced_locks() {
                return Err("completion_storage_exclusion_unavailable".into());
            }
            let shared_history = fs
                .exists(&transcript.to_string_lossy())
                .map_err(|e| e.to_string())?
                && crate::session::history::is_record_history(
                    &fs.read_snapshot(&transcript.to_string_lossy())
                        .map_err(|e| e.to_string())?,
                );
            let deletion = super::storage::Deletion {
                session_id,
                cwd,
                storage_ref,
                cleanup_id,
                store_root,
            };
            if shared_history {
                let _write = SessionWriter::commit_lock(fs.as_ref(), transcript)?;
                super::storage::mark_deleted(fs.as_ref(), transcript, &deletion)?;
            } else {
                let _write = SessionWriter::purge_lock(fs.as_ref(), transcript)?;
                super::storage::purge(fs.as_ref(), transcript, &deletion, None)?;
            }
        }
        crate::session_cleanup::resume(fs, transcript)?;
        Ok(json!({"cleanupId": cleanup_id, "disposition": "purged", "storageRef": storage_ref}))
    }
}
