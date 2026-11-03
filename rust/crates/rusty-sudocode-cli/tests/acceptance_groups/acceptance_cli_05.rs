//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../cli_runtime_setup.rs"]
mod cli_runtime_setup;

#[path = "../pty_a2a_overlay.rs"]
mod pty_a2a_overlay;

#[path = "../pty_agent_summary.rs"]
mod pty_agent_summary;

#[path = "../pty_bash_mode.rs"]
mod pty_bash_mode;

#[path = "../pty_cancel.rs"]
mod pty_cancel;

#[path = "../pty_coordinator_push.rs"]
mod pty_coordinator_push;

#[path = "../pty_error_paths.rs"]
mod pty_error_paths;

#[path = "../pty_image_handling.rs"]
mod pty_image_handling;

#[path = "../pty_model_compat.rs"]
mod pty_model_compat;

#[path = "../pty_peer_descriptions_live.rs"]
mod pty_peer_descriptions_live;

#[path = "../pty_proxy_codex_protocol.rs"]
mod pty_proxy_codex_protocol;

#[path = "../pty_repl_async_queue.rs"]
mod pty_repl_async_queue;

#[path = "../pty_session_picker.rs"]
mod pty_session_picker;

#[path = "../pty_subagent_workflow.rs"]
mod pty_subagent_workflow;

#[path = "../pty_todo_write.rs"]
mod pty_todo_write;

#[path = "../stdin_pipe_deadline.rs"]
mod stdin_pipe_deadline;
