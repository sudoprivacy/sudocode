//! Effort choices shared by the CLI and Anthropic request builder.

pub const CLAUDE_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Claude 4.6 supports max but not xhigh. Unknown providers keep their own
/// effort vocabulary; these rules apply only to the Anthropic wire contract.
#[must_use]
pub fn claude_levels(model: &str) -> &'static [&'static str] {
    let model = model.to_ascii_lowercase();
    if model.contains("opus-4-5") {
        &["low", "medium", "high"]
    } else if model.contains("opus-4-6") || model.contains("sonnet-4-6") {
        &["low", "medium", "high", "max"]
    } else if [
        "opus-4-7",
        "opus-4-8",
        "opus-5",
        "sonnet-5",
        "haiku-5-5",
        "fable-5",
        "mythos-5",
        "mythos-preview",
    ]
    .iter()
    .any(|name| model.contains(name))
    {
        CLAUDE_LEVELS
    } else {
        &[]
    }
}

/// Resolve the effective API value without changing the user's preference
/// when switching to a model with a smaller effort ladder.
#[must_use]
pub fn claude_effort<'a>(model: &str, preference: Option<&'a str>) -> Option<&'a str> {
    let levels = claude_levels(model);
    if levels.is_empty() {
        return None;
    }
    let preference = preference.filter(|level| *level != "auto");
    match preference {
        Some("xhigh") if !levels.contains(&"xhigh") => Some("high"),
        Some(level) if levels.contains(&level) => Some(level),
        _ if model.to_ascii_lowercase().contains("opus-5-5")
            || model.to_ascii_lowercase().contains("haiku-5-5") =>
        {
            Some("medium")
        }
        _ => Some("high"),
    }
}
