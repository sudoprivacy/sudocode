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
use std::sync::{OnceLock, RwLock};

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCapability {
    /// `None` when the model is listed but its window is undocumented —
    /// callers fall back to the table's [`ModelCapabilitiesFile::default`].
    ///
    /// Optional because "listed" and "documented" are different facts and the
    /// table has to be able to say so. Sudorouter lists `claude-opus-5-5` with
    /// `supported_endpoint_types` and no token metadata at all; when this field
    /// was `u32` the only way to store that model was to copy `default` into
    /// it, and the copy was indistinguishable from an authoritative number.
    /// On a 1M model carrying a copied 200K window that meant autocompaction
    /// at a fifth of the real budget — five times the prefix rebuilds — plus a
    /// preflight that rejected requests the model would have accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// `None` when undocumented; same rule as [`Self::context_window`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
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
    /// Supported API endpoint types from sudorouter (e.g. `["anthropic", "openai"]`).
    /// First item is preferred. Used by proxy passthrough to pick the optimal
    /// wire format. `None` when sudorouter hasn't populated this field yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_types: Option<Vec<String>>,
}

impl ModelCapability {
    /// A model the gateway listed but documented nothing about. Every field is
    /// unknown, so every caller takes its own fallback.
    fn undocumented() -> Self {
        Self {
            context_window: None,
            max_output_tokens: None,
            vision_supported: None,
            image_max_bytes: None,
            image_max_dimension: None,
            endpoint_types: None,
        }
    }
}

/// The unknown-model fallback, and the only place a concrete number is
/// mandatory: this *is* the answer to "we have no data for this model", so it
/// cannot itself be unknown. Deliberately not a [`ModelCapability`] — that
/// type's limits are optional and this one's must not be.
///
/// Both fields are required at the serde level, so a file missing `default`
/// fails to parse and callers fall back to the bundled seed rather than
/// silently running with a zero budget.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultLimits {
    pub context_window: u32,
    pub max_output_tokens: u32,
}

impl DefaultLimits {
    /// The default expressed as a per-model capability, for the one caller
    /// that needs a complete entry to apply a user override onto.
    fn as_capability(self) -> ModelCapability {
        ModelCapability {
            context_window: Some(self.context_window),
            max_output_tokens: Some(self.max_output_tokens),
            vision_supported: None,
            image_max_bytes: None,
            image_max_dimension: None,
            endpoint_types: None,
        }
    }
}

/// The on-disk SSOT file schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilitiesFile {
    /// Unix timestamp (seconds) of the last successful refresh.
    pub updated_at: u64,
    /// Fallback capability for models not present in `models`. This is the
    /// single source of truth for the "unknown model" default — it lives in
    /// the file, never hardcoded in code.
    ///
    /// **Re-seeded from the bundled JSON on every [`load`]**, unlike `models`.
    /// It used to be preserved verbatim across refreshes, which meant an
    /// install that first ran a binary seeding 200K kept 200K forever: the
    /// corrected 1M seed shipped in the binary reached new installs only. A
    /// sudorouter refresh never writes this field, so the bundle is its only
    /// author and there is nothing on disk worth preserving.
    pub default: DefaultLimits,
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

/// Per-model limit overrides from `sudocode.json`, keyed by lowercase wire
/// model ID. Seeded by [`apply_config_limits`] at startup; empty otherwise.
static CONFIG_LIMITS: RwLock<Option<BTreeMap<String, ModelLimitOverride>>> = RwLock::new(None);

/// Token-limit overrides a model entry may declare in `sudocode.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelLimitOverride {
    pub max_output_tokens: Option<u32>,
    pub context_window: Option<u32>,
}

impl ModelLimitOverride {
    fn is_empty(self) -> bool {
        self.max_output_tokens.is_none() && self.context_window.is_none()
    }

