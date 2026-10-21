use crate::render::styled_text::StyledText;

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
