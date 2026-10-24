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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexus_vfs_client::NexusVfsClient;

/// The node record version this `scode` understands.
const NODE_RECORD_VERSION: u32 = 1;

/// `<home>/.nexus/run/node.json`: where this machine's Nexus node is and where
/// its agent credentials live.
///
/// The node owner publishes it once the node is verified; `scode` only reads
/// it. Its presence is what attaches a machine to a node, so there is no other
/// switch: no environment variable, no working-directory lookup.
#[must_use]
pub fn node_record_path() -> Option<PathBuf> {
    crate::user_paths::home_dir().map(|home| home.join(".nexus").join("run").join("node.json"))
}

/// The fields of the node record `scode` reads. The record also names the
/// node's deployment role and zone, which are the node owner's concern.
#[derive(Debug, serde::Deserialize)]
struct NodeRecord {
    version: u32,
    /// `host:port`, dialled over mTLS.
    endpoint: String,
    /// Absolute directory holding one credential bundle per agent name.
    agents_dir: String,
}

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
    /// malformed.
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

/// How this session reaches its machine's node: where to dial and who it is.
///
/// Built by [`Config::from_node_record`], which answers `Ok(None)` when the
/// machine has no node and fails loud on a record that is present but unusable,
/// never silently degrading to the disk namespace.
#[derive(Debug, Clone)]
pub struct Config {
    /// gRPC target (`host:port`) - where to dial.
    pub endpoint: String,
    /// This agent's own A2A name, from the credential.
    pub agent: String,
    /// mTLS material from the credential; the cluster serves mutual TLS only.
    pub tls: TlsPaths,
}

impl Config {
    /// Resolve this machine's node for `agent` from [`node_record_path`].
    ///
    /// `Ok(None)` when there is no record: the machine has no node. Every other
    /// problem is an error naming the file at fault, because a present record
    /// means the machine has joined a node and the disk namespace is no longer
    /// where its peers are.
    ///
    /// # Errors
    /// The record is unreadable, malformed, of another version, has an endpoint
    /// with a scheme or a relative `agents_dir`; or `agent` has no credential
    /// there, or the credential belongs to another agent.
    pub fn from_node_record(agent: &str) -> Result<Option<Self>, String> {
        let Some(path) = node_record_path() else {
            return Ok(None);
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read node record {}: {error}", path.display())),
        };
        let record: NodeRecord = serde_json::from_str(&raw)
            .map_err(|e| format!("parse node record {}: {e}", path.display()))?;
        if record.version != NODE_RECORD_VERSION {
            return Err(format!(
                "node record {} is version {}; this scode reads version {NODE_RECORD_VERSION}",
                path.display(),
                record.version
            ));
        }
        if record.endpoint.contains("://") {
            return Err(format!(
                "node record {}: endpoint must be host:port without a scheme, got {}",
                path.display(),
                record.endpoint
            ));
        }
        let agents_dir = Path::new(&record.agents_dir);
        if !agents_dir.is_absolute() {
            return Err(format!(
                "node record {}: agents_dir must be an absolute path, got {}",
                path.display(),
                record.agents_dir
            ));
        }
        let bundle = agents_dir.join(agent);
        if !bundle.join(AgentCredential::MANIFEST).is_file() {
            return Err(format!(
                "this machine's node has no credential for agent '{agent}' at {}; \
                 ask the node owner to mint '{agent}'",
                bundle.display()
            ));
        }
        let credential = AgentCredential::load(&bundle.to_string_lossy())?;
        if credential.agent != agent {
            return Err(format!(
                "credential at {} is for agent '{}', but this session is '{agent}'",
                bundle.display(),
                credential.agent
            ));
        }
        Ok(Some(Self {
            endpoint: record.endpoint,
            agent: credential.agent,
            tls: credential.tls,
        }))
    }
    /// Construct a shared lazy client using this agent's mTLS material.
    /// Construction does not establish node reachability or authentication;
    /// actual IO reports those failures. The node derives the authenticated
    /// identity from the certificate's SAN, so no request token is substituted.
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
        peer_system_prompt(&self.agent)
    }
}

/// Stable node-namespace identity text, even before this agent has a credential.
#[must_use]
pub fn peer_system_prompt(agent: &str) -> String {
    let base = crate::agent_mailbox::repl_a2a_prompt_section(agent);
    format!(
        "{base}\n\nThis conversation is on a nexus A2A network, so a peer may \
         be on another machine; addressing it by name still reaches it."
    )
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
    fn peer_prompt_names_self_and_points_at_discovery() {
        let cfg = Config {
            endpoint: "127.0.0.1:2126".into(),
            agent: "operator".into(),
            tls: test_tls(),
        };
        let p = cfg.peer_system_prompt();
        assert!(p.contains("\"operator\""), "prompt must name self: {p}");
        assert!(p.contains("send"), "prompt must teach the tool: {p}");
        assert!(
            p.contains("agent_list"),
            "prompt must point at agent_list for discovery: {p}"
        );
        assert!(
            p.contains("another machine"),
            "prompt must carry the nexus network note: {p}"
        );
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
