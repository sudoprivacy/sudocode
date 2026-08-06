use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};

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

/// Callback invoked by `UpArrowHandler` when the input buffer is empty.
/// Returns `Some(text)` to splice into the buffer (async REPL dequeue);
/// returns `None` to fall through to history navigation.
pub type UpArrowDequeueHook = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync + 'static>;

/// Callback invoked on ESC press to cancel the currently-running turn.
/// The async REPL passes `abort_signal.abort()` here; the sync REPL
/// leaves it `None` (ESC is handled by `HookAbortMonitor` on a separate
/// thread in sync mode).
pub type EscAbortHook = Arc<dyn Fn() + Send + Sync + 'static>;

/// Rustyline handler for ESC: calls the abort hook (cancels the running
/// turn) and consumes the key. Only bound in async REPL mode — the sync
/// REPL uses `HookAbortMonitor`'s raw-mode stdin listener instead.
struct EscCancelHandler {
    abort_hook: EscAbortHook,
}

impl ConditionalEventHandler for EscCancelHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        (self.abort_hook)();
        Some(Cmd::Noop)
    }
}

/// Rustyline handler for `↑`: moves cursor to the line above (or to the
/// beginning of the current line on single-line input), and only navigates
/// history when the cursor is already at the top-left.  When an async REPL
/// dequeue hook is installed, an empty buffer triggers dequeue instead of
/// history.  This matches Claude Code's arrow-key UX.
struct UpArrowHandler {
    dequeue_hook: Option<UpArrowDequeueHook>,
}

