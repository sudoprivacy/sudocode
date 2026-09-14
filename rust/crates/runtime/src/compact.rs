use std::collections::BTreeSet;
use std::fmt;

use crate::conversation::ApiClient;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageAggregation};

const COMPACT_CONTINUATION_PREAMBLE: &str =
    "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n";
const COMPACT_RECENT_MESSAGES_NOTE: &str = "Recent messages are preserved verbatim.";
const COMPACT_DIRECT_RESUME_INSTRUCTION: &str = "Continue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, and do not preface with continuation text.";

// ---------------------------------------------------------------------------
// Rolling checkpoint prompt
// ---------------------------------------------------------------------------

const NO_TOOLS_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n\
- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.\n\
- You already have all the context you need in the conversation above.\n\
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.\n\
- Your entire response must be plain text: a <summary> block.\n\n";

const BASE_COMPACT_PROMPT: &str = "Create a concise checkpoint for continuing this coding task. Aim to keep the entire summary within 8,000 tokens while preserving the information needed to continue the task. Output only a <summary> block with these sections, using short bullets and (none) for empty sections:

1. Primary Request and Intent
2. Key Technical Concepts
3. Files and Code
4. Errors and Fixes
5. Pending Tasks
6. Current Work
7. Next Step
8. Critical Context

Preserve exact paths, commands, identifiers, important code fragments, user corrections, constraints and decisions with their rationale. Distinguish completed work from pending work. Keep the latest request and next action precise. If the conversation contains a previous summary, consolidate it with the newer history: retain still-valid facts, remove superseded details, and never copy the previous summary wholesale. Do not perform the task or call tools.";

const NO_TOOLS_TRAILER: &str =
    "\n\nREMINDER: Do NOT call any tools. Respond with plain text only — \
a <summary> block. \
Tool calls will be rejected and you will fail the task.";

const COMPACTION_SYSTEM_PROMPT: &str =
    "You are a helpful AI assistant tasked with summarizing conversations.";

/// Fixed compaction output ceiling, with room to finish an 8,000-token target
/// summary. Providers may impose a smaller output limit.
pub const COMPACT_MAX_OUTPUT_TOKENS: u32 = 12_000;

/// Base buffer subtracted from context window when computing the auto-compact
/// threshold. Scaled by [`autocompact_buffer_tokens`] for large context
/// windows — see CC's `getAutocompactBufferTokens()`.
pub const AUTOCOMPACT_BUFFER_TOKENS: u32 = 13_000;

/// Context-aware autocompact buffer. Larger context windows need more
/// headroom because a single turn can produce proportionally more tokens
/// (longer model outputs + larger tool results).
///
/// Matches CC's `getAutocompactBufferTokens()`:
/// - 800K+ context → 50K buffer
/// - 400K+ context → 30K buffer
/// - else → 13K (base constant)
#[must_use]
pub fn autocompact_buffer_tokens(model: &str) -> u32 {
    let context_window = crate::model_capabilities::context_window_or_default(model);
    if context_window >= 800_000 {
        50_000
    } else if context_window >= 400_000 {
        30_000
    } else {
        AUTOCOMPACT_BUFFER_TOKENS
    }
}

/// The numbers the context-window guard subtracts from the model's window,
/// and the arithmetic that turns them into a history budget.
///
/// Two signals get compared against this budget and they are in different
/// units: a local estimate of the conversation (history only —
/// [`Self::history_budget`]) and the context the provider reported for the
/// latest response (system prompt and tool definitions included —
/// [`Self::reported_context_budget`]). Keep them apart; mixing them
/// double-counts or drops the request overhead.
///
/// Two callers need this and they sit in different crates: the per-turn
/// preflight in the engine host (which knows the provider's output
/// reservation and the rendered tool definitions) and the in-turn check in
/// the runtime tool loop (which does not). Sharing the struct keeps one
/// formula while each layer supplies the inputs it actually has — see
/// [`ApiClient::context_budget`](crate::conversation::ApiClient::context_budget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// The model's full context window.
    pub context_limit: usize,
    /// Output space reserved for the response that has not been generated yet.
    pub max_output_tokens: usize,
    /// Rendered system prompt + tool definitions: present on every request and
    /// not something compaction can shrink.
    pub overhead_tokens: usize,
    /// Headroom so a turn that grows while it runs does not land exactly on
    /// the limit. Zero when checking the hard limit rather than the trigger.
    pub buffer_tokens: usize,
}

impl ContextBudget {
    /// Tokens of history that still fit, buffer included.
    ///
    /// Compare this against a *local estimate* of the conversation — the
    /// estimate covers history only, so the overhead is subtracted here.
    #[must_use]
    pub fn history_budget(&self) -> usize {
        self.context_limit
            .saturating_sub(self.max_output_tokens + self.overhead_tokens + self.buffer_tokens)
    }

    /// Tokens of *provider-reported* context that still fit, buffer included.
    ///
    /// Compare this against a number that came back from the provider
    /// (`input_tokens` + cache reads + cache writes). That number already
    /// counts the system prompt and the tool definitions, so unlike
    /// [`Self::history_budget`] the overhead must not be subtracted again —
    /// doing so would trigger a compaction one overhead's worth of tokens
    /// early on every model.
    #[must_use]
    pub fn reported_context_budget(&self) -> usize {
        self.context_limit
            .saturating_sub(self.max_output_tokens + self.buffer_tokens)
    }

    /// Whether `estimated_tokens` of history fits under the hard limit. The
    /// buffer is deliberately not applied here: this is the question "would
    /// the provider accept this request", not "should we compact first".
    #[must_use]
    pub fn fits(&self, estimated_tokens: usize) -> bool {
        estimated_tokens + self.overhead_tokens + self.max_output_tokens <= self.context_limit
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors specific to the compaction subsystem.
#[derive(Debug)]
pub enum CompactionError {
    /// The API client does not support LLM-based compaction (default impl).
    NotSupported,
    /// The session is too small to compact.
    NothingToCompact,
    /// The LLM call failed.
    ApiError(String),
    /// Empty, incomplete, or non-shrinking replacement.
    InvalidSummary(String),
    /// A replacement could not be durably saved.
    Persistence(String),
}

impl fmt::Display for CompactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported => write!(f, "compaction not supported by this API client"),
            Self::NothingToCompact => write!(f, "session too small to compact"),
            Self::ApiError(msg) => write!(f, "compaction API error: {msg}; history preserved"),
            Self::InvalidSummary(msg) => {
                write!(f, "invalid compaction summary: {msg}; history preserved")
            }
            Self::Persistence(msg) => {
                write!(f, "compaction persistence failed: {msg}; history preserved")
            }
        }
    }
}

impl std::error::Error for CompactionError {}

// ---------------------------------------------------------------------------
// Compaction config & result
// ---------------------------------------------------------------------------

/// Thresholds controlling when and how a session is compacted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub preserve_recent_messages: usize,
    pub max_estimated_tokens: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            preserve_recent_messages: 4,
            max_estimated_tokens: 10_000,
        }
    }
}

impl CompactionConfig {
    /// Retain a token-priced suffix, with the configured message floor and
    /// complete tool exchanges. A single large exchange is never split.
    #[must_use]
    pub fn with_token_retention(mut self, session: &Session, tokens: usize) -> Self {
        let mut accumulated = 0;
        let count = session
            .messages
            .iter()
            .rev()
            .take_while(|message| {
                if accumulated >= tokens {
                    return false;
                }
                accumulated += estimate_message_tokens(message);
                true
            })
            .count();
        self.preserve_recent_messages = self.preserve_recent_messages.max(count);
        self
    }
}

/// A failed or unnecessary attempt must return the original transcript intact.
#[must_use]
pub fn unchanged_compaction(session: &Session) -> CompactionResult {
    CompactionResult {
        summary: String::new(),
        formatted_summary: String::new(),
        compacted_session: session.clone(),
        removed_message_count: 0,
        summary_source: CompactionSummarySource::Llm,
    }
}

