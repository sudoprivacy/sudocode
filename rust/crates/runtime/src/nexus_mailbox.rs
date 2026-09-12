//! Nexus-backed A2A mailbox — the standalone counterpart to the local
//! [`crate::agent_mailbox`] (`.sudocode-inbox`).
//!
//! A message is one framed append to the recipient's replicated DT_STREAM
//! inbox at `/agents/<recipient>/chat-with-me`; under auth-on the node
//! stamps an unforgeable `from` by the authenticated writer. The wire type
//! is [`a2a::MailboxEnvelope`] — the A2A SSOT the in-process co-host
//! ([`crate::spawn_task`]) also uses — so a standalone `scode` and a co-host
//! agent interoperate over the very same stream, byte-identically, BY
//! CONSTRUCTION (one envelope type, not two that "happen to match").
//!
//! Transport only: send is a `stream_write`, poll is a cursor-advancing
//! `stream_read_at` loop. Cursor persistence + REPL surfacing live in the
//! caller (the cursor is ephemeral read position, never persisted here).

use std::sync::Arc;

use nexus_vfs_client::NexusVfsClient;

/// gRPC target of the nexus daemon this `scode` dials (`host:port`).
/// Its presence is the sole enable switch for standalone A2A.
pub const ENDPOINT_ENV: &str = "NEXUS_A2A_ENDPOINT";
/// This `scode`'s own A2A name — the inbox it polls and the advisory
/// `from` it writes (the node stamps the authenticated identity).
pub const AGENT_ENV: &str = "NEXUS_A2A_AGENT";
/// Comma-separated peer names, surfaced to the model in the system prompt
/// so it knows who it can address (advisory — any name is dialable).
pub const PEERS_ENV: &str = "NEXUS_A2A_PEER";
/// `sk-` token sent as the per-request `auth_token`; under auth-on the
/// daemon derives the stamped `from` from it. Shared name with the nexus
/// runbook helper `_open_stub` (DRY operator contract).
pub const API_KEY_ENV: &str = "NEXUS_API_KEY";
/// Path to the cluster CA PEM. Presence upgrades the dial to mTLS —
/// same gate as `_open_stub`.
pub const CA_PEM_ENV: &str = "NEXUS_CA_PEM";
/// Path to the client cert PEM (mandatory under mTLS — the cluster serves
/// MUTUAL TLS). Shared name with `_open_stub`.
pub const CLIENT_CERT_ENV: &str = "NEXUS_CLIENT_CERT";
/// Path to the client key PEM (mandatory under mTLS). Shared name with
/// `_open_stub`.
pub const CLIENT_KEY_ENV: &str = "NEXUS_CLIENT_KEY";
/// TLS SAN to validate the server against (the cluster's fixed cert name,
/// not the dialed host/IP). Shared name + default with `_open_stub`.
pub const TLS_SERVER_NAME_ENV: &str = "NEXUS_TLS_SERVER_NAME";
/// Default SAN of the cluster server cert (see [`TLS_SERVER_NAME_ENV`]).
pub const DEFAULT_TLS_SERVER_NAME: &str = "nexus-node";

/// Resolved TLS material paths for an mTLS dial (all three mandatory —
/// the cluster serves mutual TLS, so a CA alone cannot authenticate the
/// transport).
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub ca_pem: String,
    pub client_cert: String,
    pub client_key: String,
    pub server_name: String,
}

/// Standalone nexus-A2A configuration, resolved from the environment.
///
/// Built by [`Config::from_env`], which returns `Ok(None)` when the
/// feature is off (no [`ENDPOINT_ENV`]) — the fast path that leaves
/// `scode` behaviour unchanged — and fails loud on any *partial*
/// configuration (endpoint without self-name, or a CA without the client
/// cert/key mTLS mandates), never silently degrading.
#[derive(Debug, Clone)]
pub struct Config {
    /// gRPC target (`host:port`).
    pub endpoint: String,
    /// This agent's own A2A name.
    pub agent: String,
    /// Known peer names (advisory prompt hint).
    pub peers: Vec<String>,
    /// `sk-` auth token (empty under auth-off loopback).
    pub api_key: String,
    /// mTLS material, or `None` for a plaintext (loopback) dial.
    pub tls: Option<TlsPaths>,
}

