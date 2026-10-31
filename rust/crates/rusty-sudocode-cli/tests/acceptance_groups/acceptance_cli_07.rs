//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../headless_mode.rs"]
mod headless_mode;

#[path = "../pty_agent_directory.rs"]
mod pty_agent_directory;

#[path = "../pty_async_esc_cancel.rs"]
mod pty_async_esc_cancel;

#[path = "../pty_bash_streaming.rs"]
mod pty_bash_streaming;

#[path = "../pty_chrome_styles.rs"]
mod pty_chrome_styles;

#[path = "../pty_cron.rs"]
mod pty_cron;

#[path = "../pty_footer_tips.rs"]
mod pty_footer_tips;

#[path = "../pty_input_batch.rs"]
mod pty_input_batch;

#[path = "../pty_model_discovery.rs"]
mod pty_model_discovery;

#[path = "../pty_permission_prompt.rs"]
mod pty_permission_prompt;

#[path = "../pty_queue_overlay.rs"]
mod pty_queue_overlay;

#[path = "../pty_repl_iocraft_features.rs"]
mod pty_repl_iocraft_features;

#[path = "../pty_sleep.rs"]
mod pty_sleep;

#[path = "../pty_sync_repl_crash_redelivery.rs"]
mod pty_sync_repl_crash_redelivery;

#[path = "../pty_tool_history.rs"]
mod pty_tool_history;

#[path = "../undo_e2e.rs"]
mod undo_e2e;
