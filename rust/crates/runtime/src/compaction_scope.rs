//! One budget and receipt ledger for every provider request in a compaction.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::session::MaintenanceUsageReceipt;
use crate::usage::TokenUsage;

pub const MAX_REQUEST_ATTEMPTS: u32 = 4;
pub const MAX_COMPLETED_RESPONSES: u32 = 2;
pub const MAX_OUTER_RETRIES: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionScopeSnapshot {
    pub attempts: u32,
    pub completed_responses: u32,
    pub outer_retries: u32,
    pub receipts: Vec<MaintenanceUsageReceipt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBudgetExceeded {
    RequestAttempts,
    CompletedResponses,
}

impl Display for CompactionBudgetExceeded {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestAttempts => f.write_str("compaction request attempt budget exhausted"),
            Self::CompletedResponses => {
                f.write_str("compaction completed response budget exhausted")
            }
        }
    }
}

impl std::error::Error for CompactionBudgetExceeded {}

#[derive(Debug, Default)]
struct ScopeState {
    run_id: String,
    snapshot: CompactionScopeSnapshot,
    completed_attempts: BTreeSet<u32>,
}

#[derive(Debug, Clone)]
pub struct CompactionRequestScope(Arc<Mutex<ScopeState>>);

tokio::task_local! {
    static ACTIVE_SCOPE: CompactionRequestScope;
}

impl CompactionRequestScope {
    #[must_use]
    pub fn new(run_id: impl Into<String>) -> Self {
        Self(Arc::new(Mutex::new(ScopeState {
            run_id: run_id.into(),
            ..ScopeState::default()
        })))
    }

    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        ACTIVE_SCOPE.scope(self.clone(), future).await
    }

    #[must_use]
    pub fn snapshot(&self) -> CompactionScopeSnapshot {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    /// Used by legacy clients which return a completion without a real HTTP
    /// attempt. Their completed outputs consume quota, but never fabricate a
    /// request or a provider usage receipt.
    pub fn record_completion(
        &self,
        usage: Option<TokenUsage>,
    ) -> Result<(), CompactionBudgetExceeded> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(usage) = usage {
            update_usage(&mut state, usage);
        }
        complete_response(&mut state)
    }
}

#[must_use]
pub fn is_active() -> bool {
    ACTIVE_SCOPE.try_with(|_| ()).is_ok()
}

/// Reserve immediately before a model HTTP send, after local preflight. A
/// failed reservation does not count as a request attempt.
pub fn reserve_request_attempt() -> Result<Option<u32>, CompactionBudgetExceeded> {
    ACTIVE_SCOPE
        .try_with(|scope| {
            let mut state = scope
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.snapshot.completed_responses >= MAX_COMPLETED_RESPONSES {
                return Err(CompactionBudgetExceeded::CompletedResponses);
            }
            if state.snapshot.attempts >= MAX_REQUEST_ATTEMPTS {
                return Err(CompactionBudgetExceeded::RequestAttempts);
            }
            state.snapshot.attempts += 1;
            let attempt_id = state.snapshot.attempts;
            let run_id = state.run_id.clone();
            state.snapshot.receipts.push(MaintenanceUsageReceipt {
                run_id,
                attempt_id,
                usage: None,
            });
            Ok(Some(attempt_id))
        })
        .unwrap_or(Ok(None))
}

pub fn record_usage(usage: TokenUsage) {
    let _ = ACTIVE_SCOPE.try_with(|scope| {
        let mut state = scope
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update_usage(&mut state, usage);
    });
}

pub fn record_completed_response() -> Result<(), CompactionBudgetExceeded> {
    ACTIVE_SCOPE
        .try_with(|scope| {
            let mut state = scope
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            complete_response(&mut state)
        })
        .unwrap_or(Ok(()))
}

#[must_use]
pub fn claim_outer_retry() -> bool {
    ACTIVE_SCOPE
        .try_with(|scope| {
            let mut state = scope
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.snapshot.outer_retries >= MAX_OUTER_RETRIES
                || state.snapshot.attempts >= MAX_REQUEST_ATTEMPTS
                || state.snapshot.completed_responses >= MAX_COMPLETED_RESPONSES
            {
                return false;
            }
            state.snapshot.outer_retries += 1;
            true
        })
        .unwrap_or(true)
}

fn complete_response(state: &mut ScopeState) -> Result<(), CompactionBudgetExceeded> {
    let attempt = state.snapshot.attempts;
    if attempt > 0 && state.completed_attempts.contains(&attempt) {
        return Ok(());
    }
    if state.snapshot.completed_responses >= MAX_COMPLETED_RESPONSES {
        return Err(CompactionBudgetExceeded::CompletedResponses);
    }
    state.snapshot.completed_responses += 1;
    if attempt > 0 {
        state.completed_attempts.insert(attempt);
    }
    Ok(())
}

fn update_usage(state: &mut ScopeState, usage: TokenUsage) {
    let Some(receipt) = state.snapshot.receipts.last_mut() else {
        return;
    };
    let Some(existing) = receipt.usage.as_mut() else {
        receipt.usage = Some(usage);
        return;
    };
    // Provider frames carry cumulative counters. Merge partial receipts from
    // message_start, terminal deltas and Drop without billing them twice.
    existing.input_tokens = existing.input_tokens.max(usage.input_tokens);
    existing.output_tokens = existing.output_tokens.max(usage.output_tokens);
    existing.cache_creation_input_tokens = existing
        .cache_creation_input_tokens
        .max(usage.cache_creation_input_tokens);
    existing.cache_read_input_tokens = existing
        .cache_read_input_tokens
        .max(usage.cache_read_input_tokens);
    if let Some(units) = usage.cost_units {
        // A parser can observe a newer frame before returning a batch error;
        // Drop may then replay its older state. Never roll cumulative cost back.
        existing.cost_units = Some(existing.cost_units.unwrap_or_default().max(units));
        if usage.cost_currency.is_some() {
            existing.cost_currency = usage.cost_currency;
        }
    }
}
