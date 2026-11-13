//! The todo list as the user sees it, in one place: the live todo panel and
//! the `TodoWrite` card both draw it here, with the same layout and colors.
//! The panel is a glance, so it cuts a long item to a few rows; the card in
//! scrollback is the record and keeps every item whole. Presentation only: the
//! result sent to the model, including any verification nudge addressed to it,
//! is untouched.

use serde_json::Value;

use super::{tool_label, value_card, ToolCardContent, TOOL_OUTPUT_DISPLAY_MAX_LINES};
use crate::render::left_frame;
use crate::render::styled_text::StyledText;
use crate::render::text_layout::{display_width, truncate_to_width, wrap_words};
use crate::render::{theme, StyledLine, TextRole, BOLD, DIM, RESET};

/// Columns before an item's label (`  ■ `). A label that wraps continues under
/// itself, as a Markdown list item does, not under the icon.
const LABEL_COLUMN: usize = 4;

/// `TodoWrite`, then the list it saved, drawn like the todo panel. The result
/// also carries model-facing fields (the verification nudge is a
/// `<system-reminder>` for the model, not the user), so only the list is
/// shown. A result without a list is shown raw rather than dropped. `width`
/// is the terminal width the card is painted at; 0 leaves wrapping to the
/// frame.
pub(super) fn card(name: &str, output: Option<&Value>, width: usize) -> ToolCardContent {
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
    let body_width = if width == 0 {
        0
    } else {
        left_frame::body_width(width)
    };
    // The header row and a possible "… +N" row stay within the card's cap.
    let body = render(&todos, TOOL_OUTPUT_DISPLAY_MAX_LINES - 2, body_width, false);
    ToolCardContent::new(header, body)
}

/// The live todo panel, `width` columns wide (0: no wrapping):
///
/// ```text
/// 5 todos (2 done, 1 in progress, 2 open)
///   ■ Writing unit tests for the parser's error
///     recovery
///   □ Fix login bug
///   ✓ Update docs
///   … +2 pending, 1 completed
/// ```
///
/// - Header: count summary with done/in_progress/open breakdown.
/// - Each visible todo: icon, then the label, which wraps under itself. The
///   item in progress may take three rows, an open item two and a finished one
///   one; a longer item is cut with `…`.
/// - Colors follow the tool cards: in progress takes a running card's brand
///   color with a bold label, finished takes a finished card's color with a
///   dim, crossed-out label, and open takes a queued card's secondary color.
/// - Priority order: in_progress > pending > completed; hidden summary.
pub(crate) fn panel(todos: &[runtime::Todo], max_display: usize, width: usize) -> String {
    render(todos, max_display, width, true)
}

