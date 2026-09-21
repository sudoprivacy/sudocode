//! Zone context for the sudocode runtime (P1a, SW-20260915-002 §8.11).
//!
//! One rule above all (R6.2): a client payload zone is NEVER authority. The
//! only two trusted sources of an execution zone are
//!
//! 1. the **planted agent descriptor** in the cohost path — written by the
//!    nexus-vfs `ManagedAgentService` (a trusted host), never taken from a
//!    client request body; and
//! 2. the **host-injected runner environment** in the subprocess path —
//!    [`NEXUS_ZONE_ID`] / [`NEXUS_V2_BASE_URL`] / [`NEXUS_DELEGATION_REF`] are set
//!    by the moss runner manifest (step 07's bridge), which derives them
//!    from the authenticated org binding, not from any payload.
//!
//! Both paths build the same [`HostZoneContext`], so cohost and subprocess
//! share identical zone semantics (R6.4). [`ResourceRef`] targets are
//! re-validated on every use (R6.3): the zone-id shape comes from the
//! `sudo-contracts` owner validators (R6.1 — derived Rust artifact of the
//! nexus owner schemas, never a hand-rewritten regex), and a [`ResourceRef`] may
//! only name the runtime's own zone in P1a — cross-zone references are left
//! to the nexus authorization plane, never granted locally.

use kernel::core::agents::registry::AgentDescriptor;
use sudo_contracts::{validate_existing_zone_id_ref, validate_zone_path, ResourceRef};

/// Env vars the trusted host (moss runner manifest) injects.
pub const ENV_NEXUS_ZONE_ID: &str = "NEXUS_ZONE_ID";
pub const ENV_NEXUS_V2_BASE_URL: &str = "NEXUS_V2_BASE_URL";
pub const ENV_NEXUS_DELEGATION_REF: &str = "NEXUS_DELEGATION_REF";

/// Where a zone context claims to come from — carried for audit/log so
/// a mis-wired source is visible instead of silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSource {
    /// Cohost: planted by the nexus-vfs [`ManagedAgentService`].
    PlantedDescriptor,
    /// Subprocess: host-injected runner environment.
    HostEnvironment,
    /// No zone context (root/local standalone runs — unchanged legacy path).
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostZoneContext {
    zone_id: Option<String>,
    source: ContextSource,
}

impl HostZoneContext {
    /// Cohost path (R6.2): the descriptor is planted by the trusted
    /// [`ManagedAgentService`]; its `zone_id` is authority by construction.
    #[must_use]
    pub fn from_planted_descriptor(desc: &AgentDescriptor) -> Self {
        Self {
            zone_id: Some(desc.zone_id.clone()),
            source: ContextSource::PlantedDescriptor,
        }
    }

    /// Subprocess path (R6.2): the zone arrives via host-injected env.
    /// A malformed id is rejected (recorded as absent with the source kept
    /// visible) — never coerced, never defaulted to a guess.
    #[must_use]
    pub fn from_host_env() -> Self {
        match std::env::var(ENV_NEXUS_ZONE_ID) {
            Ok(value) if validate_existing_zone_id_ref(&value) => Self {
                zone_id: Some(value),
                source: ContextSource::HostEnvironment,
            },
            Ok(invalid) => {
                // Payload/host data does not shape-shift into authority: an
                // invalid id downgrades to Absent (legacy root semantics),
                // never into a "closest guess".
                tracing::debug!(zone_id = %invalid, "invalid NEXUS_ZONE_ID ignored");
                Self {
                    zone_id: None,
                    source: ContextSource::Absent,
                }
            }
            Err(_) => Self {
                zone_id: None,
                source: ContextSource::Absent,
            },
        }
    }

    /// The execution zone, when a trusted context exists.
    #[must_use]
    pub fn zone_id(&self) -> Option<&str> {
        self.zone_id.as_deref()
    }

    #[must_use]
    pub fn source(&self) -> &ContextSource {
        &self.source
    }

    /// R6.3 — re-validate a [`ResourceRef`] target against this context.
    ///
    /// Rules: the ref's zone-id must pass the owner validator; its path must
    /// be a zone-relative absolute path; and in P1a the ref may only name
    /// this runtime's own zone — a cross-zone ref is refused here and stays
    /// the nexus authorization plane's decision, never a local grant.
    pub fn authorize_resource_ref(&self, resource: &ResourceRef) -> Result<(), ZoneAuthError> {
        if !validate_existing_zone_id_ref(&resource.zone_id) {
            return Err(ZoneAuthError::InvalidZoneId(resource.zone_id.clone()));
        }
        if !validate_zone_path(&resource.path) {
            return Err(ZoneAuthError::InvalidPath(resource.path.clone()));
        }
        match (&self.zone_id, resource.zone_id.as_str()) {
            (Some(own), z) if own == z => Ok(()),
            (Some(_), _) => Err(ZoneAuthError::CrossZoneRef {
                own: self.zone_id.clone().unwrap_or_default(),
                asked: resource.zone_id.clone(),
            }),
            // No trusted zone context: this runtime is a root/local
            // standalone run; only the reserved root ref is permitted and
            // anything else is refused (fail closed, no implicit grant).
            (None, "root") => Ok(()),
            (None, _) => Err(ZoneAuthError::NoZoneContext(resource.zone_id.clone())),
        }
    }
}

/// Refusal reasons — no grant ever happens implicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneAuthError {
    InvalidZoneId(String),
    InvalidPath(String),
    CrossZoneRef { own: String, asked: String },
    NoZoneContext(String),
}

impl std::fmt::Display for ZoneAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidZoneId(z) => write!(f, "resource zone id {z:?} fails the owner validator"),
            Self::InvalidPath(p) => write!(f, "resource path {p:?} fails the zone-path validator"),
            Self::CrossZoneRef { own, asked } => {
                write!(f, "cross-zone ref to {asked} refused (own zone {own}); cross-zone is the nexus plane's decision")
            }
            Self::NoZoneContext(z) => {
                write!(f, "zone-less runtime cannot authorize ref to {z}")
            }
        }
    }
}

impl std::error::Error for ZoneAuthError {}

/// Process-wide subprocess zone context (resolved once from the host env).
///
/// The env does not change mid-process; caching keeps every later
/// [`ResourceRef`](ResourceRef) authorization consistent with the context
/// the runtime started under.
pub fn host_zone_once() -> &'static HostZoneContext {
    static CACHED: std::sync::OnceLock<HostZoneContext> = std::sync::OnceLock::new();
    CACHED.get_or_init(HostZoneContext::from_host_env)
}
