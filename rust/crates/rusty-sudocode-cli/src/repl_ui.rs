//! iocraft-based terminal UI for the REPL.
//!
//! **`ReplApp` / `spawn_repl_ui`** — full iocraft REPL component that
//! replaces rustyline for interactive input.  Shows a persistent prompt
//! with spinner overlay.  The coordinator thread reads `InputEvent`s from
//! the returned `ReplHandle` and dispatches turns.
//!
//! # ChromeSlot pattern
//!
//! The REPL chrome uses a **ChromeSlot** convention: each position in the
//! layout is an enum that renders exactly one variant at a time.  The enum
//! + match makes the contract visible to any reader:
//!
//! ```text
//! [StatusSlot]    ← spinner | turn_result (auto-dismiss) | tips (first-run) | empty
//! ──── separator ────
//! [InputSlot]     ← text_input | question_panel   (Option<QuestionPromptView>)
//! ──── separator ────
//! [FooterSlot]    ← full | minimal | turn_active
//! ```

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use commands::suggest_slash_commands;
use iocraft::prelude::*;

// ── SpinnerState ───────────────────────────────────────────────────────

/// Shared state for the spinner, read by the render thread and written
/// by the runner thread. All fields are atomic for lock-free cross-thread
/// access.
#[derive(Clone)]
pub struct SpinnerState {
    pub response_bytes: Arc<AtomicU32>,
    pub is_thinking: Arc<AtomicBool>,
    pub is_paused: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    start_time: Arc<Mutex<Instant>>,
    label: Arc<Mutex<String>>,
    model: Arc<Mutex<Option<String>>>,
    token_budget: Arc<Mutex<Option<u32>>>,
}

impl SpinnerState {
    /// Create a new inactive spinner state.
    #[must_use]
    pub fn new_inactive() -> Self {
        Self {
            response_bytes: Arc::new(AtomicU32::new(0)),
            is_thinking: Arc::new(AtomicBool::new(false)),
            is_paused: Arc::new(AtomicBool::new(false)),
            active: Arc::new(AtomicBool::new(false)),
            start_time: Arc::new(Mutex::new(Instant::now())),
            label: Arc::new(Mutex::new(String::new())),
            model: Arc::new(Mutex::new(None)),
            token_budget: Arc::new(Mutex::new(None)),
        }
    }

    /// Reset and activate for a new turn.
    pub fn start_turn(&self, label: &str, model: Option<&str>, token_budget: Option<u32>) {
        self.response_bytes.store(0, Ordering::SeqCst);
        self.is_thinking.store(false, Ordering::SeqCst);
        self.is_paused.store(false, Ordering::SeqCst);
        *self.start_time.lock().unwrap() = Instant::now();
        *self.label.lock().unwrap() = label.to_string();
        *self.model.lock().unwrap() = model.map(ToString::to_string);
        *self.token_budget.lock().unwrap() = token_budget;
        self.active.store(true, Ordering::SeqCst);
    }

