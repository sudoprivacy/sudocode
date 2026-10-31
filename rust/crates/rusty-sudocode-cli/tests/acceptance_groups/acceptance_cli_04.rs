//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../cli_flags_and_config_defaults.rs"]
mod cli_flags_and_config_defaults;

#[path = "../pty_a2a_health.rs"]
mod pty_a2a_health;

#[path = "../pty_agent_permission_inheritance.rs"]
mod pty_agent_permission_inheritance;

#[path = "../pty_bash_execution.rs"]
mod pty_bash_execution;

#[path = "../pty_cache_stats.rs"]
mod pty_cache_stats;

#[path = "../pty_coordinator_mode.rs"]
mod pty_coordinator_mode;

#[path = "../pty_edit_uniqueness.rs"]
mod pty_edit_uniqueness;

#[path = "../pty_identity_receiver_standby.rs"]
mod pty_identity_receiver_standby;

#[path = "../pty_mermaid.rs"]
mod pty_mermaid;

#[path = "../pty_offline_delivery.rs"]
mod pty_offline_delivery;

#[path = "../pty_presets_e2e.rs"]
mod pty_presets_e2e;

#[path = "../pty_repl_async_batched_flush.rs"]
mod pty_repl_async_batched_flush;

#[path = "../pty_session_management.rs"]
mod pty_session_management;

#[path = "../pty_step_budget.rs"]
mod pty_step_budget;

#[path = "../pty_thinking_visible.rs"]
mod pty_thinking_visible;

#[path = "../resume_slash_commands.rs"]
mod resume_slash_commands;
