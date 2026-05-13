use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Context, EditMode, Editor, EventContext,
    EventHandler, Helper, KeyCode, KeyEvent, Modifiers, RepeatCount,
};

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

        // Write the indicator on the pre-allocated line above the prompt
        // chrome.  Layout: indicator is 2 lines above the cursor (prompt).
        let mut stdout = std::io::stdout();
        // Save cursor, move up 2 to indicator line, clear & write, restore.
        write!(stdout, "\x1b7\x1b[2A\x1b[2K").ok();
        let hash_prefix = &registered.hash[..12];
        write!(stdout, "  \x1b[1m[Image #{hash_prefix}]\x1b[0m").ok();
        write!(stdout, "\x1b8").ok();
        stdout.flush().ok();

        Some(Cmd::Noop)
    }
}

struct SlashCommandHelper {
    /// Each entry is (command, description). Description may be empty.
    completions: Vec<(String, String)>,
    current_line: RefCell<String>,
}

impl SlashCommandHelper {
    fn new(completions: Vec<(String, String)>) -> Self {
        Self {
            completions: normalize_completions(completions),
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

pub struct LineEditor {
    prompt: String,
    editor: Editor<SlashCommandHelper, DefaultHistory>,
    /// Whether the previous read returned a Ctrl-C on an empty prompt.
    pending_exit: bool,
    /// Shared image map populated by the `ImagePasteHandler`.
    images: ImageMap,
}

impl LineEditor {
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
        editor.bind_sequence(
            KeyEvent(KeyCode::Enter, Modifiers::NONE),
            EventHandler::Conditional(Box::new(AcceptNonEmpty)),
        );

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
            pending_exit: false,
            images,
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
                    self.pending_exit = false;
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
                        self.pending_exit = false;
                    } else if self.pending_exit {
                        // Second Ctrl-C — clear remaining chrome and exit.
                        writeln!(stdout, "\x1b[J")?;
                        stdout.flush()?;
                        return Ok(ReadOutcome::Exit);
                    } else {
                        self.pending_exit = true;
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
