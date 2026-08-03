//! Input chrome — the separator/footer UI around the REPL prompt.
//!
//! Both the sync REPL (`run_repl_loop`) and the async REPL input thread
//! render the same visual structure:
//!
//! ```text
//! ──────────────────────── (top separator)
//! ❯ <user input here>     (prompt — rendered by rustyline)
//! ──────────────────────── (bottom separator)
//!   /help · /status · Tab for /commands  (footer)
//! ```
//!
//! After the user submits, the chrome is replaced with a styled echo of
//! the input. This module owns the rendering so neither REPL duplicates it.

use std::io::{self, Write};

use crate::cli::format::format_input_echo;

fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80)
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

/// Print the input chrome block (top sep, prompt placeholder, bottom sep,
/// footer) then move the cursor back to the prompt line so `read_line()`
/// renders there.
pub fn print_before_prompt(permission_mode: &str) -> io::Result<()> {
    let w = term_width();
    let sep = separator(w);
    let footer = format_footer(permission_mode);
    let mut stdout = io::stdout();
    writeln!(stdout, "{sep}")?;
    writeln!(stdout)?; // prompt placeholder
    writeln!(stdout, "{sep}")?;
    write!(stdout, "{footer}")?;
    write!(stdout, "\x1b[2F\x1b[2K")?; // cursor up 2, clear prompt placeholder
    stdout.flush()
}

/// After the user submits text: clear the pre-printed bottom sep + footer,
/// replace the prompt line with a styled echo, then print a trailing separator.
/// Used by both sync and async REPLs (both now use `print_before_prompt`).
pub fn replace_after_submit(input: &str) -> io::Result<usize> {
    let w = term_width();
    let sep = separator(w);
    let mut stdout = io::stdout();

    // Clear pre-printed bottom sep + footer.
    write!(stdout, "\x1b[J")?;

    // Replace prompt line with a gray-background echo of the user input.
    let trimmed = input.trim();
    let (echo_block, line_count) = format_input_echo(trimmed, w);
    for _ in 0..line_count {
        write!(stdout, "\x1b[1F\x1b[2K")?;
    }
    write!(stdout, "{echo_block}")?;
    writeln!(stdout)?;
    writeln!(stdout, "{sep}")?;
    stdout.flush()?;
    Ok(w)
}
