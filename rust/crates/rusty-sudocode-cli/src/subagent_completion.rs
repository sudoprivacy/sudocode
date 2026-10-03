//! Turn-boundary delivery for this session's background sub-agents.
//!
//! A lifecycle can finish before its spawning tool returns. Wait for that
//! return to distinguish synchronous results from background handoffs, including
//! auto-backgrounded synchronous calls. A result explicitly collected during
//! the turn must not also trigger a redundant follow-up turn.

use std::collections::BTreeMap;

use engine_core::EngineEvent;
use engine_events::{SubagentLifecycle, SubagentPhase, SubagentUpdate};

#[derive(Default)]
struct Worker {
    agent_id: String,
    background: bool,
    finished: Option<Box<SubagentLifecycle>>,
}

#[derive(Default)]
pub struct Completions {
    active: bool,
    workers: BTreeMap<String, Worker>,
}

impl Completions {
    #[inline]
    pub fn observe(&mut self, event: &EngineEvent) -> Vec<SubagentLifecycle> {
        match event {
            EngineEvent::TurnStarted { .. } => self.active = true,
            EngineEvent::TurnComplete(_) | EngineEvent::Error { .. } => self.active = false,
            EngineEvent::Subagent(event) if event.stream.is_none() => {
                if let SubagentUpdate::Lifecycle(lifecycle) = &event.update {
                    match lifecycle.phase {
                        SubagentPhase::Started => {
                            self.workers.insert(
                                lifecycle.tool_call_id.clone(),
                                Worker {
                                    agent_id: lifecycle.agent.agent_id.clone(),
                                    ..Worker::default()
                                },
                            );
                        }
                        SubagentPhase::Finished => {
                            if let Some(worker) = self.workers.get_mut(&lifecycle.tool_call_id) {
                                worker.finished = Some(lifecycle.clone());
                            }
                        }
                    }
                }
            }
            EngineEvent::ToolResult { id, output, .. } => {
                // Only parse results that can affect a tracked worker.
                if !self.workers.is_empty() {
                    // Coordinator mode returns the notification itself from
                    // pid_output rather than its ordinary JSON view.
                    self.workers.retain(|_, worker| {
                        worker
                            .finished
                            .as_ref()
                            .and_then(|finished| finished.completion_notification.as_deref())
                            != Some(output.as_str())
                    });
                    let manifest = serde_json::from_str::<serde_json::Value>(output).ok();
                    if let Some(worker) = self.workers.get_mut(id) {
                        let handed_off = manifest.as_ref().is_some_and(|value| {
                            matches!(value["status"].as_str(), Some("running" | "backgrounded"))
                        });
                        if handed_off {
                            worker.background = true;
                        } else {
                            self.workers.remove(id);
                        }
                    }
                    if let Some(manifest) = manifest {
                        if matches!(
                            manifest["status"].as_str(),
                            Some("completed" | "failed" | "killed" | "cancelled")
                        ) {
                            if let Some(agent_id) = manifest["agentId"]
                                .as_str()
                                .or_else(|| manifest["agent_id"].as_str())
                            {
                                self.workers.retain(|_, worker| worker.agent_id != agent_id);
                            }
                        }
                    }
                }
            }
            _ => return Vec::new(),
        }
        if self.active {
            return Vec::new();
        }
        let mut ready = Vec::new();
        self.workers.retain(|_, worker| {
            if worker.background {
                if let Some(finished) = worker.finished.take() {
                    ready.push(*finished);
                    return false;
                }
            }
            true
        });
        ready
    }
}
