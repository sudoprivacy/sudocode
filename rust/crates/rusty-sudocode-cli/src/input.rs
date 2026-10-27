use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rustyline::completion::{Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Context, EditMode, Editor, EventContext,
    EventHandler, Helper, KeyCode, KeyEvent, Modifiers, Movement, RepeatCount,
};

use crate::render::{DIM, RESET};

/// Rustyline handler for `↑`: moves cursor to the line above (or to the
/// beginning of the current line on single-line input), and only navigates
/// history when the cursor is already at the top-left. This matches Claude
/// Code's arrow-key UX.
struct UpArrowHandler;

impl ConditionalEventHandler for UpArrowHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        // Empty buffer: history.
        if ctx.line().is_empty() {
            return Some(Cmd::PreviousHistory);
        }
        // Non-empty, cursor not at beginning: move to beginning of line.
        if ctx.pos() > 0 {
            return Some(Cmd::Move(Movement::BeginningOfLine));
        }
        // Cursor already at beginning: navigate history.
        Some(Cmd::PreviousHistory)
    }
}

/// Rustyline handler for `↓`: moves cursor to the line below (or to the
/// end of the current line on single-line input), and only navigates
/// history when the cursor is already at the bottom-right.
struct DownArrowHandler;

impl ConditionalEventHandler for DownArrowHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let line = ctx.line();
        // Empty buffer: history navigation.
        if line.is_empty() {
            return Some(Cmd::NextHistory);
        }
        // Cursor not at end: move to end of line.
        if ctx.pos() < line.len() {
            return Some(Cmd::Move(Movement::EndOfLine));
        }
        // Cursor already at end: navigate history.
        Some(Cmd::NextHistory)
    }
}

/// Accept the line only when it contains non-whitespace text.
/// When the line is empty, Enter is a no-op, unless turns are waiting to run
/// at the prompt: then an empty Enter hands the prompt back so they can.
struct AcceptNonEmpty {
    waiting_turns: Arc<AtomicUsize>,
}

impl ConditionalEventHandler for AcceptNonEmpty {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if ctx.line().trim().is_empty() && self.waiting_turns.load(Ordering::Acquire) == 0 {
            Some(Cmd::Noop)
        } else {
            Some(Cmd::AcceptLine)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Submit(String),
    Exit,
}

/// How an image paste was asked for.
#[derive(Clone, Copy)]
enum ImagePasteTrigger {
    /// A bracketed paste began. Cmd+V on macOS sends an empty one when the
    /// clipboard holds only an image; clipboard text keeps the normal paste.
    Paste,
    /// Ctrl+V: attaches an image, and otherwise keeps its default binding.
    CtrlV,
    /// Alt+V: attaches an image, and otherwise says there is none.
    AltV,
}

/// Inserts the clipboard image as an `@"path"` reference at the cursor, the
/// same reference a user can type. The prompt resolves it into the image when
/// it is submitted, exactly as in every other input path.
struct ImagePasteHandler {
    trigger: ImagePasteTrigger,
}

impl ConditionalEventHandler for ImagePasteHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let line = ctx.line();
        let image = match self.trigger {
            ImagePasteTrigger::Paste => crate::clipboard_image::read_image_for_paste(),
            ImagePasteTrigger::CtrlV | ImagePasteTrigger::AltV => {
                if crate::clipboard_image::is_held_repeat(line) {
                    return Some(Cmd::Noop);
                }
                crate::clipboard_image::read_image()
            }
        };
        let notice = match image {
            Ok(Some(path)) => {
                let (before, after) = line.split_at(ctx.pos());
                let reference = format!(
                    "{}{} ",
                    crate::clipboard_image::separator_before(before),
                    crate::clipboard_image::reference(&path)
                );
                crate::clipboard_image::note_paste_key(&format!("{before}{reference}{after}"));
                return Some(Cmd::Insert(1, reference));
            }
            Ok(None) => crate::clipboard_image::NO_IMAGE_NOTICE.to_string(),
            Err(error) => format!("Could not read the clipboard: {error}"),
        };
        if !matches!(self.trigger, ImagePasteTrigger::AltV) {
            return None;
        }
        crate::clipboard_image::note_paste_key(line);
        // Like other editor notices, shown above the prompt; the next redraw
        // restores the line being edited.
        let mut stdout = std::io::stdout();
        write!(stdout, "\r\x1b[2K  {DIM}{notice}{RESET}\n").ok();
        stdout.flush().ok();
        Some(Cmd::Repaint)
    }
}

