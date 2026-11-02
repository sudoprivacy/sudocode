//! Retained text presentation for the disposable prompt viewport.

use std::sync::Arc;

use iocraft::prelude::*;

use crate::render::styled_text::StyledText;

use super::ansi_text::RichText;

#[derive(Clone, Default, Props)]
pub(in crate::repl_ui) struct TextFrameProps {
    pub header: Arc<StyledText>,
    pub body: Arc<StyledText>,
    pub width: u32,
    pub header_background: Option<Color>,
}

/// Retain the complete immutable text subtree between editor-only updates.
/// Skipping just the text leaves still dirties their containing layout nodes.
#[derive(Default)]
pub(in crate::repl_ui) struct TextFrame {
    previous: Option<TextFrameProps>,
}

impl Component for TextFrame {
    type Props<'a> = TextFrameProps;

    fn new(_props: &Self::Props<'_>) -> Self {
        Self::default()
    }

    fn update(
        &mut self,
        props: &mut Self::Props<'_>,
        _hooks: Hooks,
        updater: &mut ComponentUpdater,
    ) {
        if self.previous.as_ref().is_some_and(|previous| {
            previous.width == props.width
                && previous.header_background == props.header_background
                && Arc::ptr_eq(&previous.header, &props.header)
                && Arc::ptr_eq(&previous.body, &props.body)
        }) {
            return;
        }
        if self
            .previous
            .as_ref()
            .is_none_or(|previous| previous.width != props.width)
        {
            let mut style = LayoutStyle::default();
            style.width = Size::Length(props.width);
            style.flex_direction = FlexDirection::Column;
            updater.set_layout_style(style.into());
        }
        let width = Size::Length(props.width);
        let children: [AnyElement<'static>; 2] = [
            element! {
                View(height: 1, width, background_color: props.header_background) {
                    RichText(content: Arc::clone(&props.header), width, wrap: TextWrap::NoWrap)
                }
            }
            .into(),
            element! {
                RichText(content: Arc::clone(&props.body), width, wrap: TextWrap::NoWrap)
            }
            .into(),
        ];
        updater.update_children(children, None);
        self.previous = Some(props.clone());
    }
}
