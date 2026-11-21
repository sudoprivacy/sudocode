//! In-process host for negotiated managed sessions.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use a2a::session::{SessionEndpoint, SessionPayload};
use engine_host::managed_agent::{prepare_managed_agent, PreparedManagedAgent};
use engine_host::managed_context::ManagedRuntimeContext;
use engine_host::SessionEngine;
use kernel::core::agents::host_cleanup::ManagedHostCleanup;
use managed_agent::{SpawnHandle, SpawnOptions, SpawnTask};
use runtime::spawn_task::{AgentDescriptor, AgentState, KernelConvenience};
use runtime::{agent_operation_context, HookAbortSignal, KernelFsAccess};

use crate::acp_sdk_server::{
    run_acp_on_transport, AcpError, HostedSessionFactory, SessionRegistry,
};
use crate::managed::managed_session_config;

type StateObserver = Arc<dyn Fn(AgentState, Option<String>) + Send + Sync>;

/// Close every session and receiver, then fold in an observed lease release
/// failure: only `Ok` is a confirmed end of this host.
async fn confirmed_cleanup(
    registry: Arc<SessionRegistry>,
    lease_release_failure: &Mutex<Option<String>>,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || registry.close_all())
        .await
        .unwrap_or_else(|error| Err(format!("session cleanup did not finish: {error}")))?;
    released(lease_release_failure)
}

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

    fn state_changed(&self, state: engine_core::EngineState) {
        let (state, reason) = AgentState::project_foreground(state);
        (self.observer)(state, reason);
    }
}

/// Runs managed sessions as in-process sudocode agents.
pub struct SudoCodeSpawnAdapter {
    /// The daemon's auth posture (`ServiceBootCtx::auth_armed`): when armed,
    /// every conversation writer is authenticated and stamped, so peers'
    /// senders are verified.
    pub authenticated_senders: bool,
}

impl<K: KernelConvenience + KernelFsAccess + Send + Sync + 'static> SpawnTask<K>
    for SudoCodeSpawnAdapter
{
    fn spawn(
        &self,
        _kernel: Arc<K>,
        _desc: AgentDescriptor,
        _observer: StateObserver,
    ) -> Result<Box<dyn SpawnHandle>, String> {
        Err("managed sessions require an admitted session transport".into())
    }

    /// The registry's only entry for an in-process host. `cleanup` is this
    /// incarnation's native retirement latch: it is completed once, by this
    /// host, after every writer and receiver has joined, and only an accepted
    /// joined outcome may publish `Closed` and `Terminated`.
    fn spawn_with_cleanup(
        &self,
        kernel: Arc<K>,
        desc: AgentDescriptor,
        options: SpawnOptions,
        cleanup: ManagedHostCleanup,
        observer: StateObserver,
    ) -> Result<Box<dyn SpawnHandle>, String> {
        let context =
            match ManagedRuntimeContext::cohost(&kernel, &desc, self.authenticated_senders) {
                Ok(context) => {
                    context.with_runtime_observation(options.runtime_observation.clone())
                }
                Err(error) => {
                    cleanup.complete(Ok(()));
                    return Err(error);
                }
            };
        let lease_release_failure = context.lease_release_failure();
        start_host(kernel, &desc, &options, &context, cleanup.clone(), observer).inspect_err(|_| {
            // A refused start owns no thread or receiver: everything it made
            // was dropped before it returned, including the session lease.
            cleanup.complete(released(&lease_release_failure));
        })
    }
}

/// An observed lease release failure, or `Ok` when every release succeeded.
fn released(lease_release_failure: &Mutex<Option<String>>) -> Result<(), String> {
    lease_release_failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .map_or(Ok(()), Err)
}

fn start_host<K: KernelConvenience + KernelFsAccess + Send + Sync + 'static>(
    kernel: Arc<K>,
    desc: &AgentDescriptor,
    options: &SpawnOptions,
    context: &ManagedRuntimeContext,
    cleanup: ManagedHostCleanup,
    observer: StateObserver,
) -> Result<Box<dyn SpawnHandle>, String> {
    let transport = options
        .session_transport
        .as_ref()
        .ok_or("managed session has no transport")?;
    let endpoint = transport.endpoint().clone();
    let prepared = prepare_managed_agent(context, options.resume_session_id.as_deref())
        .map_err(|error| error.to_string())?;
    let lease_release_failure = context.lease_release_failure();
    let peer_mailbox = prepared.mailbox();
    let session_id = prepared.durable_session_id().to_string();
    // This signal terminates the HOST. Turn cancellation belongs to each
    // SessionEngine and is reset only when that engine starts another turn.
    let stopped = prepared.abort_signal();
    let config = managed_session_config(prepared.model());
    let ctx = agent_operation_context(&desc.owner_id, &desc.zone_id, &desc.name);
    let mailbox = Arc::new(transport.open(kernel, ctx)?);
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
    let agent_pid = desc.pid.clone();
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
                                        Ok(Ok(Some(SessionPayload::Closed { reason })) | Err(reason)) => reason,
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
                let outcome = confirmed_cleanup(Arc::clone(&thread_registry), &lease_release_failure).await;
                let reason = result.err().map_or_else(|| "session connection closed".into(), |e| e.to_string());
                // Closed and Terminated are a confirmed end: every writer and
                // receiver joined and the registry's latch accepted it. An
                // unconfirmed cleanup stays nonterminal, so the registry keeps
                // exclusive custody of this instance.
                if !cleanup.complete(outcome) {
                    eprintln!("managed session {agent_pid} cleanup unconfirmed ({reason}): {:?}", cleanup.outcome());
                    return;
                }
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
    fn durable_session_id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }
    fn session_endpoint(&self) -> Option<&SessionEndpoint> {
        Some(&self.endpoint)
    }
}
