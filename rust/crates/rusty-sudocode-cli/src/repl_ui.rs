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
//! [StatusSlot]    ← spinner | turn_result | tips | empty
//! ──── separator ────
//! [InputSlot]     ← hint | text_input | question_panel
//! ──── separator ────
//! [FooterSlot]    ← hint (3s) > turn_active > minimal > full
//! ```

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use commands::suggest_slash_commands;
use iocraft::prelude::*;

// ── stderr redirect ───────────────────────────────────────────────────

/// Windows stub — stderr redirect is a no-op on non-Unix platforms.
#[cfg(not(unix))]
mod stderr_redirect {
    pub struct StderrRedirect;
    impl StderrRedirect {
        pub fn activate() -> Option<Self> {
            None
        }
        pub fn drain(&self) -> Option<String> {
            None
        }
    }
}

/// Unix pipe-based stderr redirect: replaces fd 2 with a pipe so that
/// any library writing to stderr (e.g. tracing, nix, openssl) does not
/// corrupt the iocraft canvas.  Captured bytes are drained each tick and
/// routed through the iocraft stdout handle.
///
/// All syscalls go through `nix`'s safe wrappers — no `unsafe` blocks.
#[cfg(unix)]
mod stderr_redirect {
    use std::io::Read;
    use std::os::fd::{AsRawFd, OwnedFd};

    pub struct StderrRedirect {
        read_file: std::fs::File,
    }

    impl StderrRedirect {
        /// Replace fd 2 with the write end of a pipe.  Returns `Some` on
        /// success.  The read end is kept for `drain()`.
        pub fn activate() -> Option<Self> {
            let (read_fd, write_fd): (OwnedFd, OwnedFd) = nix::unistd::pipe().ok()?;

            // Point fd 2 at the write end of the pipe.
            nix::unistd::dup2(write_fd.as_raw_fd(), 2).ok()?;
            // `write_fd` is dropped here — fd 2 keeps the write end alive.

            // Make the read end non-blocking so drain() never stalls.
            let flags =
                nix::fcntl::fcntl(read_fd.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL).ok()?;
            let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
            oflags.insert(nix::fcntl::OFlag::O_NONBLOCK);
            nix::fcntl::fcntl(read_fd.as_raw_fd(), nix::fcntl::FcntlArg::F_SETFL(oflags)).ok()?;

            let read_file = std::fs::File::from(read_fd);
            Some(Self { read_file })
        }

        /// Non-blocking drain: returns captured text or `None`.
        pub fn drain(&self) -> Option<String> {
            let mut buf = [0u8; 4096];
            let mut collected = String::new();
            let mut file = &self.read_file;
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => collected.push_str(&String::from_utf8_lossy(&buf[..n])),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if collected.is_empty() {
                None
            } else {
                Some(collected)
            }
        }
    }
}

// ── TurnPhase + SpinnerState ───────────────────────────────────────────

/// Phase of the current agent turn — drives spinner icon/label selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnPhase {
    Thinking,
    Reasoning,
    Retry {
        attempt: u32,
        max_retries: u32,
        reason: String,
    },
    Paused,
}

/// Shared state for the spinner, read by the render thread and written
/// by the runner thread.
#[derive(Clone)]
pub struct SpinnerState {
    pub response_bytes: Arc<AtomicU32>,
    phase: Arc<Mutex<TurnPhase>>,
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
            phase: Arc::new(Mutex::new(TurnPhase::Thinking)),
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
        *self.phase.lock().unwrap() = TurnPhase::Thinking;
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

    /// Read the current turn phase.
    pub fn phase(&self) -> TurnPhase {
        self.phase.lock().unwrap().clone()
    }

    /// Convenience: check whether the current phase is `Paused`.
    pub fn is_paused(&self) -> bool {
        *self.phase.lock().unwrap() == TurnPhase::Paused
    }

    /// Return a clone of the phase `Arc` for sharing with other threads.
    pub fn phase_arc(&self) -> Arc<Mutex<TurnPhase>> {
        Arc::clone(&self.phase)
    }

    /// Render the current spinner frame as a colored ANSI string.
    /// Returns empty string when inactive or paused.
    pub fn render_frame(&self, frame_index: usize) -> String {
        if !self.active.load(Ordering::SeqCst) {
            return String::new();
        }

        let current_phase = self.phase.lock().unwrap().clone();
        if current_phase == TurnPhase::Paused {
            return String::new();
        }

        let is_reasoning = current_phase == TurnPhase::Reasoning;
        let is_retry = matches!(current_phase, TurnPhase::Retry { .. });

        let (frame, current_label): (String, String) = if is_retry {
            if let TurnPhase::Retry {
                attempt,
                max_retries,
                ref reason,
            } = current_phase
            {
                (
                    "\u{27f3}".to_string(),
                    format!("Retry {attempt}/{max_retries}: {reason}"),
                )
            } else {
                unreachable!()
            }
        } else if is_reasoning {
            let reasoning_frames: &[&str] = &["\u{25d0}", "\u{25d3}", "\u{25d1}", "\u{25d2}"];
            (
                reasoning_frames[frame_index % reasoning_frames.len()].to_string(),
                "\u{1f9e0} Reasoning...".to_string(),
            )
        } else {
            let thinking_frames: &[&str] = &[
                "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
                "\u{2827}", "\u{2807}", "\u{280f}",
            ];
            let label = self.label.lock().unwrap();
            (
                thinking_frames[frame_index % thinking_frames.len()].to_string(),
                label.clone(),
            )
        };

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

        let t = crate::render::theme();
        let color = if is_retry {
            crate::render::ansi_fg(t.warning)
        } else {
            // Stall detection: yellow when no new bytes for 3+ seconds.
            let is_stalled = bytes > 0 && !is_reasoning && elapsed > 3.0;
            if is_stalled {
                crate::render::ansi_fg(t.warning)
            } else {
                crate::render::ansi_fg(t.info)
            }
        };
        format!("{color}{line}{}", crate::render::RESET)
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
    /// Turn result (tokens/cost/ctx). Cleared when the next turn starts.
    TurnResult(String),
    /// First-run tips. Dismissed on first submit.
    Tips,
    /// Nothing — slot not rendered, saves vertical space.
    Empty,
}

/// ChromeSlot: bottom hint line. Content varies by lifecycle phase.
///
/// Priority (highest first): Hint > TurnActive > Minimal > Full.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FooterSlot {
    /// Transient hint — highest priority, auto-dismissed after 3 seconds.
    Hint(String),
    /// Before first submit — full hints.
    Full,
    /// After first submit — just permission mode + essentials.
    Minimal,
    /// During turn — permission mode only.
    TurnActive,
}

fn format_footer_text(slot: &FooterSlot, perm: &str) -> String {
    match slot {
        FooterSlot::Hint(msg) => format!("  {msg}"),
        FooterSlot::Full => format!(
            "  \u{23f5}\u{23f5} {perm} \u{00b7} /help for commands \u{00b7} /exit to quit \u{00b7} Tab for /commands"
        ),
        FooterSlot::Minimal => format!(
            "  \u{23f5}\u{23f5} {perm} \u{00b7} /help \u{00b7} /exit to quit"
        ),
        FooterSlot::TurnActive => format!("  \u{23f5}\u{23f5} {perm}"),
    }
}

/// Maximum number of options for DialPad mode (digit-key shortcuts 1–9).
/// Questions with more options auto-route to FuzzySelect.
const DIALPAD_MAX_OPTIONS: usize = 9;

/// Number of visible candidates in the FuzzySelect scroll window.
const FUZZY_WINDOW_SIZE: usize = 11;

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// iocraft REPL — replaces rustyline for interactive input
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOptionView {
    pub label: String,
    pub value: String,
    pub description: Option<String>,
    pub recommended: bool,
    /// When true, Right arrow drills into this option (tree navigation).
    pub is_navigable: bool,
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
    /// When true, always use FuzzySelect regardless of option count.
    /// Used for config picker and other lists that benefit from search.
    pub force_fuzzy_select: bool,
    /// When set, Left arrow submits this value as the answer (← Back).
    /// Right arrow always submits the current selection (same as Enter).
    pub back_value: Option<String>,
}

/// ChromeSlot: primary interaction area between the two separators.
/// Exactly one variant renders at a time.
#[derive(Clone, Debug)]
enum InputSlot {
    /// Transient hint — dismissed on any keypress.
    Hint(String),
    /// Normal text input with ❯ prompt.
    TextInput,
    /// ≤9 options, digit-key shortcut per item (tool approval, init).
    /// Named after a phone dial pad — the constraint is self-evident.
    DialPad(QuestionPromptView),
    /// Filterable list with scroll window (model picker, session switch).
    /// Auto-selected when options.len() > DIALPAD_MAX_OPTIONS.
    FuzzySelect(FuzzySelectState),
}

