//! # THE engine↔renderer seam (read this before adding any cross-boundary path)
//!
//! sudocode has exactly ONE abstraction between the engine (the model/tool loop)
//! and every renderer (the REPL, ACP for moss/sudowork, …). If you are wiring a
//! new frontend, a new transport, or any engine↔UI communication, it goes
//! through here — do NOT grow a second path. The whole cut is three types:
//!
//! ```text
//!   renderer side (ABOVE the seam)                 engine side (BELOW)
//!   ──────────────────────────────                 ───────────────────
//!   REPL ┐
//!   ACP  ┼─▶ EngineHandle { commands, events } ─▶ EngineSession ─▶ EngineDelegate
//!   moss ┘   send EngineCommand / recv EngineEvent   (the pump)     (impl'd per engine)
//! ```
//!
//! * [`EngineCommand`] / [`EngineEvent`] — the only *data* that crosses (defined
//!   in `engine_events`; payload value types ride along, re-exported there).
//! * [`EngineHandle`] — **to add a renderer, consume this.** `commands.send(..)`
//!   drives the engine, `events.recv()` observes it. Nothing else.
//! * [`EngineDelegate`] — **to plug an engine, implement this.** One method per
//!   thing a turn can do. Nothing else.
//!
//! [`EngineSession`] is the pump between them — generic over `dyn EngineDelegate`,
//! zero engine/renderer-specific logic. See [`session`] for the full map + the
//! one internal subtlety (the sync-prompt → async-answer bridge).
//!
//! # What else this crate owns
//!
//! The pure (non-rendering) `ApiClient` the engine uses, and the
//! **config/provider/error re-export surface**: `api` is historically the CLI's
//! config SSOT (`SudoCodeConfig`, `resolve_model`, `ApiError`, …), so
//! re-exporting those here lets renderer crates drop their direct `api`
//! dependency — which makes the wire type `api::StreamEvent` *un-nameable* in a
//! renderer (the compiler half of the seam enforcement). Only the config /
//! provider / error surface is re-exported; the wire / streaming types
//! (`StreamEvent`, `MessageRequest`, the provider clients, …) stay internal to
//! the engine side.

mod session;
pub use session::{EngineDelegate, EngineHandle, EngineSession};

// Re-export the seam data types so a renderer / engine gets the WHOLE seam from
// `engine_core` alone (`use engine_core::{EngineEvent, EngineCommand,
// EngineSession, EngineHandle, EngineDelegate};`). The payload value types ride
// along via `engine_events` (which re-exports them from `runtime`).
pub use engine_events;
pub use engine_events::{EngineCommand, EngineEvent, EngineState, RequestId, TurnComplete};

// --- Config / provider / error surface (the renderer's `api::` SSOT) ---------
//
// These 16 symbols are every `api::` item the renderer crate uses that is NOT a
// wire/streaming type. Re-exported verbatim so `rusty-sudocode-cli` /
// `engine-acp` can flip `api::X` → `engine_core::X` and remove `api` from their
// Cargo.toml.
pub use api::{
    // Provider/base-url resolution + constants.
    base_url_for_mode,
    max_tokens_for_model,
    read_base_url,
    resolve_model,
    resolve_provider_from_config,
    resolve_startup_auth_source,
    // Error surface.
    ApiError,
    // Auth / provider enums + sources.
    AuthMode,
    AuthSource,
    // Config SSOT (these are themselves re-exported by `api` from `runtime`).
    ModelConfigEntry,
    ModelProviderMapping,
    ProviderConnectionConfig,
    ProviderKind,
    // Transport retry hook (implemented by the CLI spinner bridge).
    RetryNotifier,
    SudoCodeConfig,
    DEFAULT_BASE_URL,
};
