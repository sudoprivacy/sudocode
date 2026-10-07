//! The terminal renderer for the engine↔renderer seam: it consumes
//! [`engine_events::EngineEvent`]s from an `EngineHandle` and draws them (the
//! markdown/ANSI stream, the • glyph margin, the spinner's byte counter +
//! "Reasoning…" cue, and tool call/result lines). This is the render *half* of
//! the old `CliStreamState`, now cleanly separated: the engine produces events,
//! this draws them, and nothing engine-side renders.
//!
//! `render` returns a [`RenderOutcome`] so the REPL loop knows when the engine
//! is blocked on a permission / question answer (which the loop collects and
//! sends back as an `EngineCommand`) and when the turn is finished.

use std::io::Write;

use engine_events::{
    EngineEvent, HookProgressEvent, PermissionRequest, QuestionPromptRequest, RequestId,
    RetryEvent, ToolProgressEvent,
};

use crate::cli::format::format_tool_result;
use crate::render::{
    layout_policy::LayoutPolicy, query_terminal_width, MarkdownStreamState, ResponseGlyphState,
    SpinnerRef, TerminalRenderer, DIM, RESET,
};
use crate::repl_ui::OutputSender;

/// What the caller (the REPL event loop) should do after an event is rendered.
pub(crate) enum RenderOutcome {
    /// Keep pulling events.
    Continue,
    /// The engine is blocked on a permission decision; collect it and send an
    /// [`engine_events::EngineCommand::PermissionAnswer`] with this id.
    NeedPermission {
        id: RequestId,
        request: PermissionRequest,
    },
    /// The engine is blocked on a structured question; collect answers and send
    /// an [`engine_events::EngineCommand::QuestionAnswer`] with this id.
    NeedQuestion {
        id: RequestId,
        request: QuestionPromptRequest,
    },
    /// The turn finished (completed or errored); stop pulling for this turn.
    Done,
}

/// Stateful terminal renderer for one turn's event stream.
pub(crate) struct EngineEventRenderer {
    markdown: MarkdownStreamState,
    /// Buffers thinking deltas until a line is complete so reasoning is emitted
    /// one dim line at a time, not one `\x1b[2m…\x1b[0m` pair per streamed delta
    /// (the fragmentation bug). Kept separate from `markdown` because thinking is
    /// shown as raw dim prose, deliberately not run through the answer's markdown
    /// renderer (its ANSI resets would cancel the dim, and its parser state must
    /// not interleave with the answer across the block boundary).
    thinking_pending: String,
    renderer: TerminalRenderer,
    glyph: ResponseGlyphState,
    layout: LayoutPolicy,
    block: Option<OutputBlock>,
    spinner: Option<SpinnerRef>,
    output_writer: Option<OutputSender>,
    /// `true` while inside a thinking block, so the "Reasoning…" spinner cue is
    /// raised once and lowered when real content resumes.
    thinking_active: bool,
    /// `true` once this thinking block has written text to the terminal, so
    /// leaving the block can close it off. Distinct from `thinking_active`:
    /// a block that produced only empty deltas needs no separator.
    thinking_printed: bool,
    /// Tool-call arguments remembered from `ToolCall` and paired with the
    /// matching `ToolResult`, so the completed card can show what was requested
    /// — the result payload does not echo command/path/strings. Shared type
    /// with session replay ([`crate::cli::format::ToolInputRegistry`]).
    tool_inputs: crate::cli::format::ToolInputRegistry,
    /// Whether the latest progress summary occupies the live status row.
    activity_visible: bool,
}

/// Consecutive chunks of prose/reasoning/activity belong to the same block.
/// Each completed tool result and notice starts an independent block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputBlock {
    Assistant,
    Reasoning,
    Activity,
    Tool,
    Notice,
}

impl EngineEventRenderer {
    pub(crate) fn new(spinner: Option<SpinnerRef>, output_writer: Option<OutputSender>) -> Self {
        Self {
            markdown: MarkdownStreamState::default(),
            thinking_pending: String::new(),
            renderer: TerminalRenderer::new(),
            glyph: ResponseGlyphState::new(query_terminal_width()),
            layout: LayoutPolicy::default(),
            block: None,
            spinner,
            output_writer,
            thinking_active: false,
            thinking_printed: false,
            tool_inputs: crate::cli::format::ToolInputRegistry::default(),
            activity_visible: false,
        }
    }

