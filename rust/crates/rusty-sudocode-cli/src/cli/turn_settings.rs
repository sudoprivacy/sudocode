//! Settings commands while a turn is running. The turn holds the session for
//! its whole length, so a picker opened during it reads a snapshot taken before
//! the turn started, and the choice applies when the turn ends: the running
//! turn keeps its model and effort.

use std::sync::{Arc, Mutex, PoisonError};

use commands::SlashCommand;

use crate::render::{theme, TextRole, RESET};
use crate::repl_ui::{OutputSender, QuestionOptionView, QuestionPromptView, UiCommandSender};
use crate::{show_slash_selection, LiveCli, SlashSelectionHandler};

/// What the settings pickers show during a turn, taken by the runner when the
/// turn starts. Settings change only between turns, so it stays accurate.
#[derive(Debug, Clone, Default)]
pub(crate) struct SettingsView {
    model: String,
    models: Vec<String>,
    effort_preference: Option<String>,
    effort_effective: Option<String>,
    /// The resolved model the effort levels belong to.
    effort_model: String,
    /// Effort levels the current model accepts; empty when it has none.
    effort_levels: Vec<String>,
}

impl SettingsView {
    /// Taken under the session, before a turn starts.
    pub(crate) fn capture(cli: &LiveCli) -> Self {
        let model = cli.lifecycle.current_model();
        let (effort_preference, effort_effective) = cli.lifecycle.current_effort();
        let (effort_model, effort_levels) = cli
            .lifecycle
            .effort_options(&model)
            .unwrap_or_else(|_| (model.clone(), Vec::new()));
        Self {
            models: cli.lifecycle.known_models(),
            model,
            effort_preference,
            effort_effective,
            effort_model,
            effort_levels,
        }
    }
}

/// Settings chosen during a turn, applied when it ends.
#[derive(Debug, Default)]
pub(crate) struct PendingSettings {
    model: Option<String>,
    /// Picked from the list rather than typed, so its effort is asked when it
    /// applies, as the idle picker asks.
    is_model_picked: bool,
    effort: Option<String>,
}

pub(crate) type SharedPendingSettings = Arc<Mutex<PendingSettings>>;
pub(crate) type SharedSettingsView = Arc<Mutex<SettingsView>>;

/// Run `/model` or `/effort` while a turn is in progress. Returns the picker
/// that takes the answer, when one opened.
pub(crate) fn handle_during_turn(
    command: &SlashCommand,
    view: &SettingsView,
    pending: &SharedPendingSettings,
    ui: &UiCommandSender,
    out: &OutputSender,
) -> Option<SlashSelectionHandler> {
    match command {
        SlashCommand::Model { model: Some(model) } => {
            record_model(pending, out, model.clone(), false);
            None
        }
        SlashCommand::Model { model: None } => {
            // A choice already made during this turn is what applies next.
            let current = pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .model
                .clone()
                .unwrap_or_else(|| view.model.clone());
            let pending = Arc::clone(pending);
            Some(show_slash_selection(
                ui,
                model_question(&view.models, &current),
                move |model, _, out| {
                    record_model(&pending, out, model, true);
                    None
                },
            ))
        }
        SlashCommand::Effort { level: Some(level) } => {
            record_effort(pending, out, level.clone());
            None
        }
        SlashCommand::Effort { level: None } if !view.effort_levels.is_empty() => {
            let pending = Arc::clone(pending);
            Some(show_slash_selection(
                ui,
                effort_question(
                    &view.effort_model,
                    view.effort_effective.as_deref(),
                    view.effort_preference.as_deref(),
                    &view.effort_levels,
                    false,
                ),
                move |level, _, out| {
                    record_effort(&pending, out, level);
                    None
                },
            ))
        }
        SlashCommand::Effort { level: None } => {
            out.println(&format_effort_line(
                view.effort_preference.as_deref(),
                view.effort_effective.as_deref(),
            ));
            None
        }
        _ => None,
    }
}

