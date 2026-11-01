//! iocraft-based terminal UI for the REPL.
//!
//! **`ReplApp` / `spawn_repl_ui`** — full iocraft REPL component that
//! replaces rustyline for interactive input.  Shows a persistent prompt
//! with spinner overlay.  The coordinator thread reads `InputEvent`s from
//! the returned `ReplHandle` and dispatches turns.
//!
//! # ChromeSlot pattern
//!
//! The REPL chrome uses named slots with explicit content ownership.
//! `StatusSlot` and `InputSlot` select their current view; the footer combines
//! permission, task summary and one prioritized tip;
//! `PendingSlot` and `TodoSlot` render their current lists and hide when empty:
//!
//! ```text
//! [PendingSlot]   ← in-flight tools and queued messages
//! [StatusSlot]    ← foreground | background | turn_result | empty
//! [TodoSlot]      ← todo summary and items | empty
//! ──── separator ────
//! [InputSlot]     ← text_input | question_panel
//! ──── separator ────
//! [FooterSlot]    ← permission · tasks · one prioritized tip
//! ```

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::render::activity::{ActivityState, ToolActivity};
use commands::suggest_slash_commands;
use engine_events::EngineState;
use iocraft::prelude::*;

mod ansi_text;
mod choice_text;
mod footer;
mod input_history;
mod layout;
mod prompt_text;
mod queued_message;
mod tasks;
mod transcript;
use crate::render::output::{split_lines, OutputOp, Utf8Stream};
use crate::render::styled_text::StyledText;
use ansi_text::{row_text, AnsiText, RichText};
use choice_text::ChoiceText;
pub(crate) use footer::StatusSource;
use footer::{FooterHints, FooterTips};
use layout::{ansi, ansi_with_style, ChromeLayout, InputLayout, Measurements, Slot};
pub(crate) mod prose_preview;
use prose_preview::{PreviewUpdate, ProsePreview};
use tasks::{TaskBrowser, TaskFocus};

use crate::stderr_capture as stderr_redirect;

// ── ChromeSlot enums ──────────────────────────────────────────────────

/// ChromeSlot: status area above the separator. One variant renders at a time.
///
/// Priority: Foreground > Background > TurnResult > Empty.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StatusSlot {
    /// Running model, tool, or retry activity; input panels own waiting states.
    Active(String),
    /// Task conclusion; shares the footer's event-fed task projection.
    Background {
        full: Arc<StyledText>,
        summary: Arc<StyledText>,
    },
    /// Turn result (tokens/cost/ctx). Cleared when the next turn starts.
    TurnResult(Arc<StyledText>),
    /// Nothing — slot not rendered, saves vertical space.
    Empty,
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
    pub presentation: runtime::QuestionPresentation,
    pub title: Option<String>,
    pub description: Option<runtime::PromptText>,
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
        let theme = crate::render::theme();
        let support = crate::render::ColorSupport::detect();
        let caption = ChoiceText::new(theme, support, false);
        let total = self.filtered.len();
        if self.filter.is_empty() {
            lines.push(format!(
                "{}{}",
                caption.muted(&format!("{total} items  ")),
                self.question.prompt
            ));
        } else {
            lines.push(format!(
                "{}{}{}{}{}",
                caption.muted(&format!("{total} match  ")),
                self.question.prompt,
                caption.muted(" (filter: "),
                self.filter,
                caption.muted(")"),
            ));
        }
        let end = (self.scroll + FUZZY_WINDOW_SIZE).min(self.filtered.len());
        for (vi, &oi) in self.filtered[self.scroll..end].iter().enumerate() {
            let abs_vi = self.scroll + vi;
            let option = &self.question.options[oi];
            let selector = if abs_vi == self.cursor { ">" } else { " " };
            let choice = ChoiceText::new(theme, support, abs_vi == self.cursor);
            let marker = if option.recommended {
                choice.muted(" (recommended)")
            } else {
                String::new()
            };
            lines.push(format!(
                "{}{}{marker}",
                choice.prefix(&format!("{selector} ")),
                choice.body(&option.label)
            ));
        }
        if end < self.filtered.len() {
            lines.push(caption.muted(&format!("  … {} more", self.filtered.len() - end)));
        }
        if self.question.allow_custom_input && !self.filter.is_empty() && self.filtered.is_empty() {
            let hint = self
                .question
                .custom_input_hint
                .as_deref()
                .filter(|hint| !hint.is_empty())
                .unwrap_or("no match — press Enter to use what you typed");
            lines.push(caption.muted(&format!("  {hint}")));
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
    name: String,
    input: String,
    status: crate::cli::format::ToolStatus,
    content: Arc<crate::cli::format::ToolCardContent>,
}

impl ToolCard {
    fn new(id: String, name: &str, input: &str) -> Self {
        Self {
            id,
            name: crate::cli::format::ToolKind::visible_name(name).to_string(),
            input: input.to_string(),
            status: crate::cli::format::ToolStatus::Queued,
            // Parsing and syntax highlighting happen once per tool event,
            // never on spinner ticks or keystrokes. Layout still adapts on resize.
            content: Arc::new(crate::cli::format::tool_call_start_content(name, input)),
        }
    }
}

#[derive(Clone, Debug)]
pub enum UiCommand {
    BackgroundTask(engine_events::BackgroundTaskEvent),
    BackgroundTaskError(String),
    OpenTasks,
    ToolQueued {
        id: String,
        name: String,
        input: String,
    },
    ShowQuestion(QuestionPromptView),
    ClearQuestion,
    SetTurnResult {
        text: Arc<StyledText>,
        turn: u32,
    },
    SetTip(engine_core::Tip),
    SetCacheTip(CacheTip),
    /// A persistent tip owned by `source`; `None` clears it.
    SetStatusTip(StatusSource, Option<engine_core::Tip>),
    /// Where the workspace is, `repo ⎇ branch`, for the footer.
    SetWorkspacePosition(Option<String>),
    BeginTurn,
    /// Update the `TodoSlot` panel with the current todo list.
    UpdateTodos(Vec<runtime::Todo>),
    /// A tool call started — append a running (yellow) card to the pending
    /// overlay, in arrival order relative to queued messages.
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
    /// Upsert by identity: queued → steering preserves its place among tools.
    InputUpsert {
        id: u64,
        display: String,
        is_human: bool,
        delivery: engine_events::InputDelivery,
    },
    InputRemove(u64),
    PendingInputsClear,
}

#[derive(Clone)]
pub struct UiCommandSender {
    tx: SyncSender<UiCommand>,
}

impl UiCommandSender {
    pub fn open_tasks(&self) {
        let _ = self.tx.send(UiCommand::OpenTasks);
    }

    pub fn background_task(&self, event: engine_events::BackgroundTaskEvent) {
        let _ = self.tx.send(UiCommand::BackgroundTask(event));
    }
    pub fn background_task_error(&self, message: String) {
        let _ = self.tx.send(UiCommand::BackgroundTaskError(message));
    }
    pub fn tool_queued(&self, id: &str, name: &str, input: &str) {
        let _ = self.tx.send(UiCommand::ToolQueued {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
        });
    }
    pub fn show_question(&self, question: QuestionPromptView) {
        let _ = self.tx.send(UiCommand::ShowQuestion(question));
    }

    pub fn clear_question(&self) {
        let _ = self.tx.send(UiCommand::ClearQuestion);
    }

    pub(crate) fn set_turn_result(&self, text: &crate::render::StyledLine, turn: u32) {
        let _ = self.tx.send(UiCommand::SetTurnResult {
            text: Arc::new(text.structured()),
            turn,
        });
    }

    pub fn show_input_hint(&self, text: &str) {
        self.set_tip(engine_core::Tip {
            priority: engine_core::TipPriority::Normal,
            message: text.to_string(),
        });
    }

    pub(crate) fn set_tip(&self, tip: engine_core::Tip) {
        let _ = self.tx.send(UiCommand::SetTip(tip));
    }

    pub(crate) fn begin_turn(&self) {
        let _ = self.tx.send(UiCommand::BeginTurn);
    }

    pub(crate) fn set_cache_tip(&self, tip: CacheTip) {
        let _ = self.tx.send(UiCommand::SetCacheTip(tip));
    }

    pub(crate) fn set_status_tip(&self, source: StatusSource, tip: Option<engine_core::Tip>) {
        let _ = self.tx.send(UiCommand::SetStatusTip(source, tip));
    }

    pub(crate) fn set_workspace_position(&self, label: Option<String>) {
        let _ = self.tx.send(UiCommand::SetWorkspacePosition(label));
    }

