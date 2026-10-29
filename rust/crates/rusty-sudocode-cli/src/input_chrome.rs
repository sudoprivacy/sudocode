//! Input chrome — the separator/footer UI around the REPL prompt.
//!
//! Print mode/help above the synchronous editor's input region:
//!
//! ```text
//! ──────────────────────── (top separator)
//!   ⏵⏵ full access mode · /help for commands · /permissions to change
//! ❯ <user input here>     (prompt — rendered by rustyline)
//! ```
//!
//! Rustyline owns every row from its prompt downward. Its external printer
//! moves that region after a peer notice or captured stderr; prepainting a
//! separator/footer below it lets the relocated prompt overwrite that chrome.
//! Keep informational output above the editor, with no reserved rows or cursor
//! movement across the ownership boundary.

use std::io::{self, Write};

use crate::render::{DIM, RESET};

fn term_width() -> usize {
    crossterm::terminal::size().map_or(80, |(cols, _)| cols as usize)
}

fn separator(width: usize) -> String {
    format!("{DIM}{}{RESET}", "─".repeat(width))
}

fn format_footer(permission_mode: &str) -> String {
    let icon = match permission_mode {
        "danger-full-access" => "⏵⏵",
        "workspace-write" => "⏵",
        _ => "▷",
    };
    let label = match permission_mode {
        "danger-full-access" => "full access",
        "workspace-write" => "workspace write",
        "read-only" => "read only",
        other => other,
    };
    format!("  {DIM}{icon} {label} mode · /help for commands · /permissions to change{RESET}")
}

/// Print the mode/help header before handing the terminal to `read_line()`.
pub fn print_before_prompt(permission_mode: &str) {
    let w = term_width();
    let sep = separator(w);
    let footer = format_footer(permission_mode);
    let header = format!("{sep}\n{footer}\n");
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(header.as_bytes());
    let _ = stdout.flush();
}
