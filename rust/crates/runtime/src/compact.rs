use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::conversation::ApiClient;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session};
use crate::usage::{TokenUsage, UsageAggregation};

const COMPACT_CONTINUATION_PREAMBLE: &str =
    "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n";
const COMPACT_RECENT_MESSAGES_NOTE: &str = "Recent messages are preserved verbatim.";
const COMPACT_DIRECT_RESUME_INSTRUCTION: &str = "Continue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening, and do not preface with continuation text.";

// ---------------------------------------------------------------------------
// CC-verbatim compaction prompt constants
// ---------------------------------------------------------------------------

const NO_TOOLS_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n\
- Do NOT use Read, Bash, Grep, Glob, Edit, Write, or ANY other tool.\n\
- You already have all the context you need in the conversation above.\n\
- Tool calls will be REJECTED and will waste your only turn — you will fail the task.\n\
- Your entire response must be plain text: an <analysis> block followed by a <summary> block.\n\n";

const BASE_COMPACT_PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions.\n\
This summary should be thorough in capturing technical details, code patterns, and architectural decisions that would be essential for continuing development work without losing context.\n\n\
Before providing your final summary, wrap your analysis in <analysis> tags to organize your thoughts and ensure you've covered all necessary points. In your analysis process:\n\n\
1. Chronologically analyze each message and section of the conversation. For each section thoroughly identify:\n\
   - The user's explicit requests and intents\n\
   - Your approach to addressing the user's requests\n\
   - Key decisions, technical concepts and code patterns\n\
   - Specific details like:\n\
     - file names\n\
     - full code snippets\n\
     - function signatures\n\
     - file edits\n\
   - Errors that you ran into and how you fixed them\n\
   - Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.\n\
2. Double-check for technical accuracy and completeness, addressing each required element thoroughly.\n\n\
Your summary should include the following sections:\n\n\
1. Primary Request and Intent: Capture all of the user's explicit requests and intents in detail\n\
2. Key Technical Concepts: List all important technical concepts, technologies, and frameworks discussed.\n\
3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. Pay special attention to the most recent messages and include full code snippets where applicable and include a summary of why this file read or edit is important.\n\
4. Errors and fixes: List all errors that you ran into, and how you fixed them. Pay special attention to specific user feedback that you received, especially if the user told you to do something differently.\n\
5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.\n\
6. All user messages: List ALL user messages that are not tool results. These are critical for understanding the users' feedback and changing intent.\n\
7. Pending Tasks: Outline any pending tasks that you have explicitly been asked to work on.\n\
8. Current Work: Describe in detail precisely what was being worked on immediately before this summary request, paying special attention to the most recent messages from both user and assistant. Include file names and code snippets where applicable.\n\
9. Optional Next Step: List the next step that you will take that is related to the most recent work you were doing. IMPORTANT: ensure that this step is DIRECTLY in line with the user's most recent explicit requests, and the task you were working on immediately before this summary request. If your last task was concluded, then only list next steps if they are explicitly in line with the users request. Do not start on tangential requests or really old requests that were already completed without confirming with the user first.\n\
                       If there is a next step, include direct quotes from the most recent conversation showing exactly what task you were working on and where you left off. This should be verbatim to ensure there's no drift in task interpretation.\n\n\
Here's an example of how your output should be structured:\n\n\
<example>\n\
<analysis>\n\
[Your thought process, ensuring all points are covered thoroughly and accurately]\n\
</analysis>\n\n\
<summary>\n\
1. Primary Request and Intent:\n\
   [Detailed description]\n\n\
2. Key Technical Concepts:\n\
   - [Concept 1]\n\
   - [Concept 2]\n\
   - [...]\n\n\
3. Files and Code Sections:\n\
   - [File Name 1]\n\
      - [Summary of why this file is important]\n\
      - [Summary of the changes made to this file, if any]\n\
      - [Important Code Snippet]\n\
   - [File Name 2]\n\
      - [Important Code Snippet]\n\
   - [...]\n\n\
4. Errors and fixes:\n\
    - [Detailed description of error 1]:\n\
      - [How you fixed the error]\n\
      - [User feedback on the error if any]\n\
    - [...]\n\n\
