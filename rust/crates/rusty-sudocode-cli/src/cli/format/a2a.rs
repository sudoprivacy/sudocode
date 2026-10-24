//! Presentation only: never replace the tool result persisted or sent to the model.

use serde_json::Value;
use std::fmt::Write as _;

use super::{
    generic_tool_card, tool_label, truncate_for_summary, truncate_output_for_display,
    ToolCardContent, ToolStatus, TOOL_OUTPUT_DISPLAY_MAX_LINES,
};

/// Bound prose previews without clipping the exact addresses needed by `send`.
#[inline]
fn preview(text: &str) -> String {
    truncate_output_for_display(text.trim(), 3, 240)
}

#[inline]
fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A successful tool call acknowledges durable inbox writes, not receiver activity.
#[inline]
fn send_receipt(target: &str, output: &Value) -> Option<String> {
    if output.get("success").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    if target != "*" {
        return Some("Written to inbox".into());
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
    let Some(target) = string(input, "to") else {
        return generic_tool_card("send", input, output, status);
    };
    let header = format!("{} → {target}", tool_label("send"));
    let mut body = Vec::new();
    // Keep the exact address in the wrapping body too: titles clip at narrow widths.
    body.push(format!("To: {target}"));
    if let Some(summary) = string(input, "summary").filter(|s| !s.trim().is_empty()) {
        body.push(format!("Summary: {}", preview(summary)));
    }
    if let Some(message) = input.get("message") {
        match message {
            Value::String(text) => body.push(preview(text)),
            Value::Object(_) => {
                let Some(kind) = string(message, "type") else {
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
                    if let Some(text) = string(message, key) {
                        body.push(preview(text));
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
        body.insert(0, receipt);
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
        if let Some(id) = string(output, "request_id").or_else(|| {
            input
                .get("message")
                .and_then(|message| string(message, "request_id"))
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
            .any(|key| string(agent, key).is_none())
    }) {
        return generic_tool_card("agent_list", input, Some(output), status);
    }
    let _ = write!(header, " · {} agents", agents.len());
    if agents.is_empty() {
        return ToolCardContent::new(header, "No agents found".into());
    }
    let mut rows = Vec::new();
    for agent in agents.iter().take(TOOL_OUTPUT_DISPLAY_MAX_LINES) {
        let name = string(agent, "name").unwrap_or_default();
        let address = string(agent, "send_to").unwrap_or_default();
        let kind = string(agent, "kind").unwrap_or_default();
        let mut row = format!("{kind}  {name}");
        if address != name {
            let _ = write!(row, " · send_to={address}");
        }
        if let Some(active) = agent.get("active").and_then(Value::as_bool) {
            row.push_str(if active { " · active" } else { " · inactive" });
        }
        for key in ["role", "task_id"] {
            if let Some(value) = string(agent, key).filter(|value| *value != address) {
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
