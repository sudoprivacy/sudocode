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
}

#[component]
pub(super) fn RichText(props: &RichTextProps) -> impl Into<AnyElement<'static>> {
    element! { MixedText(contents: contents(&props.content, None)) }
}

#[derive(Default, Props)]
pub(super) struct PromptTextViewProps {
    pub content: Option<runtime::PromptText>,
    pub width: usize,
}

struct PromptTextCache {
    source: runtime::PromptText,
    width: usize,
    rendered: std::sync::Arc<StyledText>,
}

/// One derived document per mounted prompt, invalidated only by source or width.
/// Ref updates do not schedule a repaint (unlike State), so spinner ticks and
/// typing cannot reparse the plan or starve the terminal event loop.
#[component]
pub(super) fn PromptTextView(
    props: &PromptTextViewProps,
    mut hooks: Hooks,
) -> impl Into<AnyElement<'static>> {
    let renderer = hooks.use_ref(crate::render::TerminalRenderer::new);
    let mut cache = hooks.use_ref_default::<Option<PromptTextCache>>();
    let content = props.content.as_ref().map(|source| {
        let mut cache = cache.write();
        if !cache
            .as_ref()
            .is_some_and(|cached| cached.width == props.width && cached.source.same_source(source))
        {
            *cache = Some(PromptTextCache {
                source: source.clone(),
                width: props.width,
                rendered: std::sync::Arc::new(
                    renderer.read().render_prompt_text(source, props.width),
                ),
            });
        }
        cache.as_ref().unwrap().rendered.clone()
    });
    element! {
        View(flex_direction: FlexDirection::Column) {
            #(content.map(|content| element! { RichText(content) }))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_text_preserves_bold_and_dim_and_resets_both() {
        let text = StyledText::from_ansi("\x1b[1;2mBoldDimSample\x1b[0mPlainSample");
        let spans = contents(&text, Some(Color::DarkGrey));
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "BoldDimSample");
        assert_eq!(spans[0].color, Some(Color::DarkGrey));
        assert_eq!(spans[0].weight, Weight::Bold);
        assert!(spans[0].dim);
        assert!(!spans[0].strikethrough);
        assert_eq!(spans[1].text, "PlainSample");
        assert_eq!(spans[1].color, Some(Color::DarkGrey));
        assert_eq!(spans[1].weight, Weight::Normal);
        assert!(!spans[1].dim);
        assert!(!spans[1].strikethrough);
    }

    #[test]
    fn markdown_strikethrough_reaches_rich_text_and_resets_without_color_leaks() {
        use crate::render::{ColorSupport, TerminalRenderer};

        for support in [
            ColorSupport::TrueColor,
            ColorSupport::Ansi256,
            ColorSupport::NoColor,
        ] {
            let renderer = TerminalRenderer::new().with_color_support(support);
            let rendered =
                renderer.render_markdown_with_width("PlainBefore ~~ThemeStrike~~ PlainAfter", 100);
            let text = StyledText::from_ansi(&rendered);
            assert_eq!(text.text.trim_end(), "PlainBefore ThemeStrike PlainAfter");
            let spans = contents(&text, None);
            if support == ColorSupport::NoColor {
                assert!(!rendered.contains('\x1b'));
            } else {
                let strike = text
                    .spans(0..text.text.len())
                    .find(|(_, text)| *text == "ThemeStrike")
                    .unwrap();
                assert!(strike.0.attributes.has(Attribute::CrossedOut));
                let strike = spans
                    .iter()
                    .find(|span| span.text == "ThemeStrike")
                    .unwrap();
                assert!(strike.strikethrough);
            }
            for (style, span_text) in text.spans(0..text.text.len()) {
                assert_eq!(
                    style.attributes.has(Attribute::CrossedOut),
                    support != ColorSupport::NoColor && span_text == "ThemeStrike"
                );
            }
            for span in spans {
                assert_eq!(
                    span.strikethrough,
                    support != ColorSupport::NoColor && span.text == "ThemeStrike"
                );
                assert_eq!(span.color, None);
                assert_eq!(span.background_color, None);
                assert_eq!(span.weight, Weight::Normal);
                assert!(!span.dim);
                assert!(!span.italic);
                assert_eq!(span.decoration, TextDecoration::None);
                assert!(!span.invert);
            }
        }
    }

    #[test]
    fn queued_overlay_preserves_dim_without_bold_or_color() {
        for is_human in [true, false] {
            for width in [100, 78] {
                let overlay = super::super::render_pending_overlay(
                    &[super::super::PendingItem::QueuedMessage {
                        display: "StyleQueuedMarker".into(),
                        is_human,
                    }],
                    40,
                    width,
                );
                let text = StyledText::from_ansi(&overlay);
                let spans = contents(&text, None);
                assert!(text.text.starts_with("↳ queued: "));
                assert!(!spans.is_empty());
                for span in spans {
                    assert_eq!(span.color, None);
                    assert_eq!(span.weight, Weight::Normal);
                    assert!(span.dim);
                    assert!(!span.strikethrough);
                }
            }
        }
    }

    #[test]
    fn completed_todo_preserves_dim_and_strikethrough_without_leaking() {
        let todos = [runtime::Todo {
            content: "Finished parser".into(),
            status: runtime::TodoStatus::Completed,
            active_form: "Finishing parser".into(),
        }];
        let panel = super::super::render_todo_panel(&todos, 40);
        let text = StyledText::from_ansi(&format!("{panel}\nPlainSample"));
        let spans = contents(&text, Some(Color::DarkGrey));
        let label = spans
            .iter()
            .find(|span| span.text == "Finished parser")
            .unwrap();
        assert_eq!(label.color, Some(Color::DarkGrey));
        assert_eq!(label.weight, Weight::Normal);
        assert!(label.dim);
        assert!(label.strikethrough);
        let plain = spans.last().unwrap();
        assert!(plain.text.ends_with("PlainSample"));
        assert_eq!(plain.color, Some(Color::DarkGrey));
        assert_eq!(plain.weight, Weight::Normal);
        assert!(!plain.dim);
        assert!(!plain.strikethrough);
    }
}
