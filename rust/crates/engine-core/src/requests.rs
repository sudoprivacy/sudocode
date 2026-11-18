//! Outstanding answers, input ownership and attention have one request owner.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use engine_events::{
    AttentionRequest, EngineCommand, EngineEvent, EngineState, InputReason,
    PermissionPromptDecision, QuestionPromptAnswer, RequestId, RequestPhase,
};
use tokio::sync::oneshot;

use crate::subagent_completion::EventSink;

pub(crate) enum PendingAnswer {
    HostContext(oneshot::Sender<Result<String, String>>),
    Permission(oneshot::Sender<PermissionPromptDecision>),
    Question(oneshot::Sender<Result<Vec<QuestionPromptAnswer>, String>>),
}

#[derive(Clone, Default)]
pub(crate) struct RequestTable {
    requests: Arc<Mutex<RequestState>>,
    next_id: Arc<AtomicU64>,
    interaction: runtime::PromptQueue,
    scope: Option<Arc<AtomicBool>>,
}

#[derive(Default)]
struct RequestState {
    pending: BTreeMap<RequestId, PendingRequest>,
    cancelled: bool,
    published_reason: Option<InputReason>,
}

struct PendingRequest {
    // Input ownership survives answering until the reply future releases it.
    answer: Option<PendingAnswer>,
    attention: Option<AttentionRequest>,
    origin: Option<runtime::PermissionPromptOrigin>,
    scope: Option<Arc<AtomicBool>>,
}

impl PendingRequest {
    fn is_background(&self) -> bool {
        self.origin
            .as_ref()
            .is_some_and(runtime::PermissionPromptOrigin::is_detached)
    }
}

impl RequestState {
    fn publish(&mut self, tx: &EventSink) {
        for request in self.pending.values_mut() {
            if let (Some(attention), Some(origin)) = (&mut request.attention, &request.origin) {
                attention.agent = Some(origin.identity());
            }
        }
        let reason = self.pending.values().find_map(|request| {
            request.attention.as_ref().and_then(|attention| {
                (attention.phase == RequestPhase::Presented && !request.is_background())
                    .then_some(attention.reason)
            })
        });
        if reason != self.published_reason {
            self.published_reason = reason;
            let state = reason.map_or(EngineState::Running, EngineState::AwaitingInput);
            let _ = tx.send(EngineEvent::State(state));
        }
        tx.publish_attention(
            self.pending
                .values()
                .filter(|request| request.answer.is_some())
                .filter_map(|request| request.attention.clone())
                .collect(),
        );
    }
}

impl RequestTable {
    pub(crate) fn new(next_id: Arc<AtomicU64>) -> Self {
        Self {
            next_id,
            ..Self::default()
        }
    }

    /// A turn owns its foreground requests; detached children retain the
    /// session registry and can ask after that turn has completed normally.
    pub(crate) fn turn(&self) -> Self {
        Self {
            scope: Some(Arc::new(AtomicBool::new(false))),
            ..self.clone()
        }
    }

    pub(crate) fn refresh(&self, tx: &EventSink) {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .publish(tx);
    }