/// Slash-command prefixes whose argument is a filesystem path. When the
/// cursor sits after one of these, fall through to `FilenameCompleter` so
/// `<Tab>` lists files in the working directory instead of matching the
/// literal slash-command name list.
const FILE_ARG_PREFIXES: &[&str] = &["/export ", "/plugin install "];

/// Returns `true` when the cursor in `line` sits inside the argument of a
/// path-taking slash command and tab completion should produce filesystem
/// paths instead of slash-command names.
fn is_file_arg_position(line: &str, pos: usize) -> bool {
    if pos > line.len() {
        return false;
    }
    let prefix_before_cursor = &line[..pos];
    FILE_ARG_PREFIXES
        .iter()
        .any(|cmd_prefix| prefix_before_cursor.starts_with(cmd_prefix))
}

struct SlashCommandHelper {
    /// Each entry is (command, description). Description may be empty.
    completions: Vec<(String, String)>,
    filename_completer: FilenameCompleter,
    current_line: RefCell<String>,
}

impl SlashCommandHelper {
    fn new(completions: Vec<(String, String)>) -> Self {
        Self {
            completions: normalize_completions(completions),
            filename_completer: FilenameCompleter::new(),
            current_line: RefCell::new(String::new()),
        }
    }

    fn reset_current_line(&self) {
        self.current_line.borrow_mut().clear();
    }

    fn current_line(&self) -> String {
        self.current_line.borrow().clone()
    }

    fn set_current_line(&self, line: &str) {
        let mut current = self.current_line.borrow_mut();
        current.clear();
        current.push_str(line);
    }

    fn set_completions(&mut self, completions: Vec<(String, String)>) {
        self.completions = normalize_completions(completions);
    }
}

impl Completer for SlashCommandHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        // When the cursor sits inside the argument of a path-taking slash
        // command (`/export <path>`, `/plugin install <path>`), delegate to
        // `FilenameCompleter` so users get real directory listings on <Tab>
        // instead of the literal slash-command suggestions.
        if is_file_arg_position(line, pos) {
            return self.filename_completer.complete_path(line, pos);
        }

        let Some(prefix) = slash_command_prefix(line, pos) else {
            return Ok((0, Vec::new()));
        };

        let matches = self
            .completions
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(prefix))
            .map(|(cmd, desc)| Pair {
                display: if desc.is_empty() {
                    cmd.clone()
                } else {
                    format!("{cmd:<24} — {desc}")
                },
                replacement: cmd.clone(),
            })
            .collect();

        Ok((0, matches))
    }
}

impl Hinter for SlashCommandHelper {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}

impl Highlighter for SlashCommandHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        self.set_current_line(line);
        Cow::Borrowed(line)
    }

    fn highlight_char(&self, line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.set_current_line(line);
        false
    }
}

impl Validator for SlashCommandHelper {}
impl Helper for SlashCommandHelper {}

use crate::cancel;

pub struct LineEditor {
    prompt: String,
    editor: Editor<SlashCommandHelper, DefaultHistory>,
    /// Timestamp of the first Ctrl-C on an empty prompt. `None` when no
    /// pending exit. A second Ctrl-C within [`cancel::DOUBLE_CTRLC_WINDOW`] triggers
    /// exit; after that the pending state resets automatically.
    pending_exit_at: Option<std::time::Instant>,
    /// How many turns are waiting to run at the next prompt.
    waiting_turns: Arc<AtomicUsize>,
}

