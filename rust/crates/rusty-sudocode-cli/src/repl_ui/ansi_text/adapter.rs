//! UI adapters for structured text and external ANSI content.
//! The canvas owns terminal writes; control sequences are never replayed.
//! Foregrounds, backgrounds and attributes use the same spans as scrollback.

use std::sync::Arc;

use crate::render::styled_text::StyledText;
use crossterm::style::{Attribute, Color as TerminalColor, ContentStyle};
use iocraft::prelude::*;

#[derive(Default, Props)]
pub(in crate::repl_ui) struct AnsiTextProps {
    pub content: String,
    pub initial_style: ContentStyle,
    pub width: Size,
}

#[component]
pub(in crate::repl_ui) fn AnsiText(props: &AnsiTextProps) -> impl Into<AnyElement<'static>> {
    let text = StyledText::from_ansi_with_style(&props.content, props.initial_style);
    text_element(
        TextSource::Mixed(contents(&text)),
        TextWrap::Wrap,
        props.width,
    )
}

#[derive(Default, Props)]
pub(in crate::repl_ui) struct RichTextProps {
    pub content: Arc<StyledText>,
    pub wrap: TextWrap,
    pub width: Size,
}

#[component]
pub(in crate::repl_ui) fn RichText(props: &RichTextProps) -> impl Into<AnyElement<'static>> {
    text_element(
        TextSource::Styled(Arc::clone(&props.content)),
        props.wrap,
        props.width,
    )
}

// Structured sources are immutable and already shared by their layout owner.
// Keep their identity through the adapter instead of cloning every span on input.
enum TextSource {
    Mixed(Vec<MixedTextContent>),
    Styled(Arc<StyledText>),
}

impl Default for TextSource {
    fn default() -> Self {
        Self::Mixed(Vec::new())
    }
}

fn text_element(source: TextSource, wrap: TextWrap, width: Size) -> AnyElement<'static> {
    element! { WidthText(source, wrap, width) }.into()
}

#[derive(Default, Props)]
struct WidthTextProps {
    source: TextSource,
    wrap: TextWrap,
    width: Size,
}

/// Give the text leaf its width before measurement and retain unchanged layout.
#[derive(Default)]
struct WidthText {
    text: MixedText,
    source: Option<Arc<StyledText>>,
    width: Option<u32>,
    wrap: TextWrap,
}

impl Component for WidthText {
    type Props<'a> = WidthTextProps;

    fn new(_props: &Self::Props<'_>) -> Self {
        Self::default()
    }

    fn update(
        &mut self,
        props: &mut Self::Props<'_>,
        hooks: Hooks,
        updater: &mut ComponentUpdater,
    ) {
        let width = match props.width {
            Size::Length(width) => Some(width),
            _ => None,
        };
        if self.width != width {
            let mut style = LayoutStyle::default();
            if let Some(width) = width {
                style.width = Size::Length(width);
            }
            updater.set_layout_style(style.into());
            self.width = width;
        }
        let unchanged = match &props.source {
            TextSource::Styled(source) => self
                .source
                .as_ref()
                .is_some_and(|previous| Arc::ptr_eq(previous, source)),
            TextSource::Mixed(_) => false,
        };
        if unchanged && self.wrap == props.wrap {
            // Replacing a measure function or even assigning an identical Taffy
            // style marks the leaf dirty and rewraps the body on every keystroke.
            // Width changes above still invalidate layout with the existing text.
            return;
        }
        let mut text_props = MixedTextProps::default();
        text_props.contents = match &mut props.source {
            TextSource::Styled(source) => {
                self.source = Some(Arc::clone(source));
                contents(source)
            }
            TextSource::Mixed(spans) => {
                self.source = None;
                std::mem::take(spans)
            }
        };
        text_props.wrap = props.wrap;
        self.wrap = props.wrap;
        Component::update(&mut self.text, &mut text_props, hooks, updater);
    }

    fn draw(&mut self, drawer: &mut ComponentDrawer<'_>) {
        Component::draw(&mut self.text, drawer);
    }
}

pub(super) fn contents(text: &StyledText) -> Vec<MixedTextContent> {
    text.rich_spans(0..text.text.len())
        .map(|(style, text, hyperlink)| {
            let has = |attr| style.attributes.has(attr);
            let mut span = MixedTextContent::default();
            span.text = text.replace('\t', " ").replace('\r', "");
            span.hyperlink = hyperlink.map(ToString::to_string);
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
