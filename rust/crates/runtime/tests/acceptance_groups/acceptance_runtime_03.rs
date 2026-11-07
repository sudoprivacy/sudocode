//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../agent_mailbox_roundtrip.rs"]
mod agent_mailbox_roundtrip;

#[path = "../coordinator_gate.rs"]
mod coordinator_gate;

#[path = "../fork_dispatch_context.rs"]
mod fork_dispatch_context;

#[path = "../mailbox_delivery.rs"]
mod mailbox_delivery;

#[path = "../monitor_watch_lifecycle.rs"]
mod monitor_watch_lifecycle;

#[path = "../peer_description_store.rs"]
mod peer_description_store;

#[path = "../subagent_completion.rs"]
mod subagent_completion;