/// Apply the settings chosen during the turn that just ended, through the same
/// paths as `/model <name>` and `/effort <level>` typed when idle. Returns the
/// effort picker when a model picked from the list asks for one.
pub(crate) fn apply_pending(
    pending: &SharedPendingSettings,
    cli: &Arc<Mutex<LiveCli>>,
    ui: &UiCommandSender,
    out: &OutputSender,
) -> Option<SlashSelectionHandler> {
    let PendingSettings {
        model,
        is_model_picked,
        effort,
    } = std::mem::take(&mut *pending.lock().unwrap_or_else(PoisonError::into_inner));
    if let Some(model) = model {
        if is_model_picked && effort.is_none() {
            if let Some(picker) = crate::show_effort_selection(ui, cli, Some(model.clone())) {
                return Some(picker);
            }
        }
        crate::run_session_command(cli, SlashCommand::Model { model: Some(model) }, ui, out);
    }
    if let Some(level) = effort {
        crate::run_session_command(cli, SlashCommand::Effort { level: Some(level) }, ui, out);
    }
    None
}

fn record_model(pending: &SharedPendingSettings, out: &OutputSender, model: String, picked: bool) {
    out.println(&note(&format!(
        "Model switches to {model} after this turn."
    )));
    let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
    pending.model = Some(model);
    pending.is_model_picked = picked;
}

fn record_effort(pending: &SharedPendingSettings, out: &OutputSender, level: String) {
    out.println(&note(&format!(
        "Effort switches to {level} after this turn."
    )));
    pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .effort = Some(level);
}

fn note(text: &str) -> String {
    format!("{}{text}{RESET}", theme().fg_seq(TextRole::Secondary))
}

/// The model picker, idle or during a turn.
pub(crate) fn model_question(models: &[String], current: &str) -> QuestionPromptView {
    let options = models
        .iter()
        .map(|model| QuestionOptionView {
            label: model.clone(),
            value: model.clone(),
            description: None,
            recommended: model == current,
            is_navigable: false,
        })
        .collect();
    QuestionPromptView {
        presentation: runtime::QuestionPresentation::Question,
        title: Some("Model".to_string()),
        description: Some(format!("Current: {current}").into()),
        index: 0,
        total: 1,
        prompt: "Select model".to_string(),
        options,
        allow_custom_input: true,
        custom_input_hint: Some("or type a model name".to_string()),
        force_fuzzy_select: false,
        back_value: None,
        initial_value: None,
    }
}

/// The effort picker for `model`, idle or during a turn. `is_second_step` when
/// it follows the model picker.
pub(crate) fn effort_question(
    model: &str,
    effective: Option<&str>,
    preference: Option<&str>,
    levels: &[String],
    is_second_step: bool,
) -> QuestionPromptView {
    let options = std::iter::once("auto")
        .chain(levels.iter().map(String::as_str))
        .map(|level| QuestionOptionView {
            label: level.to_string(),
            value: level.to_string(),
            description: (level == "max").then(|| "Current session only".to_string()),
            recommended: preference.unwrap_or("auto") == level,
            is_navigable: false,
        })
        .collect();
    QuestionPromptView {
        presentation: runtime::QuestionPresentation::Question,
        title: Some("Reasoning effort".to_string()),
        description: Some(
            format!("Model: {model} · Current: {}", effective.unwrap_or("auto")).into(),
        ),
        index: usize::from(is_second_step),
        total: if is_second_step { 2 } else { 1 },
        prompt: "Select effort".to_string(),
        options,
        allow_custom_input: false,
        custom_input_hint: None,
        force_fuzzy_select: true,
        back_value: None,
        initial_value: None,
    }
}

/// The `/effort` report line.
pub(crate) fn format_effort_line(preference: Option<&str>, effective: Option<&str>) -> String {
    let scope = if preference == Some("max") {
        " (this session only)"
    } else {
        ""
    };
    format!(
        "Effort: {} · Effective: {}{scope}",
        preference.unwrap_or("auto"),
        effective.unwrap_or("model default")
    )
}
