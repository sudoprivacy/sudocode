//! ACP (Agent Client Protocol) server + transports — the renderer that speaks
//! ACP over stdio/ws to moss and sudowork.
//!
//! Moved out of the `runtime` crate (the engine's runtime layer) so ACP is a
//! renderer-side consumer of the engine, not baked into the engine core — the
//! same cut the in-process REPL renderer follows. `AcpError` lives here (its
//! only consumer is the CLI's ACP wiring, which names it through this crate).

pub mod acp_sdk_server;
pub mod acp_stdio_server;
pub mod acp_ws_server;

pub use acp_sdk_server::AcpError;
