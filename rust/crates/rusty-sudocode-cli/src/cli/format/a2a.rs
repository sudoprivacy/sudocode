//! Presentation only: never replace the tool result persisted or sent to the model.

use engine_core::engine_events::{
    EngineState, InputReason, RuntimeAvailability, RuntimeObservation, RuntimePresence,
};
use serde_json::Value;
use std::fmt::Write as _;

use super::{
    generic_tool_card, text, tool_label, truncate_for_summary, truncate_output_for_display,
    ToolCardContent, ToolStatus, TOOL_OUTPUT_DISPLAY_MAX_LINES,
};

/// The same sender-supplied summary appears on outgoing and incoming cards.
#[inline]
pub(crate) fn summary_preview(text: &str) -> String {
    format!(
        "Summary: {}",
        truncate_output_for_display(text.trim(), 3, 240)
    )
}

/// Bound the message body independently of its concise summary and address.
#[inline]
fn message_preview(text: &str) -> String {
    truncate_output_for_display(text.trim(), 6, 960)
}

/// A successful tool call acknowledges durable inbox writes, not receiver activity.
#[inline]
fn send_receipt(target: &str, output: &Value) -> Option<String> {
    if output.get("success").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if target != "*" {
        return Some("Sent to inbox".into());
    }
    let recipients = output.get("recipients")?.as_array()?;
    if recipients.is_empty() {
        return Some("No recipients · no inboxes written".into());
    }
    let names = recipients
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?;
    let shown = names
        .iter()
        .take(TOOL_OUTPUT_DISPLAY_MAX_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = if names.len() > TOOL_OUTPUT_DISPLAY_MAX_LINES {
        format!(
            " · +{} recipients in session",
            names.len() - TOOL_OUTPUT_DISPLAY_MAX_LINES
        )
    } else {
        String::new()
    };
    Some(format!(
        "Written to {} inboxes: {shown}{remaining}",
        names.len()
    ))
}

pub(super) fn send_card(
    input: &Value,
    output: Option<&Value>,
    status: ToolStatus,
) -> ToolCardContent {
    let Some(target) = text(input, "to") else {
        return generic_tool_card("send", input, output, status);
    };
    let mut header = format!("{} → {target}", tool_label("send"));
    let mut body = Vec::new();
    if let Some(summary) = text(input, "summary") {
        body.push(summary_preview(summary));
    }
    if let Some(message) = input.get("message") {
        match message {
            Value::String(text) => body.push(message_preview(text)),
            Value::Object(_) => {
                let Some(kind) = text(message, "type") else {
                    return generic_tool_card("send", input, output, status);
                };
                if !matches!(
                    kind,
                    "shutdown_request" | "shutdown_response" | "plan_approval_response"
                ) {
                    return generic_tool_card("send", input, output, status);
                }
                let mut intent = kind.to_string();
                if matches!(kind, "shutdown_response" | "plan_approval_response") {
                    let approve = message
                        .get("approve")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    intent.push_str(if approve {
                        " · approved"
                    } else {
                        " · rejected"
                    });
                }
                body.push(intent);
                for key in ["reason", "feedback"] {
                    if let Some(text) = text(message, key) {
                        body.push(message_preview(text));
                    }
                }
            }
            _ => return generic_tool_card("send", input, output, status),
        }
    }
    if let Some(output) = output.filter(|_| status != ToolStatus::Error) {
        let Some(receipt) = send_receipt(target, output) else {
            return generic_tool_card("send", input, Some(output), status);
        };
        if target == "*" {
            body.insert(0, receipt);
        } else {
            let _ = write!(header, " · {receipt}");
        }
        if let Some(signaled) = output.get("live_abort_signaled").and_then(Value::as_bool) {
            body.push(
                if signaled {
                    "Local cancellation signaled"
                } else {
                    "Local cancellation not signaled"
                }
                .into(),
            );
        }
        if let Some(id) = text(output, "request_id").or_else(|| {
            input
                .get("message")
                .and_then(|message| text(message, "request_id"))
        }) {
            body.push(format!("Request: {id}"));
        }
    }
    ToolCardContent::new(header, body.join("\n"))
}

pub(super) fn agent_list_card(
    input: &Value,
    output: Option<&Value>,
    status: ToolStatus,
) -> ToolCardContent {
    let mut header = tool_label("agent_list");
    if input.get("active_only").and_then(Value::as_bool) == Some(true) {
        header.push_str(" · active only");
    }
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    let Some(agents) = output.get("agents").and_then(Value::as_array) else {
        return generic_tool_card("agent_list", input, Some(output), status);
    };
    // Unknown shapes retain their generic result instead of silently losing data.
    if agents.iter().any(|agent| {
        ["name", "send_to", "kind"]
            .iter()
            .any(|key| text(agent, key).is_none())
    }) {
        return generic_tool_card("agent_list", input, Some(output), status);
    }
    let _ = write!(header, " · {} agents", agents.len());
    let is_incomplete = output.get("runtime_discovery").is_some_and(|coverage| {
        coverage.get("unsupported").and_then(Value::as_bool) == Some(true)
            || coverage
                .get("unknown_instances")
                .and_then(Value::as_u64)
                .is_some_and(|count| count > 0)
            || coverage
                .get("unavailable")
                .and_then(Value::as_array)
                .is_some_and(|scopes| !scopes.is_empty())
    });
    if is_incomplete {
        header.push_str(" · observations incomplete");
    }
    if agents.is_empty() {
        return ToolCardContent::new(
            header,
            if is_incomplete {
                "Runtime observations unavailable"
            } else {
                "No agents found"
            }
            .into(),
        );
    }
    let mut rows = Vec::new();
    for agent in agents.iter().take(TOOL_OUTPUT_DISPLAY_MAX_LINES) {
        let name = text(agent, "name").unwrap_or_default();
        let address = text(agent, "send_to").unwrap_or_default();
        let kind = text(agent, "kind").unwrap_or_default();
        let mut row = format!("{kind}  {name}");
        if address != name {
            let _ = write!(row, " · send_to={address}");
        }
        if let Some(description) = text(agent, "description") {
            let _ = write!(row, " · {}", truncate_for_summary(description, 160));
            if let Some(age) = text(agent, "description_updated_at")
                .and_then(runtime::agent_description::relative_age)
            {
                let _ = write!(row, " · updated {age}");
            }
        }
        if let Some(runtime) = agent.get("runtime") {
            let Ok(observation) = serde_json::from_value::<RuntimeObservation>(runtime.clone())
            else {
                return generic_tool_card("agent_list", input, Some(output), status);
            };
            row.push_str(&runtime_labels(&observation));
        } else if let Some(active) = agent.get("active").and_then(Value::as_bool) {
            row.push_str(if active { " · active" } else { " · inactive" });
        }
        for key in ["role", "task_id"] {
            if let Some(value) = text(agent, key).filter(|value| *value != address) {
                let _ = write!(row, " · {key}={}", truncate_for_summary(value, 80));
            }
        }
        rows.push(row);
    }
    if agents.len() > rows.len() {
        rows.push(format!(
            "… +{} agents · full list preserved in session",
            agents.len() - rows.len()
        ));
    }
    ToolCardContent::new(header, rows.join("\n"))
}

fn runtime_labels(observation: &RuntimeObservation) -> String {
    let identity = &observation.instance;
    let mut labels = format!(
        " · instance {}/{}",
        identity.registry_id.chars().take(8).collect::<String>(),
        identity.incarnation.chars().take(8).collect::<String>()
    );
    labels.push_str(if observation.presence == RuntimePresence::Online {
        " · online"
    } else {
        " · presence unknown"
    });
    let Some(update) = observation
        .update
        .as_ref()
        .filter(|_| observation.availability == RuntimeAvailability::Fresh)
    else {
        labels.push_str(" · activity unknown");
        return labels;
    };
    labels.push_str(match update.status.foreground {
        EngineState::Idle => " · idle",
        EngineState::Running => " · running",
        EngineState::AwaitingInput(InputReason::Permission) => " · waiting for permission",
        EngineState::AwaitingInput(InputReason::Question) => " · waiting for answer",
    });
    let tasks = update.status.background;
    if tasks.terminals > 0
        || tasks.agents > 0
        || tasks.execution_unconfirmed > 0
        || tasks.tool_operations > 0
    {
        let _ = write!(
            labels,
            " · background shells={} agents={} · operations={} · unconfirmed={}",
            tasks.terminals, tasks.agents, tasks.tool_operations, tasks.execution_unconfirmed
        );
    }
    labels
}
