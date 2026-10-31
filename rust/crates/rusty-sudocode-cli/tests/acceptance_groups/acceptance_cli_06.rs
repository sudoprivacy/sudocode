//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../compact_output.rs"]
mod compact_output;

#[path = "../pty_adaptive_cache_live.rs"]
mod pty_adaptive_cache_live;

#[path = "../pty_arrow_keys.rs"]
mod pty_arrow_keys;

#[path = "../pty_bash_process_tree.rs"]
mod pty_bash_process_tree;

#[path = "../pty_chrome_budget.rs"]
mod pty_chrome_budget;

#[path = "../pty_core_conversation.rs"]
mod pty_core_conversation;

#[path = "../pty_file_operations.rs"]
mod pty_file_operations;

#[path = "../pty_image_paste.rs"]
mod pty_image_paste;

#[path = "../pty_model_context.rs"]
mod pty_model_context;

#[path = "../pty_peer_status.rs"]
mod pty_peer_status;

#[path = "../pty_proxy_passthrough.rs"]
mod pty_proxy_passthrough;

#[path = "../pty_repl_async_up_dequeue.rs"]
mod pty_repl_async_up_dequeue;

#[path = "../pty_shared_session.rs"]
mod pty_shared_session;

#[path = "../pty_subcommands.rs"]
mod pty_subcommands;

#[path = "../pty_tool_concurrency.rs"]
mod pty_tool_concurrency;

#[path = "../system_prompt_command.rs"]
mod system_prompt_command;
