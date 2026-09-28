//! Nexus-backed A2A mailbox - the standalone counterpart to the local
//! [`crate::agent_mailbox`] (`.sudocode-inbox`).
//!
//! A message is one framed append to the recipient's replicated DT_STREAM
//! inbox at `/agents/<recipient>/chat-with-me`; under auth-on the node
//! stamps an unforgeable `from` by the authenticated writer. The wire type
//! is [`a2a::MailboxEnvelope`] - the A2A SSOT the in-process co-host
//! ([`crate::spawn_task`]) also uses - so a standalone `scode` and a co-host
//! agent interoperate over the very same stream, byte-identically, BY
//! CONSTRUCTION (one envelope type, not two that "happen to match").
//!
//! Transport only: send is a `stream_write`, poll is a cursor-advancing
//! `stream_read_at` loop. Cursor persistence + REPL surfacing live in the
//! caller (the cursor is ephemeral read position, never persisted here).

use std::sync::Arc;

use nexus_vfs_client::NexusVfsClient;

/// gRPC target of the nexus daemon this `scode` dials (`host:port`).
/// Its presence is the sole enable switch for standalone A2A. Topology, not
/// identity: a credential can be carried between endpoints, so where to dial is
/// always a separate input from who you are.
pub const ENDPOINT_ENV: &str = "NEXUS_A2A_ENDPOINT";
/// Path to the minted agent credential - the directory `nexusd-cluster auth
/// mint` writes (or its `credential.json` manifest). It carries this agent's
/// identity (name + mTLS material), so it is the single "who you are" input.
pub const CREDENTIAL_ENV: &str = "NEXUS_A2A_CREDENTIAL";
/// Comma-separated peer names, surfaced to the model in the system prompt so it
/// knows who it can address (advisory - any name is dialable, and discovery
/// finds the rest).
pub const PEERS_ENV: &str = "NEXUS_A2A_PEER";

/// Resolved TLS material paths for an mTLS dial (all three mandatory - the
/// cluster serves mutual TLS, so a CA alone cannot authenticate the transport).
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub ca_pem: String,
    pub client_cert: String,
    pub client_key: String,
    pub server_name: String,
}

/// A minted agent credential: the identity + mTLS material scode dials with.
///
/// `nexusd-cluster auth mint` writes one directory holding the PEMs plus a
/// `credential.json` manifest. The manifest is the source of truth for what the
/// bundle contains - the agent's name, the TLS server name to verify, and the
/// relative filenames of the three PEMs - so scode reads it rather than
/// re-spelling a filename layout that only the mint actually decides.
#[derive(Debug, Clone)]
pub struct AgentCredential {
    /// This agent's A2A name (the manifest records it; the node ultimately
    /// trusts the cert's SAN, so this is the same identity either way).
    pub agent: String,
    /// mTLS material resolved to absolute paths under the bundle dir.
    pub tls: TlsPaths,
}

/// On-disk shape of `credential.json` (`AgentCredential`, version 1). The `ca`,
/// `cert` and `key` fields are filenames relative to the bundle directory.
#[derive(Debug, serde::Deserialize)]
struct CredentialManifest {
    agent: String,
    server_name: String,
    ca: String,
    cert: String,
    key: String,
}

impl AgentCredential {
    /// Manifest filename inside a bundle directory.
    const MANIFEST: &'static str = "credential.json";

    /// Load a credential from its bundle directory or its `credential.json`
    /// path directly (the mint prints either; an operator copies whichever).
    ///
    /// # Errors
    /// Returns a message naming the path when the manifest is missing or
    /// malformed - a wrong `NEXUS_A2A_CREDENTIAL` reports itself.
    pub fn load(path: &str) -> Result<Self, String> {
        let p = std::path::Path::new(path);
        let (manifest_path, dir) = if p.is_dir() {
            (p.join(Self::MANIFEST), p.to_path_buf())
        } else {
            let dir = p
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf();
            (p.to_path_buf(), dir)
        };
        let raw = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("read credential manifest {}: {e}", manifest_path.display()))?;
        let manifest: CredentialManifest = serde_json::from_str(&raw)
            .map_err(|e| format!("parse credential manifest {}: {e}", manifest_path.display()))?;
        let join = |name: &str| dir.join(name).to_string_lossy().into_owned();
        Ok(Self {
            agent: manifest.agent,
            tls: TlsPaths {
                ca_pem: join(&manifest.ca),
                client_cert: join(&manifest.cert),
                client_key: join(&manifest.key),
                server_name: manifest.server_name,
            },
        })
    }
}

