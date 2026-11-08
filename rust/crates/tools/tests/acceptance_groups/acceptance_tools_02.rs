//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../agent_permission_bubble.rs"]
mod agent_permission_bubble;

#[path = "../agent_summary_threshold.rs"]
mod agent_summary_threshold;

#[path = "../agent_telemetry_pipeline.rs"]
mod agent_telemetry_pipeline;

#[path = "../cohost_agent_store.rs"]
mod cohost_agent_store;

#[path = "../send_dead_pid.rs"]
mod send_dead_pid;

#[path = "../subagent_multi_turn_loop.rs"]
mod subagent_multi_turn_loop;