/// Trim only oversized tool text. The caller stages this on a clone and
/// archives the original before committing. Keep identifiers and error flags.
pub fn prune_tool_results(session: &mut Session) -> usize {
    let mut pruned = 0;
    for message in &mut session.messages {
        for block in &mut message.blocks {
            if let ContentBlock::ToolResult {
                tool_name, output, ..
            } = block
            {
                // ToolSearch JSON is also consumed by deferred-schema routing.
                if matches!(tool_name.as_str(), "ToolSearch" | "tool_search") {
                    continue;
                }
                let chars: Vec<char> = output.chars().collect();
                if chars.len() > 8_192 {
                    *output = chars[..4_096].iter().collect::<String>()
                        + "\n\n[... tool result middle pruned; original retained in pre-compaction transcript ...]\n\n"
                        + &chars[chars.len() - 1_024..].iter().collect::<String>();
                    pruned += 1;
                }
            }
        }
    }
    pruned
}

/// Completion validity belongs to the checkpoint policy, not the transport.
pub(crate) fn validate_completion(
    response: crate::conversation::TextCompletion,
) -> Result<String, crate::conversation::RuntimeError> {
    use crate::conversation::RuntimeError;
    if matches!(
        response.stop_reason.as_deref(),
        Some("max_tokens" | "length" | "incomplete")
    ) {
        return Err(RuntimeError::new(
            "compaction summary was truncated at the output limit",
        ));
    }
    if response.has_tool_calls {
        return Err(RuntimeError::new(
            "compaction returned tool calls instead of a complete checkpoint",
        ));
    }
    if response.text.trim().is_empty() {
        return Err(RuntimeError::new("compaction returned no summary text"));
    }
    Ok(response.text)
}

/// Validate the actual framed replacement, not just the raw model response.
fn validate_summary(summary: &str, removed: &[ConversationMessage]) -> Result<(), CompactionError> {
    let text = format_compact_summary(summary);
    if text.trim().is_empty() || text.trim() == "Summary:" {
        return Err(CompactionError::InvalidSummary("empty text".into()));
    }
    if summary.contains("<summary>") && !summary.contains("</summary>") {
        return Err(CompactionError::InvalidSummary("unclosed summary".into()));
    }
    let replacement =
        ConversationMessage::user_text(get_compact_continuation_message(summary, true, true));
    let before: usize = removed.iter().map(estimate_message_tokens).sum();
    if estimate_message_tokens(&replacement) >= before {
        return Err(CompactionError::InvalidSummary(
            "replacement does not reduce context".into(),
        ));
    }
    Ok(())
}

/// Which path produced a [`CompactionResult`]'s summary.
///
/// The local heuristic summary is much lossier than the LLM one, so callers
/// surface this to the user instead of reporting a silent downgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionSummarySource {
    /// Deterministic shortening of tool output, without a summary call.
    ToolPruning,
    /// The summary came from the LLM compaction call.
    Llm,
    /// The summary came from the local structural heuristic
    /// ([`compact_session_sync`]). `fallback_reason` is set when the LLM
    /// path was attempted first and failed.
    Local { fallback_reason: Option<String> },
}

impl fmt::Display for CompactionSummarySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ToolPruning => write!(f, "tool output pruning"),
            Self::Llm => write!(f, "llm"),
            Self::Local {
                fallback_reason: None,
            } => write!(f, "local"),
            Self::Local {
                fallback_reason: Some(reason),
            } => write!(f, "local (LLM compaction failed: {reason})"),
        }
    }
}

/// Result of compacting a session into a summary plus preserved tail messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub summary: String,
    pub formatted_summary: String,
    pub compacted_session: Session,
    pub removed_message_count: usize,
    pub summary_source: CompactionSummarySource,
}

// ---------------------------------------------------------------------------
// Token estimation (kept from original)
// ---------------------------------------------------------------------------

/// Roughly estimates the token footprint of the current session transcript.
#[must_use]
pub fn estimate_session_tokens(session: &Session) -> usize {
    session.messages.iter().map(estimate_message_tokens).sum()
}

/// Estimate tokens for a single message block.
/// This is useful for preflight checks before sending a request.
#[must_use]
pub fn estimate_block_tokens(block: &ContentBlock) -> usize {
    estimate_single_block_tokens(block)
}

fn estimate_single_block_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len() / 4 + 1,
        ContentBlock::Image { data, .. } => {
            let base64_len = data.len();
            if base64_len < 50_000 {
                85
            } else if base64_len < 200_000 {
                256
            } else if base64_len < 500_000 {
                512
            } else if base64_len < 1_000_000 {
                1000
            } else {
                base64_len / 1000
            }
        }
        ContentBlock::ToolUse { name, input, .. } => (name.len() + input.len()) / 4 + 1,
        ContentBlock::ToolResult {
            tool_name, output, ..
        } => (tool_name.len() + output.len()) / 4 + 1,
        ContentBlock::Thinking {
            thinking,
            signature,
        } => thinking.len() / 4 + signature.as_ref().map_or(0, |value| value.len() / 4 + 1),
    }
}

pub(crate) fn estimate_message_tokens(message: &ConversationMessage) -> usize {
    message
        .blocks
        .iter()
        .map(estimate_single_block_tokens)
        .sum()
}

// ---------------------------------------------------------------------------
// should_compact — simplified; caller decides threshold
// ---------------------------------------------------------------------------

/// Returns `true` when the session exceeds the configured compaction budget.
#[must_use]
pub fn should_compact(session: &Session, config: CompactionConfig) -> bool {
    let start = compacted_summary_prefix_len(session);
    let compactable = &session.messages[start..];

    compactable.len() > config.preserve_recent_messages
        && compactable
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>()
            >= config.max_estimated_tokens
}

// ---------------------------------------------------------------------------
// format / continuation helpers (kept from original — CC-compatible)
// ---------------------------------------------------------------------------

/// Normalizes a compaction summary into user-facing continuation text.
#[must_use]
pub fn format_compact_summary(summary: &str) -> String {
    let without_analysis = strip_tag_block(summary, "analysis");
    let formatted = if let Some(content) = extract_tag_block(&without_analysis, "summary") {
        without_analysis.replace(
            &format!("<summary>{content}</summary>"),
            &format!("Summary:\n{}", content.trim()),
        )
    } else {
        without_analysis
    };

    collapse_blank_lines(&formatted).trim().to_string()
}

/// Builds the synthetic system message used after session compaction.
#[must_use]
pub fn get_compact_continuation_message(
    summary: &str,
    suppress_follow_up_questions: bool,
    recent_messages_preserved: bool,
) -> String {
    let mut base = format!(
        "{COMPACT_CONTINUATION_PREAMBLE}{}",
        format_compact_summary(summary)
    );

    if recent_messages_preserved {
        base.push_str("\n\n");
        base.push_str(COMPACT_RECENT_MESSAGES_NOTE);
    }

    if suppress_follow_up_questions {
        base.push('\n');
        base.push_str(COMPACT_DIRECT_RESUME_INSTRUCTION);
    }

    base
}

// ---------------------------------------------------------------------------
// LLM-based compaction (new async path)
// ---------------------------------------------------------------------------

/// Build the compaction prompt, optionally injecting custom instructions.
fn build_compaction_prompt(custom_instructions: Option<&str>) -> String {
    let mut prompt = String::with_capacity(
        NO_TOOLS_PREAMBLE.len()
            + BASE_COMPACT_PROMPT.len()
            + NO_TOOLS_TRAILER.len()
            + custom_instructions.map_or(0, |s| s.len() + 30),
    );
    prompt.push_str(NO_TOOLS_PREAMBLE);
    prompt.push_str(BASE_COMPACT_PROMPT);
    if let Some(instructions) = custom_instructions {
        if !instructions.trim().is_empty() {
            prompt.push_str("\n\nAdditional Instructions:\n");
            prompt.push_str(instructions);
        }
    }
    prompt.push_str(NO_TOOLS_TRAILER);
    prompt
}

