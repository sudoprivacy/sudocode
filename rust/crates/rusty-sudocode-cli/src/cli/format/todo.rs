//! The todo list as the user sees it, in one place: the live todo panel and
//! the `TodoWrite` card both draw it with [`panel`], so a list in scrollback
//! reads exactly like the panel did. Presentation only: the result sent to the
//! model, including any verification nudge addressed to it, is untouched.

use serde_json::Value;

use super::{tool_label, value_card, ToolCardContent, TOOL_OUTPUT_DISPLAY_MAX_LINES};
use crate::render::{ansi_fg, theme, StyledLine, TextRole, BOLD, DIM, RESET};

/// `TodoWrite`, then the list it saved, drawn like the todo panel. The result
/// also carries model-facing fields (the verification nudge is a
/// `<system-reminder>` for the model, not the user), so only the list is
/// shown. A result without a list is shown raw rather than dropped.
pub(super) fn card(name: &str, output: Option<&Value>) -> ToolCardContent {
    let header = tool_label(name);
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    let Some(todos) = tools::todos_from_tool_result(&output.to_string()) else {
        return value_card(header, output);
    };
    if todos.is_empty() {
        let mut cleared = StyledLine::muted();
        cleared.push("todo list cleared");
        return ToolCardContent::new(header, cleared.to_string());
    }
    // The header row and a possible "… +N" row stay within the card's cap.
    let body = panel(&todos, TOOL_OUTPUT_DISPLAY_MAX_LINES - 2);
    ToolCardContent::new(header, body)
}

/// Render the todo panel matching CC's `TodoWrite` layout:
///
/// ```text
/// 5 todos (2 done, 1 in progress, 2 open)
///   ✓ Update docs
///   ■ Writing unit tests
///   □ Fix login bug
///   … +2 pending, 1 completed
/// ```
///
/// - Header: count summary with done/in_progress/open breakdown
/// - Each visible todo: icon + label (completed = strikethrough+dim, in_progress = bold activeForm)
/// - Full projection bounds item count; shared layout owns terminal height.
/// - Priority order: in_progress > pending > completed; hidden summary
pub(crate) fn panel(todos: &[runtime::Todo], max_display: usize) -> String {
    if todos.is_empty() {
        return String::new();
    }

    let t = theme();
    let success = t.fg_seq(TextRole::Success);
    let info = ansi_fg(t.color(TextRole::Info));

    let completed_count = todos
        .iter()
        .filter(|t| t.status == runtime::TodoStatus::Completed)
        .count();
    let in_progress_count = todos
        .iter()
        .filter(|t| t.status == runtime::TodoStatus::InProgress)
        .count();
    let open_count = todos
        .iter()
        .filter(|t| t.status == runtime::TodoStatus::Pending)
        .count();

    // Header summary line
    let mut header = crate::render::StyledLine::muted();
    header.push_count(todos.len());
    header.push(" todos (");
    header.push_count(completed_count);
    header.push(" done");
    if in_progress_count > 0 {
        header.push(", ");
        header.push_count(in_progress_count);
        header.push(" in progress");
    }
    header.push(", ");
    header.push_count(open_count);
    header.push(" open)");

    let mut lines = Vec::with_capacity(todos.len() + 2);
    lines.push(header.to_string());

    // Summary and full view derive their counts/styles from the same source.
    if max_display == 0 {
        return lines.join("\n");
    }

    // Sort by priority: in_progress first, then pending, then completed.
    let mut sorted: Vec<&runtime::Todo> = todos.iter().collect();
    sorted.sort_by_key(|t| match t.status {
        runtime::TodoStatus::InProgress => 0,
        runtime::TodoStatus::Pending => 1,
        runtime::TodoStatus::Completed => 2,
    });

    let display_count = sorted.len().min(max_display);
    let visible = &sorted[..display_count];
    let hidden = &sorted[display_count..];

    for todo in visible {
        // While in progress, show the present-continuous `activeForm`; otherwise
        // the imperative `content`.
        let label = if todo.status == runtime::TodoStatus::InProgress {
            todo.active_form.as_str()
        } else {
            todo.content.as_str()
        };
        let (icon, label_fmt) = match todo.status {
            runtime::TodoStatus::Completed => (
                format!("{success}\u{2713}{RESET}"),
                format!("{DIM}\x1b[9m{label}\x1b[29m{RESET}"),
            ),
            runtime::TodoStatus::InProgress => (
                format!("{info}\u{25a0}{RESET}"),
                format!("{BOLD}{label}{RESET}"),
            ),
            runtime::TodoStatus::Pending => ("\u{25a1}".to_string(), label.to_string()),
        };
        lines.push(format!("  {icon} {label_fmt}"));
    }

    if !hidden.is_empty() {
        let mut parts = Vec::new();
        let hi = hidden
            .iter()
            .filter(|t| t.status == runtime::TodoStatus::InProgress)
            .count();
        let hp = hidden
            .iter()
            .filter(|t| t.status == runtime::TodoStatus::Pending)
            .count();
        let hc = hidden
            .iter()
            .filter(|t| t.status == runtime::TodoStatus::Completed)
            .count();
        if hi > 0 {
            parts.push(format!("{hi} in progress"));
        }
        if hp > 0 {
            parts.push(format!("{hp} pending"));
        }
        if hc > 0 {
            parts.push(format!("{hc} completed"));
        }
        lines.push(format!("{DIM}  \u{2026} +{}{RESET}", parts.join(", ")));
    }

    lines.join("\n")
}
