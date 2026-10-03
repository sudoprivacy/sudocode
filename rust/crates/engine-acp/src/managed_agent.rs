//! In-process host for the same session mailbox used by subprocess hosting.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use engine_host::managed_agent::{prepare_managed_agent, PreparedManagedAgent};
use engine_host::SessionEngine;
use kernel::kernel::OperationContext;
use managed_agent::{SpawnHandle, SpawnOptions, SpawnTask};
use runtime::spawn_task::{AgentDescriptor, AgentState, KernelConvenience};
use runtime::HookAbortSignal;

use crate::acp_sdk_server::{
    run_acp_on_transport, AcpError, HostedSessionFactory, SdkAcpConfig, SessionRegistry,
};

type StateObserver = Arc<dyn Fn(AgentState, Option<String>) + Send + Sync>;

struct Factory {
    prepared: Mutex<Option<PreparedManagedAgent>>,
    session_id: String,
    observer: StateObserver,
    mailbox: Option<Arc<runtime::mailbox::Mailbox>>,
}

impl HostedSessionFactory for Factory {
    fn mailbox(&self) -> Option<Arc<runtime::mailbox::Mailbox>> {
        self.mailbox.clone()
    }
    fn close(&self) {
        if let Ok(mut prepared) = self.prepared.lock() {
            prepared.take();
        }
    }

    fn open(
        &self,
        requested_id: Option<&str>,
        mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
        memory: runtime::memory::MemoryMode,
    ) -> Result<(Arc<SessionEngine>, PathBuf), AcpError> {
        if requested_id.is_some_and(|id| id != self.session_id) {
            return Err(AcpError::invalid_params(
                "sessionId differs from this managed session; resume it through start_session",
            ));
        }
        let prepared = self
            .prepared
            .lock()
            .map_err(|_| AcpError::internal("session factory poisoned"))?
            .take()
            .ok_or_else(|| AcpError::invalid_params("this managed session is already open"))?;
        let cwd = prepared.cwd();
        let engine = prepared
            .open(mcp_servers, prompt_overrides, memory)
            .map_err(AcpError::internal)?;
        Ok((Arc::new(engine), cwd))
    }

    fn state_changed(&self, state: engine_core::EngineState, reason: Option<String>) {
        let state = match state {
            engine_core::EngineState::Idle => AgentState::Ready,
            engine_core::EngineState::Running => AgentState::Busy,
            engine_core::EngineState::AwaitingInput => AgentState::AwaitingInput,
        };
        (self.observer)(state, reason);
    }
}

pub struct SudoCodeSpawnAdapter;

