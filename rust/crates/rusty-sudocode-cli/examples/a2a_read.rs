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
use runtime::nexus_mailbox::poll_new;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, endpoint, agent, cert_dir] = args.as_slice() else {
        eprintln!("usage: a2a_read <endpoint> <agent> <cert-dir>");
        std::process::exit(2);
    };

    let read = |name: &str| {
        std::fs::read(std::path::Path::new(cert_dir).join(name))
            .unwrap_or_else(|e| panic!("read {cert_dir}/{name}: {e}"))
    };
    let client = Arc::new(
        NexusVfsClient::connect_tls(
            endpoint,
            read("ca.pem"),
            read("agent.pem"),
            read("agent-key.pem"),
            "nexus-node",
        )
        .unwrap_or_else(|e| panic!("dial {endpoint} over mTLS: {e}")),
    );

    // From the head, non-blocking: everything the inbox holds, right now.
    let (msgs, next) = poll_new(&client, agent, 0, "", 0).unwrap_or_else(|e| panic!("read: {e}"));
    println!(
        "{agent} inbox: {} message(s), next offset {next}",
        msgs.len()
    );
    for (i, m) in msgs.iter().enumerate() {
        println!("  [{i}] from={} :: {}", m.from, m.body);
    }
}
