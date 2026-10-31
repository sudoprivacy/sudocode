//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../acp_a2a_receive.rs"]
mod acp_a2a_receive;

#[path = "../output_format_contract.rs"]
mod output_format_contract;

#[path = "../pty_agent_model_inheritance.rs"]
mod pty_agent_model_inheritance;

#[path = "../pty_background_tasks.rs"]
mod pty_background_tasks;

#[path = "../pty_bracketed_paste.rs"]
mod pty_bracketed_paste;

#[path = "../pty_config_set.rs"]
mod pty_config_set;

#[path = "../pty_dialpad_custom_input.rs"]
mod pty_dialpad_custom_input;

#[path = "../pty_hook_progress.rs"]
mod pty_hook_progress;

#[path = "../pty_mcp_tool.rs"]
mod pty_mcp_tool;

#[path = "../pty_model_workflow_live.rs"]
mod pty_model_workflow_live;

#[path = "../pty_plan_presentation.rs"]
mod pty_plan_presentation;

#[path = "../pty_release_updates.rs"]
mod pty_release_updates;

#[path = "../pty_screen_rows.rs"]
mod pty_screen_rows;

#[path = "../pty_staging_running_card.rs"]
mod pty_staging_running_card;

#[path = "../pty_terminal_handoff.rs"]
mod pty_terminal_handoff;

#[path = "../pty_verification_streak.rs"]
mod pty_verification_streak;