    pub fn update_todos(&self, todos: Vec<runtime::Todo>) {
        let _ = self.tx.send(UiCommand::UpdateTodos(todos));
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

    pub fn pending_input(
        &self,
        input: engine_events::InputView,
        delivery: engine_events::InputDelivery,
    ) {
        let display = queued_message::preview(input.display, input.source);
        let _ = self.tx.send(UiCommand::InputUpsert {
            id: input.id,
            display,
            is_human: input.source == engine_events::InputSource::Human,
            delivery,
        });
    }

    pub fn steering_push(&self, input: &engine_events::TurnInput) {
        self.pending_input(input.view(), engine_events::InputDelivery::Steer);
    }

    pub fn pending_input_remove(&self, id: u64) {
        let _ = self.tx.send(UiCommand::InputRemove(id));
    }

    pub fn pending_inputs_clear(&self) {
        let _ = self.tx.send(UiCommand::PendingInputsClear);
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
        || newline_count >= PASTE_PLACEHOLDER_MAX_LINES
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

/// Apply a paste to a text buffer, shared by the `TextInput.on_paste` handler
/// and the DialPad custom-input row so bracketed / multi-line paste behaves
/// identically in both. Normalizes line endings, then either inserts the paste
/// literally (short single-/double-line) or registers it in `store` under a
/// fresh id and appends a compact `[Pasted text #N +M lines]` placeholder (long
/// or multi-line) that `expand_paste_placeholders` restores at submit time.
/// Returns the new buffer contents.
fn apply_paste_to_buffer(
    current: &str,
    pasted: &str,
    next_id: &mut u32,
    store: &mut std::collections::HashMap<u32, String>,
) -> String {
    let pasted = normalize_paste_newlines(pasted);
    if !should_use_paste_placeholder(&pasted) {
        return format!("{current}{pasted}");
    }
    let id = *next_id;
    *next_id += 1;
    let placeholder = format_paste_placeholder(id, &pasted);
    store.insert(id, pasted);
    if current.is_empty() {
        placeholder
    } else {
        format!("{current}{placeholder}")
    }
}

/// Placeholders a `paste_store` entry can stand behind: a collapsed text paste
/// (`[Pasted text #N ...]`) and a pasted image (`[Image #N]`, whose entry is the
/// image's `@` reference).
const PASTE_PLACEHOLDER_PREFIXES: [&str; 2] = ["[Pasted text #", "[Image #"];

/// The earliest placeholder prefix in `text` and its offset.
fn next_paste_placeholder(text: &str) -> Option<(usize, &'static str)> {
    PASTE_PLACEHOLDER_PREFIXES
        .iter()
        .filter_map(|prefix| text.find(prefix).map(|at| (at, *prefix)))
        .min_by_key(|(at, _)| *at)
}

/// Insert an `[Image #N]` placeholder for the pasted image at `cursor`. It
/// expands to the image's `@` reference on submit, through the same store and
/// expansion as a collapsed text paste.
fn insert_image_placeholder(
    current: &str,
    cursor: usize,
    path: &std::path::Path,
    next_id: &mut u32,
    store: &mut std::collections::HashMap<u32, String>,
) -> TextInputEdit {
    let id = *next_id;
    *next_id += 1;
    store.insert(id, crate::clipboard_image::reference(path));
    let before = &current[..cursor];
    let insertion = format!(
        "{}[Image #{id}] ",
        crate::clipboard_image::separator_before(before)
    );
    TextInputEdit {
        value: format!("{before}{insertion}{}", &current[cursor..]),
        cursor_offset: cursor + insertion.len(),
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
    if store.is_empty() || next_paste_placeholder(input).is_none() {
        return input.to_string();
    }
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    while let Some((rel_start, placeholder_prefix)) = next_paste_placeholder(rest) {
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

        if let Some((consumed_after_prefix, real_text)) = matched {
            if let Some((before, after)) = wrap {
                result.push_str(before);
                result.push_str(real_text);
                result.push_str(after);
            } else {
                result.push_str(real_text);
            }
            rest = &after_prefix[consumed_after_prefix..];
        } else {
            result.push_str(placeholder_prefix);
            rest = after_prefix;
        }
    }
    result.push_str(rest);
    result
}

/// Replace all paste placeholders with their real text (plain).
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
        // Strip trailing newline(s): the multiline TextInput inserts a `\n`
        // for the Enter keypress that submits, and that keystroke can land in
        // the value before this handler reads it — leaving a spurious trailing
        // newline on the submitted/queued text. Internal newlines (a real
        // multi-line message) are preserved; only the submit-newline tail is
        // removed. SSOT: fixing it here cleans the idle-turn prompt, the queued
        // text, and therefore what ↑ recalls, all at once.
        let text = value.trim_end_matches('\n').to_string();
        Some(InputEvent::Submit {
            text: text.clone(),
            display: text,
        })
    }
}

fn format_question_panel(
    question: &QuestionPromptView,
    selected_index: usize,
    custom_input: &str,
) -> String {
    let mut lines = Vec::new();
    let theme = crate::render::theme();
    let support = crate::render::ColorSupport::detect();
    let caption = ChoiceText::new(theme, support, false);
    lines.push(format!(
        "{}{}",
        caption.muted(&format!(
            "{}/{}  ",
            question.index + 1,
            question.total.max(1)
        )),
        question.prompt
    ));
    for (index, option) in question.options.iter().enumerate() {
        let selector = if index == selected_index { ">" } else { " " };
        let choice = ChoiceText::new(theme, support, index == selected_index);
        let marker = if option.recommended {
            " (recommended)"
        } else {
            ""
        };
        let description = option
            .description
            .as_deref()
            .filter(|description| !description.is_empty())
            .map_or(String::new(), |description| format!(" - {description}"));
        lines.push(format!(
            "{}{}{}",
            choice.prefix(&format!("{selector} [{}] ", index + 1)),
            choice.body(&option.label),
            choice.muted(&format!("{marker}{description}")),
        ));
    }
    let max_digit = question.options.len().min(9);
    // Built-in custom-input row: every DialPad has it. When the question opted
    // into free text (`allow_custom_input`), it accepts typing in-place; the row
    // shows the typed buffer (with a caret when selected) or the hint. When the
    // question did not opt in, the row is omitted.
    if question.allow_custom_input {
        let on_row = selected_index == question.options.len();
        let choice = ChoiceText::new(theme, support, on_row);
        let selector = if on_row { ">" } else { " " };
        let body = if custom_input.is_empty() {
            let hint = question
                .custom_input_hint
                .as_deref()
                .filter(|hint| !hint.is_empty())
                .unwrap_or("type your own answer");
            if on_row {
                // Show a caret so it reads as an active field once selected.
                format!("{}{}", choice.body("\u{2588} "), choice.muted(hint))
            } else {
                choice.muted(hint)
            }
        } else if on_row {
            choice.body(&format!("{custom_input}\u{2588}"))
        } else {
            choice.body(custom_input)
        };
        lines.push(format!(
            "{}{body}",
            choice.prefix(&format!("{selector} [+] "))
        ));
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
    let digit_hint = if max_digit >= 2 {
        format!(" \u{00b7} 1-{max_digit} quick select")
    } else {
        String::new()
    };
    lines.push(caption.muted(&format!(
        "  {arrow_hint}\u{2191}\u{2193} navigate{digit_hint}{custom_hint} \u{00b7} Enter confirm",
    )));
    lines.join("\n")
}

/// Message type for the output channel: distinguishes complete lines
/// (which need a trailing newline) from raw byte chunks (which already
/// include their own newlines from the markdown renderer).
enum OutputMsg {
    /// Semantic user boundary; headers never infer prompts from assistant text.
    UserPrompt(String),
    /// One event's stable history and replacement live prose, in order.
    AssistantUpdate {
        committed: String,
        preview: PreviewUpdate,
    },
    /// Complete line — `StdoutHandle::println` will append `\n`.
    Line(String),
    /// Raw chunk — `StdoutHandle::print`, no extra newline.
    Raw(String),
    /// Replace the transient tool summary without committing it to history.
    ToolProgress(Option<String>),
    /// A barrier: flush preceding output before releasing terminal ownership.
    Suspend(TerminalHandoff),
}

struct TerminalHandoff {
    ready: SyncSender<io::Result<()>>,
    resume: Receiver<()>,
    done: SyncSender<io::Result<()>>,
}

/// Restores the render loop even when the external operation errors or unwinds.
pub struct TerminalSuspension {
    resume: Option<SyncSender<()>>,
    done: Receiver<io::Result<()>>,
}

impl TerminalSuspension {
    pub fn finish(&mut self) -> io::Result<()> {
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
            return self
                .done
                .recv()
                .map_err(|_| io::Error::other("terminal render loop stopped"))?;
        }
        Ok(())
    }
}

impl Drop for TerminalSuspension {
    fn drop(&mut self) {
        let _ = self.finish();
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
    pub(crate) fn user_prompt(&self, display: &str) {
        let _ = self.tx.send(OutputMsg::UserPrompt(display.to_string()));
    }
    pub(crate) fn tool_progress(&self, text: Option<String>) {
        let _ = self.tx.send(OutputMsg::ToolProgress(text));
    }

    pub(crate) fn assistant_update(&self, committed: String, preview: PreviewUpdate) {
        let _ = self
            .tx
            .send(OutputMsg::AssistantUpdate { committed, preview });
    }

    pub fn println(&self, text: &str) {
        let _ = self.tx.send(OutputMsg::Line(text.to_string()));
    }

    /// Commit a complete transcript echo through the existing output channel.
    /// One payload avoids separately allocating/sending its separator rows.
    #[inline]
    pub(crate) fn print_block(&self, mut text: String) {
        crate::render::layout_policy::LayoutPolicy::finish_block(&mut text);
        if !text.is_empty() {
            let _ = self.tx.send(OutputMsg::Raw(text));
        }
    }

    pub fn suspend(&self) -> io::Result<TerminalSuspension> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        self.tx
            .send(OutputMsg::Suspend(TerminalHandoff {
                ready: ready_tx,
                resume: resume_rx,
                done: done_tx,
            }))
            .map_err(|_| io::Error::other("terminal render loop stopped"))?;
        ready_rx
            .recv()
            .map_err(|_| io::Error::other("terminal handoff failed"))??;
        Ok(TerminalSuspension {
            resume: Some(resume_tx),
            done: done_rx,
        })
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

/// The cache-TTL footer tip, the advice behind it, and whether the tip offers
/// a switch (a footer stop that opens the TTL panel).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CacheTip {
    pub tip: Option<engine_core::Tip>,
    pub advice: Option<engine_core::cache_advisor::TtlAdvice>,
    pub actionable: bool,
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
    Queue {
        text: String,
        display: String,
    },
    QuestionAnswer(String),
    /// Inspect the frozen review without answering it.
    InspectDocument(String),
    StopBackgroundTask(String),
    /// A prompt-cache TTL chosen in the TTL panel.
    SetCacheTtl {
        choice: engine_core::cache_ttl::TtlChoice,
        force_now: bool,
    },
    /// Esc flushes pending input, or cancels when none is pending.
    Abort,
    /// Ctrl+C always cancels, even with pending input.
    Cancel,
    Exit,
}

/// Share the active input controls between the footer and scrollback notices.
#[must_use]
#[inline]
pub(crate) const fn running_input_controls(has_pending_input: bool) -> &'static str {
    if has_pending_input {
        "Enter steer · Tab queue · Esc flush · Ctrl+C cancel"
    } else {
        "Enter steer · Tab queue · Esc/Ctrl+C cancel"
    }
}

/// Handle returned by `spawn_repl_ui` for the coordinator to use.
pub struct ReplHandle {
    pub output: OutputSender,
    pub ui: UiCommandSender,
    pub input_rx: Receiver<InputEvent>,
    pub spinner: ActivityState,
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
        ActivityState,
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

// ── PendingSlot — in-flight tools and queued messages ─────────────────

/// One pending item awaiting the user's eye at a turn boundary: either an
/// in-flight tool call (rendered as a running L-frame card) or a message queued
/// for the next turn (a human `❯` line or an inbound `Message from …` line). They share
/// one ordered list so the overlay shows exactly what arrived, in arrival order
/// — a tool starting, then a peer message, then another tool, interleave the
/// way they happened rather than being grouped into two panes.
#[derive(Clone, Debug)]
pub enum PendingItem {
    /// An in-flight tool call. Keyed by `tool_use_id`; removed on completion
    /// (the finished green/red card is written to scrollback by the render
    /// engine, not here).
    Tool(ToolCard),
    /// A message queued for the next turn — `display` is the compact one-line
    /// form (`❯ …` for human, `Message from sender: …` for a peer). Purely transient:
    /// the coordinator echoes the real line to scrollback when it flushes.
    /// `is_human` distinguishes a typed input from an inbound A2A/peer message
    /// so the empty-buffer `↑` removes all human chips (matching the
    /// coordinator's `dequeue_all_human`) while leaving peer chips in place.
    Input {
        id: u64,
        display: String,
        is_human: bool,
        delivery: engine_events::InputDelivery,
    },
}

/// Render the pending overlay: in-flight tool cards and queued messages in one
/// ordered list, joined into one multi-line string.
///
/// Queued messages take priority in the height budget — they are the thing the
/// user is waiting to see (a queued input, an inbound A2A), and each is one
/// line, so all of them always render, in arrival order. Tool cards fill the
/// remaining budget in arrival order; excess tool cards collapse to a
/// `… +N more tools` line so a busy turn can't push a message out of view.
///
/// A pure projection of `items` — it never commits to scrollback (the render
/// engine commits finished tool cards; the coordinator echoes flushed messages).
/// The layout owner budgets the resulting projection alongside all other slots.
/// `max_lines` bounds full tool cards, not the terminal height.
fn render_pending_overlay(items: &[PendingItem], max_lines: usize, term_width: usize) -> String {
    use crate::render::{DIM, RESET};

    if items.is_empty() {
        return String::new();
    }
    let support = crate::render::ColorSupport::detect();
    let theme = crate::render::theme();
    // Messages are one line each and always shown; reserve their space first so
    // tool cards (multi-line) can never crowd a queued message out.
    let message_count = items
        .iter()
        .filter(|i| matches!(i, PendingItem::Input { .. }))
        .count();
    // Budget left for tool-card lines after messages take their rows. Keep at
    // least room for the overflow marker if there are any tools.
    let tool_budget = max_lines.saturating_sub(message_count);

    let cards: Vec<_> = items
        .iter()
        .map(|item| match item {
            PendingItem::Tool(card) => Some(
                crate::cli::format::render_tool_card_with_width(
                    &card.content,
                    card.status,
                    term_width,
                )
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            ),
            PendingItem::Input { .. } => None,
        })
        .collect();
    let tool_lines: usize = cards.iter().flatten().map(Vec::len).sum();
    let tools_total = cards.iter().flatten().count();
    // Reserve the marker only when something really overflows. Five compact
    // two-line cards fit in ten rows; reserving unconditionally hid the fifth.
    let visible_budget = tool_budget.saturating_sub(usize::from(tool_lines > tool_budget));
    let mut lines = Vec::new();
    let mut tool_lines_used = 0;
    let mut tools_shown = 0;
    for (item, card_lines) in items.iter().zip(cards) {
        match item {
            PendingItem::Input {
                display, delivery, ..
            } => {
                lines.push(queued_message::render(
                    display,
                    *delivery == engine_events::InputDelivery::Steer,
                    term_width,
                    support,
                    theme,
                ));
            }
            PendingItem::Tool(_) => {
                let card_lines = card_lines.expect("tool card projection");
                if tool_lines_used + card_lines.len() <= visible_budget {
                    tool_lines_used += card_lines.len();
                    lines.extend(card_lines);
                    tools_shown += 1;
                }
            }
        }
    }
    let tools_hidden = tools_total - tools_shown;
    if tools_hidden > 0 {
        lines.push(crate::render::text_layout::truncate_to_width(
            &format!("{DIM}… +{tools_hidden} more tools{RESET}"),
            term_width,
        ));
    }
    lines.join("\n")
}

// ── TodoSlot — todo summary and items ─────────────────────────────────

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
/// - Full projection bounds item count; shared layout owns terminal height.
/// - Priority order: in_progress > pending > completed; hidden summary
fn render_todo_panel(todos: &[runtime::Todo], max_display: usize) -> String {
    use crate::render::{ansi_fg, theme, BOLD, DIM, RESET};

    if todos.is_empty() {
        return String::new();
    }

    let t = theme();
    let success = ansi_fg(t.success);
    let info = ansi_fg(t.info());

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
    let mut header = crate::render::StyledLine::muted();
    header.push_count(todos.len());
    header.push(" todos (");
    header.push_count(completed_count);
    header.push(" done");
    if in_progress_count > 0 {
        header.push(", ");
        header.push_count(in_progress_count);
        header.push(" in progress");
    }
    header.push(", ");
    header.push_count(open_count);
    header.push(" open)");

    let mut lines = Vec::with_capacity(todos.len() + 2);
    lines.push(header.to_string());

    // Summary and full view derive their counts/styles from the same source.
    if max_display == 0 {
        return lines.join("\n");
    }

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
    transcript: Option<Arc<Mutex<transcript::Transcript>>>,
    tasks: Arc<Mutex<TaskBrowser>>,
    prose_preview: Arc<Mutex<ProsePreview>>,
    output_rx: Arc<Mutex<Receiver<OutputMsg>>>,
    terminal_handoff: Arc<Mutex<Option<TerminalHandoff>>>,
    ui_rx: Arc<Mutex<Receiver<UiCommand>>>,
    input_tx: SyncSender<InputEvent>,
    spinner: ActivityState,
    permission_mode: String,
    stderr_redir: Arc<Mutex<Option<stderr_redirect::StderrRedirect>>>,
    /// Todo items for the `TodoSlot`. Updated by `UiCommand::UpdateTodos`
    /// in the tick loop, read during the render phase. Uses `Arc<Mutex>`
    /// instead of a `use_state` hook to avoid shifting hook indices.
    todo_items: Arc<Mutex<Vec<runtime::Todo>>>,
    /// Running tool cards for the StagingSlot overlay, in insertion order.
    /// Pending items for the overlay above the StatusSlot: in-flight tool cards
    /// and queued messages (human + inbound A2A) in one ordered list, so the
    /// overlay shows arrival order across both. `ToolStarted`/`InputUpsert`
    /// append; `ToolFinished` removes a tool by id; `InputsClear` drops
    /// the queued messages. Same `Arc<Mutex>` rationale as `todo_items` —
    /// avoids shifting hook indices.
    pending: Arc<Mutex<Vec<PendingItem>>>,
    /// Empty-buffer `↑` hook: pops the newest **human** queued item out of the
    /// coordinator's staging area and returns its text for the input slot,
    /// skipping inbound A2A/peer items. `None` when no queue is wired (the
    /// handler then walks prompt history as before). Backed by the shared
    /// `Arc<Mutex<TurnInputCoordinator>>` the coordinator loop also holds.
    dequeue_hook: Option<UpArrowDequeueHook>,
}

/// Empty-buffer `↑` dequeue hook — see [`ReplContext::dequeue_hook`].
pub type UpArrowDequeueHook = Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[component]
fn ReplApp(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let ctx = hooks.use_context::<ReplContext>();
    let transcript = ctx.transcript.clone();
    let transcript_for_future = transcript.clone();
    let transcript_for_events = transcript.clone();
    let prose_preview = ctx.prose_preview.clone();
    let prose_preview_for_future = ctx.prose_preview.clone();
    let output_rx = Arc::clone(&ctx.output_rx);
    let terminal_handoff = Arc::clone(&ctx.terminal_handoff);
    let terminal_handoff_for_future = Arc::clone(&ctx.terminal_handoff);
    let ui_rx = Arc::clone(&ctx.ui_rx);
    let input_tx = ctx.input_tx.clone();
    let spinner = ctx.spinner.clone();
    let permission_mode = ctx.permission_mode.clone();
    let stderr_redir = Arc::clone(&ctx.stderr_redir);
    let todo_items = Arc::clone(&ctx.todo_items);
    let todo_items_for_future = Arc::clone(&ctx.todo_items);
    let pending = Arc::clone(&ctx.pending);
    let pending_for_future = Arc::clone(&ctx.pending);
    let dequeue_hook = ctx.dequeue_hook.clone();
    let tasks = Arc::clone(&ctx.tasks);
    let tasks_for_future = Arc::clone(&ctx.tasks);
    let tasks_for_events = Arc::clone(&ctx.tasks);
    drop(ctx);

    // use_terminal_size must be called before use_future and use_terminal_events
    // to maintain consistent hook ordering.
    let (term_width, term_height) = hooks.use_terminal_size();
    let input_frame = hooks.use_input_frame();

    let (stdout, _stderr) = hooks.use_output();
    let mut system = hooks.use_context_mut::<SystemContext>();
    let mut input_value = hooks.use_state(String::new);
    let mut frame = hooks.use_state(|| 0usize);
    let mut background_frame = hooks.use_state(|| 0usize);
    let mut background_animation = hooks.use_ref_default::<bool>();
    let mut foreground_state = hooks.use_state(|| EngineState::Idle);
    let mut tool_progress = hooks.use_state(|| None::<String>);
    let mut turn_result = hooks.use_state(|| None::<(u32, Arc<StyledText>)>);
    let mut has_submitted = hooks.use_state(|| false);
    let mut input_slot = hooks.use_state(|| InputSlot::TextInput);
    let mut should_exit = hooks.use_state(|| false);
    let mut last_ctrlc = hooks.use_state(|| None::<Instant>);
    let history = hooks.use_state(Vec::<String>::new);
    let mut history_cursor = hooks.use_state(|| None::<usize>);
    let saved_input = hooks.use_state(String::new);
    let mut tab_candidates = hooks.use_state(Vec::<String>::new);
    let mut tab_index = hooks.use_state(|| 0usize);
    let mut footer_hint = hooks.use_state(|| None::<(String, Instant)>);
    let mut footer_tips = hooks.use_state(FooterTips::default);
    let mut dialpad_cursor = hooks.use_state(|| 0usize);
    // Free-text buffer for a DialPad's built-in custom-input row. Every DialPad
    // has this row (empty until the question opts in via `allow_custom_input`);
    // typing appends here in-place — no slot switch, no component-mount race —
    // mirroring how FuzzySelect owns its `filter`. Cleared when a question opens.
    let mut dialpad_input = hooks.use_state(String::new);
    // Ephemeral paste store: placeholder_id -> real pasted text.
    // Allocated when the user pastes, freed on submit/clear.
    // Never persisted — the real content goes into the submitted message.
    let mut paste_store = hooks.use_state(std::collections::HashMap::<u32, String>::new);
    let mut next_paste_id = hooks.use_state(|| 1u32);
    let mut input_reviewable = hooks.use_state(|| true);
    let mut measurements = hooks.use_ref_default::<Measurements>();

    // Drain keys against their current UI owner before applying asynchronous
    // panel changes. Otherwise the editor's /ps submission Enter can also enter
    // task details when OpenTasks is applied between the two event consumers.
    // Clone for the terminal event handler.
    let input_tx_for_events = input_tx.clone();
    let stdout_for_events = stdout.clone();

    hooks.use_terminal_events({
        let pending_for_keys = Arc::clone(&pending);
        let input_frame = input_frame.clone();
        move |event| match event {
            TerminalEvent::Key(KeyEvent {
                code,
                kind,
                modifiers,
                ..
            }) if kind != KeyEventKind::Release => {
                if let Some(transcript) = &transcript_for_events {
                    let navigation_key = matches!(code, KeyCode::PageUp | KeyCode::PageDown)
                        || (code == KeyCode::End && modifiers.contains(KeyModifiers::ALT));
                    if navigation_key && matches!(*input_slot.read(), InputSlot::TextInput)
                        && tasks_for_events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).focus == TaskFocus::Input {
                        let mut view = transcript.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        if code == KeyCode::End { view.latest(); }
                        else { view.scroll(if code == KeyCode::PageUp { -10 } else { 10 }).expect("scroll transcript"); }
                        frame.set(frame.get().wrapping_add(1));
                        return;
                    }
                }
                let current_slot = input_slot.read().clone();
                if code == KeyCode::Char('o') && modifiers.contains(KeyModifiers::CONTROL) {
                    let question = match &current_slot {
                        InputSlot::DialPad(question) => Some(question),
                        InputSlot::FuzzySelect(state) => Some(&state.question),
                        InputSlot::TextInput => None,
                    };
                    if let Some(question) = question.filter(|q| q.description.is_some()) {
                        let document = prompt_text::render_document(
                            question.description.as_ref().expect("description checked"),
                            &question.presentation,
                            crate::render::query_terminal_width(),
                        );
                        let _ = input_tx_for_events.send(InputEvent::InspectDocument(document));
                        return;
                    }
                }
                // Never submit or edit contents that cannot be reviewed. The
                // mounted editor preserves its buffer/cursor while unfocused.
                if !(code == KeyCode::Esc
                    || input_reviewable.get() && input_frame.is_current()
                    || code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
                {
                    return;
                }
                // One keyboard owner. Task browsing never sends a model prompt
                // or cancels the parent turn, and questions retain priority.
                if matches!(current_slot, InputSlot::TextInput) {
                    let mut browser = tasks_for_events.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let focus = browser.focus;
                    let is_cancel = code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL);
                    if focus != TaskFocus::Input && is_cancel {
                        browser.close();
                        input_value.set(String::new());
                    }
                    // Navigation is local; cancellation always reaches the
                    // same handler below, including double Ctrl+C to exit.
                    if focus != TaskFocus::Input && !is_cancel {
                        match (focus, code) {
                            (TaskFocus::Footer, KeyCode::Enter | KeyCode::Down) => browser.open(),
                            (TaskFocus::Footer, KeyCode::Esc | KeyCode::Up) => browser.close(),
                            (TaskFocus::List, KeyCode::Down) => browser.move_selection(true),
                            (TaskFocus::List, KeyCode::Up) => browser.move_selection(false),
                            (TaskFocus::List, KeyCode::Enter) => browser.detail(),
                            (TaskFocus::List, KeyCode::Esc) => browser.close(),
                            (TaskFocus::Detail, KeyCode::Esc) if modifiers.contains(KeyModifiers::ALT) => browser.close(),
                            (TaskFocus::Detail, KeyCode::Esc) => browser.open(),
                            (TaskFocus::Detail, KeyCode::Up | KeyCode::PageUp) => browser.scroll(true, if code == KeyCode::PageUp { 10 } else { 1 }),
                            (TaskFocus::Detail, KeyCode::Down | KeyCode::PageDown) => browser.scroll(false, if code == KeyCode::PageDown { 10 } else { 1 }),
                            (TaskFocus::List | TaskFocus::Detail, KeyCode::Char('k')) => {
                                if let Some(id) = browser.stop_id() { let _ = input_tx_for_events.send(InputEvent::StopBackgroundTask(id)); }
                            }
                            (TaskFocus::Footer, KeyCode::Char(_)) if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                                browser.close();
                            }
                            (TaskFocus::Footer | TaskFocus::Tip, KeyCode::Left | KeyCode::Right) => {
                                browser.move_footer_stop(code == KeyCode::Right);
                            }
                            (TaskFocus::Tip, KeyCode::Enter | KeyCode::Down) => browser.open_ttl(),
                            (TaskFocus::Tip, KeyCode::Esc | KeyCode::Up) => browser.close(),
                            (TaskFocus::Tip, KeyCode::Char(_)) if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                                browser.close();
                            }
                            (TaskFocus::Ttl, KeyCode::Up | KeyCode::Down) => browser.move_ttl(code == KeyCode::Down),
                            (TaskFocus::Ttl, KeyCode::Esc) => browser.close_ttl(),
                            (TaskFocus::Ttl, KeyCode::Enter | KeyCode::Char('n')) => {
                                let _ = input_tx_for_events.send(InputEvent::SetCacheTtl {
                                    choice: browser.ttl_choice(),
                                    force_now: code == KeyCode::Char('n'),
                                });
                                browser.close();
                            }
                            _ => {}
                        }
                        frame.set(frame.get().wrapping_add(1));
                        return;
                    }
                    if code == KeyCode::Down && browser.has_footer_stop() && history_cursor.get().is_none()
                        && input_value.read().is_empty() {
                        browser.focus_footer();
                        frame.set(frame.get().wrapping_add(1));
                        return;
                    }
                }
                match code {
                    // ── Enter ──────────────────────────────────────────
                    KeyCode::Enter if !modifiers.contains(KeyModifiers::SHIFT) => {
                        match &current_slot {
                            InputSlot::DialPad(question) => {
                                let selected = dialpad_cursor.get();
                                let on_custom_row =
                                    question.allow_custom_input && selected == question.options.len();
                                if on_custom_row {
                                    // On the `[+]` row: submit the typed buffer
                                    // as the free-text answer. Empty buffer does
                                    // nothing (stay on the row so the user can
                                    // type), matching an input field. Expand any
                                    // paste placeholders so a collapsed paste is
                                    // sent in full (same as TextInput submit).
                                    let raw = dialpad_input.read().clone();
                                    let store_snap = paste_store.read().clone();
                                    let answer =
                                        expand_paste_placeholders(&raw, &store_snap).trim().to_string();
                                    if !answer.is_empty() {
                                        if !*has_submitted.read() {
                                            has_submitted.set(true);
                                        }
                                        let _ = input_tx_for_events
                                            .send(InputEvent::QuestionAnswer(answer));
                                        dialpad_input.set(String::new());
                                        paste_store.write().clear();
                                        next_paste_id.set(1);
                                        input_value.set(String::new());
                                        input_slot.set(InputSlot::TextInput);
                                    }
                                } else {
                                    submit_dialpad_selection(question, selected, &input_tx_for_events);
                                    if !*has_submitted.read() { has_submitted.set(true); }
                                    input_value.set(String::new());
                                    input_slot.set(InputSlot::TextInput);
                                }
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
                            // TextInput owns submission and clearing at event
                            // time, before the next key can edit its buffer.
                            InputSlot::TextInput => {}
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
                            // With custom input, options.len() is the extra
                            // `[+]` row, so the cursor may reach it.
                            InputSlot::DialPad(q) if q.allow_custom_input => q.options.len(),
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
                            InputSlot::TextInput => None,
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
                    // ── Digit shortcut — DialPad only ─────────────────
                    // Only when NOT composing free text: an empty custom-input
                    // buffer AND the cursor not parked on the `[+]` row. Once you
                    // start typing (or select `[+]`), digits are literal input.
                    KeyCode::Char(ch)
                        if matches!(current_slot, InputSlot::DialPad(ref q)
                            if q.options.len() >= 2
                                && dialpad_input.read().is_empty()
                                && dialpad_cursor.get() != q.options.len())
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
                    // ── Typing a free-form answer — DialPad custom-input row ──
                    // A printable char (not a digit quick-select) appends to the
                    // DialPad's in-place custom-input buffer and moves the cursor
                    // onto the `[+]` row. No slot switch, so a fast keystroke
                    // burst can't be dropped by a component-mount race. Only when
                    // the question opted in via `allow_custom_input`.
                    KeyCode::Char(ch)
                        if matches!(current_slot, InputSlot::DialPad(ref q) if q.allow_custom_input)
                            && !modifiers.contains(KeyModifiers::CONTROL)
                            && !modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if let InputSlot::DialPad(ref question) = current_slot {
                            let mut buf = dialpad_input.read().clone();
                            buf.push(ch);
                            dialpad_input.set(buf);
                            dialpad_cursor.set(question.options.len());
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
                    KeyCode::Backspace
                        if matches!(current_slot, InputSlot::DialPad(_))
                            && !dialpad_input.read().is_empty() =>
                    {
                        let mut buf = dialpad_input.read().clone();
                        buf.pop();
                        dialpad_input.set(buf);
                    }
                    // ── Global keys ───────────────────────────────────
                    KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = input_tx_for_events.send(InputEvent::Exit);
                        should_exit.set(true);
                    }
                    KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                        // TextInput applies this edit before the next queued key.
                        if !matches!(current_slot, InputSlot::TextInput) {
                            input_value.set(String::new());
                            history_cursor.set(None);
                        }
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
                            let _ = input_tx_for_events.send(InputEvent::Cancel);
                            let hint_msg = format!("{}Press Ctrl-C again to exit{}", crate::render::DIM, crate::render::RESET);
                            footer_hint.set(Some((hint_msg, Instant::now() + ctrlc_hint_ttl())));
                            // The mounted editor clears its own buffer in this
                            // key event, before subsequent typing. Panel input
                            // has a different owner and is cleared here.
                            if !matches!(current_slot, InputSlot::TextInput) {
                                input_value.set(String::new());
                                dialpad_input.set(String::new());
                            }
                        }
                    }
                    KeyCode::Esc => {
                        // The coordinator alone decides flush versus cancel;
                        // only cancellation sends ClearQuestion back to UI.
                        let _ = input_tx_for_events.send(InputEvent::Abort);
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
            // Bracketed paste. The iocraft `TextInput` component consumes this
            // via its own `on_paste` when the TextInput slot is active; for the
            // DialPad custom-input row (and FuzzySelect filter) that component is
            // not mounted, so route the paste into the active buffer here using
            // the SAME normalize/placeholder logic (DRY).
            TerminalEvent::Paste(pasted) => {
                if !input_reviewable.get() || !input_frame.is_current() {
                    return;
                }
                let current_slot = input_slot.read().clone();
                match &current_slot {
                    InputSlot::DialPad(q)
                        if q.allow_custom_input
                            && dialpad_cursor.get() == q.options.len() =>
                    {
                        let current = dialpad_input.read().clone();
                        let mut id = next_paste_id.get();
                        let mut store = paste_store.write();
                        let new_val =
                            apply_paste_to_buffer(&current, &pasted, &mut id, &mut store);
                        drop(store);
                        next_paste_id.set(id);
                        dialpad_input.set(new_val);
                    }
                    InputSlot::FuzzySelect(_) => {
                        // FuzzySelect filters in place; a paste is just more
                        // filter text (short pastes only — a giant paste as a
                        // filter is nonsensical, so insert literally).
                        let mut slot = input_slot.write();
                        if let InputSlot::FuzzySelect(ref mut fs) = *slot {
                            fs.filter.push_str(&normalize_paste_newlines(&pasted));
                            fs.apply_filter();
                        }
                    }
                    _ => {}
                }
            }
            TerminalEvent::FullscreenMouse(event) => {
                if let Some(transcript) = &transcript_for_events {
                    let delta = match event.kind {
                        MouseEventKind::ScrollUp => -3,
                        MouseEventKind::ScrollDown => 3,
                        _ => return,
                    };
                    transcript.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                        .scroll(delta).expect("scroll transcript");
                    frame.set(frame.get().wrapping_add(1));
                }
            }
            _ => {}
        }
    });

    // Clone handles for the future (StdoutHandle is Clone).
    let stdout_for_future = stdout.clone();
    let spinner_for_future = spinner.clone();
    let output_rx_for_future = Arc::clone(&output_rx);
    let ui_rx_for_future = Arc::clone(&ui_rx);
    let stderr_redir_for_future = Arc::clone(&stderr_redir);

    // 80ms tick loop: drain output/control channels, update spinner text.
    hooks.use_future(async move {
        let mut todo_hide_deadline: Option<Instant> = None;
        let mut stderr_text = Utf8Stream::default();
        let mut task_clock_tick = Instant::now();
        let mut background_tick = Instant::now();
        loop {
            smol::Timer::after(Duration::from_millis(80)).await;
            let mut is_preview_changed = false;
            let mut is_transcript_changed = false;

            // Drain stderr redirect — route captured text through stdout
            // so it appears in the iocraft scrollback instead of corrupting
            // the canvas.
            if let Ok(guard) = stderr_redir_for_future.lock() {
                if let Some(ref redir) = *guard {
                    if let Some(captured) = redir.drain() {
                        write_iocraft_output(
                            &stdout_for_future,
                            transcript_for_future.as_ref(),
                            &stderr_text.push(&captured),
                            false,
                        );
                        is_transcript_changed = transcript_for_future.is_some();
                    }
                }
            }

            // Drain output channel.
            if let Ok(rx) = output_rx_for_future.lock() {
                loop {
                    let (text, terminated) = match rx.try_recv() {
                        Ok(OutputMsg::UserPrompt(display)) => {
                            if let Some(view) = &transcript_for_future {
                                view.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .begin_prompt().expect("mark transcript prompt");
                            }
                            let mut text = crate::cli::format::format_input_echo(&display, crate::render::query_terminal_width()).0;
                            crate::render::layout_policy::LayoutPolicy::finish_block(&mut text);
                            (text, false)
                        }
                        Ok(OutputMsg::AssistantUpdate { committed, preview }) => {
                            prose_preview_for_future
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .update(preview);
                            is_preview_changed = true;
                            // use_output queues history for the same presentation
                            // as the newly rendered preview, never an empty frame.
                            (committed, false)
                        }
                        Ok(OutputMsg::Line(text)) => (text, true),
                        Ok(OutputMsg::Raw(text)) => (text, false),
                        Ok(OutputMsg::ToolProgress(text)) => {
                            if *tool_progress.read() != text {
                                tool_progress.set(text);
                            }
                            continue;
                        }
                        Ok(OutputMsg::Suspend(request)) => {
                            *terminal_handoff_for_future
                                .lock()
                                .expect("terminal handoff mutex poisoned") = Some(request);
                            frame.set(frame.get().wrapping_add(1));
                            break;
                        }
                        Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                    };
                    write_iocraft_output(&stdout_for_future, transcript_for_future.as_ref(), &text, terminated);
                    is_transcript_changed |= transcript_for_future.is_some();
                }
            }

            if let Ok(rx) = ui_rx_for_future.lock() {
                loop {
                    match rx.try_recv() {
                        Ok(UiCommand::OpenTasks) => {
                            tasks_for_future.lock().unwrap_or_else(std::sync::PoisonError::into_inner).open();
                            frame.set(frame.get().wrapping_add(1));
                        }
                        Ok(UiCommand::BackgroundTask(event)) => {
                            let changed = tasks_for_future
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .update(event);
                            if changed {
                                frame.set(frame.get().wrapping_add(1));
                            }
                        }
                        Ok(UiCommand::BackgroundTaskError(message)) => {
                            footer_hint
                                .set(Some((message, Instant::now() + Duration::from_secs(3))));
                        }
                        Ok(UiCommand::ShowQuestion(question)) => {
                            // A request's immutable description belongs to
                            // history. Only its first field presents it; the
                            // live slot owns the current field's controls.
                            if question.index == 0 {
                                if let Some(description) = &question.description {
                                    let document = prompt_text::render_document(
                                        description,
                                        &question.presentation,
                                        crate::render::query_terminal_width(),
                                    );
                                    write_iocraft_output(&stdout_for_future, transcript_for_future.as_ref(), &document, true);
                                    is_transcript_changed |= transcript_for_future.is_some();
                                }
                            }
                            tasks_for_future
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .close();
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
                                dialpad_input.set(String::new());
                                InputSlot::DialPad(question)
                            };
                            input_slot.set(slot);
                            input_value.set(String::new());
                        }
                        Ok(UiCommand::ClearQuestion) => {
                            let is_plain_input = matches!(*input_slot.read(), InputSlot::TextInput);
                            if !is_plain_input {
                                input_slot.set(InputSlot::TextInput);
                                input_value.set(String::new());
                            }
                        }
                        Ok(UiCommand::SetTip(tip)) => footer_tips.write().set(tip),
                        Ok(UiCommand::SetCacheTip(view)) => {
                            footer_tips.write().status(StatusSource::Cache, view.tip);
                            tasks_for_future
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .set_cache_advice(view.advice, view.actionable);
                            frame.set(frame.get().wrapping_add(1));
                        }
                        Ok(UiCommand::SetWorkspacePosition(label)) => {
                            footer_tips.write().position(label);
                        }
                        Ok(UiCommand::SetStatusTip(source, tip)) => {
                            footer_tips.write().status(source, tip);
                            frame.set(frame.get().wrapping_add(1));
                        }
                        Ok(UiCommand::BeginTurn) => footer_tips.write().begin_turn(),
                        Ok(UiCommand::SetTurnResult { text, turn }) => {
                            turn_result.set(Some((turn, text)));
                        }
                        Ok(UiCommand::UpdateTodos(todos)) => {
                            if let Ok(mut items) = todo_items_for_future.lock() {
                                let has_incomplete = todos
                                    .iter()
                                    .any(|t| t.status != runtime::TodoStatus::Completed);
                                if todos.is_empty() {
                                    // Empty list → hide immediately
                                    items.clear();
                                    todo_hide_deadline = None;
                                } else if has_incomplete {
                                    // Has open todos → show, cancel any hide timer
                                    *items = todos;
                                    todo_hide_deadline = None;
                                } else if todo_hide_deadline.is_none() {
                                    // All terminal → start 5s hide timer
                                    *items = todos;
                                    todo_hide_deadline =
                                        Some(Instant::now() + Duration::from_secs(5));
                                } else {
                                    *items = todos;
                                }
                            }
                        }
                        Ok(UiCommand::ToolQueued { id, name, input }) => {
                            if let Ok(mut pending) = pending_for_future.lock() {
                                pending.push(PendingItem::Tool(ToolCard::new(id, &name, &input)));
                            }
                        }
                        Ok(UiCommand::ToolStarted { id, name, input }) => {
                            if let Ok(mut pending) = pending_for_future.lock() {
                                if let Some(PendingItem::Tool(card)) = pending.iter_mut().find(
                                    |item| matches!(item, PendingItem::Tool(card) if card.id == id),
                                ) {
                                    if card.input != input {
                                        card.content =
                                            Arc::new(crate::cli::format::tool_call_start_content(
                                                &name, &input,
                                            ));
                                        card.input = input;
                                    }
                                    card.status = crate::cli::format::ToolStatus::Running;
                                }
                            }
                        }
                        Ok(UiCommand::ToolFinished { id }) => {
                            // Only clear the transient overlay entry. The
                            // finished card's permanent content is written to
                            // scrollback by the render engine on the ordered
                            // `output` channel — never from here — so the
                            // overlay stays a pure projection.
                            if let Ok(mut pending) = pending_for_future.lock() {
                                pending
                                    .retain(|p| !matches!(p, PendingItem::Tool(c) if c.id == id));
                            }
                        }
                        // Waiting for an answer has no spinner-driven redraw.
                        // Input ownership changes must wake their own frame.
                        Ok(UiCommand::InputUpsert { id, display, is_human, delivery }) => {
                            if let Ok(mut pending) = pending_for_future.lock() {
                                let row = PendingItem::Input { id, display, is_human, delivery };
                                if let Some(existing) = pending.iter_mut().find(|item| matches!(item, PendingItem::Input { id: value, .. } if *value == id)) {
                                    *existing = row;
                                } else { pending.push(row); }
                            }
                            frame.set(frame.get().wrapping_add(1));
                        }
                        Ok(UiCommand::InputRemove(id)) => {
                            if let Ok(mut pending) = pending_for_future.lock() {
                                pending.retain(|item| !matches!(item, PendingItem::Input { id: value, .. } if *value == id));
                            }
                            frame.set(frame.get().wrapping_add(1));
                        }
                        Ok(UiCommand::PendingInputsClear) => {
                            if let Ok(mut pending) = pending_for_future.lock() { pending.retain(|item| matches!(item, PendingItem::Tool(_))); }
                            frame.set(frame.get().wrapping_add(1));
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => break,
                    }
                }
            }

            // Auto-hide the TodoSlot 5s after all todos are completed.
            // Refresh elapsed time only in an open running detail, once a second.
            // An invisible task entry introduces no idle rendering churn.
            if task_clock_tick.elapsed() >= Duration::from_secs(1) {
                task_clock_tick = Instant::now();
                if tasks_for_future
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shows_running_detail()
                {
                    frame.set(frame.get().wrapping_add(1));
                }
            }
            if let Some(deadline) = todo_hide_deadline {
                if Instant::now() >= deadline {
                    if let Ok(mut items) = todo_items_for_future.lock() {
                        if !items.is_empty() {
                            items.clear();
                            frame.set(frame.get().wrapping_add(1));
                        }
                    }
                    todo_hide_deadline = None;
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

            // One lifecycle snapshot drives the status and footer. Foreground
            // activity owns the fast cadence; visible background work reuses
            // this clock at 400ms. It never records model/task progress.
            // Identical idle State::set calls starve keyboard delivery in iocraft.
            let idx = frame.get();
            let state = spinner_for_future.engine_state();
            if state == EngineState::Idle && background_animation.get() {
                if background_tick.elapsed() >= Duration::from_millis(400) {
                    background_frame.set(background_frame.get().wrapping_add(1));
                    background_tick = Instant::now();
                }
            } else {
                background_tick = Instant::now();
            }
            if state == EngineState::Running || is_preview_changed || is_transcript_changed {
                frame.set(idx.wrapping_add(1));
            }
            if state != EngineState::Idle && turn_result.read().is_some() {
                turn_result.set(None);
            }
            if foreground_state.get() != state {
                foreground_state.set(state);
            }
        }
    });

    // Exit check: `system` was obtained before the event handler and is
    // NOT captured by the Send closure. The exit flag is set inside the
    // closure; we check it here outside the closure.
    if let Some(request) = terminal_handoff
        .lock()
        .expect("terminal handoff mutex poisoned")
        .take()
    {
        system.suspend(move || {
            let mut redir = stderr_redir.lock().expect("stderr redirect mutex poisoned");
            let released = redir
                .as_mut()
                .map_or(Ok(()), stderr_redirect::StderrRedirect::suspend);
            let is_released = released.is_ok();
            if request.ready.send(released).is_ok() && is_released {
                // Channel closure also releases us if the caller unwinds.
                let _ = request.resume.recv();
            }
            let restored = redir
                .as_mut()
                .map_or(Ok(()), stderr_redirect::StderrRedirect::resume);
            let _ = request.done.send(restored);
        });
    }
    if *should_exit.read() {
        system.exit();
    }

    // ── Derive ChromeSlots from state ──────────────────────────────────
    let activity_state = foreground_state.get();
    let w = usize::from(term_width);
    let val = input_value.read().clone();
    let current_input_slot = input_slot.read().clone();
    let submitted = *has_submitted.read();
    let turn_active = activity_state != EngineState::Idle;

    let mut browser = tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Task events and the slow icon frame invalidate only this bounded row.
    // Both surfaces consume the browser's existing event projection, even idle.
    let completed_turn = turn_result.read().as_ref().map(|(turn, _)| *turn);
    let background_animates = browser.summary().animates_status();
    let background_status = hooks.use_memo(
        || {
            browser
                .summary()
                .status_lines(w, completed_turn, background_frame.get())
        },
        (browser.summary(), w, completed_turn, background_frame.get()),
    );

    // StatusSlot: Foreground > Background > TurnResult > Empty
    let status_slot = if activity_state == EngineState::Running {
        // Reuse the existing status row and shared column layout. No additional
        // live rows can spill into scrollback as tool output grows.
        let progress = tool_progress.read();
        let pending = pending.lock().unwrap();
        let mut tools = ToolActivity {
            progress: progress.as_deref(),
            ..Default::default()
        };
        for item in pending.iter() {
            if let PendingItem::Tool(card) = item {
                if card.status == crate::cli::format::ToolStatus::Running {
                    tools.count += 1;
                    tools.name.get_or_insert(&card.name);
                }
            }
        }
        // Format with the same iocraft width snapshot as the other chrome,
        // including the very first frame after a resize.
        StatusSlot::Active(spinner.render_frame(frame.get(), activity_state, tools, w))
    } else if activity_state == EngineState::AwaitingInput {
        // The existing question/approval panel owns this interaction. No
        // animation or duplicate waiting instruction above InputSlot.
        StatusSlot::Empty
    } else if let Some((full, summary)) = &background_status {
        StatusSlot::Background {
            full: full.clone(),
            summary: summary.clone(),
        }
    } else if let Some((_, ref tr)) = *turn_result.read() {
        StatusSlot::TurnResult(tr.clone())
    } else {
        StatusSlot::Empty
    };

    // The frozen document belongs to history; both pickers retain only controls.
    let question = match &current_input_slot {
        InputSlot::DialPad(question) => Some(question),
        InputSlot::FuzzySelect(state) => Some(&state.question),
        InputSlot::TextInput => None,
    };
    // InputSlot rendering
    let (panel_text, prompt_label) = match &current_input_slot {
        InputSlot::TextInput => (None, crate::render::PROMPT_PREFIX),
        InputSlot::DialPad(q) => (
            Some(format_question_panel(
                q,
                dialpad_cursor.get(),
                &dialpad_input.read(),
            )),
            "\u{2753} ",
        ),
        InputSlot::FuzzySelect(fs) => (Some(fs.format_panel()), "\u{1f50d} "),
    };

    let perm = permission_mode.clone();
    let sep = "\u{2500}".repeat(w);
    let pending_input = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|item| matches!(item, PendingItem::Input { .. }));
    // The keys for the current state follow the workspace position. Only turn
    // and queue state choose them — never typing — so the footer changes when
    // a turn starts or ends, as the rest of the screen does, and keystrokes
    // never redraw it. While queued input exists, Esc flushes it.
    let keys = if turn_active {
        running_input_controls(pending_input)
    } else if pending_input {
        "Enter send · Esc flush · ↑ edit queued"
    } else if submitted {
        "/help"
    } else {
        "Type /help for commands"
    };
    let explicit_hint = footer_hint.read().clone();
    let native_hint = explicit_hint.as_ref().map(|(text, _)| text.as_str());
    // Spinner and draft ticks do not change footer content. Keep only the
    // current full/compact pair, invalidating on every visible dependency.
    let (footer_text, footer_summary) = {
        let tips = footer_tips.read();
        let hints = FooterHints {
            transient: native_hint,
            keys,
        };
        hooks.use_memo(
            || {
                let render =
                    |permission| Arc::new(tips.render(permission, &browser, turn_active, hints, w));
                (render(&perm), render("compact · enlarge to expand"))
            },
            (
                &*tips,
                &perm,
                browser.footer_label(false),
                browser.footer_label(true),
                browser.focus,
                turn_active,
                hints,
                w,
                crate::render::ColorSupport::detect(),
            ),
        )
    };
    let task_panel = browser.is_panel().then(|| browser.panel(w, usize::MAX));
    let task_focus = browser.focus;
    drop(browser);

    let todos = if task_panel.is_some() {
        Vec::new()
    } else {
        todo_items
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    };
    // PendingSlot projects tools and queued messages in arrival order. It
    // never commits history: completed cards and flushed messages have their
    // existing transcript owners.
    let pending_items = if task_panel.is_some() {
        Vec::new()
    } else {
        pending
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    };
    let queued = pending_items
        .iter()
        .filter(|item| matches!(item, PendingItem::Input { .. }))
        .count();
    let running = pending_items.len() - queued;
    let pending_summary = if pending_items.is_empty() {
        String::new()
    } else {
        format!("{running} running · {queued} pending")
    };
    // Decode semantic initial styles before measurement. Painting consumes the
    // same spans, so explicit resets never acquire a parent fallback color.
    let chrome_has_color =
        crate::render::ColorSupport::detect() != crate::render::ColorSupport::NoColor;
    let chrome_theme = crate::render::theme();
    let chrome_style = |color| crossterm::style::ContentStyle {
        foreground_color: chrome_has_color.then_some(color),
        ..crossterm::style::ContentStyle::default()
    };
    let chrome_border = chrome_has_color
        .then_some(chrome_theme.border)
        .and_then(ansi_text::ui_color);
    let chrome_muted = chrome_style(chrome_theme.muted);
    let question_style = chrome_style(chrome_theme.primary);
    let status_text = match &status_slot {
        StatusSlot::Active(s) => ansi(s),
        StatusSlot::Background { full, .. } => full.clone(),
        StatusSlot::TurnResult(s) => s.clone(),
        StatusSlot::Empty => Arc::default(),
    };
    let status_summary = match &status_slot {
        StatusSlot::Active(_) => ansi("Status: turn active"),
        StatusSlot::Background { summary, .. } => summary.clone(),
        StatusSlot::TurnResult(_) => ansi("Status: last turn (collapsed)"),
        StatusSlot::Empty => Arc::default(),
    };
    let heading = ansi_with_style(
        question
            .map(|q| {
                let title = q.title.as_ref().map(|title| format!("[{title}]"));
                if q.description.is_some() {
                    format!(
                        "{} · Ctrl+O view full text",
                        title.as_deref().unwrap_or("Review document")
                    )
                } else {
                    title.unwrap_or_default()
                }
            })
            .unwrap_or_default(),
        question_style,
    );
    let mut controls = if let Some(panel) = task_panel.clone() {
        ansi(panel)
    } else {
        ansi(panel_text.unwrap_or_default())
    };

    // Match the editor's reserved cursor column and trailing empty lines.
    // This memo invalidates only on draft/width, never spinner or task ticks.
    let desired_text_rows = hooks.use_memo(
        {
            let content = format!("{val}\u{2588}");
            move || {
                element! { Text(content) }
                    .render(Some(w.saturating_sub(3).max(1)))
                    .height()
                    .max(1)
            }
        },
        (&val, w),
    );
    let mut measured = measurements.write();
    measured.begin(w);
    let heading_rows = measured.rows(&heading);
    let mut controls_rows = measured.rows(&controls);
    let input_row_count = if task_panel.is_some() {
        0
    } else {
        match &current_input_slot {
            InputSlot::TextInput => desired_text_rows,
            InputSlot::DialPad(_) => 0,
            InputSlot::FuzzySelect(fs) => {
                measured.rows(&ansi(format!("{prompt_label}{}", fs.filter)))
            }
        }
    };
    let layout = ChromeLayout::allocate(
        w,
        term_height as usize,
        heading_rows + controls_rows + input_row_count,
        [
            Slot {
                minimum_full_rows: 0,
                full: ansi(render_pending_overlay(&pending_items, 10, w)),
                summary: ansi(pending_summary),
            },
            Slot {
                // Retain main's stable spinner/result height while the
                // shared allocator owns all slot and prose row accounting.
                minimum_full_rows: if status_text.text.is_empty() {
                    0
                } else {
                    128usize.div_ceil(w.max(1))
                },
                full: status_text,
                summary: status_summary.clone(),
            },
            Slot {
                minimum_full_rows: 0,
                full: ansi(render_todo_panel(&todos, 10)),
                summary: ansi(render_todo_panel(&todos, 0)),
            },
            Slot {
                minimum_full_rows: 0,
                full: footer_text,
                summary: footer_summary,
            },
        ],
        format!(
            "{running} tools · {queued} queued · {} todos · {}",
            todos.len(),
            if matches!(status_slot, StatusSlot::Background { .. }) {
                status_summary.text.clone()
            } else {
                format!(
                    "{} status",
                    usize::from(!matches!(status_slot, StatusSlot::Empty))
                )
            }
        ),
        &mut measured,
    );
    // Do not wake the canvas when the allocator has hidden this row/icon or
    // a question owns input. This presentation flag derives from the same
    // task summary; it is neither a task registry nor an engine lifecycle.
    *background_animation.write() = background_animates
        && matches!(status_slot, StatusSlot::Background { .. })
        && matches!(current_input_slot, InputSlot::TextInput)
        && layout.status.rows > 0
        && layout
            .status
            .content
            .text
            .starts_with(crate::render::activity::spinner_frame(
                background_frame.get(),
            ));
    if task_panel.is_some() {
        // The browser chooses its visible rows from the shared allocation.
        // Its lazy output cache survives both projections; fixed controls are
        // still checked below rather than being clipped or blindly operable.
        controls = ansi(
            tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .panel(w, layout.input_rows),
        );
        controls_rows = measured.rows(&controls);
    }
    let input_layout = InputLayout::allocate(
        layout.input_rows,
        [heading_rows, controls_rows, input_row_count],
    );
    measured.end();
    drop(measured);
    let input_warning = layout.warning.clone().or_else(|| {
        ((!matches!(current_input_slot, InputSlot::TextInput) || task_panel.is_some())
            && !input_layout.fits_controls)
            .then(|| "Enlarge terminal to review; Ctrl+C cancels".to_string())
    });
    let reviewable = input_warning.is_none();
    input_frame.set_enabled(reviewable);
    if input_reviewable.get() != reviewable {
        input_reviewable.set(reviewable);
    }
    let visible = if reviewable {
        input_layout
    } else {
        InputLayout::default()
    };
    let row_height = |rows: usize| {
        u32::try_from(rows).expect("allocated chrome rows fit the terminal's u16 geometry")
    };

    let preview_text = (|| {
        if transcript.as_ref().is_some_and(|view| {
            view.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_reading()
        }) {
            return None;
        }
        let mut state = prose_preview
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.is_active()
            || !reviewable
            || task_panel.is_some()
            || !matches!(&current_input_slot, InputSlot::TextInput)
        {
            return None;
        }
        // Leave room for prompt context while an unfinished paragraph fills
        // the preview. The shared allocator still reserves all input controls.
        let context_rows = usize::from(transcript.is_some()) * 2;
        state.layout(w, layout.preview_rows.saturating_sub(context_rows))
    })();
    let preview_rows = preview_text
        .as_ref()
        .map_or(0, |text| text.text.lines().count());
    let slot_width = Size::Length(u32::from(term_width));
    let transcript_rows = if transcript.is_some() {
        (term_height as usize).saturating_sub(
            1 + preview_rows
                + layout.pending.rows
                + layout.status.rows
                + layout.todo.rows
                + layout.footer.rows
                + layout.input_rows
                + 2 * layout.separators,
        )
    } else {
        0
    };
    let transcript_frame = transcript
        .as_ref()
        .filter(|_| transcript_rows > 1)
        .map(|view| {
            view.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .render(w, transcript_rows)
                .expect("render transcript")
        });
    let input_background = transcript
        .as_ref()
        .and_then(|_| crate::render::prompt_style(true).background_color)
        .and_then(ansi_text::ui_color);
    let input_foreground = transcript
        .as_ref()
        .and_then(|_| crate::render::prompt_style(true).foreground_color)
        .and_then(ansi_text::ui_color);
    let header_background = transcript
        .as_ref()
        .and_then(|_| crate::render::prompt_style(false).background_color)
        .and_then(ansi_text::ui_color);
    let viewport_enabled = transcript.is_some();
    // Present an empty live frame before exiting. iocraft otherwise retains
    // its last canvas, leaving status/todos above the next resumed session.
    // The ordinary history writer has already committed conversation output.
    if *should_exit.read() {
        return element! { View(height: 0) };
    }
    element! {
        View(width: u32::from(term_width), flex_direction: FlexDirection::Column,
            height: if viewport_enabled { Size::Length(u32::from(term_height.saturating_sub(1))) } else { Size::Auto },
            max_height: u32::from(term_height.saturating_sub(1)), overflow: Overflow::Hidden) {
            View(height: row_height(transcript_rows), flex_shrink: 0.0, overflow: Overflow::Hidden) {
                #(transcript_frame.map(|display| element! {
                    View(flex_direction: FlexDirection::Column, width: slot_width) {
                        View(height: 1, width: slot_width, background_color: header_background) {
                            RichText(content: display.header, width: slot_width, wrap: TextWrap::NoWrap)
                        }
                        RichText(content: display.body, width: slot_width, wrap: TextWrap::NoWrap)
                    }
                }))
            }
            // Keep the preview parent mounted across streaming handoff so it
            // cannot shift the editor's component identity.
            View(flex_direction: FlexDirection::Column, width: slot_width,
                height: Size::Length(row_height(preview_rows))) {
                #(preview_text.map(|text| element! { RichText(content: text, wrap: TextWrap::NoWrap, width: slot_width) }))
            }
            #(row_text("pending", layout.pending.content, layout.pending.rows))
            #(row_text("status", layout.status.content, layout.status.rows))
            // TodoSlot owns semantic spans; separator grey is not its
            // fallback for terminal-foreground counts or descriptions.
            #(row_text("todo", layout.todo.content, layout.todo.rows))
            #((layout.separators > 0).then(|| element! {
                View(height: row_height(layout.separators), flex_shrink: 0.0, background_color: input_background) {
                    #((!viewport_enabled).then(|| element! { Text(content: sep.clone(), color: chrome_border, wrap: TextWrap::NoWrap) }))
                }
            }))
            #((!reviewable && layout.input_rows > 0).then(|| element! {
                View(height: row_height(layout.input_rows), flex_shrink: 0.0, overflow: Overflow::Hidden) {
                    Text(content: input_warning.unwrap_or_default(), color: chrome_has_color.then_some(chrome_theme.warning).and_then(ansi_text::ui_color))
                }
            }))
            #(row_text("heading", heading, visible.heading))
            #(row_text("controls", controls, visible.controls))
            // The editor stays mounted even with zero allocated rows so a
            // shrink/grow cannot discard its cursor, draft or edit ownership.
            View(height: row_height(visible.editor), flex_shrink: 0.0, overflow: Overflow::Hidden, background_color: input_background) {
            #(if task_panel.is_some() {
                element! { View(flex_direction: FlexDirection::Row) {} }
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
                        Text(content: prompt_label, color: input_foreground)
                        Text(content: filter_display, color: input_foreground)
                    }
                }
            } else {
                let tasks_for_edit = Arc::clone(&tasks);
                let mut input_history = input_history::InputHistory {
                    history,
                    selected: history_cursor,
                    saved_input,
                    dequeue: dequeue_hook.clone(),
                    pending: Arc::clone(&pending),
                };
                let input_tx_for_edit = input_tx.clone();
                let on_edit: TextInputEditHandler = Box::new(move |event, current, cursor| {
                    let navigation_key = matches!(event, TerminalEvent::Key(KeyEvent { code: KeyCode::PageUp | KeyCode::PageDown, .. }))
                        || matches!(event, TerminalEvent::Key(KeyEvent { code: KeyCode::End, modifiers, .. }) if modifiers.contains(KeyModifiers::ALT));
                    if viewport_enabled && navigation_key {
                        return Some(TextInputEdit { value: current.to_string(), cursor_offset: cursor });
                    }
                    // Read focus at event time: several keys can arrive before
                    // the next render. TextInput remains the only draft editor.
                    let mut browser = tasks_for_edit.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let edits_draft = match event {
                        TerminalEvent::Paste(_) => true,
                        TerminalEvent::Key(KeyEvent { code: KeyCode::Char(_), modifiers, .. }) =>
                            !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
                        _ => false,
                    };
                    if matches!(browser.focus, TaskFocus::Footer | TaskFocus::Tip) && edits_draft {
                        browser.close();
                    }
                    if !reviewable || browser.focus != TaskFocus::Input
                        || !matches!(*input_slot.read(), InputSlot::TextInput) {
                        return Some(TextInputEdit { value: input_value.read().clone(), cursor_offset: cursor });
                    }
                    drop(browser);
                    if let Some(edit) = input_history.edit(event, current, cursor) {
                        return Some(edit);
                    }
                    match event {
                        TerminalEvent::Key(KeyEvent { code, modifiers, .. })
                            if (*code == KeyCode::Enter && !modifiers.contains(KeyModifiers::SHIFT))
                                || (*code == KeyCode::Tab && !modifiers.contains(KeyModifiers::CONTROL)
                                    && !current.starts_with('/')) => {
                            if let Some(event) = enter_key_event_for_value(None, 0, current) {
                                if !*has_submitted.read() { has_submitted.set(true); }
                                let event = match event {
                                    InputEvent::Exit => { should_exit.set(true); InputEvent::Exit }
                                    InputEvent::Submit { text, .. } => {
                                        let store = paste_store.read().clone();
                                        let expanded = expand_paste_placeholders(&text, &store);
                                        let display = expand_paste_for_display(&text, &store);
                                        paste_store.write().clear();
                                        next_paste_id.set(1);
                                        let trimmed = expanded.trim();
                                        let mut history = input_history.history.write();
                                        if !trimmed.is_empty() && history.last().is_none_or(|last| last != trimmed) {
                                            history.push(trimmed.to_string());
                                        }
                                        if *code == KeyCode::Tab { InputEvent::Queue { text: expanded, display } }
                                        else { InputEvent::Submit { text: expanded, display } }
                                    }
                                    _ => unreachable!("plain input cannot answer a panel"),
                                };
                                let _ = input_tx_for_edit.send(event);
                                input_history.selected.set(None);
                                input_history.saved_input.set(String::new());
                                Some(TextInputEdit { value: String::new(), cursor_offset: 0 })
                            } else { Some(TextInputEdit { value: current.to_string(), cursor_offset: cursor }) }
                        }
                        TerminalEvent::Key(KeyEvent { code, modifiers, .. })
                            if *code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) =>
                            Some(TextInputEdit { value: String::new(), cursor_offset: 0 }),
                        // Image paste: an empty bracketed paste (Cmd+V on macOS
                        // with only an image on the clipboard), or Ctrl+V/Alt+V.
                        TerminalEvent::Paste(pasted) if pasted.is_empty() => {
                            match crate::clipboard_image::read_image_for_paste() {
                                Ok(Some(path)) => {
                                    let mut id = next_paste_id.get();
                                    let edit = insert_image_placeholder(current, cursor, &path, &mut id, &mut paste_store.write());
                                    next_paste_id.set(id);
                                    Some(edit)
                                }
                                Ok(None) => None,
                                Err(error) => {
                                    footer_tips.write().set(engine_core::Tip {
                                        priority: engine_core::TipPriority::Normal,
                                        message: format!("Could not read the clipboard: {error}"),
                                    });
                                    None
                                }
                            }
                        }
                        TerminalEvent::Key(KeyEvent { code: KeyCode::Char(key), modifiers, kind, .. })
                            if key.eq_ignore_ascii_case(&'v')
                                && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                && *kind != KeyEventKind::Release => {
                            let unchanged = TextInputEdit { value: current.to_string(), cursor_offset: cursor };
                            if crate::clipboard_image::is_held_repeat(current) {
                                return Some(unchanged);
                            }
                            let message = match crate::clipboard_image::read_image() {
                                Ok(Some(path)) => {
                                    let mut id = next_paste_id.get();
                                    let edit = insert_image_placeholder(current, cursor, &path, &mut id, &mut paste_store.write());
                                    next_paste_id.set(id);
                                    crate::clipboard_image::note_paste_key(&edit.value);
                                    return Some(edit);
                                }
                                Ok(None) => crate::clipboard_image::NO_IMAGE_NOTICE.to_string(),
                                Err(error) => format!("Could not read the clipboard: {error}"),
                            };
                            crate::clipboard_image::note_paste_key(current);
                            footer_tips.write().set(engine_core::Tip {
                                priority: engine_core::TipPriority::Normal,
                                message,
                            });
                            Some(unchanged)
                        }
                        TerminalEvent::Paste(pasted) => {
                            let mut id = next_paste_id.get();
                            let mut store = paste_store.write();
                            let insertion = apply_paste_to_buffer("", pasted, &mut id, &mut store);
                            next_paste_id.set(id);
                            Some(TextInputEdit {
                                value: format!("{}{}{}", &current[..cursor], insertion, &current[cursor..]),
                                cursor_offset: cursor + insertion.len(),
                            })
                        }
                        _ => None,
                    }
                });
                element! {
                    View(flex_direction: FlexDirection::Row, height: 100pct, width: 100pct) {
                        View(width: 2, flex_shrink: 0.0) { Text(content: prompt_label, color: input_foreground) }
                        TextInput(
                            color: input_foreground,
                            value: val,
                            has_focus: reviewable && matches!(task_focus, TaskFocus::Input | TaskFocus::Footer | TaskFocus::Tip),
                            multiline: true,
                            auto_grow: false,
                            on_edit: Some(on_edit),
                            on_change: move |new_val: String| {
                                input_value.set(new_val);
                            },
                        )
                    }
                }
            })
            }
            // Separator
            #((layout.separators > 0).then(|| element! {
                View(height: row_height(layout.separators), flex_shrink: 0.0, background_color: input_background) {
                    #((!viewport_enabled).then(|| element! { Text(content: sep, color: chrome_border, wrap: TextWrap::NoWrap) }))
                }
            }))
            // FooterSlot
            #(row_text("footer", layout.footer.content, layout.footer.rows))
        }
    }
}