/// Standalone nexus-A2A configuration, resolved from the environment.
///
/// Built by [`Config::from_env`], which returns `Ok(None)` when the feature is
/// off (no [`ENDPOINT_ENV`]) - the fast path that leaves `scode` behaviour
/// unchanged - and fails loud on a partial configuration (an endpoint without a
/// credential, or a credential that cannot be loaded), never silently degrading.
#[derive(Debug, Clone)]
pub struct Config {
    /// gRPC target (`host:port`) - where to dial.
    pub endpoint: String,
    /// This agent's own A2A name, from the credential.
    pub agent: String,
    /// Known peer names (advisory prompt hint; discovery finds the rest).
    pub peers: Vec<String>,
    /// mTLS material. `from_env` always fills this from the credential; `None`
    /// is only the plaintext loopback dial an auth-off dev cluster allows (used
    /// by live tests), never a silent downgrade of a credentialed session.
    pub tls: TlsPaths,
}

impl Config {
    /// Resolve from the environment.
    ///
    /// `Ok(None)` when the feature is off (no [`ENDPOINT_ENV`]). Fails loud when
    /// an endpoint is set without a [`CREDENTIAL_ENV`], or when the credential
    /// cannot be loaded - never a silent degrade.
    ///
    /// # Errors
    /// A message when the configuration is partial (endpoint without a
    /// credential) or the credential path is missing/malformed.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(endpoint) = non_empty_env(ENDPOINT_ENV) else {
            return Ok(None);
        };
        let credential_path = non_empty_env(CREDENTIAL_ENV).ok_or_else(|| {
            format!(
                "{ENDPOINT_ENV} is set but {CREDENTIAL_ENV} (the minted agent credential) is not"
            )
        })?;
        let credential = AgentCredential::load(&credential_path)?;
        let peers = non_empty_env(PEERS_ENV)
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(Self {
            endpoint,
            agent: credential.agent,
            peers,
            tls: credential.tls,
        }))
    }

    /// Dial the daemon, returning a shared client. mTLS with the credential's
    /// material when [`Config::tls`] is set (the cert is the whole
    /// authorization - the node derives identity from its SAN, so no token is
    /// sent).
    ///
    /// # Errors
    /// Returns a message if a PEM file cannot be read or the channel fails
    /// to construct.
    pub fn connect(&self) -> Result<Arc<NexusVfsClient>, String> {
        let t = &self.tls;
        let ca = std::fs::read(&t.ca_pem)
            .map_err(|e| format!("read credential CA {}: {e}", t.ca_pem))?;
        let cert = std::fs::read(&t.client_cert)
            .map_err(|e| format!("read credential cert {}: {e}", t.client_cert))?;
        let key = std::fs::read(&t.client_key)
            .map_err(|e| format!("read credential key {}: {e}", t.client_key))?;
        let client = NexusVfsClient::connect_tls(&self.endpoint, ca, cert, key, &t.server_name)
            .map_err(|e| format!("mTLS dial {}: {e}", self.endpoint))?;
        Ok(Arc::new(client))
    }

    /// System-prompt section telling the model its A2A identity and how to
    /// reach peers. Delegates the reply contract + framing to the shared
    /// [`crate::agent_mailbox::repl_a2a_prompt_section`] (the SSOT every REPL
    /// receive path renders) and prepends the nexus-specific network note.
    #[must_use]
    pub fn peer_system_prompt(&self) -> String {
        let base = crate::agent_mailbox::repl_a2a_prompt_section(&self.agent, &self.peers);
        format!(
            "{base}\n\nThis conversation is on a nexus A2A network, so a peer may \
             be on another machine; addressing it by name still reaches it."
        )
    }
}