/// Format messages being removed into a transcript for the compaction LLM.
///
/// Images are stripped (replaced with `[image: <mime>]` placeholders) and
/// thinking blocks are omitted to save tokens. The resulting messages are
/// returned as `ConversationMessage`s suitable for passing to
/// [`ApiClient::send_compaction`].
fn build_compaction_messages(
    removed: &[ConversationMessage],
    compaction_prompt: &str,
) -> Vec<ConversationMessage> {
    let mut messages: Vec<ConversationMessage> = removed
        .iter()
        .filter_map(|msg| {
            let blocks: Vec<ContentBlock> = msg
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(ContentBlock::Text { text: text.clone() }),
                    ContentBlock::Image { mime_type, .. } => Some(ContentBlock::Text {
                        text: format!("[image: {mime_type}]"),
                    }),
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                    } => Some(ContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        thought_signature: thought_signature.clone(),
                    }),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        tool_name,
                        output,
                        is_error,
                    } => Some(ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        output: output.clone(),
                        is_error: *is_error,
                    }),
                    ContentBlock::Thinking { .. } => None,
                })
                .collect();

            if blocks.is_empty() {
                return None;
            }

            // Map System to User for the compaction conversation. Tool keeps
            // its role: the API client relies on it to merge consecutive
            // tool results into one user message, which Anthropic requires
            // for every `tool_use` in the preceding assistant message
            // (parallel tool calls otherwise fail with `tool_use ids were
            // found without tool_result blocks immediately after`).
            let role = match msg.role {
                MessageRole::System | MessageRole::User => MessageRole::User,
                MessageRole::Tool => MessageRole::Tool,
                MessageRole::Assistant => MessageRole::Assistant,
            };

            Some(ConversationMessage {
                role,
                blocks,
                usage: None,
                model: None,
            })
        })
        .collect();

    // Append the compaction request as a final user message
    messages.push(ConversationMessage::user_text(compaction_prompt));
    messages
}

/// Maximum retries for the compaction streaming call.
/// Matches CC's `MAX_COMPACT_STREAMING_RETRIES`.
const MAX_COMPACT_RETRIES: u32 = 2;

/// Check if an error is retryable (transient failures, not permanent ones).
fn is_retryable_error(error_msg: &str) -> bool {
    let lower = error_msg.to_lowercase();
    lower.contains("timeout")
        || lower.contains("connection")
        || lower.contains("server error")
        || lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("529")
        || lower.contains("overloaded")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
}

/// Check if an error indicates prompt-too-long.
fn is_prompt_too_long(error_msg: &str) -> bool {
    let lower = error_msg.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("prompt_too_long")
        || lower.contains("maximum context length")
        || lower.contains("token limit")
        // The API client's local preflight rejects an oversized compaction
        // request before it leaves the process; treat it like the provider's
        // own prompt-too-long so no destructive retry is attempted.
        || lower.contains("context_window_blocked")
        // Anthropic: "input length and `max_tokens` exceed context limit".
        || lower.contains("context limit")
        || lower.contains("context window")
}

