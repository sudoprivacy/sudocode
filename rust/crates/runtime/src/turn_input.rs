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
    Scheduled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput {
    pub id: u64,
    pub text: String,
    pub display: String,
    pub source: InputSource,
    /// Byte ranges of `display` that were pasted rather than typed.
    pub pasted: Vec<std::ops::Range<usize>>,
    /// Native image blocks travel with the input through queueing and steering.
    pub attachments: Vec<crate::ContentBlock>,
}

/// Identity and display projection; expanded prompt content stays with its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputView {
    pub id: u64,
    pub display: String,
    pub source: InputSource,
    /// Byte ranges of `display` that were pasted rather than typed.
    pub pasted: Vec<std::ops::Range<usize>>,
}

/// How an input was drawn in scrollback. It is saved with the message the
/// input became, so a restored session draws it exactly as it was shown live:
/// a sub-agent result as its one-line notice rather than the notification the
/// model read, a paste dimmed. Never sent to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEcho {
    pub source: InputSource,
    pub display: String,
    /// Byte ranges of `display` that were pasted rather than typed.
    pub pasted: Vec<std::ops::Range<usize>>,
}

impl TurnInput {
    #[must_use]
    #[inline]
    pub fn view(&self) -> InputView {
        InputView {
            id: self.id,
            display: self.display.clone(),
            source: self.source,
            pasted: self.pasted.clone(),
        }
    }
}

impl InputView {
    #[must_use]
    pub fn echo(&self) -> InputEcho {
        InputEcho {
            source: self.source,
            display: self.display.clone(),
            pasted: self.pasted.clone(),
        }
    }
}

impl InputEcho {
    pub(crate) fn to_json(&self) -> crate::json::JsonValue {
        use crate::json::JsonValue;
        let mut object = std::collections::BTreeMap::from([
            (
                "source".to_string(),
                JsonValue::String(self.source.name().to_string()),
            ),
            (
                "display".to_string(),
                JsonValue::String(self.display.clone()),
            ),
        ]);
        if !self.pasted.is_empty() {
            let ranges = self
                .pasted
                .iter()
                .map(|range| {
                    JsonValue::Array(
                        [range.start, range.end]
                            .map(|offset| {
                                JsonValue::Number(i64::try_from(offset).unwrap_or(i64::MAX))
                            })
                            .to_vec(),
                    )
                })
                .collect();
            object.insert("pasted".to_string(), JsonValue::Array(ranges));
        }
        JsonValue::Object(object)
    }

    /// `None` for a record this build cannot read (a source added later, a
    /// range outside its text), so the message falls back to its text instead
    /// of failing to load. The record is presentation only and is not kept:
    /// this build drops it when it next saves the session, as builds older
    /// than the `inputs` key drop the whole key.
    pub(crate) fn from_json(value: &crate::json::JsonValue) -> Option<Self> {
        let object = value.as_object()?;
        let source = InputSource::from_name(object.get("source")?.as_str()?)?;
        let display = object.get("display")?.as_str()?.to_string();
        let pasted = match object.get("pasted") {
            None => Vec::new(),
            Some(ranges) => ranges
                .as_array()?
                .iter()
                .map(|range| {
                    let [start, end] = range.as_array()? else {
                        return None;
                    };
                    let start = usize::try_from(start.as_i64()?).ok()?;
                    let end = usize::try_from(end.as_i64()?).ok()?;
                    (start < end
                        && end <= display.len()
                        && display.is_char_boundary(start)
                        && display.is_char_boundary(end))
                    .then_some(start..end)
                })
                .collect::<Option<_>>()?,
        };
        Some(Self {
            source,
            display,
            pasted,
        })
    }
}

impl InputSource {
    const fn name(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Peer => "peer",
            Self::Subagent => "subagent",
            Self::Background => "background",
            Self::Scheduled => "scheduled",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        [
            Self::Human,
            Self::Peer,
            Self::Subagent,
            Self::Background,
            Self::Scheduled,
        ]
        .into_iter()
        .find(|source| source.name() == name)
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
        debug_assert_eq!(state.pending, [] as [crate::turn_input::TurnInput; 0]);
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
