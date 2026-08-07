//! Input chrome — the separator/footer UI around the REPL prompt.
//!
//! Renders a sandwich layout:
//!
//! ```text
//! ──────────────────────── (top separator)
//! ❯ <user input here>     (prompt — rendered by rustyline)
//! ──────────────────────── (bottom separator)
//!   ⏵⏵ full access mode · /help for commands · /permissions to change
//! ```
//!
//! `print_before_prompt` prints all four lines then cursors back to the
//! prompt line. This cursor-up is safe because the lines were just printed
//! (positions are deterministic, no scrolling has occurred).
//!
//! After the user submits, the caller clears the bottom sep + footer with
//! `\x1b[J` and prints the echo downward via `println!`. The old
//! `replace_after_submit` (which cursored UP after readline returned) was
//! the root cause of scrollback jumping and has been permanently removed.

use std::io::{self, Write};

fn term_width() -> usize {
    crossterm::terminal::size().map_or(80, |(cols, _)| cols as usize)
}

fn separator(width: usize) -> String {
    format!("\x1b[2m{}\x1b[0m", "─".repeat(width))
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
    format!("  \x1b[2m{icon} {label} mode · /help for commands · /permissions to change\x1b[0m")
}

/// Print the sandwich chrome (top sep, prompt placeholder, bottom sep,
/// footer) then cursor back to the prompt line so `read_line()` renders
/// there.
///
/// The `\x1b[2F` is safe: we just printed these lines so positions are
/// deterministic.
pub fn print_before_prompt(permission_mode: &str) {
    let w = term_width();
    let sep = separator(w);
    let footer = format_footer(permission_mode);
    let mut stdout = io::stdout();
    let _ = writeln!(stdout, "{sep}");
    let _ = writeln!(stdout); // prompt placeholder
    let _ = writeln!(stdout, "{sep}");
    let _ = write!(stdout, "{footer}");
    let _ = write!(stdout, "\x1b[2F\x1b[2K"); // cursor up 2, clear prompt placeholder
    let _ = stdout.flush();
}