    /// Deactivate after a turn ends.
    pub fn stop_turn(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Render the current spinner frame as a colored ANSI string.
    /// Returns empty string when inactive or paused.
    pub fn render_frame(&self, frame_index: usize) -> String {
        if !self.active.load(Ordering::SeqCst) {
            return String::new();
        }
        if self.is_paused.load(Ordering::SeqCst) {
            return String::new();
        }

        let thinking = self.is_thinking.load(Ordering::SeqCst);
        let frames: &[&str] = if thinking {
            &["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"]
        } else {
            &[
                "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
                "\u{2827}", "\u{2807}", "\u{280f}",
            ]
        };
        let label = self.label.lock().unwrap();
        let current_label = if thinking {
            "\u{1f9e0} Reasoning..."
        } else {
            &label
        };

        let frame = frames[frame_index % frames.len()];
        let elapsed = self.start_time.lock().unwrap().elapsed().as_secs_f64();

        let mut line = format!("{frame} {current_label}");
        if let Some(ref m) = *self.model.lock().unwrap() {
            let _ = write!(line, " [{m}]");
        }
        let _ = write!(line, " ({elapsed:.1}s)");

        let bytes = self.response_bytes.load(Ordering::Relaxed);
        let token_budget = *self.token_budget.lock().unwrap();
        if bytes > 0 && elapsed >= 1.0 {
            let approx_tokens = bytes / 4;
            if let Some(budget) = token_budget {
                let pct = (f64::from(approx_tokens) / f64::from(budget) * 100.0).min(100.0);
                let fmt_t = format_compact_tokens(approx_tokens);
                let fmt_b = format_compact_tokens(budget);
                let _ = write!(line, " \u{2193} {fmt_t} / {fmt_b} ({pct:.0}%)");
                if approx_tokens >= 2000 && elapsed > 5.0 {
                    let rate = f64::from(approx_tokens) / elapsed;
                    let remaining = f64::from(budget.saturating_sub(approx_tokens));
                    let eta_secs = remaining / rate;
                    let _ = if eta_secs >= 60.0 {
                        write!(line, " ~{:.0}m", eta_secs / 60.0)
                    } else {
                        write!(line, " ~{eta_secs:.0}s")
                    };
                }
            } else {
                let _ = if approx_tokens >= 1000 {
                    write!(
                        line,
                        " \u{2193} {:.1}k tokens",
                        f64::from(approx_tokens) / 1000.0
                    )
                } else {
                    write!(line, " \u{2193} {approx_tokens} tokens")
                };
            }
        }

        // Stall detection: yellow when no new bytes for 3+ seconds.
        let is_stalled = bytes > 0 && !thinking && elapsed > 3.0;
        let color_code = if is_stalled { "33" } else { "34" };
        format!("\x1b[{color_code}m{line}\x1b[0m")
    }
}

fn format_compact_tokens(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", f64::from(tokens) / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", f64::from(tokens) / 1_000.0)
    } else {
        tokens.to_string()
    }
}

// ── ChromeSlot enums ──────────────────────────────────────────────────

/// ChromeSlot: status area above the separator. One variant renders at a time.
///
/// Priority: Spinner > TurnResult > Tips > Empty.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StatusSlot {
    /// Animated progress during a turn.
    Spinner(String),
    /// Turn result (tokens/cost/ctx). Auto-dismisses after ~5 seconds.
    TurnResult(String),
    /// First-run tips. Dismissed on first submit.
    Tips,
    /// Nothing — slot not rendered, saves vertical space.
    Empty,
}

/// ChromeSlot: bottom hint line. Content varies by lifecycle phase.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FooterSlot {
    /// Before first submit — full hints.
    Full,
    /// After first submit — just permission mode + essentials.
    Minimal,
    /// During turn — permission mode only.
    TurnActive,
}

