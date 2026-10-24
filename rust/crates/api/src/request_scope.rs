//! One request attempt budget shared by HTTP retries and safe stream replay.
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use telemetry::SessionTracer;

use crate::error::ApiError;
use crate::http_transport::{RetryNotifier, RetryPolicy};

tokio::task_local! {
    static CURRENT: Option<RequestScope>;
}

#[derive(Clone, Default)]
pub(crate) struct RequestScope(Arc<ScopeState>);

#[derive(Default)]
struct ScopeState {
    policy: OnceLock<RetryPolicy>,
    attempts: AtomicU32,
    transport: Mutex<Option<TransportState>>,
}

#[derive(Clone)]
pub(crate) struct TransportState {
    pub request_id: String,
    pub path: String,
    pub started_ms: u64,
    pub idle_timeout: Option<Duration>,
    pub tracer: Option<SessionTracer>,
    pub notifier: Option<Arc<dyn RetryNotifier>>,
}

impl RequestScope {
    pub fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok().flatten()
    }

    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT.scope(Some(self.clone()), future).await
    }

    /// Preflight requests have their own policy and must not initialize or
    /// consume a model stream's retry budget. Parent compaction scope remains.
    pub async fn preflight<F: Future>(future: F) -> F::Output {
        CURRENT.scope(None, future).await
    }

    pub fn begin_attempt(
        &self,
        policy: RetryPolicy,
        state: TransportState,
    ) -> Result<u32, ApiError> {
        // Compaction supplies zero retries; its orchestrator owns the parent budget.
        let policy = self.0.policy.get_or_init(|| policy);
        let limit = policy.max_retries.saturating_add(1);
        let attempt = self
            .0
            .attempts
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                (used < limit).then(|| used + 1)
            })
            .map(|used| used + 1)
            .map_err(|attempts| ApiError::RequestAttemptsExhausted { attempts })?;
        *self.0.transport.lock().unwrap() = Some(state);
        Ok(attempt)
    }

    pub fn has_attempts_left(&self) -> bool {
        self.0
            .policy
            .get()
            .is_some_and(|p| self.attempts() <= p.max_retries)
    }

    pub fn attempts(&self) -> u32 {
        self.0.attempts.load(Ordering::Relaxed)
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.0
            .transport
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|state| state.idle_timeout)
    }

    pub fn record_stream_failure(&self, error: &ApiError) {
        let state = self.0.transport.lock().unwrap().clone();
        if let Some(state) = state {
            if let Some(tracer) = state.tracer {
                tracer.record_http_request_failed(
                    telemetry::HttpRequestAttempt {
                        request_id: &state.request_id,
                        attempt: self.attempts(),
                        method: "POST",
                        path: &state.path,
                        start_timestamp_ms: state.started_ms,
                    },
                    error.to_string(),
                    error.is_retryable(),
                    serde_json::Map::new(),
                );
            }
        }
    }

    pub async fn wait_before_replay(&self, error: &ApiError) -> Result<(), ApiError> {
        let policy = self
            .0
            .policy
            .get()
            .expect("HTTP attempt initialized request scope");
        let attempt = self.attempts();
        let delay = policy.delay_for_attempt(attempt, error.retry_after())?;
        let state = self.0.transport.lock().unwrap().clone();
        let notifier = state.and_then(|state| state.notifier);
        if let Some(notifier) = &notifier {
            notifier.on_retry(attempt, policy.max_retries, &error.to_string());
        }
        tokio::time::sleep(delay).await;
        if let Some(notifier) = notifier {
            notifier.on_retry_end();
        }
        Ok(())
    }
}
