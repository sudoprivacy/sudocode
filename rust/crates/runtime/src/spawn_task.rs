//! Shared A2A prompt and reply rules. Session turns are driven by engine-acp.

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
/// The tool is how an agent ADDRESSES someone — any peer, any number of them. It is
/// not the only way a message leaves a turn: prose written instead of a tool call is
/// delivered to the sender once (`auto_reply_body`), because co-hosted there is no
/// human reading the turn. Calling this is what a turn does when it means to speak to
/// someone in particular.
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

/// The system-prompt section that teaches a co-hosted agent the A2A reply
/// contract it runs under, so the model addresses its reply correctly instead
/// of guessing a recipient from the message text.
///
/// It is the prose counterpart of two mechanisms this module owns and MUST stay
/// in step with them:
/// * inbound framing 鈥?the session driver hands each message to the turn as
///   `[message from <sender>]\n\n<body>`, so `<sender>` is the reply target;
/// * the reply path — [`crate::mailbox::Mailbox::sender`] wires the `send` tool, which
///   is how an agent addresses a peer it names; prose written instead reaches the
///   sender once (`auto_reply_body`), so the prompt must not promise silence.
///
/// Kept next to those two so the wording cannot drift from the framing/tool it
/// describes. `self_id` is the agent's own name (`Mailbox::self_id`).
#[must_use]
pub fn cohost_a2a_prompt_section(self_id: &str) -> String {
    // The SAME builder the REPL hosts use, with this host's framing as the one value
    // that differs — see `agent_mailbox::a2a_prompt_section`. Nothing about the reply
    // contract or peer discovery is restated here, which is what keeps the two hosts
    // saying the same thing without anyone having to check.
    //
    // No peer list: a co-hosted agent is given no configured peers, so it finds them
    // the way the contract tells every agent to — by asking.
    crate::agent_mailbox::a2a_prompt_section(self_id, "#", COHOST_FRAMING)
}

/// How the co-host frames an inbound message: the session driver wraps each one as
/// `[message from <sender>]`. The one value that differs from the REPL hosts'.
const COHOST_FRAMING: &str = "Peer messages are shown as \
     `[message from <sender>]` followed by their text. If you answer a peer \
     without calling `send`, your answer is delivered to that sender once. \
     Use `send` to address anyone else. Prompts without this peer framing come \
     from the session controller; answer those normally.";

/// What a co-hosted agent is told about where its shell runs.
///
/// Its files are in the VFS at `workspace` and are reached with the file tools;
/// its `bash` runs on the daemon's host filesystem, starting in `shell_root`,
/// which is NOT the workspace. Without this the model does the reasonable thing —
/// `ls` to see what it is working with — reads an unrelated directory, and
/// concludes its workspace is empty.
///
/// Said once here, beside [`cohost_a2a_prompt_section`], so the two things only
/// this host contributes are written in one place. Nothing restricts where the
/// shell may `cd`: containment is the mount table, which bounds what the FILE
/// TOOLS address, and the process sandbox is the deployment's job — not this
/// sentence's.
#[must_use]
pub fn cohost_shell_prompt_section(workspace: &str, shell_root: &std::path::Path) -> String {
    format!(
        "# Your files and your shell are in different places\n\
         Your workspace is `{workspace}` and you reach it with the file tools \
         (read_file / write_file / edit_file / glob / grep). Your `bash` runs on \
         the host that runs this daemon, starting in `{}` — a directory of your \
         own that is NOT your workspace, so `ls` there will not show your files. \
         Use the file tools for your work and `bash` for commands.",
        shell_root.display()
    )
}

/// Decide whether to deliver a turn's prose back to its sender.
///
/// A co-hosted agent has no other audience. In the REPL hosts a turn's text goes to
/// the human who asked; co-hosted, it went nowhere, so an agent that wrote its answer
/// rather than calling the tool answered into a void. That is not a hypothetical: in
/// the live duet one agent wrote "PONG" into its own transcript while the other sat
/// waiting and told its operator it would relay as soon as a reply arrived.
///
/// Forwarding unconditionally is the other failure, and it is why this loop used to
/// forward nothing: two agents bounce every turn's output at each other forever. The
/// bound is what makes delivery safe — an auto-reply is delivered, and an auto-reply
/// never produces another one ([`crate::agent_mailbox::kinds::AUTO_REPLY`]). One hop,
/// so the answer arrives and the chain cannot run.
///
/// The other two conditions are about not speaking for the agent. A turn that called
/// `send` already said what it meant to say, to whoever it chose — including a peer
/// that is not this sender — so its prose is working notes, not a reply. A turn with
/// no text at all is an agent deliberately staying quiet, which the contract allows.
///
/// Pure, so the rule is testable without a running loop.
pub fn auto_reply_body(
    inbound_kind: &str,
    assistant_text: &str,
    sends_before: u64,
    sends_after: u64,
) -> Option<String> {
    if inbound_kind == crate::agent_mailbox::kinds::AUTO_REPLY {
        return None;
    }
    if sends_after != sends_before {
        return None;
    }
    let trimmed = assistant_text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cohost_prompt_teaches_reply_to_sender_via_send_message() {
        let section = super::cohost_a2a_prompt_section("chatbot");
        // Names the agent so the model knows its own identity 鈥?
        assert!(section.contains("chatbot"));
        // 鈥?names the ONLY reply path 鈥?
        assert!(section.contains("send"));
        // 鈥?mirrors the `[message from <sender>]` framing the session driver emits 鈥?
        assert!(section.contains("[message from <sender>]"));
        // 鈥?and encodes the fix: reply target is the sender, never a word
        // lifted from the message body (the exact mistake this prevents).
        assert!(section.contains("never a word copied"));
    }
}

#[cfg(test)]
mod auto_reply_tests {
    use super::auto_reply_body;
    use crate::agent_mailbox::kinds;

    #[test]
    fn prose_with_no_send_is_delivered() {
        assert_eq!(
            auto_reply_body(kinds::MESSAGE, "  PONG\n", 7, 7).as_deref(),
            Some("PONG"),
            "an answer written as prose has to reach the one who asked"
        );
    }

    #[test]
    fn an_auto_reply_never_produces_another_one() {
        // THE bound. Without it two agents that both answer in prose exchange
        // pleasantries until something stops them.
        assert_eq!(auto_reply_body(kinds::AUTO_REPLY, "thanks!", 7, 7), None);
    }

    #[test]
    fn a_turn_that_called_send_speaks_for_itself() {
        // It already addressed whoever it chose — possibly not this sender — so its
        // prose is working notes, not a reply to forward.
        assert_eq!(
            auto_reply_body(kinds::MESSAGE, "done, told bob", 7, 8),
            None
        );
    }

    #[test]
    fn silence_stays_silence() {
        assert_eq!(auto_reply_body(kinds::MESSAGE, "", 7, 7), None);
        assert_eq!(auto_reply_body(kinds::MESSAGE, "  \n\t ", 7, 7), None);
    }
}
