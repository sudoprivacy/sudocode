//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../coordinator_push_wire.rs"]
mod coordinator_push_wire;

#[path = "../cursor_fence.rs"]
mod cursor_fence;

#[path = "../delivery_stall_observability.rs"]
mod delivery_stall_observability;

#[path = "../one_receiver_per_agent.rs"]
mod one_receiver_per_agent;

#[path = "../session_pre_seed.rs"]
mod session_pre_seed;

#[path = "../shared_session_migration.rs"]
mod shared_session_migration;
