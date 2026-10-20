//! Storage-bound control operations. A deleted transcript is never loaded again.
use super::{checksum, CompletionJournal, Completions};
use crate::fs_backend::FsBackend;
use crate::session_writer::SessionWriter;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
struct Binding {
    version: u32,
    session_id: String,
    cwd: std::path::PathBuf,
    storage_ref: String,
    deleted: Option<String>,
}

impl CompletionJournal {
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
            let _write = self.writer.write()?;
            if state.active_trace.is_some() {
                return Err("trace_still_active".into());
            }
            let _responses =
                SessionWriter::acquire_response_archive(self.fs.as_ref(), &self.transcript)?;
            let binding = Binding {
                version: 2,
                session_id: state.session_id.clone(),
                cwd: state.cwd.clone(),
                storage_ref: state.storage_ref.clone(),
                deleted: Some(cleanup_id.into()),
            };
            self.fs
                .write_atomic(
                    &self.path,
                    &serde_json::to_vec(&binding).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            // Retire before releasing the same mutex used by terminal callbacks.
            self.writer.retire();
            state.sources.clear();
            state.reducer = Completions::default();
            state.context = None;
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
        Ok(json!({"cleanupId": cleanup_id, "disposition": "purged", "storageRef": storage_ref}))
    }

    /// Read-only probe, including minimal tombstones. No engine, lock or mutation.
    pub fn probe(
        fs: &dyn FsBackend,
        transcript: &Path,
        session_id: &str,
        cwd: &Path,
    ) -> Result<Value, String> {
        let path = transcript.with_extension("completions.json");
        if fs
            .exists(&path.to_string_lossy())
            .map_err(|e| e.to_string())?
        {
            let value: Value = serde_json::from_slice(
                &fs.read(&path.to_string_lossy())
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            if value["session_id"] != session_id || value["cwd"] != cwd.to_string_lossy().as_ref() {
                return Err("completion journal identity mismatch".into());
            }
            if value["version"] != 2 {
                return Err("legacy_writer_unconfirmed".into());
            }
            let binding: Binding = serde_json::from_value(value).map_err(|e| e.to_string())?;
            return Ok(
                json!({"sessionId": session_id, "storageRef": binding.storage_ref, "deleted": binding.deleted.is_some()}),
            );
        }
        let session =
            crate::Session::load_from_path_with(fs, transcript).map_err(|e| e.to_string())?;
        if session.session_id != session_id {
            return Err("completion session identity mismatch".into());
        }
        Ok(
            json!({"sessionId": session_id, "storageRef": checksum(&serde_json::to_vec(&(1, session_id, transcript)).map_err(|e| e.to_string())?), "deleted": false}),
        )
    }

    pub fn purge_stored(
        fs: &dyn FsBackend,
        transcript: &Path,
        session_id: &str,
        cwd: &Path,
        storage_ref: &str,
        cleanup_id: &str,
    ) -> Result<Value, String> {
        let probe = Self::probe(fs, transcript, session_id, cwd)?;
        if probe["storageRef"] != storage_ref {
            return Err("completion_storage_mismatch".into());
        }
        if cleanup_id.is_empty() || cleanup_id.len() > 512 {
            return Err("invalid cleanup identity".into());
        }
        if probe["deleted"] != true {
            let writer = SessionWriter::acquire(fs, transcript)?;
            let _write = writer.write()?;
            let _responses = SessionWriter::acquire_response_archive(fs, transcript)?;
            // Re-read under exclusion, not from the earlier read-only probe.
            let locked = Self::probe(fs, transcript, session_id, cwd)?;
            if locked["storageRef"] != storage_ref {
                return Err("completion_storage_mismatch".into());
            }
            let binding = Binding {
                version: 2,
                session_id: session_id.into(),
                cwd: cwd.into(),
                storage_ref: storage_ref.into(),
                deleted: Some(cleanup_id.into()),
            };
            fs.write_atomic(
                &transcript
                    .with_extension("completions.json")
                    .to_string_lossy(),
                &serde_json::to_vec(&binding).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            writer.retire();
        }
        Ok(json!({"cleanupId": cleanup_id, "disposition": "purged", "storageRef": storage_ref}))
    }
}
