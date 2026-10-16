use crate::render::{BOLD, DIM, RESET};

pub(super) fn queued_label(display: &str) -> String {
    let first = display.lines().next().unwrap_or("");
    format!("{DIM}↳ queued: {first}{RESET}")
}

pub(super) fn todo_item(todo: &runtime::Todo, success: &str, info: &str) -> String {
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
    format!("  {icon} {label_fmt}")
}
