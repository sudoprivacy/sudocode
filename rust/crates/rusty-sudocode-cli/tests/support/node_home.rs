//! A machine attached to a Nexus node, as a spawned `scode` sees it.
//!
//! `scode` finds its node only through `<home>/.nexus/run/node.json` and picks
//! its credential from the record's `agents_dir` by its own agent name. A test
//! gives each child process its own home holding such a record, and pins the
//! child's agent name in its workspace, so the child attaches exactly the way a
//! session on a real machine does: no environment switch, no shortcut.
//!
//! Included with `#[path = "support/node_home.rs"] mod node_home;`. Each test
//! binary that includes it uses only some of these helpers.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A temporary home whose node record points at `endpoint` and whose
/// `agents_dir` holds one credential bundle. Removed on drop.
pub struct NodeHome {
    root: PathBuf,
}

impl NodeHome {
    /// Publish a node record for `endpoint` and lay out `agent`'s credential,
    /// copied from the minted bundle directory `bundle`.
    ///
    /// `endpoint` may carry a scheme (the harness dials `https://host:port`);
    /// the record holds `host:port`, which is what a node owner publishes.
    pub fn new(label: &str, endpoint: &str, agent: &str, bundle: &Path) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "scode-node-home-{label}-{}-{nanos}",
            std::process::id()
        ));
        let agents_dir = root.join("agents");
        let credential = agents_dir.join(agent);
        std::fs::create_dir_all(&credential).expect("create the credential directory");
        for entry in std::fs::read_dir(bundle)
            .unwrap_or_else(|e| panic!("read the minted bundle {}: {e}", bundle.display()))
        {
            let entry = entry.expect("read a bundle entry");
            if entry.file_type().expect("bundle entry type").is_file() {
                std::fs::copy(entry.path(), credential.join(entry.file_name()))
                    .expect("copy a bundle file");
            }
        }
        let run = root.join(".nexus").join("run");
        std::fs::create_dir_all(&run).expect("create the node record directory");
        let host_port = endpoint
            .split_once("://")
            .map_or(endpoint, |(_, rest)| rest);
        let record = serde_json::json!({
            "version": 1,
            "endpoint": host_port,
            "agents_dir": agents_dir.to_string_lossy(),
            "role": "founder",
            "zone": "sharedzone",
        });
        std::fs::write(run.join("node.json"), record.to_string()).expect("write node.json");
        Self { root }
    }

    /// The child's `HOME` and `USERPROFILE`, so it resolves this record.
    #[must_use]
    pub fn env(&self) -> [(&'static str, String); 2] {
        let home = self.root.to_string_lossy().into_owned();
        [("HOME", home.clone()), ("USERPROFILE", home)]
    }
}

impl Drop for NodeHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Write a credential bundle for `agent` under `agents_dir` whose PEMs parse
/// but are trusted by no node: a fresh self-signed certificate serves as CA and
/// client certificate alike. It gets a session as far as dialling, which is
/// all a test of an unreachable node needs.
pub fn write_untrusted_credential(agents_dir: &Path, agent: &str) {
    let bundle = agents_dir.join(agent);
    std::fs::create_dir_all(&bundle).expect("create the credential directory");
    let issued = rcgen::generate_simple_self_signed(vec![agent.to_string()])
        .expect("generate a self-signed certificate");
    let cert = issued.cert.pem();
    std::fs::write(bundle.join("ca.pem"), &cert).expect("write ca.pem");
    std::fs::write(bundle.join("agent.pem"), &cert).expect("write agent.pem");
    std::fs::write(
        bundle.join("agent-key.pem"),
        issued.signing_key.serialize_pem(),
    )
    .expect("write agent-key.pem");
    let manifest = serde_json::json!({
        "version": 1,
        "agent": agent,
        "server_name": "nexus-node",
        "ca": "ca.pem",
        "cert": "agent.pem",
        "key": "agent-key.pem",
    });
    std::fs::write(bundle.join("credential.json"), manifest.to_string())
        .expect("write credential.json");
}

/// Inspect the complete private test tree, including its HOME and config home.
/// Do not follow symlinks out of the fixture into unrelated workspaces.
pub fn assert_no_disk_mailboxes_below(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("inspect the private fixture tree") {
            let entry = entry.expect("inspect a fixture entry");
            let name = entry.file_name();
            assert!(
                name != ".sudocode-inbox" && name != "local-mailbox",
                "unexpected disk mailbox at {}",
                entry.path().display()
            );
            if entry.file_type().expect("fixture entry type").is_dir() {
                pending.push(entry.path());
            }
        }
    }
}

/// Pin the agent name a session started in `workspace` answers to.
///
/// On a node the session's identity is its own resolved name, and the node
/// record's credential must be minted for exactly that name.
pub fn pin_agent_name(workspace: &Path, agent: &str) {
    let dir = workspace.join(".nexus").join("sudocode");
    std::fs::create_dir_all(&dir).expect("create the project settings directory");
    let path = dir.join("settings.json");
    let mut settings = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["agentName"] = serde_json::Value::String(agent.to_string());
    std::fs::write(&path, settings.to_string()).expect("write the project settings");
}
