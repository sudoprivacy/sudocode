use crate::render::styled_text::StyledText;
use iocraft::prelude::*;

use super::ansi_text::RichText;

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

