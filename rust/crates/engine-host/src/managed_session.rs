//! Durable session selection and serialized co-host initialization.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use runtime::fs_backend::FsLock;
use runtime::session_control::SessionStore;
use runtime::HostedSessionIdentity;
use runtime::{HookAbortSignal, Session};

use crate::managed_agent::PrepareError;
use crate::managed_context::ManagedRuntimeContext;

/// Serialize initialization. Legacy transcripts retain this admission guard
/// for their lifetime; committed record contexts release it after activation.
pub(crate) struct SessionLease {
    stop: mpsc::Sender<()>,
    renewal: Option<thread::JoinHandle<()>>,
}

impl SessionLease {
    /// Take the writer lease through the context's lease authority: the
    /// daemon's kernel for a co-host, the storing daemon for a remote agent.
    fn acquire(
        ctx: &ManagedRuntimeContext,
        path: &str,
        abort: HookAbortSignal,
    ) -> Result<Self, String> {
        let lock: Arc<dyn FsLock> = (ctx.session_locks)(path)
            .map_err(|e| format!("co-host: acquire session lease: {e}"))?
            .ok_or("co-host: session is still running; stop it before resuming")?;
        let (stop, receiver) = mpsc::channel();
        // The thread owns the lock: it releases when renewal ends, and a
        // thread that fails to start drops it here.
        let renewal = thread::Builder::new()
            .name("cohost-session-lease".into())
            .spawn(move || {
                while matches!(
                    receiver.recv_timeout(Duration::from_secs(10)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if !matches!(lock.renew(), Ok(true)) {
                        abort.abort();
                        let _ = receiver.recv();
                        break;
                    }
                }
                drop(lock);
            })
            .map_err(|e| format!("co-host: start session lease renewal: {e}"))?;
        Ok(Self {
            stop,
            renewal: Some(renewal),
        })
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(renewal) = self.renewal.take() {
            let _ = renewal.join();
        }
    }
}

pub(crate) fn prepare_session(
    ctx: &ManagedRuntimeContext,
    resume_id: Option<&str>,
    model: &str,
    abort: HookAbortSignal,
) -> Result<(Session, Option<SessionLease>), PrepareError> {
    let desc = &ctx.desc;
    let fs = Arc::clone(&ctx.fs);
    let workspace = fs
        .working_root()
        .map_err(|error| format!("co-host: resolve session workspace: {error}"))?;
    let identity = HostedSessionIdentity {
        agent_name: desc.name.clone(),
        owner_id: desc.owner_id.clone(),
        zone_id: desc.zone_id.clone(),
        repos: desc
            .repos
            .iter()
            .map(|repo| (repo.alias.clone(), repo.mount_path.clone()))
            .collect(),
    };
    if identity.repos.len() != desc.repos.len() {
        return Err(PrepareError::Failed(
            "co-host: repository aliases must be unique".into(),
        ));
    }
    let store = SessionStore::from_cwd_with(&workspace, Arc::clone(&fs))
        .map_err(|e| format!("co-host: open session store: {e}"))?
        .with_agent_name(&desc.name)
        .with_identity(identity.clone());
    let mut session = Session::new();
    let id = resume_id.unwrap_or(&session.session_id).to_string();
    if runtime::session_control::validate_session_id(&id).is_err() {
        return Err(PrepareError::InvalidSessionId(
            "co-host: resume_session_id must be a durable ID, not a path".into(),
        ));
    }
    let root = store.sessions_dir().to_string_lossy();
    let lease = SessionLease::acquire(ctx, &format!("{root}/{id}"), abort)?;
    if resume_id.is_some() {
        session = store
            .load_exact(&id)
            .map_err(|error| {
                let message = format!("co-host: restore session: {error}");
                match error {
                    runtime::session_control::SessionControlError::NotFound(_) => {
                        PrepareError::SessionNotFound(message)
                    }
                    _ => PrepareError::Failed(message),
                }
            })?
            .session;
    }
    if session.identity.is_none() {
        session.identity = Some(identity);
    }
    if session.model.is_none() {
        session.model = Some(model.to_string());
    }
    let handle = store.create_handle(&id);
    let mut session = session
        .with_workspace_root(workspace)
        .with_persistence_path(handle.path.clone())
        .with_fs_backend(fs);
    // Publish even an empty session before returning its ID. Use the native
    // append-log bootstrap, not snapshot replacement (which would create a
    // regular file or append a duplicate snapshot to an existing DT_STREAM).
    // Only the active writer may publish interrupted tool results. Inspection
    // loads keep them in memory; this host already holds the session lease.
    if resume_id.is_none() {
        session
            .initialize_persistence()
            .map_err(|e| format!("co-host: persist session: {e}"))?;
    } else {
        session
            .activate_persistence()
            .map_err(|error| format!("co-host: activate restored context: {error}"))?;
        session
            .repair_orphan_tool_uses()
            .map_err(|error| format!("co-host: persist interrupted tool recovery: {error}"))?;
    }
    let concurrent = session
        .supports_concurrent_resume()
        .map_err(|error| error.to_string())?;
    let lease = if concurrent {
        drop(lease);
        None
    } else {
        Some(lease)
    };
    Ok((session, lease))
}