/// Compacts a session using an LLM to produce a high-quality summary.
///
/// Retries transient failures up to [`MAX_COMPACT_RETRIES`] times with
/// exponential backoff. Oversized inputs and invalid summaries return an
/// error without discarding source messages or installing a local fallback.
pub async fn compact_session<C: ApiClient>(
    session: &Session,
    config: CompactionConfig,
    api_client: &mut C,
    model: &str,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, CompactionError> {
    if session.messages.len() <= config.preserve_recent_messages
        || estimate_session_tokens(session) < config.max_estimated_tokens
    {
        return Err(CompactionError::NothingToCompact);
    }

    let compacted_prefix_len = 0;

    let raw_keep_from = session
        .messages
        .len()
        .saturating_sub(config.preserve_recent_messages);

    // Protect tool-use / tool-result boundaries
    let keep_from = find_safe_compaction_boundary(session, raw_keep_from, compacted_prefix_len);

    let existing_usage = session.compaction.as_ref().and_then(|value| value.usage);
    let removed = &session.messages[compacted_prefix_len..keep_from];
    let preserved = session.messages[keep_from..].to_vec();

    if removed.is_empty() {
        return Err(CompactionError::NothingToCompact);
    }

    let compacted_usage = aggregate_compaction_usage(existing_usage, removed);

    // Build prompt and messages for the LLM
    let prompt = build_compaction_prompt(custom_instructions);
    let compaction_messages = build_compaction_messages(removed, &prompt);

    let max_tokens = std::cmp::min(
        COMPACT_MAX_OUTPUT_TOKENS,
        crate::model_capabilities::max_output_tokens_or_default(model),
    );

    // Retry transient failures without throwing away any source history.
    let mut attempt = 0;
    let llm_summary = loop {
        match api_client
            .send_compaction(
                model,
                COMPACTION_SYSTEM_PROMPT,
                compaction_messages.clone(),
                max_tokens,
            )
            .await
        {
            Ok(summary) => break summary,
            Err(error) => {
                let message = error.to_string();
                if attempt < MAX_COMPACT_RETRIES
                    && is_retryable_error(&message)
                    && !is_prompt_too_long(&message)
                {
                    tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
                    attempt += 1;
                } else {
                    return Err(CompactionError::ApiError(message));
                }
            }
        }
    };

    let discovered = extract_pre_compact_discovered_tools(session);

    validate_summary(&llm_summary, removed)?;
    let summary = llm_summary;
    let formatted_summary = format_compact_summary(&summary);
    let continuation = get_compact_continuation_message(&summary, true, !preserved.is_empty());

    let mut compacted_messages = vec![ConversationMessage {
        role: MessageRole::System,
        blocks: vec![ContentBlock::Text { text: continuation }],
        usage: None,
        model: None,
    }];
    compacted_messages.extend(preserved);

    let mut compacted_session = session.clone();
    compacted_session.messages = compacted_messages;
    compacted_session.record_compaction_with_usage(
        summary.clone(),
        removed.len(),
        compacted_usage,
        discovered,
    );

    Ok(CompactionResult {
        summary,
        formatted_summary,
        compacted_session,
        removed_message_count: removed.len(),
        summary_source: CompactionSummarySource::Llm,
    })
}

/// Cache-safe compaction: sends the compaction prompt over the same
/// system-prompt + message prefix the previous conversation turn used,
/// enabling prompt cache reuse. Falls back to [`CompactionError::ApiError`]
/// when the API client doesn't support it.
///
/// Matches CC's `streamCompactSummary` path with `tengu_compact_cache_prefix`.
pub async fn compact_session_cache_safe<C: ApiClient>(
    session: &Session,
    config: CompactionConfig,
    api_client: &mut C,
    model: &str,
    system_prompt: &crate::prompt::SystemPrompt,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, CompactionError> {
    if session.messages.len() <= config.preserve_recent_messages
        || estimate_session_tokens(session) < config.max_estimated_tokens
    {
        return Err(CompactionError::NothingToCompact);
    }

    let compacted_prefix_len = 0;

    let raw_keep_from = session
        .messages
        .len()
        .saturating_sub(config.preserve_recent_messages);
    let keep_from = find_safe_compaction_boundary(session, raw_keep_from, compacted_prefix_len);

    let existing_usage = session.compaction.as_ref().and_then(|value| value.usage);
    let removed = &session.messages[compacted_prefix_len..keep_from];
    let preserved = session.messages[keep_from..].to_vec();

    if removed.is_empty() {
        return Err(CompactionError::NothingToCompact);
    }

    let compacted_usage = aggregate_compaction_usage(existing_usage, removed);
    let prompt = build_compaction_prompt(custom_instructions);

    let max_tokens = std::cmp::min(
        COMPACT_MAX_OUTPUT_TOKENS,
        crate::model_capabilities::max_output_tokens_or_default(model),
    );

    let request = crate::conversation::ApiRequest {
        system_prompt: system_prompt.clone(),
        messages: session.messages[..keep_from].to_vec(),
        trace_id: None,
        pre_compact_discovered_tools: extract_pre_compact_discovered_tools(session),
    };

    let llm_summary = api_client
        .send_cache_safe_compaction(request, &prompt, max_tokens)
        .await
        .map_err(|error| CompactionError::ApiError(error.to_string()))?;

    let discovered = extract_pre_compact_discovered_tools(session);

    validate_summary(&llm_summary, removed)?;
    let summary = llm_summary;
    let formatted_summary = format_compact_summary(&summary);
    let continuation = get_compact_continuation_message(&summary, true, !preserved.is_empty());

    let mut compacted_messages = vec![ConversationMessage {
        role: MessageRole::System,
        blocks: vec![ContentBlock::Text { text: continuation }],
        usage: None,
        model: None,
    }];
    compacted_messages.extend(preserved);

    let mut compacted_session = session.clone();
    compacted_session.messages = compacted_messages;
    compacted_session.record_compaction_with_usage(
        summary.clone(),
        removed.len(),
        compacted_usage,
        discovered,
    );

    Ok(CompactionResult {
        summary,
        formatted_summary,
        compacted_session,
        removed_message_count: removed.len(),
        summary_source: CompactionSummarySource::Llm,
    })
}

/// Legacy compatibility helper: on LLM failure preserve all messages.
/// New callers should propagate the error rather than install a replacement.
#[must_use]
pub fn compact_session_sync_after_llm_failure(
    session: &Session,
    config: CompactionConfig,
    error: &CompactionError,
) -> CompactionResult {
    let _ = config;
    let mut result = unchanged_compaction(session);
    result.summary_source = CompactionSummarySource::Local {
        fallback_reason: Some(error.to_string()),
    };
    result
}

/// Legacy explicit structural compaction API. Production session paths use
/// validated LLM compaction; this must never be used for failure recovery.
#[must_use]
pub fn compact_session_sync(session: &Session, config: CompactionConfig) -> CompactionResult {
    if !should_compact(session, config) {
        return CompactionResult {
            summary: String::new(),
            formatted_summary: String::new(),
            compacted_session: session.clone(),
            removed_message_count: 0,
            summary_source: CompactionSummarySource::Local {
                fallback_reason: None,
            },
        };
    }

    let existing_summary = session
        .messages
        .first()
        .and_then(extract_existing_compacted_summary);
    let compacted_prefix_len = usize::from(existing_summary.is_some());
    let raw_keep_from = session
        .messages
        .len()
        .saturating_sub(config.preserve_recent_messages);

    let keep_from = find_safe_compaction_boundary(session, raw_keep_from, compacted_prefix_len);
    let existing_usage = session.compaction.as_ref().and_then(|value| value.usage);
    let removed = &session.messages[compacted_prefix_len..keep_from];
    let preserved = session.messages[keep_from..].to_vec();
    let compacted_usage = aggregate_compaction_usage(existing_usage, removed);
    let discovered = extract_pre_compact_discovered_tools(session);
    let summary = merge_compact_summaries(
        existing_summary.as_deref(),
        &summarize_messages_local(removed),
    );
    let formatted_summary = format_compact_summary(&summary);
    let continuation = get_compact_continuation_message(&summary, true, !preserved.is_empty());

    let mut compacted_messages = vec![ConversationMessage {
        role: MessageRole::System,
        blocks: vec![ContentBlock::Text { text: continuation }],
        usage: None,
        model: None,
    }];
    compacted_messages.extend(preserved);

    let mut compacted_session = session.clone();
    compacted_session.messages = compacted_messages;
    compacted_session.record_compaction_with_usage(
        summary.clone(),
        removed.len(),
        compacted_usage,
        discovered,
    );

    CompactionResult {
        summary,
        formatted_summary,
        compacted_session,
        removed_message_count: removed.len(),
        summary_source: CompactionSummarySource::Local {
            fallback_reason: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Walk the compaction boundary back to avoid splitting a tool-use /
/// tool-result pair.
fn find_safe_compaction_boundary(
    session: &Session,
    raw_keep_from: usize,
    compacted_prefix_len: usize,
) -> usize {
    let mut boundary = raw_keep_from.min(session.messages.len());
    loop {
        let mut pending = std::collections::BTreeMap::new();
        for (index, message) in session.messages[..boundary].iter().enumerate() {
            for block in &message.blocks {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        pending.insert(id, index);
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        pending.remove(tool_use_id);
                    }
                    _ => {}
                }
            }
        }
        let Some(start) = pending.values().min().copied() else {
            return boundary;
        };
        if start <= compacted_prefix_len {
            return compacted_prefix_len;
        }
        boundary = start;
    }
}

fn aggregate_compaction_usage(
    existing_usage: Option<TokenUsage>,
    removed_messages: &[ConversationMessage],
) -> Option<TokenUsage> {
    let mut total = UsageAggregation::default();
    let mut found = false;
    if let Some(usage) = existing_usage {
        total.push(usage);
        found = true;
    }
    for message in removed_messages {
        if message.role != MessageRole::Assistant {
            continue;
        }
        if let Some(usage) = message.usage {
            total.push(usage);
            found = true;
        }
    }
    found.then(|| total.finish())
}

fn compacted_summary_prefix_len(session: &Session) -> usize {
    usize::from(
        session
            .messages
            .first()
            .and_then(extract_existing_compacted_summary)
            .is_some(),
    )
}

/// Simple local summary used as fallback when the LLM path is unavailable.
fn summarize_messages_local(messages: &[ConversationMessage]) -> String {
    let user_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count();
    let assistant_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .count();
    let tool_messages = messages
        .iter()
        .filter(|message| message.role == MessageRole::Tool)
        .count();

    let mut tool_names = messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
            ContentBlock::ToolResult { tool_name, .. } => Some(tool_name.as_str()),
            ContentBlock::Text { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. } => None,
        })
        .collect::<Vec<_>>();
    tool_names.sort_unstable();
    tool_names.dedup();

    let mut lines = vec![
        "<summary>".to_string(),
        "Conversation summary:".to_string(),
        format!(
            "- Scope: {} earlier messages compacted (user={}, assistant={}, tool={}).",
            messages.len(),
            user_messages,
            assistant_messages,
            tool_messages
        ),
    ];

    if !tool_names.is_empty() {
        lines.push(format!("- Tools mentioned: {}.", tool_names.join(", ")));
    }

    lines.push("</summary>".to_string());
    lines.join("\n")
}

/// Section headers emitted by [`merge_compact_summaries`]. Also recognized on
/// re-compaction so prior merge scaffolding can be unwrapped instead of
/// re-nested (see [`flatten_merged_highlights`]).
const MERGED_PREVIOUS_CONTEXT_HEADER: &str = "- Previously compacted context:";
const MERGED_NEW_CONTEXT_HEADER: &str = "- Newly compacted context:";

/// Unwrap highlights that were produced by an earlier
/// [`merge_compact_summaries`] pass: drop its section headers and the one
/// indentation level they added, keeping the content lines verbatim.
///
/// Without this, every re-compaction wraps the prior summary in another
/// "- Previously compacted context:" layer, so the summary gains a header
/// plus two spaces of indent on every line per compaction cycle — nesting
/// that compounds for the lifetime of a long session. Highlights that did
/// not come from a merged summary (no section header present) are returned
/// unchanged. The loop collapses summaries that already carry multiple
/// nesting layers (persisted by older builds) down to a single level.
fn flatten_merged_highlights(mut highlights: Vec<String>) -> Vec<String> {
    while highlights
        .iter()
        .any(|line| line == MERGED_PREVIOUS_CONTEXT_HEADER || line == MERGED_NEW_CONTEXT_HEADER)
    {
        highlights = highlights
            .into_iter()
            .filter(|line| {
                line != MERGED_PREVIOUS_CONTEXT_HEADER && line != MERGED_NEW_CONTEXT_HEADER
            })
            .map(|line| line.strip_prefix("  ").map_or(line.clone(), str::to_string))
            .collect();
    }
    highlights
}

fn merge_compact_summaries(existing_summary: Option<&str>, new_summary: &str) -> String {
    let Some(existing_summary) = existing_summary else {
        return new_summary.to_string();
    };

    // Flatten prior merge scaffolding before re-wrapping, so repeated
    // compaction keeps exactly one "Previously compacted context" section
    // instead of nesting a new layer per cycle.
    let previous_highlights =
        flatten_merged_highlights(extract_summary_highlights(existing_summary));
    let new_formatted_summary = format_compact_summary(new_summary);
    let new_highlights = extract_summary_highlights(&new_formatted_summary);
    let new_timeline = extract_summary_timeline(&new_formatted_summary);

    let mut lines = vec!["<summary>".to_string(), "Conversation summary:".to_string()];

    if !previous_highlights.is_empty() {
        lines.push(MERGED_PREVIOUS_CONTEXT_HEADER.to_string());
        lines.extend(
            previous_highlights
                .into_iter()
                .map(|line| format!("  {line}")),
        );
    }

    if !new_highlights.is_empty() {
        lines.push(MERGED_NEW_CONTEXT_HEADER.to_string());
        lines.extend(new_highlights.into_iter().map(|line| format!("  {line}")));
    }

    if !new_timeline.is_empty() {
        lines.push("- Key timeline:".to_string());
        lines.extend(new_timeline.into_iter().map(|line| format!("  {line}")));
    }

    lines.push("</summary>".to_string());
    lines.join("\n")
}

fn extract_tag_block(content: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    let start_index = content.find(&start)? + start.len();
    let end_index = content[start_index..].find(&end)? + start_index;
    Some(content[start_index..end_index].to_string())
}

fn strip_tag_block(content: &str, tag: &str) -> String {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    if let (Some(start_index), Some(end_index_rel)) = (content.find(&start), content.find(&end)) {
        let end_index = end_index_rel + end.len();
        let mut stripped = String::new();
        stripped.push_str(&content[..start_index]);
        stripped.push_str(&content[end_index..]);
        stripped
    } else {
        content.to_string()
    }
}

fn collapse_blank_lines(content: &str) -> String {
    let mut result = String::new();
    let mut last_blank = false;
    for line in content.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && last_blank {
            continue;
        }
        result.push_str(line);
        result.push('\n');
        last_blank = is_blank;
    }
    result
}