#[derive(Clone, Debug)]
struct FuzzySelectState {
    question: QuestionPromptView,
    filter: String,
    /// Indices into `question.options` that match the current filter.
    filtered: Vec<usize>,
    /// Index into `filtered` of the currently highlighted item.
    cursor: usize,
    /// Scroll offset — first visible row in the filtered list.
    scroll: usize,
}

impl FuzzySelectState {
    fn new(question: QuestionPromptView) -> Self {
        let filtered: Vec<usize> = (0..question.options.len()).collect();
        let cursor = question
            .options
            .iter()
            .position(|o| o.recommended)
            .unwrap_or(0);
        let scroll = cursor.saturating_sub(FUZZY_WINDOW_SIZE / 2);
        Self {
            question,
            filter: String::new(),
            filtered,
            cursor,
            scroll,
        }
    }

    fn apply_filter(&mut self) {
        let lower = self.filter.to_ascii_lowercase();
        self.filtered = self
            .question
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| lower.is_empty() || o.label.to_ascii_lowercase().contains(&lower))
            .map(|(i, _)| i)
            .collect();
        self.cursor = 0;
        self.scroll = 0;
    }

    fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
    }

    fn move_down(&mut self) {
        if !self.filtered.is_empty() {
            self.cursor = (self.cursor + 1).min(self.filtered.len() - 1);
            if self.cursor >= self.scroll + FUZZY_WINDOW_SIZE {
                self.scroll = self.cursor + 1 - FUZZY_WINDOW_SIZE;
            }
        }
    }

    fn selected_value(&self) -> Option<String> {
        self.filtered.get(self.cursor).map(|&i| (i + 1).to_string())
    }

    fn selected_option(&self) -> Option<&QuestionOptionView> {
        self.filtered
            .get(self.cursor)
            .and_then(|&i| self.question.options.get(i))
    }

    fn format_panel(&self) -> String {
        let mut lines = Vec::new();
        if let Some(title) = self.question.title.as_deref().filter(|t| !t.is_empty()) {
            lines.push(format!("[{title}]"));
        }
        if let Some(desc) = self
            .question
            .description
            .as_deref()
            .filter(|d| !d.is_empty())
        {
            lines.push(desc.to_string());
        }
        let total = self.filtered.len();
        if self.filter.is_empty() {
            lines.push(format!("{} items  {}", total, self.question.prompt));
        } else {
            lines.push(format!(
                "{total} match  {} (filter: {})",
                self.question.prompt, self.filter
            ));
        }
        let end = (self.scroll + FUZZY_WINDOW_SIZE).min(self.filtered.len());
        for (vi, &oi) in self.filtered[self.scroll..end].iter().enumerate() {
            let abs_vi = self.scroll + vi;
            let option = &self.question.options[oi];
            let selector = if abs_vi == self.cursor { ">" } else { " " };
            let marker = if option.recommended {
                format!(
                    " {}(recommended){}",
                    crate::render::DIM,
                    crate::render::RESET
                )
            } else {
                String::new()
            };
            lines.push(format!("{selector} {}{marker}", option.label));
        }
        if end < self.filtered.len() {
            lines.push(format!("  … {} more", self.filtered.len() - end));
        }
        if self.question.allow_custom_input && !self.filter.is_empty() && self.filtered.is_empty() {
            let hint = self
                .question
                .custom_input_hint
                .as_deref()
                .filter(|hint| !hint.is_empty())
                .unwrap_or("no match — press Enter to use what you typed");
            lines.push(format!(
                "{}  {hint}{}",
                crate::render::DIM,
                crate::render::RESET
            ));
        }
        lines.join("\n")
    }
}

/// A tool call shown as a running (yellow) card in the staging overlay,
/// keyed by `tool_use_id`. Only in-flight calls live here: on completion the
/// card is removed and the finished (green/red) card is written to scrollback
/// by the render engine on the ordered `output` channel — the staging overlay
/// never commits permanent content. Keyed by id (not FIFO) because a denied
/// tool can finish without ever having a running phase, and lookups by id stay
/// correct under missing/out-of-order events.
#[derive(Clone, Debug)]
pub struct ToolCard {
    pub id: String,
    pub name: String,
    pub input: String,
}

#[derive(Clone, Debug)]
pub enum UiCommand {
    ShowQuestion(QuestionPromptView),
    ClearQuestion,
    SetTurnResult(String),
    ShowInputHint(String),
    /// Update the ContextSlot's todo panel with the current todo list.
    UpdateContext(Vec<runtime::Todo>),
    /// A tool call started — add a running (yellow) card to the staging
    /// overlay.
    ToolStarted {
        id: String,
        name: String,
        input: String,
    },
    /// A tool call finished — remove the matching running card from the
    /// overlay. The finished card's permanent content is written to scrollback
    /// by the render engine (ordered `output` channel), NOT here: this only
    /// clears the transient overlay entry.
    ToolFinished {
        id: String,
    },
    /// Replace the queued-input overlay with the coordinator's current queue
    /// (compact display texts, oldest first). Empty clears the overlay. This is
    /// a pure projection of the coordinator's queue — the coordinator sends it
    /// whenever the queue changes (enqueue on submit-during-turn, clear on
    /// drain).
    SetQueue(Vec<String>),
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

    pub fn show_input_hint(&self, text: &str) {
        let _ = self.tx.send(UiCommand::ShowInputHint(text.to_string()));
    }

    pub fn update_context(&self, todos: Vec<runtime::Todo>) {
        let _ = self.tx.send(UiCommand::UpdateContext(todos));
    }

    pub fn tool_started(&self, id: &str, name: &str, input: &str) {
        let _ = self.tx.send(UiCommand::ToolStarted {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        });
    }

    pub fn tool_finished(&self, id: &str) {
        let _ = self.tx.send(UiCommand::ToolFinished { id: id.to_string() });
    }

    pub fn set_queue(&self, display_texts: Vec<String>) {
        let _ = self.tx.send(UiCommand::SetQueue(display_texts));
    }
}

/// Submit the currently selected DialPad option. Shared by Enter key
/// and digit-key shortcut paths (DRY).
fn submit_dialpad_selection(
    question: &QuestionPromptView,
    selected_index: usize,
    input_tx: &SyncSender<InputEvent>,
) {
    let answer = (selected_index + 1).to_string();
    let _ = input_tx.send(InputEvent::QuestionAnswer(answer));
}

// ---------------------------------------------------------------------------
// Paste placeholder helpers (CC-style [Pasted text #N +M lines] placeholders)
// ---------------------------------------------------------------------------

/// Paste length (in characters) at or below which a paste is inserted
/// literally instead of collapsed into a placeholder. Mirrors Claude Code's
/// `PASTE_THRESHOLD` (800).
const PASTE_PLACEHOLDER_CHAR_THRESHOLD: usize = 800;

/// Line count above which a paste collapses into a placeholder regardless of
/// length. Mirrors Claude Code's `maxLines` default (2): 1–2 line pastes stay
/// literal, 3+ lines become a placeholder.
const PASTE_PLACEHOLDER_MAX_LINES: usize = 2;

/// Decide whether a paste should be shown as a compact `[Pasted text #N]`
/// placeholder (true) or inserted into the input box literally (false).
///
/// Matches Claude Code: only long (> 800 chars) or multi-line (> 2 lines)
/// pastes collapse into a placeholder; short single-/double-line pastes are
/// inserted verbatim so the box shows exactly what was pasted. `text` is
/// expected to be newline-normalized already.
fn should_use_paste_placeholder(text: &str) -> bool {
    let newline_count = text.chars().filter(|&c| c == '\n').count();
    text.chars().count() > PASTE_PLACEHOLDER_CHAR_THRESHOLD
        || newline_count > PASTE_PLACEHOLDER_MAX_LINES
}

/// Normalize pasted line endings to `\n`.
fn normalize_paste_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Format a placeholder reference for pasted text.
/// Matches the CC convention so history/session files are compatible.
fn format_paste_placeholder(id: u32, text: &str) -> String {
    let newline_count = text.chars().filter(|&c| c == '\n').count();
    if newline_count == 0 {
        format!("[Pasted text #{id}]")
    } else {
        format!("[Pasted text #{id} +{newline_count} lines]")
    }
}

