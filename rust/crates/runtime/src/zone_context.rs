//! Zone context for the sudocode runtime (P1a, SW-20260915-002 §8.11).
//!
//! One rule above all (R6.2): a client payload zone is NEVER authority. The
//! execution-zone metadata may arrive from
//!
//! 1. the **planted agent descriptor** in the cohost path; and
//! 2. the **host-injected runner environment** in the subprocess path —
//!    [`NEXUS_ZONE_ID`] / [`NEXUS_V2_BASE_URL`] / [`NEXUS_DELEGATION_REF`] are set
//!    by the moss runner manifest.
//!
//! Neither source proves that the service validated the referenced delegation,
//! so both are [`ContextSource::UnverifiedDelegationRef`] and can only cause an
//! early denial. Only an authenticated system/NoAuth in-process kernel context
//! is [`ContextSource::TrustedLocal`]. [`ResourceRef`] targets are
//! re-validated on every use (R6.3): the zone-id shape comes from the
//! `sudo-contracts` owner validators (R6.1 — derived Rust artifact of the
//! nexus owner schemas, never a hand-rewritten regex), and a [`ResourceRef`] may
//! only name the runtime's own zone in P1a — cross-zone references are left
//! to the nexus authorization plane, never granted locally.

use kernel::core::agents::registry::AgentDescriptor;
use sudo_contracts::{
    validate_existing_zone_id_ref, validate_zone_path, ResourceRef, RuntimeResourceScope, Validate,
};

/// Env vars the trusted host (moss runner manifest) injects.
pub const ENV_NEXUS_ZONE_ID: &str = "NEXUS_ZONE_ID";
pub const ENV_NEXUS_V2_BASE_URL: &str = "NEXUS_V2_BASE_URL";
pub const ENV_NEXUS_DELEGATION_REF: &str = "NEXUS_DELEGATION_REF";
pub const ENV_NEXUS_RESOURCE_SCOPE: &str = "NEXUS_RESOURCE_SCOPE";

/// Where a zone context claims to come from — carried for audit/log so
/// a mis-wired source is visible instead of silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSource {
    /// Authenticated system/NoAuth in-process kernel context.
    TrustedLocal,
    /// Descriptor/env metadata without a server-verifiable delegation credential.
    UnverifiedDelegationRef,
    /// No zone context (root/local standalone runs — unchanged legacy path).
    Absent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostZoneContext {
    zone_id: Option<String>,
    nexus_v2_base_url: Option<String>,
    delegation_ref: Option<String>,
    resource_scope: Option<RuntimeResourceScope>,
    resource_scope_present: bool,
    source: ContextSource,
}

impl HostZoneContext {
    /// Cohost path (R6.2): the descriptor is planted by the trusted
    /// [`ManagedAgentService`]; its `zone_id` is authority by construction,
    /// so the context is [`ContextSource::TrustedLocal`] — an in-process
    /// planted descriptor is not an unverified credential. A descriptor
    /// carrying `ENV_NEXUS_RESOURCE_SCOPE` is enforced to that scope
    /// (host-injected runtime resource scope); one without labels runs
    /// unscoped, exactly as the trusted host that planted it.
    #[must_use]
    pub fn from_planted_descriptor(desc: &AgentDescriptor) -> Self {
        let zone_id = validate_existing_zone_id_ref(&desc.zone_id).then(|| desc.zone_id.clone());
        let scope_raw = desc.labels.get(ENV_NEXUS_RESOURCE_SCOPE).cloned();
        let resource_scope = parse_resource_scope(scope_raw.as_deref());
        Self {
            zone_id,
            nexus_v2_base_url: desc.labels.get(ENV_NEXUS_V2_BASE_URL).cloned(),
            delegation_ref: desc.labels.get(ENV_NEXUS_DELEGATION_REF).cloned(),
            resource_scope,
            resource_scope_present: scope_raw.is_some(),
            source: ContextSource::TrustedLocal,
        }
    }