fn extract_existing_compacted_summary(message: &ConversationMessage) -> Option<String> {
    if message.role != MessageRole::System {
        return None;
    }

    let text = first_text_block(message)?;
    let summary = text.strip_prefix(COMPACT_CONTINUATION_PREAMBLE)?;
    let summary = summary
        .split_once(&format!("\n\n{COMPACT_RECENT_MESSAGES_NOTE}"))
        .map_or(summary, |(value, _)| value);
    let summary = summary
        .split_once(&format!("\n{COMPACT_DIRECT_RESUME_INSTRUCTION}"))
        .map_or(summary, |(value, _)| value);
    Some(summary.trim().to_string())
}

fn first_text_block(message: &ConversationMessage) -> Option<&str> {
    message.blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        ContentBlock::ToolUse { .. }
        | ContentBlock::ToolResult { .. }
        | ContentBlock::Thinking { .. }
        | ContentBlock::Text { .. }
        | ContentBlock::Image { .. } => None,
    })
}

fn extract_summary_highlights(summary: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_timeline = false;

    for line in format_compact_summary(summary).lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed == "Summary:" || trimmed == "Conversation summary:" {
            continue;
        }
        if trimmed == "- Key timeline:" {
            in_timeline = true;
            continue;
        }
        if in_timeline {
            continue;
        }
        lines.push(trimmed.to_string());
    }

    lines
}

fn extract_summary_timeline(summary: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_timeline = false;

    for line in format_compact_summary(summary).lines() {
        let trimmed = line.trim_end();
        if trimmed == "- Key timeline:" {
            in_timeline = true;
            continue;
        }
        if !in_timeline {
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed.to_string());
    }

    lines
}