impl<K: KernelConvenience + Send + Sync + 'static> SpawnTask<K> for SudoCodeSpawnAdapter {
    fn spawn(
        &self,
        _kernel: Arc<K>,
        _desc: AgentDescriptor,
        _observer: StateObserver,
    ) -> Result<Box<dyn SpawnHandle>, String> {
        Err("managed sessions require a session mailbox endpoint".into())
    }

    fn spawn_with_options(
        &self,
        kernel: Arc<K>,
        desc: AgentDescriptor,
        options: SpawnOptions,
        observer: StateObserver,
    ) -> Result<Box<dyn SpawnHandle>, String> {
        let endpoint = options
            .session_endpoint
            .clone()
            .ok_or("managed session has no mailbox endpoint")?;
        let prepared = prepare_managed_agent(&kernel, &desc, &options)?;
        let peer_mailbox = prepared.mailbox();
        let session_id = prepared.durable_session_id().to_string();
        // This signal terminates the HOST. Turn cancellation belongs to each
        // SessionEngine and is reset only when that engine starts another turn.
        let stopped = prepared.abort_signal();
        let config = SdkAcpConfig {
            agent_version: env!("CARGO_PKG_VERSION").into(),
            model: prepared.model().to_string(),
            model_flag_raw: Some(prepared.model().to_string()),
            permission_mode_override: Some(runtime::PermissionMode::Prompt),
            reasoning_effort: None,
            allowed_tools: None,
            auth_mode: None,
            git_sha: None,
            build_target: None,
        };
        let ctx = OperationContext::new(
            &desc.owner_id,
            &desc.zone_id,
            false,
            Some(&desc.name),
            false,
        );
        let mailbox = Arc::new(SessionMailbox::open(
            kernel,
            ctx,
            endpoint.clone(),
            SessionSide::Agent,
        )?);
        let registry = Arc::new(SessionRegistry::with_hosted_factory(Arc::new(Factory {
            prepared: Mutex::new(Some(prepared)),
            mailbox: peer_mailbox,
            session_id: session_id.clone(),
            observer: Arc::clone(&observer),
        })));
        let thread_stop = stopped.clone();
        let thread_registry = Arc::clone(&registry);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // Each managed session owns an I/O driver. Kernel calls and turns
            // use blocking workers; do not allocate a CPU-sized pool per agent.
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        std::thread::Builder::new().name(format!("managed-session-{}", desc.pid)).spawn(move || {
            runtime.block_on(async move {
                let read_mailbox = Arc::clone(&mailbox);
                let read_stop = thread_stop.clone();
                let read_registry = Arc::clone(&thread_registry);
                let incoming = futures::stream::unfold(
                    (read_mailbox, read_stop, read_registry),
                    |(mailbox, stop, registry)| async move {
                        loop {
                            if stop.is_aborted() {
                                registry.cancel_all();
                                return None;
                            }
                            let reader = Arc::clone(&mailbox);
                            match tokio::task::spawn_blocking(move || reader.receive(200)).await {
                                Ok(Ok(Some(SessionPayload::Rpc { message }))) => {
                                    return Some((Ok::<_, io::Error>(message.to_string()), (mailbox, stop, registry)));
                                }
                                Ok(Ok(None)) => {}
                                result => {
                                    stop.abort();
                                    registry.cancel_all();
                                    let error = match result {
                                        Ok(Ok(Some(SessionPayload::Closed { reason }))) | Ok(Err(reason)) => reason,
                                        Err(error) => error.to_string(),
                                        _ => unreachable!(),
                                    };
                                    return Some((Err(io::Error::other(error)), (mailbox, stop, registry)));
                                }
                            }
                        }
                    },
                );
                let outgoing = futures::sink::unfold(Arc::clone(&mailbox), |mailbox, line: String| async move {
                    let message = serde_json::from_str(&line).map_err(io::Error::other)?;
                    let writer = Arc::clone(&mailbox);
                    tokio::task::spawn_blocking(move || writer.send(SessionPayload::Rpc { message }))
                        .await.map_err(io::Error::other)?.map_err(io::Error::other)?;
                    Ok::<_, io::Error>(mailbox)
                });
                let transport = agent_client_protocol::Lines::new(Box::pin(outgoing), Box::pin(incoming));
                (observer)(AgentState::Ready, None);
                let result = tokio::select! {
                    result = run_acp_on_transport(&config, Arc::clone(&thread_registry), transport) => result,
                    () = thread_stop.cancelled() => Ok(()),
                };
                thread_registry.cancel_all();
                thread_stop.abort();
                let closing = Arc::clone(&thread_registry);
                let _ = tokio::task::spawn_blocking(move || closing.close_all()).await;
                let reason = result.err().map_or_else(|| "session connection closed".into(), |e| e.to_string());
                let closed_reason = reason.clone();
                let _ = tokio::task::spawn_blocking(move || mailbox.send(SessionPayload::Closed { reason: closed_reason })).await;
                (observer)(AgentState::Terminated, Some(reason));
            });
        }).map_err(|e| format!("start managed session driver: {e}"))?;
        Ok(Box::new(Handle {
            endpoint,
            session_id,
            stopped,
            registry,
        }))
    }
}

struct Handle {
    endpoint: SessionEndpoint,
    session_id: String,
    stopped: HookAbortSignal,
    registry: Arc<SessionRegistry>,
}

impl SpawnHandle for Handle {
    fn abort(&self) {
        self.stopped.abort();
        self.registry.cancel_all();
    }
    fn durable_session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }
    fn session_endpoint(&self) -> Option<&SessionEndpoint> {
        Some(&self.endpoint)
    }
}
