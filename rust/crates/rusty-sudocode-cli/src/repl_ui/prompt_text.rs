/// Render a frozen review document once into native terminal history.
/// The input slot owns choices and feedback; neither repainting it nor moving
/// between fields may reprint, clip or retain a second canvas for the document.
#[must_use]
pub(super) fn render_document(
    content: &runtime::PromptText,
    presentation: &runtime::QuestionPresentation,
    width: usize,
) -> String {
    crate::render::TerminalRenderer::new().render_question_document(content, presentation, width)
}