5. Problem Solving:\n\
   [Description of solved problems and ongoing troubleshooting]\n\n\
6. All user messages: \n\
    - [Detailed non tool use user message]\n\
    - [...]\n\n\
7. Pending Tasks:\n\
   - [Task 1]\n\
   - [Task 2]\n\
   - [...]\n\n\
8. Current Work:\n\
   [Precise description of current work]\n\n\
9. Optional Next Step:\n\
   [Optional Next step to take]\n\n\
</summary>\n\
</example>\n\n\
Please provide your summary based on the conversation so far, following this structure and ensuring precision and thoroughness in your response. \n\n\
There may be additional summarization instructions provided in the included context. If so, remember to follow these instructions when creating the above summary. Examples of instructions include:\n\
<example>\n\
## Compact Instructions\n\
When summarizing the conversation focus on typescript code changes and also remember the mistakes you made and how you fixed them.\n\
</example>\n\n\
<example>\n\
# Summary instructions\n\
When you are using compact - please focus on test output and code changes. Include file reads verbatim.\n\
</example>";

const NO_TOOLS_TRAILER: &str =
    "\n\nREMINDER: Do NOT call any tools. Respond with plain text only — \
an <analysis> block followed by a <summary> block. \
Tool calls will be rejected and you will fail the task.";

const COMPACTION_SYSTEM_PROMPT: &str =
    "You are a helpful AI assistant tasked with summarizing conversations.";

/// Maximum output tokens requested from the LLM for a compaction summary.
/// CC uses `min(16384, model_max)` for the API request; the higher ceiling
/// here covers re-compactions whose input is already a prior summary.
pub const COMPACT_MAX_OUTPUT_TOKENS: u32 = 20_000;

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

// ---------------------------------------------------------------------------
// Post-compact file restore (CC parity)
// ---------------------------------------------------------------------------

/// Maximum number of recently-read files to re-inject after compaction.
/// Matches CC's `POST_COMPACT_MAX_FILES_TO_RESTORE`.
const POST_COMPACT_MAX_FILES: usize = 5;

/// Total token budget for all re-injected file content.
/// Matches CC's `POST_COMPACT_TOKEN_BUDGET`.
const POST_COMPACT_TOKEN_BUDGET: usize = 50_000;

/// Per-file token cap. Matches CC's `POST_COMPACT_MAX_TOKENS_PER_FILE`.
const POST_COMPACT_MAX_TOKENS_PER_FILE: usize = 5_000;

/// Tracks the most recent read_file tool result for each path.
/// Populated by `ConversationRuntime` each time a `read_file` tool
/// succeeds; consumed after compaction to restore file context.
#[derive(Debug, Default)]
pub struct ReadFileTracker {
    entries: BTreeMap<PathBuf, Instant>,
}

impl ReadFileTracker {
    /// Record that a file was read at the current instant.
    pub fn record(&mut self, path: PathBuf) {
        self.entries.insert(path, Instant::now());
    }

    /// Clear all tracked entries (called after file restore messages are built).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Build user messages re-injecting recently-read file content.
    ///
    /// Files already visible in `preserved_messages` (the tail kept after
    /// compaction) are skipped — they're already in the model's context.
    /// Returns messages ordered most-recent-first, constrained by both
    /// file count and token budget.
    #[must_use]
    pub fn build_post_compact_file_messages(
        &self,
        preserved_messages: &[ConversationMessage],
    ) -> Vec<ConversationMessage> {
        if self.entries.is_empty() {
            return Vec::new();
        }

        // Collect read_file paths already in the preserved tail.
        let preserved_paths = collect_read_file_paths(preserved_messages);

        // Sort by timestamp (most recent first), skip preserved.
        let mut candidates: Vec<(&PathBuf, &Instant)> = self
            .entries
            .iter()
            .filter(|(path, _)| !preserved_paths.iter().any(|pp| pp == *path))
            .collect();
        candidates.sort_by(|a, b| b.1.cmp(a.1));
        candidates.truncate(POST_COMPACT_MAX_FILES);

        let mut messages = Vec::new();
        let mut total_tokens = 0usize;

        for (path, _) in candidates {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Estimate tokens and enforce per-file + total budget
            let token_estimate = content.len() / 4 + 1;
            let capped = if token_estimate > POST_COMPACT_MAX_TOKENS_PER_FILE {
                // Truncate to fit the per-file budget (conservative char estimate)
                let max_chars = POST_COMPACT_MAX_TOKENS_PER_FILE * 4;
                &content[..content.floor_char_boundary(max_chars.min(content.len()))]
            } else {
                content.as_str()
            };

            let capped_tokens = capped.len() / 4 + 1;
            if total_tokens + capped_tokens > POST_COMPACT_TOKEN_BUDGET {
                break;
            }
            total_tokens += capped_tokens;

            let display_path = path.display();
            messages.push(ConversationMessage::user_text(format!(
                "[Post-compact file restore: {display_path}]\n{capped}"
            )));
        }

        messages
    }
}

