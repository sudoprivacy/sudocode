//! Ordered session storage work behind the existing engine command seam.
//! Only one blocking task runs at a time. Input admission, status and cancel
//! stay on the pump; persistence and its errors retain their original authority.

use std::collections::VecDeque;
use std::sync::Arc;

use engine_events::{EngineEvent, InteractionAction, InteractionReceipt};

use crate::session_inputs::InteractionControlLease;
use crate::subagent_completion::EventSink;
use crate::EngineDelegate;

const MAX_CONTROLS: usize = 32;

enum Operation {
    Command {
        id: u64,
        action: InteractionAction,
        refresh: bool,
    },
    Refresh,
    Retry,
    Scan,
    CompletionDelivery,
    Completion {
        id: Option<u64>,
        action: engine_events::CompletionAction,
    },
}

struct Work {
    operation: Operation,
    // Admission fences session replacement until durable work has completed.
    // Only binding changes also hold model execution; ordinary replies do not.
    lease: InteractionControlLease,
}

struct Completed {
    lease: InteractionControlLease,
    command: Option<(u64, Result<serde_json::Value, String>)>,
    direct: Option<InteractionReceipt>,
}

#[derive(Default)]
// These are independent coalesced wakeups, not mutually exclusive modes.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct InteractionControls {
    pending: VecDeque<Work>,
    active: Option<tokio::task::JoinHandle<Completed>>,
    active_id: Option<u64>,
    ready_direct: Option<InteractionReceipt>,
    retired_direct: Option<InteractionReceipt>,
    refresh: bool,
    retry: bool,
    scan: bool,
    completions: bool,
}

impl InteractionControls {
    pub(crate) fn completion(
        &mut self,
        id: Option<u64>,
        action: engine_events::CompletionAction,
        sink: &EventSink,
    ) {
        if self.pending.len() + usize::from(self.active.is_some()) >= MAX_CONTROLS {
            let message =
                "Completion controls are busy; retry this request after a response".to_owned();
            let event = id.map_or_else(
                || EngineEvent::Error {
                    message: message.clone(),
                },
                |id| EngineEvent::CommandResult {
                    id,
                    result: Err(message.clone()),
                },
            );
            let _ = sink.send(event);
            return;
        }
        let binding = matches!(action, engine_events::CompletionAction::Configure { .. });
        self.pending.push_back(Work {
            operation: Operation::Completion { id, action },
            lease: sink.inputs.begin_completion_control(binding),
        });
    }

    pub(crate) fn command(
        &mut self,
        id: u64,
        action: InteractionAction,
        running: bool,
        sink: &EventSink,
    ) {
        let configure = matches!(action, InteractionAction::Configure { .. });
        if matches!(
            action,
            InteractionAction::Configure { .. } | InteractionAction::Reply { .. }
        ) {
            self.scan = true;
        }
        // In-process hosts can replace an idle session through their lifecycle
        // port. Resolve that binding before using its journal, just as on a
        // lifecycle release. Active-turn replies retain the current binding.
        let refresh = configure || !running || sink.interactions.needs_refresh();
        let admission = if self.pending.len() + usize::from(self.active.is_some()) >= MAX_CONTROLS {
            Err("Interaction controls are busy; retry this request after a response".into())
        } else if refresh && running {
            Err("configure interactions before starting a turn".into())
        } else {
            sink.inputs.begin_interaction_control(refresh)
        };
        match admission {
            Ok(lease) => self.pending.push_back(Work {
                operation: Operation::Command {
                    id,
                    action,
                    refresh,
                },
                lease,
            }),
            Err(message) => {
                let _ = sink.send(EngineEvent::CommandResult {
                    id,
                    result: Err(message),
                });
            }
        }
    }

    pub(crate) fn retry(&mut self) {
        self.retry = true;
        self.scan = true;
    }

    pub(crate) fn refresh(&mut self, sink: &EventSink) {
        if sink.interactions.invalidate_binding() {
            self.refresh = true;
            if let Some(receipt) = self.ready_direct.take() {
                self.retired_direct = Some(receipt);
            }
        }
    }

    pub(crate) fn binding_pending(&self) -> bool {
        self.refresh
    }

    pub(crate) fn scan(&mut self) {
        self.scan = true;
    }

    pub(crate) fn deliver_completions(&mut self) {
        self.completions = true;
    }

