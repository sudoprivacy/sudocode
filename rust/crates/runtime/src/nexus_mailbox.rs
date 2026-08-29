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

use a2a::MailboxEnvelope;
use nexus_vfs_client::NexusVfsClient;

use crate::spawn_task::MailboxSender;

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
             To message another agent, call the `send_message` tool with a JSON object \
             {{\"to\": \"<agent name>\", \"body\": \"<your message>\"}}. \
             Messages other agents send you are delivered into this conversation as they arrive.",
            self.agent
        );
        if !self.peers.is_empty() {
            s.push_str(&format!("\n\nKnown peers you can address: {}.", self.peers.join(", ")));
        }
        s
    }
}

/// Read an env var, treating unset OR empty/whitespace as absent.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// VFS path of an agent's replicated A2A inbox.
#[must_use]
pub fn inbox_path(agent: &str) -> String {
    format!("/agents/{agent}/chat-with-me")
}

/// Append a message to `to`'s inbox and return the offset it landed at.
///
/// The `from` we write is advisory: under auth-on the daemon's
/// `MailboxStampingHook` overwrites it with the authenticated caller's
/// identity (so it cannot be forged); under auth-off it is used as-is.
///
/// # Errors
/// Returns a `String` error if the `stream_write` RPC fails.
pub fn send(
    client: &NexusVfsClient,
    from: &str,
    to: &str,
    body: &str,
    auth_token: &str,
) -> Result<u64, String> {
    let env = MailboxEnvelope {
        from: from.to_string(),
        to: to.to_string(),
        body: body.to_string(),
    };
    let path = inbox_path(to);
    client
        .stream_write(&path, env.to_bytes(), auth_token)
        .map_err(|e| format!("A2A stream_write to {path}: {e}"))
}

/// Build a [`MailboxSender`] backed by a gRPC [`NexusVfsClient`] — the
/// standalone counterpart to [`crate::spawn_task::mailbox_sender`] (which is
/// backed by the in-process kernel). Both feed the SAME shared
/// [`crate::spawn_task::handle_send_message`], so co-host and standalone
/// `send_message` share every line except this transport closure.
///
/// `from` is the standalone agent's own name (advisory — the node stamps the
/// authenticated identity under auth-on). `client` is shared (constructed
/// once at startup, held by the CLI executor).
#[must_use]
pub fn grpc_sender(client: Arc<NexusVfsClient>, from: String, auth_token: String) -> MailboxSender {
    Arc::new(move |to: &str, body: &str| send(&client, &from, to, body, &auth_token).map(|_| ()))
}

/// One inbound A2A message (self-writes and empty bodies already filtered).
#[derive(Debug, Clone)]
pub struct Inbound {
    pub from: String,
    pub body: String,
}

