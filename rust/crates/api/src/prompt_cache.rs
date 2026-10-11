//! Per-session record of how the provider's prompt cache behaved.
//!
//! **Only the Anthropic provider feeds this.** The OpenAI-compatible, Gemini
//! and Codex clients never call in, so a session routed through them leaves
//! no record here — empty stats mean "not measured", not "no cache problems".
//!
//! Extending it is not just wiring. OpenAI reports `cached_tokens` but has no
//! cache-write concept, so `cache_creation_input_tokens` is always zero there
//! and a read/(read+write) ratio over mixed providers would read 100% no
//! matter what actually happened. Break detection would still work; the
//! totals would not be comparable.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::types::{MessageRequest, MessageResponse, Usage};

const DEFAULT_COMPLETION_TTL_SECS: u64 = 30;
const DEFAULT_PROMPT_TTL_SECS: u64 = 5 * 60;
const DEFAULT_BREAK_MIN_DROP: u32 = 2_000;
const MAX_SANITIZED_LENGTH: usize = 80;
const REQUEST_FINGERPRINT_VERSION: u32 = 2;
const REQUEST_FINGERPRINT_PREFIX: &str = "v1";
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Debug, Clone)]
pub struct PromptCacheConfig {
    pub session_id: String,
    pub completion_ttl: Duration,
    pub prompt_ttl: Duration,
    pub cache_break_min_drop: u32,
}

impl PromptCacheConfig {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            completion_ttl: Duration::from_secs(DEFAULT_COMPLETION_TTL_SECS),
            prompt_ttl: Duration::from_secs(DEFAULT_PROMPT_TTL_SECS),
            cache_break_min_drop: DEFAULT_BREAK_MIN_DROP,
        }
    }
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self::new("default")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCachePaths {
    pub root: PathBuf,
    pub session_dir: PathBuf,
    pub completion_dir: PathBuf,
    pub session_state_path: PathBuf,
    pub stats_path: PathBuf,
    /// Append-only, one line per tracked request. `stats.json` is a rollup and
    /// answers "how is the cache doing"; this answers "what happened, in what
    /// order, and on whose account" — which a rollup structurally cannot,
    /// because the moment a prefix goes cold is a point in a sequence.
    pub requests_path: PathBuf,
}

impl PromptCachePaths {
    #[must_use]
    pub fn for_session(session_id: &str) -> Self {
        let root = base_cache_root();
        let session_dir = root.join(sanitize_path_segment(session_id));
        let completion_dir = session_dir.join("completions");
        Self {
            root,
            session_state_path: session_dir.join("session-state.json"),
            stats_path: session_dir.join("stats.json"),
            requests_path: session_dir.join("requests.jsonl"),
            session_dir,
            completion_dir,
        }
    }

