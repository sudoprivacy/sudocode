//! Finished-turn result collectors for the CLI's `--output-format json` /
//! status paths.
//!
//! The provider client, the incremental wire→event translation, and the inline
//! markdown/spinner rendering that used to live here have moved across the
//! engine↔renderer seam: the pure client is now `engine_core::EngineApiClient`,
//! the display half is `render_engine::EngineEventRenderer`, and friendly
//! API-error formatting lives in the `api` crate
//! (`engine_core::format_user_visible_api_error`). What remains are small pure helpers
//! that shape a finished [`runtime::TurnSummary`] into the JSON the one-shot /
//! status paths emit — no I/O, no rendering.

use runtime::ContentBlock;

pub(crate) fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(crate) fn collect_tool_uses(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some(serde_json::json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_tool_results(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(serde_json::json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_prompt_cache_events(
    summary: &runtime::TurnSummary,
) -> Vec<serde_json::Value> {
    summary
        .prompt_cache_events
        .iter()
        .map(|event| {
            serde_json::json!({
                "unexpected": event.unexpected,
                "reason": event.reason,
                "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                "token_drop": event.token_drop,
            })
        })
        .collect()
}

pub(crate) fn max_tokens_for_model(model: &str) -> u32 {
    engine_core::max_tokens_for_model(model)
}
