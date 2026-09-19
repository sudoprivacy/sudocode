//! `!<command>` bash mode — the REPL's rendering of it.
//!
//! The behaviour (parse, run, transcript shape) lives in
//! [`commands::bash_mode`] and is shared with the ACP server; this module
//! only decides how the exchange looks in a terminal: the echoed input line
//! and the `⎿`-connected output block under it.

pub use commands::bash_mode::{failure_blocks, history_blocks, parse_bang_command, run};
use runtime::{ContentBlock, ConversationMessage, MessageRole};

/// How many output lines the REPL shows before folding the rest. The human
/// asked for this output, so it is shown in full up to a flood guard (a
/// model-initiated tool card folds at 15); the transcript always keeps all
/// of it.
const DISPLAY_MAX_LINES: usize = 500;

/// Turn transcript blocks into the user-role messages the REPL pushes.
#[must_use]
pub fn messages_from_blocks(blocks: Vec<ContentBlock>) -> Vec<ConversationMessage> {
    blocks
        .into_iter()
        .map(|block| ConversationMessage {
            role: MessageRole::User,
            blocks: vec![block],
            usage: None,
            model: None,
            duration_ms: None,
        })
        .collect()
}

/// The echoed input line: `!` in the error colour, the command on the code
/// background — the same shape CC uses so `!` lines stand out from prompts in
/// scrollback.
#[must_use]
pub fn render_input_line(command: &str) -> String {
    use crate::render::{ansi_fg, theme, RESET};
    let t = theme();
    format!(
        "{bg}{bang}!{RESET}{bg} {command}{RESET}",
        bg = t.code_bg_seq(),
        bang = ansi_fg(t.error),
    )
}

/// The output block under the echoed input: first line hangs off a `⎿`
/// connector, the rest are indented to align; stderr lines are tinted with the
/// error colour; an empty result says so instead of printing nothing.
#[must_use]
pub fn render_output_block(stdout: &str, stderr: &str, exit_status: Option<&str>) -> String {
    use crate::render::{ansi_fg, theme, DIM, RESET};
    let t = theme();
    let err = ansi_fg(t.error);
    let mut lines: Vec<String> = stdout
        .lines()
        .map(str::to_string)
        .chain(stderr.lines().map(|line| format!("{err}{line}{RESET}")))
        .collect();
    let total = lines.len();
    if total > DISPLAY_MAX_LINES {
        lines.truncate(DISPLAY_MAX_LINES);
        lines.push(format!(
            "{DIM}… +{} lines (full output is in the transcript){RESET}",
            total - DISPLAY_MAX_LINES
        ));
    }
    if let Some(status) = exit_status {
        lines.push(format!("{err}{status}{RESET}"));
    }
    if lines.is_empty() {
        lines.push(format!("{DIM}(no output){RESET}"));
    }
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let prefix = if i == 0 { "  \u{23bf}  " } else { "     " };
        out.push_str(prefix);
        out.push_str(line);
    }
    out
}