    fn apply(self, cap: &mut ModelCapability) {
        if let Some(value) = self.max_output_tokens {
            cap.max_output_tokens = Some(value);
        }
        if let Some(value) = self.context_window {
            cap.context_window = Some(value);
        }
    }
}

/// Seed the per-model limit overrides from a parsed `sudocode.json`.
///
/// The compiled-in capabilities table cannot cover every model a user points
/// `scode` at — an unknown model silently inherits the table's `default`
/// entry, and a too-large `max_tokens` is rejected by the provider
/// (`DashScope`
/// answers `Range of max_tokens should be [1, 32768]`). `maxOutputTokens` /
/// `contextWindow` on the model entry are the user-side patch for that.
///
/// Overrides are keyed by **wire model ID**, since that is what every
/// capability lookup on the hot path has in hand. Two aliases mapping to the
/// same wire ID therefore share one override; the last one wins (aliases are
/// visited in sorted order). Calling this again replaces the previous set.
pub fn apply_config_limits(config: &crate::config::SudoCodeConfig) {
    let mut overrides: BTreeMap<String, ModelLimitOverride> = BTreeMap::new();
    for entry in config.models.values() {
        let over = ModelLimitOverride {
            max_output_tokens: entry.max_output_tokens,
            context_window: entry.context_window,
        };
        if over.is_empty() {
            continue;
        }
        for mapping in entry.providers.values() {
            overrides.insert(mapping.model.to_ascii_lowercase(), over);
        }
    }
    let value = (!overrides.is_empty()).then_some(overrides);
    if let Ok(mut guard) = CONFIG_LIMITS.write() {
        *guard = value;
    }
}

/// The configured `maxOutputTokens` for a wire model ID, if the user set one.
///
/// Callers that apply their own heuristic cap use this to tell "the table's
/// number" (cap it) from "the user's number" (obey it).
#[must_use]
pub fn config_max_output_tokens(model_id: &str) -> Option<u32> {
    config_limit_for(model_id).and_then(|over| over.max_output_tokens)
}

/// The `max_tokens` a chat request for this model is sent with.
///
/// An explicit `models.<alias>.maxOutputTokens` is the user's own number and
/// is taken as written. Otherwise a heuristic default (32K for opus, 64K for
/// everything else) capped by the model's registered `max_output_tokens`
/// when the model is known.
///
/// This is the number the provider — and the API client's local preflight —
/// adds to the input when deciding whether a request fits the context
/// window, so everything that budgets history against the window must
/// subtract *this*, not the model's nominal `max_output_tokens` and not the
/// compaction summary's much smaller reservation. Consumers: the
/// auto-compaction threshold, [`crate::ContextBudget`], and
/// `api::max_tokens_for_model`, which builds the request itself.
#[must_use]
pub fn request_max_output_tokens(model_id: &str) -> u32 {
    if let Some(configured) = config_max_output_tokens(model_id) {
        return configured;
    }
    let heuristic = if model_id.contains("opus") {
        32_000
    } else {
        64_000
    };
    lookup(model_id)
        .and_then(|cap| cap.max_output_tokens)
        .map_or(heuristic, |registered| heuristic.min(registered))
}

/// Anthropic's minimum `thinking.budget_tokens`; a smaller budget is rejected.
pub const MIN_THINKING_BUDGET_TOKENS: u32 = 1024;

