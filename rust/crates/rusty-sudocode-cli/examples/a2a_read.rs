//! Dump an agent's whole inbox from offset 0, over mTLS.
//!
//! The receiver a `scode` client runs is deliberately forward-only — it
//! resumes from where that client last read, and a brand-new client seeks to
//! the tail. Neither ever looks backwards. So when a message is known to have
//! been delivered but nobody was there to see it, this is how you read it: the
//! frames are durable in the DT_STREAM, it is only the client cursors that
//! moved past them.
//!
//! ```text
//! cargo run -p rusty-sudocode-cli --example a2a_read -- <endpoint> <agent> <cert-dir>
//! ```

use std::sync::Arc;

use nexus_vfs_client::NexusVfsClient;
use runtime::mailbox::Mailbox;

/// The unified mailbox for `agent` — the same transport a running agent uses.
fn mailbox(client: &Arc<NexusVfsClient>, agent: &str, auth: &str) -> Mailbox {
    Mailbox::over_nexus(Arc::clone(client), agent, auth)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, endpoint, agent, cert_dir] = args.as_slice() else {
        eprintln!("usage: a2a_read <endpoint> <agent> <cert-dir>");
        std::process::exit(2);
    };

    // The same dial a running `scode` performs: one constructor, which names
    // the bundle layout and the server SAN once and reports which PEM failed.
    let client = runtime::nexus_mailbox::Config {
        endpoint: endpoint.clone(),
        agent: agent.clone(),
        peers: Vec::new(),
        api_key: String::new(),
        tls: Some(runtime::nexus_mailbox::TlsPaths::from_bundle_dir(cert_dir)),
    }
    .connect()
    .unwrap_or_else(|e| panic!("{e}"));

    // From the head, non-blocking: everything the inbox holds, right now.
    let (msgs, next) = mailbox(&client, agent, "")
        .poll(0, 0)
        .unwrap_or_else(|e| panic!("read: {e}"));
    println!(
        "{agent} inbox: {} message(s), next offset {next}",
        msgs.len()
    );
    for (i, m) in msgs.iter().enumerate() {
        println!("  [{i}] from={} :: {}", m.from, m.body);
    }
}
