//! Unified HTTP transport layer — owns retry loop, observability, and lifecycle logging.
//!
//! All providers prepare `(url, headers, body)` and hand them to `HttpTransport::send_json`.
//! Provider-specific error parsing is injected via the `check_response` callback.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use telemetry::{AnalyticsEvent, SessionTracer};

use crate::error::ApiError;
use crate::http_client::build_http_client_or_default;

const REQUEST_ID_HEADER: &str = "request-id";
const ALT_REQUEST_ID_HEADER: &str = "x-request-id";

// ---------------------------------------------------------------------------
// RetryPolicy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl RetryPolicy {
    pub const DEFAULT: Self = Self {
        max_retries: 8,
        initial_backoff: Duration::from_secs(1),
        max_backoff: Duration::from_secs(128),
    };

    fn backoff_for_attempt(&self, attempt: u32) -> Result<Duration, ApiError> {
        let Some(multiplier) = 1_u32.checked_shl(attempt.saturating_sub(1)) else {
            return Err(ApiError::BackoffOverflow {
                attempt,
                base_delay: self.initial_backoff,
            });
        };
        Ok(self
            .initial_backoff
            .checked_mul(multiplier)
            .map_or(self.max_backoff, |delay| delay.min(self.max_backoff)))
    }

    fn jittered_backoff_for_attempt(&self, attempt: u32) -> Result<Duration, ApiError> {
        let base = self.backoff_for_attempt(attempt)?;
        Ok(base + jitter_for_base(base))
    }
}

