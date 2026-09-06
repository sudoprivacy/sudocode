//! Session lifecycle classification now lives in `commands::reports` (shared
//! with the ACP renderer). Re-exported here so the crate's existing
//! `crate::cli::lifecycle::*` import paths keep resolving to the one definition.

pub(crate) use commands::reports::{classify_session_lifecycle_for, SessionLifecycleSummary};