    pub(crate) fn take_direct(&mut self) -> Option<InteractionReceipt> {
        self.ready_direct.take()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn start_next(&mut self, delegate: &Arc<dyn EngineDelegate>, sink: &EventSink) {
        if self.active.is_some() {
            return;
        }
        if !sink.interactions.is_enabled() {
            self.retry = false;
            self.scan = false;
        }
        // A lifecycle release must rebind before a later reply can run. Work
        // already admitted prevents that lifecycle from starting in the first place.
        let operation = if self.refresh {
            Some(Operation::Refresh)
        } else if !self.pending.is_empty() {
            None
        } else if self.retry {
            Some(Operation::Retry)
        } else if self.completions {
            Some(Operation::CompletionDelivery)
        } else if self.scan && self.ready_direct.is_none() {
            Some(Operation::Scan)
        } else {
            return;
        };
        let work = if let Some(operation) = operation {
            let Ok(lease) = sink
                .inputs
                .begin_interaction_control(matches!(operation, Operation::Refresh))
            else {
                // A host lifecycle or turn owns the binding. Its release wakes
                // the pump; retain one coalesced retry instead of spinning.
                return;
            };
            match operation {
                Operation::Refresh => self.refresh = false,
                Operation::Retry => self.retry = false,
                Operation::Scan => self.scan = false,
                Operation::CompletionDelivery => self.completions = false,
                Operation::Command { .. } | Operation::Completion { .. } => unreachable!(),
            }
            Work { operation, lease }
        } else {
            self.pending
                .pop_front()
                .expect("queued interaction control")
        };
        let delegate = delegate.clone();
        let sink = sink.clone();
        self.active_id = match &work.operation {
            Operation::Command { id, .. } => Some(*id),
            Operation::Completion { id, .. } => *id,
            _ => None,
        };
        let retired = if matches!(work.operation, Operation::Refresh) {
            self.retired_direct.take()
        } else {
            None
        };
        self.active = Some(tokio::task::spawn_blocking(move || {
            Self::perform(work, delegate.as_ref(), &sink, retired)
        }));
    }

    fn perform(
        work: Work,
        delegate: &dyn EngineDelegate,
        sink: &EventSink,
        retired: Option<InteractionReceipt>,
    ) -> Completed {
        let Work { operation, lease } = work;
        let (command, direct) = match operation {
            Operation::Command {
                id,
                action,
                refresh,
            } => {
                // Refresh requires the admitted binding barrier. Ordinary
                // replies never acquire the model's session lock.
                let result = sink.interactions.command(delegate, action, sink, !refresh);
                (Some((id, result)), None)
            }
            Operation::Refresh => {
                if let Some(receipt) = retired {
                    sink.interactions.return_delivery(receipt);
                }
                if let Err(message) = sink.interactions.refresh(delegate, sink) {
                    let _ = sink.send(EngineEvent::InteractionError { message });
                }
                (None, None)
            }
            Operation::Retry => {
                sink.interactions.retry(delegate, sink);
                (None, None)
            }
            Operation::Scan => match sink.interactions.next_direct() {
                Ok(receipt) => (None, receipt),
                Err(message) => {
                    let _ = sink.send(EngineEvent::InteractionError { message });
                    (None, None)
                }
            },
            Operation::CompletionDelivery => {
                sink.completions.deliver(sink);
                (None, None)
            }
            Operation::Completion { id, action } => {
                let result = sink.completions.command(action);
                if let Some(id) = id {
                    (Some((id, result)), None)
                } else {
                    if let Err(message) = result {
                        let _ = sink.send(EngineEvent::Error { message });
                    }
                    (None, None)
                }
            }
        };
        Completed {
            lease,
            command,
            direct,
        }
    }

    pub(crate) async fn finished(&mut self, sink: &EventSink) {
        let result = self
            .active
            .as_mut()
            .expect("active interaction control")
            .await;
        self.active = None;
        let id = self.active_id.take();
        match result {
            Ok(Completed {
                lease,
                command,
                direct,
            }) => {
                if let Some(receipt) = direct {
                    self.ready_direct = Some(receipt);
                }
                // A lifecycle cannot pass the admission fence before the pump
                // has observed this result. Release before acknowledging it.
                drop(lease);
                if let Some((id, result)) = command {
                    let _ = sink.send(EngineEvent::CommandResult { id, result });
                }
            }
            Err(error) => {
                if let Some(id) = id {
                    let _ = sink.send(EngineEvent::CommandResult {
                        id,
                        result: Err(format!("interaction control failed: {error}; persistence may have completed, query the original request before retrying")),
                    });
                }
                let _ = sink.send(EngineEvent::InteractionError {
                    message: format!("interaction control failed: {error}"),
                });
            }
        }
    }

    /// Stop delivery before draining the one admitted storage task. An already
    /// persisted decision remains durable; shutdown never turns it into an effect.
    pub(crate) async fn close(&mut self, sink: &EventSink) {
        sink.interactions.begin_close();
        for work in self.pending.drain(..) {
            if let Operation::Command { id, .. } | Operation::Completion { id: Some(id), .. } =
                work.operation
            {
                let _ = sink.send(EngineEvent::CommandResult {
                    id,
                    result: Err("Session closed before this interaction control started".into()),
                });
            }
        }
        if self.active.is_some() {
            self.finished(sink).await;
        }
        let sink = sink.clone();
        let _ = tokio::task::spawn_blocking(move || sink.interactions.release(&sink)).await;
    }
}