/// Draw up to `max_display` items. `glance` cuts long items to the rows their
/// status allows; without it every item is shown whole.
fn render(todos: &[runtime::Todo], max_display: usize, width: usize, glance: bool) -> String {
    if todos.is_empty() {
        return String::new();
    }

    let t = theme();
    let brand = t.fg_seq(TextRole::Brand);
    let info = t.fg_seq(TextRole::Info);
    let secondary = t.fg_seq(TextRole::Secondary);

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
        let (icon, label_fmt, rows) = match todo.status {
            runtime::TodoStatus::Completed => (
                format!("{info}\u{2713}{RESET}"),
                format!("{DIM}\x1b[9m{label}\x1b[29m{RESET}"),
                1,
            ),
            runtime::TodoStatus::InProgress => (
                format!("{brand}\u{25a0}{RESET}"),
                format!("{BOLD}{label}{RESET}"),
                3,
            ),
            runtime::TodoStatus::Pending => {
                (format!("{secondary}\u{25a1}{RESET}"), label.to_string(), 2)
            }
        };
        let prefix = format!("  {icon} ");
        lines.extend(layout_item(
            &prefix,
            &label_fmt,
            width,
            glance.then_some(rows),
        ));
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

/// Lay one item out in `width` columns: the label wraps at word boundaries and
/// continues under itself, then is cut to `max_rows`, the last kept row ending
/// with `…`. `prefix` is the indent and icon, [`LABEL_COLUMN`] wide.
fn layout_item(prefix: &str, label: &str, width: usize, max_rows: Option<usize>) -> Vec<String> {
    if width == 0 || LABEL_COLUMN + display_width(label) <= width {
        return vec![format!("{prefix}{label}")];
    }
    let label_width = width.saturating_sub(LABEL_COLUMN).max(1);
    let mut rows: Vec<String> = wrap_words(&StyledText::from_ansi(label), label_width)
        .iter()
        .map(|row| {
            let mut ansi = String::new();
            row.write_ansi(0..row.text.len(), &mut ansi);
            ansi
        })
        .collect();
    if let Some(max_rows) = max_rows.filter(|&max| max > 0 && rows.len() > max) {
        // The rest always overflows one row, so the cut always shows `…`.
        let rest = rows.split_off(max_rows - 1).join(" ");
        rows.push(truncate_to_width(&rest, label_width));
    }
    let continuation = " ".repeat(LABEL_COLUMN);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let lead = if index == 0 { prefix } else { &continuation };
            format!("{lead}{row}")
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(text: &str) -> String {
        StyledText::from_ansi(text).text
    }

    fn todo(content: &str, status: runtime::TodoStatus) -> runtime::Todo {
        runtime::Todo {
            content: content.into(),
            status,
            active_form: content.into(),
        }
    }

    const LONG: &str = "Wire the managed child entry through the bootstrap line on private stdin, then the typed ready or error reply, then ACP on the same reader";

    #[test]
    fn a_wrapped_label_continues_under_itself_not_under_the_icon() {
        let rows = strip_ansi(&panel(&[todo(LONG, runtime::TodoStatus::Pending)], 10, 40));
        let rows: Vec<&str> = rows.lines().skip(1).collect();
        assert_eq!(
            rows.len(),
            2,
            "an open item takes at most two rows: {rows:?}"
        );
        assert!(rows[0].starts_with("  \u{25a1} Wire"), "{rows:?}");
        assert!(
            rows[1].starts_with("    ") && !rows[1].starts_with("     "),
            "{rows:?}"
        );
        for row in &rows {
            assert!(display_width(row) <= 40, "row fits the panel: {row:?}");
        }
    }

    #[test]
    fn the_panel_cuts_each_item_to_the_rows_its_status_allows() {
        let todos = [
            todo(LONG, runtime::TodoStatus::InProgress),
            todo(LONG, runtime::TodoStatus::Pending),
            todo(LONG, runtime::TodoStatus::Completed),
        ];
        let text = strip_ansi(&panel(&todos, 10, 40));
        let mut items: Vec<Vec<&str>> = Vec::new();
        for row in text.lines().skip(1) {
            if row.starts_with("    ") {
                items.last_mut().unwrap().push(row);
            } else {
                items.push(vec![row]);
            }
        }
        let rows: Vec<usize> = items.iter().map(Vec::len).collect();
        assert_eq!(rows, [3, 2, 1], "in progress, open, done: {text}");
        for item in &items {
            assert!(
                item.last().unwrap().ends_with('\u{2026}'),
                "cut items end with …: {item:?}"
            );
        }
    }

    #[test]
    fn the_card_keeps_every_item_whole_under_its_label() {
        let card = card(
            "TodoWrite",
            Some(&serde_json::json!({
                "todos": [{"content": LONG, "status": "pending", "activeForm": LONG}]
            })),
            40,
        );
        let body = strip_ansi(card.body.as_deref().unwrap());
        let rows: Vec<&str> = body.lines().skip(1).collect();
        assert!(rows.len() > 2, "the record is not cut: {rows:?}");
        assert!(!body.contains('\u{2026}'), "{body}");
        assert!(
            rows[1..].iter().all(|row| row.starts_with("    ")),
            "{rows:?}"
        );
        let words: Vec<&str> = rows
            .iter()
            .flat_map(|row| row.split_whitespace())
            .skip(1)
            .collect();
        assert_eq!(words.join(" "), LONG);
        for row in &rows {
            assert!(display_width(row) <= left_frame::body_width(40), "{row:?}");
        }
    }

    #[test]
    fn a_short_item_and_an_unknown_width_are_left_as_they_were() {
        let todos = [todo("Fix login bug", runtime::TodoStatus::Pending)];
        assert_eq!(panel(&todos, 10, 80), panel(&todos, 10, 0));
        let unwrapped = panel(&[todo(LONG, runtime::TodoStatus::Pending)], 10, 0);
        assert_eq!(strip_ansi(&unwrapped).lines().count(), 2, "{unwrapped}");
    }

    #[test]
    fn icons_take_the_tool_card_status_colors() {
        let todos = [
            todo("Run", runtime::TodoStatus::InProgress),
            todo("Wait", runtime::TodoStatus::Pending),
            todo("Done", runtime::TodoStatus::Completed),
        ];
        let text = panel(&todos, 10, 80);
        let t = theme();
        for (role, icon) in [
            (TextRole::Brand, '\u{25a0}'),
            (TextRole::Secondary, '\u{25a1}'),
            (TextRole::Info, '\u{2713}'),
        ] {
            let expected = format!("{}{icon}", t.fg_seq(role));
            assert!(text.contains(&expected), "{icon} uses {role:?}: {text:?}");
        }
    }
}