/// Drain every frame in `self_agent`'s inbox from `cursor` forward
/// (non-blocking — stops at the first `eof`). Returns the new inbound
/// messages and the advanced cursor to persist for the next poll.
///
/// Skips our OWN writes (`from == self_agent`) so a shared read/write stream
/// never echoes to us, and skips senderless / empty-body frames — the same
/// filter the co-host loop applies in `parse_inbound`.
///
/// # Errors
/// Returns a `String` error if a `stream_read_at` RPC fails.
pub fn poll_new(
    client: &NexusVfsClient,
    self_agent: &str,
    mut cursor: u64,
    auth_token: &str,
) -> Result<(Vec<Inbound>, u64), String> {
    let path = inbox_path(self_agent);
    let mut out = Vec::new();
    loop {
        let (data, next, eof) = client
            .stream_read_at(&path, cursor, auth_token)
            .map_err(|e| format!("A2A stream_read_at {path}@{cursor}: {e}"))?;
        if eof {
            break;
        }
        if let Some(env) = MailboxEnvelope::from_bytes(&data) {
            if !env.from.is_empty() && env.from != self_agent && !env.body.is_empty() {
                out.push(Inbound {
                    from: env.from,
                    body: env.body,
                });
            }
        }
        if next <= cursor {
            // No forward progress — guard against an infinite loop on a
            // stream that returns the same offset (a buggy server must not
            // wedge the poller).
            break;
        }
        cursor = next;
    }
    Ok((out, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_path_is_the_a2a_mailbox() {
        assert_eq!(inbox_path("win-ai"), "/agents/win-ai/chat-with-me");
    }

    #[test]
    fn from_env_off_partial_and_full() {
        // All env cases run in ONE test fn: these NEXUS_A2A_* / NEXUS_* vars
        // are read by no other test, so sequential mutation here is race-free
        // even under the parallel test harness.
        let keys = [
            ENDPOINT_ENV,
            AGENT_ENV,
            PEERS_ENV,
            API_KEY_ENV,
            CA_PEM_ENV,
            CLIENT_CERT_ENV,
            CLIENT_KEY_ENV,
            TLS_SERVER_NAME_ENV,
        ];
        let clear = || {
            for k in keys {
                std::env::remove_var(k);
            }
        };

        clear();
        // Off: no endpoint -> the fast Ok(None) path.
        assert!(Config::from_env().unwrap().is_none());

        // Partial: endpoint set but no self-name -> fail loud.
        std::env::set_var(ENDPOINT_ENV, "127.0.0.1:2126");
        assert!(Config::from_env().is_err());

        // Enabled plaintext (loopback / auth-off): no TLS.
        std::env::set_var(AGENT_ENV, "operator");
        std::env::set_var(PEERS_ENV, "win-ai, mac-ai");
        let cfg = Config::from_env().unwrap().unwrap();
        assert_eq!(cfg.agent, "operator");
        assert_eq!(cfg.peers, vec!["win-ai".to_string(), "mac-ai".to_string()]);
        assert!(cfg.tls.is_none());

        // Partial mTLS: a CA without the client cert/key the cluster's mutual
        // TLS mandates -> fail loud (never a silent server-auth-only downgrade).
        std::env::set_var(CA_PEM_ENV, "/tmp/ca.pem");
        assert!(Config::from_env().is_err());
        std::env::set_var(CLIENT_CERT_ENV, "/tmp/client.pem");
        assert!(Config::from_env().is_err());

        // Full mTLS: server name defaults to the cluster SAN.
        std::env::set_var(CLIENT_KEY_ENV, "/tmp/client.key");
        let tls = Config::from_env().unwrap().unwrap().tls.unwrap();
        assert_eq!(tls.ca_pem, "/tmp/ca.pem");
        assert_eq!(tls.server_name, DEFAULT_TLS_SERVER_NAME);

        clear();
    }

    #[test]
    fn peer_prompt_names_self_and_lists_known_peers() {
        let cfg = Config {
            endpoint: "127.0.0.1:2126".into(),
            agent: "operator".into(),
            peers: vec!["win-ai".into(), "mac-ai".into()],
            api_key: String::new(),
            tls: None,
        };
        let p = cfg.peer_system_prompt();
        assert!(p.contains("\"operator\""), "prompt must name self: {p}");
        assert!(p.contains("send_message"), "prompt must teach the tool: {p}");
        assert!(
            p.contains("win-ai, mac-ai"),
            "prompt must list known peers: {p}"
        );
    }

    #[test]
    fn peer_prompt_omits_peer_list_when_none_known() {
        let cfg = Config {
            endpoint: "127.0.0.1:2126".into(),
            agent: "operator".into(),
            peers: vec![],
            api_key: String::new(),
            tls: None,
        };
        let p = cfg.peer_system_prompt();
        assert!(p.contains("\"operator\""));
        assert!(!p.contains("Known peers"), "no peer line when empty: {p}");
    }

    #[test]
    fn envelope_round_trips_via_the_a2a_ssot_type() {
        // We reuse the a2a-crate envelope (the SSOT the co-host writes), so a
        // round-trip through the same to_bytes/from_bytes the co-host uses
        // must recover from/body — this is what makes standalone ⇄ co-host
        // interop byte-identical by construction.
        let env = MailboxEnvelope {
            from: "operator".into(),
            to: "win-ai".into(),
            body: "hi".into(),
        };
        let back = MailboxEnvelope::from_bytes(&env.to_bytes()).expect("a2a envelope round-trip");
        assert_eq!(back.from, "operator");
        assert_eq!(back.body, "hi");
    }
}
