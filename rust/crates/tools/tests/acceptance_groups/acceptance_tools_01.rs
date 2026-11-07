//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../agent_abort_registry.rs"]
mod agent_abort_registry;

#[path = "../coordinator_dispatch_guard.rs"]
mod coordinator_dispatch_guard;

#[path = "../unified_agent_pid_e2e.rs"]
mod unified_agent_pid_e2e;
