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
pub(crate) fn print_with_pager(text: &str) {
    print_with_pager_threshold(text, 0);
}

/// As [`print_with_pager`], but accounts for `extra_trailing_rows` lines
/// the caller plans to print after `text` when deciding whether to paginate.
pub(crate) fn print_with_pager_threshold(text: &str, extra_trailing_rows: usize) {
    if !io::stdout().is_terminal() {
        println!("{text}");
        return;
    }

    let term_height = crossterm::terminal::size()
        .ok()
        .map_or(0, |(_, rows)| usize::from(rows));

    if fits_in_viewport(text, term_height, extra_trailing_rows) {
        println!("{text}");
        return;
    }

    let Some((program, args)) = select_pager(|key| env::var(key).ok()) else {
        println!("{text}");
        return;
    };

    let Ok(mut child) = Command::new(&program)
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

/// Returns `true` when `text` fits inside the visible viewport and no
/// pagination is needed.
///
/// A `term_height` of 0 means the terminal size could not be determined;
/// the caller should not paginate in that case (the rendered output may
/// be safer than a broken pager invocation).
fn fits_in_viewport(text: &str, term_height: usize, extra_trailing_rows: usize) -> bool {
    if term_height == 0 {
        return true;
    }
    let line_count = text.lines().count().saturating_add(extra_trailing_rows);
    line_count <= term_height
}

/// Resolve the pager command (program + args) from the environment.
///
/// `$PAGER` is honored when set to a non-whitespace value; otherwise
/// `DEFAULT_PAGER` is used. Returns `None` when `$PAGER` is set but
/// contains only whitespace (an explicit "no pager" signal).
///
/// The lookup is parameterized so this is unit-testable without mutating
/// the process environment.
fn select_pager(get_env: impl Fn(&str) -> Option<String>) -> Option<(String, Vec<String>)> {
    let raw = get_env("PAGER").unwrap_or_else(|| DEFAULT_PAGER.to_string());
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let program = parts.next()?.to_string();
    let args: Vec<String> = parts.map(str::to_string).collect();
    Some((program, args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn fits_in_viewport_short_output_fits() {
        assert!(fits_in_viewport("one\ntwo\nthree", 24, 0));
    }

    #[test]
    fn fits_in_viewport_exact_height_fits() {
        let text = (0..10)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // 10 lines, viewport 10 rows — exactly fits, no pager.
        assert!(fits_in_viewport(&text, 10, 0));
    }

    #[test]
    fn fits_in_viewport_one_over_height_overflows() {
        let text = (0..11)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // 11 lines vs viewport 10 — overflows.
        assert!(!fits_in_viewport(&text, 10, 0));
    }

    #[test]
    fn fits_in_viewport_extra_trailing_rows_counted() {
        let text = (0..8).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        // 8 lines + 3 trailing = 11 > 10 — overflows.
        assert!(!fits_in_viewport(&text, 10, 3));
    }

    #[test]
    fn fits_in_viewport_unknown_terminal_size_skips_pager() {
        // term_height == 0 means crossterm couldn't read the size; never
        // paginate in that case, just print.
        let text = (0..10_000)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(fits_in_viewport(&text, 0, 0));
    }

    #[test]
    fn select_pager_defaults_to_less_dash_r() {
        let (program, args) = select_pager(env(&[])).expect("default pager");
        assert_eq!(program, "less");
        assert_eq!(args, vec!["-R".to_string()]);
    }

    #[test]
    fn select_pager_honors_pager_env() {
        let (program, args) =
            select_pager(env(&[("PAGER", "bat --paging=always")])).expect("custom pager");
        assert_eq!(program, "bat");
        assert_eq!(args, vec!["--paging=always".to_string()]);
    }

    #[test]
    fn select_pager_returns_none_on_empty_pager() {
        // Empty PAGER is an explicit "no pager" signal; respect it.
        assert!(select_pager(env(&[("PAGER", "")])).is_none());
        assert!(select_pager(env(&[("PAGER", "   ")])).is_none());
    }

    #[test]
    fn select_pager_single_command_has_no_args() {
        let (program, args) = select_pager(env(&[("PAGER", "more")])).expect("single command");
        assert_eq!(program, "more");
        assert!(args.is_empty());
    }

    #[test]
    fn select_pager_collapses_whitespace_runs() {
        // `split_whitespace` already collapses runs of spaces/tabs, but the
        // expectation is documented as part of the public contract.
        let (program, args) =
            select_pager(env(&[("PAGER", "  less   -R   -F  ")])).expect("trimmed pager");
        assert_eq!(program, "less");
        assert_eq!(args, vec!["-R".to_string(), "-F".to_string()]);
    }
}
