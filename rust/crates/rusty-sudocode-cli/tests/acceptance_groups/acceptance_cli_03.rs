//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../acp_integration.rs"]
mod acp_integration;

#[path = "../pty_a2a_cards.rs"]
mod pty_a2a_cards;

#[path = "../pty_agent_permission_bubble.rs"]
mod pty_agent_permission_bubble;

#[path = "../pty_bash_compact_results.rs"]
mod pty_bash_compact_results;

#[path = "../pty_cache_prefix.rs"]
mod pty_cache_prefix;

#[path = "../pty_context_usage.rs"]
mod pty_context_usage;

#[path = "../pty_double_ctrlc_exit.rs"]
mod pty_double_ctrlc_exit;

#[path = "../pty_host_input_queue.rs"]
mod pty_host_input_queue;

#[path = "../pty_memory.rs"]
mod pty_memory;

#[path = "../pty_node_record.rs"]
mod pty_node_record;

#[path = "../pty_plan_review.rs"]
mod pty_plan_review;

#[path = "../pty_render_boundaries.rs"]
mod pty_render_boundaries;

#[path = "../pty_send_message_multi_turn.rs"]
mod pty_send_message_multi_turn;

#[path = "../pty_startup.rs"]
mod pty_startup;

#[path = "../pty_text_layout.rs"]
mod pty_text_layout;

#[path = "../pty_write_plan.rs"]
mod pty_write_plan;
