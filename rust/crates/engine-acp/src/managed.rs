//! A managed agent's ACP sessions, served in the one context its host
//! resolved: the registry binding plus the single authenticated backend
//! every session's storage, tools, mailbox and model go through.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::role::acp::Agent;
use agent_client_protocol::ConnectTo;
use engine_host::managed_agent::{managed_model, prepare_managed_agent, PrepareError};
use engine_host::managed_context::ManagedRuntimeContext;
use engine_host::SessionEngine;

use crate::acp_sdk_server::{
    run_acp_on_transport, AcpError, HostedSessionFactory, SdkAcpConfig, SessionRegistry,
};

/// The ACP server settings every managed session shares: the agent's model
/// and the Prompt permission mode, so tool approval stays with its client.
pub(crate) fn managed_session_config(model: &str) -> SdkAcpConfig {
    SdkAcpConfig {
        agent_version: env!("CARGO_PKG_VERSION").into(),
        model: model.to_string(),
        model_flag_raw: Some(model.to_string()),
        permission_mode_override: Some(runtime::PermissionMode::Prompt),
        reasoning_effort: None,
        allowed_tools: None,
        auth_mode: None,
        git_sha: None,
        build_target: None,
    }
}

/// Every session of one managed agent, in its one resolved context:
/// `session/new` prepares a fresh durable session and `session/load` exactly
/// the one requested. A failed load never becomes a new session.
struct ManagedSessions {
    context: ManagedRuntimeContext,
}

impl HostedSessionFactory for ManagedSessions {
    fn mailbox(&self) -> Option<Arc<runtime::mailbox::Mailbox>> {
        Some(self.context.mailbox())
    }

    fn open(
        &self,
        requested_id: Option<&str>,
        mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
        memory: runtime::memory::MemoryMode,
    ) -> Result<(Arc<SessionEngine>, PathBuf), AcpError> {
        let prepared =
            prepare_managed_agent(&self.context, requested_id).map_err(|error| match error {
                PrepareError::SessionNotFound(_) => {
                    AcpError::session_not_found(requested_id.unwrap_or_default())
                }
                PrepareError::InvalidSessionId(message) => AcpError::invalid_params(message),
                PrepareError::Failed(message) => AcpError::internal(message),
            })?;
        let cwd = prepared.cwd();
        let engine = prepared
            .open(mcp_servers, prompt_overrides, memory)
            .map_err(AcpError::internal)?;
        Ok((Arc::new(engine), cwd))
    }
}

/// Serve ACP on `transport` for one managed agent, every session in
/// `context`. The host resolves the context once — its registry binding and
/// its one authenticated backend — and this is the only way in, so no session
/// can reach local disk, a private kernel or a direct model endpoint.
pub async fn serve_managed(
    context: ManagedRuntimeContext,
    transport: impl ConnectTo<Agent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = managed_session_config(&managed_model(context.descriptor()));
    let registry = Arc::new(SessionRegistry::with_hosted_factory(Arc::new(
        ManagedSessions { context },
    )));
    let result = run_acp_on_transport(&config, Arc::clone(&registry), transport).await;
    registry.cancel_all();
    let closing = Arc::clone(&registry);
    tokio::task::spawn_blocking(move || closing.close_all()).await?;
    result
}
