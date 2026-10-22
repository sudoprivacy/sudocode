//! ACP (Agent Client Protocol) server + transports — the renderer that speaks
//! ACP over stdio/ws to moss and sudowork.
//!
//! Moved out of the `runtime` crate (the engine's runtime layer) so ACP is a
//! renderer-side consumer of the engine, not baked into the engine core — the
//! same cut the in-process REPL renderer follows. `AcpError` lives here (its
//! only consumer is the CLI's ACP wiring, which names it through this crate).

// The ACP server's handler chain is one nested generic type per request it
// serves; computing its layout needs more than the default 128 levels. The
// server boxes that future, so crates that run it need no such limit.
#![recursion_limit = "256"]

pub mod acp_sdk_server;
pub mod acp_stdio_server;
#[cfg(feature = "websocket")]
pub mod acp_ws_server;
#[cfg(feature = "mailbox")]
pub mod managed_agent;
mod session_driver;
/// ACP-side session lifecycle + turn glue on top of the `engine_core` seam
/// (build/load/fork a session, drive it through `EngineHandle`, and translate
/// `EngineEvent`/`TurnComplete` onto the ACP wire).
mod session_ops;
mod subagent_completions;

pub use acp_sdk_server::AcpError;