    /// Completion and foreground cancellation retire the same input scope.
    /// Detached tasks retain their own lifetime until stopped or session close.
    pub(crate) fn close_turn(&self, tx: &EventSink) {
        let Some(scope) = &self.scope else { return };
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        scope.store(true, Ordering::Relaxed);
        requests.pending.retain(|_, request| {
            let owned = request
                .scope
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, scope));
            !owned || request.is_background()
        });
        requests.publish(tx);
    }

    pub(crate) fn take(
        &self,
        id: RequestId,
        tx: &EventSink,
        accepts: impl FnOnce(&PendingAnswer) -> bool,
    ) -> Option<PendingAnswer> {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let request = requests.pending.get_mut(&id)?;
        if !accepts(request.answer.as_ref()?)
            || request
                .attention
                .as_ref()
                .is_some_and(|view| view.phase != RequestPhase::Presented)
        {
            return None;
        }
        let answer = request.answer.take();
        requests.publish(tx);
        answer
    }

    pub(crate) fn answer(&self, command: EngineCommand, tx: &EventSink) {
        match command {
            EngineCommand::PermissionAnswer { id, decision } => {
                if let Some(PendingAnswer::Permission(reply)) = self.take(id, tx, |answer| {
                    matches!(answer, PendingAnswer::Permission(_))
                }) {
                    let _ = reply.send(decision);
                }
            }
            EngineCommand::QuestionAnswer { id, answers } => {
                if let Some(PendingAnswer::Question(reply)) = self.take(id, tx, |answer| {
                    matches!(answer, PendingAnswer::Question(_))
                }) {
                    let _ = reply.send(answers);
                }
            }
            EngineCommand::HostContextAnswer { id, context } => {
                if let Some(PendingAnswer::HostContext(reply)) = self.take(id, tx, |answer| {
                    matches!(answer, PendingAnswer::HostContext(_))
                }) {
                    let _ = reply.send(context);
                }
            }
            _ => unreachable!("only request answers are routed to the request table"),
        }
    }

    pub(crate) fn cancel_all(&self, tx: &EventSink) {
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        requests.cancelled = true;
        requests.pending.clear();
        requests.publish(tx);
    }

    /// Register before waiting for input ownership, so queued requests are
    /// visible and cancellation also removes futures that were never polled.
    pub(crate) fn enqueue<T: Send + 'static>(
        &self,
        tx: &EventSink,
        answer: PendingAnswer,
        agent: Option<runtime::PermissionPromptOrigin>,
        event: impl FnOnce(RequestId) -> EngineEvent,
        reply: impl std::future::Future<Output = T> + Send + 'static,
    ) -> runtime::PromptReply<T> {
        let (lease, event) = self.register(tx, answer, agent, event);
        self.interaction.enqueue(move || async move {
            lease.present(event);
            let answer = reply.await;
            drop(lease);
            answer
        })
    }

    pub(crate) fn begin_request(
        &self,
        tx: &EventSink,
        answer: PendingAnswer,
        event: impl FnOnce(RequestId) -> EngineEvent,
    ) -> RequestLease {
        let (lease, event) = self.register(tx, answer, None, event);
        lease.present(event);
        lease
    }

    fn register(
        &self,
        tx: &EventSink,
        answer: PendingAnswer,
        agent: Option<runtime::PermissionPromptOrigin>,
        event: impl FnOnce(RequestId) -> EngineEvent,
    ) -> (RequestLease, EngineEvent) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let event = event(id);
        let detail = match &event {
            EngineEvent::PermissionRequest { request, .. } => Some((
                InputReason::Permission,
                request.tool_name.as_str(),
                request.input.as_str(),
            )),
            EngineEvent::QuestionRequest { request, .. } => Some((
                InputReason::Question,
                "",
                request
                    .title
                    .as_deref()
                    .or_else(|| request.fields.first().map(|field| field.prompt.as_str()))
                    .unwrap_or("Question"),
            )),
            _ => None,
        };
        let attention = detail.map(|(reason, prefix, summary)| AttentionRequest {
            id,
            reason,
            phase: RequestPhase::Queued,
            agent: agent
                .as_ref()
                .map(runtime::PermissionPromptOrigin::identity),
            summary: prefix
                .chars()
                .chain(if prefix.is_empty() { "" } else { " · " }.chars())
                .chain(summary.chars())
                .take(240)
                .collect(),
        });
        let mut requests = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let is_background = agent
            .as_ref()
            .is_some_and(runtime::PermissionPromptOrigin::is_detached);
        let admitted = self
            .scope
            .as_ref()
            .is_none_or(|scope| !scope.load(Ordering::Relaxed) || is_background);
        if !requests.cancelled && admitted {
            requests.pending.insert(
                id,
                PendingRequest {
                    answer: Some(answer),
                    attention,
                    origin: agent,
                    scope: self.scope.clone(),
                },
            );
            requests.publish(tx);
        }
        (
            RequestLease {
                table: self.clone(),
                tx: tx.clone(),
                id,
            },
            event,
        )
    }
}

pub(crate) struct RequestLease {
    table: RequestTable,
    tx: EventSink,
    id: RequestId,
}

impl RequestLease {
    fn present(&self, event: EngineEvent) {
        let mut requests = self
            .table
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(request) = requests.pending.get_mut(&self.id) {
            if let Some(attention) = &mut request.attention {
                attention.phase = RequestPhase::Presented;
            }
            requests.publish(&self.tx);
            let _ = self.tx.send(event);
        }
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        let mut requests = self
            .table
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if requests.pending.remove(&self.id).is_some() {
            requests.publish(&self.tx);
        }
    }
}
