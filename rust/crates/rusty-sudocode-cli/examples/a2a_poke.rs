//! Write one A2A envelope into an agent's inbox over mTLS, then exit.
//!
//! A stand-in for the far end of a duet while the other machine is still
//! building: it proves the auth-on receive path — dial, stamp, deliver — with
//! nothing but a cert, so the first real cross-machine attempt is not also the
//! first time this chain has ever run.
//!
//! ```text
//! cargo run -p rusty-sudocode-cli --example a2a_poke -- \
//!   <endpoint> <from-agent> <to-agent> <cert-dir> <body>
//! ```
//!
//! `<cert-dir>` is a minted agent bundle (`agent.pem`, `agent-key.pem`,
//! `ca.pem`). The node overwrites the authored `from` with the authenticated
//! identity, so passing a `<from-agent>` that does not match the cert is the
//! forgery check, not a way to spoof.

use std::sync::Arc;

use nexus_vfs_client::NexusVfsClient;
use runtime::agent_mailbox::MailboxEnvelope;

/// The unified mailbox for `agent` — the same transport a running agent uses.
///
/// These call sites used to reach a second implementation that lived beside
/// `Mailbox` and duplicated it. Production had already moved, so exercising the
/// copy proved nothing about what ships.
fn mailbox(client: &Arc<NexusVfsClient>, agent: &str, auth: &str) -> Mailbox {
    Mailbox::over_nexus(Arc::clone(client), agent, auth)
}

fn send_to(
    client: &Arc<NexusVfsClient>,
    from_agent: &str,
    to_agent: &str,
    body: &str,
    auth: &str,
) -> Result<(), String> {
    mailbox(client, from_agent, auth).send(MailboxEnvelope {
        from: from_agent.to_string(),
        to: to_agent.to_string(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: String::new(),
        request_id: None,
    })
}

use runtime::mailbox::Mailbox;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, endpoint, from, to, cert_dir, body] = args.as_slice() else {
        eprintln!("usage: a2a_poke <endpoint> <from> <to> <cert-dir> <body>");
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

    // Idempotent, and the sender's own inbox has to exist for a reply to land.
    mailbox(&client, from, "")
        .ensure_inbox()
        .unwrap_or_else(|e| panic!("ensure {from} inbox: {e}"));
    send_to(&client, from, to, body, "").unwrap_or_else(|e| panic!("send to {to}: {e}"));
    println!("sent as {from} -> {to}: {body}");
}
