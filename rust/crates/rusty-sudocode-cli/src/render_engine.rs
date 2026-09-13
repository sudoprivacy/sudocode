//! The terminal renderer for the engine↔renderer seam: it consumes
//! [`engine_events::EngineEvent`]s from an `EngineHandle` and draws them (the
//! markdown/ANSI stream, the ⏺ glyph margin, the spinner's byte counter +
//! "Reasoning…" cue, and tool call/result lines). This is the render *half* of
//! the old `CliStreamState`, now cleanly separated: the engine produces events,
//! this draws them, and nothing engine-side renders.
//!
//! `render` returns a [`RenderOutcome`] so the REPL loop knows when the engine
//! is blocked on a permission / question answer (which the loop collects and
//! sends back as an `EngineCommand`) and when the turn is finished.

use std::io::{self, Write};

use engine_events::{
    EngineEvent, HookProgressEvent, PermissionRequest, QuestionPromptRequest, RequestId,
    RetryEvent, ToolProgressEvent,
};

use crate::cli::format::{format_tool_call_start, format_tool_result};
use crate::render::{
    query_terminal_width, MarkdownStreamState, ResponseGlyphState, SpinnerRef, TerminalRenderer,
    DIM, RESET,
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
    renderer: TerminalRenderer,
    glyph: ResponseGlyphState,
    spinner: Option<SpinnerRef>,
    output_writer: Option<OutputSender>,
    /// `true` while inside a thinking block, so the "Reasoning…" spinner cue is
    /// raised once and lowered when real content resumes.
    thinking_active: bool,
    /// `true` when a staging overlay (the iocraft REPL) shows in-flight tool
    /// calls as running cards. In that mode the command header is NOT appended
    /// to scrollback here — the overlay renders it — while the finished result
    /// card still appends normally (the ordered scrollback sink). Off for
    /// one-shot / `--print`, where there is no overlay and the header appends.
    staging_overlay: bool,
    /// Tool-call arguments remembered from `ToolCall`, keyed by tool_use_id,
    /// so the completed card (`ToolResult`) can show what was requested — the
    /// result payload does not echo command/path/strings. Removed on result.
    tool_inputs: std::collections::HashMap<String, String>,
}

impl EngineEventRenderer {
    pub(crate) fn new(spinner: Option<SpinnerRef>, output_writer: Option<OutputSender>) -> Self {
        Self {
            markdown: MarkdownStreamState::default(),
            renderer: TerminalRenderer::new(),
            glyph: ResponseGlyphState::new(query_terminal_width()),
            spinner,
            output_writer,
            thinking_active: false,
            staging_overlay: false,
            tool_inputs: std::collections::HashMap::new(),
        }
    }

    /// Enable staging-overlay mode: suppress the command-header append (the
    /// overlay shows in-flight calls); the finished result card still appends.
    pub(crate) fn with_staging_overlay(mut self) -> Self {
        self.staging_overlay = true;
        self
    }