    /// Subprocess path (R6.2): the zone arrives via host-injected env.
    /// A malformed id is rejected (recorded without a usable zone while the
    /// attempted host source remains visible) — never coerced or guessed.
    #[must_use]
    pub fn from_host_env() -> Self {
        match std::env::var(ENV_NEXUS_ZONE_ID) {
            Ok(value) if validate_existing_zone_id_ref(&value) => Self {
                zone_id: Some(value),
                nexus_v2_base_url: std::env::var(ENV_NEXUS_V2_BASE_URL).ok(),
                delegation_ref: std::env::var(ENV_NEXUS_DELEGATION_REF).ok(),
                resource_scope: parse_resource_scope(
                    std::env::var(ENV_NEXUS_RESOURCE_SCOPE).ok().as_deref(),
                ),
                resource_scope_present: std::env::var(ENV_NEXUS_RESOURCE_SCOPE).is_ok(),
                source: ContextSource::UnverifiedDelegationRef,
            },
            Ok(invalid) => {
                // Payload/host data does not shape-shift into authority: an
                // invalid id downgrades to Absent (legacy root semantics),
                // never into a "closest guess".
                tracing::debug!(zone_id = %invalid, "invalid NEXUS_ZONE_ID ignored");
                Self {
                    zone_id: None,
                    nexus_v2_base_url: None,
                    delegation_ref: None,
                    resource_scope: None,
                    resource_scope_present: std::env::var(ENV_NEXUS_RESOURCE_SCOPE).is_ok(),
                    source: ContextSource::UnverifiedDelegationRef,
                }
            }
            Err(_) => Self {
                zone_id: None,
                nexus_v2_base_url: None,
                delegation_ref: None,
                resource_scope: None,
                resource_scope_present: false,
                source: ContextSource::Absent,
            },
        }
    }

    /// Explicit constructor used by trusted host adapters and integration
    /// tests without mutating process-global environment variables.
    #[must_use]
    pub fn from_trusted_parts(
        zone_id: impl Into<String>,
        nexus_v2_base_url: Option<String>,
        delegation_ref: Option<String>,
        source: ContextSource,
    ) -> Self {
        Self::from_parts_with_scope(zone_id, nexus_v2_base_url, delegation_ref, None, source)
    }

