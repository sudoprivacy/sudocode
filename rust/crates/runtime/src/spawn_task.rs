//! Co-host `send` tool and prompt helpers. Session turns are driven by engine-acp.

use std::sync::Arc;

// Re-export kernel types so downstream crates (e.g. `tools`) can
// reference them without adding a direct `kernel` dependency.
pub use kernel::core::agents::registry::{AgentDescriptor, AgentState};
pub use kernel::kernel::convenience::KernelConvenience;
pub use kernel::kernel::syscall::KernelSyscall;

pub use crate::agent_mailbox::MailboxEnvelope;
/// A type-erased "send a message to a peer's mailbox" capability handed to the
/// co-hosted agent's `send` tool. It writes a [`MailboxEnvelope`] (the a2a SSOT) to
/// the recipient's inbox; the a2a stamp hook overwrites `from` with the authenticated
/// caller when auth is armed.
///
/// The tool is how an agent addresses someone — any peer, any number of them — and
/// it is the only way anything a turn produces reaches a peer.
pub type MailboxSender = Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

/// Shared handler for the `send` A2A tool. The mailbox supplies delivery;
/// this function validates the tool's `{to, message}` input.
///
/// # Errors
/// Returns a `String` error when the input lacks a string `to`/`message`, or
/// when the send fails.
pub fn handle_send_message(
    sender: &MailboxSender,
    input: &serde_json::Value,
) -> Result<String, String> {
    let to = input
        .get("to")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "send_message requires a string 'to'".to_string())?;
    let message = input
        .get("message")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "send_message requires a string 'message'".to_string())?;
    (sender)(to, message)?;
    Ok(format!("message delivered to {to}"))
}

/// What a co-hosted agent is told about where its shell runs.
///
/// A local reference uses the same directory for both and needs no extra section.
/// Otherwise its files are in the VFS at `workspace` and reached with file tools;
/// its `bash` runs on the daemon's host filesystem, starting in `shell_root`,
/// which is NOT the workspace. Without this the model does the reasonable thing —
/// `ls` to see what it is working with — reads an unrelated directory, and
/// concludes its workspace is empty.
///
/// Nothing restricts where the shell may `cd`: containment is the mount table,
/// which bounds what the FILE TOOLS address, and the process sandbox is the
/// deployment's job — not this sentence's.
#[must_use]
pub fn cohost_shell_prompt_section(
    workspace: &str,
    shell_root: &std::path::Path,
) -> Option<String> {
    if std::path::Path::new(workspace) == shell_root {
        return None;
    }
    Some(format!(
        "# Your files and your shell are in different places\n\
         Your workspace is `{workspace}` and you reach it with the file tools \
         (read_file / write_file / edit_file / glob / grep). Your `bash` runs on \
         the host that runs this daemon, starting in `{}` — a directory of your \
         own that is NOT your workspace, so `ls` there will not show your files. \
         Use the file tools for your work and `bash` for commands.",
        shell_root.display()
    ))
}
