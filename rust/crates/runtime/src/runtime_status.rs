//! Runtime observation contracts, shared with each host's instance registry.
//!
//! The engine remains the writer of foreground facts and background counts.
//! Hosts supply routing, instance ownership and freshness; they do not infer
//! activity from transcripts, output or elapsed time.

pub use contracts::runtime_status::{
    online_agent_names, BackgroundTaskSummary, ForegroundState, InputReason, ListRuntimesRequest,
    OnlineAgents, PresenceRequest, PublishRuntimeRequest, RegisterRuntimeRequest,
    RuntimeAvailability, RuntimeFacts, RuntimeIdentity, RuntimeInstanceId, RuntimeInstanceRequest,
    RuntimeInventory, RuntimeInventoryScope, RuntimeObservation, RuntimeObservationHost,
    RuntimePresence, RuntimeStatus, RuntimeStatusSource, RuntimeStatusUpdate,
};

/// A non-owning engine observation. Missing facts must remain unavailable.
pub type RuntimeStatusReader = std::sync::Arc<dyn Fn() -> Option<RuntimeStatus> + Send + Sync>;