/// Core expansion engine shared by plain and display variants.
///
/// Scans left to right and copies each placeholder's replacement into a fresh
/// output buffer. Inserted text is never re-scanned, so a stored paste whose
/// own text happens to contain a `[Pasted text #N]` string can never cause an
/// infinite loop (it is emitted verbatim, not re-expanded).
///
/// When `wrap` is `Some((before, after))`, each pasted region is wrapped with
/// the given ANSI sequences for visual distinction in terminal output.
#[inline]
fn expand_paste_inner(
    input: &str,
    store: &std::collections::HashMap<u32, String>,
    wrap: Option<(&str, &str)>,
) -> String {
    if store.is_empty() || !input.contains("[Pasted text #") {
        return input.to_string();
    }
    let placeholder_prefix = "[Pasted text #";
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(rel_start) = rest.find(placeholder_prefix) {
        result.push_str(&rest[..rel_start]);
        let after_prefix = &rest[rel_start + placeholder_prefix.len()..];

        let id_end = after_prefix.find(|c: char| !c.is_ascii_digit());
        let matched = id_end.and_then(|id_end| {
            if id_end == 0 {
                return None;
            }
            let id = after_prefix[..id_end].parse::<u32>().ok()?;
            let tail = &after_prefix[id_end..];
            let close = tail.find(']')?;
            let real_text = store.get(&id)?;
            Some((id_end + close + 1, real_text))
        });

        match matched {
            Some((consumed_after_prefix, real_text)) => {
                if let Some((before, after)) = wrap {
                    result.push_str(before);
                    result.push_str(real_text);
                    result.push_str(after);
                } else {
                    result.push_str(real_text);
                }
                rest = &after_prefix[consumed_after_prefix..];
            }
            None => {
                result.push_str(placeholder_prefix);
                rest = after_prefix;
            }
        }
    }
    result.push_str(rest);
    result
}

/// Replace all `[Pasted text #N ...]` placeholders with the real text (plain).
fn expand_paste_placeholders(
    input: &str,
    store: &std::collections::HashMap<u32, String>,
) -> String {
    expand_paste_inner(input, store, None)
}

/// Replace placeholders with the real text, wrapping each pasted region in DIM
/// so it's visually distinguishable from typed text in scrollback.
fn expand_paste_for_display(input: &str, store: &std::collections::HashMap<u32, String>) -> String {
    expand_paste_inner(
        input,
        store,
        Some((crate::render::DIM, crate::render::RESET)),
    )
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
        Some(InputEvent::Submit {
            text: value.to_string(),
            display: value.to_string(),
        })
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
            "{selector} [{}] {}{}{}",
            index + 1,
            option.label,
            marker,
            description
        ));
    }
    let max_digit = question.options.len().min(9);
    if question.allow_custom_input {
        let hint = question
            .custom_input_hint
            .as_deref()
            .filter(|hint| !hint.is_empty())
            .unwrap_or("type your own answer");
        lines.push(format!("  [+] {hint}"));
    }
    let arrow_hint = if question.back_value.is_some() {
        "\u{2190}\u{2192} back/open \u{00b7} "
    } else {
        ""
    };
    let custom_hint = if question.allow_custom_input {
        " \u{00b7} type to enter your own"
    } else {
        ""
    };
    lines.push(format!(
        "{}  {arrow_hint}\u{2191}\u{2193} navigate \u{00b7} 1-{max_digit} quick select{custom_hint} \u{00b7} Enter confirm{}",
        crate::render::DIM,
        crate::render::RESET,
    ));
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

