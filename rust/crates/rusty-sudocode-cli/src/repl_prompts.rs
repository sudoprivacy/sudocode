//! Session-owned input surfaces, driven by outstanding engine requests.
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use crate::cli::interactions::permission_question;
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
        earlier: Option<&[runtime::QuestionPromptAnswer]>,
    ) {
        let field = &request.fields[index];
        let multi_select = field.kind == runtime::QuestionKind::MultiSelect;
        self.ui.show_turn_question(repl_ui::QuestionPromptView {
            presentation: request.presentation.clone(),
            title: match (&self.agent, &request.title) {
                (Some(agent), Some(title)) => Some(format!("{agent} · {title}")),
                (Some(agent), None) => Some(agent.clone()),
                (None, title) => title.clone(),
            },
            description: request.description.clone(),
            index,
            total: request.fields.len(),
            prompt: field.prompt.clone(),
            options: field
                .options
                .iter()
                .map(|option| repl_ui::QuestionOptionView {
                    label: option.label.clone(),
                    value: option.value.clone(),
                    description: option.description.clone(),
                    recommended: option.recommended,
                    is_navigable: false,
                })
                .collect(),
            allow_custom_input: field.allow_custom_input,
            custom_input_hint: field.custom_input_hint.clone(),
            force_fuzzy_select: false,
            back_value: (index > 0).then(|| repl_ui::QUESTION_BACK.to_string()),
            initial_value: earlier.and_then(|answers| {
                if multi_select {
                    let values: Vec<String> =
                        answers.iter().map(|answer| answer.value.clone()).collect();
                    Some(repl_ui::multi_select_answer(&values))
                } else {
                    answers.first().map(|answer| answer.value.clone())
                }
            }),
            multi_select,
        });
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

    /// One answer per chosen value: a multi-select dialog sends them together.
    fn answers_for_field(
        field: &runtime::QuestionField,
        raw_answer: String,
    ) -> Vec<runtime::QuestionPromptAnswer> {
        match repl_ui::multi_select_values(&raw_answer) {
            Some(values) => values
                .into_iter()
                .map(|value| Self::answer_for_field(field, value))
                .collect(),
            None => vec![Self::answer_for_field(field, raw_answer)],
        }
    }

    fn answer_for_field(
        field: &runtime::QuestionField,
        raw_answer: String,
    ) -> runtime::QuestionPromptAnswer {
        let matched = field
            .options
            .iter()
            .find(|option| option.value == raw_answer);
        let label = matched
            .map(|option| option.label.clone())
            .or_else(|| (!raw_answer.is_empty()).then(|| raw_answer.clone()));
        runtime::QuestionPromptAnswer {
            id: field.id.clone(),
            value: raw_answer,
            label,
        }
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
            let mut answers: Vec<Option<Vec<runtime::QuestionPromptAnswer>>> =
                vec![None; request.fields.len()];
            let mut index = 0;
            while index < request.fields.len() {
                let rx = this.prepare_answer_receiver()?;
                this.show_field(&request, index, answers[index].as_deref());
                let Ok(raw) = rx.await else {
                    this.ui.clear_turn_question();
                    return Err("question prompt cancelled".into());
                };
                if raw == repl_ui::QUESTION_BACK {
                    index = index.saturating_sub(1);
                    continue;
                }
                answers[index] = Some(Self::answers_for_field(&request.fields[index], raw));
                index += 1;
            }
            this.ui.clear_turn_question();
            Ok(answers.into_iter().flatten().flatten().collect())
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

#[cfg(test)]
mod tests {
    use super::IocraftQuestionPrompter;

    #[test]
    fn a_multi_select_answer_becomes_one_answer_per_chosen_option() {
        let field = runtime::QuestionField {
            id: "cleanup".into(),
            prompt: "Delete which?".into(),
            kind: runtime::QuestionKind::MultiSelect,
            required: true,
            allow_custom_input: true,
            custom_input_hint: None,
            options: ["worktree", "target"]
                .map(|value| runtime::QuestionOption {
                    label: value.to_uppercase(),
                    value: value.into(),
                    description: None,
                    recommended: false,
                })
                .into(),
        };
        let raw = crate::repl_ui::multi_select_answer(&["target".into(), "typed".into()]);
        let answers: Vec<_> = IocraftQuestionPrompter::answers_for_field(&field, raw)
            .into_iter()
            .map(|answer| (answer.id, answer.value, answer.label))
            .collect();
        assert_eq!(
            answers,
            [
                ("cleanup".into(), "target".into(), Some("TARGET".into())),
                ("cleanup".into(), "typed".into(), Some("typed".into())),
            ]
        );
        // Any other answer stays a single answer, even if it looks like a list.
        let single = IocraftQuestionPrompter::answers_for_field(&field, r#"["target"]"#.into());
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].value, r#"["target"]"#);
    }
}
