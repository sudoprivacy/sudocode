//! Terminal projection of session-owned questions. This owns only navigation
//! and unfinished answer fields; requests and accepted decisions stay in engine.
use std::collections::HashSet;
use std::sync::mpsc::Sender;

use engine_events::{
    EngineCommand, Interaction, InteractionAction, InteractionDeliveryOwner, InteractionReply,
    InteractionRequest, InteractionSnapshot, InteractionStatus, PermissionRequest, QuestionField,
    QuestionKind, QuestionOption, QuestionPresentation, QuestionPromptAnswer,
    QuestionPromptRequest,
};

use crate::repl_ui::{self, InteractionFocus, OutputSender, UiCommandSender};

pub(crate) fn permission_question(request: &PermissionRequest) -> QuestionPromptRequest {
    QuestionPromptRequest {
        presentation: QuestionPresentation::Question,
        title: Some(format!("Approve {}?", request.tool_name)),
        description: Some(
            format!(
                "{}\n{}",
                request.input,
                request.reason.as_deref().unwrap_or_default()
            )
            .into(),
        ),
        fields: vec![QuestionField {
            id: "permission".into(),
            prompt: "Allow this tool call?".into(),
            kind: QuestionKind::SingleSelect,
            required: true,
            allow_custom_input: false,
            custom_input_hint: None,
            options: vec![
                QuestionOption {
                    label: "Allow once".into(),
                    value: "allow".into(),
                    description: None,
                    recommended: false,
                },
                QuestionOption {
                    label: "Deny".into(),
                    value: "deny".into(),
                    description: None,
                    recommended: false,
                },
            ],
        }],
    }
}

fn presentation(request: &InteractionRequest) -> std::borrow::Cow<'_, QuestionPromptRequest> {
    match request {
        InteractionRequest::Question(question) => std::borrow::Cow::Borrowed(question),
        InteractionRequest::Permission(permission) => {
            std::borrow::Cow::Owned(permission_question(permission))
        }
    }
}

pub(crate) fn question_field(
    request: &QuestionPromptRequest,
    index: usize,
    earlier: Option<&[QuestionPromptAnswer]>,
) -> repl_ui::QuestionPromptView {
    let field = &request.fields[index];
    let multi_select = field.kind == QuestionKind::MultiSelect;
    repl_ui::QuestionPromptView {
        presentation: request.presentation.clone(),
        title: request.title.clone(),
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
    }
}

/// One answer per chosen value: a multi-select dialog sends them together.
pub(crate) fn answers_for_field(
    field: &QuestionField,
    raw_answer: String,
) -> Vec<QuestionPromptAnswer> {
    match repl_ui::multi_select_values(&raw_answer) {
        Some(values) => values
            .into_iter()
            .map(|value| answer_for_field(field, value))
            .collect(),
        None => vec![answer_for_field(field, raw_answer)],
    }
}

fn answer_for_field(field: &QuestionField, value: String) -> QuestionPromptAnswer {
    let label = field
        .options
        .iter()
        .find(|option| option.value == value)
        .map(|option| option.label.clone())
        .or_else(|| (!value.is_empty()).then(|| value.clone()));
    QuestionPromptAnswer {
        id: field.id.clone(),
        value,
        label,
    }
}

struct FocusedQuestion {
    interaction: Interaction,
    index: usize,
    answers: Vec<Option<Vec<QuestionPromptAnswer>>>,
}

struct PendingReview {
    id: Option<String>,
}

#[derive(Default)]
pub(crate) struct Questions {
    session_id: Option<String>,
    snapshot: Option<InteractionSnapshot>,
    focused: Option<FocusedQuestion>,
    pending_review: Option<PendingReview>,
    command_id: u64,
    commands: HashSet<u64>,
}

impl Questions {
    fn send(&mut self, action: InteractionAction, commands: &Sender<EngineCommand>) {
        self.command_id += 1;
        self.commands.insert(self.command_id);
        let _ = commands.send(EngineCommand::Interactions {
            id: self.command_id,
            action,
        });
    }

    pub(crate) fn configure(&mut self, commands: &Sender<EngineCommand>) {
        self.send(
            InteractionAction::Configure {
                enabled: true,
                delivery: InteractionDeliveryOwner::Host,
            },
            commands,
        );
    }

    pub(crate) fn refresh(
        &mut self,
        session_id: String,
        commands: &Sender<EngineCommand>,
        ui: &UiCommandSender,
    ) {
        if runtime::experiments::is_enabled(
            runtime::experiments::Experiment::NonblockingInteractions,
        ) {
            if self.session_id.as_ref() != Some(&session_id) {
                self.session_id = Some(session_id);
                self.snapshot = None;
                self.dismiss(ui);
                ui.set_status_tip(repl_ui::StatusSource::Interactions, None);
            }
            self.send(InteractionAction::Snapshot, commands);
        }
    }

    pub(crate) fn result(
        &mut self,
        id: u64,
        result: Result<serde_json::Value, String>,
        output: &OutputSender,
    ) {
        if self.commands.remove(&id) {
            if let Err(message) = result {
                self.pending_review = None;
                output.println(&format!("Question request failed: {message}"));
            }
        }
    }

