//! Local diagnostics for background work which already owns a retry policy.
//!
//! This observes attempts; it never schedules, retries, acknowledges or changes
//! their results. Share one instance across a service's workers so one outage
//! does not become one terminal warning per worker.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::{AnalyticsEvent, SudoclawLogSink, TelemetryEvent};
use serde_json::{Map, Value};

const REMINDER_INTERVAL: Duration = Duration::from_mins(1);
const RECOVERY_QUIET_PERIOD: Duration = Duration::from_secs(2);

/// Presentation belongs to the service's host; aggregation and local logging
/// use the same policy regardless of whether that host owns terminal output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryNotices {
    Terminal,
    LogOnly,
}

/// An unrecovered outage: when it began, and each scope still failing with its
/// latest detail. Empty scopes mean all have succeeded but recovery is not yet
/// confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outage {
    pub since: Instant,
    pub scopes: Vec<(String, String)>,
}

/// Aggregated local file records with optional, rate-limited terminal notices.
///
/// A scope identifies one independently failing operation, such as a receiver
/// seat or a conversation. Success clears only that scope. Recovery requires
/// every observed failure to clear and a subsequent healthy check after a short
/// quiet period; one healthy worker cannot declare its siblings recovered.
pub struct RetryDiagnostics {
    notices: RetryNotices,
    component: &'static str,
    subject: String,
    pending: &'static str,
    recovered: &'static str,
    state: Mutex<State>,
    log: OnceLock<Result<SudoclawLogSink, String>>,
}

#[derive(Default)]
struct State {
    failures: HashMap<String, Failure>,
    active: bool,
    /// When the current outage began: set as `active` turns on, cleared as it
    /// turns off, so a change of scope or error text never moves it.
    since: Option<Instant>,
    last_failure: Option<Instant>,
    last_notice: Option<Instant>,
    log_error: Option<String>,
}

struct Failure {
    detail: String,
    context: Map<String, Value>,
    last_log: Instant,
    suppressed: u64,
}

impl RetryDiagnostics {
    /// No file is opened until a real failure is observed. This is a local log,
    /// not remote telemetry or a model/session-history event.
    #[inline]
    #[must_use]
    pub fn new(
        component: &'static str,
        subject: impl Into<String>,
        pending: &'static str,
        recovered: &'static str,
    ) -> Self {
        Self {
            notices: RetryNotices::Terminal,
            component,
            subject: subject.into(),
            pending,
            recovered,
            state: Mutex::default(),
            log: OnceLock::new(),
        }
    }

    /// A service with structured status can retain all local records without
    /// writing into a terminal it does not own. The caller still owns its
    /// operation errors, including when the diagnostic file cannot be opened.
    #[inline]
    #[must_use]
    pub fn with_notices(mut self, notices: RetryNotices) -> Self {
        self.notices = notices;
        self
    }

    /// Record an already-failed attempt; the caller retains the actual error.
    #[inline]
    pub fn failed(&self, scope: &str, detail: &str) {
        self.failed_with(scope, detail, Map::new());
    }

    /// Domain facts for the same observation, nested so they cannot replace
    /// the reporter's subject, process or error. Unknown facts stay absent/null.
    #[inline]
    pub fn failed_with(&self, scope: &str, detail: &str, context: Map<String, Value>) {
        self.update(scope, Some((detail, context)));
    }

    /// Observe a successful check of this scope, not another worker's health.
    #[inline]
    pub fn succeeded(&self, scope: &str) {
        self.update(scope, None);
    }