    fn apply_response(&mut self, text: &str) -> String {
        self.glyph.set_width(query_terminal_width());
        self.glyph.apply(text)
    }

    fn write_out(&mut self, text: &str) {
        self.clear_activity();
        if text.is_empty() {
            return;
        }
        self.layout.observe(text);
        if let Some(writer) = self.output_writer.as_mut() {
            let _ = write!(writer, "{text}").and_then(|()| writer.flush());
        } else {
            let _ = crate::render::output::write_stdout(text);
        }
    }

    #[inline]
    fn start_block(&mut self, block: OutputBlock) {
        if self.block != Some(block) || matches!(block, OutputBlock::Tool | OutputBlock::Notice) {
            self.write_out(self.layout.before_block());
            self.glyph.visible_col = 0;
        }
        self.block = Some(block);
    }

    fn write_response(&mut self, text: &str, block: OutputBlock) {
        if !text.is_empty() {
            self.start_block(block);
            let prefixed = self.apply_response(text);
            self.write_out(&prefixed);
        }
    }

    fn write_block(&mut self, text: &str, block: OutputBlock) {
        let text = text.trim_matches('\n');
        if !text.is_empty() {
            // Asynchronous tool/hook/notice events can arrive inside a text
            // block. They do not end the provider's Markdown or reasoning.
            self.pause_spinner();
            self.start_block(block);
            self.write_out(&format!("{text}\n"));
        }
    }

    fn finish_turn(&mut self) {
        self.finish_response();
        self.write_out(self.layout.before_block());
    }

    /// Close buffered content at a provider content boundary or turn end.
    /// Asynchronous UI events leave the stream's parser context intact.
    fn finish_response(&mut self) {
        self.end_thinking();
        self.flush_markdown();
    }

    fn flush_markdown(&mut self) {
        if let Some(rendered) = self.markdown.flush(&self.renderer) {
            self.pause_spinner();
            self.write_response(&rendered, OutputBlock::Assistant);
        }
    }

    fn clear_activity(&mut self) {
        if self.activity_visible {
            if let Some(writer) = &self.output_writer {
                writer.tool_progress(None);
            }
            self.activity_visible = false;
        }
    }

    fn render_tool_progress(&mut self, progress: &ToolProgressEvent) {
        let text = format_tool_progress(progress);
        if let Some(writer) = &self.output_writer {
            writer.tool_progress(Some(text));
            self.activity_visible = true;
        } else {
            // Without an interactive live region, retain a bounded log line.
            let text = crate::render::text_layout::truncate_to_width(&text, query_terminal_width());
            self.write_block(&text, OutputBlock::Activity);
        }
        self.resume_spinner();
    }

    fn pause_spinner(&self) {
        if let Some(s) = &self.spinner {
            s.pause();
        }
    }

    fn resume_spinner(&self) {
        if let Some(s) = &self.spinner {
            s.resume();
        }
    }

    /// Show that the transport is retrying a failed request.
    ///
    /// The line is unconditional. A status-line phase alone would be invisible
    /// on the paths that have no phase to set — the rustyline REPL and
    /// `--print` both hold a spinner whose `phase` is `None` — and "the
    /// indicator exists but not where you are looking" is how this stopped
    /// being visible in the first place. Where a phase IS available (iocraft)
    /// it is set too, so a long backoff also reads on the live status line
    /// rather than only in scrollback.
    fn render_retry(&mut self, event: &RetryEvent) {
        match event {
            RetryEvent::Waiting {
                attempt,
                max_retries,
                reason,
            } => {
                self.write_block(
                    &format!(
                        "{DIM}  \u{27f3} retry {attempt}/{max_retries} \u{2014} {reason}{RESET}"
                    ),
                    OutputBlock::Activity,
                );
                self.resume_spinner();
                if let Some(spinner) = &self.spinner {
                    spinner.set_retry(*attempt, *max_retries, reason.clone());
                }
            }
            RetryEvent::Resumed => {
                if let Some(spinner) = &self.spinner {
                    spinner.set_thinking(true);
                }
            }
        }
    }

