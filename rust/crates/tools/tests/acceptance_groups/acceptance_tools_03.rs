//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../agent_color_assignment.rs"]
mod agent_color_assignment;

#[path = "../agent_list_discovery.rs"]
mod agent_list_discovery;

#[path = "../agent_output_wire_schema.rs"]
mod agent_output_wire_schema;

#[path = "../coordinator_push_notification.rs"]
mod coordinator_push_notification;

#[path = "../registry_fs_injection.rs"]
mod registry_fs_injection;

#[path = "../task_list_backgrounded.rs"]
mod task_list_backgrounded;

#[path = "../verification_streak.rs"]
mod verification_streak;
