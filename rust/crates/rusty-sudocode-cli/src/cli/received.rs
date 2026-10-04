//! Inbound peer presentation, shared by live echoes and transcript replay.
//!
//! Persisted turns contain the harness envelope as text, including when human
//! and peer inputs were batched. Decode only complete, top-level envelopes;
//! examples inside code blocks and malformed wrappers remain ordinary input.

use pulldown_cmark::{Event, Parser, Tag};

use crate::render::{
    styled_text::StyledText,
    text_layout::{truncate_to_width, wrap_ansi_to_width},
    theme, ColorSupport, TerminalRenderer, RESET,
};

struct ReceivedMessage<'a> {
    sender: String,
    body: &'a str,
    kind: &'static str,
}

enum PromptPart<'a> {
    Human(&'a str),
    Peer(ReceivedMessage<'a>),
}

fn envelope_header(line: &str) -> Option<(String, &'static str, &'static str)> {
    let (tag, attributes) = line.strip_prefix('<')?.strip_suffix('>')?.split_once(' ')?;
    let (tag, kind) = match tag {
        "mailbox-message" => ("mailbox-message", ""),
        "shutdown-request" => ("shutdown-request", "shutdown request"),
        "shutdown-response" => ("shutdown-response", "shutdown response"),
        "plan-approval-response" => ("plan-approval-response", "plan approval response"),
        _ => return None,
    };
    let (sender, rest) = attributes.strip_prefix("from=\"")?.split_once('"')?;
    if sender.is_empty() || sender.contains(['<', '>']) {
        return None;
    }
    if !rest.is_empty() {
        let request_id = rest.strip_prefix(" request-id=\"")?.strip_suffix('"')?;
        if request_id.contains(['"', '<', '>']) {
            return None;
        }
    }
    // Decode just the attribute escaping performed by the prompt composer.
    // Body entities belong to Markdown and are left to that renderer.
    let sender = sender
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    Some((sender, tag, kind))
}

fn prompt_parts(text: &str) -> Vec<PromptPart<'_>> {
    if !text.contains(" from=\"") {
        return vec![PromptPart::Human(text)];
    }
    let code_blocks: Vec<_> = Parser::new(text)
        .into_offset_iter()
        .filter_map(|(event, range)| {
            matches!(event, Event::Start(Tag::CodeBlock(_))).then_some(range)
        })
        .collect();
    let mut parts = Vec::new();
    let mut consumed = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if start < consumed
            || (start != 0 && !text[..start].ends_with("\n\n"))
            || code_blocks
                .iter()
                .any(|range| range.start >= consumed && range.contains(&start))
        {
            continue;
        }
        let Some((sender, tag, kind)) = envelope_header(line.trim_end_matches('\n')) else {
            continue;
        };
        let closing = format!("\n</{tag}>");
        let Some(body_len) = text[offset..].find(&closing) else {
            continue;
        };
        let end = offset + body_len + closing.len();
        if end != text.len() && !text[end..].starts_with("\n\n") {
            continue;
        }
        let human = text[consumed..start].trim_matches('\n');
        if !human.is_empty() {
            parts.push(PromptPart::Human(human));
        }
        parts.push(PromptPart::Peer(ReceivedMessage {
            sender,
            body: &text[offset..offset + body_len],
            kind,
        }));
        consumed = end;
    }
    let remaining = text[consumed..].trim_matches('\n');
    if !remaining.is_empty() {
        parts.push(PromptPart::Human(remaining));
    }
    parts
}

impl ReceivedMessage<'_> {
    fn header(&self) -> String {
        let sender = StyledText::from_ansi(&self.sender)
            .text
            .replace(['\r', '\n', '\t'], " ");
        let suffix = if self.kind.is_empty() {
            String::new()
        } else {
            format!(" · {}", self.kind)
        };
        format!(
            "{RESET}{}← {RESET}{}{sender}{RESET}{}{suffix}{RESET}",
            theme().muted_fg(),
            theme().peer_sender_fg(),
            theme().muted_fg(),
        )
    }

    fn render(&self, width: usize, renderer: &TerminalRenderer) -> String {
        let mut lines = vec![truncate_to_width(&self.header(), width)];
        let body_width = width.saturating_sub(2).max(1);
        let body = renderer.render_markdown_with_width(self.body, body_width);
        // Paragraphs, code and tables share the existing Markdown renderer.
        // Wrap remaining long prose with the same grapheme/style layout used
        // by tool frames, retaining the gutter on every physical row.
        for line in body.trim_end_matches('\n').split('\n') {
            for row in wrap_ansi_to_width(line, body_width) {
                lines.push(format!("{}│{RESET} {row}", theme().muted_fg()));
            }
        }
        let output = lines.join("\n");
        if renderer.color_support() == ColorSupport::NoColor {
            StyledText::from_ansi(&output).text
        } else {
            output
        }
    }
}

/// Render a whole persisted or live prompt, preserving mixed-input order.
pub(crate) fn render_prompt(text: &str, width: usize, renderer: &TerminalRenderer) -> String {
    prompt_parts(text)
        .into_iter()
        .map(|part| match part {
            PromptPart::Human(text) => super::format::format_input_echo(text, width).0,
            PromptPart::Peer(message) => message.render(width, renderer),
        })
        .map(|part| part.trim_end_matches('\n').to_string())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Prepare a bounded, one-line preview once on arrival, never parse Markdown
/// or the full envelope in the input repaint path. The overlay clips this to
/// its current width; the complete body remains in the queued prompt.
pub(crate) fn queued_preview(prompt: &str) -> String {
    let Some(PromptPart::Peer(message)) = prompt_parts(prompt).into_iter().next() else {
        return String::new();
    };
    let first = message
        .body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let first = StyledText::from_ansi(first).text.replace(['\r', '\t'], " ");
    let preview = truncate_to_width(&format!("{}: {first}", message.header()), 512);
    if ColorSupport::detect() == ColorSupport::NoColor {
        StyledText::from_ansi(&preview).text
    } else {
        preview
    }
}