    /// Emit buffered thinking text up to the last complete line, one dim line
    /// at a time, leaving any trailing partial line in `thinking_pending` for
    /// the next delta (or the `end_thinking` flush). Dimming per line — rather
    /// than per delta — is what collapses the stream of `\x1b[2m…\x1b[0m`
    /// fragments into clean, copy-safe reasoning lines while staying live.
    fn flush_thinking_lines(&mut self) {
        while let Some(newline) = self.thinking_pending.find('\n') {
            let line: String = self.thinking_pending.drain(..=newline).collect();
            let rendered = render_thinking_line(&line);
            self.write_response(&rendered, OutputBlock::Reasoning);
        }
    }

    /// Leave the thinking state (lower the "Reasoning…" cue) when real content
    /// resumes after a thinking block.
    fn end_thinking(&mut self) {
        if self.thinking_active {
            self.thinking_active = false;
            if let Some(s) = &self.spinner {
                s.set_thinking(false);
            }
        }
        if self.thinking_printed {
            self.thinking_printed = false;
            // Emit any trailing partial line (a block that ended without a final
            // newline) before closing, so the last reasoning sentence is not
            // dropped. `flush_thinking_lines` only emits up to the last newline;
            // the remainder lives in `thinking_pending` until here.
            if !self.thinking_pending.is_empty() {
                let rendered = render_thinking_line(&self.thinking_pending);
                self.thinking_pending.clear();
                self.write_response(&rendered, OutputBlock::Reasoning);
            }
            // Close partial lines before a legacy spinner pause clears its
            // current row. The next block observes this gap instead of adding
            // a second one.
            self.write_out(self.layout.before_block());
            self.glyph.visible_col = 0;
        }
    }

