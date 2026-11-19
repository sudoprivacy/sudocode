//! A managed agent's own process: its host hands it one private bootstrap
//! line on stdin, then speaks ACP on the same stdin and stdout.
//!
//! Nothing reaches Nexus before the host-supplied connection exists: the line
//! is validated, the named credential loaded and the connection built before
//! the one remote context answers for its workspace. Every refusal is a typed
//! reply on stdout and a non-zero exit, never a fallback to the default
//! registry or local disk.

use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol::role::acp::Agent;
use agent_client_protocol::ConnectTo;
use engine_host::managed_context::ManagedRuntimeContext;
use managed_agent::bootstrap::{read_bootstrap, write_reply, BootstrapFailure, BootstrapReply};
use runtime::nexus_mailbox::{AgentCredential, Config};
use tokio::io::AsyncWriteExt;

use crate::managed::{primary_outcome, serve_managed_sessions};

/// Serve one managed agent on this process's private stdio.
///
/// The host closing its side is this process's normal end: it then exits
/// cleanly exactly when every session's teardown was confirmed.
///
/// # Errors
///
/// The bootstrap was refused (its typed reason is already on stdout), the
/// ACP connection failed, or a session's cleanup was not confirmed.
pub async fn run_managed_stdio() -> Result<(), Box<dyn std::error::Error>> {
    // Reading stdin, loading the credential and the workspace probe all block.
    let (reader, opened) = tokio::task::spawn_blocking(|| {
        let mut reader = BufReader::new(io::stdin());
        let opened = open_context(&mut reader);
        (reader, opened)
    })
    .await?;
    let context = match opened {
        Ok(context) => context,
        Err((failure, detail)) => {
            eprintln!("managed bootstrap refused ({failure:?}): {detail}");
            reply(&BootstrapReply::Error { message: failure })?;
            return Err(detail.into());
        }
    };
    reply(&BootstrapReply::Ready)?;
    let host_closed = Arc::new(AtomicBool::new(false));
    let (result, cleanup) =
        serve_managed_sessions(context, stdio_lines(reader, Arc::clone(&host_closed))).await;
    if host_closed.load(Ordering::SeqCst) {
        return cleanup.map_err(Into::into);
    }
    primary_outcome(result, cleanup)
}

/// Everything before the first Nexus request is local: the line, the
/// credential manifest and the client. The first request is `remote`'s
/// workspace probe, over the connection the host supplied.
fn open_context(
    reader: &mut impl BufRead,
) -> Result<ManagedRuntimeContext, (BootstrapFailure, String)> {
    let frame = read_bootstrap(reader, std::process::id())
        .map_err(|error| (BootstrapFailure::InvalidPrelude, error.to_string()))?;
    let credential =
        AgentCredential::load(&frame.connection.credential_directory.to_string_lossy())
            .map_err(|error| (BootstrapFailure::CredentialRefused, error))?;
    if credential.agent != frame.descriptor.name {
        return Err((
            BootstrapFailure::CredentialRefused,
            format!(
                "the credential is for agent '{}', the bootstrap names '{}'",
                credential.agent, frame.descriptor.name
            ),
        ));
    }
    let client = Config {
        endpoint: frame.connection.endpoint.clone(),
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .map_err(|error| (BootstrapFailure::ConnectionRefused, error))?;
    ManagedRuntimeContext::remote(
        &frame.descriptor,
        client,
        &frame.workspace_vfs_root,
        frame.shell_root.clone(),
        frame.authenticated_senders,
    )
    .map_err(|error| (BootstrapFailure::ContextRefused, error))
}

fn reply(reply: &BootstrapReply) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    write_reply(&mut stdout, reply)?;
    stdout.flush()
}

/// ACP lines on the reader that carried the bootstrap, so lines the host sent
/// right after it are not lost. The host ending its side ends the connection:
/// a line transport keeps running on a bare EOF while its output is open.
fn stdio_lines(
    mut reader: BufReader<io::Stdin>,
    host_closed: Arc<AtomicBool>,
) -> impl ConnectTo<Agent> {
    let (lines, received) = tokio::sync::mpsc::unbounded_channel::<io::Result<String>>();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            let next = match reader.read_line(&mut line) {
                Ok(0) => {
                    host_closed.store(true, Ordering::SeqCst);
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the host closed the managed connection",
                    ))
                }
                Ok(_) => Ok(line.trim_end_matches(['\r', '\n']).to_owned()),
                Err(error) => Err(error),
            };
            let ended = next.is_err();
            if lines.send(next).is_err() || ended {
                return;
            }
        }
    });
    let incoming = futures::stream::unfold(received, |mut received| async move {
        received.recv().await.map(|line| (line, received))
    });
    let outgoing =
        futures::sink::unfold(tokio::io::stdout(), |mut stdout, line: String| async move {
            stdout.write_all(line.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
            Ok::<_, io::Error>(stdout)
        });
    agent_client_protocol::Lines::new(Box::pin(outgoing), Box::pin(incoming))
}