    #[must_use]
    pub fn from_parts_with_scope(
        zone_id: impl Into<String>,
        nexus_v2_base_url: Option<String>,
        delegation_ref: Option<String>,
        resource_scope_json: Option<&str>,
        source: ContextSource,
    ) -> Self {
        let zone_id = zone_id.into();
        let resource_scope = parse_resource_scope(resource_scope_json);
        Self {
            zone_id: validate_existing_zone_id_ref(&zone_id).then_some(zone_id),
            nexus_v2_base_url,
            delegation_ref: delegation_ref.filter(|value| !value.trim().is_empty()),
            resource_scope,
            resource_scope_present: resource_scope_json.is_some(),
            source,
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

    #[must_use]
    pub fn nexus_v2_base_url(&self) -> Option<&str> {
        self.nexus_v2_base_url.as_deref()
    }

    #[must_use]
    pub fn delegation_ref(&self) -> Option<&str> {
        self.delegation_ref.as_deref()
    }

    /// Build and validate the canonical [`ResourceRef`] for one target access.
    pub fn authorize_path(&self, path: &str) -> Result<ResourceRef, ZoneAuthError> {
        self.authorize_path_for(ResourceAccessKind::Read, "zone.data.read", path)
    }

    pub fn authorize_path_for(
        &self,
        access_kind: ResourceAccessKind,
        capability: &str,
        path: &str,
    ) -> Result<ResourceRef, ZoneAuthError> {
        let zone_id = match (&self.zone_id, &self.source) {
            (Some(zone_id), _) => zone_id.clone(),
            (None, ContextSource::Absent) => "root".to_string(),
            (None, _) => return Err(ZoneAuthError::NoZoneContext("invalid".to_string())),
        };
        let resource = ResourceRef {
            api_version: "common.sudo.dev/v1".to_string(),
            kind: "ResourceRef".to_string(),
            zone_id,
            path: path.to_string(),
            version: None,
            digest: None,
            media_type: None,
            size_bytes: None,
        };
        RuntimeResourceAuthorizer::new(self).authorize(access_kind, capability, &resource)?;
        Ok(resource)
    }

    /// R6.3 — re-validate a [`ResourceRef`] target against this context.
    ///
    /// Rules: the ref's zone-id must pass the owner validator; its path must
    /// be a zone-relative absolute path; and in P1a the ref may only name
    /// this runtime's own zone — a cross-zone ref is refused here and stays
    /// the nexus authorization plane's decision, never a local grant.
    pub fn authorize_resource_ref(&self, resource: &ResourceRef) -> Result<(), ZoneAuthError> {
        RuntimeResourceAuthorizer::new(self).authorize(
            ResourceAccessKind::Read,
            "zone.data.read",
            resource,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAccessKind {
    Read,
    Write,
}

pub struct RuntimeResourceAuthorizer<'a> {
    context: &'a HostZoneContext,
}

impl<'a> RuntimeResourceAuthorizer<'a> {
    #[must_use]
    pub fn new(context: &'a HostZoneContext) -> Self {
        Self { context }
    }

    pub fn authorize(
        &self,
        access_kind: ResourceAccessKind,
        capability: &str,
        resource: &ResourceRef,
    ) -> Result<(), ZoneAuthError> {
        if !validate_existing_zone_id_ref(&resource.zone_id) {
            return Err(ZoneAuthError::InvalidZoneId(resource.zone_id.clone()));
        }
        if !validate_zone_path(&resource.path) {
            return Err(ZoneAuthError::InvalidPath(resource.path.clone()));
        }
        let expected_capability = match access_kind {
            ResourceAccessKind::Read => "zone.data.read",
            ResourceAccessKind::Write => "zone.data.write",
        };
        if capability != expected_capability {
            return Err(ZoneAuthError::OutOfScope(resource.path.clone()));
        }
        match (&self.context.zone_id, resource.zone_id.as_str()) {
            (Some(own), asked) if own == asked => Ok(()),
            (Some(_), _) => Err(ZoneAuthError::CrossZoneRef {
                own: self.context.zone_id.clone().unwrap_or_default(),
                asked: resource.zone_id.clone(),
            }),
            (None, "root") if self.context.source == ContextSource::Absent => Ok(()),
            (None, _) => Err(ZoneAuthError::NoZoneContext(resource.zone_id.clone())),
        }?;

        match self.context.source {
            ContextSource::Absent => Ok(()),
            ContextSource::TrustedLocal => self.authorize_scope(capability, resource),
            ContextSource::UnverifiedDelegationRef => {
                self.authorize_scope(capability, resource)?;
                Err(ZoneAuthError::DelegationInvalid)
            }
        }
    }

    fn authorize_scope(
        &self,
        capability: &str,
        resource: &ResourceRef,
    ) -> Result<(), ZoneAuthError> {
        let Some(scope) = &self.context.resource_scope else {
            return if self.context.resource_scope_present {
                Err(ZoneAuthError::DelegationInvalid)
            } else if self.context.source == ContextSource::TrustedLocal {
                Ok(())
            } else {
                Err(ZoneAuthError::ScopeRequired)
            };
        };
        if scope.zone_id != resource.zone_id {
            return Err(ZoneAuthError::DelegationInvalid);
        }
        let allowed = scope.rules.iter().any(|rule| {
            rule.capability == capability
                && rule
                    .resource_prefixes
                    .iter()
                    .any(|prefix| path_is_within(&resource.path, prefix))
        });
        allowed
            .then_some(())
            .ok_or_else(|| ZoneAuthError::OutOfScope(resource.path.clone()))
    }
}

fn parse_resource_scope(raw: Option<&str>) -> Option<RuntimeResourceScope> {
    raw.and_then(|value| serde_json::from_str::<RuntimeResourceScope>(value).ok())
        .filter(|scope| scope.validate().is_ok())
}

fn path_is_within(path: &str, prefix: &str) -> bool {
    prefix == "/" || path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// Refusal reasons — no grant ever happens implicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneAuthError {
    InvalidZoneId(String),
    InvalidPath(String),
    CrossZoneRef { own: String, asked: String },
    NoZoneContext(String),
    MissingDelegation(String),
    OutOfScope(String),
    ScopeRequired,
    DelegationInvalid,
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
            Self::MissingDelegation(z) => {
                write!(f, "runtime in zone {z} has no short-lived delegation")
            }
            Self::OutOfScope(path) => write!(f, "resource path {path:?} is outside runtime scope"),
            Self::ScopeRequired => write!(f, "runtime resource scope is required"),
            Self::DelegationInvalid => write!(f, "delegation reference is not server-verified"),
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