/// The `thinking.budget_tokens` every request in a session declares.
///
/// A property of the model, deliberately *not* of the request's own
/// `max_tokens`, because the value of the thinking parameter is part of
/// Anthropic's prompt-cache key. Deriving it per request from `max_tokens / 2`
/// meant a compaction request — which asks for a smaller output cap than a
/// turn — declared a different budget than the turn whose prefix it was
/// replaying byte-for-byte, and silently read nothing. Measured on a live
/// route, three interleaved repetitions of each arm, identical prefixes, both
/// arms HTTP 200
/// (`ladder/tools/cache_prefix_probe.py --pairs budget-changed-on-turn2
/// thinking-on-returned`):
///
///   budget 2048 on turn 1, 6000 on turn 2 ... read    0 / write 3132, 3153
///   budget unchanged (control) ............. read 3041 / 3026 / 3020
///
/// Half the output budget is generous enough for deep reasoning on a
/// large-context model (32K on a 64K model) while leaving the other half for
/// the visible response. The API additionally requires
/// `budget_tokens < max_tokens`, so a caller that wants this budget honoured
/// unclamped must ask for an output cap above it — see
/// `COMPACT_MAX_OUTPUT_TOKENS`'s use in `compact_session_cache_safe`.
#[must_use]
pub fn thinking_budget_tokens(model_id: &str) -> u32 {
    (request_max_output_tokens(model_id) / 2).max(MIN_THINKING_BUDGET_TOKENS)
}

/// Look up the configured override for a wire model ID, if any.
fn config_limit_for(model_id: &str) -> Option<ModelLimitOverride> {
    let guard = CONFIG_LIMITS.read().ok()?;
    let map = guard.as_ref()?;
    let full_id = model_id.to_ascii_lowercase();
    // A provider prefix or local deployment path can be part of the actual
    // model ID. Honor an exact configured ID before the legacy basename fallback.
    map.get(&full_id)
        .or_else(|| map.get(full_id.rsplit('/').next().unwrap_or(&full_id)))
        .copied()
}

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
    let active = crate::model_discovery::active_snapshot();
    let scoped = active.is_some();
    let caps = active.unwrap_or_else(|| {
        CAPABILITIES
            .get_or_init(ModelCapabilitiesFile::default)
            .clone()
    });
    let table_entry = caps
        .models
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(base))
        .map(|(_, cap)| cap.clone())
        .or_else(|| {
            // Published limits remain useful before the first response. This
            // fallback does not add IDs to the account's available-model list.
            scoped
                .then(ModelCapabilitiesFile::default)
                .and_then(|bundle| {
                    bundle
                        .models
                        .into_iter()
                        .find(|(id, _)| id.eq_ignore_ascii_case(base))
                        .map(|(_, cap)| cap)
                })
        });
    let Some(over) = config_limit_for(model_id) else {
        return table_entry;
    };
    // A configured model is "known" even when the table has never heard of
    // it: start from the table's `default` entry so the override lands on a
    // complete capability rather than being dropped. Filling the
    // un-overridden half from `default` is fine here and is *not* the
    // fabrication `merge_and_write` avoids — the user named this model in
    // `sudocode.json`, so falling back to the documented default is a path
    // they asked for, not a number invented for every model the gateway
    // happens to list.
    let mut cap = table_entry.unwrap_or_else(|| caps.default.as_capability());
    over.apply(&mut cap);
    Some(cap)
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

/// Preferred API endpoint type for a model from the SSOT.
/// Returns the first entry in `endpoint_types` (preferred by sudorouter).
/// Returns `None` if the model is unknown or has no endpoint type data.
#[must_use]
pub fn preferred_endpoint_type(model_id: &str) -> Option<String> {
    lookup(model_id)
        .and_then(|cap| cap.endpoint_types)
        .and_then(|types| types.into_iter().next())
}

/// All known model IDs from the capabilities SSOT.
/// Used by discovery surfaces (`/model`, tab completion, `get_model_info`).
#[must_use]
pub fn all_model_ids() -> Vec<String> {
    let caps = crate::model_discovery::active_snapshot().unwrap_or_else(|| {
        CAPABILITIES
            .get_or_init(ModelCapabilitiesFile::default)
            .clone()
    });
    caps.models.keys().cloned().collect()
}

