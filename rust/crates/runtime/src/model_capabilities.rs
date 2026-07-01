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

/// Token limit + image-cap metadata for a single model.
///
/// All image-cap fields are optional + `serde(default)` so existing on-disk
/// JSON files (and the bundled seed) deserialize without modification.
/// Sudorouter populates them per-model via `/v1/models` (commit 784fbf0,
/// 2026-07-01) — until a model's entry lands in the SSOT table, callers use
/// [`vision_capable`] / [`per_model_image_cap`] which fall back to
/// optimistic defaults; see those fns' docs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapability {
    pub context_window: u32,
    pub max_output_tokens: u32,
    /// `true`/`false` when documented; `None` when unknown (→ optimistic fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_supported: Option<bool>,
    /// Per-image byte cap the model's API will actually accept. `None` (or
    /// `0`) means "sudorouter doesn't have a documented number" — fall back
    /// to sudocode's conservative default in [`crate::image_registry`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_bytes: Option<u32>,
    /// Per-image longest-edge pixel cap. Same fallback rule as
    /// [`Self::image_max_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_dimension: Option<u32>,
}

/// The on-disk SSOT file schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilitiesFile {
    /// Unix timestamp (seconds) of the last successful refresh.
    pub updated_at: u64,
    /// Fallback capability for models not present in `models`. This is the
    /// single source of truth for the "unknown model" default — it lives in
    /// the SSOT file (bundled seed, preserved across sudorouter refreshes),
    /// never hardcoded in code.
    pub default: ModelCapability,
    /// Wire model ID → capability metadata.
    pub models: BTreeMap<String, ModelCapability>,
}

impl Default for ModelCapabilitiesFile {
    fn default() -> Self {
        // The bundled JSON is compiled into the binary, so a parse failure is a
        // build defect (malformed asset, or missing the required `default`
        // entry) — fail fast rather than silently degrading to an empty,
        // default-less table.
        parse_capabilities_json(BUNDLED_CAPABILITIES)
            .expect("bundled model-capabilities.json must parse and contain a 'default' entry")
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

/// Returns `true` if the model is known to accept image input. Used by the
/// push_images path to decide whether to send the image natively or route it
/// through a VLM-describe side-call (substituting a text description into the
/// prompt).
///
/// Resolution order:
///   1. If the SSOT file has an explicit `vision_supported` value for this
///      model, use it (populated by sudorouter's `/v1/models` — preferred).
///   2. Otherwise default to **true**: 2026-era frontier chat models all
///      accept image input, and the cost of a false-positive (one wasteful
///      native send to a text-only model) is bounded by the upstream API's
///      own rejection. False-negatives (treating a vision model as text-only
///      and burning a VLM round-trip on every image) would be the more
///      common silent regression — keep the default optimistic until the
///      SSOT is filled in. **This matches sudorouter's contract**: they
///      emit `*bool` so `false` == documented text-only (fires wrong-model
///      route correctly), `None` == unknown (optimistic).
#[must_use]
pub fn vision_capable(model_id: &str) -> bool {
    lookup(model_id)
        .and_then(|cap| cap.vision_supported)
        .unwrap_or(true)
}

/// Per-model image byte/dimension cap, when sudorouter has documented values
/// for this model; `(None, None)` when unknown. Callers (e.g.
/// `image_registry::capability`) fall back to sudocode's conservative
/// defaults when either half is missing.
///
/// Documented today (per sudorouter): Anthropic 5 MB / 8000 px, OpenAI 20 MB,
/// Gemini ~7 MB. Not documented (returns None): Grok, Qwen-vl, Llama-4,
/// Doubao — sudorouter will backfill as canonical numbers become available.
#[must_use]
pub fn per_model_image_cap(model_id: &str) -> (Option<u32>, Option<u32>) {
    match lookup(model_id) {
        Some(cap) => (
            cap.image_max_bytes.filter(|&n| n > 0),
            cap.image_max_dimension.filter(|&n| n > 0),
        ),
        None => (None, None),
    }
}

/// Context window for a wire model ID, falling back to the SSOT file's
/// `default` entry when the model is unknown. The single source of truth for
/// both per-model values and the unknown-model default is this capabilities
/// file (bundled seed or sudorouter refresh) — never a hardcoded constant.
#[must_use]
pub fn context_window_or_default(model_id: &str) -> u32 {
    lookup(model_id).map_or_else(
        || {
            CAPABILITIES
                .get_or_init(ModelCapabilitiesFile::default)
                .default
                .context_window
        },
        |cap| cap.context_window,
    )
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
                    vision_supported: entry.vision_supported,
                    image_max_bytes: entry.image_max_bytes,
                    image_max_dimension: entry.image_max_dimension,
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
    #[serde(default)]
    pub vision_supported: Option<bool>,
    #[serde(default)]
    pub image_max_bytes: Option<u32>,
    #[serde(default)]
    pub image_max_dimension: Option<u32>,
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
            let vision_supported = entry.get("vision_supported").and_then(|v| v.as_bool());
            let image_max_bytes = entry
                .get("image_max_bytes")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let image_max_dimension = entry
                .get("image_max_dimension")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            Some(ApiModelEntry {
                id,
                context_window,
                max_output_tokens,
                vision_supported,
                image_max_bytes,
                image_max_dimension,
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
        // The `default` entry is the SSOT for the unknown-model fallback; guard
        // that the bundled seed carries a sane value (no hardcoded fallback exists
        // in code anymore, so a missing/zero default would be a real regression).
        assert!(
            file.default.context_window > 0,
            "bundled JSON must define a non-zero default context window"
        );
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