    pub(crate) fn render(&mut self, event: EngineEvent) -> RenderOutcome {
        match event {
            EngineEvent::TextDelta { text } => {
                self.end_thinking();
                if !text.is_empty() {
                    if let Some(s) = &self.spinner {
                        s.add_response_bytes(text.len() as u32);
                    }
                    if let Some(rendered) = self.markdown.push(&self.renderer, &text) {
                        self.pause_spinner();
                        self.write_response(&rendered, OutputBlock::Assistant);
                    }
                }
                RenderOutcome::Continue
            }
            EngineEvent::ThinkingDelta { text } => {
                if let Some(s) = &self.spinner {
                    s.add_response_bytes(text.len() as u32);
                }
                if !self.thinking_active {
                    self.flush_markdown();
                    self.thinking_active = true;
                    if let Some(s) = &self.spinner {
                        s.set_thinking(true);
                    }
                }
                // Surface the reasoning instead of dropping it. Extended
                // thinking is billed either way, and these deltas only arrive
                // when the `thinking` setting is on (with it off the request
                // carries no thinking parameter at all), so the single setting
                // means both "spend the tokens" and "show what they bought" —
                // paying for reasoning and then hiding it is the one
                // combination with no argument for it.
                //
                // Not fed through `self.markdown`: thinking is prose the model
                // wrote for itself, and interleaving it with the answer's
                // markdown stream would corrupt that parser's state across the
                // block boundary.
                if !text.is_empty() {
                    if !self.thinking_printed {
                        // Pause (and write the header) exactly once per block,
                        // not once per delta. `SpinnerRef::pause` emits a
                        // carriage return + erase-line to clear its own row;
                        // per-delta that lands mid-line and wipes the thinking
                        // text already drawn on it. `TextDelta` can afford to
                        // pause every delta only because it writes at markdown
                        // block boundaries, so its erase always lands at column
                        // 0. Reasoning is written at line boundaries and has the
                        // same protection.
                        self.pause_spinner();
                        self.write_response(&crate::cli::format::thinking_header(), OutputBlock::Reasoning);
                        self.thinking_printed = true;
                    }
                    self.thinking_pending.push_str(&text);
                    self.flush_thinking_lines();
                }
                RenderOutcome::Continue
            }
            EngineEvent::ToolCall { id, name, input } => {
                self.finish_response();
                // Remember the arguments so the completed card can show what was
                // requested — the ToolResult event/payload does not echo the
                // command (bash) or the path/strings (edit), they live only in
                // the call's input.
                self.tool_inputs.remember(&id, &input);
                self.pause_spinner();
                // No card is committed here. `Running` (amber) is a live,
                // self-clearing status that belongs only to the iocraft staging
                // overlay; scrollback is durable and must carry exactly one card
                // per call — the terminal-status (green/red) card committed on
                // `ToolResult` below. Committing a `Running` header here froze an
                // amber "in-flight" card permanently above the real result. The
                // in-flight cue is the spinner and the live progress summary.
                //
                // The glyph reset still runs: the tool line reset column 0, so
                // the next assistant text starts a fresh •-margined block.
                self.glyph.visible_col = 0;
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::ToolStarted { id, input, .. } => {
                self.tool_inputs.remember(&id, &input);
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                let input = self.tool_inputs.take(&id);
                self.write_block(&format_tool_result(&name, &input, &output, is_error), OutputBlock::Tool);
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::ToolProgress(progress) => {
                self.render_tool_progress(&progress);
                RenderOutcome::Continue
            }
            EngineEvent::HookProgress(ev) => {
                // Same pause/write/resume as every other event: writing through
                // `write_out` keeps this on stdout, behind the same lock the
                // spinner uses, so a hook line can no longer be torn in half
                // by a spinner frame.
                self.write_block(&format_hook_progress(&ev), OutputBlock::Activity);
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::Retry(ev) => {
                self.render_retry(&ev);
                RenderOutcome::Continue
            }
            EngineEvent::Notice { text } => {
                if !text.is_empty() {
                    self.write_block(&text, OutputBlock::Notice);
                    self.resume_spinner();
                }
                RenderOutcome::Continue
            }
            EngineEvent::Error { message } => {
                // Flush any partial assistant text first, then surface the error.
                // `end_thinking` first so a turn that failed while still
                // reasoning closes its dim block rather than letting the error
                // line inherit the dim attribute.
                self.finish_response();
                self.write_block(&message, OutputBlock::Notice);
                self.finish_turn();
                RenderOutcome::Done
            }
            EngineEvent::TurnComplete(_) => {
                // A turn can end on a thinking block (the model reasoned and
                // then produced no text, e.g. it was interrupted): close it so
                // the next prompt is not written into an open dim run.
                self.finish_turn();
                RenderOutcome::Done
            }
            EngineEvent::MessageComplete => {
                self.finish_response();
                self.block = None;
                RenderOutcome::Continue
            }
            EngineEvent::PermissionRequest { id, request } => RenderOutcome::NeedPermission { id, request },
            EngineEvent::QuestionRequest { id, request } => RenderOutcome::NeedQuestion { id, request },
            // No direct terminal effect: lifecycle/state/telemetry events. The
            // spinner already tracks progress from the deltas above.
            EngineEvent::PermissionDenied { .. }
            | EngineEvent::TurnStarted { .. }
            | EngineEvent::State(_)
            | EngineEvent::ModelResolved { .. }
            | EngineEvent::Usage(_)
            | EngineEvent::PromptCache(_)
            | EngineEvent::AutoCompaction(_)
            | EngineEvent::Compaction(_)
            | EngineEvent::ModelChanged { .. }
            | EngineEvent::PermissionModeChanged { .. }
            // Background completions are scheduled by the REPL event bridge.
            | EngineEvent::Subagent(_)
            | EngineEvent::BackgroundTask(_)
            | EngineEvent::BackgroundTaskError { .. } => RenderOutcome::Continue,
        }
    }
}

/// Format a live tool-progress event for the terminal. This is the render half
/// of the CLI `ToolExecutor`'s old `make_bash_progress_callback` /
/// `make_mcp_progress_callback`: the executor now reports structured data
/// (`ToolProgressEvent`) and the ANSI/glyph formatting lives here, above the
/// seam. External output is text, never terminal cursor or screen control.
fn format_tool_progress(progress: &ToolProgressEvent) -> String {
    let summary = |text: &str| {
        crate::render::styled_text::StyledText::from_ansi(text)
            .text
            .replace(['\r', '\n', '\t'], " ")
    };
    match progress {
        ToolProgressEvent::Bash {
            last_line,
            total_lines,
            total_bytes,
        } => {
            let bytes_display = if *total_bytes >= 1024 {
                format!("{:.1} KB", *total_bytes as f64 / 1024.0)
            } else {
                format!("{total_bytes} B")
            };
            format!(
                "  {DIM}\u{27f3} {}  ({total_lines} lines, {bytes_display}){RESET}",
                summary(last_line)
            )
        }
        ToolProgressEvent::Mcp {
            message,
            progress,
            total,
        } => {
            let status = match total {
                Some(total) if *total > 0.0 => {
                    let pct = (progress / total * 100.0).min(100.0);
                    format!(" ({pct:.0}%)")
                }
                _ => String::new(),
            };
            if let Some(msg) = message {
                format!("  {DIM}\u{27f3} {}{status}{RESET}", summary(msg))
            } else {
                format!("  {DIM}\u{27f3} progress: {progress:.0}{status}{RESET}")
            }
        }
    }
}

/// Format one live plugin-hook progress event for the terminal.
///
/// A pure formatter, deliberately: these lines used to go straight to stderr
/// via `eprintln!` while the turn spinner was painting stdout. Rust gives
/// stdout and stderr separate locks, so the two writes could interleave
/// mid-line and the terminal showed a torn line — the spinner's frame with the
/// tail of a hook line grafted onto it:
///
/// ```text
/// [hook PreToolUse] bash: echo hook-observed
/// ⠙ 🦀 Thinking... [claude-sonnet-4-6] (0.4s)          ] bash: echo hook-observed
/// ```
///
/// Every other event in this renderer already went through `write_out`, which
/// writes to `io::stdout()` and so shares the spinner's lock; hook progress was
/// the one exception, and the only one that tore. Returning a `String` lets the
/// caller emit it the same way as the rest.
///
/// The text is kept byte-identical to the pre-seam output (`[hook <event>]
/// <tool>: <cmd>` lines, with `(SudoCode plugin <id>)` attribution) for PTY
/// parity.
fn format_hook_progress(event: &HookProgressEvent) -> String {
    // Format SudoCode plugin attribution once; each outcome line includes it so
    // the user sees *who* ran the hook in addition to *what* happened.
    fn attribution(plugin_source: Option<&str>) -> String {
        match plugin_source {
            Some(plugin_id) => format!(" (SudoCode plugin {plugin_id})"),
            None => String::new(),
        }
    }
    let (label, event, tool_name, command, plugin_source) = match event {
        HookProgressEvent::Started {
            event,
            tool_name,
            command,
            plugin_source,
        } => ("hook", event, tool_name, command, plugin_source),
        HookProgressEvent::Completed {
            event,
            tool_name,
            command,
            plugin_source,
        } => ("hook done", event, tool_name, command, plugin_source),
        HookProgressEvent::Denied {
            event,
            tool_name,
            command,
            plugin_source,
        } => ("hook DENIED", event, tool_name, command, plugin_source),
        HookProgressEvent::Failed {
            event,
            tool_name,
            command,
            plugin_source,
        } => ("hook FAILED", event, tool_name, command, plugin_source),
        HookProgressEvent::Cancelled {
            event,
            tool_name,
            command,
            plugin_source,
        } => ("hook cancelled", event, tool_name, command, plugin_source),
    };
    format!(
        "[{label} {event_name}] {tool_name}: {command}{attr}",
        event_name = event.as_str(),
        attr = attribution(plugin_source.as_deref())
    )
}

/// Render one complete thinking line for the transcript: dim prose, but a
/// blank line emitted raw. Dimming is applied once per line here — the single
/// place that decides it — instead of once per streamed delta, which is what
/// turned reasoning into a stream of `\x1b[2m…\x1b[0m` fragments. A whitespace-
/// only line has no text to tint, so wrapping it would only add escape noise.
fn render_thinking_line(line: &str) -> String {
    if line.trim().is_empty() {
        line.to_string()
    } else {
        crate::cli::format::dim_thinking(line)
    }
}

#[cfg(test)]
mod tests {
    use super::{format_hook_progress, render_thinking_line};
    use crate::render::{DIM, RESET};
    use engine_events::HookProgressEvent;
    use runtime::HookEvent;

    /// The five outcome lines, byte-for-byte. These strings are a terminal
    /// contract the PTY tests match on, so a reworded label is a breaking
    /// change and should fail here rather than in a flaky screen scrape.
    #[test]
    fn every_outcome_renders_its_documented_line() {
        let cases = [
            (
                HookProgressEvent::Started {
                    event: HookEvent::PreToolUse,
                    tool_name: "bash".to_string(),
                    command: "echo hook-observed".to_string(),
                    plugin_source: None,
                },
                "[hook PreToolUse] bash: echo hook-observed",
            ),
            (
                HookProgressEvent::Completed {
                    event: HookEvent::PreToolUse,
                    tool_name: "bash".to_string(),
                    command: "echo hook-observed".to_string(),
                    plugin_source: None,
                },
                "[hook done PreToolUse] bash: echo hook-observed",
            ),
            (
                HookProgressEvent::Denied {
                    event: HookEvent::PreToolUse,
                    tool_name: "bash".to_string(),
                    command: "echo nope".to_string(),
                    plugin_source: None,
                },
                "[hook DENIED PreToolUse] bash: echo nope",
            ),
            (
                HookProgressEvent::Failed {
                    event: HookEvent::PostToolUse,
                    tool_name: "bash".to_string(),
                    command: "echo boom".to_string(),
                    plugin_source: None,
                },
                "[hook FAILED PostToolUse] bash: echo boom",
            ),
            (
                HookProgressEvent::Cancelled {
                    event: HookEvent::PostToolUseFailure,
                    tool_name: "bash".to_string(),
                    command: "echo stop".to_string(),
                    plugin_source: None,
                },
                "[hook cancelled PostToolUseFailure] bash: echo stop",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(format_hook_progress(&event), expected);
        }
    }

    /// Plugin-contributed hooks name their plugin, so a user can tell which
    /// installed thing is gating (or slowing) their tool call.
    #[test]
    fn a_plugin_hook_is_attributed_to_its_plugin() {
        let event = HookProgressEvent::Started {
            event: HookEvent::PreToolUse,
            tool_name: "bash".to_string(),
            command: "echo hi".to_string(),
            plugin_source: Some("guardrails".to_string()),
        };
        assert_eq!(
            format_hook_progress(&event),
            "[hook PreToolUse] bash: echo hi (SudoCode plugin guardrails)"
        );
    }

    /// The formatter returns a line with no trailing newline: the caller adds
    /// it when writing through `write_out`. Returning it pre-terminated would
    /// double-space the terminal.
    #[test]
    fn the_formatted_line_carries_no_trailing_newline() {
        let event = HookProgressEvent::Completed {
            event: HookEvent::PreToolUse,
            tool_name: "bash".to_string(),
            command: "echo hi".to_string(),
            plugin_source: None,
        };
        let line = format_hook_progress(&event);
        assert!(!line.ends_with('\n'), "unexpected newline in {line:?}");
    }

    /// A non-empty reasoning line is dimmed exactly once — one `DIM…RESET` pair
    /// for the whole line, which is the fix for the per-delta fragmentation that
    /// wrapped every streamed chunk in its own escape pair.
    #[test]
    fn a_reasoning_line_is_dimmed_once_as_a_whole() {
        let rendered = render_thinking_line("checking 17 is prime\n");
        assert_eq!(rendered, format!("{DIM}checking 17 is prime\n{RESET}"));
        assert_eq!(rendered.matches(DIM).count(), 1, "one dim open per line");
        assert_eq!(rendered.matches(RESET).count(), 1, "one reset per line");
    }

    /// A blank separator line between reasoning paragraphs is emitted raw: it
    /// has no text to tint, so wrapping it would only add escape noise to the
    /// scrollback the user copies out.
    #[test]
    fn a_blank_reasoning_line_is_emitted_without_escapes() {
        let rendered = render_thinking_line("  \n");
        assert_eq!(rendered, "  \n");
        assert!(!rendered.contains(DIM), "blank line carries no dim escape");
    }
}