/// Merge config alias keys with capabilities SSOT model IDs into a single
/// discovery list: config keys first (preserving order), then capabilities
/// IDs not already covered (case-insensitive dedup).
///
/// This is the DRY helper for all flat-list discovery surfaces
/// (`get_model_info`, FuzzySelect picker). Call sites that need two-phase
/// formatting (config aliases with rich display vs. capabilities wire IDs)
/// should use [`all_model_ids`] directly.
#[inline]
#[must_use]
pub fn merge_discovery_ids(config_keys: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = config_keys.to_vec();
    let seen: std::collections::BTreeSet<String> =
        config_keys.iter().map(|k| k.to_ascii_lowercase()).collect();
    for id in all_model_ids() {
        if !seen.contains(&id.to_ascii_lowercase()) {
            merged.push(id);
        }
    }
    merged
}

/// Context window for a wire model ID, falling back to the SSOT file's
/// `default` entry when the model is unknown **or listed without a documented
/// window**. The single source of truth for both per-model values and the
/// unknown-model default is this capabilities file (bundled seed or sudorouter
/// refresh) — never a hardcoded constant.
#[must_use]
pub fn context_window_or_default(model_id: &str) -> u32 {
    lookup(model_id)
        .and_then(|cap| cap.context_window)
        .unwrap_or_else(|| {
            crate::model_discovery::active_snapshot()
                .unwrap_or_else(|| {
                    CAPABILITIES
                        .get_or_init(ModelCapabilitiesFile::default)
                        .clone()
                })
                .default
                .context_window
        })
}

/// Max output tokens for a wire model ID, falling back to the SSOT file's
/// `default` entry when the model is unknown. Mirrors
/// [`context_window_or_default`] for the output-token dimension.
#[must_use]
pub fn max_output_tokens_or_default(model_id: &str) -> u32 {
    lookup(model_id)
        .and_then(|cap| cap.max_output_tokens)
        .unwrap_or_else(|| {
            crate::model_discovery::active_snapshot()
                .unwrap_or_else(|| {
                    CAPABILITIES
                        .get_or_init(ModelCapabilitiesFile::default)
                        .clone()
                })
                .default
                .max_output_tokens
        })
}