    fn write_out(&mut self, text: &str) {
        if let Some(writer) = self.output_writer.as_mut() {
            let _ = write!(writer, "{text}").and_then(|()| writer.flush());
        } else {
            let mut stdout = io::stdout();
            let _ = write!(stdout, "{text}").and_then(|()| stdout.flush());
        }
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
                self.pause_spinner();
                self.write_out(&format!(
                    "{DIM}  \u{27f3} retry {attempt}/{max_retries} \u{2014} {reason}{RESET}\n"
                ));
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

    /// Leave the thinking state (lower the "Reasoning…" cue) when real content
    /// resumes after a thinking block.
    fn end_thinking(&mut self) {
        if self.thinking_active {
            self.thinking_active = false;
            if let Some(s) = &self.spinner {
                s.set_thinking(false);
            }
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
                        let prefixed = self.glyph.apply(&rendered);
                        self.write_out(&prefixed);
                    }
                }
                RenderOutcome::Continue
            }
            EngineEvent::ThinkingDelta { text } => {
                if let Some(s) = &self.spinner {
                    s.add_response_bytes(text.len() as u32);
                }
                // Thinking is not surfaced in the transcript; the spinner's
                // "Reasoning…" mode is the only cue.
                if !self.thinking_active {
                    self.thinking_active = true;
                    if let Some(s) = &self.spinner {
                        s.set_thinking(true);
                    }
                }
                RenderOutcome::Continue
            }
            EngineEvent::ToolCall { id, name, input } => {
                self.end_thinking();
                if let Some(rendered) = self.markdown.flush(&self.renderer) {
                    let prefixed = self.glyph.apply(&rendered);
                    self.write_out(&prefixed);
                }
                // Remember the arguments so the completed card can show what was
                // requested — the ToolResult event/payload does not echo the
                // command (bash) or the path/strings (edit), they live only in
                // the call's input.
                self.tool_inputs.insert(id.clone(), input.clone());
                self.pause_spinner();
                // Staging overlay owns the command header (as a running card),
                // so suppress the scrollback append here to avoid showing it
                // twice. The glyph reset and spinner pause/resume still run —
                // they are streaming-cursor bookkeeping, independent of who
                // renders the header. Without an overlay (one-shot / --print)
                // the header appends as before.
                if !self.staging_overlay {
                    let line = format!("\n{}\n", format_tool_call_start(&name, &input));
                    self.write_out(&line);
                }
                // The tool line reset column 0; the next assistant text starts a
                // fresh ⏺-margined block.
                self.glyph.visible_col = 0;
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::ToolResult {
                id,
                name,
                output,
                is_error,
            } => {
                self.pause_spinner();
                let input = self.tool_inputs.remove(&id).unwrap_or_default();
                let line = format!("{}\n", format_tool_result(&name, &input, &output, is_error));
                self.write_out(&line);
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::ToolProgress(progress) => {
                self.pause_spinner();
                self.write_out(&format!("{}\n", format_tool_progress(&progress)));
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::HookProgress(ev) => {
                // Same pause/write/resume as every other event: writing through
                // `write_out` keeps this on stdout, behind the same lock the
                // spinner uses, so a hook line can no longer be torn in half
                // by a spinner frame.
                self.pause_spinner();
                self.write_out(&format!("{}\n", format_hook_progress(&ev)));
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::Retry(ev) => {
                self.render_retry(&ev);
                RenderOutcome::Continue
            }
            EngineEvent::Notice { text } => {
                if !text.is_empty() {
                    self.write_out(&format!("{text}\n"));
                }
                RenderOutcome::Continue
            }
            EngineEvent::Error { message } => {
                // Flush any partial assistant text first, then surface the error.
                if let Some(rendered) = self.markdown.flush(&self.renderer) {
                    let prefixed = self.glyph.apply(&rendered);
                    self.write_out(&prefixed);
                }
                self.pause_spinner();
                self.write_out(&format!("\n{message}\n"));
                RenderOutcome::Done
            }
            EngineEvent::TurnComplete(_) => {
                if let Some(rendered) = self.markdown.flush(&self.renderer) {
                    let prefixed = self.glyph.apply(&rendered);
                    self.write_out(&prefixed);
                }
                RenderOutcome::Done
            }
            EngineEvent::PermissionRequest { id, request } => {
                RenderOutcome::NeedPermission { id, request }
            }
            EngineEvent::QuestionRequest { id, request } => {
                RenderOutcome::NeedQuestion { id, request }
            }
            // No direct terminal effect: lifecycle/state/telemetry events. The
            // spinner already tracks progress from the deltas above.
            EngineEvent::TurnStarted { .. }
            | EngineEvent::State(_)
            | EngineEvent::ModelResolved { .. }
            | EngineEvent::Usage(_)
            | EngineEvent::PromptCache(_)
            | EngineEvent::AutoCompaction(_)
            | EngineEvent::ModelChanged { .. }
            | EngineEvent::PermissionModeChanged { .. } => RenderOutcome::Continue,
        }
    }
}

/// Format a live tool-progress event for the terminal. This is the render half
/// of the CLI `ToolExecutor`'s old `make_bash_progress_callback` /
/// `make_mcp_progress_callback`: the executor now reports structured data
/// (`ToolProgressEvent`) and the ANSI/glyph formatting lives here, above the
/// seam. Kept byte-identical to the pre-seam output for PTY parity.
fn format_tool_progress(progress: &ToolProgressEvent) -> String {
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
            format!("  {DIM}\u{27f3} {last_line}  ({total_lines} lines, {bytes_display}){RESET}")
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
                format!("  {DIM}\u{27f3} {msg}{status}{RESET}")
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

#[cfg(test)]
mod tests {
    use super::format_hook_progress;
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
}