/// Read an env var, treating unset OR empty/whitespace as absent.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test-only: the module's own code no longer touches envelopes - the
    // transport that did moved to `crate::mailbox`. These tests stay because
    // they pin the WIRE FORMAT, which both transports share.
    use crate::agent_mailbox::MailboxEnvelope;

    /// Write a minimal minted-bundle dir (manifest + three PEM files) and return
    /// its path - the shape `nexusd-cluster auth mint` produces.
    fn write_test_bundle(agent: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "nexus-cred-{agent}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["ca.pem", "agent.pem", "agent-key.pem"] {
            std::fs::write(dir.join(f), b"-----TEST PEM-----").unwrap();
        }
        std::fs::write(
            dir.join("credential.json"),
            format!(
                r#"{{"version":1,"agent":"{agent}","server_name":"nexus-node","ca":"ca.pem","cert":"agent.pem","key":"agent-key.pem"}}"#
            ),
        )
        .unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// A throwaway `TlsPaths` for prompt tests that never dial.
    fn test_tls() -> TlsPaths {
        TlsPaths {
            ca_pem: "ca.pem".into(),
            client_cert: "agent.pem".into(),
            client_key: "agent-key.pem".into(),
            server_name: "nexus-node".into(),
        }
    }

    #[test]
    fn from_env_off_partial_and_full() {
        // All env cases run in ONE test fn: these NEXUS_A2A_* vars are read by
        // no other test, so sequential mutation here is race-free even under
        // the parallel harness.
        let clear = || {
            std::env::remove_var(ENDPOINT_ENV);
            std::env::remove_var(CREDENTIAL_ENV);
            std::env::remove_var(PEERS_ENV);
        };

        clear();
        // Off: no endpoint -> the fast Ok(None) path.
        assert!(Config::from_env().unwrap().is_none());

        // Partial: endpoint set but no credential -> fail loud.
        std::env::set_var(ENDPOINT_ENV, "127.0.0.1:2126");
        assert!(Config::from_env().is_err());

        // A missing credential path -> fail loud (names the path).
        std::env::set_var(CREDENTIAL_ENV, "/no/such/bundle");
        assert!(Config::from_env().is_err());

        // Full: a real bundle dir (manifest + PEMs) resolves name + TLS.
        let bundle = write_test_bundle("operator");
        std::env::set_var(CREDENTIAL_ENV, &bundle);
        std::env::set_var(PEERS_ENV, "win-ai, mac-ai");
        let cfg = Config::from_env().unwrap().unwrap();
        assert_eq!(cfg.agent, "operator");
        assert_eq!(cfg.peers, vec!["win-ai".to_string(), "mac-ai".to_string()]);
        let tls = cfg.tls;
        assert_eq!(tls.server_name, "nexus-node");
        assert!(tls.ca_pem.ends_with("ca.pem"));
        assert!(tls.client_cert.ends_with("agent.pem"));

        clear();
        let _ = std::fs::remove_dir_all(&bundle);
    }

    #[test]
    fn load_credential_from_manifest_path_or_dir() {
        let bundle = write_test_bundle("mac-ai");
        // Directory path.
        let by_dir = AgentCredential::load(&bundle).unwrap();
        assert_eq!(by_dir.agent, "mac-ai");
        assert_eq!(by_dir.tls.server_name, "nexus-node");
        // Manifest file path directly.
        let manifest = std::path::Path::new(&bundle).join("credential.json");
        let by_file = AgentCredential::load(&manifest.to_string_lossy()).unwrap();
        assert_eq!(by_file.agent, "mac-ai");
        assert_eq!(by_file.tls.client_key, by_dir.tls.client_key);
        // A malformed manifest fails loud.
        std::fs::write(&manifest, b"{ not json").unwrap();
        assert!(AgentCredential::load(&bundle).is_err());
        let _ = std::fs::remove_dir_all(&bundle);
    }

    #[test]
    fn peer_prompt_names_self_and_lists_known_peers() {
        let cfg = Config {
            endpoint: "127.0.0.1:2126".into(),
            agent: "operator".into(),
            peers: vec!["win-ai".into(), "mac-ai".into()],
            tls: test_tls(),
        };
        let p = cfg.peer_system_prompt();
        assert!(p.contains("\"operator\""), "prompt must name self: {p}");
        assert!(p.contains("send"), "prompt must teach the tool: {p}");
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
            tls: test_tls(),
        };
        let p = cfg.peer_system_prompt();
        assert!(p.contains("\"operator\""));
        assert!(!p.contains("Known peers"), "no peer line when empty: {p}");
    }

    #[test]
    fn envelope_round_trips_via_unified_type() {
        let env = MailboxEnvelope {
            from: "operator".into(),
            to: "win-ai".into(),
            body: "hi".into(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        };
        let back = MailboxEnvelope::from_bytes(&env.to_bytes()).expect("envelope round-trip");
        assert_eq!(back.from, "operator");
        assert_eq!(back.body, "hi");
    }

    #[test]
    fn unified_envelope_interops_with_a2a_3field_wire() {
        // A 3-field JSON written by an old a2a::MailboxEnvelope writer
        // must deserialise into the unified type with extras defaulted.
        let wire = br#"{"from":"agent-a","to":"agent-b","body":"hello"}"#;
        let env = MailboxEnvelope::from_bytes(wire).expect("3-field wire compat");
        assert_eq!(env.from, "agent-a");
        assert_eq!(env.body, "hello");
        assert!(env.kind.is_empty());
        assert_eq!(env.timestamp, 0);
    }
}