impl Config {
    /// Resolve from the environment.
    ///
    /// # Errors
    /// Returns a message when the configuration is *partial*: an endpoint
    /// with no [`AGENT_ENV`], or a [`CA_PEM_ENV`] without both
    /// [`CLIENT_CERT_ENV`] and [`CLIENT_KEY_ENV`].
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(endpoint) = non_empty_env(ENDPOINT_ENV) else {
            return Ok(None);
        };
        let agent = non_empty_env(AGENT_ENV).ok_or_else(|| {
            format!("{ENDPOINT_ENV} is set but {AGENT_ENV} (this agent's A2A name) is not")
        })?;
        let peers = non_empty_env(PEERS_ENV)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let api_key = non_empty_env(API_KEY_ENV).unwrap_or_default();
        let tls = match non_empty_env(CA_PEM_ENV) {
            None => None,
            Some(ca_pem) => {
                let client_cert = non_empty_env(CLIENT_CERT_ENV).ok_or_else(|| {
                    format!("{CA_PEM_ENV} is set (mTLS) but {CLIENT_CERT_ENV} is not")
                })?;
                let client_key = non_empty_env(CLIENT_KEY_ENV).ok_or_else(|| {
                    format!("{CA_PEM_ENV} is set (mTLS) but {CLIENT_KEY_ENV} is not")
                })?;
                Some(TlsPaths {
                    ca_pem,
                    client_cert,
                    client_key,
                    server_name: non_empty_env(TLS_SERVER_NAME_ENV)
                        .unwrap_or_else(|| DEFAULT_TLS_SERVER_NAME.to_string()),
                })
            }
        };
        Ok(Some(Self {
            endpoint,
            agent,
            peers,
            api_key,
            tls,
        }))
    }

    /// Dial the daemon, returning a shared client. mTLS when [`Config::tls`]
    /// is set, else plaintext (loopback / auth-off).
    ///
    /// # Errors
    /// Returns a message if a PEM file cannot be read or the channel fails
    /// to construct.
    pub fn connect(&self) -> Result<Arc<NexusVfsClient>, String> {
        let client = match &self.tls {
            None => NexusVfsClient::connect(&self.endpoint)
                .map_err(|e| format!("dial {}: {e}", self.endpoint))?,
            Some(t) => {
                let ca = std::fs::read(&t.ca_pem)
                    .map_err(|e| format!("read {CA_PEM_ENV} {}: {e}", t.ca_pem))?;
                let cert = std::fs::read(&t.client_cert)
                    .map_err(|e| format!("read {CLIENT_CERT_ENV} {}: {e}", t.client_cert))?;
                let key = std::fs::read(&t.client_key)
                    .map_err(|e| format!("read {CLIENT_KEY_ENV} {}: {e}", t.client_key))?;
                NexusVfsClient::connect_tls(&self.endpoint, ca, cert, key, &t.server_name)
                    .map_err(|e| format!("mTLS dial {}: {e}", self.endpoint))?
            }
        };
        Ok(Arc::new(client))
    }

    /// System-prompt section telling the model its A2A identity and how to
    /// reach peers. Derived purely from config, so it stays the single
    /// source for the peer-awareness text.
    #[must_use]
    pub fn peer_system_prompt(&self) -> String {
        let mut s = format!(
            "## Agent-to-agent messaging\n\nYou are reachable on a nexus A2A network as the agent \"{}\". \
             To message another agent, call the `send` tool with a JSON object \
             {{\"to\": \"<agent name>\", \"message\": \"<your message>\"}}. \
             A successful send returns `message delivered to <agent name>`; any other result \
             means the message did NOT leave this machine — say so rather than reporting success. \
             Messages other agents send you are delivered into this conversation as they arrive.",
            self.agent
        );
        if !self.peers.is_empty() {
            s.push_str(&format!(
                "\n\nKnown peers you can address: {}.",
                self.peers.join(", ")
            ));
        }
        s
    }
}

/// Read an env var, treating unset OR empty/whitespace as absent.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