// ---------------------------------------------------------------------------
// HttpTransport
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpTransport {
    inner: reqwest::Client,
    session_tracer: Option<SessionTracer>,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: build_http_client_or_default(),
            session_tracer: None,
        }
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    pub fn set_session_tracer(&mut self, session_tracer: SessionTracer) {
        self.session_tracer = Some(session_tracer);
    }

    #[must_use]
    pub fn session_tracer(&self) -> Option<&SessionTracer> {
        self.session_tracer.as_ref()
    }

    /// Direct access to the inner `reqwest::Client` for OAuth token refresh
    /// and other operations that don't go through the standard retry path.
    #[must_use]
    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }

    pub fn record_analytics(&self, event: AnalyticsEvent) {
        if let Some(tracer) = &self.session_tracer {
            tracer.record_analytics(event);
        }
    }

    /// Send a JSON request with retry + full lifecycle logging.
    ///
    /// `check_response` is provider-specific error parsing (non-2xx → `ApiError`).
    /// The transport uses `error.is_retryable()` to decide whether to retry.
    pub async fn send_json<F, Fut>(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
        retry_policy: &RetryPolicy,
        check_response: F,
    ) -> Result<reqwest::Response, ApiError>
    where
        F: Fn(reqwest::Response) -> Fut,
        Fut: Future<Output = Result<reqwest::Response, ApiError>>,
    {
        let path = extract_path(url);
        let mut attempts = 0u32;
        let mut last_error: Option<ApiError>;

        loop {
            attempts += 1;

            // Log started.
            if let Some(tracer) = &self.session_tracer {
                tracer.record_http_request_started(attempts, "POST", &path, Map::new());
            }

            // Log debug (every attempt — matches existing Anthropic behavior).
            if let Some(tracer) = &self.session_tracer {
                let masked = telemetry::mask_sensitive_headers(headers);
                tracer.record_http_request_debug(url, "POST", masked, body.clone());
            }

            // Build and send request.
            let send_result = {
                let mut builder = self.inner.post(url);
                for (name, value) in headers {
                    builder = builder.header(name.as_str(), value.as_str());
                }
                builder.json(body).send().await.map_err(ApiError::from)
            };

            match send_result {
                Ok(response) => match check_response(response).await {
                    Ok(response) => {
                        if let Some(tracer) = &self.session_tracer {
                            tracer.record_http_request_succeeded(
                                attempts,
                                "POST",
                                &path,
                                response.status().as_u16(),
                                request_id_from_headers(response.headers()),
                                Map::new(),
                            );
                        }
                        return Ok(response);
                    }
                    Err(error)
                        if error.is_retryable() && attempts <= retry_policy.max_retries + 1 =>
                    {
                        self.record_failure(attempts, &path, &error);
                        last_error = Some(error);
                    }
                    Err(error) => {
                        self.record_failure(attempts, &path, &error);
                        return Err(error);
                    }
                },
                Err(error) if error.is_retryable() && attempts <= retry_policy.max_retries + 1 => {
                    self.record_failure(attempts, &path, &error);
                    last_error = Some(error);
                }
                Err(error) => {
                    self.record_failure(attempts, &path, &error);
                    return Err(error);
                }
            }

            if attempts > retry_policy.max_retries {
                break;
            }

            tokio::time::sleep(retry_policy.jittered_backoff_for_attempt(attempts)?).await;
        }

        Err(ApiError::RetriesExhausted {
            attempts,
            last_error: Box::new(last_error.expect("retry loop must capture an error")),
        })
    }

    fn record_failure(&self, attempt: u32, path: &str, error: &ApiError) {
        if let Some(tracer) = &self.session_tracer {
            tracer.record_http_request_failed(
                attempt,
                "POST",
                path,
                error.to_string(),
                error.is_retryable(),
                Map::new(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Extract the URL path for logging
/// (e.g., `"https://api.anthropic.com/v1/messages"` → `"/v1/messages"`).
fn extract_path(url: &str) -> String {
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        if let Some(slash_pos) = rest.find('/') {
            return rest[slash_pos..].to_string();
        }
    }
    url.to_string()
}

/// Extract request ID from response headers (`request-id` or `x-request-id`).
pub fn request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| headers.get(ALT_REQUEST_ID_HEADER))
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

// ---------------------------------------------------------------------------
// Jitter — extracted from duplicated code in anthropic.rs & openai_compat.rs
// ---------------------------------------------------------------------------

/// Process-wide counter that guarantees distinct jitter samples even when
/// the system clock resolution is coarser than consecutive retry sleeps.
static JITTER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns a random additive jitter in `[0, base]` to decorrelate retries
/// from multiple concurrent clients. Entropy is drawn from the nanosecond
/// wall clock mixed with a monotonic counter and run through a splitmix64
/// finalizer; adequate for retry jitter (no cryptographic requirement).
fn jitter_for_base(base: Duration) -> Duration {
    let base_nanos = u64::try_from(base.as_nanos()).unwrap_or(u64::MAX);
    if base_nanos == 0 {
        return Duration::ZERO;
    }
    let raw_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let tick = JITTER_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut mixed = raw_nanos
        .wrapping_add(tick)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^= mixed >> 31;
    let jitter_nanos = mixed % base_nanos.saturating_add(1);
    Duration::from_nanos(jitter_nanos)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_default_has_expected_values() {
        let policy = RetryPolicy::DEFAULT;
        assert_eq!(policy.max_retries, 8);
        assert_eq!(policy.initial_backoff, Duration::from_secs(1));
        assert_eq!(policy.max_backoff, Duration::from_secs(128));
    }

    #[test]
    fn backoff_doubles_each_attempt_up_to_max() {
        let policy = RetryPolicy {
            max_retries: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(128),
        };
        assert_eq!(
            policy.backoff_for_attempt(1).unwrap(),
            Duration::from_secs(1)
        );
        assert_eq!(
            policy.backoff_for_attempt(2).unwrap(),
            Duration::from_secs(2)
        );
        assert_eq!(
            policy.backoff_for_attempt(3).unwrap(),
            Duration::from_secs(4)
        );
        assert_eq!(
            policy.backoff_for_attempt(8).unwrap(),
            Duration::from_secs(128)
        );
        // Attempt 9 would be 256s but is clamped to max.
        assert_eq!(
            policy.backoff_for_attempt(9).unwrap(),
            Duration::from_secs(128)
        );
    }

    #[test]
    fn jittered_backoff_is_bounded() {
        let policy = RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
        };
        for attempt in 1..=3 {
            let base = policy.backoff_for_attempt(attempt).unwrap();
            let jittered = policy.jittered_backoff_for_attempt(attempt).unwrap();
            // jitter in [0, base], so jittered in [base, 2*base].
            assert!(jittered >= base, "jittered must be >= base");
            assert!(jittered <= base * 2, "jittered must be <= 2*base");
        }
    }

    #[test]
    fn extract_path_from_https_url() {
        assert_eq!(
            extract_path("https://api.anthropic.com/v1/messages"),
            "/v1/messages"
        );
    }

    #[test]
    fn extract_path_from_http_url() {
        assert_eq!(
            extract_path("http://127.0.0.1:8080/v1/chat/completions"),
            "/v1/chat/completions"
        );
    }

    #[test]
    fn extract_path_without_scheme_returns_input() {
        assert_eq!(extract_path("/v1/messages"), "/v1/messages");
    }
}