/// Extract `read_file` tool-use paths from a message slice.
fn collect_read_file_paths(messages: &[ConversationMessage]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for msg in messages {
        for block in &msg.blocks {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                if name == "read_file" || name == "Read" {
                    if let Some(p) = extract_file_path_from_tool_input(input) {
                        paths.push(PathBuf::from(&p));
                    }
                }
            }
        }
    }
    paths
}

/// Parse the `file_path` field from a tool-use input JSON string.
/// Public for use by `ConversationRuntime::find_tool_use_file_path`.
pub fn extract_file_path_from_tool_input(input: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(input).ok()?;
    parsed
        .get("file_path")
        .or_else(|| parsed.get("filePath"))
        .or_else(|| parsed.get("path"))
        .and_then(|v| v.as_str())
        .map(String::from)
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
}

impl fmt::Display for CompactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported => write!(f, "compaction not supported by this API client"),
            Self::NothingToCompact => write!(f, "session too small to compact"),
            Self::ApiError(msg) => write!(f, "compaction API error: {msg}"),
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

/// Result of compacting a session into a summary plus preserved tail messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub summary: String,
    pub formatted_summary: String,
    pub compacted_session: Session,
    pub removed_message_count: usize,
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

fn estimate_message_tokens(message: &ConversationMessage) -> usize {
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

            // Map System/Tool roles to User for the compaction conversation
            let role = match msg.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => MessageRole::User,
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

/// Maximum PTL (prompt-too-long) truncation retries.
/// Matches CC's `MAX_PTL_RETRIES`.
const MAX_PTL_RETRIES: u32 = 3;

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
}

/// Drop the oldest ~20% of message groups from compaction input to make
/// room for a PTL retry. Returns `None` if nothing can be dropped.
fn truncate_head_for_ptl(messages: &[ConversationMessage]) -> Option<Vec<ConversationMessage>> {
    if messages.len() <= 2 {
        return None;
    }
    // Drop ~20% of messages from the front (excluding the final compaction prompt)
    let drop_count = std::cmp::max(1, (messages.len() - 1) / 5);
    let remaining = &messages[drop_count..];
    if remaining.len() < 2 {
        return None;
    }
    Some(remaining.to_vec())
}

