//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../a2a_live_duet.rs"]
mod a2a_live_duet;

#[path = "../mock_parity_harness.rs"]
mod mock_parity_harness;

#[path = "../pty_agent_memory_scoping.rs"]
mod pty_agent_memory_scoping;

#[path = "../pty_azure_inline_reasoning.rs"]
mod pty_azure_inline_reasoning;

#[path = "../pty_bocha_search.rs"]
mod pty_bocha_search;

#[path = "../pty_config_discovery.rs"]
mod pty_config_discovery;

#[path = "../pty_deferred_tools.rs"]
mod pty_deferred_tools;

#[path = "../pty_headless.rs"]
mod pty_headless;

#[path = "../pty_mcp_manage.rs"]
mod pty_mcp_manage;

#[path = "../pty_model_switch_iocraft.rs"]
mod pty_model_switch_iocraft;

#[path = "../pty_plan_markdown.rs"]
mod pty_plan_markdown;

#[path = "../pty_receiver_freeze.rs"]
mod pty_receiver_freeze;

#[path = "../pty_retry_visibility.rs"]
mod pty_retry_visibility;

#[path = "../pty_spinner_token_counter.rs"]
mod pty_spinner_token_counter;

#[path = "../pty_syntax_theme.rs"]
mod pty_syntax_theme;

#[path = "../pty_turn_steering.rs"]
mod pty_turn_steering;
