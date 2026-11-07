//! The `AskUserQuestion` card: what was asked and what was chosen. Each
//! question keeps its options in the order the dialog showed them; the chosen
//! one is highlighted and the rest dimmed, and the recommended option is
//! marked wherever it sits, so a choice against the recommendation is visible.
//! A result it cannot read is shown raw rather than dropped.

use serde_json::Value;
use std::fmt::Write as _;

use super::{text, tool_label, truncate_for_summary, value_card, ToolCardContent};
use crate::render::{theme, TextRole, BOLD, DIM, RESET};

/// Widest a prompt or option row may be before it is cut with an ellipsis.
const ROW_CHARS: usize = 160;

/// `AskUserQuestion · <title>` (or the question count), then each question.
pub(super) fn card(input: &Value, output: Option<&Value>) -> ToolCardContent {
    let source = output.unwrap_or(input);
    let questions = source.get("questions").and_then(Value::as_array);
    let mut header = tool_label("AskUserQuestion");
    match (text(source, "title"), questions.map(Vec::len)) {
        (Some(title), _) => {
            let _ = write!(header, " · {}", truncate_for_summary(title, ROW_CHARS));
        }
        (None, Some(1)) => header.push_str(" · 1 question"),
        (None, Some(count)) if count > 1 => {
            let _ = write!(header, " · {count} questions");
        }
        _ => {}
    }
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    let (Some(questions), Some(answers)) =
        (questions, output.get("answers").and_then(Value::as_array))
    else {
        return value_card(header, output);
    };
    let mut rows = Vec::new();
    for question in questions {
        let Some(id) = text(question, "id") else {
            return value_card(header, output);
        };
        let chosen: Vec<&Value> = answers
            .iter()
            .filter(|answer| text(answer, "id") == Some(id))
            .collect();
        rows.extend(question_rows(question, &chosen));
    }
    ToolCardContent::new(header, rows.join("\n"))
}

/// The prompt, every option (chosen highlighted, others dimmed), then a typed
/// answer that matches no option, or `skipped` when nothing was given.
fn question_rows(question: &Value, chosen: &[&Value]) -> Vec<String> {
    let mut rows = Vec::new();
    if let Some(prompt) = text(question, "prompt").or_else(|| text(question, "question")) {
        rows.push(truncate_for_summary(prompt, ROW_CHARS));
    }
    let options = question
        .get("options")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let is_chosen = |option: &Value| {
        let value = text(option, "value");
        value.is_some() && chosen.iter().any(|answer| text(answer, "value") == value)
    };
    for option in options {
        let label = text(option, "label")
            .or_else(|| text(option, "value"))
            .unwrap_or("?");
        let recommended = option.get("recommended").and_then(Value::as_bool) == Some(true);
        rows.push(option_row(label, is_chosen(option), recommended));
    }
    let typed = chosen.iter().filter(|answer| {
        let value = text(answer, "value");
        !options.iter().any(|option| text(option, "value") == value)
    });
    let mut answered = options.iter().any(is_chosen);
    for answer in typed {
        let Some(given) = text(answer, "label").or_else(|| text(answer, "value")) else {
            continue;
        };
        answered = true;
        rows.push(option_row(&format!("\"{given}\""), true, false));
    }
    if !answered {
        rows.push(format!("{DIM}→ skipped{RESET}"));
    }
    rows
}

fn option_row(label: &str, chosen: bool, recommended: bool) -> String {
    let label = truncate_for_summary(label, ROW_CHARS);
    let marker = if recommended { " · recommended" } else { "" };
    if chosen {
        let brand = theme().fg_seq(TextRole::Brand);
        format!("{BOLD}{brand}→ {label}{RESET}{DIM}{marker}{RESET}")
    } else {
        format!("{DIM}  {label}{marker}{RESET}")
    }
}
