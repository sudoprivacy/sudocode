//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../interrupt_e2e.rs"]
mod interrupt_e2e;

#[path = "../pty_agent_duet.rs"]
mod pty_agent_duet;

#[path = "../pty_auth_profile.rs"]
mod pty_auth_profile;

#[path = "../pty_block_spacing.rs"]
mod pty_block_spacing;

#[path = "../pty_compaction_safety.rs"]
mod pty_compaction_safety;

#[path = "../pty_custom_agents.rs"]
mod pty_custom_agents;

#[path = "../pty_foreground_activity.rs"]
mod pty_foreground_activity;

#[path = "../pty_iocraft_chrome.rs"]
mod pty_iocraft_chrome;

#[path = "../pty_model_flag_priority.rs"]
mod pty_model_flag_priority;

#[path = "../pty_plan_confirm.rs"]
mod pty_plan_confirm;

#[path = "../pty_raw_mode_line_endings.rs"]
mod pty_raw_mode_line_endings;

#[path = "../pty_resume.rs"]
mod pty_resume;

#[path = "../pty_smoke.rs"]
mod pty_smoke;

#[path = "../pty_sync_repl_receive.rs"]
mod pty_sync_repl_receive;

#[path = "../pty_tool_pause.rs"]
mod pty_tool_pause;

#[path = "../visual_regression.rs"]
mod visual_regression;