    #[must_use]
    pub fn completion_entry_path(&self, request_hash: &str) -> PathBuf {
        self.completion_dir.join(format!("{request_hash}.json"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheStats {
    pub tracked_requests: u64,
    pub completion_cache_hits: u64,
    pub completion_cache_misses: u64,
    pub completion_cache_writes: u64,
    pub expected_invalidations: u64,
    pub unexpected_cache_breaks: u64,
    /// How many breaks each cause took part in, keyed by [`cache_break_cause`].
    ///
    /// One break can have several causes, so these do not sum to the two
    /// counters above. This is the breakdown `scode cache stats` reports:
    /// without it the rollup keeps only `last_break_reason`, so a session that
    /// threw its prefix away ten times over changed tool definitions looked
    /// the same as one that never did.
    #[serde(default)]
    pub breaks_by_cause: BTreeMap<String, u64>,
    pub total_cache_creation_input_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    /// Legacy rollups did not retain uncached input. Coverage must match the
    /// tracked count before reporting percentages over all prompt tokens.
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub input_tokens_observed_requests: u64,
    pub last_cache_creation_input_tokens: Option<u32>,
    pub last_cache_read_input_tokens: Option<u32>,
    pub last_request_hash: Option<String>,
    pub last_completion_cache_key: Option<String>,
    pub last_break_reason: Option<String>,
    pub last_cache_source: Option<String>,
}

/// Stable, index-free names for what changed, for tallying across sessions.
///
/// `reason` is prose and carries message indices, so counting raw reason
/// strings never aggregates. These do.
pub mod cache_break_cause {
    pub const MODEL: &str = "model";
    pub const SYSTEM: &str = "system";
    pub const TOOLS: &str = "tools";
    pub const MESSAGES_REWRITTEN: &str = "messages-rewritten";
    pub const TTL_EXPIRY: &str = "ttl-expiry";
    pub const FINGERPRINT_VERSION: &str = "fingerprint-version";
    pub const UNEXPLAINED: &str = "unexplained";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakEvent {
    pub unexpected: bool,
    pub reason: String,
    /// Which parts of the request changed, from [`cache_break_cause`].
    ///
    /// Separate from `unexpected`, which asks only whether the request
    /// explains the break. A break we can explain is still a break we may be
    /// inflicting on ourselves: a mid-session `tools` change is "explained",
    /// and it throws away the whole prefix, so bucketing it as expected hid
    /// exactly the class of defect this record exists to find.
    #[serde(default)]
    pub causes: Vec<String>,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// One line of [`PromptCachePaths::requests_path`].
///
/// Deliberately small: enough to reconstruct a session's cache history and to
/// join it against a gateway's own ledger, and nothing else. The join key is
/// `gateway_request_id` — a pooling gateway records that same value against the
/// upstream account it picked, so this is what turns "the prefix went cold" into
/// "the prefix went cold *because the account changed*". Without it those two
/// are indistinguishable from here, which is why a rollup was never going to be
/// enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheRequestRow {
    pub at_unix_secs: u64,
    /// The echoed gateway correlation id, or the UUID sent for this HTTP attempt.
    /// Resolving the latter requires the route to preserve `x-client-request-id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_request_id: Option<String>,
    /// The provider's (or gateway's own) request id, as the response reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    pub model: String,
    pub input_tokens: u32,
    pub cache_read_input_tokens: u32,
    pub cache_creation_input_tokens: u32,
    /// Present only when this request broke the cache; `unexpected` separates
    /// "the request changed, so of course it did" from "it should have hit".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub break_reason: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub break_unexpected: bool,
}

/// The correlation ids a single response carried.
///
/// Exists because the streaming path used to lose one of them. The row was
/// built from `Option<&MessageResponse>`, and the stream has no assembled
/// response to pass — so `provider_request_id` was structurally unreachable for
/// every streaming request, which is **all** real traffic. Measured against a
/// live gateway: 480 consecutive ledger rows carried neither id, while the SSE
/// response itself carried `x-oneapi-request-id` the whole time.
///
/// Naming the two fields instead of taking two `Option<&str>` parameters is the
/// point: they have the same type and index different ledgers, so a positional
/// call site can swap them, and dropping one is invisible at the call site.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseTraceIds {
    /// `x-client-request-id` — which upstream *account* a pooling gateway
    /// selected. The only value that separates "cold prefix" from "the request
    /// went to a different account", which is the first question in every
    /// cache-break investigation.
    pub gateway: Option<String>,
    /// `request-id` / `x-request-id` / `x-oneapi-request-id` — keys the
    /// gateway's own request log: which channel served the call, and its cost.
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheRecord {
    pub cache_break: Option<CacheBreakEvent>,
    pub stats: PromptCacheStats,
}

#[derive(Debug, Clone)]
pub struct PromptCache {
    inner: Arc<Mutex<PromptCacheInner>>,
}

impl PromptCache {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_config(PromptCacheConfig::new(session_id))
    }

    #[must_use]
    pub fn with_config(config: PromptCacheConfig) -> Self {
        let paths = PromptCachePaths::for_session(&config.session_id);
        let stats = read_json::<PromptCacheStats>(&paths.stats_path).unwrap_or_default();
        let previous = read_json::<TrackedPromptState>(&paths.session_state_path);
        Self {
            inner: Arc::new(Mutex::new(PromptCacheInner {
                config,
                paths,
                stats,
                previous,
            })),
        }
    }

    #[must_use]
    pub fn paths(&self) -> PromptCachePaths {
        self.lock().paths.clone()
    }

    #[must_use]
    pub fn stats(&self) -> PromptCacheStats {
        self.lock().stats.clone()
    }

    #[must_use]
    pub fn lookup_completion(&self, request: &MessageRequest) -> Option<MessageResponse> {
        let request_hash = request_hash_hex(request);
        let (paths, ttl) = {
            let inner = self.lock();
            (inner.paths.clone(), inner.config.completion_ttl)
        };
        let entry_path = paths.completion_entry_path(&request_hash);
        let entry = read_json::<CompletionCacheEntry>(&entry_path);
        let Some(entry) = entry else {
            let mut inner = self.lock();
            inner.stats.completion_cache_misses += 1;
            inner.stats.last_completion_cache_key = Some(request_hash);
            persist_state(&inner);
            return None;
        };

        if entry.fingerprint_version != current_fingerprint_version() {
            let mut inner = self.lock();
            inner.stats.completion_cache_misses += 1;
            inner.stats.last_completion_cache_key = Some(request_hash.clone());
            let _ = fs::remove_file(entry_path);
            persist_state(&inner);
            return None;
        }

        let expired = now_unix_secs().saturating_sub(entry.cached_at_unix_secs) >= ttl.as_secs();
        let mut inner = self.lock();
        inner.stats.last_completion_cache_key = Some(request_hash.clone());
        if expired {
            inner.stats.completion_cache_misses += 1;
            let _ = fs::remove_file(entry_path);
            persist_state(&inner);
            return None;
        }

        inner.stats.completion_cache_hits += 1;
        // A local replay sends no provider request and cannot refresh its
        // prompt cache. Keep the last real usage and timestamp intact.
        inner.stats.last_cache_source = Some("completion-cache".to_owned());
        persist_state(&inner);
        Some(entry.response)
    }

    #[must_use]
    pub fn record_response(
        &self,
        request: &MessageRequest,
        response: &MessageResponse,
    ) -> PromptCacheRecord {
        self.record_usage_internal(
            request,
            &response.usage,
            ResponseTraceIds {
                gateway: response.gateway_request_id.clone(),
                provider: response.request_id.clone(),
            },
            Some(response),
        )
    }

    /// The streaming path, where there is no assembled `MessageResponse`.
    ///
    /// The ids are a parameter here rather than being read off something,
    /// because the stream holds them separately — and losing one silently costs
    /// the correlation this row exists to provide, so they are not defaulted
    /// away. They arrive as a struct for the reason the field names say: both
    /// are `Option<String>`, so two positional arguments can be swapped or
    /// half-supplied without the compiler noticing.
    #[must_use]
    pub fn record_usage(
        &self,
        request: &MessageRequest,
        usage: &Usage,
        ids: ResponseTraceIds,
    ) -> PromptCacheRecord {
        self.record_usage_internal(request, usage, ids, None)
    }

    fn record_usage_internal(
        &self,
        request: &MessageRequest,
        usage: &Usage,
        ids: ResponseTraceIds,
        // Only the completion cache needs the assembled response, and only the
        // non-streaming path has one. It is deliberately *not* where the row's
        // ids come from any more: deriving them from a value the streaming path
        // cannot supply is what made `provider_request_id` unreachable for all
        // real traffic.
        completion_entry: Option<&MessageResponse>,
    ) -> PromptCacheRecord {
        let request_hash = request_hash_hex(request);
        let mut inner = self.lock();
        let previous = inner.previous.clone();
        let current = TrackedPromptState::from_usage(request, usage);
        let cache_break = detect_cache_break(&inner.config, previous.as_ref(), &current);

        inner.stats.tracked_requests += 1;
        apply_usage_to_stats(&mut inner.stats, usage, &request_hash, "api-response");
        if let Some(event) = &cache_break {
            if event.unexpected {
                inner.stats.unexpected_cache_breaks += 1;
            } else {
                inner.stats.expected_invalidations += 1;
            }
            inner.stats.last_break_reason = Some(event.reason.clone());
            for cause in &event.causes {
                *inner
                    .stats
                    .breaks_by_cause
                    .entry(cause.clone())
                    .or_insert(0) += 1;
            }
        }

        inner.previous = Some(current);
        append_request_row(
            &inner.paths,
            &PromptCacheRequestRow {
                at_unix_secs: now_unix_secs(),
                gateway_request_id: ids.gateway,
                provider_request_id: ids.provider,
                model: request.model.clone(),
                input_tokens: usage.input_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                break_reason: cache_break.as_ref().map(|event| event.reason.clone()),
                break_unexpected: cache_break.as_ref().is_some_and(|event| event.unexpected),
            },
        );
        if let Some(response) = completion_entry {
            write_completion_entry(&inner.paths, &request_hash, response);
            inner.stats.completion_cache_writes += 1;
        }
        persist_state(&inner);

        PromptCacheRecord {
            cache_break,
            stats: inner.stats.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PromptCacheInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
struct PromptCacheInner {
    config: PromptCacheConfig,
    paths: PromptCachePaths,
    stats: PromptCacheStats,
    previous: Option<TrackedPromptState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionCacheEntry {
    cached_at_unix_secs: u64,
    #[serde(default = "current_fingerprint_version")]
    fingerprint_version: u32,
    response: MessageResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrackedPromptState {
    observed_at_unix_secs: u64,
    #[serde(default = "current_fingerprint_version")]
    fingerprint_version: u32,
    model_hash: u64,
    system_hash: u64,
    tools_hash: u64,
    /// Cumulative hash after each message, so two turns can be compared by
    /// their common prefix rather than by one hash over the whole array.
    ///
    /// A single `messages_hash` cannot tell the two apart, and they are not
    /// remotely equivalent: appending a turn leaves the cached prefix intact,
    /// while rewriting an earlier message invalidates everything after it.
    /// Because messages grow on every turn the whole-array hash always
    /// differed, so "message payload changed" was reported on every break and
    /// carried no information.
    message_hashes: Vec<u64>,
    cache_read_input_tokens: u32,
}

impl TrackedPromptState {
    fn from_usage(request: &MessageRequest, usage: &Usage) -> Self {
        let hashes = RequestFingerprints::from_request(request);
        Self {
            observed_at_unix_secs: now_unix_secs(),
            fingerprint_version: current_fingerprint_version(),
            model_hash: hashes.model,
            system_hash: hashes.system,
            tools_hash: hashes.tools,
            message_hashes: message_prefix_hashes(request),
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestFingerprints {
    model: u64,
    system: u64,
    tools: u64,
}

impl RequestFingerprints {
    fn from_request(request: &MessageRequest) -> Self {
        Self {
            model: hash_serializable(&request.model),
            system: hash_serializable(&request.system),
            tools: hash_serializable(&request.tools),
        }
    }
}

fn detect_cache_break(
    config: &PromptCacheConfig,
    previous: Option<&TrackedPromptState>,
    current: &TrackedPromptState,
) -> Option<CacheBreakEvent> {
    let previous = previous?;
    if previous.fingerprint_version != current.fingerprint_version {
        return Some(CacheBreakEvent {
            unexpected: false,
            reason: format!(
                "fingerprint version changed (v{} -> v{})",
                previous.fingerprint_version, current.fingerprint_version
            ),
            causes: vec![cache_break_cause::FINGERPRINT_VERSION.to_string()],
            previous_cache_read_input_tokens: previous.cache_read_input_tokens,
            current_cache_read_input_tokens: current.cache_read_input_tokens,
            token_drop: previous
                .cache_read_input_tokens
                .saturating_sub(current.cache_read_input_tokens),
        });
    }
    let token_drop = previous
        .cache_read_input_tokens
        .saturating_sub(current.cache_read_input_tokens);

    let mut reasons = Vec::new();
    let mut causes = Vec::new();
    if previous.model_hash != current.model_hash {
        reasons.push("model changed");
        causes.push(cache_break_cause::MODEL.to_string());
    }
    if previous.system_hash != current.system_hash {
        reasons.push("system prompt changed");
        causes.push(cache_break_cause::SYSTEM.to_string());
    }
    if previous.tools_hash != current.tools_hash {
        reasons.push("tool definitions changed");
        causes.push(cache_break_cause::TOOLS.to_string());
    }
    // Appending a turn is the normal case and leaves the cached prefix
    // whole; only a rewrite *inside* the prefix invalidates it. Reporting
    // both as "message payload changed" made the reason useless, because the
    // whole-array hash differs on every single turn.
    let shared = common_prefix_len(&previous.message_hashes, &current.message_hashes);
    let rewritten = shared < previous.message_hashes.len();
    let rewrite_detail = rewritten.then(|| {
        format!(
            "message history rewritten at index {shared} (had {}, now {})",
            previous.message_hashes.len(),
            current.message_hashes.len()
        )
    });
    if let Some(detail) = &rewrite_detail {
        reasons.push(detail.as_str());
        causes.push(cache_break_cause::MESSAGES_REWRITTEN.to_string());
    }

    let elapsed = current
        .observed_at_unix_secs
        .saturating_sub(previous.observed_at_unix_secs);

    let (unexpected, reason) = if reasons.is_empty() {
        // Nothing in the request explains a break, so the only evidence left is
        // the token counts — and they have to have moved enough to mean
        // something. This gate belongs *here*, not above the fingerprint
        // comparison where it used to sit: a prefix thrown away before it was
        // ever read shows no drop at all. A live 3-turn session revealed a
        // deferred tool on turn 2, which rewrote the whole prefix (read 0,
        // written 8421, right after turn 1 wrote 7926) — reads went 0 -> 0, the
        // drop was zero, and the break this record exists to catch was never
        // recorded. When the fingerprint says what changed, that IS the
        // evidence; the drop is only severity.
        if token_drop < config.cache_break_min_drop {
            return None;
        }
        if elapsed > config.prompt_ttl.as_secs() {
            causes.push(cache_break_cause::TTL_EXPIRY.to_string());
            (
                false,
                format!("possible prompt cache TTL expiry after {elapsed}s"),
            )
        } else {
            causes.push(cache_break_cause::UNEXPLAINED.to_string());
            (
                true,
                "cache read tokens dropped while prompt fingerprint remained stable".to_string(),
            )
        }
    } else {
        (false, reasons.join(", "))
    };

    Some(CacheBreakEvent {
        unexpected,
        reason,
        causes,
        previous_cache_read_input_tokens: previous.cache_read_input_tokens,
        current_cache_read_input_tokens: current.cache_read_input_tokens,
        token_drop,
    })
}

fn apply_usage_to_stats(
    stats: &mut PromptCacheStats,
    usage: &Usage,
    request_hash: &str,
    source: &str,
) {
    stats.total_cache_creation_input_tokens += u64::from(usage.cache_creation_input_tokens);
    stats.total_cache_read_input_tokens += u64::from(usage.cache_read_input_tokens);
    stats.total_input_tokens += u64::from(usage.input_tokens);
    stats.input_tokens_observed_requests += 1;
    stats.last_cache_creation_input_tokens = Some(usage.cache_creation_input_tokens);
    stats.last_cache_read_input_tokens = Some(usage.cache_read_input_tokens);
    stats.last_request_hash = Some(request_hash.to_string());
    stats.last_cache_source = Some(source.to_string());
}

fn persist_state(inner: &PromptCacheInner) {
    let _ = ensure_cache_dirs(&inner.paths);
    let _ = write_json(&inner.paths.stats_path, &inner.stats);
    if let Some(previous) = &inner.previous {
        let _ = write_json(&inner.paths.session_state_path, previous);
    }
}

/// Append one line to the request ledger.
///
/// Append rather than rewrite, and one line rather than a document, because the
/// file has to survive being written by a process that is killed mid-session —
/// which is the normal way an agent run ends. A partial final line costs one
/// row; a truncated JSON document would cost the session.
///
/// Failures are ignored for the same reason the other writes here are: this is
/// observability, and losing a row must never fail the request that produced it.
fn append_request_row(paths: &PromptCachePaths, row: &PromptCacheRequestRow) {
    let _ = ensure_cache_dirs(paths);
    let Ok(mut line) = serde_json::to_vec(row) else {
        return;
    };
    line.push(b'\n');
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.requests_path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, &line));
}

fn write_completion_entry(
    paths: &PromptCachePaths,
    request_hash: &str,
    response: &MessageResponse,
) {
    let _ = ensure_cache_dirs(paths);
    let entry = CompletionCacheEntry {
        cached_at_unix_secs: now_unix_secs(),
        fingerprint_version: current_fingerprint_version(),
        response: response.clone(),
    };
    let _ = write_json(&paths.completion_entry_path(request_hash), &entry);
}

fn ensure_cache_dirs(paths: &PromptCachePaths) -> std::io::Result<()> {
    fs::create_dir_all(&paths.completion_dir)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, json)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn request_hash_hex(request: &MessageRequest) -> String {
    format!(
        "{REQUEST_FINGERPRINT_PREFIX}-{:016x}",
        hash_serializable(request)
    )
}

/// Cumulative hash after each message: element `k` covers `messages[0..=k]`.
///
/// Chaining rather than hashing each message alone so that a single
/// comparison finds the first index where two turns diverge — which is
/// exactly where the provider's cached prefix stops matching.
fn message_prefix_hashes(request: &MessageRequest) -> Vec<u64> {
    let mut acc = FNV_OFFSET_BASIS;
    request
        .messages
        .iter()
        .map(|message| {
            let json = serde_json::to_vec(message).unwrap_or_default();
            for byte in &json {
                acc ^= u64::from(*byte);
                acc = acc.wrapping_mul(FNV_PRIME);
            }
            acc
        })
        .collect()
}

/// Length of the longest shared leading run — the part of the conversation
/// both requests agree on byte for byte.
fn common_prefix_len(previous: &[u64], current: &[u64]) -> usize {
    previous
        .iter()
        .zip(current.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

fn hash_serializable<T: Serialize>(value: &T) -> u64 {
    let json = serde_json::to_vec(value).unwrap_or_default();
    stable_hash_bytes(&json)
}

fn sanitize_path_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let suffix = format!("-{:x}", hash_string(value));
    format!(
        "{}{}",
        &sanitized[..MAX_SANITIZED_LENGTH.saturating_sub(suffix.len())],
        suffix
    )
}

fn hash_string(value: &str) -> u64 {
    stable_hash_bytes(value.as_bytes())
}

/// Directory holding one subdirectory of stats per session.
///
/// Public so a command can enumerate past sessions without each caller
/// rebuilding the same path and drifting from it.
#[must_use]
pub fn cache_root() -> PathBuf {
    base_cache_root()
}

/// Resolved through [`runtime::config::default_config_home`] rather than from a
/// private copy of the same rules, because the two disagreeing is not a
/// cosmetic bug.
///
/// The copy this replaced read `SUDO_CODE_CONFIG_HOME`, then `HOME`, then fell
/// back to the system temp dir — it was the only home resolver in the workspace
/// that omitted the Windows `USERPROFILE` fallback (`runtime::config` has it,
/// the CLI's own test harness was fixed for the same omission). `HOME` is set
/// inside Git Bash and unset in PowerShell, so on Windows the same machine had
/// two stores: config loaded from `%USERPROFILE%\.nexus\sudocode` while the
/// cache was written to `%TEMP%\sudocode-prompt-cache`. That cost three things,
/// in rising order of importance:
///
/// 1. The record landed somewhere Windows disk cleanup deletes, so the
///    per-request ledger — the only instrument that can attribute a cache break
///    to a TTL expiry — was disposable.
/// 2. `stats.json` and the ledger were split across two directories, so no
///    single store described a user's actual traffic.
/// 3. `session-state.json` is how break detection remembers the *previous*
///    request's fingerprint. Resuming a session from a different launch context
///    silently found no previous state, which disables break detection and the
///    completion cache for that session — the behavior this module exists to
///    measure, broken by where it chose to write.
fn base_cache_root() -> PathBuf {
    runtime::config::default_config_home()
        .join("cache")
        .join("prompt-cache")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

const fn current_fingerprint_version() -> u32 {
    REQUEST_FINGERPRINT_VERSION
}

fn stable_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        detect_cache_break, read_json, request_hash_hex, sanitize_path_segment, PromptCache,
        PromptCacheConfig, PromptCachePaths, TrackedPromptState, REQUEST_FINGERPRINT_PREFIX,
    };
    use crate::types::{InputMessage, MessageRequest, MessageResponse, OutputContentBlock, Usage};

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    #[test]
    fn path_builder_sanitizes_session_identifier() {
        let paths = PromptCachePaths::for_session("session:/with spaces");
        let session_dir = paths
            .session_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("session dir name");
        assert_eq!(session_dir, "session--with-spaces");
        assert!(paths.completion_dir.ends_with("completions"));
        assert!(paths.stats_path.ends_with("stats.json"));
        assert!(paths.session_state_path.ends_with("session-state.json"));
    }

    /// With `HOME` unset the cache must still land next to the config, not in a
    /// temp directory.
    ///
    /// PowerShell and cmd do not set `HOME`; Git Bash does. The resolver this
    /// replaced fell through to `std::env::temp_dir()` in that case, so the same
    /// machine wrote its cache to two different places depending on which shell
    /// launched `scode` — and the PowerShell half landed where Windows disk
    /// cleanup deletes it. `session-state.json` is what break detection reads to
    /// learn the previous request's fingerprint, so a split store silently
    /// disables break detection for a resumed session.
    #[test]
    fn cache_root_follows_the_config_home_on_windows_without_home() {
        let _guard = test_env_lock();
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["SUDO_CODE_CONFIG_HOME", "HOME", "USERPROFILE"]
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect();
        let profile = std::env::temp_dir().join(format!(
            "prompt-cache-userprofile-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));

        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::remove_var("HOME");
        std::env::set_var("USERPROFILE", &profile);
        let root = PromptCachePaths::for_session("profile-session").root;

        for (key, value) in saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        assert_eq!(
            root,
            profile
                .join(".nexus")
                .join("sudocode")
                .join("cache")
                .join("prompt-cache"),
            "with HOME unset the cache root must follow USERPROFILE, exactly as \
             runtime::config::default_config_home resolves it — never the system \
             temp dir"
        );
    }

    #[test]
    fn request_fingerprint_drives_unexpected_break_detection() {
        let request = sample_request("same");
        let previous = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
                ..Usage::default()
            },
        );
        let current = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
                ..Usage::default()
            },
        );
        let event = detect_cache_break(&PromptCacheConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(event.unexpected);
        assert!(event.reason.contains("stable"));
    }

    /// Rewriting an earlier message is the expensive case: the provider's
    /// prefix stops matching at that index and everything after it is rebuilt.
    /// The reason has to name the index, otherwise it is indistinguishable
    /// from the harmless case below.
    #[test]
    fn changed_prompt_marks_break_as_expected() {
        let previous_request = sample_request("first");
        let current_request = sample_request("second");
        let previous = TrackedPromptState::from_usage(
            &previous_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
                ..Usage::default()
            },
        );
        let current = TrackedPromptState::from_usage(
            &current_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
                ..Usage::default()
            },
        );
        let event = detect_cache_break(&PromptCacheConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(!event.unexpected);
        assert!(
            event
                .reason
                .contains("message history rewritten at index 0"),
            "reason should name where the prefix diverged, got: {}",
            event.reason
        );
    }

    /// Appending a turn is what every normal request does, and it leaves the
    /// cached prefix whole. Before prefix comparison the whole-array hash
    /// always differed, so this case was reported identically to a rewrite —
    /// which made the reason field carry no information at all.
    #[test]
    fn appended_turn_is_not_reported_as_a_rewrite() {
        let previous_request = sample_request("first");
        let mut current_request = sample_request("first");
        current_request
            .messages
            .push(crate::types::InputMessage::user_text("second"));

        let previous = TrackedPromptState::from_usage(
            &previous_request,
            &Usage {
                cache_read_input_tokens: 6_000,
                ..Usage::default()
            },
        );
        let current = TrackedPromptState::from_usage(
            &current_request,
            &Usage {
                cache_read_input_tokens: 1_000,
                ..Usage::default()
            },
        );

        let event = detect_cache_break(&PromptCacheConfig::default(), Some(&previous), &current)
            .expect("a 5k drop still counts as a break");
        assert!(
            !event.reason.contains("rewritten"),
            "an append must not be reported as a rewrite, got: {}",
            event.reason
        );
    }

    /// The ledger is the only thing that can tie a session's cache history to
    /// the upstream account that served it, so the join key has to survive into
    /// the file — and the rows have to stay in order, because "the prefix went
    /// cold here" is a position in a sequence, not an aggregate.
    /// A streaming request must still get both correlation ids into its row.
    ///
    /// This is the shape of **all** real traffic, and it used to be the one
    /// shape that could not carry `provider_request_id`: the row took it from
    /// an `Option<&MessageResponse>`, and a stream has no assembled response to
    /// pass. Measured against a live gateway before the fix: 480 consecutive
    /// rows carried neither id while the SSE response itself carried
    /// `x-oneapi-request-id` the whole time. The regression cannot be expressed
    /// as a failing assertion any more -- dropping the id now means not
    /// constructing `ResponseTraceIds`, which does not compile. That is the
    /// point of the struct.
    #[test]
    fn streaming_record_keeps_both_correlation_ids() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-stream-ids-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::new("stream-ids-session");

        let _ = cache.record_usage(
            &sample_request("streamed"),
            &Usage {
                input_tokens: 3,
                cache_creation_input_tokens: 2_048,
                cache_read_input_tokens: 0,
                output_tokens: 7,
                ..Usage::default()
            },
            super::ResponseTraceIds {
                gateway: Some("client:acct-7".to_string()),
                provider: Some("202610061441274065481938268d9d6".to_string()),
            },
        );

        let path = PromptCachePaths::for_session("stream-ids-session").requests_path;
        let text = std::fs::read_to_string(&path).expect("the ledger should exist");
        let row: super::PromptCacheRequestRow = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("one row per request"))
            .next()
            .expect("the streamed request must produce a row");
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");

        assert_eq!(
            row.gateway_request_id.as_deref(),
            Some("client:acct-7"),
            "the account-selection key must survive the streaming path"
        );
        assert_eq!(
            row.provider_request_id.as_deref(),
            Some("202610061441274065481938268d9d6"),
            "so must the gateway's own request-log key -- it is the only id an              SSE response actually carries on this route"
        );
    }

    #[test]
    fn request_ledger_records_the_gateway_id_in_order() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-ledger-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::new("ledger-session");

        // Two turns on the same gateway, then one where the gateway said
        // nothing — the last is what talking straight to a provider looks like,
        // and it must still produce a row rather than be dropped.
        for (text, gateway) in [
            ("first", Some("client:aaa")),
            ("second", Some("client:bbb")),
            ("third", None),
        ] {
            let mut response = sample_response(100, 5, "ok");
            response.gateway_request_id = gateway.map(ToOwned::to_owned);
            let _ = cache.record_response(&sample_request(text), &response);
        }

        let path = PromptCachePaths::for_session("ledger-session").requests_path;
        let text = std::fs::read_to_string(&path).expect("the ledger should exist");
        let rows: Vec<super::PromptCacheRequestRow> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("each line is one row"))
            .collect();

        assert_eq!(rows.len(), 3, "one row per tracked request");
        assert_eq!(
            rows.iter()
                .map(|row| row.gateway_request_id.clone())
                .collect::<Vec<_>>(),
            vec![
                Some("client:aaa".to_string()),
                Some("client:bbb".to_string()),
                None
            ],
            "the join key must reach the file, in request order, and an absent \
             gateway must not drop the row"
        );
        assert!(
            rows.iter()
                .all(|row| row.cache_read_input_tokens == 100 && row.at_unix_secs > 0),
            "each row carries the usage and a timestamp: {rows:?}"
        );

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn completion_cache_round_trip_persists_recent_response() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::new("unit-test-session");
        let request = sample_request("cache me");
        let response = sample_response(42, 12, "cached");

        assert!(cache.lookup_completion(&request).is_none());
        let record = cache.record_response(&request, &response);
        assert!(record.cache_break.is_none());

        let cached = cache
            .lookup_completion(&request)
            .expect("cached response should load");
        assert_eq!(cached.content, response.content);

        let stats = cache.stats();
        assert_eq!(stats.completion_cache_hits, 1);
        assert_eq!(stats.completion_cache_misses, 1);
        assert_eq!(stats.completion_cache_writes, 1);

        let persisted = read_json::<super::PromptCacheStats>(&cache.paths().stats_path)
            .expect("stats should persist");
        assert_eq!(persisted.completion_cache_hits, 1);

        let _ = std::fs::remove_dir_all(temp_root);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
    }

    #[test]
    fn distinct_requests_do_not_collide_in_completion_cache() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-distinct-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::new("distinct-request-session");
        let first_request = sample_request("first");
        let second_request = sample_request("second");

        let response = sample_response(42, 12, "cached");
        let _ = cache.record_response(&first_request, &response);

        assert!(cache.lookup_completion(&second_request).is_none());

        let _ = std::fs::remove_dir_all(temp_root);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
    }

    #[test]
    fn expired_completion_entries_are_not_reused() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-expired-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::with_config(PromptCacheConfig {
            session_id: "expired-session".to_string(),
            completion_ttl: Duration::ZERO,
            ..PromptCacheConfig::default()
        });
        let request = sample_request("expire me");
        let response = sample_response(7, 3, "stale");

        let _ = cache.record_response(&request, &response);

        assert!(cache.lookup_completion(&request).is_none());
        let stats = cache.stats();
        assert_eq!(stats.completion_cache_hits, 0);
        assert_eq!(stats.completion_cache_misses, 1);

        let _ = std::fs::remove_dir_all(temp_root);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
    }

    #[test]
    fn sanitize_path_caps_long_values() {
        let long_value = "x".repeat(200);
        let sanitized = sanitize_path_segment(&long_value);
        assert!(sanitized.len() <= 80);
    }

    #[test]
    fn request_hashes_are_versioned_and_stable() {
        let request = sample_request("stable");
        let first = request_hash_hex(&request);
        let second = request_hash_hex(&request);
        assert_eq!(first, second);
        assert!(first.starts_with(REQUEST_FINGERPRINT_PREFIX));
    }

    fn sample_request(text: &str) -> MessageRequest {
        MessageRequest {
            model: "claude-3-7-sonnet-latest".to_string(),
            max_tokens: 64,
            messages: vec![InputMessage::user_text(text)],
            system: Some("system".to_string()),
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        }
    }

    fn sample_response(
        cache_read_input_tokens: u32,
        output_tokens: u32,
        text: &str,
    ) -> MessageResponse {
        MessageResponse {
            id: "msg_test".to_string(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![OutputContentBlock::Text {
                text: text.to_string(),
            }],
            model: "claude-3-7-sonnet-latest".to_string(),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 10,
                cache_creation_input_tokens: 5,
                cache_read_input_tokens,
                output_tokens,
                ..Usage::default()
            },
            request_id: Some("req_test".to_string()),
            gateway_request_id: None,
        }
    }
}