/// Compacts a session using an LLM to produce a high-quality summary.
///
/// Retries transient failures up to [`MAX_COMPACT_RETRIES`] times with
/// exponential backoff. On prompt-too-long errors, truncates the oldest
/// messages and retries up to [`MAX_PTL_RETRIES`] times.
///
/// Falls back to local heuristic compaction when the API client doesn't
/// support `send_compaction`.
pub async fn compact_session<C: ApiClient>(
    session: &Session,
    config: CompactionConfig,
    api_client: &mut C,
    model: &str,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult, CompactionError> {
    if !should_compact(session, config) {
        return Err(CompactionError::NothingToCompact);
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

    // Determine max tokens for compaction output
    let max_tokens = std::cmp::min(
        COMPACT_MAX_OUTPUT_TOKENS,
        crate::model_capabilities::max_output_tokens_or_default(model),
    );

    // Call the LLM with retry logic
    let mut current_messages = compaction_messages;
    let mut ptl_attempts = 0u32;
    let mut last_error = String::new();

    let llm_summary = 'outer: loop {
        for attempt in 0..=MAX_COMPACT_RETRIES {
            match api_client
                .send_compaction(
                    model,
                    COMPACTION_SYSTEM_PROMPT,
                    current_messages.clone(),
                    max_tokens,
                )
                .await
            {
                Ok(summary) => break 'outer summary,
                Err(e) => {
                    last_error = e.to_string();
                    if is_prompt_too_long(&last_error) {
                        ptl_attempts += 1;
                        if ptl_attempts <= MAX_PTL_RETRIES {
                            if let Some(truncated) = truncate_head_for_ptl(&current_messages) {
                                current_messages = truncated;
                                continue 'outer;
                            }
                        }
                        return Err(CompactionError::ApiError(last_error));
                    }
                    if attempt < MAX_COMPACT_RETRIES && is_retryable_error(&last_error) {
                        tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
                        continue;
                    }
                    return Err(CompactionError::ApiError(last_error));
                }
            }
        }
        return Err(CompactionError::ApiError(last_error));
    };

    let summary = merge_compact_summaries(existing_summary.as_deref(), &llm_summary);
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
    compacted_session.record_compaction_with_usage(summary.clone(), removed.len(), compacted_usage);

    Ok(CompactionResult {
        summary,
        formatted_summary,
        compacted_session,
        removed_message_count: removed.len(),
    })
}

/// Synchronous fallback compaction for callers without an async runtime or
/// API client (e.g. the `run_resume_command` path, overflow recovery).
///
/// Uses a simple structural summary instead of an LLM call.
#[must_use]
pub fn compact_session_sync(session: &Session, config: CompactionConfig) -> CompactionResult {
    if !should_compact(session, config) {
        return CompactionResult {
            summary: String::new(),
            formatted_summary: String::new(),
            compacted_session: session.clone(),
            removed_message_count: 0,
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
    compacted_session.record_compaction_with_usage(summary.clone(), removed.len(), compacted_usage);

    CompactionResult {
        summary,
        formatted_summary,
        compacted_session,
        removed_message_count: removed.len(),
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
    let mut k = raw_keep_from;
    loop {
        if k == 0 || k <= compacted_prefix_len {
            break;
        }
        let first_preserved = &session.messages[k];
        let starts_with_tool_result = first_preserved
            .blocks
            .first()
            .is_some_and(|b| matches!(b, ContentBlock::ToolResult { .. }));
        if !starts_with_tool_result {
            break;
        }
        let preceding = &session.messages[k - 1];
        let preceding_has_tool_use = preceding
            .blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
        if preceding_has_tool_use {
            k = k.saturating_sub(1);
            break;
        }
        k = k.saturating_sub(1);
    }
    k
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

        // Unknown model falls back to SSOT default (200K) → 13K
        let unknown = super::autocompact_buffer_tokens("unknown-model-xyz");
        assert_eq!(unknown, 13_000);
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
        assert!(prompt.contains("Your task is to create a detailed summary"));
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

        assert!(second
            .formatted_summary
            .contains("Previously compacted context:"));
        assert!(second
            .formatted_summary
            .contains("Newly compacted context:"));
        assert!(matches!(
            &second.compacted_session.messages[0].blocks[0],
            ContentBlock::Text { text }
                if text.contains("Previously compacted context:")
                    && text.contains("Newly compacted context:")
        ));
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

    #[test]
    fn extract_file_path_from_tool_input_parses_variants() {
        assert_eq!(
            super::extract_file_path_from_tool_input(r#"{"file_path":"/tmp/a.rs"}"#),
            Some("/tmp/a.rs".to_string()),
        );
        assert_eq!(
            super::extract_file_path_from_tool_input(r#"{"filePath":"/tmp/b.rs"}"#),
            Some("/tmp/b.rs".to_string()),
        );
        assert_eq!(
            super::extract_file_path_from_tool_input(r#"{"path":"/tmp/c.rs"}"#),
            Some("/tmp/c.rs".to_string()),
        );
        assert_eq!(super::extract_file_path_from_tool_input("not json"), None,);
    }

    #[test]
    fn read_file_tracker_builds_post_compact_messages() {
        use std::io::Write;

        // Create temp files
        let dir = std::env::temp_dir().join(format!(
            "scode-compact-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let file_a = dir.join("alpha.rs");
        let file_b = dir.join("beta.rs");
        let file_c = dir.join("gamma.rs");
        std::fs::write(&file_a, "fn alpha() {}").unwrap();
        std::fs::write(&file_b, "fn beta() {}").unwrap();
        // gamma is tracked but also in preserved messages — should be skipped
        std::fs::write(&file_c, "fn gamma() {}").unwrap();

        let mut tracker = super::ReadFileTracker::default();
        tracker.record(file_a.clone());
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.record(file_b.clone());
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.record(file_c.clone());

        // Simulate preserved messages containing a read_file tool use for gamma
        let preserved = vec![ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                input: format!(r#"{{"file_path":"{}"}}"#, file_c.display()),
                thought_signature: None,
            },
        ])];

        let messages = tracker.build_post_compact_file_messages(&preserved);

        // gamma should be skipped (already in preserved)
        assert_eq!(
            messages.len(),
            2,
            "should restore alpha and beta, skip gamma"
        );

        // Most recent first: beta before alpha
        let first_text = match &messages[0].blocks[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text block"),
        };
        assert!(first_text.contains("fn beta()"), "most recent file first");
        assert!(first_text.contains("beta.rs"), "should mention filename");

        let second_text = match &messages[1].blocks[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text block"),
        };
        assert!(second_text.contains("fn alpha()"), "second file");

        // Clear should empty the tracker
        let _ = Write::write(&mut std::io::sink(), b"");
        tracker.clear();
        assert!(tracker.build_post_compact_file_messages(&[]).is_empty());

        // Cleanup
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_file_tracker_respects_file_limit() {
        let dir = std::env::temp_dir().join(format!(
            "scode-compact-limit-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut tracker = super::ReadFileTracker::default();
        for i in 0..10 {
            let path = dir.join(format!("file{i}.txt"));
            std::fs::write(&path, format!("content {i}")).unwrap();
            tracker.record(path);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let messages = tracker.build_post_compact_file_messages(&[]);
        assert!(
            messages.len() <= super::POST_COMPACT_MAX_FILES,
            "should respect max files limit, got {}",
            messages.len()
        );

        std::fs::remove_dir_all(&dir).ok();
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
    async fn compact_session_retries_ptl_with_truncation() {
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
        let result = super::compact_session(&session, config, &mut client, "sonnet", None)
            .await
            .expect("should succeed after PTL truncation");

        assert!(result.removed_message_count > 0);
        assert!(result.formatted_summary.contains("PTL recovered"));
        assert!(
            PTL_ATTEMPT.load(Ordering::Relaxed) >= 2,
            "should have made at least 2 attempts"
        );
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

    #[test]
    fn truncate_head_for_ptl_drops_oldest_messages() {
        let messages = vec![
            ConversationMessage::user_text("a"),
            ConversationMessage::user_text("b"),
            ConversationMessage::user_text("c"),
            ConversationMessage::user_text("d"),
            ConversationMessage::user_text("e"),
            ConversationMessage::user_text("prompt"),
        ];

        let truncated = super::truncate_head_for_ptl(&messages).expect("should truncate");
        // 6 messages, drop ~20% = 1 → 5 remaining
        assert_eq!(truncated.len(), 5);
        // First dropped message was "a"
        assert!(matches!(
            &truncated[0].blocks[0],
            ContentBlock::Text { text } if text == "b"
        ));
    }

    #[test]
    fn truncate_head_for_ptl_returns_none_for_tiny_input() {
        let messages = vec![
            ConversationMessage::user_text("only"),
            ConversationMessage::user_text("two"),
        ];
        assert!(super::truncate_head_for_ptl(&messages).is_none());
    }
}
