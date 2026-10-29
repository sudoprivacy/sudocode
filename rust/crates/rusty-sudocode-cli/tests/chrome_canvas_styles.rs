//! Production ANSI adapter-to-Canvas boundary checks; not `ConPTY` acceptance.

#[path = "../src/render"]
mod render {
    pub const RESET: &str = "\x1b[0m";
    #[path = "hyperlink.rs"]
    #[expect(
        dead_code,
        reason = "This Canvas boundary fixture imports only the ANSI adapter's subset of the production module."
    )]
    pub(crate) mod hyperlink;
    #[path = "styled_text.rs"]
    #[expect(
        dead_code,
        reason = "This Canvas boundary fixture imports only the ANSI adapter's subset of the production module."
    )]
    pub(crate) mod styled_text;
}

#[path = "../src/repl_ui"]
mod repl_ui {
    #[path = "ansi_text"]
    mod ansi_text {
        #[path = "adapter.rs"]
        mod adapter;
        pub(super) use adapter::{AnsiText, RichText};
    }

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
    fn initial_style_and_resets_reach_ansi_text_canvas() {
        const TARGET: &str = "https://example.test/code?tags=one;two;three";
        for width in [100, 68, 78] {
            let canvas = element! {
                ansi_text::AnsiText(
                    content: format!("\x1b]8;;{TARGET}\x1b\\\x1b[1;2mBothMarker\x1b]8;;\x1b\\\x1b[22mIntensityReset\x1b[0mPlainAfter"),
                    initial_style: crossterm::style::ContentStyle {
                        foreground_color: Some(TerminalColor::DarkGrey),
                        ..crossterm::style::ContentStyle::default()
                    },
                )
            }
            .render(Some(width));
            let mut wire = Vec::new();
            canvas.write_ansi(&mut wire).unwrap();
            let rendered = StyledText::from_ansi(std::str::from_utf8(&wire).unwrap());
            assert_eq!(
                rendered.text.trim_end(),
                "BothMarkerIntensityResetPlainAfter"
            );
            assert_style(
                &rendered,
                "BothMarker",
                Some(TerminalColor::DarkGrey),
                true,
                true,
                false,
            );
            assert_style(
                &rendered,
                "IntensityReset",
                Some(TerminalColor::DarkGrey),
                false,
                false,
                false,
            );
            assert_style(&rendered, "PlainAfter", None, false, false, false);
            for (_, _, hyperlink) in rendered.rich_spans(0.."BothMarker".len()) {
                assert_eq!(hyperlink.map(AsRef::as_ref), Some(TARGET));
            }
            for (_, _, hyperlink) in rendered.rich_spans("BothMarker".len()..rendered.text.len()) {
                assert!(hyperlink.is_none());
            }
        }
    }

    #[test]
    fn combined_intensity_and_both_reset_forms_reach_rich_text_canvas() {
        for width in [100, 68, 78] {
            let source = StyledText::from_ansi("\x1b[48;2;1;3;4mNoOperandLeak\x1b[0mPlainAfter");
            let mut ansi = String::new();
            source.write_ansi(0..source.text.len(), &mut ansi);
            let content = std::sync::Arc::new(StyledText::from_ansi(&ansi));
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
