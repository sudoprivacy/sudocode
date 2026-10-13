//! UI adapters for structured text and external ANSI content.
//! The canvas owns terminal writes; control sequences are never replayed.
//! Foregrounds, backgrounds and attributes use the same spans as scrollback.

use crate::render::styled_text::StyledText;
use crossterm::style::{Attribute, Color as TerminalColor, ContentStyle};
use iocraft::prelude::*;

#[derive(Default, Props)]
pub(in crate::repl_ui) struct AnsiTextProps {
    pub content: String,
    pub initial_style: ContentStyle,
}

#[component]
pub(in crate::repl_ui) fn AnsiText(props: &AnsiTextProps) -> impl Into<AnyElement<'static>> {
    let text = StyledText::from_ansi_with_style(&props.content, props.initial_style);
    element! { MixedText(contents: contents(&text)) }
}

#[derive(Default, Props)]
pub(in crate::repl_ui) struct RichTextProps {
    pub content: std::sync::Arc<StyledText>,
}

#[component]
pub(in crate::repl_ui) fn RichText(props: &RichTextProps) -> impl Into<AnyElement<'static>> {
    element! { MixedText(contents: contents(&props.content)) }
}

pub(super) fn contents(text: &StyledText) -> Vec<MixedTextContent> {
    text.spans(0..text.text.len())
        .map(|(style, text)| {
            let has = |attr| style.attributes.has(attr);
            let mut span = MixedTextContent::default();
            span.text = text.replace('\t', " ").replace('\r', "");
            span.color = style.foreground_color.and_then(ui_color);
            span.background_color = style.background_color.and_then(ui_color);
            span.weight = if has(Attribute::Bold) {
                Weight::Bold
            } else {
                Weight::Normal
            };
            span.dim = has(Attribute::Dim);
            span.italic = has(Attribute::Italic);
            span.decoration = if has(Attribute::Underlined) {
                TextDecoration::Underline
            } else {
                TextDecoration::None
            };
            span.invert = has(Attribute::Reverse);
            span.strikethrough = has(Attribute::CrossedOut);
            span
        })
        .collect()
}

// iocraft and the CLI use different crossterm versions. Convert at the UI boundary.
pub(in crate::repl_ui) fn ui_color(color: TerminalColor) -> Option<Color> {
    Some(match color {
        TerminalColor::Reset => return None,
        TerminalColor::Black => Color::Black,
        TerminalColor::DarkGrey => Color::DarkGrey,
        TerminalColor::Red => Color::Red,
        TerminalColor::DarkRed => Color::DarkRed,
        TerminalColor::Green => Color::Green,
        TerminalColor::DarkGreen => Color::DarkGreen,
        TerminalColor::Yellow => Color::Yellow,
        TerminalColor::DarkYellow => Color::DarkYellow,
        TerminalColor::Blue => Color::Blue,
        TerminalColor::DarkBlue => Color::DarkBlue,
        TerminalColor::Magenta => Color::Magenta,
        TerminalColor::DarkMagenta => Color::DarkMagenta,
        TerminalColor::Cyan => Color::Cyan,
        TerminalColor::DarkCyan => Color::DarkCyan,
        TerminalColor::White => Color::White,
        TerminalColor::Grey => Color::Grey,
        TerminalColor::Rgb { r, g, b } => Color::Rgb { r, g, b },
        TerminalColor::AnsiValue(n) => Color::AnsiValue(n),
    })
}

