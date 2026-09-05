//! `engine-host` — the sudocode engine, hosted below the seam.
//!
//! This crate owns the concrete **engine side** of the `engine-core` seam: the
//! thing that actually runs a turn. It is the home of
//!
//! * `SessionEngine` — the `engine_core::EngineDelegate` (turns) **and**
//!   `SessionLifecycle` (non-turn ops: model/auth/permission switch, reset,
//!   resume, fork, compaction) impl for one live session;
//! * the `ConversationRuntime` wrapper (`BuiltRuntime`) + `build_runtime*`
//!   construction (plugins, MCP, policy, system prompt, `EngineApiClient`);
//! * the CLI tool executor (renderer-agnostic: it dispatches tools and returns
//!   results; live output crosses the seam as `EngineEvent`, never a direct
//!   terminal write).
//!
//! # Why it is its own crate
//!
//! Both consumers of a session build one through here:
//!
//! ```text
//!   rusty-sudocode-cli (REPL renderer) ─┐
//!                                        ├─▶ engine_host::SessionEngine ─▶ EngineDelegate
//!   engine-acp (ACP renderer, N sess.) ─┘        (one live session)
//! ```
//!
//! Keeping the engine here — not in the CLI binary — means the renderer crates
//! (`rusty-sudocode-cli`, `engine-acp`) never need to *name* the engine-internal
//! streaming types (`runtime::RuntimeObserver`, `runtime::TurnSummary`): those
//! stay below the seam, in `engine-core` (the adapters) and here (the delegate
//! impl). That is what lets the CI boundary-gate forbid those types in the
//! renderer crates without a false positive on legitimate engine code.
//!
//! The engine code moves in atomically (out of `rusty-sudocode-cli/src/main.rs`);
//! this module doc is the scaffold that records the crate's contract first.

/// Config / model / permission resolution (the engine-side SSOT the REPL and
/// `engine-acp` both build a session from).
pub mod config;

/// Engine-side MCP state: merges plugin/config/session MCP servers, discovers
/// their tools on an isolated runtime, and dispatches MCP tool calls. The
/// renderer holds an `Arc<Mutex<RuntimeMcpState>>` and drives it by method.
pub mod mcp;

/// Session construction / resolution / persistence over `runtime::SessionStore`
/// (the transcript store the turn loop reads and writes). The renderer keeps
/// only the session-list / picker / confirmation UI.
pub mod session;

/// The CLI tool executor: dispatches a tool call and returns the result.
/// Renderer-agnostic — live output crosses the seam as an `EngineEvent`, never a
/// direct terminal write. The `ConversationRuntime` holds it as its `T`.
pub mod tool_executor;

/// System-prompt assembly (process-default + CLI-flag overrides + per-session
/// `_meta` layering). Which prompt the model sees is an engine input.
pub mod prompt;

/// Process-singleton standalone nexus-A2A session (send + receive) the engine
/// build path dials. The renderer starts the receive poller; the tool executor
/// gets the send half.
pub mod nexus_a2a;
