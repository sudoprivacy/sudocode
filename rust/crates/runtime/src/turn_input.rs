//! Turn-local steering ownership, independent of the session's execution lock.
//!
//! Delivery never implies cancellation. The engine admits messages here; only
//! the conversation loop commits them, between complete assistant/tool exchanges.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    #[default]
    Steer,
    Queue,
}

impl InputDelivery {
    #[must_use]
    pub const fn is_steer(&self) -> bool {
        matches!(self, Self::Steer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    Human,
    Peer,
    Subagent,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput {
    pub id: u64,
    pub text: String,
    pub display: String,
    pub source: InputSource,
    /// Native image blocks travel with the input through queueing and steering.
    pub attachments: Vec<crate::ContentBlock>,
}

/// Identity and display projection; expanded prompt content stays with its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputView {
    pub id: u64,
    pub display: String,
    pub source: InputSource,
}

impl TurnInput {
    #[must_use]
    #[inline]
    pub fn view(&self) -> InputView {
        InputView {
            id: self.id,
            display: self.display.clone(),
            source: self.source,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    open: bool,
    pending: Vec<TurnInput>,
}

/// A cheap control endpoint: no operation takes the runtime/session mutex.
#[derive(Debug, Clone, Default)]
pub struct TurnInputInbox {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
}

impl TurnInputInbox {
    pub fn begin(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(state.pending.is_empty());
        state.open = true;
    }

    /// Refusal transfers every item back to the caller, including turn-end races.
    pub fn admit(&self, inputs: Vec<TurnInput>) -> Result<(), Vec<TurnInput>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.open {
            return Err(inputs);
        }
        state.pending.extend(inputs);
        self.changed.notify_one();
        Ok(())
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .is_empty()
    }

    pub async fn ready(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.has_pending() {
                return;
            }
            notified.await;
        }
    }

    #[must_use]
    pub fn batch(&self) -> Vec<TurnInput> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .clone()
    }

    pub fn committed(&self, id: u64) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .retain(|input| input.id != id);
    }

    /// Atomically decide that this turn has no more work and refuse late inputs.
    pub fn seal_if_empty(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.pending.is_empty() {
            return false;
        }
        state.open = false;
        true
    }

    /// Return unapplied inputs after failure/cancellation; never silently drop them.
    pub fn finish(&self) -> Vec<TurnInput> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.open = false;
        std::mem::take(&mut state.pending)
    }
}
