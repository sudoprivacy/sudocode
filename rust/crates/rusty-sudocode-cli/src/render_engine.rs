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

use engine_events::{EngineEvent, PermissionRequest, QuestionPromptRequest, RequestId};

use crate::cli::format::{format_tool_call_start, format_tool_result};
use crate::render::{
    query_terminal_width, MarkdownStreamState, ResponseGlyphState, SpinnerRef, TerminalRenderer,
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
        }
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
            EngineEvent::ToolCall { name, input, .. } => {
                self.end_thinking();
                if let Some(rendered) = self.markdown.flush(&self.renderer) {
                    let prefixed = self.glyph.apply(&rendered);
                    self.write_out(&prefixed);
                }
                self.pause_spinner();
                let line = format!("\n{}\n", format_tool_call_start(&name, &input));
                self.write_out(&line);
                // The tool line reset column 0; the next assistant text starts a
                // fresh ⏺-margined block.
                self.glyph.visible_col = 0;
                self.resume_spinner();
                RenderOutcome::Continue
            }
            EngineEvent::ToolResult {
                name,
                output,
                is_error,
                ..
            } => {
                self.pause_spinner();
                let line = format!("{}\n", format_tool_result(&name, &output, is_error));
                self.write_out(&line);
                self.resume_spinner();
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
