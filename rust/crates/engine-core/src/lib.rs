//! Engine-side core, just below the [engine↔renderer seam](engine_events).
//!
//! This crate hosts the driver that runs a turn and turns the runtime's
//! callbacks into [`engine_events::EngineEvent`]s (added incrementally in
//! later commits), plus the pure (non-rendering) `ApiClient` the engine uses.
//!
//! It also owns the **config/provider/error re-export surface**. The CLI is a
//! renderer that lives ABOVE the seam, yet `api` is historically the CLI's
//! configuration SSOT too (`SudoCodeConfig`, `resolve_model`, `ApiError`, …).
//! Re-exporting those symbols here lets renderer crates drop their direct `api`
//! dependency entirely — which is what makes the wire type `api::StreamEvent`
//! *un-nameable* in a renderer (the compiler half of the seam enforcement).
//!
//! Only the config / provider / error surface is re-exported. The wire /
//! streaming types (`StreamEvent`, `MessageRequest`, `ContentBlockDelta`, the
//! provider clients, …) are intentionally kept internal to the engine side.
//!
//! # The seam, in one place
//!
//! [`EngineSession`] / [`EngineHandle`] / [`EngineDelegate`] (this crate) plus
//! [`EngineEvent`] / [`EngineCommand`] (re-exported from `engine_events`) are
//! the entire engine↔renderer abstraction. See [`session`] for the map.

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
