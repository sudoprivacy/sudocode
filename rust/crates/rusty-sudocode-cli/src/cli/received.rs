//! Inbound peer presentation, shared by live echoes and transcript replay.
//!
//! Persisted turns contain the harness envelope as text, including when human
//! and peer inputs were batched. Decode only complete, top-level envelopes;
//! examples inside code blocks and malformed wrappers remain ordinary input.

use pulldown_cmark::{Event, Parser, Tag};

use crate::render::{
    left_frame, styled_text::StyledText, text_layout::truncate_to_width, theme, ColorSupport,
    TerminalRenderer, RESET,
};

struct EnvelopeHeader {
    sender: String,
    summary: Option<String>,
    tag: &'static str,
    kind: &'static str,
}

struct ReceivedMessage<'a> {
    header: EnvelopeHeader,
    body: &'a str,
}

enum PromptPart<'a> {
    Human(&'a str),
    Peer(ReceivedMessage<'a>),
}

fn envelope_header(line: &str) -> Option<EnvelopeHeader> {
    let (tag, attributes) = line.strip_prefix('<')?.strip_suffix('>')?.split_once(' ')?;
    let (tag, kind) = match tag {
        "mailbox-message" => ("mailbox-message", ""),
        "shutdown-request" => ("shutdown-request", "shutdown request"),
        "shutdown-response" => ("shutdown-response", "shutdown response"),
        "plan-approval-response" => ("plan-approval-response", "plan approval response"),
        _ => return None,
    };
    let (sender, mut rest) = attributes.strip_prefix("from=\"")?.split_once('"')?;
    if sender.is_empty() || sender.contains(['<', '>']) {
        return None;
    }
    let mut has_request_id = false;
    let mut summary = None;
    while !rest.is_empty() {
        let (name, value) = rest.strip_prefix(' ')?.split_once("=\"")?;
        let (value, remaining) = value.split_once('"')?;
        if value.contains(['<', '>']) {
            return None;
        }
        match name {
            "request-id" if !has_request_id => has_request_id = true,
            "summary" if summary.is_none() => summary = Some(decode_attribute(value)),
            _ => return None,
        }
        rest = remaining;
    }
    Some(EnvelopeHeader {
        sender: decode_attribute(sender),
        summary,
        tag,
        kind,
    })
}

fn decode_attribute(value: &str) -> String {
    // Decode just the attribute escaping performed by the prompt composer.
    // Body entities belong to Markdown and are left to that renderer.
    value
        .replace("&#10;", "\n")
        .replace("&#13;", "\r")
        .replace("&#9;", "\t")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
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
        let Some(header) = envelope_header(line.trim_end_matches('\n')) else {
            continue;
        };
        let closing = format!("\n</{}>", header.tag);
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
            header,
            body: &text[offset..offset + body_len],
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
    fn title(&self) -> String {
        let sender = StyledText::from_ansi(&self.header.sender)
            .text
            .replace(['\r', '\n', '\t'], " ");
        let suffix = if self.header.kind.is_empty() {
            String::new()
        } else {
            format!(" · {}", self.header.kind)
        };
        format!(
            "{RESET}{}Message from {RESET}{}{sender}{RESET}{}{suffix}{RESET}",
            theme().muted_fg(),
            theme().peer_sender_fg(),
            theme().muted_fg(),
        )
    }

    fn render(&self, width: usize, renderer: &TerminalRenderer) -> String {
        let body = renderer.render_markdown_with_width(self.body, left_frame::body_width(width));
        let summary = self
            .header
            .summary
            .as_deref()
            .filter(|summary| !summary.trim().is_empty())
            .map(super::format::summary_preview);
        let output = left_frame::render(
            &self.title(),
            summary
                .as_deref()
                .into_iter()
                .chain(std::iter::once(body.trim_end_matches('\n'))),
            width,
            theme().muted,
        );
        if renderer.color_support() == ColorSupport::NoColor {
            StyledText::from_ansi(&output).text
        } else {
            output
        }
    }
}

/// Render a whole persisted or live prompt, preserving mixed-input order.
pub(crate) fn render_prompt(text: &str, width: usize, renderer: &TerminalRenderer) -> String {
    let mut output = String::new();
    for part in prompt_parts(text) {
        let rendered = match part {
            PromptPart::Human(text) => super::format::format_input_echo(text, width).0,
            PromptPart::Peer(message) => message.render(width, renderer),
        };
        crate::render::layout_policy::LayoutPolicy::append_block(&mut output, &rendered);
    }
    output
}

/// Prepare a bounded, one-line preview once on arrival, never parse Markdown
/// or the full envelope in the input repaint path. The overlay clips this to
/// its current width; the complete body remains in the queued prompt.
pub(crate) fn queued_preview(prompt: &str) -> String {
    let Some(PromptPart::Peer(message)) = prompt_parts(prompt).into_iter().next() else {
        return String::new();
    };
    let first = message
        .header
        .summary
        .as_deref()
        .filter(|summary| !summary.trim().is_empty())
        .unwrap_or(message.body)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let first = StyledText::from_ansi(first).text.replace(['\r', '\t'], " ");
    let preview = truncate_to_width(&format!("{}: {first}", message.title()), 512);
    if ColorSupport::detect() == ColorSupport::NoColor {
        StyledText::from_ansi(&preview).text
    } else {
        preview
    }
}
