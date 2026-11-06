//! Sub-agent cards: `agent_spawn` and `pid_fork` start one; `pid_output`,
//! `pid_status` and `pid_kill` read or stop it. One reader, [`AgentState`],
//! takes what the user needs from any of their result shapes (state, pid,
//! result), so the five cards cannot disagree. A shape it doesn't recognise
//! is shown raw rather than dropped. Presentation only: the result sent to the
//! model is untouched.

use serde_json::Value;
use std::fmt::Write as _;

use super::{
    identity_annotation, tool_label, truncate_output_for_display, value_card, ToolCardContent,
    TOOL_OUTPUT_DISPLAY_MAX_CHARS, TOOL_OUTPUT_DISPLAY_MAX_LINES,
};

#[inline]
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

/// What one sub-agent or background-task result says about itself.
struct AgentState<'a> {
    pid: &'a str,
    status: &'a str,
    /// `pid_output` answered before the task finished.
    pending: bool,
    exit_code: Option<i64>,
    result: Option<&'a str>,
    name: Option<&'a str>,
}

impl<'a> AgentState<'a> {
    /// `None` unless the value names a task and its status.
    fn read(value: &'a Value) -> Option<Self> {
        Some(Self {
            pid: ["agentId", "agent_id", "pid"]
                .into_iter()
                .find_map(|key| text(value, key))?,
            status: text(value, "status")?,
            pending: matches!(
                text(value, "retrieval_status"),
                Some("timeout" | "not_ready")
            ),
            exit_code: value.get("exit_code").and_then(Value::as_i64),
            result: ["result", "stdout"]
                .into_iter()
                .find_map(|key| text(value, key)),
            name: text(value, "name"),
        })
    }

    /// `Running in background`, `Completed · exit 0`, `Still running`, …
    fn summary(&self, spawned: bool) -> String {
        let mut line = if self.pending {
            "Still running".to_string()
        } else if spawned && self.status == "running" {
            "Running in background".to_string()
        } else {
            let status = self.status.replace('_', " ");
            let mut chars = status.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        };
        if let Some(code) = self.exit_code {
            let _ = write!(line, " · exit {code}");
        }
        line
    }

    /// The summary, then the result within the display caps.
    fn body(&self, first_line: String) -> String {
        let mut body = first_line;
        if let Some(result) = self.result {
            body.push('\n');
            body.push_str(&truncate_output_for_display(
                result.trim_end(),
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            ));
        }
        body
    }
}

/// `AgentSpawn · Explore · audit · <description>`, then `Running in
/// background · pid …` or, for a synchronous run, its outcome and result.
pub(super) fn spawn_card(name: &str, input: &Value, output: Option<&Value>) -> ToolCardContent {
    let mut header = tool_label(name);
    // The tool still accepts the legacy `subagent_type` for `agent`.
    let agent = text(input, "agent").or_else(|| text(input, "subagent_type"));
    for value in [agent, text(input, "name")].into_iter().flatten() {
        let _ = write!(header, " · {value}");
    }
    header.push_str(&identity_annotation(input));
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    let Some(state) = AgentState::read(output) else {
        return value_card(header, output);
    };
    let line = format!("{} · pid {}", state.summary(true), state.pid);
    ToolCardContent::new(header, state.body(line))
}

/// `PidOutput · <pid>` (or `PidStatus` listing every agent), then the state.
pub(super) fn pid_card(name: &str, input: &Value, output: Option<&Value>) -> ToolCardContent {
    let mut header = tool_label(name);
    let pid = text(input, "pid").or_else(|| text(input, "task_id"));
    if let Some(pid) = pid {
        let _ = write!(header, " · {pid}");
    }
    if input.get("merge").and_then(Value::as_bool) == Some(true) {
        header.push_str(" · merge");
    }
    if input.get("backgrounded_only").and_then(Value::as_bool) == Some(true) {
        header.push_str(" · background only");
    }
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    if pid.is_none() {
        if let Some(agents) = output.get("agents").and_then(Value::as_array) {
            return list_card(header, agents, output);
        }
    }
    let Some(state) = AgentState::read(output) else {
        return value_card(header, output);
    };
    let mut line = state.summary(false);
    if let Some(name) = state.name {
        let _ = write!(line, " · {name}");
    }
    ToolCardContent::new(header, state.body(line))
}

/// Every agent `pid_status` reported, one row each.
fn list_card(mut header: String, agents: &[Value], output: &Value) -> ToolCardContent {
    let Some(states) = agents
        .iter()
        .map(AgentState::read)
        .collect::<Option<Vec<_>>>()
    else {
        return value_card(header, output);
    };
    let _ = write!(header, " · {} agents", states.len());
    if states.is_empty() {
        return ToolCardContent::new(header, "No agents".into());
    }
    let mut rows: Vec<_> = states
        .iter()
        .take(TOOL_OUTPUT_DISPLAY_MAX_LINES)
        .map(|state| {
            let mut row = format!("{} · pid {}", state.summary(false), state.pid);
            if let Some(name) = state.name {
                let _ = write!(row, " · {name}");
            }
            row
        })
        .collect();
    if states.len() > rows.len() {
        rows.push(format!("+{} more agents", states.len() - rows.len()));
    }
    ToolCardContent::new(header, rows.join("\n"))
}