/// Adapt the shared line framing to iocraft for every captured/output stream.
fn write_iocraft_output(
    stdout: &StdoutHandle,
    transcript: Option<&Arc<Mutex<transcript::Transcript>>>,
    text: &str,
    terminated: bool,
) {
    if let Some(view) = transcript {
        view.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(text, terminated)
            .expect("spool transcript");
        return;
    }
    split_lines(text, terminated, |op| match op {
        OutputOp::Println(line) => stdout.println(line),
        OutputOp::Print(chunk) => stdout.print(chunk),
    });
}

/// Spawn the iocraft REPL UI on a dedicated thread and return a handle
/// for the coordinator to communicate with it.
///
/// The coordinator reads `InputEvent`s from `ReplHandle::input_rx` and
/// sends output text via `ReplHandle::output`. The spinner state is
/// shared so the runner thread can update it atomically.
pub fn spawn_repl_ui(
    permission_mode: &str,
    startup_banner: &str,
    startup_prompts: Vec<usize>,
    todo_items: Vec<runtime::Todo>,
    dequeue_hook: Option<UpArrowDequeueHook>,
) -> ReplHandle {
    let (output_tx, output_rx) = mpsc::sync_channel::<OutputMsg>(512);
    let (ui_tx, ui_rx) = mpsc::sync_channel::<UiCommand>(16);
    let (input_tx, input_rx) = mpsc::sync_channel::<InputEvent>(16);
    let spinner = ActivityState::new();
    let transcript = runtime::experiments::is_enabled(
        runtime::experiments::Experiment::PromptViewport,
    )
    .then(|| {
        Arc::new(Mutex::new(
            transcript::Transcript::new().expect("create transcript spool"),
        ))
    });

    let stderr_redir: Arc<Mutex<Option<stderr_redirect::StderrRedirect>>> =
        Arc::new(Mutex::new(None));

    let ctx = ReplContext {
        transcript: transcript.clone(),
        tasks: Arc::new(Mutex::new(TaskBrowser::default())),
        prose_preview: Arc::new(Mutex::new(ProsePreview::default())),
        output_rx: Arc::new(Mutex::new(output_rx)),
        terminal_handoff: Arc::new(Mutex::new(None)),
        ui_rx: Arc::new(Mutex::new(ui_rx)),
        input_tx: input_tx.clone(),
        spinner: spinner.clone(),
        permission_mode: permission_mode.to_string(),
        stderr_redir: Arc::clone(&stderr_redir),
        todo_items: Arc::new(Mutex::new(todo_items)),
        pending: Arc::new(Mutex::new(Vec::new())),
        dequeue_hook,
    };

    let banner = startup_banner.to_string();
    let join_handle = std::thread::Builder::new()
        .name("repl-ui".into())
        .spawn(move || {
            // Print banner before entering the render loop so the user sees
            // session info in the scrollback. iocraft's render_loop enters
            // raw mode immediately, so we print first.
            if let Some(view) = &transcript {
                let mut view = view
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut start = 0;
                for position in startup_prompts {
                    view.append(&banner[start..position], false)
                        .expect("seed transcript");
                    view.begin_prompt().expect("seed prompt");
                    start = position;
                }
                view.append(&banner[start..], true)
                    .expect("seed transcript");
            } else {
                println!("{banner}");
            }

            // Activate stderr redirect before entering raw mode so that
            // any library writing to fd 2 is captured and routed through
            // the iocraft stdout handle by the tick loop.
            if let Ok(mut guard) = stderr_redir.lock() {
                *guard = stderr_redirect::StderrRedirect::activate();
            }

            let mut app = element! {
                ContextProvider(value: Context::owned(ctx)) {
                    ReplApp
                }
            };
            let render = app.render_loop().ignore_ctrl_c();
            let render = if transcript.is_some() {
                // Keep the inline renderer in the main terminal buffer. Mouse
                // reporting supplies navigation for the conversation viewport.
                render.enable_mouse_capture()
            } else {
                render
            };
            smol::block_on(render).expect("iocraft render_loop failed");
            if let Some(view) = transcript {
                view.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .replay(&mut io::stdout().lock())
                    .expect("restore native transcript");
            }
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
        ToolCard::new(id.to_string(), name, "{}")
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
    fn pending_overlay_empty_when_nothing_pending() {
        assert_eq!(render_pending_overlay(&[], 40, 80), "");
    }

    #[test]
    fn pending_overlay_reports_tools_when_cards_have_no_budget() {
        let plain = strip_ansi(&render_pending_overlay(
            &[PendingItem::Tool(tool_card("1", "bash"))],
            0,
            80,
        ));
        assert!(plain.contains("+1 more tools"), "{plain}");
    }

    #[test]
    fn pending_overlay_renders_one_card_per_running_call() {
        let items = vec![
            PendingItem::Tool(tool_card("1", "bash")),
            PendingItem::Tool(tool_card("2", "read_file")),
        ];
        let plain = strip_ansi(&render_pending_overlay(&items, 40, 80));
        // Each running call is a Running L-frame card (╭─ header … ╰─).
        assert_eq!(plain.matches("╭─").count(), 2, "{plain}");
        // Headers show the canonical tool label (`Bash`, `Read`) — the SAME
        // label the completed scrollback card resolves, so a tool never
        // changes case when it finishes.
        assert!(plain.contains("Bash"), "{plain}");
        assert!(plain.contains("Read"), "{plain}");
    }

    #[test]
    fn pending_overlay_interleaves_tools_and_messages_in_arrival_order() {
        // A tool, then a queued message, then another tool — the overlay shows
        // them in arrival order, not grouped into two panes.
        let items = vec![
            PendingItem::Tool(tool_card("1", "bash")),
            PendingItem::Input {
                display: "Message from mac-ai: hi".to_string(),
                is_human: false,
                id: 0,
                delivery: engine_events::InputDelivery::Queue,
            },
            PendingItem::Tool(tool_card("2", "read_file")),
        ];
        let plain = strip_ansi(&render_pending_overlay(&items, 40, 80));
        let bash_at = plain.find("Bash").expect("bash card");
        let msg_at = plain.find("Message from mac-ai").expect("queued message");
        let read_at = plain.find("Read").expect("read card");
        assert!(
            bash_at < msg_at && msg_at < read_at,
            "arrival order tool→message→tool must be preserved: {plain}"
        );
        assert!(plain.contains("↳ queued:"), "message line marker: {plain}");
    }

    #[test]
    fn pending_overlay_collapses_overflow_beyond_height_budget() {
        // The full projection has a ten-line card budget. Each card is 3 lines
        // (╭─ / │ / ╰─ — bash with a "{}" input has a $ body line), so ~3 cards
        // fit and the rest collapse into a "+N more pending" line.
        let items: Vec<PendingItem> = (0..8)
            .map(|i| PendingItem::Tool(tool_card(&i.to_string(), "bash")))
            .collect();
        let plain = strip_ansi(&render_pending_overlay(&items, 10, 80));
        assert!(
            plain.contains("more tools"),
            "expected overflow summary: {plain}"
        );
        let shown = plain.matches("╭─").count();
        assert!((1..8).contains(&shown), "shown={shown}: {plain}");
    }

    fn question_with_options() -> QuestionPromptView {
        QuestionPromptView {
            presentation: runtime::QuestionPresentation::Question,
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
            presentation: runtime::QuestionPresentation::Question,
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
        let panel =
            StyledText::from_ansi(&format_question_panel(&question_with_options(), 1, "")).text;

        assert!(panel.contains(" [1] Project (recommended)"));
        assert!(panel.contains("> [2] User"));
    }

    #[test]
    fn question_hints_only_offer_active_digit_shortcuts() {
        let mut question = question_with_options();
        let panel = StyledText::from_ansi(&format_question_panel(&question, 0, "")).text;
        assert!(panel.contains("1-2 quick select"));
        question.options.truncate(1);
        let panel = StyledText::from_ansi(&format_question_panel(&question, 0, "")).text;
        assert!(!panel.contains("quick select"));
        assert!(panel.contains("Enter confirm"));
    }

    #[test]
    fn custom_input_row_is_selectable_past_last_option() {
        let mut question = question_with_options();
        question.allow_custom_input = true;
        question.custom_input_hint = Some("type a comment".to_string());

        // Cursor on the `[+]` row (index == options.len()) marks it; empty buffer
        // shows a caret + the hint. This is the row Up/Down reach and typing
        // fills in-place.
        let panel = StyledText::from_ansi(&format_question_panel(
            &question,
            question.options.len(),
            "",
        ))
        .text;
        assert!(panel.contains("> [+]"));
        assert!(panel.contains("type a comment"));
        assert!(panel.contains("  [1] Project"));
        assert!(panel.contains("  [2] User"));
    }

    #[test]
    fn custom_input_row_shows_typed_buffer_in_place() {
        let mut question = question_with_options();
        question.allow_custom_input = true;

        // With a non-empty buffer, the `[+]` row renders the typed text (with a
        // caret when selected) instead of the hint — no slot switch needed.
        let on_row = StyledText::from_ansi(&format_question_panel(
            &question,
            question.options.len(),
            "hi there",
        ))
        .text;
        assert!(on_row.contains("> [+] hi there"));
        let off_row = StyledText::from_ansi(&format_question_panel(&question, 0, "hi there")).text;
        assert!(off_row.contains("[+] hi there"));
        assert!(!off_row.contains("type your own answer"));
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
    fn apply_paste_short_single_line_inserts_literally() {
        let mut store = std::collections::HashMap::new();
        let mut id = 1u32;
        let out = apply_paste_to_buffer("hi ", "there", &mut id, &mut store);
        assert_eq!(out, "hi there");
        assert!(store.is_empty(), "short paste is literal, not stored");
        assert_eq!(id, 1, "no id consumed");
    }

    #[test]
    fn apply_paste_line_count_boundary() {
        for (pasted, expected, next_id) in [
            ("l1\nl2", "l1\nl2", 1),
            ("l1\nl2\nl3", "[Pasted text #1 +2 lines]", 2),
        ] {
            let mut store = std::collections::HashMap::new();
            let mut id = 1;
            let out = apply_paste_to_buffer("", pasted, &mut id, &mut store);
            assert_eq!(out, expected);
            assert_eq!(id, next_id);
            assert_eq!(store.is_empty(), next_id == 1);
            assert_eq!(expand_paste_placeholders(&out, &store), pasted);
        }
    }

    #[test]
    fn apply_paste_multi_line_collapses_to_placeholder_and_expands() {
        let mut store = std::collections::HashMap::new();
        let mut id = 1u32;
        // >2 newlines crosses PASTE_PLACEHOLDER_MAX_LINES → placeholder.
        let pasted = "l1\nl2\nl3\nl4";
        let out = apply_paste_to_buffer("start ", pasted, &mut id, &mut store);
        assert_eq!(out, "start [Pasted text #1 +3 lines]");
        assert_eq!(id, 2, "id consumed");
        // Round-trips back to the real text at submit time.
        assert_eq!(
            expand_paste_placeholders(&out, &store),
            format!("start {pasted}")
        );
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
