use runtime::fs_backend::NexusVfsFsBackend;
use runtime::nexus_mailbox::{AgentCredential, Config};

pub fn backend() -> NexusVfsFsBackend {
    let credential = AgentCredential::load(
        &std::env::var("NEXUS_A2A_TEST_CERT_DIR").expect("test credential directory"),
    )
    .expect("load mTLS credential");
    let client = Config {
        endpoint: std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("test daemon endpoint"),
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .expect("connect to real daemon");
    NexusVfsFsBackend::from_arc_mtls(
        client,
        std::env::var("NEXUS_API_KEY").unwrap_or_default(),
        std::env::var("NEXUS_A2A_ZONE").expect("test daemon zone"),
    )
}
