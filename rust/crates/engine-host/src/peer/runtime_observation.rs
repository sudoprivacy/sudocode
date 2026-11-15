//! External observations use the selected mailbox's existing authenticated client.

use super::{NodeBinding, NodeState};
use runtime::runtime_status::{
    PublishRuntimeRequest, RegisterRuntimeRequest, RuntimeDirectory, RuntimeInstanceId,
    RuntimeInstanceRequest, RuntimeObservationHost, RuntimeStatusSource,
};
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

impl NodeBinding {
    fn runtime_client(
        &self,
        connect: bool,
    ) -> Result<Arc<nexus_vfs_client::NexusVfsClient>, String> {
        if connect {
            self.backend()?;
        }
        match &*self
            .state
            .try_lock()
            .map_err(|_| "selected node is connecting")?
        {
            NodeState::Ready { client, .. } => Ok(Arc::clone(client)),
            NodeState::Pending { .. } => Err("selected node is unavailable".into()),
        }
    }
}

pub(super) struct NodeRuntimeHost {
    binding: Arc<NodeBinding>,
    kernel: Arc<kernel::kernel::Kernel>,
    current: Arc<Mutex<Option<Arc<Publication>>>>,
}

struct Publication {
    state: Mutex<PublicationState>,
    stopped: AtomicBool,
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct PublicationState {
    instance: Option<RuntimeInstanceId>,
    needs_admission: bool,
    revision: u64,
}

impl Publication {
    fn publish_once(
        &self,
        binding: &NodeBinding,
        source: &RuntimeStatusSource,
        request: &RegisterRuntimeRequest,
    ) -> Result<(), String> {
        let client = binding.runtime_client(true)?;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let instance = {
            let state = self
                .state
                .lock()
                .map_err(|_| "runtime publication unavailable")?;
            if state.needs_admission {
                None
            } else {
                state.instance.clone()
            }
        };
        let instance = if let Some(instance) = instance {
            instance
        } else {
            let instance: RuntimeInstanceId = call(&client, "agent_runtime_register_v1", request)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| "runtime publication unavailable")?;
            state.instance = Some(instance.clone());
            state.needs_admission = false;
            instance
        };
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = match source() {
            Some(facts) => {
                let revision = {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| "runtime publication unavailable")?;
                    state.revision = state
                        .revision
                        .checked_add(1)
                        .ok_or("runtime publication clock exhausted")?;
                    state.revision
                };
                call::<serde_json::Value>(
                    &client,
                    "agent_runtime_publish_v1",
                    &PublishRuntimeRequest {
                        instance,
                        update: runtime::runtime_status::RuntimeStatusUpdate::from_facts(
                            revision, facts,
                        ),
                    },
                )
                .map(|_| ())
            }
            None => call::<bool>(
                &client,
                "agent_runtime_heartbeat_v1",
                &RuntimeInstanceRequest { instance },
            )
            .map(|_| ()),
        };
        if result.is_err() {
            // Retry the same admission intent after reconnect/restart. The server
            // checks its claim; a replacement sharing its PID cannot be adopted.
            self.state
                .lock()
                .map_err(|_| "runtime publication unavailable")?
                .needs_admission = true;
        }
        result
    }
}

impl std::fmt::Debug for NodeRuntimeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeRuntimeHost")
            .field("agent", &self.binding.agent)
            .finish_non_exhaustive()
    }
}

impl NodeRuntimeHost {
    pub(super) fn new(binding: Arc<NodeBinding>, kernel: Arc<kernel::kernel::Kernel>) -> Self {
        Self {
            binding,
            kernel,
            current: Arc::default(),
        }
    }
}

struct PublicationGuard {
    current: Arc<Mutex<Option<Arc<Publication>>>>,
    publication: Arc<Publication>,
}

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        self.publication.stopped.store(true, Ordering::Release);
        self.publication.wake.notify_one();
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.publication))
        {
            *current = None;
        }
    }
}

fn call<T: DeserializeOwned>(
    client: &nexus_vfs_client::NexusVfsClient,
    method: &str,
    request: &impl serde::Serialize,
) -> Result<T, String> {
    let payload = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    let bytes = client
        .call_with_deadline(method, &payload, "", Duration::from_secs(1))
        .map_err(|error| error.to_string())?;
    let envelope: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    serde_json::from_value(envelope["result"].clone()).map_err(|error| error.to_string())
}

impl RuntimeObservationHost for NodeRuntimeHost {
    fn bind(&self, source: RuntimeStatusSource) -> Option<Box<dyn Send>> {
        let mut current = self.current.lock().ok()?;
        if current.is_some() {
            return None;
        }
        let publication = Arc::new(Publication {
            state: Mutex::new(PublicationState::default()),
            stopped: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
        });
        *current = Some(Arc::clone(&publication));
        let binding = Arc::clone(&self.binding);
        let running = Arc::clone(&publication);
        let kernel = Arc::clone(&self.kernel);
        // No connection, model request, or mailbox scan on the startup thread.
        self.kernel.runtime().handle().spawn(async move {
            let request = Arc::new(RegisterRuntimeRequest {
                name: binding.agent.clone(),
                connection_id: uuid::Uuid::new_v4().to_string(),
                registration_id: uuid::Uuid::new_v4().to_string(),
                owner_id: None,
                zone_id: None,
            });
            while !running.stopped.load(Ordering::Acquire) {
                let binding = Arc::clone(&binding);
                let publication = Arc::clone(&running);
                let source = Arc::clone(&source);
                let request = Arc::clone(&request);
                let result = tokio::task::spawn_blocking(move || {
                    publication.publish_once(&binding, &source, &request)
                })
                .await;
                let delay = if matches!(result, Ok(Ok(()))) {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(5)
                };
                tokio::select! {
                    () = running.wake.notified() => {},
                    () = tokio::time::sleep(delay) => {},
                }
            }
            let instance = running
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .instance
                .clone();
            if let Some(instance) = instance {
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(client) = binding.runtime_client(false) {
                        let _: Result<bool, _> = call(
                            &client,
                            "agent_runtime_unregister_v1",
                            &RuntimeInstanceRequest { instance },
                        );
                    }
                })
                .await;
            }
            // Retain only the existing I/O host until its bounded cleanup ends.
            drop(kernel);
        });
        Some(Box::new(PublicationGuard {
            current: Arc::clone(&self.current),
            publication,
        }))
    }

    fn instance(&self) -> Option<RuntimeInstanceId> {
        let publication = self.current.lock().ok()?.clone()?;
        let instance = publication.state.lock().ok()?.instance.clone();
        instance
    }

    fn discover(&self) -> Result<RuntimeDirectory, String> {
        let client = self.binding.runtime_client(false)?;
        // RustService responses are raw JSON, unlike the generic result envelope.
        let payload = client
            .call_with_deadline(
                "runtime_directory.list_v1",
                b"{}",
                "",
                Duration::from_secs(2),
            )
            .map_err(|error| error.to_string())?;
        let directory: RuntimeDirectory =
            serde_json::from_slice(&payload).map_err(|error| error.to_string())?;
        if directory.version != 1 {
            return Err("unsupported runtime directory version".into());
        }
        Ok(directory)
    }
}
