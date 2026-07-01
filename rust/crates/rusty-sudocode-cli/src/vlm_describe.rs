//! VLM-route helper: turn a base64 image into a short text description by
//! POSTing it to an OpenAI-compatible `/chat/completions` endpoint.
//!
//! This is the side-call used by `push_images` when:
//!   1. `image_registry::preflight_base64` returns `ImageTooLargeError` even
//!      after the JPEG-quality loop — historically replaced by a static text
//!      placeholder; now replaced by a real description of the image.
//!   2. The active chat model isn't vision-capable but the user attached an
//!      image — historically a wrong-model error; now transparent VLM-route.
//!
//! Architecture: HTTP lives in this file (cli crate), the runtime crate stays
//! free of network calls. The function is async; `push_images` runs it via a
//! one-off blocking tokio runtime since the SdkAcpDelegate trait is sync.
//!
//! Design rationale: `docs/design/image-handling-non-user-facing.html`
//! (Decision 2 + the "VLM model selection" section).
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

/// Default vision model used when no per-conversation override is configured.
/// Cheap, fast, strong on vision (Gemini Flash family). Sudocode reaches it
/// through sudorouter, so the same `proxy.sudorouter` creds work.
///
/// TODO(coordination/sudorouter): once sudorouter exposes per-model image cap
/// fields in `/v1/models`, prefer the active model when it's itself vision-
/// capable (cheaper one-round-trip vs swapping models).
pub const DEFAULT_VISION_MODEL: &str = "gemini-2.5-flash";

/// Prompt template for the describe call. Concise on purpose: the description
/// becomes input tokens for the active model, so verbosity costs the user.
pub const DESCRIBE_PROMPT: &str = "Describe this image concisely in 1-3 sentences. \
    Focus on: any visible text, key UI elements or objects, and anything the \
    user is likely asking about.";

const VLM_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Error variants emitted by [`describe_image_via_vlm`].
#[derive(Debug)]
pub enum VlmError {
    /// Failed to build the HTTP client or sudorouter URL is malformed.
    Client(String),
    /// Network-level failure (DNS, TLS, connect, timeout).
    Network(String),
    /// API returned non-2xx; carries status code and (truncated) body.
    BadStatus { status: u16, body: String },
    /// API returned 2xx but the JSON shape was missing the expected
    /// `choices[0].message.content` text.
    EmptyResponse,
}

impl std::fmt::Display for VlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(s) => write!(f, "vlm client error: {s}"),
            Self::Network(s) => write!(f, "vlm network error: {s}"),
            Self::BadStatus { status, body } => {
                write!(f, "vlm api {status}: {}", truncate(body, 200))
            }
            Self::EmptyResponse => write!(f, "vlm api returned no description"),
        }
    }
}

impl std::error::Error for VlmError {}

/// Bounded LRU of (image-sha256 → description). Saves a VLM round-trip when
/// the same image bytes reappear later in the conversation (common when the
/// user re-references an attached file, or the agent re-screenshots the same
/// page via ai-dev-browser). 64-entry cap keeps memory bounded; the eldest
/// entry is evicted on overflow. Hit-rate is best-effort — failure to cache
/// is never user-visible.
const VLM_CACHE_CAPACITY: usize = 64;

static VLM_DESCRIPTION_CACHE: LazyLock<Mutex<VlmDescriptionCache>> =
    LazyLock::new(|| Mutex::new(VlmDescriptionCache::new(VLM_CACHE_CAPACITY)));

struct VlmDescriptionCache {
    capacity: usize,
    entries: HashMap<String, String>,
    order: VecDeque<String>,
}

impl VlmDescriptionCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
        }
    }

    fn get(&mut self, key: &str) -> Option<String> {
        if let Some(v) = self.entries.get(key).cloned() {
            // Move-to-front for true LRU semantics.
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(key.to_string());
            Some(v)
        } else {
            None
        }
    }

    fn put(&mut self, key: String, value: String) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), value);
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
            self.order.push_back(key);
            return;
        }
        if self.entries.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, value);
    }
}

fn cache_key(model: &str, image_b64: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update(b"\0");
    hasher.update(image_b64.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Issue a one-shot `/chat/completions` request with the image inline as a
/// `image_url` content part (OpenAI-compatible shape — works with sudorouter
/// proxy regardless of the upstream provider).
///
/// `base_url` should be the sudorouter base (e.g. `https://hk.sudorouter.ai/v1`)
/// without trailing slash; trailing slashes are stripped defensively.
///
/// Returns the model's textual reply, or a typed [`VlmError`] the caller can
/// log + recover from (the caller should always degrade gracefully — failing
/// to describe an image must not abort the user's turn).
///
/// Reads/writes [`VLM_DESCRIPTION_CACHE`] keyed by `sha256(model || image_b64)`
/// — re-describing the same image bytes in the same conversation never makes
/// a second HTTP round-trip.
pub async fn describe_image_via_vlm(
    base_url: &str,
    api_key: &str,
    model: &str,
    image_b64: &str,
    mime_type: &str,
) -> Result<String, VlmError> {
    // Cache hit short-circuits everything (no client build, no HTTP).
    let key = cache_key(model, image_b64);
    if let Ok(mut cache) = VLM_DESCRIPTION_CACHE.lock() {
        if let Some(cached) = cache.get(&key) {
            return Ok(cached);
        }
    }

    let client = reqwest::Client::builder()
        .timeout(VLM_HTTP_TIMEOUT)
        .build()
        .map_err(|e| VlmError::Client(e.to_string()))?;

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let data_url = format!("data:{mime_type};base64,{image_b64}");

    let payload = json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": DESCRIBE_PROMPT },
                { "type": "image_url", "image_url": { "url": data_url } }
            ]
        }],
        "max_tokens": 256,
        "temperature": 0.2,
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| VlmError::Network(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(VlmError::BadStatus { status, body });
    }

    let parsed: ChatCompletionResponse = resp
        .json()
        .await
        .map_err(|e| VlmError::Network(format!("json parse: {e}")))?;

    let description = parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .filter(|s| !s.trim().is_empty())
        .ok_or(VlmError::EmptyResponse)?;

    // Best-effort cache insert; a poisoned mutex is logged and skipped, never
    // surfaced to the caller (the description itself is what matters).
    if let Ok(mut cache) = VLM_DESCRIPTION_CACHE.lock() {
        cache.put(key, description.clone());
    }
    Ok(description)
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    content: String,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…(truncated)", &s[..max])
    }
}
