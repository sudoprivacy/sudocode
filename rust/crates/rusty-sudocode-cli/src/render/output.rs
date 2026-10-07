//! Line framing shared by the live UI and direct terminal writer.

use std::io::{self, IsTerminal, Write};

/// Decode an external byte stream without replacing a character split between
/// reads. Invalid bytes still become replacement characters; incomplete UTF-8
/// stays buffered until the next read.
#[derive(Default)]
pub(crate) struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        loop {
            let remaining = &self.pending[consumed..];
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    text.push_str(
                        std::str::from_utf8(&remaining[..valid]).expect("validated UTF-8 prefix"),
                    );
                    consumed += valid;
                    let Some(invalid) = error.error_len() else {
                        break;
                    };
                    text.push('\u{fffd}');
                    consumed += invalid;
                }
            }
        }
        self.pending.drain(..consumed);
        text
    }
}

/// A single call to iocraft's stdout handle. Borrows from the message being
/// split — the only allocation on this per-delta path is the one iocraft's
/// own `ToString` bound makes when the op is issued.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OutputOp<'a> {
    /// `StdoutHandle::println` — iocraft appends the line terminator itself,
    /// choosing `\r\n` in raw mode and `\n` otherwise.
    Println(&'a str),
    /// `StdoutHandle::print` — written verbatim, no terminator.
    Print(&'a str),
}

/// Split one output message into the sequence of iocraft stdout calls that
/// renders it with correct line endings.
///
/// **Why this exists.** The iocraft render loop holds the terminal in raw
/// mode, where `OPOST`/`ONLCR` are off and a bare `\n` moves the cursor down
/// *without* returning it to column 0. iocraft handles that for its own
/// canvas, and `StdoutHandle::println` terminates the message it is given —
/// but only the *end* of it: interior `\n` are passed through untouched, and
/// `StdoutHandle::print` writes its argument completely verbatim. Since
/// streaming markdown arrives as `Raw` chunks full of interior newlines,
/// every line after the first started where the previous one ended and the
/// response walked off the right edge of the screen as a staircase.
///
/// Splitting on `\n` and handing iocraft one line at a time makes iocraft
/// terminate each of them. That deliberately keeps raw-mode detection inside
/// iocraft — this crate links crossterm 0.28 while iocraft links 0.29, so the
/// two hold separate raw-mode statics and a local
/// `is_raw_mode_enabled()` query would never observe iocraft's state.
///
/// `terminated` distinguishes `Line` (the whole message is a line, so every
/// segment is `println`) from `Raw` (the tail is a partial line still being
/// streamed, so it is `print`).
///
/// Splitting is per-message and stateless: a literal `\r\n` straddling two
/// `Raw` chunks would come out as `…\r` + `\r\n`. That renders identically
/// (carriage returns are idempotent) and no current writer emits `\r\n` at
/// all — the markdown renderer and the tool formatters produce bare `\n`,
/// and `str::lines()` strips the `\r` from embedded tool output — so the
/// cost of carrying cross-message state isn't paid.
pub(crate) fn split_lines(text: &str, terminated: bool, mut issue: impl FnMut(OutputOp<'_>)) {
    let mut segments = text.split('\n').peekable();
    while let Some(segment) = segments.next() {
        let is_last = segments.peek().is_none();
        if is_last && !terminated {
            // Partial trailing line — no terminator yet.
            if !segment.is_empty() {
                issue(OutputOp::Print(segment));
            }
        } else {
            // `strip_suffix`, not `trim_end_matches`: this drops the `\r` of an
            // existing `\r\n` so iocraft's terminator does not produce `\r\r\n`,
            // while leaving any other carriage returns (e.g. the `\r\x1b[2K`
            // that rewrites the spinner line) intact.
            issue(OutputOp::Println(
                segment.strip_suffix('\r').unwrap_or(segment),
            ));
        }
    }
}

/// Frame terminal lines explicitly, including when the cancellation monitor
/// changes termios without going through crossterm. A carriage return is also
/// safe in cooked mode; redirected output retains its original line endings.
pub(crate) fn write_stdout(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    if stdout.is_terminal() {
        let mut result = Ok(());
        split_lines(text, false, |op| {
            if result.is_ok() {
                result = match op {
                    OutputOp::Print(chunk) => stdout.write_all(chunk.as_bytes()),
                    OutputOp::Println(line) => stdout
                        .write_all(line.as_bytes())
                        .and_then(|()| stdout.write_all(b"\r\n")),
                };
            }
        });
        result?;
    } else {
        stdout.write_all(text.as_bytes())?;
    }
    stdout.flush()
}