/// A single call to iocraft's stdout handle. Borrows from the message being
/// split — the only allocation on this per-delta path is the one iocraft's
/// own `ToString` bound makes when the op is issued.
#[derive(Debug, PartialEq, Eq)]
enum OutputOp<'a> {
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
fn split_for_iocraft(text: &str, terminated: bool, mut issue: impl FnMut(OutputOp<'_>)) {
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

/// Default lifetime of the "Press Ctrl-C again to exit" footer hint.
const CTRLC_HINT_TTL: Duration = Duration::from_secs(3);

/// How long that hint stays in the footer.
///
/// Overridable through `SUDOCODE_CTRLC_HINT_TTL_MS` because the hint is a
/// short-lived transient and a PTY test can only observe it by polling the
/// rendered screen. At three seconds a loaded machine can stall a 50ms poll
/// past the deadline, and the hint is then gone for good — the test spins out
/// its whole budget and fails, having tested the scheduler rather than the
/// footer. Handing the test a TTL makes both directions deterministic: a long
/// one to assert *where* the hint renders, a short one to assert that it
/// clears.
///
/// Read per use rather than cached: tests set it per-process before the REPL
/// starts, and a `OnceLock` would freeze whichever value happened to be first.
fn ctrlc_hint_ttl() -> Duration {
    std::env::var("SUDOCODE_CTRLC_HINT_TTL_MS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map_or(CTRLC_HINT_TTL, Duration::from_millis)
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
    /// A submitted line. `text` is the full content (paste placeholders
    /// expanded) sent to the model; `display` is the compact echo form (paste
    /// collapsed). The coordinator — not the UI — writes the `❯ display` echo to
    /// scrollback, so it can defer it when the input is queued behind a turn.
    Submit {
        text: String,
        display: String,
    },
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

    /// Decompose into parts for the coordinator event channel bridge.
    /// The returned join closure replaces `self.join()` — call it after
    /// the coordinator loop exits.
    #[allow(clippy::type_complexity)]
    pub fn split(
        self,
    ) -> (
        OutputSender,
        UiCommandSender,
        Receiver<InputEvent>,
        SpinnerState,
        Box<dyn FnOnce()>,
    ) {
        let join_fn = {
            let ui_thread = self.ui_thread;
            Box::new(move || {
                if let Some(h) = ui_thread {
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(300);
                    while !h.is_finished() {
                        if std::time::Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    let _ = h.join();
                }
            })
        };
        (self.output, self.ui, self.input_rx, self.spinner, join_fn)
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

// ── ContextSlot — persistent area for TodoPanel (+ future sections) ──

/// Render the todo panel matching CC's `TodoWrite` layout:
///
/// ```text
/// 5 todos (2 done, 1 in progress, 2 open)
///   ✓ Update docs
///   ■ Writing unit tests
///   □ Fix login bug
///   … +2 pending, 1 completed
/// ```
///
/// - Header: count summary with done/in_progress/open breakdown
/// - Each visible todo: icon + label (completed = strikethrough+dim, in_progress = bold activeForm)
/// - Truncation: dynamic based on terminal height (CC: `min(10, max(3, rows - 14))`)
/// - Priority order: in_progress > pending > completed; hidden summary
/// Render the staging overlay: the in-flight tool calls as running (yellow)
/// L-frame cards, joined into one multi-line string, capped to a height budget
/// so many concurrent cards can't flood the screen or make every frame redraw
/// hundreds of lines. Overflow collapses to a `… +N more running` line.
///
/// A pure projection of `cards` — it renders only running calls and never
/// commits to scrollback (the render engine does that on the ordered output
/// channel). Height budget mirrors the task panel: `min(10, max(3, rows-14))`,
/// hidden entirely on a very short terminal.
fn render_staging_overlay(cards: &[ToolCard], term_rows: usize) -> String {
    if cards.is_empty() {
        return String::new();
    }
    // Same budget family as render_todo_panel; hide on a very short terminal
    // rather than crowding out the prompt.
    if term_rows <= 10 {
        return String::new();
    }
    let max_lines = 10usize.min(3usize.max(term_rows.saturating_sub(14)));

    let mut lines: Vec<String> = Vec::new();
    let mut shown_cards = 0usize;
    for card in cards {
        let rendered = crate::cli::format::format_tool_call_start(&card.name, &card.input);
        let card_lines: Vec<&str> = rendered.lines().collect();
        // Keep whole cards: stop before a card that would breach the budget,
        // unless nothing has been shown yet (always show at least one card,
        // truncated, so the user sees *something* running).
        if !lines.is_empty() && lines.len() + card_lines.len() > max_lines {
            break;
        }
        for l in &card_lines {
            if lines.len() >= max_lines {
                break;
            }
            lines.push((*l).to_string());
        }
        shown_cards += 1;
    }

    let hidden = cards.len() - shown_cards;
    if hidden > 0 {
        use crate::render::{DIM, RESET};
        lines.push(format!("{DIM}… +{hidden} more running{RESET}"));
    }
    lines.join("\n")
}

/// Render the queue overlay: user inputs queued behind the running turn, one
/// compact `↳ queued: <text>` line each (DIM), joined into one multi-line
/// string. Capped to a height budget like the staging overlay; overflow
/// collapses to a `… +N more queued` line. Hidden on a very short terminal.
///
/// A pure projection of the queued display texts — it holds nothing itself and
/// never commits to scrollback (the coordinator echoes `❯ text` there when the
/// item actually flushes).
fn render_queue_overlay(items: &[String], term_rows: usize) -> String {
    use crate::render::{DIM, RESET};

    if items.is_empty() {
        return String::new();
    }
    if term_rows <= 10 {
        return String::new();
    }
    let max_lines = 10usize.min(3usize.max(term_rows.saturating_sub(14)));

    let mut lines: Vec<String> = Vec::new();
    for item in items {
        if lines.len() >= max_lines {
            break;
        }
        // Collapse to the first line so a multi-line queued input stays one row.
        let first = item.lines().next().unwrap_or("");
        lines.push(format!("{DIM}↳ queued: {first}{RESET}"));
    }

    let hidden = items.len() - lines.len();
    if hidden > 0 {
        // Replace the last shown line with the overflow marker so the total
        // never exceeds the budget.
        lines.pop();
        lines.push(format!("{DIM}… +{} more queued{RESET}", hidden + 1));
    }
    lines.join("\n")
}

fn render_todo_panel(todos: &[runtime::Todo], term_rows: usize) -> String {
    use crate::render::{ansi_fg, theme, BOLD, DIM, RESET};

    if todos.is_empty() {
        return String::new();
    }

    // Dynamic max display: CC uses min(10, max(3, rows - 14)).
    // When terminal is very short (≤10 rows), hide entirely.
    if term_rows <= 10 {
        return String::new();
    }
    let max_display = 10usize.min(3usize.max(term_rows.saturating_sub(14)));

    let t = theme();
    let success = ansi_fg(t.success);
    let info = ansi_fg(t.info);

    let completed_count = todos
        .iter()
        .filter(|t| t.status == runtime::TodoStatus::Completed)
        .count();
    let in_progress_count = todos
        .iter()
        .filter(|t| t.status == runtime::TodoStatus::InProgress)
        .count();
    let open_count = todos.len() - completed_count;

    // Header summary line
    let mut header_parts = vec![format!("{BOLD}{completed_count}{RESET} done")];
    if in_progress_count > 0 {
        header_parts.push(format!("{BOLD}{in_progress_count}{RESET} in progress"));
    }
    header_parts.push(format!("{BOLD}{open_count}{RESET} open"));
    let header = format!(
        "{DIM}{BOLD}{}{RESET}{DIM} todos ({}){}",
        todos.len(),
        header_parts.join(", "),
        RESET
    );

    let mut lines = Vec::with_capacity(todos.len() + 2);
    lines.push(header);

    // Sort by priority: in_progress first, then pending, then completed.
    let mut sorted: Vec<&runtime::Todo> = todos.iter().collect();
    sorted.sort_by_key(|t| match t.status {
        runtime::TodoStatus::InProgress => 0,
        runtime::TodoStatus::Pending => 1,
        runtime::TodoStatus::Completed => 2,
    });

    let display_count = sorted.len().min(max_display);
    let visible = &sorted[..display_count];
    let hidden = &sorted[display_count..];

    for todo in visible {
        // While in progress, show the present-continuous `activeForm`; otherwise
        // the imperative `content`.
        let label = if todo.status == runtime::TodoStatus::InProgress {
            todo.active_form.as_str()
        } else {
            todo.content.as_str()
        };
        let (icon, label_fmt) = match todo.status {
            runtime::TodoStatus::Completed => (
                format!("{success}\u{2713}{RESET}"),
                format!("{DIM}\x1b[9m{label}\x1b[29m{RESET}"),
            ),
            runtime::TodoStatus::InProgress => (
                format!("{info}\u{25a0}{RESET}"),
                format!("{BOLD}{label}{RESET}"),
            ),
            runtime::TodoStatus::Pending => ("\u{25a1}".to_string(), label.to_string()),
        };
        lines.push(format!("  {icon} {label_fmt}"));
    }

    if !hidden.is_empty() {
        let mut parts = Vec::new();
        let hi = hidden
            .iter()
            .filter(|t| t.status == runtime::TodoStatus::InProgress)
            .count();
        let hp = hidden
            .iter()
            .filter(|t| t.status == runtime::TodoStatus::Pending)
            .count();
        let hc = hidden
            .iter()
            .filter(|t| t.status == runtime::TodoStatus::Completed)
            .count();
        if hi > 0 {
            parts.push(format!("{hi} in progress"));
        }
        if hp > 0 {
            parts.push(format!("{hp} pending"));
        }
        if hc > 0 {
            parts.push(format!("{hc} completed"));
        }
        lines.push(format!("{DIM}  \u{2026} +{}{RESET}", parts.join(", ")));
    }

    lines.join("\n")
}

/// Context passed to `ReplApp` via `ContextProvider`.
struct ReplContext {
    output_rx: Arc<Mutex<Receiver<OutputMsg>>>,
    ui_rx: Arc<Mutex<Receiver<UiCommand>>>,
    input_tx: SyncSender<InputEvent>,
    spinner: SpinnerState,
    permission_mode: String,
    tips_line: String,
    stderr_redir: Arc<Mutex<Option<stderr_redirect::StderrRedirect>>>,
    /// Todo items for the ContextSlot. Updated by `UiCommand::UpdateContext`
    /// in the tick loop, read during the render phase. Uses `Arc<Mutex>`
    /// instead of a `use_state` hook to avoid shifting hook indices.
    context_todos: Arc<Mutex<Vec<runtime::Todo>>>,
    /// Running tool cards for the StagingSlot overlay, in insertion order.
    /// `ToolStarted` appends; `ToolFinished` removes by id. Same `Arc<Mutex>`
    /// rationale as `context_todos` — avoids shifting hook indices.
    staging_cards: Arc<Mutex<Vec<ToolCard>>>,
    /// Queued-input display texts for the queue overlay (between StagingSlot and
    /// StatusSlot), oldest first. Replaced wholesale by `UiCommand::SetQueue`.
    /// Same `Arc<Mutex>` rationale as `staging_cards`.
    queued_inputs: Arc<Mutex<Vec<String>>>,
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
    let stderr_redir = Arc::clone(&ctx.stderr_redir);
    let context_todos = Arc::clone(&ctx.context_todos);
    let context_todos_for_future = Arc::clone(&ctx.context_todos);
    let staging_cards = Arc::clone(&ctx.staging_cards);
    let staging_cards_for_future = Arc::clone(&ctx.staging_cards);
    let queued_inputs = Arc::clone(&ctx.queued_inputs);
    let queued_inputs_for_future = Arc::clone(&ctx.queued_inputs);
    drop(ctx);

    // use_terminal_size must be called before use_future and use_terminal_events
    // to maintain consistent hook ordering.
    let (term_width, term_height) = hooks.use_terminal_size();

    let (stdout, _stderr) = hooks.use_output();
    let mut system = hooks.use_context_mut::<SystemContext>();
    let mut input_value = hooks.use_state(String::new);
    let mut frame = hooks.use_state(|| 0usize);
    let mut spinner_text = hooks.use_state(String::new);
    let mut turn_result = hooks.use_state(|| None::<String>);
    let mut has_submitted = hooks.use_state(|| false);
    let mut input_slot = hooks.use_state(|| InputSlot::TextInput);
    let mut should_exit = hooks.use_state(|| false);
    let mut last_ctrlc = hooks.use_state(|| None::<Instant>);
    let mut history = hooks.use_state(Vec::<String>::new);
    let mut history_cursor = hooks.use_state(|| None::<usize>);
    let mut saved_input = hooks.use_state(String::new);
    let mut tab_candidates = hooks.use_state(Vec::<String>::new);
    let mut tab_index = hooks.use_state(|| 0usize);
    let mut footer_hint = hooks.use_state(|| None::<(String, Instant)>);
    let mut dialpad_cursor = hooks.use_state(|| 0usize);
    // When the user starts typing a free-form answer to a DialPad question
    // (only when the question set `allow_custom_input`), the active question is
    // captured here so TextInput's Enter routes the typed text back as the
    // answer instead of submitting it as a new prompt. Cleared on submit/cancel.
    let mut custom_answer_question = hooks.use_state(|| None::<QuestionPromptView>);
    // Ephemeral paste store: placeholder_id -> real pasted text.
    // Allocated when the user pastes, freed on submit/clear.
    // Never persisted — the real content goes into the submitted message.
    let mut paste_store = hooks.use_state(|| std::collections::HashMap::<u32, String>::new());
    let mut next_paste_id = hooks.use_state(|| 1u32);
    let mut text_input_handle = hooks.use_ref_default::<TextInputHandle>();
    // Where the cursor was when the user pressed the key, which is not where
    // `text_input_handle` reads during a key handler: `TextInput` is a child
    // component, and children drain their terminal events before this one
    // does, so by the time Up/Down is handled here the cursor has already
    // been moved a line. Deciding against the moved position collapses the
    // first tier away (an Up from the second line would land on the first
    // line and immediately jump to offset 0). Recorded once per render,
    // below, which is the state the next keypress starts from.
    let mut cursor_at_last_render = hooks.use_state(|| 0usize);
    // Whether a `TextInput` was mounted by the *previous* render. The
    // handle outlives the component it points at, so on the frame the slot
    // flips back from a question panel it still refers to the states of the
    // `TextInput` that was torn down — reading those panics inside iocraft.
    let mut text_input_was_mounted = hooks.use_state(|| false);

    // Clone handles for the future (StdoutHandle is Clone).
    let stdout_for_future = stdout.clone();
    let spinner_for_future = spinner.clone();
    let output_rx_for_future = Arc::clone(&output_rx);
    let ui_rx_for_future = Arc::clone(&ui_rx);
    let stderr_redir_for_future = Arc::clone(&stderr_redir);

    // 80ms tick loop: drain output/control channels, update spinner text.
    hooks.use_future(async move {
        let mut task_hide_deadline: Option<Instant> = None;
        loop {
            smol::Timer::after(Duration::from_millis(80)).await;

            // Drain stderr redirect — route captured text through stdout
            // so it appears in the iocraft scrollback instead of corrupting
            // the canvas.
            if let Ok(guard) = stderr_redir_for_future.lock() {
                if let Some(ref redir) = *guard {
                    if let Some(captured) = redir.drain() {
                        for line in captured.lines() {
                            stdout_for_future.println(line);
                        }
                    }
                }
            }

            // Drain output channel.
            if let Ok(rx) = output_rx_for_future.lock() {
                loop {
                    let (text, terminated) = match rx.try_recv() {
                        Ok(OutputMsg::Line(text)) => (text, true),
                        Ok(OutputMsg::Raw(text)) => (text, false),
                        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                    };
                    split_for_iocraft(&text, terminated, |op| match op {
                        OutputOp::Println(line) => stdout_for_future.println(line),
                        OutputOp::Print(chunk) => stdout_for_future.print(chunk),
                    });
                }
            }

            if let Ok(rx) = ui_rx_for_future.lock() {
                loop {
                    match rx.try_recv() {
                        Ok(UiCommand::ShowQuestion(question)) => {
                            let slot = if question.force_fuzzy_select
                                || question.options.len() > DIALPAD_MAX_OPTIONS
                            {
                                InputSlot::FuzzySelect(FuzzySelectState::new(question))
                            } else {
                                let initial = question
                                    .options
                                    .iter()
                                    .position(|o| o.recommended)
                                    .unwrap_or(0);
                                dialpad_cursor.set(initial);
                                InputSlot::DialPad(question)
                            };
                            input_slot.set(slot);
                            input_value.set(String::new());
                        }
                        Ok(UiCommand::ClearQuestion) => {
                            input_slot.set(InputSlot::TextInput);
                            input_value.set(String::new());
                        }
                        Ok(UiCommand::SetTurnResult(text)) => {
                            turn_result.set(Some(text));
                        }
                        Ok(UiCommand::ShowInputHint(text)) => {
                            input_slot.set(InputSlot::Hint(text));
                        }
                        Ok(UiCommand::UpdateContext(todos)) => {
                            if let Ok(mut items) = context_todos_for_future.lock() {
                                let has_incomplete = todos
                                    .iter()
                                    .any(|t| t.status != runtime::TodoStatus::Completed);
                                if todos.is_empty() {
                                    // Empty list → hide immediately
                                    items.clear();
                                    task_hide_deadline = None;
                                } else if has_incomplete {
                                    // Has open todos → show, cancel any hide timer
                                    *items = todos;
                                    task_hide_deadline = None;
                                } else if task_hide_deadline.is_none() {
                                    // All terminal → start 5s hide timer
                                    *items = todos;
                                    task_hide_deadline =
                                        Some(Instant::now() + Duration::from_secs(5));
                                } else {
                                    *items = todos;
                                }
                            }
                        }
                        Ok(UiCommand::ToolStarted { id, name, input }) => {
                            if let Ok(mut cards) = staging_cards_for_future.lock() {
                                cards.push(ToolCard { id, name, input });
                            }
                        }
                        Ok(UiCommand::ToolFinished { id }) => {
                            // Only clear the transient overlay entry. The
                            // finished card's permanent content is written to
                            // scrollback by the render engine on the ordered
                            // `output` channel — never from here — so the
                            // overlay stays a pure, order-free projection.
                            if let Ok(mut cards) = staging_cards_for_future.lock() {
                                cards.retain(|c| c.id != id);
                            }
                        }
                        Ok(UiCommand::SetQueue(items)) => {
                            // Pure projection of the coordinator's queue; the
                            // coordinator resends the full list on every change.
                            if let Ok(mut q) = queued_inputs_for_future.lock() {
                                *q = items;
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }

            // Auto-hide task panel 5s after all tasks reach terminal state.
            if let Some(deadline) = task_hide_deadline {
                if Instant::now() >= deadline {
                    if let Ok(mut items) = context_todos_for_future.lock() {
                        items.clear();
                    }
                    task_hide_deadline = None;
                }
            }

            // Auto-dismiss footer_hint after deadline.
            let hint_expired = footer_hint
                .read()
                .as_ref()
                .is_some_and(|(_, deadline)| Instant::now() >= *deadline);
            if hint_expired {
                footer_hint.set(None);
            }

            // Update spinner.  When a new turn starts (spinner becomes
            // active), clear the previous turn's result — the spinner
            // takes priority in the StatusSlot and the old result is stale.
            //
            // Only mutate state when it actually changes. `render_frame`
            // returns "" whenever the spinner is inactive (idle), so the old
            // unconditional `frame.set(idx+1)` + `spinner_text.set("")` churned
            // identical state on every 80ms idle tick. Per iocraft's render
            // model an unconditional `State::set` — even to the same value —
            // resolves `component.wait()` and can starve `term.wait()`, dropping
            // keyboard events (the failure mode guarded by
            // `iocraft_repl_keyboard_input_not_frozen`). Leaving idle ticks
            // untouched keeps key-event distribution alive between renders.
            let idx = frame.get();
            let text = spinner_for_future.render_frame(idx);
            if !text.is_empty() {
                frame.set(idx.wrapping_add(1));
                if turn_result.read().is_some() {
                    turn_result.set(None);
                }
            }
            if *spinner_text.read() != text {
                spinner_text.set(text);
            }
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
                let current_slot = input_slot.read().clone();
                // Dismiss InputSlot::Hint on any keypress.
                if matches!(current_slot, InputSlot::Hint(_)) {
                    input_slot.set(InputSlot::TextInput);
                    return;
                }
                match code {
                    // ── Enter ──────────────────────────────────────────
                    KeyCode::Enter if !modifiers.contains(KeyModifiers::SHIFT) => {
                        match &current_slot {
                            InputSlot::Hint(_) => {
                                input_slot.set(InputSlot::TextInput);
                            }
                            InputSlot::DialPad(question) => {
                                let selected = dialpad_cursor.get();
                                submit_dialpad_selection(question, selected, &input_tx_for_events);
                                if !*has_submitted.read() { has_submitted.set(true); }
                                input_value.set(String::new());
                                input_slot.set(InputSlot::TextInput);
                            }
                            InputSlot::FuzzySelect(_) => {
                                // Prefer the highlighted option. If nothing
                                // matches the filter but the question allows
                                // custom input, submit the raw filter text as a
                                // free-form answer (e.g. a model name not in the
                                // list) instead of silently doing nothing.
                                let answer = {
                                    let slot = input_slot.read();
                                    if let InputSlot::FuzzySelect(fs) = &*slot {
                                        fs.selected_value().or_else(|| {
                                            let custom = fs.filter.trim();
                                            if fs.question.allow_custom_input && !custom.is_empty() {
                                                Some(custom.to_string())
                                            } else {
                                                None
                                            }
                                        })
                                    } else {
                                        None
                                    }
                                };
                                if let Some(answer) = answer {
                                    if !*has_submitted.read() { has_submitted.set(true); }
                                    let _ = input_tx_for_events.send(InputEvent::QuestionAnswer(answer));
                                    input_value.set(String::new());
                                    input_slot.set(InputSlot::TextInput);
                                }
                            }
                            InputSlot::TextInput => {
                                let val = input_value.read().clone();
                                // If we're typing a free-form answer to a
                                // DialPad question (allow_custom_input), route
                                // the text back as the answer, not a new prompt.
                                if custom_answer_question.read().is_some() {
                                    let trimmed = val.trim();
                                    if !trimmed.is_empty() {
                                        if !*has_submitted.read() {
                                            has_submitted.set(true);
                                        }
                                        let _ = input_tx_for_events
                                            .send(InputEvent::QuestionAnswer(trimmed.to_string()));
                                        input_value.set(String::new());
                                        custom_answer_question.set(None);
                                    }
                                    return;
                                }
                                if let Some(event) = enter_key_event_for_value(None, 0, &val) {
                                    if !*has_submitted.read() { has_submitted.set(true); }
                                    match event {
                                        InputEvent::Exit => {
                                            should_exit.set(true);
                                            let _ = input_tx_for_events.send(InputEvent::Exit);
                                        }
                                        InputEvent::Submit { text, .. } => {
                                            let store_snap = paste_store.read().clone();
                                            let expanded = expand_paste_placeholders(&text, &store_snap);
                                            let display = expand_paste_for_display(&text, &store_snap);
                                            paste_store.write().clear();
                                            next_paste_id.set(1);
                                            let trimmed = expanded.trim();
                                            if !trimmed.is_empty() {
                                                let mut h = history.write();
                                                if h.last().map_or(true, |last| last != trimmed) {
                                                    h.push(trimmed.to_string());
                                                }
                                            }
                                            // The coordinator owns the `❯` echo to scrollback: it
                                            // alone knows whether this runs now (idle) or waits in
                                            // the queue overlay behind a running turn. Echoing here
                                            // would make a queued input look sent.
                                            let _ = input_tx_for_events.send(InputEvent::Submit { text: expanded, display });
                                        }
                                        InputEvent::QuestionAnswer(_) | InputEvent::Abort => {}
                                    }
                                    input_value.set(String::new());
                                    if history_cursor.get().is_some() {
                                        history_cursor.set(None);
                                        saved_input.set(String::new());
                                    }
                                }
                            }
                        }
                    }
                    // ── Up/Down — DialPad option selection ─────────────
                    KeyCode::Up if matches!(current_slot, InputSlot::DialPad(ref q) if !q.options.is_empty()) => {
                        let cur = dialpad_cursor.get();
                        dialpad_cursor.set(cur.saturating_sub(1));
                    }
                    KeyCode::Down if matches!(current_slot, InputSlot::DialPad(ref q) if !q.options.is_empty()) => {
                        let cur = dialpad_cursor.get();
                        let max = match &current_slot {
                            InputSlot::DialPad(q) => q.options.len().saturating_sub(1),
                            _ => 0,
                        };
                        dialpad_cursor.set((cur + 1).min(max));
                    }
                    // ── Up/Down — FuzzySelect cursor ───────────────────
                    KeyCode::Up if matches!(current_slot, InputSlot::FuzzySelect(_)) => {
                        let mut slot = input_slot.write();
                        if let InputSlot::FuzzySelect(ref mut fs) = *slot {
                            fs.move_up();
                        }
                    }
                    KeyCode::Down if matches!(current_slot, InputSlot::FuzzySelect(_)) => {
                        let mut slot = input_slot.write();
                        if let InputSlot::FuzzySelect(ref mut fs) = *slot {
                            fs.move_down();
                        }
                    }
                    // ── Left — submit back_value (tree navigation) ────
                    KeyCode::Left if matches!(current_slot, InputSlot::DialPad(_) | InputSlot::FuzzySelect(_)) => {
                        let back = match &current_slot {
                            InputSlot::DialPad(q) | InputSlot::FuzzySelect(FuzzySelectState { question: q, .. }) => {
                                q.back_value.clone()
                            }
                            _ => None,
                        };
                        if let Some(val) = back {
                            let _ = input_tx_for_events.send(InputEvent::QuestionAnswer(val));
                            input_slot.set(InputSlot::TextInput);
                            input_value.set(String::new());
                        }
                    }
                    // ── Right — drill into navigable option ───────────
                    KeyCode::Right if matches!(current_slot, InputSlot::DialPad(_)) => {
                        if let InputSlot::DialPad(ref q) = current_slot {
                            let selected = dialpad_cursor.get();
                            if q.options.get(selected).is_some_and(|o| o.is_navigable) {
                                submit_dialpad_selection(q, selected, &input_tx_for_events);
                                input_slot.set(InputSlot::TextInput);
                                input_value.set(String::new());
                            }
                        }
                    }
                    KeyCode::Right if matches!(current_slot, InputSlot::FuzzySelect(_)) => {
                        let should_submit = {
                            let slot = input_slot.read();
                            if let InputSlot::FuzzySelect(ref fs) = *slot {
                                fs.selected_option().is_some_and(|o| o.is_navigable)
                            } else {
                                false
                            }
                        };
                        if should_submit {
                            let slot = input_slot.read();
                            if let InputSlot::FuzzySelect(ref fs) = *slot {
                                if let Some(option) = fs.selected_option() {
                                    let _ = input_tx_for_events
                                        .send(InputEvent::QuestionAnswer(option.value.clone()));
                                }
                            }
                            drop(slot);
                            input_slot.set(InputSlot::TextInput);
                            input_value.set(String::new());
                        }
                    }
                    // ── Up/Down — TextInput: CC-parity two-step arrow keys ──
                    //
                    // Behavior mirrors rustyline's UpArrowHandler / DownArrowHandler
                    // (src/input.rs).  Three tiers per direction:
                    //
                    //   Up:  1) cursor NOT on first logical line  → pass-through
                    //            (TextInput moves to previous line)
                    //        2) cursor on first line, not at pos 0 → move to 0
                    //        3) cursor at pos 0 (or empty)         → history prev
                    //
                    //   Down: 1) cursor NOT on last logical line   → pass-through
                    //            (TextInput moves to next line)
                    //         2) cursor on last line, not at end   → move to end
                    //         3) cursor at end (or empty)          → history next
                    //
                    // "Logical line" = delimited by '\n'.  TextInput normalises
                    // platform newlines, so this is cross-platform safe.
                    KeyCode::Up if matches!(current_slot, InputSlot::TextInput) => {
                        if history_cursor.get().is_some() {
                            let h = history.read();
                            let c = history_cursor.get().unwrap_or(0);
                            let nc = c.saturating_sub(1);
                            if !h.is_empty() {
                                input_value.set(h[nc].clone());
                                history_cursor.set(Some(nc));
                            }
                        } else {
                            let val = input_value.read().clone();
                            let cursor_pos = cursor_at_last_render.get().min(val.len());
                            let on_first_line = !val.get(..cursor_pos)
                                .unwrap_or(&val)
                                .contains('\n');
                            if val.is_empty() || (on_first_line && cursor_pos == 0) {
                                let h = history.read();
                                if !h.is_empty() {
                                    saved_input.set(val);
                                    input_value.set(h[h.len() - 1].clone());
                                    history_cursor.set(Some(h.len() - 1));
                                }
                            } else if on_first_line {
                                text_input_handle.write().set_cursor_offset(0);
                            }
                            // else: not on first line — TextInput handles
                            // cursor movement to the line above.
                        }
                    }
                    KeyCode::Down if matches!(current_slot, InputSlot::TextInput) => {
                        if let Some(c) = history_cursor.get() {
                            let h = history.read();
                            if c + 1 < h.len() {
                                input_value.set(h[c + 1].clone());
                                history_cursor.set(Some(c + 1));
                            } else {
                                input_value.set(saved_input.read().clone());
                                history_cursor.set(None);
                            }
                        } else {
                            let val = input_value.read().clone();
                            let cursor_pos = cursor_at_last_render.get().min(val.len());
                            let on_last_line = !val.get(cursor_pos..)
                                .unwrap_or_default()
                                .contains('\n');
                            if on_last_line && cursor_pos < val.len() {
                                text_input_handle.write().set_cursor_offset(val.len());
                            }
                            // else if on_last_line && at end: nothing to do
                            // (no "forward history" in CC).
                            // else: not on last line — TextInput handles
                            // cursor movement to the line below.
                        }
                    }
                    // ── Digit shortcut — DialPad only ─────────────────
                    KeyCode::Char(ch)
                        if matches!(current_slot, InputSlot::DialPad(ref q) if q.options.len() >= 2)
                            && ch.is_ascii_digit()
                            && !modifiers.contains(KeyModifiers::CONTROL)
                            && !modifiers.contains(KeyModifiers::ALT) =>
                    {
                        let digit = ch.to_digit(10).unwrap_or(0) as usize;
                        let option_count = match &current_slot {
                            InputSlot::DialPad(q) => q.options.len(),
                            _ => 0,
                        };
                        if digit >= 1 && digit <= option_count {
                            dialpad_cursor.set(digit - 1);
                            if let InputSlot::DialPad(ref question) = current_slot {
                                submit_dialpad_selection(question, digit - 1, &input_tx_for_events);
                                if !*has_submitted.read() { has_submitted.set(true); }
                                input_value.set(String::new());
                                input_slot.set(InputSlot::TextInput);
                            }
                        }
                    }
                    // ── Typing a free-form answer — DialPad with allow_custom_input ──
                    // A printable character (that is not a digit quick-select)
                    // switches to TextInput seeded with that char; the active
                    // question is captured so Enter routes the text back as the
                    // answer. Only when the question opted in via
                    // `allow_custom_input`.
                    KeyCode::Char(ch)
                        if matches!(current_slot, InputSlot::DialPad(ref q) if q.allow_custom_input)
                            && !modifiers.contains(KeyModifiers::CONTROL)
                            && !modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if let InputSlot::DialPad(ref question) = current_slot {
                            custom_answer_question.set(Some(question.clone()));
                            input_slot.set(InputSlot::TextInput);
                            input_value.set(ch.to_string());
                        }
                    }
                    // ── Typing in FuzzySelect updates filter ──────────
                    KeyCode::Char(ch)
                        if matches!(current_slot, InputSlot::FuzzySelect(_))
                            && !modifiers.contains(KeyModifiers::CONTROL)
                            && !modifiers.contains(KeyModifiers::ALT) =>
                    {
                        let mut slot = input_slot.write();
                        if let InputSlot::FuzzySelect(ref mut fs) = *slot {
                            fs.filter.push(ch);
                            fs.apply_filter();
                        }
                    }
                    KeyCode::Backspace if matches!(current_slot, InputSlot::FuzzySelect(_)) => {
                        let mut slot = input_slot.write();
                        if let InputSlot::FuzzySelect(ref mut fs) = *slot {
                            fs.filter.pop();
                            fs.apply_filter();
                        }
                    }
                    // ── Global keys ───────────────────────────────────
                    KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = input_tx_for_events.send(InputEvent::Exit);
                        should_exit.set(true);
                    }
                    KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                        input_value.set(String::new());
                        history_cursor.set(None);
                    }
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
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
                            let hint_msg = format!("{}Press Ctrl-C again to exit{}", crate::render::DIM, crate::render::RESET);
                            footer_hint.set(Some((hint_msg, Instant::now() + ctrlc_hint_ttl())));
                            input_value.set(String::new());
                        }
                    }
                    KeyCode::Esc => {
                        // In FuzzySelect/DialPad/Hint, ESC cancels.
                        // Also drop any in-progress custom answer.
                        custom_answer_question.set(None);
                        if !matches!(current_slot, InputSlot::TextInput | InputSlot::Hint(_)) {
                            input_slot.set(InputSlot::TextInput);
                            input_value.set(String::new());
                            let _ = input_tx_for_events.send(InputEvent::Abort);
                        } else {
                            let _ = input_tx_for_events.send(InputEvent::Abort);
                            input_value.set(String::new());
                        }
                    }
                    KeyCode::Tab
                        if matches!(current_slot, InputSlot::TextInput)
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
                        if code != KeyCode::Tab && !tab_candidates.read().is_empty() {
                            tab_candidates.set(Vec::new());
                        }
                    }
                }
            }
            _ => {}
        }
    });

    // Snapshot the cursor for the next keypress to decide against. Guarded:
    // an unconditional `State::set` — even to the same value — resolves
    // `component.wait()` and can starve `term.wait()`, dropping keystrokes
    // (the failure mode `iocraft_repl_keyboard_input_not_frozen` guards).
    // Only while the text input is the live slot: in the question slots no
    // `TextInput` is mounted, so the handle still reports the offset it had
    // when one last was, and snapshotting that would set state on renders it
    // has no business touching.
    // Snapshot the cursor for the next keypress to decide against. Guarded
    // twice: only while a `TextInput` is the live slot *and* one was already
    // mounted a frame ago (see `text_input_was_mounted`), and only set when
    // the value actually changed — an unconditional `State::set` resolves
    // `component.wait()` and can starve `term.wait()`, dropping keystrokes
    // (the failure mode `iocraft_repl_keyboard_input_not_frozen` guards).
    let text_input_is_live = matches!(*input_slot.read(), InputSlot::TextInput);
    if text_input_is_live && text_input_was_mounted.get() {
        let live_cursor = text_input_handle.read().cursor_offset();
        if cursor_at_last_render.get() != live_cursor {
            cursor_at_last_render.set(live_cursor);
        }
    }
    if text_input_was_mounted.get() != text_input_is_live {
        text_input_was_mounted.set(text_input_is_live);
    }

    // Exit check: `system` was obtained before the event handler and is
    // NOT captured by the Send closure. The exit flag is set inside the
    // closure; we check it here outside the closure.
    if *should_exit.read() {
        system.exit();
    }

    // ── Derive ChromeSlots from state ──────────────────────────────────
    let st = spinner_text.read().clone();
    let val = input_value.read().clone();
    let current_input_slot = input_slot.read().clone();
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

    // FooterSlot: Hint > TurnActive > Minimal > Full
    let footer_slot = if let Some((ref msg, _)) = *footer_hint.read() {
        FooterSlot::Hint(msg.clone())
    } else if matches!(status_slot, StatusSlot::Spinner(_)) {
        FooterSlot::TurnActive
    } else if submitted {
        FooterSlot::Minimal
    } else {
        FooterSlot::Full
    };

    // InputSlot rendering
    let (panel_text, prompt_label) = match &current_input_slot {
        InputSlot::Hint(_) | InputSlot::TextInput => (None, crate::render::PROMPT_PREFIX),
        InputSlot::DialPad(q) => (
            Some(format_question_panel(q, dialpad_cursor.get())),
            "\u{2753} ",
        ),
        InputSlot::FuzzySelect(fs) => (Some(fs.format_panel()), "\u{1f50d} "),
    };

    let perm = permission_mode.clone();
    let w = term_width as usize;
    let sep = "\u{2500}".repeat(w);
    let footer_text = format_footer_text(&footer_slot, &perm);

    // Merge ContextSlot into the upper separator as a single
    // multi-line Text element so the element tree structure stays
    // identical (avoids iocraft hook-index shifts).
    let todo_line = context_todos
        .lock()
        .ok()
        .map(|items| render_todo_panel(&items, term_height as usize))
        .unwrap_or_default();
    let upper_sep = if todo_line.is_empty() {
        sep.clone()
    } else {
        format!("{todo_line}\n{sep}")
    };

    // StagingSlot: running tool cards (yellow), rendered via the same SSOT as
    // completed cards (`format_tool_call_start` == the Running L-frame). Built
    // as one multi-line string so the element tree keeps a fixed shape (empty
    // string when no cards) — same hook-index rationale as the task panel. This
    // is a pure overlay: it shows only in-flight calls and never commits to
    // scrollback (the render engine does that on the ordered output channel).
    let staging_text = staging_cards
        .lock()
        .ok()
        .map(|cards| render_staging_overlay(&cards, term_height as usize))
        .unwrap_or_default();

    // QueueSlot: user inputs queued behind the running turn (DIM), between the
    // StagingSlot and StatusSlot. Same pure-overlay / fixed-element-shape
    // rationale as the staging overlay — a queued input lives here (never in
    // scrollback) until the coordinator flushes it at the turn boundary.
    let queue_text = queued_inputs
        .lock()
        .ok()
        .map(|items| render_queue_overlay(&items, term_height as usize))
        .unwrap_or_default();

    element! {
        View(flex_direction: FlexDirection::Column) {
            // StagingSlot: in-flight tool cards (yellow). Empty string renders
            // nothing; the element is always present to keep hook order.
            Text(content: staging_text)
            // QueueSlot: inputs queued behind the running turn (DIM). Empty
            // string renders nothing; element always present for hook order.
            Text(content: queue_text)
            // StatusSlot
            #(match &status_slot {
                StatusSlot::Spinner(s) => Some(element! { Text(content: s.clone()) }),
                StatusSlot::TurnResult(s) => Some(element! { Text(content: s.clone(), color: Color::DarkGrey) }),
                StatusSlot::Tips => Some(element! { Text(content: tips_text.clone(), color: Color::DarkGrey) }),
                StatusSlot::Empty => None,
            })
            // Upper chrome: separator (+ ContextSlot + separator when tasks exist)
            Text(content: upper_sep, color: Color::DarkGrey)
            // InputSlot
            #(panel_text.map(|panel| element! {
                Text(content: panel, color: Color::Cyan)
            }))
            #(if let InputSlot::Hint(ref hint_text) = current_input_slot {
                element! {
                    View(flex_direction: FlexDirection::Row) {
                        Text(content: hint_text.clone(), color: Color::DarkGrey)
                    }
                }
            } else if matches!(current_input_slot, InputSlot::DialPad(_)) {
                // DialPad: no input row — all interaction is via
                // arrow keys and digit shortcuts shown in the panel.
                element! {
                    View(flex_direction: FlexDirection::Row) {}
                }
            } else if matches!(current_input_slot, InputSlot::FuzzySelect(_)) {
                // FuzzySelect owns keyboard input — render filter as
                // static text so TextInput's on_change doesn't compete.
                let filter_display = match &current_input_slot {
                    InputSlot::FuzzySelect(fs) if !fs.filter.is_empty() => fs.filter.clone(),
                    _ => String::new(),
                };
                element! {
                    View(flex_direction: FlexDirection::Row) {
                        Text(content: prompt_label)
                        Text(content: filter_display)
                    }
                }
            } else {
                element! {
                    View(flex_direction: FlexDirection::Row) {
                        Text(content: prompt_label)
                        TextInput(
                            value: val,
                            has_focus: true,
                            multiline: true,
                            auto_grow: true,
                            handle: Some(text_input_handle.clone()),
                            on_change: move |new_val: String| {
                                input_value.set(new_val);
                            },
                            on_paste: move |pasted: String| {
                                // Normalize CR / CRLF line endings first so
                                // line counting, the placeholder, and the
                                // expanded text are all consistent (Windows
                                // Terminal pastes carry \r or \r\n).
                                let pasted = normalize_paste_newlines(&pasted);
                                let current = input_value.read().clone();
                                // Match Claude Code: only long or multi-line
                                // pastes collapse into a compact placeholder;
                                // short single-/double-line pastes insert
                                // literally so the box shows what you pasted.
                                if !should_use_paste_placeholder(&pasted) {
                                    input_value.set(format!("{current}{pasted}"));
                                    return;
                                }
                                // Store the real text and replace with a
                                // compact placeholder so the input box does
                                // not overflow with potentially huge content.
                                let id = next_paste_id.get();
                                next_paste_id.set(id + 1);
                                let placeholder = format_paste_placeholder(id, &pasted);
                                paste_store.write().insert(id, pasted);
                                // Append the placeholder to whatever is already in the box.
                                input_value.set(if current.is_empty() {
                                    placeholder
                                } else {
                                    format!("{current}{placeholder}")
                                });
                            },
                        )
                    }
                }
            })
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

    let stderr_redir: Arc<Mutex<Option<stderr_redirect::StderrRedirect>>> =
        Arc::new(Mutex::new(None));

    let ctx = ReplContext {
        output_rx: Arc::new(Mutex::new(output_rx)),
        ui_rx: Arc::new(Mutex::new(ui_rx)),
        input_tx: input_tx.clone(),
        spinner: spinner.clone(),
        permission_mode: permission_mode.to_string(),
        tips_line: "Type /help for commands \u{00b7} /status for live context \u{00b7} /resume latest jumps back to the newest session \u{00b7} /diff then /commit to ship \u{00b7} Tab for /command completions".to_string(),
        stderr_redir: Arc::clone(&stderr_redir),
        context_todos: Arc::new(Mutex::new(tools::global_todo_list())),
        staging_cards: Arc::new(Mutex::new(Vec::new())),
        queued_inputs: Arc::new(Mutex::new(Vec::new())),
    };

    let banner = startup_banner.to_string();
    let join_handle = std::thread::Builder::new()
        .name("repl-ui".into())
        .spawn(move || {
            // Print banner before entering the render loop so the user sees
            // session info in the scrollback. iocraft's render_loop enters
            // raw mode immediately, so we print first.
            println!("{banner}");

            // Activate stderr redirect before entering raw mode so that
            // any library writing to fd 2 is captured and routed through
            // the iocraft stdout handle by the tick loop.
            if let Ok(mut guard) = stderr_redir.lock() {
                *guard = stderr_redirect::StderrRedirect::activate();
            }

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

    fn tool_card(id: &str, name: &str) -> ToolCard {
        ToolCard {
            id: id.to_string(),
            name: name.to_string(),
            input: "{}".to_string(),
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for n in chars.by_ref() {
                        if n.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn staging_overlay_empty_when_no_cards() {
        assert_eq!(render_staging_overlay(&[], 40), "");
    }

    #[test]
    fn staging_overlay_hidden_on_short_terminal() {
        // ≤10 rows: hide entirely rather than crowd out the prompt.
        assert_eq!(render_staging_overlay(&[tool_card("1", "bash")], 8), "");
    }

    #[test]
    fn staging_overlay_renders_one_card_per_running_call() {
        let cards = vec![tool_card("1", "bash"), tool_card("2", "read_file")];
        let plain = strip_ansi(&render_staging_overlay(&cards, 40));
        // Each running call is a Running L-frame card (╭─ header … ╰─).
        assert_eq!(plain.matches("╭─").count(), 2, "{plain}");
        // Headers show the canonical tool label (`Bash`, `Read`) — the SAME
        // label the completed scrollback card resolves, so a tool never
        // changes case when it finishes.
        assert!(plain.contains("Bash"), "{plain}");
        assert!(plain.contains("Read"), "{plain}");
    }

    #[test]
    fn staging_overlay_collapses_overflow_beyond_height_budget() {
        // rows=24 → budget min(10,max(3,10)) = 10 lines. Each card is 3 lines
        // (╭─ / │ / ╰─ — bash with a "{}" input has a $ body line), so ~3 cards
        // fit and the rest collapse into a "+N more running" line.
        let cards: Vec<ToolCard> = (0..8).map(|i| tool_card(&i.to_string(), "bash")).collect();
        let plain = strip_ansi(&render_staging_overlay(&cards, 24));
        assert!(
            plain.contains("more running"),
            "expected overflow summary: {plain}"
        );
        let shown = plain.matches("╭─").count();
        assert!(shown >= 1 && shown < 8, "shown={shown}: {plain}");
    }

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
                    is_navigable: false,
                },
                QuestionOptionView {
                    label: "User".to_string(),
                    value: "user".to_string(),
                    description: None,
                    recommended: false,
                    is_navigable: false,
                },
            ],
            allow_custom_input: false,
            custom_input_hint: None,
            force_fuzzy_select: false,
            back_value: None,
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
            force_fuzzy_select: false,
            back_value: None,
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
            Some(InputEvent::Submit {
                text: "hello".to_string(),
                display: "hello".to_string(),
            })
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
        assert!(panel.contains(" [1] Project recommended"));
        assert!(panel.contains("> [2] User"));
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
            Some(InputEvent::Submit {
                text: "/status".to_string(),
                display: "/status".to_string(),
            })
        );
        assert_eq!(
            enter_key_event_for_value(None, 0, "/cost"),
            Some(InputEvent::Submit {
                text: "/cost".to_string(),
                display: "/cost".to_string(),
            })
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

    #[test]
    fn paste_placeholder_single_line_has_no_line_count() {
        let p = format_paste_placeholder(1, "hello world");
        assert_eq!(p, "[Pasted text #1]");
    }

    #[test]
    fn paste_placeholder_multi_line_includes_line_count() {
        let p = format_paste_placeholder(1, "line one\nline two\nline three");
        assert_eq!(p, "[Pasted text #1 +2 lines]");
    }

    #[test]
    fn expand_paste_placeholders_restores_real_text() {
        let mut store = std::collections::HashMap::new();
        store.insert(1u32, "line one\nline two".to_string());
        let input = "before [Pasted text #1 +1 lines] after";
        let expanded = expand_paste_placeholders(input, &store);
        assert_eq!(expanded, "before line one\nline two after");
    }

    #[test]
    fn expand_paste_placeholders_noop_when_no_store() {
        let store = std::collections::HashMap::new();
        let input = "no placeholders here";
        assert_eq!(expand_paste_placeholders(input, &store), input);
    }

    #[test]
    fn expand_paste_placeholders_single_line_variant() {
        let mut store = std::collections::HashMap::new();
        store.insert(3u32, "hello".to_string());
        let input = "[Pasted text #3]";
        assert_eq!(expand_paste_placeholders(input, &store), "hello");
    }

    #[test]
    fn expand_paste_for_display_wraps_pasted_regions_in_dim() {
        let mut store = std::collections::HashMap::new();
        store.insert(1u32, "pasted stuff".to_string());
        let input = "typed [Pasted text #1] more";
        let display = expand_paste_for_display(input, &store);
        assert_eq!(
            display,
            format!(
                "typed {}pasted stuff{} more",
                crate::render::DIM,
                crate::render::RESET
            ),
        );
    }

    #[test]
    fn expand_paste_for_display_noop_without_placeholders() {
        let store = std::collections::HashMap::new();
        let input = "plain text";
        assert_eq!(expand_paste_for_display(input, &store), input);
    }
}
