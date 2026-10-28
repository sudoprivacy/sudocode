//! Opt-in diagnostic correlation, scoped to one client/session. It does not
//! decide cache boundaries, TTL, thinking or request identity.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use runtime::{FsBackend, StdFsBackend};
use serde_json::{json, Value};

use crate::sse::MessageMetadata;

#[derive(Clone, Debug, Default)]
pub(super) struct CacheDiagnostics {
    state: Arc<Mutex<State>>,
    path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct State {
    loaded: bool,
    previous: Option<String>,
}

impl CacheDiagnostics {
    #[must_use]
    pub(super) fn for_session(directory: &Path) -> Self {
        Self {
            path: Some(directory.join("cache-diagnostics.json")),
            ..Self::default()
        }
    }

    /// Called only when diagnostics are enabled. Loading is lazy so the normal
    /// provider path performs no diagnostic filesystem reads or writes.
    pub(super) fn apply_request(&self, body: &mut Value) {
        let Some(body) = body.as_object_mut() else {
            return;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.loaded {
            state.previous = self
                .path
                .as_ref()
                .and_then(|path| StdFsBackend.read_to_string(&path.to_string_lossy()).ok())
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| {
                    value["message_id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                });
            state.loaded = true;
        }
        // Explicit null on the first request also enables server fingerprinting.
        body.insert(
            "diagnostics".into(),
            json!({"previous_message_id": state.previous}),
        );
    }

    pub(super) fn observe(&self, metadata: MessageMetadata) {
        if let Some(id) = metadata.id.as_ref().filter(|id| !id.is_empty()) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.previous = Some(id.clone());
            state.loaded = true;
            if let Some(path) = &self.path {
                // Best effort, like the existing usage ledger. A diagnostic
                // failure must never fail or retry a paid provider request.
                if let (Some(parent), Ok(bytes)) = (
                    path.parent(),
                    serde_json::to_vec(&json!({"message_id": state.previous})),
                ) {
                    let backend = StdFsBackend;
                    if backend.create_dir_all(&parent.to_string_lossy()).is_ok() {
                        let _ = backend.write_atomic(&path.to_string_lossy(), &bytes);
                    }
                }
            }
        }
        let Some(diagnostics) = metadata.diagnostics else {
            return;
        };
        if let Some(dir) = super::dump_request_dir() {
            let record = json!({"message_id": metadata.id, "diagnostics": diagnostics});
            let _ =
                super::write_request_dump(std::path::Path::new(&dir), "cache-diagnosis", &record);
        } else {
            eprintln!("[cache-diagnosis] {diagnostics}");
        }
    }
}
