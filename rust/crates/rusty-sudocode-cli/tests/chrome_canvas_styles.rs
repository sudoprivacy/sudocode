//! Optional production formatter-to-Canvas boundary checks; not `ConPTY` acceptance.

#[path = "../src/render"]
mod render {
    #[path = "ansi_codes.rs"]
    mod ansi_codes;
    pub use ansi_codes::{BOLD, DIM, RESET};
    #[path = "styled_text.rs"]
    pub(crate) mod styled_text;
    // Include the production truncator without exercising unrelated wrapping helpers.
    #[allow(dead_code)]
    #[path = "text_layout.rs"]
    pub(crate) mod text_layout;
}

#[path = "../src/repl_ui"]
mod repl_ui {
    #[path = "ansi_text"]
    mod ansi_text {
        #[path = "adapter.rs"]
        mod adapter;
        pub(super) use adapter::{AnsiText, RichText};
    }
    #[path = "chrome_labels.rs"]
    mod chrome_labels;

    use crate::render::styled_text::StyledText;
    use crossterm::style::{Attribute, Color as TerminalColor};
    use iocraft::prelude::*;

    fn assert_style(
        rendered: &StyledText,
        label: &str,
        color: Option<TerminalColor>,
        bold: bool,
        dim: bool,
        strike: bool,
    ) {
        let start = rendered.text.find(label).unwrap_or_else(|| {
            panic!("missing {label:?} in {:?}", rendered.text);
        });
        for (style, _) in rendered.spans(start..start + label.len()) {
            assert_eq!(style.foreground_color, color, "{label}");
            assert_eq!(style.background_color, None, "{label}");
            assert_eq!(style.attributes.has(Attribute::Bold), bold, "{label}");
            assert_eq!(style.attributes.has(Attribute::Dim), dim, "{label}");
            assert_eq!(
                style.attributes.has(Attribute::CrossedOut),
                strike,
                "{label}"
            );
        }
    }

    #[test]
    fn queued_labels_reach_canvas_with_dim_and_reset() {
        for width in [100, 68, 78] {
            for display in [
                "❯ HumanQueuedMarker\nIgnoredSecondLine",
                "Message from peer: PeerQueuedMarker\nIgnoredSecondLine",
            ] {
                let label = chrome_labels::queued_label(display);
                assert_eq!(
                    label,
                    format!(
                        "\x1b[2m↳ queued: {}\x1b[0m",
                        display.lines().next().unwrap()
                    )
                );
                let content = crate::render::text_layout::truncate_to_width(&label, width);
                let canvas = element! {
                    ansi_text::AnsiText(content: format!("{content}\nPlainAfter"))
                }
                .render(Some(width));
                let mut wire = Vec::new();
                canvas.write_ansi(&mut wire).unwrap();
                let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
                assert_eq!(
                    rendered.text.replace("\r\n", "\n").trim_end(),
                    format!("↳ queued: {}\nPlainAfter", display.lines().next().unwrap())
                );
                assert_style(&rendered, "↳ queued: ", None, false, true, false);
                assert_style(
                    &rendered,
                    display.lines().next().unwrap(),
                    None,
                    false,
                    true,
                    false,
                );
                assert_style(&rendered, "PlainAfter", None, false, false, false);
            }
        }
    }

    #[test]
    fn truncated_queued_labels_keep_dim_and_reset_at_each_width() {
        for width in [100, 68, 78] {
            let label = chrome_labels::queued_label(&format!("QueueMarker {}", "x".repeat(120)));
            let content = crate::render::text_layout::truncate_to_width(&label, width);
            assert_eq!(crate::render::text_layout::display_width(&content), width);
            let expected = StyledText::from_ansi(&content).text;
            assert!(expected.ends_with('…'));
            let canvas = element! {
                ansi_text::AnsiText(content: format!("{content}\nPlainAfter"))
            }
            .render(Some(width));
            let mut wire = Vec::new();
            canvas.write_ansi(&mut wire).unwrap();
            let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
            assert_eq!(
                rendered.text.replace("\r\n", "\n").trim_end(),
                format!("{expected}\nPlainAfter")
            );
            assert_style(&rendered, "↳ queued: QueueMarker", None, false, true, false);
            assert_style(&rendered, "PlainAfter", None, false, false, false);
        }
    }