impl ConditionalEventHandler for UpArrowHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        // Empty buffer: try dequeue (async REPL), then history.
        if ctx.line().is_empty() {
            if let Some(ref hook) = self.dequeue_hook {
                if let Some(text) = (hook)() {
                    return Some(Cmd::Insert(1, text));
                }
            }
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
/// When the line is empty, Enter is a no-op.
struct AcceptNonEmpty;

impl ConditionalEventHandler for AcceptNonEmpty {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if ctx.line().trim().is_empty() {
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

/// Shared map of pasted images: hash -> (base64, mime_type).
type ImageMap = Arc<Mutex<HashMap<String, (String, String)>>>;

/// Intercepts `BracketedPasteStart` (fired by Cmd+V on macOS).  If the
/// clipboard contains only image data (no text), register the image and
/// display an `[Image #HASH_PREFIX]` indicator above the prompt.  When
/// the clipboard has text, returns `None` so rustyline's default
/// paste-text path runs instead.
struct ImagePasteHandler {
    images: ImageMap,
}

impl ConditionalEventHandler for ImagePasteHandler {
    fn handle(
        &self,
        _evt: &rustyline::Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let mut cb = arboard::Clipboard::new().ok()?;

        // If the clipboard has text, let rustyline's default bracketed-paste
        // handler insert it (return None → fall through to default keymap).
        if cb.get_text().ok().is_some_and(|t| !t.is_empty()) {
            return None;
        }

        let img_data = cb.get_image().ok()?;
        let registry = runtime::ImageRegistry::default_cache().ok()?;
        let rgba: Vec<u8> = img_data.bytes.to_vec();
        let registered = registry
            .register_rgba(
                u32::try_from(img_data.width).unwrap_or(0),
                u32::try_from(img_data.height).unwrap_or(0),
                &rgba,
            )
            .ok()?;

        let (b64, mime) = registry.load(&registered.hash).ok()?;

        let mut images = self.images.lock().ok()?;
        // If this exact hash is already inserted, don't duplicate.
        if images.contains_key(&registered.hash) {
            return Some(Cmd::Noop);
        }
        images.insert(registered.hash.clone(), (b64, mime));
        drop(images);

        // Print a simple indicator on the current line. When running
        // inside mp.suspend() (REPL mode), bars are hidden and this is
        // safe. The indicator appears above the prompt on the next
        // redraw.
        let hash_prefix = &registered.hash[..12];
        let mut stdout = std::io::stdout();
        write!(stdout, "\r\x1b[2K  \x1b[1m[Image #{hash_prefix}]\x1b[0m\n").ok();
        stdout.flush().ok();

        Some(Cmd::Noop)
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
    /// Shared image map populated by the `ImagePasteHandler`.
    images: ImageMap,
    /// Optional abort hook called on Ctrl-C with empty buffer (async REPL).
    /// Cancels the running turn so Ctrl-C works the same as ESC for
    /// turn cancellation, plus integrates with `cancel::record_ctrlc()`
    /// for double-press exit.
    abort_hook: Option<EscAbortHook>,
}

impl LineEditor {
    #[must_use]
    pub fn new(prompt: impl Into<String>, completions: Vec<(String, String)>) -> Self {
        Self::new_with_dequeue_hook(prompt, completions, None, None)
    }

    /// Same as [`new`] but binds `↑` (on an empty buffer) to `dequeue_hook`
    /// and optionally binds ESC to `esc_abort_hook`.
    ///
    /// The async REPL uses the dequeue hook to pop the newest queued input
    /// back into the editor for editing, and the ESC hook to cancel the
    /// currently-running turn. The sync REPL passes `None` for both.
    #[must_use]
    pub fn new_with_dequeue_hook(
        prompt: impl Into<String>,
        completions: Vec<(String, String)>,
        dequeue_hook: Option<UpArrowDequeueHook>,
        esc_abort_hook: Option<EscAbortHook>,
    ) -> Self {
        let mut config_builder = Config::builder()
            .completion_type(CompletionType::List)
            .edit_mode(EditMode::Emacs);
        // When an ESC abort hook is installed (async REPL), set a keyseq
        // timeout so rustyline recognizes standalone ESC after 50ms instead
        // of blocking indefinitely for a follow-up key (Emacs Meta prefix).
        if esc_abort_hook.is_some() {
            config_builder = config_builder.keyseq_timeout(Some(50));
        }
        let config = config_builder.build();
        let mut editor = Editor::<SlashCommandHelper, DefaultHistory>::with_config(config)
            .expect("rustyline editor should initialize");
        editor.set_helper(Some(SlashCommandHelper::new(completions)));
        editor.bind_sequence(KeyEvent(KeyCode::Char('J'), Modifiers::CTRL), Cmd::Newline);
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::SHIFT), Cmd::Newline);
        editor.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::NONE),
            EventHandler::Conditional(Box::new(AcceptNonEmpty)),
        );

        // ↑: beginning-of-line first, then history (+ optional dequeue for async REPL).
        // ↓: end-of-line first, then history.
        // Matches Claude Code's arrow-key UX.
        editor.bind_sequence(
            KeyEvent(KeyCode::Up, Modifiers::NONE),
            EventHandler::Conditional(Box::new(UpArrowHandler { dequeue_hook })),
        );
        editor.bind_sequence(
            KeyEvent(KeyCode::Down, Modifiers::NONE),
            EventHandler::Conditional(Box::new(DownArrowHandler)),
        );

        // ESC: cancel the running turn (async REPL only). In sync mode the
        // HookAbortMonitor handles ESC on its own raw-mode stdin thread.
        // Clone the hook so the same callback is available for Ctrl-C too.
        if let Some(ref abort_hook) = esc_abort_hook {
            editor.bind_sequence(
                KeyEvent(KeyCode::Esc, Modifiers::NONE),
                EventHandler::Conditional(Box::new(EscCancelHandler {
                    abort_hook: Arc::clone(abort_hook),
                })),
            );
        }

        let images: ImageMap = Arc::new(Mutex::new(HashMap::new()));

        let handler = ImagePasteHandler {
            images: Arc::clone(&images),
        };
        // Cmd+V on macOS triggers the terminal's paste action which sends a
        // bracketed paste sequence.  When the clipboard has only image data
        // the paste content is empty (`ESC[200~ ESC[201~`).  We intercept
        // BracketedPasteStart to check for image data; if no image (i.e. the
        // clipboard has text), we return None so the default paste path runs.
        editor.bind_sequence(
            KeyEvent(KeyCode::BracketedPasteStart, Modifiers::NONE),
            EventHandler::Conditional(Box::new(handler)),
        );
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
            images,
            abort_hook: esc_abort_hook,
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

    /// Drain all images that were pasted during the current input session.
    /// Returns a map of full SHA-256 hash -> (base64_data, mime_type).
    pub fn take_images(&mut self) -> HashMap<String, (String, String)> {
        let mut map = self.images.lock().expect("image map lock");
        std::mem::take(&mut *map)
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
                    } else if let Some(ref hook) = self.abort_hook {
                        // Async REPL: Ctrl-C cancels the running turn (same
                        // as ESC) AND checks for double-press exit via the
                        // shared cancel module.
                        if cancel::is_double_ctrlc() {
                            writeln!(stdout, "\x1b[J")?;
                            stdout.flush()?;
                            return Ok(ReadOutcome::Exit);
                        }
                        cancel::record_ctrlc();
                        (hook)();
                        // Show exit hint.
                        write!(
                            stdout,
                            "\x1b[2E\x1b[2K  \x1b[2mPress Ctrl-C again to exit\x1b[0m\x1b[2F"
                        )?;
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
                            "\x1b[2E\x1b[2K  \x1b[2mPress Ctrl-C again to exit\x1b[0m\x1b[2F"
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
