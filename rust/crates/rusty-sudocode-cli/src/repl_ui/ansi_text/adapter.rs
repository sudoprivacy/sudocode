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
    pub width: Size,
}

#[component]
pub(in crate::repl_ui) fn AnsiText(props: &AnsiTextProps) -> impl Into<AnyElement<'static>> {
    let text = StyledText::from_ansi_with_style(&props.content, props.initial_style);
    text_element(contents(&text), TextWrap::Wrap, props.width)
}

#[derive(Default, Props)]
pub(in crate::repl_ui) struct RichTextProps {
    pub content: std::sync::Arc<StyledText>,
    pub wrap: TextWrap,
    pub width: Size,
}

#[component]
pub(in crate::repl_ui) fn RichText(props: &RichTextProps) -> impl Into<AnyElement<'static>> {
    text_element(contents(&props.content), props.wrap, props.width)
}

fn text_element(
    contents: Vec<MixedTextContent>,
    wrap: TextWrap,
    width: Size,
) -> AnyElement<'static> {
    match width {
        Size::Length(width) => element! { WidthText(contents, wrap, width) }.into(),
        _ => element! { MixedText(contents, wrap) }.into(),
    }
}

#[derive(Default, Props)]
struct WidthTextProps {
    contents: Vec<MixedTextContent>,
    wrap: TextWrap,
    width: u32,
}

/// Give the text leaf its actual width before Taffy measures its height.
#[derive(Default)]
struct WidthText(MixedText);

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
        let mut style = LayoutStyle::default();
        style.width = Size::Length(props.width);
        updater.set_layout_style(style.into());
        let mut text_props = MixedTextProps::default();
        text_props.contents = std::mem::take(&mut props.contents);
        text_props.wrap = props.wrap;
        Component::update(&mut self.0, &mut text_props, hooks, updater);
    }

    fn draw(&mut self, drawer: &mut ComponentDrawer<'_>) {
        Component::draw(&mut self.0, drawer);
    }
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
