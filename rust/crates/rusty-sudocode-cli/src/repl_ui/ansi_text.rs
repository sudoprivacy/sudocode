//! UI adapters for structured text and external ANSI content.
//! The canvas owns terminal writes; control sequences are never replayed.
//! Foregrounds, backgrounds and attributes use the same spans as scrollback.

use crate::render::styled_text::StyledText;
use crossterm::style::{Attribute, Color as TerminalColor};
use iocraft::prelude::*;

#[derive(Default, Props)]
pub(super) struct AnsiTextProps {
    pub content: String,
    pub color: Option<Color>,
}

#[component]
pub(super) fn AnsiText(props: &AnsiTextProps) -> impl Into<AnyElement<'static>> {
    element! { MixedText(contents: contents(&StyledText::from_ansi(&props.content), props.color)) }
}

#[derive(Default, Props)]
pub(super) struct RichTextProps {
    pub content: std::sync::Arc<StyledText>,
    pub color: Option<Color>,
}

#[component]
pub(super) fn RichText(props: &RichTextProps) -> impl Into<AnyElement<'static>> {
    element! { MixedText(contents: contents(&props.content, props.color)) }
}

struct CachedPromptText {
    source: runtime::PromptText,
    width: usize,
    rendered: std::sync::Arc<StyledText>,
}

/// One derived document per mounted prompt, invalidated only by source or width.
/// Ref updates do not schedule a repaint (unlike State), so spinner ticks and
/// typing cannot reparse the plan or starve the terminal event loop.
#[derive(Default)]
pub(super) struct PromptTextCache(Option<CachedPromptText>);

impl PromptTextCache {
    /// Share the exact styled document between measurement and painting.
    pub fn render(
        &mut self,
        renderer: &crate::render::TerminalRenderer,
        source: Option<&runtime::PromptText>,
        width: usize,
    ) -> std::sync::Arc<StyledText> {
        let Some(source) = source else {
            self.0 = None;
            return Default::default();
        };
        if !self
            .0
            .as_ref()
            .is_some_and(|cached| cached.width == width && cached.source.same_source(source))
        {
            self.0 = Some(CachedPromptText {
                source: source.clone(),
                width,
                rendered: std::sync::Arc::new(renderer.render_prompt_text(source, width)),
            });
        }
        self.0.as_ref().unwrap().rendered.clone()
    }
}

fn contents(text: &StyledText, default_color: Option<Color>) -> Vec<MixedTextContent> {
    text.spans(0..text.text.len())
        .map(|(style, text)| {
            let has = |attr| style.attributes.has(attr);
            let mut span = MixedTextContent::default();
            span.text = text.replace('\t', " ").replace('\r', "");
            span.color = style.foreground_color.and_then(ui_color).or(default_color);
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
fn ui_color(color: TerminalColor) -> Option<Color> {
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
