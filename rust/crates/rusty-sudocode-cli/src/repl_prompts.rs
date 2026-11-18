//! Session-owned input surfaces, driven by outstanding engine requests.
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::cli::interactions::{answer_for_field, permission_question, question_field};
use crate::repl_ui;
use engine_core::{EngineCommand, EngineEvent};
use runtime::{PermissionPrompter, QuestionPrompter};

pub(crate) type PendingQuestionAnswer = Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>;

pub(crate) fn consume_pending_question_answer(
    pending: &PendingQuestionAnswer,
    text: String,
) -> bool {
    let Some(tx) = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return false;
    };
    let _ = tx.send(text);
    true
}

pub(crate) fn cancel_pending_question_answer(pending: &PendingQuestionAnswer) {
    let _ = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

/// Keep the event pump draining while the existing input owner awaits a reply.
/// The engine serializes interactions, so at most one reply worker is parked.
fn relay_prompt_reply<T: Send + 'static>(
    commands: mpsc::Sender<EngineCommand>,
    reply: runtime::PromptReply<T>,
    answer: impl FnOnce(T) -> EngineCommand + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = commands.send(answer(futures::executor::block_on(reply)));
    })
}

#[derive(Clone)]
struct IocraftQuestionPrompter {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    ui: repl_ui::UiCommandSender,
    pending_answer: PendingQuestionAnswer,
    agent: Option<String>,
}

impl IocraftQuestionPrompter {
    fn new(ui: repl_ui::UiCommandSender, pending_answer: PendingQuestionAnswer) -> Self {
        Self {
            ui,
            pending_answer,
            agent: None,
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn show_field(
        &self,
        request: &runtime::QuestionPromptRequest,
        index: usize,
        earlier: Option<&runtime::QuestionPromptAnswer>,
    ) {
        let mut field = question_field(request, index, earlier);
        field.title = match (&self.agent, &field.title) {
            (Some(agent), Some(title)) => Some(format!("{agent} · {title}")),
            (Some(agent), None) => Some(agent.clone()),
            (None, title) => title.clone(),
        };
        self.ui.show_turn_question(field);
    }

    fn prepare_answer_receiver(&self) -> Result<tokio::sync::oneshot::Receiver<String>, String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = self
                .pending_answer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("question prompt cancelled".into());
            }
            if pending.is_some() {
                return Err("question prompt already pending".to_string());
            }
            *pending = Some(tx);
        }
        Ok(rx)
    }
}

impl runtime::QuestionPrompter for IocraftQuestionPrompter {
    fn cancel_pending(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        cancel_pending_question_answer(&self.pending_answer);
        self.ui.clear_turn_question();
    }

    fn ask(
        &mut self,
        request: &runtime::QuestionPromptRequest,
    ) -> Result<Vec<runtime::QuestionPromptAnswer>, String> {
        futures::executor::block_on(self.begin_question(request))
    }

    fn begin_question(
        &mut self,
        request: &runtime::QuestionPromptRequest,
    ) -> runtime::PromptReply<Result<Vec<runtime::QuestionPromptAnswer>, String>> {
        let this = self.clone();
        let request = request.clone();
        Box::pin(async move {
            // Answers are kept per question so going back restores them.
            let mut answers: Vec<Option<runtime::QuestionPromptAnswer>> =
                vec![None; request.fields.len()];
            let mut index = 0;
            while index < request.fields.len() {
                let rx = this.prepare_answer_receiver()?;
                this.show_field(&request, index, answers[index].as_ref());
                let Ok(raw) = rx.await else {
                    this.ui.clear_turn_question();
                    return Err("question prompt cancelled".into());
                };
                if raw == repl_ui::QUESTION_BACK {
                    index = index.saturating_sub(1);
                    continue;
                }
                answers[index] = Some(answer_for_field(&request.fields[index], raw));
                index += 1;
            }
            this.ui.clear_turn_question();
            Ok(answers.into_iter().flatten().collect())
        })
    }
}

impl runtime::PermissionPrompter for IocraftQuestionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        futures::executor::block_on(self.begin_decision(request))
    }

    fn begin_decision(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PromptReply<runtime::PermissionPromptDecision> {
        let question = permission_question(request);
        let reply = runtime::QuestionPrompter::begin_question(self, &question);
        Box::pin(async move {
            match reply.await {
                Ok(answers) if answers.first().is_some_and(|a| a.value == "allow") => {
                    runtime::PermissionPromptDecision::Allow
                }
                _ => runtime::PermissionPromptDecision::Deny {
                    reason: "tool permission denied or cancelled".into(),
                },
            }
        })
    }
}

struct ActivePrompt {
    id: engine_core::RequestId,
    prompter: IocraftQuestionPrompter,
    worker: thread::JoinHandle<()>,
}

pub(crate) struct SessionPrompts {
    ui: repl_ui::UiCommandSender,
    pending: PendingQuestionAnswer,
    commands: mpsc::Sender<EngineCommand>,
    attention: Arc<engine_events::AttentionState>,
    active: Option<ActivePrompt>,
}

impl SessionPrompts {
    pub(crate) fn new(
        ui: repl_ui::UiCommandSender,
        pending: PendingQuestionAnswer,
        commands: mpsc::Sender<EngineCommand>,
    ) -> Self {
        Self {
            ui,
            pending,
            commands,
            attention: Arc::default(),
            active: None,
        }
    }

    fn clear(&mut self) {
        if let Some(mut active) = self.active.take() {
            active.prompter.cancel_pending();
            let _ = active.worker.join();
        }
    }

    /// Consume requests even when no foreground turn renderer is running.
    pub(crate) fn observe(&mut self, event: &EngineEvent) -> bool {
        if let EngineEvent::Attention(state) = event {
            if self
                .active
                .as_ref()
                .is_some_and(|active| !state.requests.iter().any(|request| request.id == active.id))
            {
                self.clear();
            }
            self.attention = state.clone();
            self.ui.attention(state.clone());
            return true;
        }
        let id = match event {
            EngineEvent::PermissionRequest { id, .. } | EngineEvent::QuestionRequest { id, .. } => {
                *id
            }
            _ => return false,
        };
        self.clear();
        let mut prompter = IocraftQuestionPrompter::new(self.ui.clone(), self.pending.clone());
        // Provenance comes from the request owner, never from generated text.
        prompter.agent = self
            .attention
            .requests
            .iter()
            .find(|request| request.id == id)
            .and_then(|request| request.agent.as_ref())
            .map(|agent| agent.name.clone());
        let commands = self.commands.clone();
        let worker = match event {
            EngineEvent::PermissionRequest { request, .. } => relay_prompt_reply(
                commands,
                prompter.begin_decision(request),
                move |decision| EngineCommand::PermissionAnswer { id, decision },
            ),
            EngineEvent::QuestionRequest { request, .. } => {
                relay_prompt_reply(commands, prompter.begin_question(request), move |answers| {
                    EngineCommand::QuestionAnswer { id, answers }
                })
            }
            _ => unreachable!(),
        };
        self.active = Some(ActivePrompt {
            id,
            prompter,
            worker,
        });
        true
    }
}

impl Drop for SessionPrompts {
    fn drop(&mut self) {
        self.clear();
    }
}