    /// The outage this instance has not yet seen recover, by the same rule its
    /// recovery notice uses: it lasts until every failed scope has succeeded
    /// again and a later check follows the quiet period. `None` while healthy.
    #[must_use]
    pub fn outage(&self) -> Option<Outage> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let since = state.since.filter(|_| state.active)?;
        let mut scopes: Vec<_> = state
            .failures
            .iter()
            .map(|(scope, failure)| (scope.clone(), failure.detail.clone()))
            .collect();
        scopes.sort();
        Some(Outage { since, scopes })
    }

    /// The caller's existing diagnostic label (also used for worker names).
    #[inline]
    #[must_use]
    pub fn component(&self) -> &'static str {
        self.component
    }

    fn update(&self, scope: &str, error: Option<(&str, Map<String, Value>)>) {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((error, context)) = error {
            if !state.active {
                state.since = Some(now);
            }
            state.active = true;
            state.last_failure = Some(now);
            let suppressed = if let Some(failure) = state.failures.get_mut(scope) {
                if failure.detail == error && failure.context == context {
                    if now.duration_since(failure.last_log) < REMINDER_INTERVAL {
                        failure.suppressed = failure.suppressed.saturating_add(1);
                        None
                    } else {
                        let count = std::mem::take(&mut failure.suppressed);
                        failure.last_log = now;
                        Some(count)
                    }
                } else {
                    if failure.suppressed > 0 {
                        let _ = self.record(
                            "background_retry_repeats",
                            scope,
                            &failure.detail,
                            failure.suppressed,
                            &failure.context,
                        );
                    }
                    *failure = Failure {
                        detail: error.into(),
                        context: context.clone(),
                        last_log: now,
                        suppressed: 0,
                    };
                    Some(0)
                }
            } else {
                state.failures.insert(
                    scope.into(),
                    Failure {
                        detail: error.into(),
                        context: context.clone(),
                        last_log: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            };
            if let Some(suppressed) = suppressed {
                state.log_error = self
                    .record("background_retry", scope, error, suppressed, &context)
                    .err();
            }
            if state
                .last_notice
                .is_none_or(|last| now.duration_since(last) >= REMINDER_INTERVAL)
            {
                match (self.log(), state.log_error.as_ref()) {
                    (Ok(log), None) => self.notice(
                        scope,
                        format_args!("{} Details: {}", self.pending, log.path().display()),
                    ),
                    // Do not hide the original cause if its log cannot open.
                    (Err(reason), _) | (_, Some(reason)) => self.notice(
                        scope,
                        format_args!(
                            "{} Diagnostic log unavailable ({reason}); {scope}: {error}",
                            self.pending
                        ),
                    ),
                }
                state.last_notice = Some(now);
            }
        } else if state.active {
            if let Some(failure) = state.failures.remove(scope) {
                state.log_error = self
                    .record(
                        "background_retry_scope_recovered",
                        scope,
                        &failure.detail,
                        failure.suppressed,
                        &failure.context,
                    )
                    .err();
            }
            if state.failures.is_empty()
                && state
                    .last_failure
                    .is_some_and(|last| now.duration_since(last) >= RECOVERY_QUIET_PERIOD)
            {
                self.notice(scope, format_args!("{}", self.recovered));
                state.active = false;
                state.since = None;
                state.last_notice = None;
            }
        }
    }

    /// The local record precedes its terminal notice. A closed pipe, terminal
    /// handoff or unwritable stderr must not panic an observing service. Do not
    /// retry a partial terminal write: it may already have displayed a prefix.
    #[inline]
    fn notice(&self, scope: &str, text: std::fmt::Arguments<'_>) {
        if self.notices == RetryNotices::LogOnly {
            return;
        }
        if let Err(error) = writeln!(std::io::stderr().lock(), "[{}] {text}", self.component) {
            let context = Map::from_iter([("kind".into(), format!("{:?}", error.kind()).into())]);
            // This is the existing fallible file path, never another terminal
            // write or a recursive retry. Retain the terminal's actual error
            // separately from the original operation's already-saved cause.
            let _ = self.record(
                "background_retry_terminal_unavailable",
                scope,
                &error.to_string(),
                0,
                &context,
            );
        }
    }

    #[inline]
    fn log(&self) -> &Result<SudoclawLogSink, String> {
        self.log
            .get_or_init(|| SudoclawLogSink::new().map_err(|error| error.to_string()))
    }

    fn record(
        &self,
        action: &str,
        scope: &str,
        error: &str,
        suppressed: u64,
        context: &Map<String, Value>,
    ) -> Result<(), String> {
        match self.log() {
            Ok(log) => log
                .try_record(&TelemetryEvent::Analytics(
                    AnalyticsEvent::new("background_retry", action)
                        .with_property("component", self.component.into())
                        .with_property("subject", self.subject.as_str().into())
                        .with_property("pid", std::process::id().into())
                        .with_property("scope", scope.into())
                        .with_property("error", error.into())
                        .with_property("suppressed_repeats", suppressed.into())
                        .with_property("context", context.clone().into()),
                ))
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.clone()),
        }
    }
}