/// Load the SSOT file into the in-memory snapshot. Call once at startup.
///
/// If the file doesn't exist, copies the bundled defaults into place and
/// loads those.
///
/// The `default` entry always comes from the bundled seed, never from disk:
/// no refresh writes that field, so the binary is its only author, and reading
/// the stale on-disk copy froze every install at whatever value its *first*
/// scode version shipped. `models` is still read from disk — that half really
/// is refreshed data and the bundle is only its seed.
pub fn load(config_home: &Path, backend: &dyn FsBackend) {
    let path = cache_path(config_home);
    let file = match read_file(backend, &path) {
        Some(mut f) => {
            f.default = ModelCapabilitiesFile::default().default;
            f
        }
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
/// Existing entries from the bundled JSON or previous pulls are preserved for
/// models the API doesn't cover.
///
/// Token limits come from the live response when it documents them, else from
/// the bundled seed, else nowhere — a listed-but-undocumented model is stored
/// with its limits unset rather than with a copy of `default`. Note that the
/// previous on-disk limits are deliberately *not* a fallback: reusing them is
/// how a fabricated window survived every later refresh.
///
/// Returns the number of models with capability metadata that were written.
pub fn merge_and_write(
    config_home: &Path,
    backend: &dyn FsBackend,
    api_models: &[ApiModelEntry],
) -> Result<usize, std::io::Error> {
    let path = cache_path(config_home);
    let bundled = ModelCapabilitiesFile::default();
    let mut file = read_file(backend, &path).unwrap_or_else(|| bundled.clone());
    // Same rule as `load`: the bundle owns `default`, disk owns `models`.
    file.default = bundled.default;

    let mut count = 0usize;
    for entry in api_models {
        // Models with capability metadata get full entries.
        if let (Some(cw), Some(mo)) = (entry.context_window, entry.max_output_tokens) {
            file.models.insert(
                entry.id.clone(),
                ModelCapability {
                    context_window: Some(cw),
                    max_output_tokens: Some(mo),
                    vision_supported: entry.vision_supported,
                    image_max_bytes: entry.image_max_bytes,
                    image_max_dimension: entry.image_max_dimension,
                    endpoint_types: entry.supported_endpoint_types.clone(),
                },
            );
            count += 1;
        } else if entry.supported_endpoint_types.is_some() {
            // Listed with no token metadata. Record what the gateway did say —
            // `endpoint_types` decides the wire format, so dropping the model
            // entirely would cost more than an unknown window does — and take
            // the limits from the bundled seed if it curates this model,
            // otherwise leave them unset.
            //
            // The limits are recomputed from scratch every refresh rather than
            // carried over from the previous entry. Carrying them over is what
            // made this branch's old `default` copy permanent: sudorouter lists
            // `claude-opus-5-5` with endpoint types and no token metadata, so
            // every refresh re-confirmed a 200K window on a 1M model.
            let curated = bundled.models.get(&entry.id);
            let mut cap = file
                .models
                .get(&entry.id)
                .cloned()
                .unwrap_or_else(ModelCapability::undocumented);
            cap.context_window = curated.and_then(|c| c.context_window);
            cap.max_output_tokens = curated.and_then(|c| c.max_output_tokens);
            cap.endpoint_types = entry.supported_endpoint_types.clone();
            file.models.insert(entry.id.clone(), cap);
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
    #[serde(default)]
    pub supported_endpoint_types: Option<Vec<String>>,
}

/// Parse the `/v1/models` API response JSON into a vec of model entries.
pub fn parse_api_response(json: &serde_json::Value) -> Vec<ApiModelEntry> {
    let Some(data) = json.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            // Accept both sudocode field names (context_window /
            // max_output_tokens) and OpenAI-standard names
            // (context_length / max_tokens) for compatibility with
            // nova-gateway's /v1/models response.
            let context_window = entry
                .get("context_window")
                .or_else(|| entry.get("context_length"))
                .or_else(|| entry.get("max_input_tokens"))
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok().filter(|v| *v > 0));
            let max_output_tokens = entry
                .get("max_output_tokens")
                .or_else(|| entry.get("max_tokens"))
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok().filter(|v| *v > 0));
            let vision_supported = entry
                .get("vision_supported")
                .and_then(|v| v.as_bool())
                .or_else(|| {
                    entry
                        .pointer("/capabilities/image_input/supported")
                        .and_then(|v| v.as_bool())
                });
            let image_max_bytes = entry
                .get("image_max_bytes")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok().filter(|v| *v > 0));
            let image_max_dimension = entry
                .get("image_max_dimension")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok().filter(|v| *v > 0));
            let supported_endpoint_types = entry
                .get("supported_endpoint_types")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
            Some(ApiModelEntry {
                id,
                context_window,
                max_output_tokens,
                vision_supported,
                image_max_bytes,
                image_max_dimension,
                supported_endpoint_types,
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
    fn all_model_ids_returns_bundled_models() {
        let ids = all_model_ids();
        assert!(
            ids.len() >= 30,
            "expected ≥30 bundled model IDs, got {}",
            ids.len()
        );
    }

    #[test]
    fn merge_discovery_ids_deduplicates_case_insensitive() {
        // Config key "claude-opus-4-6" overlaps with capabilities — should not appear twice.
        let config_keys = vec!["claude-opus-4-6".to_string(), "custom-alias".to_string()];
        let merged = merge_discovery_ids(&config_keys);
        // Config keys come first, in order.
        assert_eq!(&merged[0], "claude-opus-4-6");
        assert_eq!(&merged[1], "custom-alias");
        // No duplicates of config keys.
        let count = merged
            .iter()
            .filter(|id| id.eq_ignore_ascii_case("claude-opus-4-6"))
            .count();
        assert_eq!(count, 1, "claude-opus-4-6 should appear exactly once");
        // Capabilities models are appended.
        assert!(
            merged.len() > config_keys.len(),
            "should have more models than just config keys"
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