impl LineEditor {
    /// The count of turns waiting to run at the next prompt. While it is above
    /// zero, Enter on an empty line returns an empty submission so they can run.
    pub fn waiting_turns(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.waiting_turns)
    }
    #[must_use]
    pub fn new(prompt: impl Into<String>, completions: Vec<(String, String)>) -> Self {
        let config = Config::builder()
            .completion_type(CompletionType::List)
            .edit_mode(EditMode::Emacs)
            .build();
        let mut editor = Editor::<SlashCommandHelper, DefaultHistory>::with_config(config)
            .expect("rustyline editor should initialize");
        editor.set_helper(Some(SlashCommandHelper::new(completions)));
        editor.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::SHIFT), Cmd::Newline);
        let waiting_turns = Arc::new(AtomicUsize::new(0));
        editor.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::NONE),
            EventHandler::Conditional(Box::new(AcceptNonEmpty {
                waiting_turns: Arc::clone(&waiting_turns),
            })),
        );

        // ↑: beginning-of-line first, then history.
        // ↓: end-of-line first, then history.
        // Matches Claude Code's arrow-key UX.
        editor.bind_sequence(
            KeyEvent(KeyCode::Up, Modifiers::NONE),
            EventHandler::Conditional(Box::new(UpArrowHandler)),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Down, Modifiers::NONE),
            EventHandler::Conditional(Box::new(DownArrowHandler)),
        );

        // Image paste: an empty bracketed paste (Cmd+V on macOS with an image
        // on the clipboard), Ctrl+V where the terminal passes it through, and
        // Alt+V everywhere. A paste with clipboard text runs the default path.
        for (key, trigger) in [
            (
                KeyEvent(KeyCode::BracketedPasteStart, Modifiers::NONE),
                ImagePasteTrigger::Paste,
            ),
            (KeyEvent::ctrl('V'), ImagePasteTrigger::CtrlV),
            (KeyEvent::alt('v'), ImagePasteTrigger::AltV),
        ] {
            editor.bind_sequence(
                key,
                EventHandler::Conditional(Box::new(ImagePasteHandler { trigger })),
            );
        }
        // Consume the trailing BracketedPasteEnd that remains in the buffer
        // after we handle an image paste (the default handler would have
        // consumed it inside read_pasted_text, but we bypassed that path).
        editor.bind_sequence(
            KeyEvent(KeyCode::BracketedPasteEnd, Modifiers::NONE),
            Cmd::Noop,
        );

        Self {
            prompt: prompt.into(),
            editor,
            pending_exit_at: None,
            waiting_turns,
        }
    }

    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() {
            return;
        }

        let _ = self.editor.add_history_entry(entry);
    }

    pub fn set_completions(&mut self, completions: Vec<(String, String)>) {
        if let Some(helper) = self.editor.helper_mut() {
            helper.set_completions(completions);
        }
    }

    /// Read a single choice line for a mid-turn dialog (e.g. the write_plan
    /// approval), reusing THIS editor so there is exactly one owner of the
    /// terminal's raw-mode / bracketed-paste state. A throwaway second
    /// `rustyline::Editor` (the previous approach) toggled those modes on drop
    /// underneath the main REPL editor, perturbing its re-arm. The slash-command
    /// completions are blanked for the read so option digits don't trigger the
    /// completer; the caller re-seeds them on the next loop iteration. The
    /// answer is NOT pushed into prompt history.
    pub fn prompt_choice(&mut self, prompt: &str) -> io::Result<String> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            let mut buffer = String::new();
            if io::stdin().read_line(&mut buffer)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stdin closed while awaiting choice",
                ));
            }
            return Ok(buffer.trim().to_string());
        }
        if let Some(helper) = self.editor.helper_mut() {
            helper.set_completions(Vec::new());
        }
        match self.editor.readline(prompt) {
            Ok(line) => Ok(line.trim().to_string()),
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "choice prompt cancelled",
            )),
            Err(error) => Err(io::Error::other(error)),
        }
    }

    pub fn read_line(&mut self) -> io::Result<ReadOutcome> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return self.read_line_fallback();
        }

        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }

        loop {
            match self.editor.readline(&self.prompt) {
                Ok(line) => {
                    self.pending_exit_at = None;
                    self.push_history(&line);
                    return Ok(ReadOutcome::Submit(line));
                }
                Err(ReadlineError::Interrupted) => {
                    let has_input = !self.current_line().is_empty();
                    self.finish_interrupted_read();

                    let mut stdout = io::stdout();
                    // Undo rustyline's newline: move cursor back to prompt line, clear it.
                    write!(stdout, "\x1b[1F\x1b[2K")?;

                    if has_input {
                        // Had text — clear it and restart the prompt.
                        self.pending_exit_at = None;
                    } else if self
                        .pending_exit_at
                        .is_some_and(|t| t.elapsed() <= cancel::DOUBLE_CTRLC_WINDOW)
                    {
                        // Sync REPL: second Ctrl-C within 800ms — exit.
                        writeln!(stdout, "\x1b[J")?;
                        stdout.flush()?;
                        return Ok(ReadOutcome::Exit);
                    } else {
                        self.pending_exit_at = Some(std::time::Instant::now());
                        // Show exit hint in the footer area (2 lines below prompt).
                        write!(
                            stdout,
                            "\x1b[2E\x1b[2K  {DIM}Press Ctrl-C again to exit{RESET}\x1b[2F",
                            DIM = DIM,
                            RESET = RESET
                        )?;
                    }

                    stdout.flush()?;
                    // Loop re-enters readline on the correct prompt line.
                }
                Err(ReadlineError::Eof) => {
                    self.finish_interrupted_read();
                    let mut stdout = io::stdout();
                    writeln!(stdout, "\x1b[J")?;
                    stdout.flush()?;
                    return Ok(ReadOutcome::Exit);
                }
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }

    fn current_line(&self) -> String {
        self.editor
            .helper()
            .map_or_else(String::new, SlashCommandHelper::current_line)
    }

    fn finish_interrupted_read(&mut self) {
        if let Some(helper) = self.editor.helper_mut() {
            helper.reset_current_line();
        }
    }

    fn read_line_fallback(&self) -> io::Result<ReadOutcome> {
        let mut stdout = io::stdout();
        write!(stdout, "{}", self.prompt)?;
        stdout.flush()?;

        let mut buffer = String::new();
        let bytes_read = io::stdin().read_line(&mut buffer)?;
        if bytes_read == 0 {
            return Ok(ReadOutcome::Exit);
        }

        while matches!(buffer.chars().last(), Some('\n' | '\r')) {
            buffer.pop();
        }
        Ok(ReadOutcome::Submit(buffer))
    }
}