fn format_footer_text(slot: &FooterSlot, perm: &str) -> String {
    match slot {
        FooterSlot::Full => format!(
            "  \u{23f5}\u{23f5} {perm} \u{00b7} /help for commands \u{00b7} /exit to quit \u{00b7} Tab for /commands"
        ),
        FooterSlot::Minimal => format!(
            "  \u{23f5}\u{23f5} {perm} \u{00b7} /help \u{00b7} /exit to quit"
        ),
        FooterSlot::TurnActive => format!("  \u{23f5}\u{23f5} {perm}"),
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// iocraft REPL — replaces rustyline for interactive input
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOptionView {
    pub label: String,
    pub value: String,
    pub description: Option<String>,
    pub recommended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionPromptView {
    pub title: Option<String>,
    pub description: Option<String>,
    pub index: usize,
    pub total: usize,
    pub prompt: String,
    pub options: Vec<QuestionOptionView>,
    pub allow_custom_input: bool,
    pub custom_input_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    ShowQuestion(QuestionPromptView),
    ClearQuestion,
    SetTurnResult(String),
}

#[derive(Clone)]
pub struct UiCommandSender {
    tx: SyncSender<UiCommand>,
}

impl UiCommandSender {
    pub fn show_question(&self, question: QuestionPromptView) {
        let _ = self.tx.send(UiCommand::ShowQuestion(question));
    }

    pub fn clear_question(&self) {
        let _ = self.tx.send(UiCommand::ClearQuestion);
    }

    pub fn set_turn_result(&self, text: &str) {
        let _ = self.tx.send(UiCommand::SetTurnResult(text.to_string()));
    }
}

fn enter_key_event_for_value(
    question: Option<&QuestionPromptView>,
    selected_index: usize,
    value: &str,
) -> Option<InputEvent> {
    if let Some(question) = question {
        if question.options.is_empty() {
            return (!value.trim().is_empty())
                .then(|| InputEvent::QuestionAnswer(value.to_string()));
        }
        if value.trim().is_empty() {
            let selected = selected_index.min(question.options.len().saturating_sub(1));
            return Some(InputEvent::QuestionAnswer((selected + 1).to_string()));
        }
        return Some(InputEvent::QuestionAnswer(value.to_string()));
    }

    if value.trim().is_empty() {
        return None;
    }

    let trimmed = value.trim();
    if trimmed == "/exit" || trimmed == "/quit" {
        Some(InputEvent::Exit)
    } else {
        Some(InputEvent::Submit(value.to_string()))
    }
}

fn format_question_panel(question: &QuestionPromptView, selected_index: usize) -> String {
    let mut lines = Vec::new();
    if let Some(title) = question.title.as_deref().filter(|title| !title.is_empty()) {
        lines.push(format!("[{title}]"));
    }
    if let Some(description) = question
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
    {
        lines.push(description.to_string());
    }
    lines.push(format!(
        "{}/{}  {}",
        question.index + 1,
        question.total.max(1),
        question.prompt
    ));
    for (index, option) in question.options.iter().enumerate() {
        let selector = if index == selected_index { ">" } else { " " };
        let marker = if option.recommended {
            " recommended"
        } else {
            ""
        };
        let description = option
            .description
            .as_deref()
            .filter(|description| !description.is_empty())
            .map_or(String::new(), |description| format!(" - {description}"));
        lines.push(format!(
            "{selector} {}. {}{}{}",
            index + 1,
            option.label,
            marker,
            description
        ));
    }
    lines.join("\n")
}

/// Message type for the output channel: distinguishes complete lines
/// (which need a trailing newline) from raw byte chunks (which already
/// include their own newlines from the markdown renderer).
enum OutputMsg {
    /// Complete line — `StdoutHandle::println` will append `\n`.
    Line(String),
    /// Raw chunk — `StdoutHandle::print`, no extra newline.
    Raw(String),
}

/// Channel-backed output handle for routing text from the runner thread
/// to the iocraft render loop. Clone is cheap. Implements `std::io::Write`
/// so it can be used as a stdout replacement.
#[derive(Clone)]
pub struct OutputSender {
    tx: SyncSender<OutputMsg>,
}

impl OutputSender {
    pub fn println(&self, text: &str) {
        let _ = self.tx.send(OutputMsg::Line(text.to_string()));
    }
}

impl Write for OutputSender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf).to_string();
        let _ = self.tx.send(OutputMsg::Raw(text));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Events from the iocraft UI to the coordinator thread.
#[derive(Debug, PartialEq, Eq)]
pub enum InputEvent {
    Submit(String),
    QuestionAnswer(String),
    /// ESC pressed — cancel the running turn.
    Abort,
    Exit,
}

/// Handle returned by `spawn_repl_ui` for the coordinator to use.
pub struct ReplHandle {
    pub output: OutputSender,
    pub ui: UiCommandSender,
    pub input_rx: Receiver<InputEvent>,
    pub spinner: SpinnerState,
    ui_thread: Option<std::thread::JoinHandle<()>>,
}

impl ReplHandle {
    /// Wait for the iocraft render loop thread to exit, with a timeout.
    /// If the thread doesn't exit within 500ms (e.g. Windows PTY
    /// interaction), abandon it — the process exit will clean up.
    pub fn join(self) {
        // Drop channels so the render loop sees Disconnected and exits.
        drop(self.output);
        drop(self.ui);
        drop(self.input_rx);
        if let Some(h) = self.ui_thread {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
            while !h.is_finished() {
                if std::time::Instant::now() >= deadline {
                    return; // Abandon — process exit will clean up.
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = h.join();
        }
    }
}

/// Strip ANSI escape sequences (ESC[...m) to count visible characters.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Consume '[' then everything up to and including the terminating letter.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Context passed to `ReplApp` via `ContextProvider`.
struct ReplContext {
    output_rx: Arc<Mutex<Receiver<OutputMsg>>>,
    ui_rx: Arc<Mutex<Receiver<UiCommand>>>,
    input_tx: SyncSender<InputEvent>,
    spinner: SpinnerState,
    permission_mode: String,
    tips_line: String,
}

#[component]
fn ReplApp(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let ctx = hooks.use_context::<ReplContext>();
    let output_rx = Arc::clone(&ctx.output_rx);
    let ui_rx = Arc::clone(&ctx.ui_rx);
    let input_tx = ctx.input_tx.clone();
    let spinner = ctx.spinner.clone();
    let permission_mode = ctx.permission_mode.clone();
    let tips_text = ctx.tips_line.clone();
    drop(ctx);

    // use_terminal_size must be called before use_future and use_terminal_events
    // to maintain consistent hook ordering.
    let (term_width, _) = hooks.use_terminal_size();

    let (stdout, _stderr) = hooks.use_output();
    let mut system = hooks.use_context_mut::<SystemContext>();
    let mut input_value = hooks.use_state(String::new);
    let mut frame = hooks.use_state(|| 0usize);
    let mut spinner_text = hooks.use_state(String::new);
    let mut turn_result = hooks.use_state(|| None::<String>);
    let mut turn_result_time = hooks.use_state(|| None::<Instant>);
    let mut has_submitted = hooks.use_state(|| false);
    let mut question_state = hooks.use_state(|| None::<QuestionPromptView>);
    let mut question_selection = hooks.use_state(|| 0usize);
    let mut should_exit = hooks.use_state(|| false);
    let mut last_ctrlc = hooks.use_state(|| None::<Instant>);
    let mut history = hooks.use_state(Vec::<String>::new);
    let mut history_cursor = hooks.use_state(|| None::<usize>);
    let mut saved_input = hooks.use_state(String::new);
    let mut tab_candidates = hooks.use_state(Vec::<String>::new);
    let mut tab_index = hooks.use_state(|| 0usize);

    // Clone handles for the future (StdoutHandle is Clone).
    let stdout_for_future = stdout.clone();
    let spinner_for_future = spinner.clone();
    let output_rx_for_future = Arc::clone(&output_rx);
    let ui_rx_for_future = Arc::clone(&ui_rx);

    // 80ms tick loop: drain output/control channels, update spinner text.
    hooks.use_future(async move {
        loop {
            smol::Timer::after(Duration::from_millis(80)).await;

            // Drain output channel.
            if let Ok(rx) = output_rx_for_future.lock() {
                loop {
                    match rx.try_recv() {
                        Ok(OutputMsg::Line(text)) => stdout_for_future.println(text),
                        Ok(OutputMsg::Raw(text)) => {
                            // Raw chunks from the streaming markdown renderer
                            // contain bare \n. In raw mode iocraft needs \r\n.
                            // StdoutHandle::println handles \r\n conversion for
                            // the trailing newline but not for internal ones.
                            // Split on \n and println each line individually.
                            let mut lines = text.split('\n').peekable();
                            while let Some(line) = lines.next() {
                                if lines.peek().is_some() {
                                    stdout_for_future.println(line);
                                } else if !line.is_empty() {
                                    stdout_for_future.print(line);
                                }
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }

            if let Ok(rx) = ui_rx_for_future.lock() {
                loop {
                    match rx.try_recv() {
                        Ok(UiCommand::ShowQuestion(question)) => {
                            question_state.set(Some(question));
                            question_selection.set(0);
                            input_value.set(String::new());
                        }
                        Ok(UiCommand::ClearQuestion) => {
                            question_state.set(None);
                            question_selection.set(0);
                            input_value.set(String::new());
                        }
                        Ok(UiCommand::SetTurnResult(text)) => {
                            turn_result.set(Some(text));
                            turn_result_time.set(Some(Instant::now()));
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }

            // Auto-dismiss turn result after 5 seconds.
            if turn_result_time
                .read()
                .is_some_and(|t| t.elapsed() >= Duration::from_secs(5))
            {
                turn_result.set(None);
                turn_result_time.set(None);
            }

            // Update spinner.
            let idx = frame.get();
            frame.set(idx.wrapping_add(1));
            let text = spinner_for_future.render_frame(idx);
            spinner_text.set(text);
        }
    });

    // Clone for the terminal event handler.
    let input_tx_for_events = input_tx.clone();
    let stdout_for_events = stdout.clone();

    hooks.use_terminal_events({
        move |event| match event {
            TerminalEvent::Key(KeyEvent {
                code,
                kind,
                modifiers,
                ..
            }) if kind != KeyEventKind::Release => {
                match code {
                    KeyCode::Enter if !modifiers.contains(KeyModifiers::SHIFT) => {
                        let val = input_value.read().clone();
                        let question = question_state.read().clone();
                        let selected_index = *question_selection.read();
                        if let Some(event) =
                            enter_key_event_for_value(question.as_ref(), selected_index, &val)
                        {
                            if !*has_submitted.read() {
                                has_submitted.set(true);
                            }
                            match event {
                                InputEvent::Exit => {
                                    should_exit.set(true);
                                    let _ = input_tx_for_events.send(InputEvent::Exit);
                                }
                                InputEvent::Submit(text) => {
                                    let trimmed = text.trim();
                                    if !trimmed.is_empty() {
                                        let mut h = history.write();
                                        if h.last().map_or(true, |last| last != trimmed) {
                                            h.push(trimmed.to_string());
                                        }
                                    }
                                    stdout_for_events
                                        .println(format!("\x1b[1m\u{276f} {val}\x1b[0m"));
                                    let _ = input_tx_for_events.send(InputEvent::Submit(text));
                                }
                                InputEvent::QuestionAnswer(_) => {
                                    let _ =
                                        input_tx_for_events.send(InputEvent::QuestionAnswer(val));
                                }
                                InputEvent::Abort => {}
                            }
                            input_value.set(String::new());
                            if history_cursor.get().is_some() {
                                history_cursor.set(None);
                                saved_input.set(String::new());
                            }
                        }
                    }
                    KeyCode::Up
                        if question_state
                            .read()
                            .as_ref()
                            .is_some_and(|q| !q.options.is_empty()) =>
                    {
                        let current = *question_selection.read();
                        question_selection.set(current.saturating_sub(1));
                    }
                    KeyCode::Down
                        if question_state
                            .read()
                            .as_ref()
                            .is_some_and(|q| !q.options.is_empty()) =>
                    {
                        let option_count = question_state
                            .read()
                            .as_ref()
                            .map_or(0, |question| question.options.len());
                        let current = *question_selection.read();
                        question_selection.set((current + 1).min(option_count.saturating_sub(1)));
                    }
                    KeyCode::Up if question_state.read().is_none() => {
                        let h = history.read();
                        if !h.is_empty() {
                            let cursor = history_cursor.get();
                            let new_cursor = match cursor {
                                None => {
                                    saved_input.set(input_value.read().clone());
                                    h.len() - 1
                                }
                                Some(0) => 0,
                                Some(c) => c - 1,
                            };
                            input_value.set(h[new_cursor].clone());
                            history_cursor.set(Some(new_cursor));
                        }
                    }
                    KeyCode::Down if question_state.read().is_none() => {
                        if let Some(cursor) = history_cursor.get() {
                            let h = history.read();
                            if cursor + 1 < h.len() {
                                let new_cursor = cursor + 1;
                                input_value.set(h[new_cursor].clone());
                                history_cursor.set(Some(new_cursor));
                            } else {
                                input_value.set(saved_input.read().clone());
                                history_cursor.set(None);
                            }
                        }
                    }
                    KeyCode::Char(ch)
                        if question_state.read().as_ref().is_some_and(|q| {
                            q.options.len() >= 2
                                && ch.is_ascii_digit()
                                && !modifiers.contains(KeyModifiers::CONTROL)
                                && !modifiers.contains(KeyModifiers::ALT)
                        }) =>
                    {
                        let digit = ch.to_digit(10).unwrap_or(0) as usize;
                        let option_count = question_state
                            .read()
                            .as_ref()
                            .map_or(0, |question| question.options.len());
                        if digit >= 1 && digit <= option_count {
                            let _ = input_tx_for_events
                                .send(InputEvent::QuestionAnswer(digit.to_string()));
                            input_value.set(String::new());
                        }
                    }
                    KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = input_tx_for_events.send(InputEvent::Exit);
                        should_exit.set(true);
                    }
                    KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                        input_value.set(String::new());
                        history_cursor.set(None);
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl-C: first press shows hint + cancels turn;
                        // second press within 800ms exits.
                        let now = Instant::now();
                        if last_ctrlc
                            .read()
                            .is_some_and(|t| now.duration_since(t).as_millis() < 800)
                        {
                            let _ = input_tx_for_events.send(InputEvent::Exit);
                            should_exit.set(true);
                        } else {
                            last_ctrlc.set(Some(now));
                            let _ = input_tx_for_events.send(InputEvent::Abort);
                            stdout_for_events.println("  \x1b[2mPress Ctrl-C again to exit\x1b[0m");
                            input_value.set(String::new());
                        }
                    }
                    KeyCode::Esc => {
                        let _ = input_tx_for_events.send(InputEvent::Abort);
                        input_value.set(String::new());
                    }
                    KeyCode::Tab
                        if question_state.read().is_none()
                            && !modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        let val = input_value.read().clone();
                        if val.starts_with('/') {
                            let candidates = tab_candidates.read().clone();
                            if candidates.len() > 1 && candidates.contains(&val) {
                                let idx = (tab_index.get() + 1) % candidates.len();
                                tab_index.set(idx);
                                input_value.set(candidates[idx].clone());
                            } else {
                                let suggestions = suggest_slash_commands(&val, 10);
                                if !suggestions.is_empty() {
                                    input_value.set(suggestions[0].clone());
                                    tab_candidates.set(suggestions);
                                    tab_index.set(0);
                                }
                            }
                        }
                    }
                    _ => {
                        // Let TextInput handle all other keys via on_change.
                        // Reset tab completion on non-Tab keys, but only when
                        // there are stored candidates — avoids spurious state
                        // changes that would short-circuit poll_change before
                        // UseTerminalEventsImpl processes queued events.
                        if code != KeyCode::Tab && !tab_candidates.read().is_empty() {
                            tab_candidates.set(Vec::new());
                        }
                    }
                }
            }
            _ => {}
        }
    });

    // Exit check: `system` was obtained before the event handler and is
    // NOT captured by the Send closure. The exit flag is set inside the
    // closure; we check it here outside the closure.
    if *should_exit.read() {
        system.exit();
    }

    // ── Derive ChromeSlots from state ──────────────────────────────────
    let st = spinner_text.read().clone();
    let val = input_value.read().clone();
    let question = question_state.read().clone();
    let selected_index = *question_selection.read();
    let submitted = *has_submitted.read();

    // StatusSlot: Spinner > TurnResult > Tips > Empty
    let status_slot = if !st.is_empty() {
        StatusSlot::Spinner(st)
    } else if let Some(ref tr) = *turn_result.read() {
        StatusSlot::TurnResult(tr.clone())
    } else if !submitted {
        StatusSlot::Tips
    } else {
        StatusSlot::Empty
    };

    // FooterSlot
    let footer_slot = if matches!(status_slot, StatusSlot::Spinner(_)) {
        FooterSlot::TurnActive
    } else if submitted {
        FooterSlot::Minimal
    } else {
        FooterSlot::Full
    };

    // InputSlot (ChromeSlot: text_input | question_panel — driven by question_state)
    let question_panel = question
        .as_ref()
        .map(|question| format_question_panel(question, selected_index));
    let prompt_label = if question.is_some() {
        "\u{2753} "
    } else {
        "\u{276f} "
    };

    let perm = permission_mode.clone();
    let w = term_width as usize;
    let sep = "\u{2500}".repeat(w);
    let footer_text = format_footer_text(&footer_slot, &perm);

    element! {
        View(flex_direction: FlexDirection::Column) {
            // StatusSlot
            #(match &status_slot {
                StatusSlot::Spinner(s) => Some(element! { Text(content: s.clone()) }),
                StatusSlot::TurnResult(s) => Some(element! { Text(content: s.clone(), color: Color::DarkGrey) }),
                StatusSlot::Tips => Some(element! { Text(content: tips_text.clone(), color: Color::DarkGrey) }),
                StatusSlot::Empty => None,
            })
            // Separator
            Text(content: sep.clone(), color: Color::DarkGrey)
            // InputSlot
            #(question_panel.map(|panel| element! {
                Text(content: panel, color: Color::Cyan)
            }))
            View(flex_direction: FlexDirection::Row) {
                Text(content: prompt_label)
                TextInput(
                    value: val,
                    has_focus: true,
                    multiline: true,
                    auto_grow: true,
                    on_change: move |new_val: String| {
                        input_value.set(new_val);
                    },
                )
            }
            // Separator
            Text(content: sep, color: Color::DarkGrey)
            // FooterSlot
            Text(content: footer_text, color: Color::DarkGrey)
        }
    }
}

/// Spawn the iocraft REPL UI on a dedicated thread and return a handle
/// for the coordinator to communicate with it.
///
/// The coordinator reads `InputEvent`s from `ReplHandle::input_rx` and
/// sends output text via `ReplHandle::output`. The spinner state is
/// shared so the runner thread can update it atomically.
pub fn spawn_repl_ui(permission_mode: &str, startup_banner: &str) -> ReplHandle {
    let (output_tx, output_rx) = mpsc::sync_channel::<OutputMsg>(512);
    let (ui_tx, ui_rx) = mpsc::sync_channel::<UiCommand>(16);
    let (input_tx, input_rx) = mpsc::sync_channel::<InputEvent>(16);
    let spinner = SpinnerState::new_inactive();

    let ctx = ReplContext {
        output_rx: Arc::new(Mutex::new(output_rx)),
        ui_rx: Arc::new(Mutex::new(ui_rx)),
        input_tx: input_tx.clone(),
        spinner: spinner.clone(),
        permission_mode: permission_mode.to_string(),
        tips_line: "Type /help for commands \u{00b7} /status for live context \u{00b7} /resume latest jumps back to the newest session \u{00b7} /diff then /commit to ship \u{00b7} Tab for /command completions".to_string(),
    };

    let banner = startup_banner.to_string();
    let join_handle = std::thread::Builder::new()
        .name("repl-ui".into())
        .spawn(move || {
            // Print banner before entering the render loop so the user sees
            // session info in the scrollback. iocraft's render_loop enters
            // raw mode immediately, so we print first.
            println!("{banner}");
            smol::block_on(
                element! {
                    ContextProvider(value: Context::owned(ctx)) {
                        ReplApp
                    }
                }
                .render_loop()
                .ignore_ctrl_c(),
            )
            .expect("iocraft render_loop failed");
        })
        .expect("spawn repl-ui thread");
    // Small delay to let the render loop enter raw mode before the
    // coordinator starts sending events.
    std::thread::sleep(std::time::Duration::from_millis(50));

    ReplHandle {
        output: OutputSender { tx: output_tx },
        ui: UiCommandSender { tx: ui_tx },
        input_rx,
        spinner,
        ui_thread: Some(join_handle),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question_with_options() -> QuestionPromptView {
        QuestionPromptView {
            title: Some("Setup".to_string()),
            description: None,
            index: 0,
            total: 1,
            prompt: "Pick one".to_string(),
            options: vec![
                QuestionOptionView {
                    label: "Project".to_string(),
                    value: "project".to_string(),
                    description: None,
                    recommended: true,
                },
                QuestionOptionView {
                    label: "User".to_string(),
                    value: "user".to_string(),
                    description: None,
                    recommended: false,
                },
            ],
            allow_custom_input: false,
            custom_input_hint: None,
        }
    }

    #[test]
    fn enter_in_question_mode_routes_answer_instead_of_prompt() {
        let question = QuestionPromptView {
            title: None,
            description: None,
            index: 0,
            total: 1,
            prompt: "Paste content".to_string(),
            options: vec![],
            allow_custom_input: true,
            custom_input_hint: None,
        };

        assert_eq!(
            enter_key_event_for_value(Some(&question), 0, "answer"),
            Some(InputEvent::QuestionAnswer("answer".to_string()))
        );
    }

    #[test]
    fn enter_in_normal_mode_routes_submit() {
        assert_eq!(
            enter_key_event_for_value(None, 0, "hello"),
            Some(InputEvent::Submit("hello".to_string()))
        );
    }

    #[test]
    fn enter_in_option_question_confirms_selected_option() {
        let question = question_with_options();

        assert_eq!(
            enter_key_event_for_value(Some(&question), 1, ""),
            Some(InputEvent::QuestionAnswer("2".to_string()))
        );
    }

    #[test]
    fn question_panel_marks_selected_option() {
        let panel = format_question_panel(&question_with_options(), 1);

        assert!(panel.contains("[Setup]"));
        assert!(panel.contains("  1. Project recommended"));
        assert!(panel.contains("> 2. User"));
    }

    #[test]
    fn exit_command_routes_as_exit_event() {
        assert_eq!(
            enter_key_event_for_value(None, 0, "/exit"),
            Some(InputEvent::Exit)
        );
        assert_eq!(
            enter_key_event_for_value(None, 0, "/quit"),
            Some(InputEvent::Exit)
        );
    }

    #[test]
    fn empty_input_returns_none() {
        assert_eq!(enter_key_event_for_value(None, 0, ""), None);
        assert_eq!(enter_key_event_for_value(None, 0, "   "), None);
    }

    #[test]
    fn slash_commands_route_as_submit() {
        assert_eq!(
            enter_key_event_for_value(None, 0, "/status"),
            Some(InputEvent::Submit("/status".to_string()))
        );
        assert_eq!(
            enter_key_event_for_value(None, 0, "/cost"),
            Some(InputEvent::Submit("/cost".to_string()))
        );
    }

    #[test]
    fn suggest_slash_commands_finds_version_prefix() {
        let suggestions = suggest_slash_commands("/ver", 10);
        assert!(
            suggestions.contains(&"/version".to_string()),
            "expected /version in suggestions for /ver, got: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_slash_commands_finds_help_prefix() {
        let suggestions = suggest_slash_commands("/he", 10);
        assert!(
            suggestions.contains(&"/help".to_string()),
            "expected /help in suggestions for /he, got: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_slash_commands_no_false_positives_for_unrelated_prefix() {
        let suggestions = suggest_slash_commands("/ver", 10);
        // /ver prefix should NOT match unrelated commands like /cost
        assert!(
            !suggestions.contains(&"/cost".to_string()),
            "/cost should not appear in suggestions for /ver, got: {suggestions:?}"
        );
    }
}
