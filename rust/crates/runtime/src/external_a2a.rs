//! Controller calls share tool execution, but are not model tool history.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

/// The actual executor's mailbox, not a sender supplied in an RPC request.
/// An empty root denotes the daemon's logical namespace; local mailboxes
/// retain their explicit root. Neither value asserts transport availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxBinding {
    pub actor: String,
    pub namespace_root: String,
}

/// Host-created provenance carried through the scheduler and its hook rewrites.
#[derive(Debug, Clone)]
pub struct ExternalA2aCall {
    pub command_id: String,
    pub session_id: String,
    pub binding: MailboxBinding,
    pub tool: String,
    pub requested_input_sha256: String,
    effective_input_sha256: Arc<Mutex<Option<String>>>,
}

impl ExternalA2aCall {
    pub fn new(
        session_id: &str,
        binding: MailboxBinding,
        tool: &str,
        input: &str,
    ) -> Result<Self, String> {
        if !matches!(tool, "send" | "agent_list") {
            return Err("external A2A only admits send and agent_list".to_string());
        }
        Ok(Self {
            command_id: format!("external-a2a-{}", uuid::Uuid::new_v4()),
            session_id: session_id.to_string(),
            binding,
            tool: tool.to_string(),
            requested_input_sha256: input_digest(
                tool,
                &serde_json::from_str(input).map_err(|error| error.to_string())?,
            )?,
            effective_input_sha256: Arc::new(Mutex::new(None)),
        })
    }

    /// Recheck the effective invocation after hooks, at the actual executor.
    /// A hook must not turn the narrow request into another tool, a sender
    /// override or a structured control message.
    pub fn validate_dispatch(
        &self,
        tool: &str,
        input: &serde_json::Value,
        binding: Option<&MailboxBinding>,
        tool_id: Option<&str>,
    ) -> Result<(), String> {
        if binding != Some(&self.binding) || tool_id != Some(self.command_id.as_str()) {
            return Err("external A2A executor binding changed".to_string());
        }
        if tool != self.tool {
            return Err("external A2A hooks cannot change the tool".to_string());
        }
        let fields = input
            .as_object()
            .ok_or("external A2A input must be an object")?;
        let valid = match tool {
            "send" => {
                fields
                    .keys()
                    .all(|key| matches!(key.as_str(), "to" | "message" | "summary"))
                    && fields
                        .get("to")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|to| !to.trim().is_empty())
                    && fields
                        .get("message")
                        .is_some_and(serde_json::Value::is_string)
                    && fields
                        .get("summary")
                        .is_none_or(serde_json::Value::is_string)
            }
            "agent_list" => {
                fields.keys().all(|key| key == "active_only")
                    && fields
                        .get("active_only")
                        .is_none_or(serde_json::Value::is_boolean)
            }
            _ => false,
        };
        if !valid {
            return Err(
                "external A2A hooks must preserve the plain-text request shape and bound sender"
                    .to_string(),
            );
        }
        *self
            .effective_input_sha256
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(input_digest(tool, input)?);
        Ok(())
    }

    #[must_use]
    pub fn report(self, content: String, is_error: bool) -> ExternalToolResult {
        ExternalToolResult {
            audit: ExternalA2aAudit {
                version: 1,
                command_id: self.command_id,
                session_id: self.session_id,
                binding: self.binding,
                tool: self.tool,
                requested_input_sha256: self.requested_input_sha256,
                effective_input_sha256: self
                    .effective_input_sha256
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
                result_sha256: result_digest(&content, is_error),
            },
            content,
            is_error,
        }
    }
}

/// Metadata for the reported tool result, including an opaque error result.
/// It is not a delivery receipt or proof that a cancelled worker has stopped.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalA2aAudit {
    pub version: u32,
    pub command_id: String,
    pub session_id: String,
    #[serde(flatten)]
    pub binding: MailboxBinding,
    pub tool: String,
    pub requested_input_sha256: String,
    /// Present only if the actual executor validated an effective invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_input_sha256: Option<String>,
    pub result_sha256: String,
}

pub struct ExternalToolResult {
    pub content: String,
    pub is_error: bool,
    pub audit: ExternalA2aAudit,
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

/// ASCII field names sort before framing. Only strings and booleans occur in
/// the narrow input schema; type bytes prevent ambiguous concatenation.
fn input_digest(tool: &str, input: &serde_json::Value) -> Result<String, String> {
    let fields: std::collections::BTreeMap<_, _> = input
        .as_object()
        .ok_or("external A2A input must be an object")?
        .iter()
        .collect();
    let mut hash = Sha256::new();
    hash.update(b"sudocode.direct-a2a.input.v1\0");
    frame(&mut hash, tool.as_bytes());
    hash.update((fields.len() as u64).to_be_bytes());
    for (key, value) in fields {
        frame(&mut hash, key.as_bytes());
        match value {
            serde_json::Value::String(text) => {
                hash.update([1]);
                frame(&mut hash, text.as_bytes());
            }
            serde_json::Value::Bool(value) => hash.update([if *value { 3 } else { 2 }]),
            _ => return Err("external A2A input values must be strings or booleans".into()),
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn result_digest(content: &str, is_error: bool) -> String {
    let mut hash = Sha256::new();
    hash.update(b"sudocode.direct-a2a.result.v1\0");
    hash.update([u8::from(is_error)]);
    frame(&mut hash, content.as_bytes());
    format!("{:x}", hash.finalize())
}