/// Extract tool names previously discovered via ToolSearch from the
/// session's messages. Scans ToolSearch result blocks for the `matches`
/// array and collects the tool names. Also merges any names carried
/// forward from prior compactions (`pre_compact_discovered_tools`).
fn extract_pre_compact_discovered_tools(session: &Session) -> BTreeSet<String> {
    let mut discovered: BTreeSet<String> = session
        .compaction
        .as_ref()
        .map(|c| c.pre_compact_discovered_tools.clone())
        .unwrap_or_default();
    for message in &session.messages {
        for block in &message.blocks {
            if let ContentBlock::ToolResult {
                tool_name, output, ..
            } = block
            {
                if tool_name == "ToolSearch" {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(output) {
                        if let Some(matches) = parsed.get("matches").and_then(|m| m.as_array()) {
                            for m in matches {
                                if let Some(name) = m.as_str() {
                                    discovered.insert(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    discovered
}

#[cfg(test)]
mod tests {
    use super::{
        compact_session_sync, format_compact_summary, get_compact_continuation_message,
        should_compact, CompactionConfig,
    };
    use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
    use crate::usage::{TokenUsage, UsageCostCurrency};

    #[test]
    fn autocompact_buffer_scales_with_context_window() {
        // Small model (200K context) → base 13K buffer
        let small = super::autocompact_buffer_tokens("claude-sonnet-4-6");
        assert_eq!(small, 13_000);

        // Medium model (400K context) → 30K buffer
        let medium = super::autocompact_buffer_tokens("gpt-5.4-mini");
        assert_eq!(medium, 30_000);

        // Large model (1M context) → 50K buffer
        let large = super::autocompact_buffer_tokens("claude-opus-4-8");
        assert_eq!(large, 50_000);

        // Unknown model falls back to SSOT default (1M) → 50K (large tier)
        let unknown = super::autocompact_buffer_tokens("unknown-model-xyz");
        assert_eq!(unknown, 50_000);
    }

    #[test]
    fn formats_compact_summary_like_upstream() {
        let summary = "<analysis>scratch</analysis>\n<summary>Kept work</summary>";
        assert_eq!(format_compact_summary(summary), "Summary:\nKept work");
    }

    #[test]
    fn leaves_small_sessions_unchanged() {
        let mut session = Session::new();
        session.messages = vec![ConversationMessage::user_text("hello")];

        let result = compact_session_sync(&session, CompactionConfig::default());
        assert_eq!(result.removed_message_count, 0);
        assert_eq!(result.compacted_session, session);
        assert!(result.summary.is_empty());
        assert!(result.formatted_summary.is_empty());
    }

    #[test]
    fn compacts_older_messages_into_a_system_summary() {
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::tool_result("1", "bash", "ok ".repeat(200), false),
            ConversationMessage {
                role: MessageRole::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "recent".to_string(),
                }],
                usage: None,
                model: None,
            },
        ];

        let result = compact_session_sync(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
        );

        assert!(
            result.removed_message_count <= 2,
            "expected at most 2 removed, got {}",
            result.removed_message_count
        );
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert!(matches!(
            &result.compacted_session.messages[0].blocks[0],
            ContentBlock::Text { text } if text.contains("Summary:")
        ));
        assert!(result.formatted_summary.contains("Scope:"));
        assert!(should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            }
        ));
        assert!(
            result.removed_message_count > 0,
            "compaction must remove at least one message"
        );
    }

    #[test]
    fn compaction_records_usage_for_removed_assistant_messages() {
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "two ".repeat(200),
                }],
                Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                    cost_units: Some(100),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
            ConversationMessage::user_text("three ".repeat(200)),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "four ".repeat(200),
                }],
                Some(TokenUsage {
                    input_tokens: 20,
                    output_tokens: 6,
                    cache_creation_input_tokens: 3,
                    cache_read_input_tokens: 5,
                    cost_units: Some(250),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let result = compact_session_sync(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
        );

        let usage = result
            .compacted_session
            .compaction
            .expect("compaction")
            .usage
            .expect("compacted usage");
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.cache_creation_input_tokens, 4);
        assert_eq!(usage.cache_read_input_tokens, 7);
        assert_eq!(usage.cost_units, Some(350));
        assert_eq!(usage.cost_currency, Some(UsageCostCurrency::SudoPoint));
        assert_eq!(result.compacted_session.messages[0].usage, None);
    }

    #[test]
    fn keeps_previous_compacted_context_when_compacting_again() {
        let mut initial_session = Session::new();
        initial_session.messages = vec![
            ConversationMessage::user_text("Investigate rust/crates/runtime/src/compact.rs"),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "I will inspect the compact flow.".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                    cost_units: Some(100),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
            ConversationMessage::user_text("Also update rust/crates/runtime/src/conversation.rs"),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "Next: preserve prior summary context during auto compact.".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 20,
                    output_tokens: 6,
                    cache_creation_input_tokens: 3,
                    cache_read_input_tokens: 5,
                    cost_units: Some(250),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
        ];
        let config = CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let first = compact_session_sync(&initial_session, config);
        let mut follow_up_messages = first.compacted_session.messages.clone();
        follow_up_messages.extend([
            ConversationMessage::user_text("Please add regression tests for compaction."),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "Working on regression coverage now.".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 30,
                    output_tokens: 8,
                    cache_creation_input_tokens: 4,
                    cache_read_input_tokens: 7,
                    cost_units: Some(400),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
        ]);

        let mut second_session = Session::new();
        second_session.compaction = first.compacted_session.compaction.clone();
        second_session.messages = follow_up_messages;
        let second = compact_session_sync(&second_session, config);

        assert!(second
            .formatted_summary
            .contains("Previously compacted context:"));
        assert!(second
            .formatted_summary
            .contains("Scope: 2 earlier messages compacted"));
        assert!(second
            .formatted_summary
            .contains("Newly compacted context:"));
        assert!(matches!(
            &second.compacted_session.messages[0].blocks[0],
            ContentBlock::Text { text }
                if text.contains("Previously compacted context:")
                    && text.contains("Newly compacted context:")
        ));
        assert!(matches!(
            &second.compacted_session.messages[1].blocks[0],
            ContentBlock::Text { text } if text.contains("Please add regression tests for compaction.")
        ));
        let usage = second
            .compacted_session
            .compaction
            .expect("second compaction")
            .usage
            .expect("merged compaction usage");
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.cache_creation_input_tokens, 4);
        assert_eq!(usage.cache_read_input_tokens, 7);
        assert_eq!(usage.cost_units, Some(350));
    }

    /// Regression: repeated re-compaction must not nest the summary one
    /// level deeper per cycle. Before the fix, every merge re-wrapped the
    /// prior merged summary (headers included) under a fresh
    /// "- Previously compacted context:" line with two more spaces of
    /// indent, so the summary gained a nesting layer per compaction.
    #[test]
    fn repeated_merges_flatten_prior_context_instead_of_nesting() {
        let mut summary =
            "<summary>\nConversation summary:\n- Fact from round 0.\n</summary>".to_string();
        for round in 1..=5 {
            summary = super::merge_compact_summaries(
                Some(&summary),
                &format!(
                    "<summary>\nConversation summary:\n- Fact from round {round}.\n</summary>"
                ),
            );
        }

        assert_eq!(
            summary.matches("- Previously compacted context:").count(),
            1,
            "prior context must stay in exactly one flat section: {summary}"
        );
        assert_eq!(
            summary.matches("- Newly compacted context:").count(),
            1,
            "only the latest round is 'newly' compacted: {summary}"
        );
        assert!(
            !summary.contains("  - Previously compacted context:")
                && !summary.contains("  - Newly compacted context:"),
            "no indented (nested) section headers may remain: {summary}"
        );
        // Flattening must not drop information: every round's fact survives.
        for round in 0..=5 {
            assert!(
                summary.contains(&format!("Fact from round {round}.")),
                "fact from round {round} must survive re-compaction: {summary}"
            );
        }
        // Content lines sit at exactly one indent level under their section.
        assert!(
            summary.contains("  - Fact from round 0.") && !summary.contains("    - Fact"),
            "indentation must stay at one level: {summary}"
        );
    }

    /// Summaries persisted by builds that had the nesting bug collapse to a
    /// single flat level on the next compaction instead of nesting further.
    #[test]
    fn merge_flattens_legacy_nested_summaries() {
        let legacy = [
            "<summary>",
            "Conversation summary:",
            "- Previously compacted context:",
            "  - Previously compacted context:",
            "    - Old fact A.",
            "  - Newly compacted context:",
            "    - Mid fact B.",
            "- Newly compacted context:",
            "  - Recent fact C.",
            "</summary>",
        ]
        .join("\n");

        let merged = super::merge_compact_summaries(
            Some(&legacy),
            "<summary>\nConversation summary:\n- Fresh fact D.\n</summary>",
        );

        assert_eq!(
            merged.matches("- Previously compacted context:").count(),
            1,
            "legacy nesting must collapse to one section: {merged}"
        );
        assert_eq!(
            merged.matches("- Newly compacted context:").count(),
            1,
            "legacy nesting must collapse to one section: {merged}"
        );
        for fact in [
            "- Old fact A.",
            "- Mid fact B.",
            "- Recent fact C.",
            "- Fresh fact D.",
        ] {
            assert!(
                merged.contains(fact),
                "flattening must not drop {fact:?}: {merged}"
            );
        }
        assert!(
            !merged.contains("    -"),
            "no content may remain nested deeper than one level: {merged}"
        );
    }

    /// End-to-end regression through the real compaction pipeline: compact a
    /// session five times in a row and assert the stored summary stays flat
    /// (one "Previously compacted context" section) while context from the
    /// very first round is still present.
    #[test]
    fn repeated_sync_compaction_keeps_summary_flat_and_preserves_early_context() {
        let config = CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("start ".repeat(200)),
            // Distinctive tool name: the local summarizer records tool names
            // verbatim, so this marker proves round-0 context survives.
            ConversationMessage::tool_result("t0", "round-zero-tool", "x".repeat(800), false),
            ConversationMessage::user_text("recent 0"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept 0".to_string(),
            }]),
        ];

        let mut result = compact_session_sync(&session, config);
        assert!(result.removed_message_count > 0, "round 0 must compact");

        for round in 1..=4 {
            let mut next = result.compacted_session.clone();
            next.messages.extend([
                ConversationMessage::user_text(format!("bulk {round} ").repeat(200)),
                ConversationMessage::tool_result(
                    format!("t{round}"),
                    format!("round-{round}-tool"),
                    "y".repeat(800),
                    false,
                ),
                ConversationMessage::user_text(format!("recent {round}")),
                ConversationMessage::assistant(vec![ContentBlock::Text {
                    text: format!("kept {round}"),
                }]),
            ]);
            result = compact_session_sync(&next, config);
            assert!(
                result.removed_message_count > 0,
                "round {round} must compact"
            );
        }

        let ContentBlock::Text { text: summary } = &result.compacted_session.messages[0].blocks[0]
        else {
            panic!("first message must be the text summary");
        };

        assert_eq!(
            summary.matches("Previously compacted context:").count(),
            1,
            "summary must keep exactly one flat prior-context section: {summary}"
        );
        assert_eq!(
            summary.matches("Newly compacted context:").count(),
            1,
            "summary must keep exactly one newly-compacted section: {summary}"
        );
        assert!(
            summary.contains("round-zero-tool"),
            "round-0 context must survive five compaction cycles: {summary}"
        );
        assert!(
            summary.contains("round-4-tool"),
            "latest round context must be present: {summary}"
        );
    }

    #[test]
    fn ignores_existing_compacted_summary_when_deciding_to_recompact() {
        let summary = "<summary>Conversation summary:\n- Scope: earlier work preserved.\n- Key timeline:\n  - user: large preserved context\n</summary>";
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage {
                role: MessageRole::System,
                blocks: vec![ContentBlock::Text {
                    text: get_compact_continuation_message(summary, true, true),
                }],
                usage: None,
                model: None,
            },
            ConversationMessage::user_text("tiny"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "recent".to_string(),
            }]),
        ];

        assert!(!should_compact(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            }
        ));
    }

    /// Regression: compaction must not split an assistant(ToolUse) /
    /// user(ToolResult) pair at the boundary.
    #[test]
    fn compaction_does_not_split_tool_use_tool_result_pair() {
        use crate::session::{ContentBlock, Session};

        let tool_id = "call_abc";
        let mut session = Session::default();
        session
            .push_message(ConversationMessage::user_text("Search for files"))
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: tool_id.to_string(),
                    name: "search".to_string(),
                    input: "{\"q\":\"*.rs\"}".to_string(),
                    thought_signature: None,
                },
            ]))
            .unwrap();
        session
            .push_message(ConversationMessage::tool_result(
                tool_id,
                "search",
                "found 5 files",
                false,
            ))
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Done.".to_string(),
            }]))
            .unwrap();

        let config = CompactionConfig {
            preserve_recent_messages: 1,
            ..CompactionConfig::default()
        };
        let result = compact_session_sync(&session, config);
        let messages = &result.compacted_session.messages;
        for i in 1..messages.len() {
            let curr_is_tool_result = messages[i]
                .blocks
                .first()
                .is_some_and(|b| matches!(b, ContentBlock::ToolResult { .. }));
            if curr_is_tool_result {
                let prev_has_tool_use = messages[i - 1]
                    .blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
                assert!(
                    prev_has_tool_use,
                    "message[{}] is a ToolResult but message[{}] has no ToolUse: {:?}",
                    i,
                    i - 1,
                    &messages[i - 1].blocks
                );
            }
        }
    }

    #[test]
    fn build_compaction_prompt_includes_cc_constants() {
        let prompt = super::build_compaction_prompt(None);
        assert!(prompt.contains("CRITICAL: Respond with TEXT ONLY"));
        assert!(prompt.contains("Create a concise checkpoint"));
        assert!(prompt.contains("REMINDER: Do NOT call any tools"));
    }

    #[test]
    fn build_compaction_prompt_appends_custom_instructions() {
        let prompt = super::build_compaction_prompt(Some("Focus on test changes"));
        assert!(prompt.contains("Additional Instructions:"));
        assert!(prompt.contains("Focus on test changes"));
    }

    #[test]
    fn build_compaction_messages_strips_images_and_thinking() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "hmm...".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "response".to_string(),
                },
            ]),
            ConversationMessage {
                role: MessageRole::User,
                blocks: vec![ContentBlock::Image {
                    data: "base64data".to_string(),
                    mime_type: "image/png".to_string(),
                }],
                usage: None,
                model: None,
            },
        ];

        let result = super::build_compaction_messages(&messages, "summarize");

        // Should have 3 original messages + 1 compaction prompt
        assert_eq!(result.len(), 4);

        // Thinking block should be stripped
        assert!(result[1].blocks.len() == 1);
        assert!(matches!(&result[1].blocks[0], ContentBlock::Text { text } if text == "response"));

        // Image should be replaced with placeholder
        assert!(matches!(
            &result[2].blocks[0],
            ContentBlock::Text { text } if text == "[image: image/png]"
        ));

        // Last message is the compaction prompt
        assert!(matches!(
            &result[3].blocks[0],
            ContentBlock::Text { text } if text == "summarize"
        ));
    }

    #[tokio::test]
    async fn async_compact_session_uses_llm_summary() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;

        struct MockCompactionClient;

        #[async_trait]
        impl ApiClient for MockCompactionClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used in this test"))
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                system_prompt: &str,
                _messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                assert!(
                    system_prompt.contains("summarizing conversations"),
                    "compaction should use the correct system prompt"
                );
                Ok("<analysis>Mock analysis</analysis>\n<summary>\n1. Primary Request and Intent:\n   User asked to test compaction.\n\n7. Pending Tasks:\n   - Verify LLM compaction works\n</summary>".to_string())
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("three ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = MockCompactionClient;
        let result =
            super::compact_session(&session, config, &mut client, "claude-sonnet-4-6", None)
                .await
                .expect("LLM compaction should succeed");

        assert!(result.removed_message_count > 0);
        assert!(result
            .formatted_summary
            .contains("User asked to test compaction"));
        assert!(
            !result.formatted_summary.contains("Mock analysis"),
            "analysis block should be stripped"
        );
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System,
        );
    }

    #[tokio::test]
    async fn async_compact_session_falls_through_on_not_supported() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;

        // Use the default ApiClient impl which returns "not supported"
        struct NoCompactionClient;

        #[async_trait]
        impl ApiClient for NoCompactionClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = NoCompactionClient;
        let result =
            super::compact_session(&session, config, &mut client, "claude-sonnet-4-6", None).await;

        // Should fail with ApiError since default impl returns "not supported"
        assert!(result.is_err());
        assert!(matches!(result, Err(super::CompactionError::ApiError(_))));
    }

    #[tokio::test]
    async fn async_compact_nothing_to_compact_on_small_session() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;

        struct PanicIfCalled;

        #[async_trait]
        impl ApiClient for PanicIfCalled {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                panic!("stream should not be called");
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                panic!("send_compaction should not be called on a small session");
            }
        }

        let mut session = Session::new();
        session.messages = vec![ConversationMessage::user_text("hello")];

        let config = super::CompactionConfig::default();
        let mut client = PanicIfCalled;
        let result =
            super::compact_session(&session, config, &mut client, "claude-sonnet-4-6", None).await;

        assert!(matches!(
            result,
            Err(super::CompactionError::NothingToCompact)
        ));
    }

    #[tokio::test]
    async fn async_recompaction_merges_previous_and_new_summaries() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicU8, Ordering};

        static CALL_COUNT: AtomicU8 = AtomicU8::new(0);

        struct RecompactionMock;

        #[async_trait]
        impl ApiClient for RecompactionMock {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                let call = CALL_COUNT.fetch_add(1, Ordering::Relaxed);
                if call == 0 {
                    Ok("<summary>\n1. Primary Request and Intent:\n   User investigated compaction flow.\n</summary>".to_string())
                } else {
                    Ok("<summary>\n1. Primary Request and Intent:\n   User added regression tests.\n</summary>".to_string())
                }
            }
        }

        CALL_COUNT.store(0, Ordering::Relaxed);

        // First compaction
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("Investigate compact ".repeat(100)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Inspecting the flow ".repeat(100),
            }]),
            ConversationMessage::user_text("recent turn 1"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept 1".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = RecompactionMock;
        let first = super::compact_session(&session, config, &mut client, "sonnet", None)
            .await
            .expect("first compaction");
        assert!(first.removed_message_count > 0);

        // Add new messages to compacted session
        let mut second_session = first.compacted_session.clone();
        second_session
            .push_user_text("Add regression tests ".repeat(100))
            .unwrap();
        second_session
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Working on coverage ".repeat(100),
            }]))
            .unwrap();
        second_session.compaction = first.compacted_session.compaction;

        // Second compaction — should merge
        let second = super::compact_session(&second_session, config, &mut client, "sonnet", None)
            .await
            .expect("second compaction");

        assert!(!second
            .formatted_summary
            .contains("Previously compacted context:"));
        assert!(!second
            .formatted_summary
            .contains("Newly compacted context:"));
        assert!(second.summary.contains("User added regression tests."));
        assert!(!second
            .summary
            .contains("User investigated compaction flow."));
    }

    #[tokio::test]
    async fn async_compact_passes_custom_instructions_to_llm() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicBool, Ordering};

        static SAW_INSTRUCTIONS: AtomicBool = AtomicBool::new(false);

        struct InstructionVerifyingMock;

        #[async_trait]
        impl ApiClient for InstructionVerifyingMock {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                // The last message is the compaction prompt — verify it
                // contains the custom instructions.
                let last = messages.last().expect("messages should not be empty");
                let prompt_text = match &last.blocks[0] {
                    ContentBlock::Text { text } => text,
                    _ => panic!("last message should be text"),
                };
                if prompt_text.contains("Focus on TypeScript changes only") {
                    SAW_INSTRUCTIONS.store(true, Ordering::Relaxed);
                }
                Ok("<summary>\nCustom summary.\n</summary>".to_string())
            }
        }

        SAW_INSTRUCTIONS.store(false, Ordering::Relaxed);

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = InstructionVerifyingMock;
        let result = super::compact_session(
            &session,
            config,
            &mut client,
            "sonnet",
            Some("Focus on TypeScript changes only"),
        )
        .await
        .expect("compaction with custom instructions");

        assert!(result.removed_message_count > 0);
        assert!(
            SAW_INSTRUCTIONS.load(Ordering::Relaxed),
            "custom instructions must reach the LLM via the compaction prompt"
        );
    }

    #[test]
    fn build_compaction_messages_filters_thinking_only_messages() {
        // A message with ONLY thinking blocks should be filtered out entirely.
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "deep thought".to_string(),
                    signature: None,
                },
                ContentBlock::Thinking {
                    thinking: "more thought".to_string(),
                    signature: None,
                },
            ]),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "actual response".to_string(),
            }]),
        ];

        let result = super::build_compaction_messages(&messages, "summarize");

        // Original: 3 messages. After filter: user + text-only assistant = 2, plus prompt = 3.
        // The thinking-only assistant message should be dropped.
        assert_eq!(
            result.len(),
            3,
            "thinking-only message should be filtered out: got {:?}",
            result.iter().map(|m| m.blocks.len()).collect::<Vec<_>>()
        );

        // Verify the kept assistant message has the right text
        assert!(matches!(
            &result[1].blocks[0],
            ContentBlock::Text { text } if text == "actual response"
        ));
    }

    #[test]
    fn compaction_usage_aggregation_handles_partial_none() {
        // Some assistant messages have usage, some don't — should aggregate
        // only the ones with usage.
        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "two ".repeat(200),
                }],
                Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 0,
                    cost_units: Some(100),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
            ConversationMessage::user_text("three ".repeat(200)),
            // This assistant message has NO usage
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four ".repeat(200),
            }]),
            ConversationMessage::user_text("five ".repeat(200)),
            ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "six ".repeat(200),
                }],
                Some(TokenUsage {
                    input_tokens: 20,
                    output_tokens: 8,
                    cache_creation_input_tokens: 3,
                    cache_read_input_tokens: 2,
                    cost_units: Some(200),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let result = compact_session_sync(
            &session,
            CompactionConfig {
                preserve_recent_messages: 2,
                max_estimated_tokens: 1,
            },
        );

        let usage = result
            .compacted_session
            .compaction
            .expect("compaction")
            .usage
            .expect("compacted usage");

        // Should aggregate only the two messages WITH usage, skipping the None one
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 13);
        assert_eq!(usage.cache_creation_input_tokens, 4);
        assert_eq!(usage.cache_read_input_tokens, 2);
        assert_eq!(usage.cost_units, Some(300));
    }

    #[tokio::test]
    async fn compact_session_retries_transient_failures() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicU8, Ordering};

        static ATTEMPT: AtomicU8 = AtomicU8::new(0);

        struct FailThenSucceedClient;

        #[async_trait]
        impl ApiClient for FailThenSucceedClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
                if attempt < 2 {
                    Err(RuntimeError::new("503 server error: overloaded"))
                } else {
                    Ok("<summary>Recovered after retry.</summary>".to_string())
                }
            }
        }

        ATTEMPT.store(0, Ordering::Relaxed);

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = FailThenSucceedClient;
        let result = super::compact_session(&session, config, &mut client, "sonnet", None)
            .await
            .expect("should succeed after retries");

        assert!(result.removed_message_count > 0);
        assert!(result.formatted_summary.contains("Recovered after retry"));
        assert!(
            ATTEMPT.load(Ordering::Relaxed) == 3,
            "should have made 3 attempts (2 failures + 1 success)"
        );
    }

    #[tokio::test]
    async fn compact_session_preserves_source_on_prompt_too_long() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicU8, Ordering};

        static PTL_ATTEMPT: AtomicU8 = AtomicU8::new(0);

        struct PtlThenSucceedClient;

        #[async_trait]
        impl ApiClient for PtlThenSucceedClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                let attempt = PTL_ATTEMPT.fetch_add(1, Ordering::Relaxed);
                if attempt == 0 {
                    Err(RuntimeError::new(
                        "prompt_too_long: exceeds maximum context length",
                    ))
                } else {
                    // After truncation, message count should be smaller
                    Ok(format!(
                        "<summary>PTL recovered with {} messages.</summary>",
                        messages.len()
                    ))
                }
            }
        }

        PTL_ATTEMPT.store(0, Ordering::Relaxed);

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("three ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = PtlThenSucceedClient;
        let original = session.messages.clone();
        let result = super::compact_session(&session, config, &mut client, "sonnet", None).await;
        assert!(result.is_err());
        assert_eq!(PTL_ATTEMPT.load(Ordering::Relaxed), 1);
        assert_eq!(session.messages, original);
    }

    #[tokio::test]
    async fn compact_session_gives_up_on_permanent_failure() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;

        struct AlwaysFailClient;

        #[async_trait]
        impl ApiClient for AlwaysFailClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                Err(RuntimeError::new("authentication failed"))
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };

        let mut client = AlwaysFailClient;
        let result = super::compact_session(&session, config, &mut client, "sonnet", None).await;

        assert!(result.is_err());
        match result {
            Err(super::CompactionError::ApiError(msg)) => {
                assert!(msg.contains("authentication failed"));
            }
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_safe_compaction_uses_original_system_prompt() {
        use crate::conversation::{ApiClient, ApiRequest, AssistantEventStream, RuntimeError};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicBool, Ordering};

        static CACHE_SAFE_CALLED: AtomicBool = AtomicBool::new(false);

        struct CacheSafeClient;

        #[async_trait]
        impl ApiClient for CacheSafeClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            async fn send_cache_safe_compaction(
                &mut self,
                request: ApiRequest,
                compaction_prompt: &str,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                CACHE_SAFE_CALLED.store(true, Ordering::SeqCst);
                assert!(
                    request
                        .system_prompt
                        .render()
                        .contains("test system prompt"),
                    "cache-safe compaction must use the original system prompt"
                );
                assert!(
                    compaction_prompt.contains("CRITICAL: Respond with TEXT ONLY"),
                    "compaction prompt must include no-tools preamble"
                );
                Ok("<analysis>Cache-safe analysis</analysis>\n<summary>\n1. Primary Request and Intent:\n   Cache-safe compaction test.\n</summary>".to_string())
            }
        }

        CACHE_SAFE_CALLED.store(false, Ordering::SeqCst);

        let mut session = Session::new();
        session.messages = vec![
            ConversationMessage::user_text("one ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two ".repeat(200),
            }]),
            ConversationMessage::user_text("three ".repeat(200)),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four ".repeat(200),
            }]),
            ConversationMessage::user_text("recent"),
            ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "kept".to_string(),
            }]),
        ];

        let config = super::CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        };
        let mut system_prompt = crate::prompt::SystemPrompt::default();
        system_prompt.override_static_sections("test system prompt");

        let mut client = CacheSafeClient;
        let result = super::compact_session_cache_safe(
            &session,
            config,
            &mut client,
            "claude-sonnet-4-6",
            &system_prompt,
            None,
        )
        .await
        .expect("cache-safe compaction should succeed");

        assert!(CACHE_SAFE_CALLED.load(Ordering::SeqCst));
        assert!(result.removed_message_count > 0);
        assert!(result
            .formatted_summary
            .contains("Cache-safe compaction test"));
    }
}
