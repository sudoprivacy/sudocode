//! Plan cards summarize decisions; the review document owns the full source.

use runtime::plan_store::PlanReviewStatus;
use serde_json::Value;

use super::{tool_label, truncate_output_for_display, ToolCardContent, ToolStatus};

fn review_status(output: &Value) -> Option<PlanReviewStatus> {
    serde_json::from_value(output.get("status")?.clone()).ok()
}

pub(super) fn status(output: &Value, fallback: ToolStatus) -> ToolStatus {
    match review_status(output) {
        Some(PlanReviewStatus::RevisionRequested | PlanReviewStatus::Cancelled) => {
            ToolStatus::Review
        }
        _ => fallback,
    }
}

pub(super) fn card(output: Option<&Value>, status: ToolStatus) -> ToolCardContent {
    let review = output.filter(|output| review_status(output).is_some());
    let label = match review.and_then(review_status) {
        Some(state) => review_label(state),
        None if status == ToolStatus::Error => "failed",
        None if output.is_none() => "preparing review",
        None => "completed",
    };
    let header = format!("{} · {label}", tool_label("write_plan"));
    let Some(review) = review else {
        // Older transcripts lack structured review metadata. Retain their
        // result through the same bounded value formatter, without raw args.
        return output.map_or_else(
            || ToolCardContent::header_only(header.clone()),
            |output| super::value_card(header.clone(), output),
        );
    };
    let mut body = format!(
        "File: {}",
        review
            .get("planFile")
            .and_then(Value::as_str)
            .unwrap_or("?")
    );
    if let Some(feedback) = review.get("feedback").and_then(Value::as_str) {
        body.push_str("\nFeedback: ");
        body.push_str(&truncate_output_for_display(feedback, 12, 2000));
    }
    ToolCardContent::new(header, body)
}

fn review_label(status: PlanReviewStatus) -> &'static str {
    match status {
        PlanReviewStatus::DraftSaved => "draft saved",
        PlanReviewStatus::Approved => "approved",
        PlanReviewStatus::RevisionRequested => "revision requested",
        PlanReviewStatus::Cancelled => "review cancelled",
    }
}

/// Reconstruct historical proposals from their recorded arguments, never from
/// the mutable current plan file. Dispatch uses this same document composer.
pub(super) fn replay(
    input: &str,
    output: &str,
    renderer: &crate::render::TerminalRenderer,
    width: usize,
) -> Option<String> {
    let (payload, _) = super::split_hook_feedback(output);
    let result: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let state = review_status(&result).map_or("recorded proposal", review_label);
    let input: Value = serde_json::from_str(input).ok()?;
    let document = runtime::plan_store::compose_document(
        input.get("context")?.as_str()?,
        input.get("constraints")?.as_str()?,
        input.get("acceptance")?.as_str()?,
        input.get("content")?.as_str()?,
    );
    Some(
        renderer.render_plan_document(
            &runtime::PromptText::Markdown(document.into()),
            result
                .get("planFile")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            state,
            width,
        ),
    )
}
