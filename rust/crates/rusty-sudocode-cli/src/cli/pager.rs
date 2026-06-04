//! Pipe long terminal output through the user's `$PAGER` when it overflows
//! the viewport.
//!
//! Only paginates when stdout is a TTY *and* the rendered text would
//! otherwise spill past the visible viewport. Non-interactive callers
//! (piped output, CI, JSON paths) and short outputs fall through to a
//! direct `println!` so this helper never changes behavior in
//! scripted contexts.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::{Command, Stdio};

/// Pager command used when `$PAGER` is unset. `less -R` interprets ANSI
/// escape sequences so the colorized `/diff` output renders correctly.
const DEFAULT_PAGER: &str = "less -R";

/// Print `text` to stdout, spawning `$PAGER` when stdout is a TTY and the
/// output is taller than the terminal viewport.
///
/// `extra_trailing_rows` is added to the line count before the height
/// comparison; pass a nonzero value when the caller already plans to print
/// trailing prompts or follow-up status lines that should be considered
/// part of the visible block.
pub(crate) fn print_with_pager(text: &str) {
    print_with_pager_threshold(text, 0);
}

pub(crate) fn print_with_pager_threshold(text: &str, extra_trailing_rows: usize) {
    if !io::stdout().is_terminal() {
        println!("{text}");
        return;
    }

    let term_height = crossterm::terminal::size()
        .ok()
        .map_or(0, |(_, rows)| usize::from(rows));
    let line_count = text.lines().count().saturating_add(extra_trailing_rows);

    if term_height == 0 || line_count <= term_height {
        println!("{text}");
        return;
    }

    let pager_cmd = env::var("PAGER").unwrap_or_else(|_| DEFAULT_PAGER.to_string());
    let pager_cmd = pager_cmd.trim();
    if pager_cmd.is_empty() {
        println!("{text}");
        return;
    }

    let mut parts = pager_cmd.split_whitespace();
    let Some(program) = parts.next() else {
        println!("{text}");
        return;
    };
    let args: Vec<&str> = parts.collect();

    let Ok(mut child) = Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .spawn()
    else {
        // Pager binary missing or otherwise unavailable; fall through to
        // direct printing so the user still sees the content.
        println!("{text}");
        return;
    };

    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
        if !text.ends_with('\n') {
            let _ = stdin.write_all(b"\n");
        }
    }
    let _ = child.wait();
}
