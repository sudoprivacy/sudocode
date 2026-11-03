//! Seed an explicit memory stream for tests of an in-process, non-federated kernel.
use std::sync::Arc;

use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::Kernel;
use runtime::{HostedSessionIdentity, KernelFsBackend, Session};

pub fn seed_managed_session(
    kernel: &Arc<Kernel>,
    desc: &AgentDescriptor,
) -> managed_agent::SpawnOptions {
    let workspace = format!("/proc/{}/workspace", desc.pid);
    let mut session = Session::new().with_workspace_root(&workspace);
    session.identity = Some(HostedSessionIdentity {
        agent_name: desc.name.clone(),
        owner_id: desc.owner_id.clone(),
        zone_id: desc.zone_id.clone(),
        repos: desc
            .repos
            .iter()
            .map(|repo| (repo.alias.clone(), repo.mount_path.clone()))
            .collect(),
    });
    let id = session.session_id.clone();
    let transcript = format!("/sessions/{id}/transcript.jsonl");
    super::provision_stream_transcript(kernel, &transcript);
    let session = session
        .with_persistence_path(transcript)
        .with_fs_backend(Arc::new(KernelFsBackend::for_agent_descriptor(
            Arc::clone(kernel),
            desc,
            workspace,
        )));
    session.initialize_persistence().unwrap();
    managed_agent::SpawnOptions {
        resume_session_id: Some(id),
        ..Default::default()
    }
}