fn slash_command_prefix(line: &str, pos: usize) -> Option<&str> {
    if pos != line.len() {
        return None;
    }

    let prefix = &line[..pos];
    if !prefix.starts_with('/') {
        return None;
    }

    Some(prefix)
}

fn normalize_completions(completions: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = BTreeSet::new();
    completions
        .into_iter()
        .filter(|(cmd, _)| cmd.starts_with('/'))
        .filter(|(cmd, _)| seen.insert(cmd.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- is_file_arg_position --------

    #[test]
    fn file_arg_position_triggers_after_export_space() {
        // Cursor at end of "/export " — no path yet, but the slash command
        // is committed and tab should list files.
        assert!(is_file_arg_position("/export ", "/export ".len()));
    }

    #[test]
    fn file_arg_position_triggers_with_partial_path() {
        let line = "/export src/main";
        assert!(is_file_arg_position(line, line.len()));
    }

    #[test]
    fn file_arg_position_triggers_for_plugin_install() {
        let line = "/plugin install ./my-";
        assert!(is_file_arg_position(line, line.len()));
    }

    #[test]
    fn file_arg_position_does_not_trigger_without_trailing_space() {
        // No space after `/export` means the user is still typing the
        // command name — slash-name completion should run, not filename.
        let line = "/export";
        assert!(!is_file_arg_position(line, line.len()));
    }

    #[test]
    fn file_arg_position_does_not_trigger_on_partial_command() {
        // `/expor` is a typo on the way to `/export`; treat it as a
        // slash-name completion target.
        let line = "/expor";
        assert!(!is_file_arg_position(line, line.len()));
    }

    #[test]
    fn file_arg_position_does_not_trigger_for_non_file_command() {
        // `/model ` takes a model alias, not a path — slash-name completion
        // (which includes the model-alias entries from the prepared list)
        // is the right behavior.
        let line = "/model opus";
        assert!(!is_file_arg_position(line, line.len()));
    }

    #[test]
    fn file_arg_position_respects_cursor_before_space() {
        // Cursor *before* the space — user is still inside the command
        // name, even if a space comes later.
        let line = "/export foo";
        let pos_inside_command_name = "/expor".len();
        assert!(!is_file_arg_position(line, pos_inside_command_name));
    }

    #[test]
    fn file_arg_position_handles_empty_line() {
        assert!(!is_file_arg_position("", 0));
    }

    #[test]
    fn file_arg_position_handles_oob_cursor_safely() {
        // Defensive — `pos > line.len()` should not panic, just decline.
        assert!(!is_file_arg_position("/export ", 100));
    }

    // -------- slash_command_prefix --------

    #[test]
    fn slash_prefix_requires_cursor_at_end() {
        // Cursor not at end → None (no completion).
        let line = "/foo bar";
        assert!(slash_command_prefix(line, 3).is_none());
    }

    #[test]
    fn slash_prefix_requires_leading_slash() {
        // No leading slash → None.
        let line = "foo";
        assert!(slash_command_prefix(line, line.len()).is_none());
    }

    #[test]
    fn slash_prefix_returns_full_prefix_at_end() {
        let line = "/sess";
        assert_eq!(slash_command_prefix(line, line.len()), Some("/sess"));
    }

    // -------- normalize_completions --------

    #[test]
    fn normalize_drops_entries_without_leading_slash() {
        let input = vec![
            ("/foo".to_string(), "desc".to_string()),
            ("bar".to_string(), "desc".to_string()),
        ];
        let out = normalize_completions(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "/foo");
    }

    #[test]
    fn normalize_dedupes_keeping_first() {
        let input = vec![
            ("/foo".to_string(), "first".to_string()),
            ("/foo".to_string(), "second".to_string()),
        ];
        let out = normalize_completions(input);
        assert_eq!(out.len(), 1);
        // First insertion wins.
        assert_eq!(out[0].1, "first");
    }
}