    #[test]
    fn todo_labels_and_icons_reach_canvas_without_style_leaks() {
        for width in [100, 68, 78] {
            let mut lines = Vec::new();
            for (status, content, active_form, expected) in [
                (
                    runtime::TodoStatus::Completed,
                    "CompletedMarker",
                    "UnusedCompletedActiveForm",
                    "  \x1b[32m✓\x1b[0m \x1b[2m\x1b[9mCompletedMarker\x1b[29m\x1b[0m",
                ),
                (
                    runtime::TodoStatus::InProgress,
                    "UnusedProgressContent",
                    "ProgressMarker",
                    "  \x1b[36m■\x1b[0m \x1b[1mProgressMarker\x1b[0m",
                ),
                (
                    runtime::TodoStatus::Pending,
                    "PendingMarker",
                    "UnusedPendingActiveForm",
                    "  □ PendingMarker",
                ),
            ] {
                let todo = runtime::Todo {
                    status,
                    content: content.into(),
                    active_form: active_form.into(),
                };
                let line = chrome_labels::todo_item(&todo, "\x1b[32m", "\x1b[36m");
                assert_eq!(line, expected);
                lines.push(format!("{line} PlainAfter{content}"));
            }
            let canvas =
                element! { ansi_text::AnsiText(content: lines.join("\n")) }.render(Some(width));
            let mut wire = Vec::new();
            canvas.write_ansi(&mut wire).unwrap();
            let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
            assert_eq!(rendered.text.replace("\r\n", "\n").trim_end(), "  ✓ CompletedMarker PlainAfterCompletedMarker\n  ■ ProgressMarker PlainAfterUnusedProgressContent\n  □ PendingMarker PlainAfterPendingMarker");
            assert_style(
                &rendered,
                "✓",
                Some(TerminalColor::DarkGreen),
                false,
                false,
                false,
            );
            assert_style(
                &rendered,
                "■",
                Some(TerminalColor::DarkCyan),
                false,
                false,
                false,
            );
            assert_style(&rendered, "□", None, false, false, false);
            assert_style(&rendered, "CompletedMarker", None, false, true, true);
            assert_style(&rendered, "ProgressMarker", None, true, false, false);
            assert_style(&rendered, "PendingMarker", None, false, false, false);
            for suffix in ["CompletedMarker", "UnusedProgressContent", "PendingMarker"] {
                assert_style(
                    &rendered,
                    &format!("PlainAfter{suffix}"),
                    None,
                    false,
                    false,
                    false,
                );
            }
        }
    }

    #[test]
    fn combined_intensity_and_both_reset_forms_reach_rich_text_canvas() {
        for width in [100, 68, 78] {
            let content = std::sync::Arc::new(StyledText::from_ansi(
                "\x1b[48;2;1;3;4mNoOperandLeak\x1b[0mPlainAfter",
            ));
            let canvas = element! { ansi_text::RichText(content) }.render(Some(width));
            let mut wire = Vec::new();
            canvas.write_ansi(&mut wire).unwrap();
            let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
            assert_eq!(
                rendered.text.replace("\r\n", "\n").trim_end(),
                "NoOperandLeakPlainAfter"
            );
            for (style, _) in rendered.spans(0.."NoOperandLeak".len()) {
                assert_eq!(
                    style.background_color,
                    Some(TerminalColor::Rgb { r: 1, g: 3, b: 4 })
                );
                assert_eq!(style.foreground_color, None);
                for attribute in [
                    Attribute::Bold,
                    Attribute::Dim,
                    Attribute::Italic,
                    Attribute::Underlined,
                    Attribute::CrossedOut,
                ] {
                    assert!(!style.attributes.has(attribute));
                }
            }
            assert_style(&rendered, "PlainAfter", None, false, false, false);
            for (style, _) in rendered.spans("NoOperandLeak".len()..rendered.text.len()) {
                assert!(!style.attributes.has(Attribute::Italic));
                assert!(!style.attributes.has(Attribute::Underlined));
            }
            for intensity in ["1;2", "2;1"] {
                for reset in ["22", "0"] {
                    let content = std::sync::Arc::new(StyledText::from_ansi(&format!(
                        "\x1b[90m\x1b[{intensity}mBothMarker\x1b[{reset}m\x1b[90mPlainMarker \
                         \x1b[1mBoldMarker\x1b[22m \x1b[2mDimMarker\x1b[22m ResetMarker"
                    )));
                    let canvas = element! { ansi_text::RichText(content) }.render(Some(width));
                    let mut wire = Vec::new();
                    canvas.write_ansi(&mut wire).unwrap();
                    let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
                    assert_eq!(
                        rendered.text.replace("\r\n", "\n").trim_end(),
                        "BothMarkerPlainMarker BoldMarker DimMarker ResetMarker"
                    );
                    for (label, bold, dim) in [
                        ("BothMarker", true, true),
                        ("PlainMarker", false, false),
                        ("BoldMarker", true, false),
                        ("DimMarker", false, true),
                        ("ResetMarker", false, false),
                    ] {
                        assert_style(
                            &rendered,
                            label,
                            Some(TerminalColor::DarkGrey),
                            bold,
                            dim,
                            false,
                        );
                    }
                }
            }
        }
    }
}
