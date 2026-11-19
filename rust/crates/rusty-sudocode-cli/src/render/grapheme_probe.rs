//! How the terminal draws grapheme clusters, decided once at startup.
//!
//! Terminals disagree on a cluster of several code points. iTerm2, kitty,
//! WezTerm, Ghostty and Windows Terminal draw `👨‍💻` as one 2-cell glyph;
//! xterm.js with Unicode 11 (VS Code) lays out each code point, 4 cells.
//! iocraft's live frame and scode's history measure text by the rule set
//! here (`iocraft::width`), so the caret, text after such a cluster and
//! wrapped rows land where the terminal draws them.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use iocraft::width::{set_grapheme_width, GraphemeWidth};

/// The cluster measured: 2 columns where it is one glyph, 4 per code point.
/// A two-part ZWJ sequence, the form clustering implementations agree on;
/// some count longer sequences, such as `👩🏽‍💻`, as neither rule.
const PROBE: &str = "\u{1F468}\u{200D}\u{1F4BB}";

/// Each cursor query's deadline. Terminals answer in milliseconds; this only
/// bounds the cost of one that never does.
const DEADLINE: Duration = Duration::from_millis(250);

/// The rule the terminal is known for, without asking it: VS Code lays out
/// each code point, the native terminals cluster. Holds for output that
/// never measures, such as print mode.
pub(crate) fn assume_from_environment() {
    let vscode = env::var_os("TERM_PROGRAM").is_some_and(|program| program == "vscode");
    set_grapheme_width(if vscode {
        GraphemeWidth::PerCodePoint
    } else {
        GraphemeWidth::Clustered
    });
}

/// Measures the terminal before the REPL's first frame. Called beside the
/// palette probe, through iocraft's input reader, so keys typed meanwhile
/// stay queued for the editor. Without an answer, the assumed rule stays.
///
/// Not on Windows: the cursor position there comes from the console host
/// behind ConPTY, not from the terminal that draws the text. The host clusters
/// while VS Code, behind the same ConPTY, lays out each code point, so the
/// environment's rule is the better answer.
pub(crate) fn measure() {
    if cfg!(windows)
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || env::var_os("TERM").is_some_and(|term| term == "dumb")
    {
        return;
    }
    if let Some(mode) = probe() {
        set_grapheme_width(mode);
    }
}

/// Prints the probe at the start of the row, asks where the cursor went and
/// erases the probe. Only from column 0, so the erase removes nothing but the
/// probe, and only after the terminal has answered once, so a terminal that
/// never answers never sees it.
fn probe() -> Option<GraphemeWidth> {
    let column = || {
        iocraft::query_cursor_position(DEADLINE)
            .ok()
            .flatten()
            .map(|(column, _)| column)
    };
    if column()? != 0 {
        return None;
    }
    let mut stdout = io::stdout();
    stdout.write_all(PROBE.as_bytes()).ok()?;
    stdout.flush().ok()?;
    let drawn = column();
    let _ = stdout.write_all(b"\r\x1b[K");
    let _ = stdout.flush();
    match drawn? {
        2 => Some(GraphemeWidth::Clustered),
        4 => Some(GraphemeWidth::PerCodePoint),
        _ => None,
    }
}
