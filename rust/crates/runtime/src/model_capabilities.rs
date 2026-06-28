//! Model Capabilities Service — dynamic model metadata from sudorouter.
//!
//! Maintains a per-agent-type SSOT file at
//! `{config_home}/cache/model-capabilities.json` that maps wire model IDs to
//! their context window and max output token limits. The file is refreshed
//! asynchronously from sudorouter's `/v1/models` endpoint and read
//! synchronously by `model_token_limit()` on the API hot path.
//!
//! Fallback chain:
//!   SSOT file (last pull or bundled initial) → heuristic (opus 32k, others 64k)

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::fs_backend::FsBackend;

/// Bundled model capabilities shipped with the binary. Serves as the initial
/// SSOT file content on first launch (before the first successful API pull).
const BUNDLED_CAPABILITIES: &str = include_str!("model-capabilities.bundled.json");

/// Stale threshold: refresh if `updated_at` is older than this.
const TTL_SECS: u64 = 24 * 60 * 60; // 24 hours

/// Token limit metadata for a single model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapability {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

/// The on-disk SSOT file schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilitiesFile {
    /// Unix timestamp (seconds) of the last successful refresh.
    pub updated_at: u64,
    /// Wire model ID → capability metadata.
    pub models: BTreeMap<String, ModelCapability>,
}

impl Default for ModelCapabilitiesFile {
    fn default() -> Self {
        parse_capabilities_json(BUNDLED_CAPABILITIES).unwrap_or_else(|| Self {
            updated_at: 0,
            models: BTreeMap::new(),
        })
    }
}

/// In-memory snapshot of the capabilities file, loaded once per session.
static CAPABILITIES: OnceLock<ModelCapabilitiesFile> = OnceLock::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Look up token limits for a wire model ID from the in-memory snapshot.
///
/// Handles provider-prefixed model IDs (`openai/gpt-5.4` → `gpt-5.4`).
/// Returns `None` if the model is unknown — callers should apply heuristic
/// defaults.
#[must_use]
pub fn lookup(model_id: &str) -> Option<ModelCapability> {
    let base = model_id.rsplit('/').next().unwrap_or(model_id);
    let caps = CAPABILITIES.get_or_init(ModelCapabilitiesFile::default);
    caps.models
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(base))
        .map(|(_, cap)| *cap)
}

/// Load the SSOT file into the in-memory snapshot. Call once at startup.
///
/// If the file doesn't exist, copies the bundled defaults into place and
/// loads those.
pub fn load(config_home: &Path, backend: &dyn FsBackend) {
    let path = cache_path(config_home);
    let file = match read_file(backend, &path) {
        Some(f) => f,
        None => {
            // First launch: seed with bundled defaults.
            let default = ModelCapabilitiesFile::default();
            let _ = write_file(backend, &path, &default);
            default
        }
    };
    // OnceLock::set fails silently if already set (e.g. test double-init).
    let _ = CAPABILITIES.set(file);
}

/// Returns `true` if the cached data is stale (older than TTL or missing).
#[must_use]
pub fn is_stale(config_home: &Path, backend: &dyn FsBackend) -> bool {
    let path = cache_path(config_home);
    match read_file(backend, &path) {
        Some(file) => {
            let now = now_secs();
            now.saturating_sub(file.updated_at) > TTL_SECS
        }
        None => true,
    }
}

/// Merge API response data into the SSOT file and write it atomically.
///
/// `api_models` is the parsed `/v1/models` response `data` array — each
/// entry should have `id`, and optionally `context_window` + `max_output_tokens`.
/// Models without both metadata fields are skipped. Existing entries from
/// the bundled JSON or previous pulls are preserved for models the API
/// doesn't cover.
///
/// Returns the number of models with capability metadata that were written.
pub fn merge_and_write(
    config_home: &Path,
    backend: &dyn FsBackend,
    api_models: &[ApiModelEntry],
) -> Result<usize, std::io::Error> {
    let path = cache_path(config_home);
    let mut file = read_file(backend, &path).unwrap_or_default();

    let mut count = 0usize;
    for entry in api_models {
        if let (Some(cw), Some(mo)) = (entry.context_window, entry.max_output_tokens) {
            file.models.insert(
                entry.id.clone(),
                ModelCapability {
                    context_window: cw,
                    max_output_tokens: mo,
                },
            );
            count += 1;
        }
    }

    file.updated_at = now_secs();
    write_file(backend, &path, &file)?;
    Ok(count)
}

/// A single model entry from the sudorouter `/v1/models` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiModelEntry {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// Parse the `/v1/models` API response JSON into a vec of model entries.
pub fn parse_api_response(json: &serde_json::Value) -> Vec<ApiModelEntry> {
    let Some(data) = json.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            let context_window = entry
                .get("context_window")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let max_output_tokens = entry
                .get("max_output_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            Some(ApiModelEntry {
                id,
                context_window,
                max_output_tokens,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn cache_path(config_home: &Path) -> PathBuf {
    config_home.join("cache").join("model-capabilities.json")
}

fn read_file(backend: &dyn FsBackend, path: &Path) -> Option<ModelCapabilitiesFile> {
    let json = backend.read_to_string(&path.to_string_lossy()).ok()?;
    parse_capabilities_json(&json)
}

fn write_file(
    backend: &dyn FsBackend,
    path: &Path,
    file: &ModelCapabilitiesFile,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        backend.create_dir_all(&parent.to_string_lossy())?;
    }
    let json = serde_json::to_string_pretty(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    backend.write_atomic(&path.to_string_lossy(), json.as_bytes())
}

fn parse_capabilities_json(json: &str) -> Option<ModelCapabilitiesFile> {
    serde_json::from_str(json).ok()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_json_parses() {
        let file: ModelCapabilitiesFile =
            serde_json::from_str(BUNDLED_CAPABILITIES).expect("bundled JSON must parse");
        assert!(!file.models.is_empty(), "bundled JSON must have models");
    }

    #[test]
    fn parse_api_response_extracts_models_with_metadata() {
        let json = serde_json::json!({
            "data": [
                { "id": "model-a", "context_window": 200000, "max_output_tokens": 32000 },
                { "id": "model-b" },
                { "id": "model-c", "context_window": 100000, "max_output_tokens": 8000 }
            ]
        });
        let entries = parse_api_response(&json);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].context_window, Some(200000));
        assert!(entries[1].context_window.is_none());
    }
}
