//! Terminal facts share the session journal; the task panel is only a cache.
use super::{checksum, Completion, CompletionJournal, Journal, Source};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Bash,
    Powershell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalSnapshot {
    pub shell: ShellKind,
    pub scope: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub termination: String,
    pub output_truncated: bool,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalMetadata {
    pub shell: ShellKind,
    pub scope: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub termination: String,
    pub output_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalCompletion {
    pub session_id: String,
    pub completion_id: String,
    pub kind: String,
    pub task_id: String,
    pub parent_tool_call_id: String,
    pub origin_trace_id: String,
    pub status: String,
    pub terminal: TerminalMetadata,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskCompletion {
    Subagent(Completion),
    Terminal(TerminalCompletion),
}

impl TaskCompletion {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Subagent(p) => &p.completion_id,
            Self::Terminal(p) => &p.completion_id,
        }
    }

    pub(super) fn clear_message(&mut self) {
        match self {
            Self::Subagent(p) => p.message.clear(),
            Self::Terminal(p) => p.message.clear(),
        }
    }

    pub fn wire_value(&self, tasks: bool) -> Result<Value, String> {
        let mut value = serde_json::to_value(self).map_err(|e| e.to_string())?;
        match self {
            Self::Subagent(p) if tasks => {
                let object = value.as_object_mut().ok_or("invalid completion")?;
                object.remove("agentId");
                object.insert("kind".into(), json!("subagent"));
                object.insert("taskId".into(), json!(p.agent_id));
            }
            Self::Terminal(_) if !tasks => return Err("task_completion_protocol_required".into()),
            _ => {}
        }
        Ok(value)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct TerminalSource {
    task_id: String,
    shell: ShellKind,
    pub(super) owner_instance: String,
    pub(super) state: String,
    pub(super) backgrounded: bool,
    pub(super) snapshot: Option<TerminalSnapshot>,
    pub(super) collected: bool,
    pub(super) collected_in: Option<String>,
}

impl CompletionJournal {
    pub fn register_terminal(
        &self,
        tool: &str,
        task: &str,
        shell: ShellKind,
    ) -> Result<(), String> {
        self.change(|state| {
            if state.deleted.is_some() {
                return Err("session_deleted".into());
            }
            let trace = state
                .active_trace
                .clone()
                .ok_or("no active completion source")?;
            if tool.is_empty() || tool.len() > 512 || task.is_empty() || task.len() > 512 {
                return Err("invalid terminal identity".into());
            }
            if state.sources.contains_key(tool)
                || state.sources.values().any(|s| {
                    s.terminal.as_ref().is_some_and(|t| t.task_id == task)
                        || s.lifecycle
                            .as_ref()
                            .is_some_and(|l| l.agent.agent_id == task)
                })
            {
                return Err("duplicate completion source".into());
            }
            if state.sources.len() >= 10000 {
                return Err("completion_journal_capacity".into());
            }
            state.sources.insert(
                tool.to_owned(),
                Source {
                    ordinal: 0,
                    lifecycle: None,
                    trace_id: trace,
                    manifest_path: String::new(),
                    closed: false,
                    settled: false,
                    payload: None,
                    ack: None,
                    blocked: None,
                    suppressed: false,
                    terminal: Some(TerminalSource {
                        task_id: task.to_owned(),
                        shell,
                        owner_instance: self.instance.clone(),
                        state: "prepared".into(),
                        backgrounded: false,
                        snapshot: None,
                        collected: false,
                        collected_in: None,
                    }),
                },
            );
            Self::check_reservation(state)
        })
    }

    /// Spawn failed before an OS process existed; it is safe to retry the tool.
    pub fn terminal_not_started(&self, task: &str) -> Result<(), String> {
        self.change(|state| {
            let source = state
                .sources
                .values_mut()
                .find(|s| s.terminal.as_ref().is_some_and(|t| t.task_id == task))
                .ok_or("terminal source missing")?;
            source
                .terminal
                .as_mut()
                .ok_or("terminal source missing")?
                .state = "not_started".into();
            source.settled = true;
            Ok(())
        })
    }

    pub fn terminal_started(&self, task: &str) -> Result<(), String> {
        self.change(|state| {
            if state.deleted.is_some() {
                return Ok(());
            }
            let terminal = state
                .sources
                .values_mut()
                .filter_map(|s| s.terminal.as_mut())
                .find(|t| t.task_id == task)
                .ok_or("terminal source missing")?;
            if terminal.snapshot.is_none() {
                terminal.state = "running".into();
            }
            Ok(())
        })
    }

    pub fn terminal_unconfirmed(&self, task: &str) -> Result<(), String> {
        self.change(|state| {
            if state.deleted.is_some() {
                return Ok(());
            }
            let source = state
                .sources
                .values_mut()
                .find(|s| s.terminal.as_ref().is_some_and(|t| t.task_id == task))
                .ok_or("terminal source missing")?;
            let terminal = source.terminal.as_mut().ok_or("terminal source missing")?;
            if terminal.snapshot.is_none() {
                terminal.state = "execution_unconfirmed".into();
                if !source.settled {
                    source.blocked = Some("execution_unconfirmed".into());
                }
            }
            Ok(())
        })
    }

    pub fn terminal_finished(
        &self,
        task: &str,
        mut snapshot: TerminalSnapshot,
    ) -> Result<(), String> {
        if snapshot.scope != "command"
            || !matches!(
                snapshot.termination.as_str(),
                "exit" | "signal" | "cancelled"
            )
            || (snapshot.exit_code.is_none()
                && snapshot.signal.is_none()
                && snapshot.termination != "cancelled")
        {
            return Err("terminal exit evidence missing".into());
        }
        if snapshot.output.len() > crate::background_tasks::TASK_OUTPUT_TAIL_BYTES {
            snapshot.output = output_tail(
                &snapshot.output,
                crate::background_tasks::TASK_OUTPUT_TAIL_BYTES,
            );
            snapshot.output_truncated = true;
        }
        self.change(|state| {
            if state.deleted.is_some() {
                return Ok(());
            }
            let source = state
                .sources
                .values_mut()
                .find(|s| s.terminal.as_ref().is_some_and(|t| t.task_id == task))
                .ok_or("terminal source missing")?;
            let terminal = source.terminal.as_mut().ok_or("terminal source missing")?;
            if terminal.snapshot.is_some() {
                return Ok(());
            }
            if terminal.owner_instance != self.instance || terminal.shell != snapshot.shell {
                return Err("terminal owner mismatch".into());
            }
            terminal.state = "terminal".into();
            terminal.snapshot = Some(snapshot);
            if source.blocked.as_deref() == Some("execution_unconfirmed") {
                source.blocked = None;
            }
            Self::publish_terminals(state)
        })
    }

    pub(super) fn terminal_tool_result(state: &mut Journal, tool: &str, name: &str, output: &str) {
        let Ok(value) = serde_json::from_str::<Value>(output) else {
            return;
        };
        if let Some(source) = state.sources.get_mut(tool) {
            if let Some(terminal) = &mut source.terminal {
                terminal.backgrounded =
                    value["backgroundTaskId"].as_str() == Some(terminal.task_id.as_str());
            }
        }
        if crate::tool_names::canonicalize_tool_name(name) != "pid_output"
            || value["retrieval_status"] != "success"
        {
            return;
        }
        let Some(task) = value["pid"].as_str() else {
            return;
        };
        if let Some(source) = state
            .sources
            .values_mut()
            .find(|s| s.terminal.as_ref().is_some_and(|t| t.task_id == task))
        {
            if source.payload.is_none() {
                if let Some(terminal) = &mut source.terminal {
                    if terminal.snapshot.is_some()
                        && matches!(
                            value["status"].as_str(),
                            Some("completed" | "failed" | "cancelled")
                        )
                    {
                        terminal.collected_in.clone_from(&state.active_trace);
                    }
                }
            }
        }
    }

    pub(super) fn publish_terminals(state: &mut Journal) -> Result<(), String> {
        if state.deleted.is_some() {
            return Ok(());
        }
        for (tool, source) in &mut state.sources {
            let Some(terminal) = &source.terminal else {
                continue;
            };
            if source.settled
                || source.payload.is_some()
                || !source.closed
                || source.blocked.is_some()
            {
                continue;
            }
            if terminal.collected {
                source.settled = true;
                continue;
            }
            let Some(snapshot) = &terminal.snapshot else {
                continue;
            };
            if !terminal.backgrounded || state.active_trace.is_some() {
                continue;
            }
            let id = checksum(
                &serde_json::to_vec(&(
                    "taskCompletions:1",
                    &state.session_id,
                    "terminal",
                    &terminal.task_id,
                    &source.trace_id,
                    tool,
                ))
                .map_err(|e| e.to_string())?,
            );
            let mut payload = TerminalCompletion {
                session_id: state.session_id.clone(),
                completion_id: id,
                kind: "terminal".into(),
                task_id: terminal.task_id.clone(),
                parent_tool_call_id: tool.clone(),
                origin_trace_id: source.trace_id.clone(),
                status: terminal_status(snapshot).into(),
                terminal: TerminalMetadata {
                    shell: snapshot.shell,
                    scope: snapshot.scope.clone(),
                    exit_code: snapshot.exit_code,
                    signal: snapshot.signal,
                    termination: snapshot.termination.clone(),
                    output_truncated: snapshot.output_truncated,
                },
                message: String::new(),
            };
            let mut output = snapshot.output.clone();
            loop {
                // The controller forwards this message verbatim to the model;
                // envelope-only metadata cannot inform the continuation.
                let metadata =
                    serde_json::to_string(&payload.terminal).map_err(|e| e.to_string())?;
                payload.message = format!(
                    "<task-notification><task-id>{}</task-id><status>{}</status><terminal>{}</terminal><result>{}</result></task-notification>",
                    escape(&payload.task_id), payload.status, escape(&metadata), escape(&output)
                );
                if serde_json::to_vec(&payload)
                    .map_err(|e| e.to_string())?
                    .len()
                    <= 65536
                {
                    break;
                }
                if output.is_empty() {
                    return Err("terminal identity exceeds payload budget".into());
                }
                output = output_tail(&output, output.len() / 2);
                payload.terminal.output_truncated = true;
            }
            source.ordinal = state.watermark;
            source.payload = Some(TaskCompletion::Terminal(payload));
        }
        Ok(())
    }

    pub fn terminal_result(&self, task: &str) -> Result<Option<Value>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "completion journal poisoned")?;
        if state.deleted.is_some() {
            return Err("session_deleted".into());
        }
        Ok(state
            .sources
            .values()
            .find(|s| s.terminal.as_ref().is_some_and(|t| t.task_id == task))
            .and_then(Self::terminal_view))
    }

    pub(super) fn terminal_view(source: &Source) -> Option<Value> {
        let terminal = source.terminal.as_ref()?;
        let snapshot = terminal.snapshot.as_ref();
        Some(
            json!({ "pid": terminal.task_id, "kind": "terminal", "originTraceId": source.trace_id,
            "status": snapshot.map_or(terminal.state.as_str(), terminal_status),
            "stdout": snapshot.map(|s| &s.output), "exit_code": snapshot.and_then(|s| s.exit_code),
            "terminal": snapshot, "deliverySuppressed": source.suppressed,
            "retrieval_status": if snapshot.is_some() { "success" } else { "not_ready" }, "fullOutputAvailable": false }),
        )
    }

    // Reserve the largest remaining serialized state, including JSON escaping.
    // ACK/suppress shrink only the notification copy; a late terminal still fits.
    pub(super) fn check_reservation(state: &Journal) -> Result<(), String> {
        let actual = serde_json::to_vec(state).map_err(|e| e.to_string())?.len();
        let mut projected = actual
            - serde_json::to_vec(&state.sources)
                .map_err(|e| e.to_string())?
                .len()
            - serde_json::to_vec(&state.reducer)
                .map_err(|e| e.to_string())?
                .len()
            + serde_json::to_vec(&state.reducer.collected)
                .map_err(|e| e.to_string())?
                .len()
            + 64 * 1024;
        for source in state.sources.values() {
            let current = serde_json::to_vec(source).map_err(|e| e.to_string())?.len();
            // Fixed maximum final footprints, not "current + spare" (which
            // would consume the spare again when a reserved result arrives).
            let ceiling = if source.terminal.is_some() {
                24 * 1024 * 6 + 65536 + 32768
            } else if source.settled {
                current + 8192
            } else {
                65536 * 12 + 65536
            };
            projected = projected.saturating_add(current.max(ceiling));
        }
        if projected > 64 * 1024 * 1024 {
            return Err(
                "completion_journal_capacity: reserved terminal results exceed 64 MiB".into(),
            );
        }
        Ok(())
    }
}

fn terminal_status(snapshot: &TerminalSnapshot) -> &str {
    if snapshot.termination == "cancelled" {
        "cancelled"
    } else if snapshot.exit_code == Some(0) {
        "completed"
    } else {
        "failed"
    }
}

fn output_tail(value: &str, max: usize) -> String {
    let mut start = value.len().saturating_sub(max);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    value[start..].to_owned()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
