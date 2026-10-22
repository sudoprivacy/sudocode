/// Render a frozen review document once into native terminal history.
/// The input slot owns choices and feedback; neither repainting it nor moving
/// between fields may reprint, clip or retain a second canvas for the document.
#[must_use]
pub(super) fn render_document(content: &runtime::PromptText, width: usize) -> String {
    let rendered = crate::render::TerminalRenderer::new().render_prompt_text(content, width);
    let mut output = String::new();
    rendered.write_ansi(0..rendered.text.len(), &mut output);
    output
}
