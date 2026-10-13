mod adapter;

pub(super) use adapter::{AnsiText, RichText};

#[cfg(test)]
use crate::render::styled_text::StyledText;
#[cfg(test)]
use adapter::contents;
#[cfg(test)]
use crossterm::style::{Attribute, Color as TerminalColor};
#[cfg(test)]
use iocraft::prelude::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rich_text_preserves_bold_and_dim_and_resets_both() {
        for intensity in ["1;2", "2;1"] {
            for reset in ["22", "0"] {
                let text = StyledText::from_ansi(&format!(
                    "\x1b[{intensity}mBoldDimSample\x1b[{reset}mPlainSample"
                ));
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

                let content = std::sync::Arc::new(StyledText::from_ansi(&format!(
                    "\x1b[90m\x1b[{intensity}mBoldDimSample\x1b[{reset}m\x1b[90mPlainSample \
                     \x1b[1mBoldSample\x1b[22m \x1b[2mDimSample\x1b[22m IntensityResetSample"
                )));
                let canvas = element! { RichText(content) }.render(Some(100));
                let mut wire = Vec::new();
                canvas.write_ansi(&mut wire).unwrap();
                let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
                assert_eq!(
                    rendered.text.trim_end(),
                    "BoldDimSamplePlainSample BoldSample DimSample IntensityResetSample"
                );
                for (label, bold, dim) in [
                    ("BoldDimSample", true, true),
                    ("PlainSample", false, false),
                    ("BoldSample", true, false),
                    ("DimSample", false, true),
                    ("IntensityResetSample", false, false),
                ] {
                    let start = rendered.text.rfind(label).unwrap();
                    for (style, _) in rendered.spans(start..start + label.len()) {
                        assert_eq!(style.foreground_color, Some(TerminalColor::DarkGrey));
                        assert_eq!(style.attributes.has(Attribute::Bold), bold, "{label}");
                        assert_eq!(style.attributes.has(Attribute::Dim), dim, "{label}");
                    }
                }
            }
        }
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