    pub(crate) fn update(
        &mut self,
        snapshot: InteractionSnapshot,
        ui: &UiCommandSender,
        output: &OutputSender,
    ) {
        // A storage response can arrive after the host selected another
        // session. It must never reopen a question from the old projection.
        if self
            .session_id
            .as_ref()
            .is_some_and(|id| id != &snapshot.session_id)
        {
            return;
        }
        if self.snapshot.as_ref().is_some_and(|current| {
            current.session_id == snapshot.session_id && current.revision > snapshot.revision
        }) {
            return;
        }
        self.session_id = Some(snapshot.session_id.clone());
        let pending = snapshot
            .requests
            .iter()
            .filter(|r| r.status == InteractionStatus::Pending)
            .count();
        let previous = self.snapshot.as_ref().map_or(0, |s| {
            s.requests
                .iter()
                .filter(|r| r.status == InteractionStatus::Pending)
                .count()
        });
        if pending > previous {
            output.println(&format!("{pending} request(s) waiting · Alt+Q or /questions to review · chat remains available"));
        }
        ui.set_status_tip(
            repl_ui::StatusSource::Interactions,
            (pending > 0).then(|| engine_core::Tip {
                priority: engine_core::TipPriority::Important,
                message: format!("{pending} request(s) waiting · Alt+Q to review"),
            }),
        );
        if self.focused.as_ref().is_some_and(|focused| {
            !snapshot.requests.iter().any(|r| {
                r.id == focused.interaction.id
                    && r.revision == focused.interaction.revision
                    && r.status == InteractionStatus::Pending
            })
        }) {
            self.dismiss(ui);
        }
        self.snapshot = Some(snapshot);
        if let Some(review) = self.pending_review.take() {
            self.open_loaded(review.id.as_deref(), ui, output);
        }
    }

    pub(crate) fn open(
        &mut self,
        id: Option<&str>,
        commands: &Sender<EngineCommand>,
        ui: &UiCommandSender,
        output: &OutputSender,
    ) {
        if self.snapshot.is_none() {
            self.pending_review = Some(PendingReview {
                id: id.map(str::to_owned),
            });
            self.send(InteractionAction::Snapshot, commands);
            return;
        }
        self.open_loaded(id, ui, output);
    }

    fn open_loaded(&mut self, id: Option<&str>, ui: &UiCommandSender, output: &OutputSender) {
        let pending: Vec<_> = self
            .snapshot
            .iter()
            .flat_map(|s| &s.requests)
            .filter(|r| r.status == InteractionStatus::Pending)
            .collect();
        let selected = pending
            .iter()
            .find(|r| id.is_none_or(|id| r.id == id))
            .copied();
        let Some(selected) = selected else {
            output.println("No matching question is waiting for an answer.");
            return;
        };
        let request = presentation(&selected.request);
        if request.fields.is_empty() {
            output.println("This question has no answer fields.");
            return;
        }
        if pending.len() > 1 {
            for item in &pending {
                let question = presentation(&item.request);
                output.println(&format!(
                    "/questions {} · {}",
                    item.id,
                    question.title.as_deref().unwrap_or("Question")
                ));
            }
        }
        self.focused = Some(FocusedQuestion {
            interaction: selected.clone(),
            index: 0,
            answers: vec![None; request.fields.len()],
        });
        self.show(ui);
    }

    fn show(&self, ui: &UiCommandSender) {
        let Some(focused) = &self.focused else {
            return;
        };
        let request = presentation(&focused.interaction.request);
        ui.show_interaction(
            InteractionFocus {
                id: focused.interaction.id.clone(),
                revision: focused.interaction.revision,
                index: focused.index,
            },
            question_field(
                &request,
                focused.index,
                focused.answers[focused.index].as_deref(),
            ),
        );
    }

    pub(crate) fn dismiss(&mut self, ui: &UiCommandSender) {
        self.pending_review = None;
        self.focused = None;
        ui.close_interaction();
    }

    pub(crate) fn cancel(
        &mut self,
        id: &str,
        commands: &Sender<EngineCommand>,
        ui: &UiCommandSender,
        output: &OutputSender,
    ) {
        let Some(request) = self
            .snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.requests)
            .find(|request| request.id == id && request.status == InteractionStatus::Pending)
        else {
            output.println("No matching question is waiting for an answer.");
            return;
        };
        let action = InteractionAction::Reply {
            interaction_id: request.id.clone(),
            revision: request.revision,
            reply: InteractionReply::Cancelled {
                reason: "Cancelled by the user".into(),
            },
        };
        if self
            .focused
            .as_ref()
            .is_some_and(|focused| focused.interaction.id == id)
        {
            self.dismiss(ui);
        }
        self.send(action, commands);
    }

    pub(crate) fn answer(
        &mut self,
        focus: &InteractionFocus,
        value: String,
        commands: &Sender<EngineCommand>,
        ui: &UiCommandSender,
    ) {
        let Some(focused) = &mut self.focused else {
            return;
        };
        if focus.id != focused.interaction.id
            || focus.revision != focused.interaction.revision
            || focus.index != focused.index
        {
            return;
        }
        if value == repl_ui::QUESTION_BACK {
            focused.index = focused.index.saturating_sub(1);
        } else {
            let request = presentation(&focused.interaction.request);
            focused.answers[focused.index] =
                Some(answers_for_field(&request.fields[focused.index], value));
            focused.index += 1;
            if focused.index == request.fields.len() {
                let action = InteractionAction::Reply {
                    interaction_id: focused.interaction.id.clone(),
                    revision: focused.interaction.revision,
                    reply: match &focused.interaction.request {
                        InteractionRequest::Question(_) => InteractionReply::Answers {
                            answers: focused
                                .answers
                                .iter()
                                .flatten()
                                .flatten()
                                .cloned()
                                .collect(),
                        },
                        InteractionRequest::Permission(_) => InteractionReply::Permission {
                            approved: focused
                                .answers
                                .first()
                                .and_then(Option::as_ref)
                                .and_then(|answers| answers.first())
                                .is_some_and(|answer| answer.value == "allow"),
                            reason: None,
                        },
                    },
                };
                self.dismiss(ui);
                self.send(action, commands);
                return;
            }
        }
        self.show(ui);
    }
}
