//! Pure formatting and report functions extracted from `main.rs`.

use std::fmt::Write as _;

use engine_core::{AuthMode, ProviderKind};
use runtime::{self, TokenUsage};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Display width helpers
// ---------------------------------------------------------------------------

/// Compute the display width of a string, stripping ANSI escape sequences
/// and accounting for unicode character widths (CJK = 2 columns, etc.).
///
/// This is the single source of truth for terminal column calculations.
/// Use this instead of `chars().count()` or `.len()` whenever sizing
/// borders, padding, or alignment.
pub(crate) fn display_width(s: &str) -> usize {
    strip_ansi_codes(s).width()
}

/// Strip ANSI SGR escape sequences (`ESC[...m`) from a string.
fn strip_ansi_codes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

use crate::render::{ansi_bold_fg, ansi_fg, theme, BOLD, DIM, RESET};
use crate::{
    load_sudocode_config_for_current_dir, GitWorkspaceSummary, InternalPromptProgressEvent,
    InternalPromptProgressState, BUILD_TARGET, DEFAULT_DATE, GIT_SHA, LATEST_SESSION_REFERENCE,
    PRIMARY_SESSION_EXTENSION, VERSION,
};

// ---------------------------------------------------------------------------
// Unified message rendering pipeline
// ---------------------------------------------------------------------------

/// Render a single `ConversationMessage` into styled terminal output.
///
/// This is the SSOT for "how does a completed message look on screen."
/// Both session replay (`--resume`) and any future message display
/// (e.g. `/history`, export) should call this instead of hand-rolling
/// role/block matching.
///
/// The live REPL uses a different path (streaming event callbacks) for
/// progressive rendering during a turn, but the *final* visual result
/// is the same because both paths call the same per-block format
/// functions (`format_input_echo`, `format_tool_call_start`, etc.).
pub(crate) fn render_message(
    msg: &runtime::ConversationMessage,
    term_width: usize,
    renderer: &crate::render::TerminalRenderer,
) -> Option<String> {
    let mut out = String::new();

    match msg.role {
        runtime::MessageRole::User => {
            let text = text_from_blocks(&msg.blocks);
            if text.is_empty() {
                return None;
            }
            let sep = format!("{DIM}{}{RESET}", "─".repeat(term_width));
            out.push_str(&sep);
            out.push('\n');
            let (echo, _) = format_input_echo(&text, term_width);
            out.push_str(&echo);
            out.push('\n');
            out.push_str(&sep);
        }
        runtime::MessageRole::Assistant => {
            for block in &msg.blocks {
                match block {
                    runtime::ContentBlock::Text { text } if !text.is_empty() => {
                        let rendered = renderer.render_markdown(text);
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&rendered);
                    }
                    runtime::ContentBlock::ToolUse { name, input, .. } => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&format_tool_call_start(name, input));
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                return None;
            }
        }
        runtime::MessageRole::Tool => {
            for block in &msg.blocks {
                if let runtime::ContentBlock::ToolResult {
                    tool_name,
                    output,
                    is_error,
                    ..
                } = block
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    // Session replay renders each ToolResult block on its own;
                    // the matching call's input isn't threaded here, so pass
                    // empty (extractors fall back to the result payload — same
                    // as before input threading existed). Live turns supply the
                    // input via the render engine's per-turn id→input map.
                    out.push_str(&format_tool_result(tool_name, "", output, *is_error));
                }
            }
            if out.is_empty() {
                return None;
            }
        }
        runtime::MessageRole::System => return None,
    }

    Some(out)
}

/// Render a slice of messages into a single string. Convenience wrapper
/// over [`render_message`] for session replay.
pub(crate) fn render_messages(
    messages: &[runtime::ConversationMessage],
    term_width: usize,
    renderer: &crate::render::TerminalRenderer,
) -> String {
    let mut parts = Vec::new();
    for msg in messages {
        if let Some(rendered) = render_message(msg, term_width, renderer) {
            parts.push(rendered);
        }
    }
    parts.join("\n")
}

/// `true` for Text blocks the runtime injected (date announcements,
/// rollover reminders, task notifications) rather than the user typing.
/// These travel to the model inside user messages but must not surface
/// in user-facing echoes: transcript replay and ↑-history recall.
pub(crate) fn is_system_reminder_text(text: &str) -> bool {
    text.trim_start().starts_with("<system-reminder>")
}

/// Extract concatenated text content from a message's blocks, skipping
/// runtime-injected `<system-reminder>` blocks (see
/// [`is_system_reminder_text`]).
fn text_from_blocks(blocks: &[runtime::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            runtime::ContentBlock::Text { text } if !is_system_reminder_text(text) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// NOTE: DISPLAY_TRUNCATION_NOTICE uses DIM + RESET which are theme-invariant
// constants. Expressed via concat! so the value can remain a `const`.
pub(crate) const DISPLAY_TRUNCATION_NOTICE: &str = concat!(
    "\x1b[2m",
    "… output truncated for display; full result preserved in session.",
    "\x1b[0m"
);
pub(crate) const READ_DISPLAY_MAX_LINES: usize = 10;
pub(crate) const READ_DISPLAY_MAX_CHARS: usize = 2_000;
/// Default upper bound on lines shown inline when summarizing tool results.
/// Anything beyond this is replaced with a "+N more lines" notice; the full
/// result is still preserved in the session file.
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 15;
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;
/// Longest single line shown inline. A JSON-escaped blob or a minified file
/// is one "line" that wraps across dozens of terminal rows; past this it is
/// cut with an ellipsis. The full result is still in the session file.
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_LINE_CHARS: usize = 200;

pub(crate) fn provider_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Xai => "xai",
        ProviderKind::OpenAi => "openai",
        ProviderKind::Codex => "codex",
        ProviderKind::Gemini => "gemini",
    }
}

pub(crate) fn format_connected_line(model: &str) -> String {
    format_connected_line_with_mode(model, None)
}

pub(crate) fn format_connected_line_with_mode(model: &str, mode: Option<AuthMode>) -> String {
    let config = load_sudocode_config_for_current_dir();
    format_connected_line_with_config(model, mode, &config)
}

pub(crate) fn format_connected_line_with_config(
    model: &str,
    mode: Option<AuthMode>,
    sudocode_config: &engine_core::SudoCodeConfig,
) -> String {
    // Try to get provider label from sudocode.json config.
    let resolved_mode = mode.or_else(|| {
        // Auto-detect from model config: first available in priority order.
        const PRIORITY: &[&str] = &["subscription", "proxy", "api-key"];
        let entry = engine_core::resolve_model(sudocode_config, model)?;
        let mode_str = PRIORITY
            .iter()
            .find(|m| entry.providers.contains_key(**m))?;
        AuthMode::parse(mode_str).ok()
    });
    let provider = {
        // Look up provider name from config entry's mapping for the resolved mode.
        let mode_key = resolved_mode.map(|m| m.label().to_string());
        engine_core::resolve_model(sudocode_config, model)
            .and_then(|entry| {
                let mapping = if let Some(key) = &mode_key {
                    entry.providers.get(key.as_str())
                } else {
                    entry.providers.values().next()
                };
                mapping.map(|m| m.provider.clone())
            })
            .unwrap_or_else(|| model.to_string())
    };
    let auth_hint = match resolved_mode {
        Some(m) => format!(" ({})", m.label()),
        None => String::new(),
    };
    let base_url = match mode {
        Some(m) => engine_core::base_url_for_mode(m),
        None => engine_core::read_base_url(),
    };
    let endpoint_hint = if base_url == engine_core::DEFAULT_BASE_URL {
        String::new()
    } else {
        format!("\nEndpoint:  {base_url}")
    };
    format!("Connected: {model} via {provider}{auth_hint}{endpoint_hint}")
}

// The model / compact / sandbox report formatters now live in
// `commands::reports` (shared with the ACP renderer so both render the same
// reports from one definition). `format_model_report` gained a `config`
// parameter there so it stays a pure formatter: the caller loads the config.
pub(crate) use commands::reports::{
    format_acp_compact_report, format_model_report, format_model_switch_report,
    format_sandbox_report,
};

pub(crate) fn format_permissions_report(mode: &str) -> String {
    let modes = [
        ("read-only", "Read/search tools only", mode == "read-only"),
        (
            "workspace-write",
            "Edit files inside the workspace",
            mode == "workspace-write",
        ),
        (
            "danger-full-access",
            "Unrestricted tool access",
            mode == "danger-full-access",
        ),
    ]
    .into_iter()
    .map(|(name, description, is_current)| {
        let marker = if is_current {
            "● current"
        } else {
            "○ available"
        };
        format!("  {name:<18} {marker:<11} {description}")
    })
    .collect::<Vec<_>>()
    .join(
        "
",
    );

    format!(
        "Permissions
  Active mode      {mode}
  Mode status      live session default

Modes
{modes}

Usage
  Inspect current mode with /permissions
  Switch modes with /permissions <mode>"
    )
}

pub(crate) fn format_permissions_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Permissions updated
  Result           mode switched
  Previous mode    {previous}
  Active mode      {next}
  Applies to       subsequent tool calls
  Usage            /permissions to inspect current mode"
    )
}

pub(crate) fn format_auth_report(current: &str) -> String {
    let modes = [
        (
            "subscription",
            "OAuth subscription token",
            current == "subscription",
        ),
        ("proxy", "Proxy bearer token", current == "proxy"),
        ("api-key", "Direct API key", current == "api-key"),
    ]
    .into_iter()
    .map(|(name, description, is_current)| {
        let marker = if is_current {
            "● current"
        } else {
            "○ available"
        };
        format!("  {name:<18} {marker:<11} {description}")
    })
    .collect::<Vec<_>>()
    .join(
        "
",
    );

    format!(
        "Auth
  Active mode      {current}
  Mode status      live session default

Modes
{modes}

Usage
  Inspect current mode with /auth
  Switch modes with /auth <mode>"
    )
}

pub(crate) fn format_auth_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Auth updated
  Result           mode switched
  Previous mode    {previous}
  Active mode      {next}
  Applies to       subsequent API calls
  Usage            /auth to inspect current mode"
    )
}

/// Render `/account`: who pays, what chose them, and what else is on offer.
///
/// `current` is the rendered [`engine_host::BillingAccount`] line, so the
/// deciding rule travels with the name — an account nobody selected reads very
/// differently from one this project asked for, and that difference is the
/// whole reason to look.
pub(crate) fn format_account_report(
    current: &str,
    current_name: Option<&str>,
    available: &[String],
) -> String {
    let accounts = if available.is_empty() {
        "  (none configured under auth_modes.proxy in sudocode.json)".to_string()
    } else {
        available
            .iter()
            .map(|name| {
                let marker = if Some(name.as_str()) == current_name {
                    "● current"
                } else {
                    "○ available"
                };
                format!("  {name:<18} {marker}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "Account
  Billed to        {current}
  Defined in       auth_modes.proxy in sudocode.json
  Selection scope  this project (.nexus/sudocode/settings.local.json)

Accounts
{accounts}

Usage
  Inspect current account with /account
  Switch accounts with /account <name>"
    )
}

pub(crate) fn format_account_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Account updated
  Result           account switched
  Previous account {previous}
  Billed to        {next}
  Applies to       subsequent API calls
  Persisted to     this project's settings.local.json
  Usage            /account to inspect current account"
    )
}

pub(crate) fn format_cost_report(usage: TokenUsage) -> String {
    format!(
        "Cost
  Input tokens     {}
  Output tokens    {}
  Cache create     {}
  Cache read       {}
  Total tokens     {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        usage.total_tokens(),
    )
}

pub(crate) fn format_resume_report(session_path: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Session resumed
  Session file     {session_path}
  Messages         {message_count}
  Turns            {turns}"
    )
}

pub(crate) fn render_resume_usage() -> String {
    format!(
        "Resume
  Usage            /resume <session-path|session-id|{LATEST_SESSION_REFERENCE}>
  Auto-save        .scode/sessions/<session-id>.{PRIMARY_SESSION_EXTENSION}
  Tip              use /session list to inspect saved sessions"
    )
}

pub(crate) fn format_compact_report(
    removed: usize,
    resulting_messages: usize,
    skipped: bool,
    summary_source: &runtime::CompactionSummarySource,
) -> String {
    if skipped {
        format!(
            "Compact
  Result           skipped
  Reason           session below compaction threshold
  Messages kept    {resulting_messages}"
        )
    } else {
        format!(
            "Compact
  Result           compacted
  Messages removed {removed}
  Messages kept    {resulting_messages}
  Summary          {summary_source}"
        )
    }
}

pub(crate) fn format_auto_compaction_notice(removed: usize) -> String {
    format!("[auto-compacted: removed {removed} messages]")
}

pub(crate) fn format_commit_preflight_report(
    branch: Option<&str>,
    summary: GitWorkspaceSummary,
) -> String {
    format!(
        "Commit
  Result           ready
  Branch           {}
  Workspace        {}
  Changed files    {}
  Action           create a git commit from the current workspace changes",
        branch.unwrap_or("unknown"),
        summary.headline(),
        summary.changed_files,
    )
}

pub(crate) fn format_commit_skipped_report() -> String {
    "Commit
  Result           skipped
  Reason           no workspace changes
  Action           create a git commit from the current workspace changes
  Next             /status to inspect context · /diff to inspect repo changes"
        .to_string()
}

pub(crate) fn format_bughunter_report(scope: Option<&str>) -> String {
    format!(
        "Bughunter
  Scope            {}
  Action           inspect the selected code for likely bugs and correctness issues
  Output           findings should include file paths, severity, and suggested fixes",
        scope.unwrap_or("the current repository")
    )
}

pub(crate) fn format_ultraplan_report(task: Option<&str>) -> String {
    format!(
        "Ultraplan
  Task             {}
  Action           break work into a multi-step execution plan
  Output           plan should cover goals, risks, sequencing, verification, and rollback",
        task.unwrap_or("the current repo work")
    )
}

pub(crate) fn format_pr_report(branch: &str, context: Option<&str>) -> String {
    format!(
        "PR
  Branch           {branch}
  Context          {}
  Action           draft or create a pull request for the current branch
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

pub(crate) fn format_issue_report(context: Option<&str>) -> String {
    format!(
        "Issue
  Context          {}
  Action           draft or create a GitHub issue from the current context
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

pub(crate) fn render_version_report() -> String {
    let git_sha = GIT_SHA.unwrap_or("unknown");
    let target = BUILD_TARGET.unwrap_or("unknown");
    format!(
        "Sudo Code\n  Version          {VERSION}\n  Git SHA          {git_sha}\n  Target           {target}\n  Build date       {DEFAULT_DATE}"
    )
}

pub(crate) fn format_internal_prompt_progress_line(
    event: InternalPromptProgressEvent,
    snapshot: &InternalPromptProgressState,
    elapsed: Duration,
    error: Option<&str>,
) -> String {
    let elapsed_seconds = elapsed.as_secs();
    let step_label = if snapshot.step == 0 {
        "current step pending".to_string()
    } else {
        format!("current step {}", snapshot.step)
    };
    let mut status_bits = vec![step_label, format!("phase {}", snapshot.phase)];
    if let Some(detail) = snapshot
        .detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
    {
        status_bits.push(detail.to_string());
    }
    let status = status_bits.join(" · ");
    match event {
        InternalPromptProgressEvent::Started => {
            format!(
                "🧭 {} status · planning started · {status}",
                snapshot.command_label
            )
        }
        InternalPromptProgressEvent::Update => {
            format!("… {} status · {status}", snapshot.command_label)
        }
        InternalPromptProgressEvent::Heartbeat => format!(
            "… {} heartbeat · {elapsed_seconds}s elapsed · {status}",
            snapshot.command_label
        ),
        InternalPromptProgressEvent::Complete => format!(
            "✔ {} status · completed · {elapsed_seconds}s elapsed · {} steps total",
            snapshot.command_label, snapshot.step
        ),
        InternalPromptProgressEvent::Failed => format!(
            "✘ {} status · failed · {elapsed_seconds}s elapsed · {}",
            snapshot.command_label,
            error.unwrap_or("unknown error")
        ),
    }
}

/// Render the REPL echo for a submitted user input. Each line is on its own
/// row, padded out to `term_width` and wrapped in the gray-background SGR pair
/// the REPL uses for echoed input.
///
/// `lines_consumed` is the number of `\n`-delimited input lines so callers
/// know how many rows of rustyline output to clear before printing the echo.
///
/// #182 item 3: multi-line input used to collapse to a single line because
/// the call site did `replace('\n', " ")`. We now preserve every line so the
/// echo matches what the user actually typed.
pub(crate) fn format_input_echo(input: &str, term_width: usize) -> (String, usize) {
    let trimmed = input.trim();
    // `split('\n')` (not `lines()`) so an input that ends with `\n` still
    // contributes a trailing empty echo row — rustyline drew one for it.
    let raw_lines: Vec<&str> = if trimmed.is_empty() {
        vec![""]
    } else {
        trimmed.split('\n').collect()
    };
    let code_bg = theme().code_bg;
    let mut rendered = String::new();
    for (idx, line) in raw_lines.iter().enumerate() {
        let prefix = if idx == 0 { " › " } else { "   " };
        let body = format!("{prefix}{line}");
        let visible = display_width(&body);
        let pad = term_width.saturating_sub(visible);
        if idx > 0 {
            rendered.push('\n');
        }
        rendered.push_str(&format!("\x1b[48;5;{code_bg}m"));
        rendered.push_str(&body);
        if pad > 0 {
            rendered.push_str(&" ".repeat(pad));
        }
        rendered.push_str(RESET);
    }
    (rendered, raw_lines.len())
}

pub(crate) fn describe_tool_progress(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));
    match name {
        "bash" | "Bash" => {
            let command = parsed
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if command.is_empty() {
                "running shell command".to_string()
            } else {
                format!("command {}", truncate_for_summary(command.trim(), 100))
            }
        }
        "read_file" | "Read" => format!("reading {}", extract_tool_path(&parsed)),
        "write_file" | "Write" => format!("writing {}", extract_tool_path(&parsed)),
        "edit_file" | "Edit" => format!("editing {}", extract_tool_path(&parsed)),
        "glob_search" | "Glob" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("glob `{pattern}` in {scope}")
        }
        "grep_search" | "Grep" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("grep `{pattern}` in {scope}")
        }
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .map_or_else(
                || "running web search".to_string(),
                |query| format!("query {}", truncate_for_summary(query, 100)),
            ),
        _ => {
            let summary = summarize_tool_payload(input);
            if summary.is_empty() {
                format!("running {name}")
            } else {
                format!("{name}: {summary}")
            }
        }
    }
}

pub(crate) fn format_tool_call_start(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    let detail = match name {
        "bash" | "Bash" => format_bash_call(&parsed),
        "read_file" | "Read" => {
            let path = extract_tool_path(&parsed);
            format!("{DIM}📄 Reading {path}…{RESET}")
        }
        "write_file" | "Write" => {
            let path = extract_tool_path(&parsed);
            let lines = parsed
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            let success = ansi_bold_fg(theme().success);
            format!("{success}✏️ Writing {path}{RESET} {DIM}({lines} lines){RESET}")
        }
        "edit_file" | "Edit" => {
            let path = extract_tool_path(&parsed);
            let old_value = parsed
                .get("old_string")
                .or_else(|| parsed.get("oldString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let new_value = parsed
                .get("new_string")
                .or_else(|| parsed.get("newString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let warning = ansi_bold_fg(theme().warning);
            format!(
                "{warning}📝 Editing {path}{RESET}{}",
                format_patch_preview(old_value, new_value)
                    .map(|preview| format!("\n{preview}"))
                    .unwrap_or_default()
            )
        }
        "glob_search" | "Glob" => format_search_start("🔎 Glob", &parsed),
        "grep_search" | "Grep" => format_search_start("🔎 Grep", &parsed),
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => summarize_tool_payload(input),
    };

    // A tool call in flight is the Running state of the same card that
    // `format_tool_result` later renders on completion: same L-frame, colored
    // yellow. The tool name (bold info color) is the header; the summary detail
    // is the body. One renderer for both moments is the SSOT that makes command
    // header and result visually identical.
    let cn = ansi_bold_fg(theme().info);
    let header = format!("{cn}{name}{RESET}");
    let content = if detail.is_empty() {
        ToolCardContent::header_only(header)
    } else {
        ToolCardContent::new(header, detail)
    };
    render_tool_card(&content, ToolStatus::Running)
}

/// Split a `ToolResult.output` string into its JSON-payload prefix and any
/// trailing hook-feedback section that `runtime::conversation::merge_hook_feedback`
/// may have appended. Returns `(payload, Some(feedback))` when a marker is
/// found, or `(output, None)` otherwise.
pub(crate) fn split_hook_feedback(output: &str) -> (&str, Option<&str>) {
    const MARKERS: &[&str] = &["\n\nHook feedback:\n", "\n\nHook feedback (error):\n"];
    for marker in MARKERS {
        if let Some(idx) = output.find(marker) {
            let (head, tail) = output.split_at(idx);
            return (head, Some(tail.trim_start_matches('\n')));
        }
    }
    (output, None)
}

/// Execution status of a tool call. Single source of truth for the
/// success/error/running color semantics: it drives the left-frame color in
/// [`render_tool_card`] and nothing else encodes "did this tool succeed."
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ToolStatus {
    /// In flight — yellow frame. Rendered in the staging area.
    Running,
    /// Completed successfully — green frame.
    Ok,
    /// Failed / denied / cancelled — red frame.
    Error,
}

/// The *semantic* content of a tool card, free of any frame, prefix, or
/// status decoration. Per-tool extractors (`bash_card`, `read_card`, …)
/// produce this; [`render_tool_card`] is the single place that adds the
/// status-colored L-frame. New tools only supply a header (and optional
/// body) and inherit the unified look automatically.
pub(crate) struct ToolCardContent {
    /// First line: tool identity + summary. May carry emoji and the tool's
    /// own identity color (bash muted, write green, edit warning, …).
    pub header: String,
    /// Optional multi-line body, already styled/highlighted, with **no**
    /// leading prefix — `render_tool_card` owns the frame.
    pub body: Option<String>,
}

impl ToolCardContent {
    fn header_only(header: String) -> Self {
        Self { header, body: None }
    }

    fn new(header: String, body: String) -> Self {
        Self {
            header,
            body: Some(body),
        }
    }
}

/// SSOT for how a tool call looks on screen: an L-frame whose color carries
/// the status. `╭─ header` / `│ body…` / `╰─`, colored yellow (running),
/// green (ok), or red (error).
///
/// Deliberately never draws a right border: tool output carries ANSI, tabs,
/// and CJK width, so a closed box's right edge is unreliable — a left frame
/// keeps output copy-pasteable and pipe-safe, matching Sudo Code's scrollback
/// ethos. The frame carries the status color; the tool name inside `header`
/// keeps its own identity color. There is no `⏺` glyph — the frame is the cue.
pub(crate) fn render_tool_card(content: &ToolCardContent, status: ToolStatus) -> String {
    use std::fmt::Write as _;
    let t = theme();
    let frame = match status {
        // Running uses the brand amber (primary), NOT warning/teal: teal read
        // too close to the success green, so an in-flight card looked already
        // done. Amber vs green vs red now reads at a glance.
        ToolStatus::Running => ansi_bold_fg(t.primary),
        ToolStatus::Ok => ansi_bold_fg(t.success),
        ToolStatus::Error => ansi_bold_fg(t.error),
    };
    let top = format!("{frame}\u{256d}\u{2500}{RESET}");
    let bar = format!("{frame}\u{2502}{RESET}");
    let bottom = format!("{frame}\u{2570}\u{2500}{RESET}");
    // Wrap every content line to the width left after the 2-column `│ ` prefix,
    // then prefix each wrapped segment with the bar. A line longer than the
    // terminal would otherwise be wrapped by the terminal itself, and that
    // continuation would carry no `│` — spilling past the left frame. Wrapping
    // here (not truncating) keeps all content and keeps every visible row
    // inside the frame. This is the single place that guarantees framed output
    // stays framed — extractors need not each reason about width.
    let term_width = crossterm::terminal::size().map_or(80, |(cols, _)| cols as usize);
    let content_width = term_width.saturating_sub(2).max(1);
    let mut out = String::new();
    for (i, seg) in wrap_ansi_to_width(&content.header, content_width)
        .iter()
        .enumerate()
    {
        let prefix = if i == 0 { &top } else { &bar };
        if i > 0 {
            out.push('\n');
        }
        let _ = write!(out, "{prefix} {seg}");
    }
    if let Some(body) = &content.body {
        for line in body.lines() {
            for seg in wrap_ansi_to_width(line, content_width) {
                let _ = write!(out, "\n{bar} {seg}");
            }
        }
    }
    let _ = write!(out, "\n{bottom}");
    out
}

/// Hard-wrap a possibly-ANSI-styled string to `width` visible columns,
/// returning one string per wrapped row. ANSI SGR escapes are copied verbatim
/// and don't count toward width; each row is closed with `RESET` so a color
/// opened before the break doesn't bleed past the frame bar of the next row.
/// An empty input yields one empty row (so a blank body line still renders a
/// framed blank row rather than vanishing).
fn wrap_ansi_to_width(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut rows = Vec::new();
    let mut cur = String::new();
    let mut vis = 0usize;
    let mut carried_style = false;
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            // Copy the whole CSI sequence verbatim (ESC [ ... final-byte).
            cur.push(ch);
            if chars.peek() == Some(&'[') {
                cur.push(chars.next().unwrap());
                for c in chars.by_ref() {
                    cur.push(c);
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            carried_style = true;
            continue;
        }
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if vis + ch_w > width && vis > 0 {
            if carried_style {
                cur.push_str(RESET);
            }
            rows.push(std::mem::take(&mut cur));
            vis = 0;
        }
        cur.push(ch);
        vis += ch_w;
    }
    rows.push(cur);
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

pub(crate) fn format_tool_result(name: &str, input: &str, output: &str, is_error: bool) -> String {
    let t = theme();
    let muted = ansi_fg(t.muted);
    let (payload, hook_feedback) = split_hook_feedback(output);
    let status = if is_error {
        ToolStatus::Error
    } else {
        ToolStatus::Ok
    };
    let mut content = if is_error {
        let summary = truncate_for_summary(output.trim(), 160);
        let removed = ansi_fg(t.diff_removed);
        if summary.is_empty() {
            ToolCardContent::header_only(format!("{muted}{name}{RESET}"))
        } else {
            ToolCardContent::new(
                format!("{muted}{name}{RESET}"),
                format!("{removed}{summary}{RESET}"),
            )
        }
    } else {
        // Every card is built from BOTH the tool's `input` (the arguments the
        // model sent — command, path, oldString…) and its `output` (the
        // result — stdout, diff, content). The header's identity fields live
        // in `input`; the body lives in `output`. Passing both to every
        // extractor is the uniform contract that stops a tool from silently
        // reading a field out of the wrong payload (bash's command and edit's
        // path are ONLY in input, never in output).
        let in_val: serde_json::Value =
            serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
        let out_val: serde_json::Value =
            serde_json::from_str(payload).unwrap_or(serde_json::Value::String(payload.to_string()));
        match name {
            "bash" | "Bash" => bash_card(&in_val, &out_val),
            "read_file" | "Read" => read_card(&in_val, &out_val),
            "write_file" | "Write" => write_card(&in_val, &out_val),
            "edit_file" | "Edit" => edit_card(&in_val, &out_val),
            "glob_search" | "Glob" => glob_card(&in_val, &out_val),
            "grep_search" | "Grep" => grep_card(&in_val, &out_val),
            "Skill" => skill_card(&in_val, &out_val),
            "read_tool_output" => read_tool_output_card(&in_val, &out_val),
            _ => generic_tool_card(name, &in_val, &out_val),
        }
    };
    if let (Some(feedback), false) = (hook_feedback, is_error) {
        let hf = ansi_fg(t.hook_feedback);
        let feedback_body = format!("{hf}{feedback}{RESET}");
        content.body = Some(match content.body {
            Some(body) => format!("{body}\n{feedback_body}"),
            None => feedback_body,
        });
    }
    render_tool_card(&content, status)
}

/// Read a string field that may live in the tool's `input` (arguments) or its
/// `output` (result), preferring `input` — that is the authoritative record of
/// what the model asked for, and some tools (bash, edit) never echo it back in
/// the result. Falls back to `output`, then `""`.
#[inline]
fn field<'a>(input: &'a serde_json::Value, output: &'a serde_json::Value, key: &str) -> &'a str {
    input
        .get(key)
        .or_else(|| output.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
}

/// Like [`field`] but tries several keys in order (e.g. snake_case vs
/// camelCase across the input/output boundary).
#[inline]
fn field_any<'a>(
    input: &'a serde_json::Value,
    output: &'a serde_json::Value,
    keys: &[&str],
) -> &'a str {
    for k in keys {
        let v = input
            .get(k)
            .or_else(|| output.get(k))
            .and_then(|v| v.as_str());
        if let Some(s) = v {
            return s;
        }
    }
    ""
}

pub(crate) fn extract_tool_path(parsed: &serde_json::Value) -> String {
    parsed
        .get("file_path")
        .or_else(|| parsed.get("filePath"))
        .or_else(|| parsed.get("path"))
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string()
}

pub(crate) fn format_search_start(label: &str, parsed: &serde_json::Value) -> String {
    let pattern = parsed
        .get("pattern")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let scope = parsed
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or(".");
    format!("{label} {pattern}\n{DIM}in {scope}{RESET}")
}

pub(crate) fn format_patch_preview(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    let removed = ansi_fg(theme().diff_removed);
    let added = ansi_fg(theme().diff_added);
    Some(format!(
        "{removed}- {}{RESET}\n{added}+ {}{RESET}",
        truncate_for_summary(first_visible_line(old_value), 72),
        truncate_for_summary(first_visible_line(new_value), 72)
    ))
}

pub(crate) fn format_bash_call(parsed: &serde_json::Value) -> String {
    let command = parsed
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if command.is_empty() {
        String::new()
    } else {
        let code_bg = theme().code_bg;
        format!(
            "\x1b[48;5;{code_bg};38;5;255m $ {} {RESET}",
            truncate_for_summary(command, 160)
        )
    }
}

pub(crate) fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

pub(crate) fn bash_card(input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    use std::fmt::Write as _;

    // Command lives in `input` (never echoed in the result payload).
    let command = field(input, output, "command");

    let muted = ansi_fg(theme().muted);
    let mut header = if command.is_empty() {
        format!("{muted}Bash{RESET}")
    } else {
        format!("{muted}Bash{RESET}({})", truncate_for_summary(command, 120))
    };

    // Background id / return-code interpretation live in `output`.
    if let Some(task_id) = output
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
    {
        write!(&mut header, " backgrounded ({task_id})").expect("write to string");
    } else if let Some(status) = output
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        write!(&mut header, " {status}").expect("write to string");
    }

    let stdout_text = output
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let stderr_text = output
        .get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    stdout_stderr_card(header, stdout_text, stderr_text)
}

/// Shared stdout/stderr body builder used by [`bash_card`]. Combines the two
/// streams, drops blank lines, and applies the per-tool line cap from
/// `TOOL_OUTPUT_DISPLAY_MAX_LINES`. Returns a [`ToolCardContent`]; the L-frame
/// prefix is applied later by [`render_tool_card`].
fn stdout_stderr_card(header: String, stdout: &str, stderr: &str) -> ToolCardContent {
    use std::fmt::Write as _;

    let all_output: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();

    if all_output.is_empty() {
        return ToolCardContent::header_only(header);
    }

    let term_width = crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80);
    // 4 = frame + space prefix applied by render_tool_card, plus safety margin
    // to avoid wrapping.
    let max_content_width = term_width.saturating_sub(6);

    let preview_count = TOOL_OUTPUT_DISPLAY_MAX_LINES;
    let mut body = String::new();

    for (i, line) in all_output.iter().take(preview_count).enumerate() {
        let truncated = truncate_to_width(line, max_content_width);
        if i > 0 {
            body.push('\n');
        }
        body.push_str(&truncated);
    }

    if all_output.len() > preview_count {
        let remaining = all_output.len() - preview_count;
        let line_or_lines = if remaining == 1 { "line" } else { "lines" };
        write!(
            &mut body,
            "\n{DIM}… +{remaining} more {line_or_lines} · full output preserved in session{RESET}"
        )
        .expect("write to string");
    }

    ToolCardContent::new(header, body)
}

/// Truncate a string to fit within `max_width` display columns, appending `…`
/// if truncated. Strips ANSI codes for width calculation but preserves them in
/// output up to the cut point.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let stripped = strip_ansi_codes(s);
    if UnicodeWidthStr::width(stripped.as_str()) <= max_width {
        return s.to_string();
    }
    // Walk characters of the stripped version, accumulating display width.
    let mut width = 0usize;
    let mut byte_end = 0usize;
    for ch in stripped.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width + 1 > max_width {
            // +1 for the trailing `…`
            break;
        }
        width += ch_width;
        byte_end += ch.len_utf8();
    }
    format!("{}…", &stripped[..byte_end])
}

pub(crate) fn read_card(input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    let file = output.get("file").unwrap_or(output);
    // Path is authoritative in `input`; fall back to the result envelope.
    let path = {
        let p = extract_tool_path(input);
        if p == "?" {
            extract_tool_path(file)
        } else {
            p
        }
    };
    let content = file
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    // runtime `TextFilePayload` serializes as camelCase via `#[serde(rename
    // = "totalLines")]`. Snake_case kept as a defensive fallback in case the
    // wire format is normalized later.
    let total_lines = file
        .get("totalLines")
        .or_else(|| file.get("total_lines"))
        .and_then(serde_json::Value::as_u64);
    let mut header = format!("{DIM}Read {path}{RESET}");
    if let Some(total) = total_lines {
        let _ = write!(header, " {DIM}({total} lines){RESET}");
    }
    if content.is_empty() {
        return ToolCardContent::header_only(header);
    }

    // Cap to READ_DISPLAY_* lines; read_file results commonly run hundreds of
    // lines and overwhelm the terminal otherwise.
    //
    // CRITICAL: do not pre-format a notice into the body before highlighting.
    // syntect treats `\x1b` as a literal codepoint, splits it from the
    // following `[2m`, and wraps each side with its own escape — the terminal
    // then consumes the loose `\x1b` and renders `[2m` as plain text. Compute
    // truncation against the raw body, highlight the visible-only slice, and
    // append the (already-styled) notice afterwards.
    let lines_with_endings: Vec<&str> = content.split_inclusive('\n').collect();
    let total_input_lines = if content.is_empty() {
        0
    } else if content.ends_with('\n') {
        lines_with_endings.len()
    } else {
        // `split_inclusive` keeps the final partial line; it still counts.
        lines_with_endings.len()
    };
    let visible_count = total_input_lines.min(READ_DISPLAY_MAX_LINES);
    let mut visible_body = String::new();
    let mut char_budget = READ_DISPLAY_MAX_CHARS;
    let mut char_truncated = false;
    for line in lines_with_endings.iter().take(visible_count) {
        let line_chars = line.chars().count();
        if line_chars > char_budget {
            visible_body.extend(line.chars().take(char_budget));
            char_truncated = true;
            break;
        }
        visible_body.push_str(line);
        char_budget = char_budget.saturating_sub(line_chars);
    }
    if visible_body.is_empty() {
        return ToolCardContent::header_only(header);
    }
    let language = language_token_from_path(&path);
    let renderer = crate::render::TerminalRenderer::new();
    let highlighted = renderer.highlight_code(&visible_body, language);
    let mut body = highlighted.lines().collect::<Vec<_>>().join("\n");

    let remaining_lines = total_input_lines.saturating_sub(visible_count);
    if remaining_lines > 0 {
        let line_or_lines = if remaining_lines == 1 {
            "line"
        } else {
            "lines"
        };
        let _ = write!(
            body,
            "\n{DIM}… +{remaining_lines} more {line_or_lines} · full output preserved in session{RESET}"
        );
    } else if char_truncated {
        let _ = write!(body, "\n{DISPLAY_TRUNCATION_NOTICE}");
    }

    ToolCardContent::new(header, body)
}

/// Derive a syntect-friendly language token from a filename.
///
/// `find_syntax_by_token` matches both extensions (e.g. `"rs"`) and language
/// names; an empty string makes it fall back to plain text.
pub(crate) fn language_token_from_path(path: &str) -> &str {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
}

/// Max number of `-`/`+` lines shown per side before the body is collapsed.
pub(crate) const DIFF_PREVIEW_MAX_BODY_LINES: usize = 8;
/// Lines of unchanged context shown above and below the edit window.
pub(crate) const DIFF_PREVIEW_CONTEXT_LINES: usize = 3;

pub(crate) fn write_card(input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    // Path is authoritative in `input`; the body (type/content/originalFile)
    // comes from the result envelope.
    let path = {
        let p = extract_tool_path(input);
        if p == "?" {
            extract_tool_path(output)
        } else {
            p
        }
    };
    let kind = output
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("write");
    let new_content = output
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let new_line_count = new_content.lines().count();
    let original = output.get("originalFile").and_then(|value| value.as_str());
    let verb = if kind == "create" { "Wrote" } else { "Updated" };
    let success = ansi_bold_fg(theme().success);
    let header = match original {
        Some(prev) if kind != "create" => {
            let prev_lines = prev.lines().count();
            let delta = new_line_count.cast_signed() - prev_lines.cast_signed();
            let delta_str = match delta.cmp(&0) {
                std::cmp::Ordering::Greater => format!(" +{delta}"),
                std::cmp::Ordering::Less => format!(" {delta}"),
                std::cmp::Ordering::Equal => String::new(),
            };
            format!(
                "{success}✏️ {verb} {path}{RESET} {DIM}({new_line_count} lines, was {prev_lines}{delta_str}){RESET}",
            )
        }
        _ => {
            format!("{success}✏️ {verb} {path}{RESET} {DIM}({new_line_count} lines){RESET}",)
        }
    };
    match original {
        Some(prev) if kind != "create" => match format_full_replace_diff_preview(prev, new_content)
        {
            Some(preview) => ToolCardContent::new(header, preview),
            None => ToolCardContent::header_only(header),
        },
        _ => ToolCardContent::header_only(header),
    }
}

/// Build a small context-windowed diff preview for a write_file that
/// fully replaces an existing file. Walks the line lists from both ends
/// to skip identical head/tail, then prints up to
/// `DIFF_PREVIEW_MAX_BODY_LINES` of removed and added lines with a hunk
/// header. Returns `None` when the contents are byte-identical.
pub(crate) fn format_full_replace_diff_preview(original: &str, updated: &str) -> Option<String> {
    if original == updated {
        return None;
    }
    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = updated.lines().collect();

    let mut head = 0;
    while head < old_lines.len() && head < new_lines.len() && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < old_lines.len() - head
        && tail < new_lines.len() - head
        && old_lines[old_lines.len() - 1 - tail] == new_lines[new_lines.len() - 1 - tail]
    {
        tail += 1;
    }

    let old_changed = &old_lines[head..old_lines.len() - tail];
    let new_changed = &new_lines[head..new_lines.len() - tail];
    let edit_start_line = head + 1;
    Some(render_diff_window(
        old_changed,
        new_changed,
        &old_lines,
        head,
        edit_start_line,
    ))
}

pub(crate) fn edit_card(input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    // path / oldString / newString / replaceAll are what the model sent → input.
    // originalFile (the pre-edit file, for the diff) is only in the result → output.
    let path = {
        let p = extract_tool_path(input);
        if p == "?" {
            extract_tool_path(output)
        } else {
            p
        }
    };
    let replace_all = input
        .get("replaceAll")
        .or_else(|| output.get("replaceAll"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let original = output
        .get("originalFile")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let old_value = field_any(input, output, &["oldString", "old_string"]);
    let new_value = field_any(input, output, &["newString", "new_string"]);

    let occurrences = if replace_all && !old_value.is_empty() {
        count_non_overlapping(original, old_value)
    } else {
        usize::from(!old_value.is_empty() && original.contains(old_value))
    };
    let suffix = if replace_all {
        if occurrences > 1 {
            format!(" (replace all, {occurrences} occurrences)")
        } else {
            " (replace all)".to_string()
        }
    } else {
        String::new()
    };

    let preview = format_edit_diff_preview(original, old_value, new_value)
        .or_else(|| format_patch_preview(old_value, new_value));

    let warning = ansi_bold_fg(theme().warning);
    let header = format!("{warning}📝 Edited {path}{suffix}{RESET}");
    match preview {
        Some(preview) => ToolCardContent::new(header, preview),
        None => ToolCardContent::header_only(header),
    }
}

/// Render a context-windowed diff for the first occurrence of `old_string`
/// inside `original`. Returns `None` when we cannot locate the match (e.g.
/// `original` was not captured, or the JSON was malformed).
///
/// `old_string` is treated as a substring of `original`, not necessarily
/// line-aligned. The "affected block" is widened out to whole-line
/// boundaries on both ends so the rendered `-`/`+` lines reflect the
/// actual lines that changed — not just the literal `old_string` /
/// `new_string` fragments (which would mislead the user when an edit
/// happens mid-line).
pub(crate) fn format_edit_diff_preview(
    original: &str,
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    if original.is_empty() || old_string.is_empty() {
        return None;
    }
    let match_start = original.find(old_string)?;
    let match_end = match_start + old_string.len();

    // Widen to whole-line boundaries.
    let line_start = original[..match_start].rfind('\n').map_or(0, |idx| idx + 1);
    let line_end = original[match_end..]
        .find('\n')
        .map_or(original.len(), |idx| match_end + idx);

    let affected_old = &original[line_start..line_end];
    let new_region = format!(
        "{}{}{}",
        &original[line_start..match_start],
        new_string,
        &original[match_end..line_end],
    );

    let old_lines: Vec<&str> = affected_old.lines().collect();
    let new_lines: Vec<&str> = new_region.lines().collect();
    let edit_start_line = original[..line_start].matches('\n').count() + 1;
    let pre_context_start = edit_start_line.saturating_sub(1 + DIFF_PREVIEW_CONTEXT_LINES);
    let original_lines: Vec<&str> = original.lines().collect();

    Some(render_diff_window(
        &old_lines,
        &new_lines,
        &original_lines,
        pre_context_start,
        edit_start_line,
    ))
}

/// Render a single diff hunk: pre-context, `-` body, `+` body, post-context.
/// Body lines beyond `DIFF_PREVIEW_MAX_BODY_LINES` per side are collapsed
/// with a "…" summary line.
fn render_diff_window(
    old_body: &[&str],
    new_body: &[&str],
    original_lines: &[&str],
    pre_context_start: usize,
    edit_start_line_1based: usize,
) -> String {
    let mut out: Vec<String> = Vec::new();
    let pre_context = &original_lines
        [pre_context_start..pre_context_start + (edit_start_line_1based - 1 - pre_context_start)];
    let post_context_start = pre_context_start + pre_context.len() + old_body.len();
    let post_context_end = post_context_start
        .saturating_add(DIFF_PREVIEW_CONTEXT_LINES)
        .min(original_lines.len());
    let post_context = if post_context_start <= original_lines.len() {
        &original_lines[post_context_start..post_context_end]
    } else {
        &[][..]
    };

    let t = theme();
    let muted = ansi_fg(t.muted);
    let removed = ansi_fg(t.diff_removed);
    let added = ansi_fg(t.diff_added);
    out.push(format!(
        "{muted}@@ -{},{} +{},{} @@{RESET}",
        edit_start_line_1based,
        old_body.len(),
        edit_start_line_1based,
        new_body.len(),
    ));
    for line in pre_context {
        out.push(format!("{DIM}  {line}{RESET}"));
    }
    push_body_lines(&mut out, old_body, '-', &removed);
    push_body_lines(&mut out, new_body, '+', &added);
    for line in post_context {
        out.push(format!("{DIM}  {line}{RESET}"));
    }
    out.join("\n")
}

fn push_body_lines(out: &mut Vec<String>, body: &[&str], sign: char, color: &str) {
    let limit = DIFF_PREVIEW_MAX_BODY_LINES;
    if body.len() <= limit {
        for line in body {
            out.push(format!("{color}{sign} {line}{RESET}"));
        }
    } else {
        let head = limit.saturating_sub(1);
        for line in &body[..head] {
            out.push(format!("{color}{sign} {line}{RESET}"));
        }
        out.push(format!(
            "{DIM}{sign} … +{} more lines{RESET}",
            body.len() - head,
        ));
    }
}

fn count_non_overlapping(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        count += 1;
        start += idx + needle.len();
    }
    count
}

pub(crate) fn glob_card(_input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    let num_files = output
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    ToolCardContent::header_only(format!("{DIM}Found {num_files} files{RESET}"))
}

pub(crate) fn grep_card(_input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    let num_matches = output
        .get("numMatches")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let num_files = output
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    ToolCardContent::header_only(format!(
        "{DIM}{num_matches} matches across {num_files} files{RESET}"
    ))
}

pub(crate) fn generic_tool_card(
    name: &str,
    _input: &serde_json::Value,
    output: &serde_json::Value,
) -> ToolCardContent {
    let rendered_output = match output {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(map) => digest_json_object(map),
        serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(output).unwrap_or_else(|_| output.to_string())
        }
        _ => output.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );

    let muted = ansi_fg(theme().muted);
    if preview.is_empty() {
        ToolCardContent::header_only(format!("{muted}{name}{RESET}"))
    } else {
        ToolCardContent::new(format!("{muted}{name}{RESET}"), preview)
    }
}

/// One line per top-level key: `key: value`. A multi-line string shows its
/// first line and a `(+N lines)` count; nested objects and arrays are
/// compact JSON. Pretty-printing the whole object put a skill's entire
/// SKILL.md on screen as one JSON-escaped line; the transcript keeps the
/// full value, the screen only needs to say what came back.
fn digest_json_object(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::with_capacity(map.len());
    for (key, value) in map {
        let rendered = match value {
            serde_json::Value::Null => continue,
            serde_json::Value::String(text) => {
                let mut it = text.lines();
                let first = it.next().unwrap_or_default();
                let rest = it.count();
                if rest > 0 {
                    format!("{first} (+{rest} lines)")
                } else {
                    first.to_string()
                }
            }
            other => other.to_string(),
        };
        lines.push(format!("{key}: {rendered}"));
    }
    lines.join("\n")
}

fn skill_card(input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    let muted = ansi_fg(theme().muted);
    let path = {
        let p = field(input, output, "path");
        if p.is_empty() {
            "?"
        } else {
            p
        }
    };
    let prompt = output.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let lines = prompt.lines().count();
    ToolCardContent::header_only(format!(
        "{muted}Skill{RESET} loaded {path} {DIM}({lines} lines){RESET}"
    ))
}

fn read_tool_output_card(input: &serde_json::Value, output: &serde_json::Value) -> ToolCardContent {
    let muted = ansi_fg(theme().muted);
    let total = output.get("totalBytes").and_then(serde_json::Value::as_u64);
    // Seek mode reports matches; window mode reports a byte range + content.
    if let Some(matches) = output.get("matches").and_then(|v| v.as_array()) {
        let total_matches = output
            .get("totalMatches")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(matches.len() as u64);
        let pattern = {
            let p = field(input, output, "pattern");
            if p.is_empty() {
                "?"
            } else {
                p
            }
        };
        let header =
            format!("{muted}read_tool_output{RESET} {total_matches} match(es) for {pattern}");
        let mut body = String::new();
        for hit in matches.iter().take(TOOL_OUTPUT_DISPLAY_MAX_LINES) {
            let line = hit
                .get("line")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let text = hit.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&format!(
                "{DIM}{line}:{RESET} {}",
                truncate_for_summary(text, 120)
            ));
        }
        return if body.is_empty() {
            ToolCardContent::header_only(header)
        } else {
            ToolCardContent::new(header, body)
        };
    }
    let start = output
        .get("byteOffset")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let end = output
        .get("byteEnd")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let content = output.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let preview = truncate_output_for_display(
        content,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );
    let header = match total {
        Some(total) => {
            format!("{muted}read_tool_output{RESET} bytes {start}–{end} of {total}")
        }
        None => format!("{muted}read_tool_output{RESET} bytes {start}–{end}"),
    };
    if preview.is_empty() {
        ToolCardContent::header_only(header)
    } else {
        ToolCardContent::new(header, preview)
    }
}

pub(crate) fn summarize_tool_payload(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.trim().to_string(),
    };
    truncate_for_summary(&compact, 96)
}

pub(crate) fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Format the `ctx <used>/<window> (<pct>%)` status segment, or `None` when
/// the window is unknown/zero.
///
/// `context_tokens` is the CURRENT context-window occupancy — the size of the
/// prompt the provider actually processed on the latest response (uncached
/// input + cache reads + cache writes), i.e. `TokenUsage::context_tokens()`.
/// It is NOT the session-cumulative token total: cumulative grows every turn
/// and never shrinks, so it overshoots the window (and rockets past 100%)
/// while telling the user nothing about how full the context actually is.
/// This is the same occupancy metric auto-compaction compares against the
/// window, so the indicator and the compaction trigger stay in agreement.
#[inline]
pub(crate) fn format_context_usage_segment(context_tokens: u32, window: u32) -> Option<String> {
    if window == 0 {
        return None;
    }
    let pct = (f64::from(context_tokens) / f64::from(window) * 100.0).min(100.0);
    let used_display = format_token_count(context_tokens);
    let win_display = format_token_count_round(window);
    Some(format!("ctx {used_display}/{win_display} ({pct:.0}%)"))
}

/// Compact token count with one decimal (`1.2k`, `3.4M`).
#[inline]
fn format_token_count(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", f64::from(n) / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", f64::from(n) / 1000.0)
    } else {
        n.to_string()
    }
}

/// Compact token count with no decimals (`1M`, `200k`) — used for the window
/// denominator, which is a round capacity figure.
#[inline]
fn format_token_count_round(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.0}M", f64::from(n) / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.0}k", f64::from(n) / 1000.0)
    } else {
        n.to_string()
    }
}

/// What one finished turn cost, for [`format_turn_status_line`].
///
/// Grouped rather than passed positionally: the line summarises a single turn,
/// and a call site with three `Option`s in a row is one transposition away from
/// reporting the window as the occupancy with nothing to catch it.
pub(crate) struct TurnStatus<'a> {
    /// The model that answered.
    pub model: &'a str,
    /// 1-based turn number within the session.
    pub turn: u32,
    /// The turn's usage — token counts and the cost estimate.
    pub usage: &'a TokenUsage,
    /// Current context-window occupancy (`TokenUsage::context_tokens` of the
    /// latest turn), NOT the session-cumulative total. See
    /// [`format_context_usage_segment`].
    pub context_tokens: Option<u32>,
    /// The model's context window, the denominator for the occupancy segment.
    pub context_window: Option<u32>,
    /// Wall-clock time the turn took.
    pub elapsed: Duration,
    /// Current git branch, when the workspace is a repository.
    pub branch: Option<&'a str>,
    /// The proxy account the turn was billed to, when one resolved. Shown
    /// because a session spends someone's money and the name is otherwise
    /// invisible until the bill arrives; `/status` carries which rule chose it.
    pub account: Option<&'a str>,
}

/// Render the KV-cache efficiency segment for the turn status line, or `None`
/// when the turn had no prompt tokens to speak of (e.g. a trivial/empty turn).
///
/// The prompt side of a turn splits into three disjoint buckets whose sum is
/// the total prompt sent this turn:
/// - `cache_read_input_tokens` — served from the KV cache (a hit; cheap).
/// - `cache_creation_input_tokens` — written into the cache this turn (a miss
///   being cached; billed at a premium — a spike here means the prefix changed
///   and a large span was re-cached, i.e. a cache "break").
/// - `input_tokens` — fresh, uncached prompt tokens.
///
/// Both percentages are taken over the prompt total (output tokens are a
/// generation-side concern, unrelated to cache reuse, so they are excluded).
/// Showing hit% and write% together lets a reader reconstruct all three buckets
/// (fresh% = 100 - hit% - write%) from two compact numbers:
/// - `⚡NN%` — hit rate (higher is better), colored by tier so a broken-cache
///   turn stands out: green ≥80, warning 40–79, red <40.
/// - `✎NN%` — write rate (lower is steadier); turns red on a spike (≥25%).
///
/// The values are fixed for the whole turn (the provider reports cache counts
/// at message_start; only output tokens grow while streaming), so this renders
/// once at turn end and never needs in-turn refresh.
fn format_cache_efficiency_segment(usage: &TokenUsage) -> Option<String> {
    let read = u64::from(usage.cache_read_input_tokens);
    let creation = u64::from(usage.cache_creation_input_tokens);
    let fresh = u64::from(usage.input_tokens);
    let prompt_total = read + creation + fresh;
    // Below this the percentages are noise (e.g. a turn with almost no prompt).
    if prompt_total < 1000 {
        return None;
    }

    // Round to nearest percent.
    let hit_pct = (read * 100 + prompt_total / 2) / prompt_total;
    let write_pct = (creation * 100 + prompt_total / 2) / prompt_total;

    let theme = crate::render::theme();
    let hit_color = if hit_pct >= 80 {
        theme.success
    } else if hit_pct >= 40 {
        theme.warning
    } else {
        theme.error
    };
    let hit = format!(
        "{}\u{26a1}{hit_pct}%{}{DIM}",
        crate::render::ansi_fg(hit_color),
        RESET,
    );

    // Write rate: dim normally, red on a spike (a cache break).
    let write = if write_pct >= 25 {
        format!(
            "{}\u{270e}{write_pct}%{}{DIM}",
            crate::render::ansi_fg(theme.error),
            RESET,
        )
    } else {
        format!("\u{270e}{write_pct}%")
    };

    Some(format!("{hit} {write}"))
}

/// Render the dim per-turn status line shown after each interactive turn.
///
/// Contains, in order: model name, billing account, turn number, cumulative
/// token count, estimated cost (when pricing for the model is known), elapsed
/// wall-clock time for the turn, context-window occupancy, and the current git
/// branch (when one is available). All fields are dimmed; turn and tokens are
/// kept compact (`turn 3`, `3.2k tokens`) so the line stays single-row even at
/// narrow widths.
pub(crate) fn format_turn_status_line(status: &TurnStatus<'_>) -> String {
    let &TurnStatus {
        model,
        turn,
        usage,
        context_tokens,
        context_window,
        elapsed,
        branch,
        account,
    } = status;
    let total = usage.total_tokens();
    let tokens_display = if total >= 1000 {
        format!("{:.1}k", f64::from(total) / 1000.0)
    } else {
        total.to_string()
    };
    // Cost: prefer the real amount the billing backend charged; fall back to a
    // per-model local estimate (marked with a leading `~`) when it did not
    // report one. The estimate uses the actual model's pricing, not a fixed
    // default tier.
    let cost_display = if let Some(real) = usage.real_cost_usd() {
        (real > 0.0).then(|| format!("${real:.2}"))
    } else {
        let pricing = runtime::pricing_for_model(model)
            .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier);
        let est = usage
            .estimate_cost_usd_with_pricing(pricing)
            .total_cost_usd();
        (est > 0.0).then(|| format!("~${est:.2}"))
    };
    let secs = elapsed.as_secs_f64();

    let mut segments: Vec<String> = Vec::with_capacity(8);
    segments.push(format!("[{model}]"));
    // Next to the model: together they answer "who served this turn, and on
    // whose account".
    if let Some(account) = account.filter(|a| !a.is_empty()) {
        segments.push(format!("acct {account}"));
    }
    segments.push(format!("turn {turn}"));
    segments.push(format!("{tokens_display} tokens"));
    if let Some(cost) = cost_display {
        segments.push(cost);
    }
    segments.push(format!("{secs:.1}s"));
    // Context-window usage: current occupancy / model window. Uses the same
    // occupancy metric as auto-compaction (see format_context_usage_segment).
    if let (Some(used), Some(window)) = (context_tokens, context_window) {
        if let Some(segment) = format_context_usage_segment(used, window) {
            segments.push(segment);
        }
    }
    // KV-cache efficiency: hit rate + write rate over the prompt total.
    if let Some(segment) = format_cache_efficiency_segment(usage) {
        segments.push(segment);
    }
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        segments.push(branch.to_string());
    }
    format!("{DIM}{}{RESET}", segments.join(" · "))
}

/// Render the box that frames an interactive permission-approval prompt.
///
/// Output shape (newlines preserved verbatim):
/// ```text
///   ╭─ ⚠ Permission required ─╮
///   │ Tool      bash
///   │ Action    command "cargo test"
///   │ Mode      workspace-write → danger-full-access
///   │ Reason    requires unrestricted access
///   ╰──────────────────────────╯
/// ```
///
/// `Action` is derived from [`describe_tool_progress`] so it stays consistent
/// with the spinner phase label the user already sees. `Reason` is shown only
/// when the runtime supplied one.
pub(crate) fn format_permission_prompt_box(
    tool_name: &str,
    input: &str,
    current_mode: &str,
    required_mode: &str,
    reason: Option<&str>,
) -> String {
    let action = describe_tool_progress(tool_name, input);
    let mode_transition = format!("{current_mode} → {required_mode}");
    let title = "⚠ Permission required";
    // Header width: " ─ {title} ─ " inside the corners. Compute the floor of
    // the body box from the widest visible row.
    let visible_widths: Vec<usize> = [
        format!("Tool      {tool_name}"),
        format!("Action    {action}"),
        format!("Mode      {mode_transition}"),
    ]
    .into_iter()
    .chain(reason.map(|r| format!("Reason    {r}")))
    .map(|line| line.chars().count())
    .collect();
    let inner_width = visible_widths
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 4);
    let border = "─".repeat(inner_width + 2);

    let t = theme();
    let grey = ansi_fg(t.muted);
    let reset = RESET;
    let bold_yellow = ansi_bold_fg(t.warning);
    let bold_cyan = ansi_bold_fg(t.info);
    let dim = DIM;

    let mut out = String::new();
    let title_dashes = "─".repeat(inner_width.saturating_sub(title.chars().count() + 2));
    let _ = writeln!(
        out,
        "  {grey}╭─ {bold_yellow}{title}{reset}{grey} {title_dashes}─╮{reset}"
    );
    let _ = writeln!(
        out,
        "  {grey}│{reset} Tool      {bold_cyan}{tool_name}{reset}"
    );
    let _ = writeln!(out, "  {grey}│{reset} Action    {dim}{action}{reset}");
    let _ = writeln!(
        out,
        "  {grey}│{reset} Mode      {dim}{mode_transition}{reset}"
    );
    if let Some(reason) = reason {
        let _ = writeln!(out, "  {grey}│{reset} Reason    {dim}{reason}{reset}");
    }
    let _ = write!(out, "  {grey}╰{border}╯{reset}");
    out
}

/// Compact one-line summary of all tool calls that ran in a turn.
///
/// Returns `None` when no tool calls happened (silent for plain
/// text-only turns). Each entry shows the tool name followed by a status
/// glyph (`✓` for success, `✗` for error). The line ends with the total
/// count and turn duration so users can read it as `"3 tools, 1.2s"`.
///
/// Example output:
/// ```text
/// 🔧 bash ✓  read_file ✓  edit_file ✗ (3 tools, 4.7s)
/// ```
pub(crate) fn format_tool_timeline(
    tool_results: &[runtime::ConversationMessage],
    elapsed: Duration,
) -> Option<String> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    for message in tool_results {
        for block in &message.blocks {
            if let runtime::ContentBlock::ToolResult {
                tool_name,
                is_error,
                ..
            } = block
            {
                entries.push((tool_name.clone(), !*is_error));
            }
        }
    }
    if entries.is_empty() {
        return None;
    }
    let count = entries.len();
    let t = theme();
    let success_fg = ansi_fg(t.success);
    let error_fg = ansi_fg(t.error);
    let parts: Vec<String> = entries
        .into_iter()
        .map(|(name, ok)| {
            // Bold tool name; green check or red cross.
            let glyph = if ok {
                format!("{success_fg}✓{RESET}")
            } else {
                format!("{error_fg}✗{RESET}")
            };
            format!("{BOLD}{name}{RESET} {glyph}")
        })
        .collect();
    let body = parts.join("  ");
    let plural = if count == 1 { "tool" } else { "tools" };
    let secs = elapsed.as_secs_f64();
    Some(format!(
        "🔧 {body} {DIM}({count} {plural}, {secs:.1}s){RESET}"
    ))
}

pub(crate) fn truncate_output_for_display(
    content: &str,
    max_lines: usize,
    max_chars: usize,
) -> String {
    let original = content.trim_end_matches('\n');
    if original.is_empty() {
        return String::new();
    }

    let total_lines = original.lines().count();
    let mut preview_lines = Vec::new();
    let mut used_chars = 0usize;
    let mut truncated = false;

    for (index, line) in original.lines().enumerate() {
        if index >= max_lines {
            truncated = true;
            break;
        }

        let newline_cost = usize::from(!preview_lines.is_empty());
        let available = max_chars.saturating_sub(used_chars + newline_cost);
        if available == 0 {
            truncated = true;
            break;
        }

        let line_chars = line.chars().count();
        if line_chars > available {
            preview_lines.push(line.chars().take(available).collect::<String>());
            truncated = true;
            break;
        }

        if line_chars > TOOL_OUTPUT_DISPLAY_MAX_LINE_CHARS {
            let mut cut = line
                .chars()
                .take(TOOL_OUTPUT_DISPLAY_MAX_LINE_CHARS)
                .collect::<String>();
            cut.push('…');
            used_chars += newline_cost + TOOL_OUTPUT_DISPLAY_MAX_LINE_CHARS + 1;
            preview_lines.push(cut);
            continue;
        }

        preview_lines.push(line.to_string());
        used_chars += newline_cost + line_chars;
    }

    let mut preview = preview_lines.join("\n");
    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        // Prefer a counted notice when we know how many lines were dropped;
        // fall back to the static notice when the cap was character-based
        // rather than line-based (mid-line truncation).
        let shown_lines = preview_lines.len();
        if total_lines > shown_lines {
            let remaining = total_lines - shown_lines;
            let _ = write!(
                preview,
                "{DIM}… +{remaining} more {line_or_lines} · full output preserved in session{RESET}",
                line_or_lines = if remaining == 1 { "line" } else { "lines" },
            );
        } else {
            preview.push_str(DISPLAY_TRUNCATION_NOTICE);
        }
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_configured_limit_errors_are_rendered_as_context_window_guidance() {
        let error = engine_core::ApiError::Api {
            status: "400".parse().expect("status"),
            error_type: Some("invalid_request_error".to_string()),
            message: Some(
                "Input tokens exceed the configured limit of 922000 tokens. Your messages resulted in 1860900 tokens. Please reduce the length of the messages."
                    .to_string(),
            ),
            request_id: Some("req_ctx_openai_456".to_string()),
            body: String::new(),
            retryable: false,
            suggested_action: None,
            retry_after: None,
        };

        let rendered = engine_core::format_user_visible_api_error("session-issue-32", &error);
        assert!(rendered.contains("Context window blocked"), "{rendered}");
        assert!(rendered.contains("context_window_blocked"), "{rendered}");
        assert!(
            rendered.contains("Trace            req_ctx_openai_456"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "Detail           Input tokens exceed the configured limit of 922000 tokens."
            ),
            "{rendered}"
        );
        assert!(rendered.contains("Compact          /compact"), "{rendered}");
        assert!(
            rendered.contains("Fresh session    /clear --confirm"),
            "{rendered}"
        );
    }

    fn user_message_with_results(results: Vec<(&str, bool)>) -> runtime::ConversationMessage {
        runtime::ConversationMessage {
            role: runtime::MessageRole::User,
            blocks: results
                .into_iter()
                .enumerate()
                .map(|(i, (name, is_error))| runtime::ContentBlock::ToolResult {
                    tool_use_id: format!("tool_{i}"),
                    tool_name: name.to_string(),
                    output: String::new(),
                    is_error,
                })
                .collect(),
            usage: None,
            model: None,
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn render_tool_card_body_line_never_exceeds_terminal_width() {
        // Regression: a body line longer than the terminal used to be wrapped
        // by the terminal itself, and the continuation had no `│` prefix, so it
        // spilled past the left frame (seen with pid_status/generic_tool_card's
        // long JSON values). render_tool_card now wraps every content line to
        // the frame's inner width, prefixing each segment with `│`.
        let term_width = crossterm::terminal::size().map_or(80usize, |(cols, _)| cols as usize);
        let long = "x".repeat(term_width * 3);
        let content = ToolCardContent::new("pid_status".to_string(), long.clone());
        let rendered = render_tool_card(&content, ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        // (1) No rendered row exceeds the terminal width.
        for line in plain.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= term_width,
                "line exceeds terminal width {term_width}: {line:?}"
            );
        }
        // (2) Wrapping preserves content — every `x` survives (not truncated).
        let x_count = plain.chars().filter(|&c| c == 'x').count();
        assert_eq!(x_count, long.len(), "wrapped body must keep all content");
        // (3) Every body row carries the frame bar (no bare continuation).
        for line in plain.lines().filter(|l| l.contains('x')) {
            assert!(
                line.trim_start().starts_with('\u{2502}'),
                "wrapped body row must start with the frame bar: {line:?}"
            );
        }
    }

    #[test]
    fn render_tool_card_running_color_differs_from_ok() {
        // Bug 3: running used warning/teal, too close to success green. It now
        // uses the brand amber (primary) so the three states read distinctly.
        let content = ToolCardContent::header_only("Bash".to_string());
        let running = render_tool_card(&content, ToolStatus::Running);
        let ok = render_tool_card(&content, ToolStatus::Ok);
        assert_ne!(
            running, ok,
            "running and ok cards must differ (distinct frame color)"
        );
    }

    #[test]
    fn tool_card_reads_identity_fields_from_input() {
        // Bug: bash's command and edit's path live ONLY in the call input, not
        // the result payload. The completed card must read them from input.
        let bash_in = r#"{"command":"cargo test --workspace"}"#;
        let bash_out = r#"{"stdout":"ok","stderr":""}"#;
        let rendered = strip_ansi(&format_tool_result("bash", bash_in, bash_out, false));
        assert!(
            rendered.contains("Bash(cargo test --workspace)"),
            "bash header must show the command from input: {rendered}"
        );

        let edit_in = r#"{"filePath":"src/main.rs","oldString":"a","newString":"b"}"#;
        // Result payload without filePath (the field the header needs).
        let edit_out = r#"{"originalFile":"a\n","structuredPatch":[]}"#;
        let rendered = strip_ansi(&format_tool_result("edit_file", edit_in, edit_out, false));
        assert!(
            rendered.contains("Edited src/main.rs"),
            "edit header must show the path from input, not `?`: {rendered}"
        );
    }

    #[test]
    fn tool_timeline_is_silent_when_no_tools_ran() {
        let messages = vec![user_message_with_results(vec![])];
        assert!(format_tool_timeline(&messages, Duration::from_millis(500)).is_none());
        assert!(format_tool_timeline(&[], Duration::from_millis(500)).is_none());
    }

    #[test]
    fn tool_timeline_singular_form_for_one_tool() {
        let messages = vec![user_message_with_results(vec![("bash", false)])];
        let rendered = format_tool_timeline(&messages, Duration::from_secs_f64(1.2)).unwrap();
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("bash"), "{plain}");
        assert!(plain.contains("✓"), "{plain}");
        assert!(plain.contains("(1 tool, 1.2s)"), "{plain}");
    }

    #[test]
    fn tool_timeline_lists_each_tool_with_status_glyph() {
        let messages = vec![user_message_with_results(vec![
            ("bash", false),
            ("read_file", false),
            ("edit_file", true),
        ])];
        let rendered = format_tool_timeline(&messages, Duration::from_secs_f64(4.7)).unwrap();
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("bash ✓"), "{plain}");
        assert!(plain.contains("read_file ✓"), "{plain}");
        assert!(plain.contains("edit_file ✗"), "{plain}");
        assert!(plain.contains("(3 tools, 4.7s)"), "{plain}");
    }

    #[test]
    fn tool_timeline_walks_multiple_messages() {
        let messages = vec![
            user_message_with_results(vec![("bash", false)]),
            user_message_with_results(vec![("read_file", false)]),
        ];
        let rendered = format_tool_timeline(&messages, Duration::from_millis(900)).unwrap();
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("(2 tools, 0.9s)"), "{plain}");
    }

    #[test]
    fn permission_prompt_box_renders_all_fields() {
        let rendered = format_permission_prompt_box(
            "bash",
            "{\"command\":\"cargo test\"}",
            "workspace-write",
            "danger-full-access",
            Some("requires unrestricted access"),
        );
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("Permission required"), "{plain}");
        assert!(plain.contains("Tool      bash"), "{plain}");
        // Action is derived via describe_tool_progress — for bash it shows the
        // command. We just assert the prefix matches the schema.
        assert!(plain.contains("Action    command"), "{plain}");
        assert!(
            plain.contains("Mode      workspace-write → danger-full-access"),
            "{plain}"
        );
        assert!(
            plain.contains("Reason    requires unrestricted access"),
            "{plain}"
        );
        assert!(plain.starts_with("  ╭─"), "{plain}");
        assert!(plain.trim_end().ends_with('╯'), "{plain}");
    }

    #[test]
    fn turn_status_line_includes_cost_when_nonzero() {
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            ..TokenUsage::default()
        };
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-6",
            turn: 3,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_secs_f64(1.2),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("[claude-opus-4-6]"), "{plain}");
        assert!(plain.contains("turn 3"), "{plain}");
        assert!(plain.contains("1.5k tokens"), "{plain}");
        assert!(plain.contains("$"), "expected cost segment in {plain}");
        assert!(plain.contains("1.2s"), "{plain}");
    }

    #[test]
    fn turn_status_line_omits_cost_when_zero() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-6",
            turn: 1,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_secs_f64(0.3),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(!plain.contains("$"), "{plain}");
    }

    #[test]
    fn turn_status_line_prefers_real_cost_over_estimate() {
        // 385_000 sudo_point / 500_000 units-per-USD = $0.77, shown as-is (no ~).
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cost_units: Some(385_000),
            cost_currency: Some(runtime::UsageCostCurrency::SudoPoint),
            ..TokenUsage::default()
        };
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-8",
            turn: 3,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_secs_f64(1.2),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("$0.77"), "{plain}");
        assert!(
            !plain.contains("~$"),
            "real cost must not be marked estimated: {plain}"
        );
    }

    #[test]
    fn turn_status_line_marks_estimate_and_uses_model_pricing() {
        // No cost_units => estimate. Opus pricing (input $15/M, output $75/M):
        // 1M input + 1M output = $15 + $75 = $90.00, marked with a leading ~.
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-8",
            turn: 3,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_secs_f64(1.2),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(
            plain.contains("~$90.00"),
            "estimate should be model-priced and marked: {plain}"
        );
    }

    #[test]
    fn turn_status_line_appends_branch_when_present() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-6",
            turn: 1,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_millis(800),
            branch: Some("feat/tui-backlog-179"),
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.ends_with("feat/tui-backlog-179"), "{plain}");
    }

    #[test]
    fn turn_status_line_omits_branch_when_empty() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-6",
            turn: 1,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_millis(800),
            branch: Some(""),
            account: None,
        });
        let plain = strip_ansi(&rendered);
        // Trailing segment should be the duration, not an empty " · ".
        assert!(plain.ends_with("0.8s"), "{plain}");
    }

    #[test]
    fn context_usage_segment_reports_occupancy_over_window() {
        // 150k occupancy in a 1M window => 15%.
        let seg = format_context_usage_segment(150_000, 1_000_000)
            .expect("segment present for non-zero window");
        assert_eq!(seg, "ctx 150.0k/1M (15%)", "{seg}");
    }

    #[test]
    fn context_usage_segment_never_exceeds_100_percent() {
        // Occupancy above the window is clamped to 100% rather than showing a
        // nonsensical >100% figure (the old cumulative-total bug rendered
        // things like 7740%).
        let seg = format_context_usage_segment(2_000_000, 1_000_000).expect("segment present");
        assert!(seg.ends_with("(100%)"), "{seg}");
    }

    #[test]
    fn context_usage_segment_absent_for_zero_window() {
        assert!(format_context_usage_segment(1234, 0).is_none());
    }

    #[test]
    fn cache_efficiency_segment_reports_hit_and_write_over_prompt() {
        // read 8000 / (8000 + 1000 + 1000) = 80% hit; creation 1000 = 10% write.
        let usage = TokenUsage {
            input_tokens: 1000,
            cache_creation_input_tokens: 1000,
            cache_read_input_tokens: 8000,
            ..TokenUsage::default()
        };
        let seg = strip_ansi(&format_cache_efficiency_segment(&usage).expect("segment present"));
        assert_eq!(seg, "\u{26a1}80% \u{270e}10%", "{seg}");
    }

    #[test]
    fn cache_efficiency_segment_excludes_output_tokens() {
        // Output tokens must not dilute the denominator: read 9000 of a 10k
        // prompt is 90% regardless of how much was generated.
        let usage = TokenUsage {
            input_tokens: 1000,
            cache_read_input_tokens: 9000,
            output_tokens: 50_000,
            ..TokenUsage::default()
        };
        let seg = strip_ansi(&format_cache_efficiency_segment(&usage).expect("segment present"));
        assert_eq!(seg, "\u{26a1}90% \u{270e}0%", "{seg}");
    }

    #[test]
    fn cache_efficiency_segment_absent_for_trivial_prompt() {
        // A turn with almost no prompt would make the percentages noise.
        let usage = TokenUsage {
            input_tokens: 42,
            ..TokenUsage::default()
        };
        assert!(format_cache_efficiency_segment(&usage).is_none());
    }

    #[test]
    fn turn_status_line_renders_cache_segment() {
        let usage = TokenUsage {
            input_tokens: 2000,
            cache_read_input_tokens: 8000,
            ..TokenUsage::default()
        };
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-sonnet-4-6",
            turn: 3,
            usage: &usage,
            context_tokens: Some(10_000),
            context_window: Some(1_000_000),
            elapsed: Duration::from_secs_f64(0.5),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        // 8000 / 10000 = 80% hit, 0% write.
        assert!(plain.contains("\u{26a1}80% \u{270e}0%"), "{plain}");
    }

    #[test]
    fn turn_status_line_renders_context_segment() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-sonnet-4-6",
            turn: 2,
            usage: &usage,
            context_tokens: Some(150_000),
            context_window: Some(1_000_000),
            elapsed: Duration::from_secs_f64(0.5),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("ctx 150.0k/1M (15%)"), "{plain}");
    }

    #[test]
    fn turn_status_line_names_the_billing_account() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-sonnet-4-6",
            turn: 1,
            usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Duration::from_millis(500),
            branch: None,
            account: Some("fujitoken"),
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("acct fujitoken"), "{plain}");
    }

    #[test]
    fn truncate_output_emits_counted_line_notice() {
        let input = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = truncate_output_for_display(&input, 15, 4_000);
        let plain = strip_ansi(&preview);
        // First 15 shown.
        assert!(plain.contains("line 1\n"), "{plain}");
        assert!(plain.contains("line 15"), "{plain}");
        assert!(!plain.contains("line 16"), "{plain}");
        // Counted notice.
        assert!(
            plain.contains("+5 more lines · full output preserved in session"),
            "{plain}"
        );
    }

    #[test]
    fn truncate_output_singular_line_form() {
        let input = (1..=16)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = truncate_output_for_display(&input, 15, 4_000);
        let plain = strip_ansi(&preview);
        assert!(plain.contains("+1 more line"), "{plain}");
    }

    #[test]
    fn truncate_output_falls_back_to_static_notice_on_char_truncation() {
        // One single very long line — exceeds char cap before line cap.
        let input = "x".repeat(10_000);
        let preview = truncate_output_for_display(&input, 15, 200);
        let plain = strip_ansi(&preview);
        // Static notice retained for mid-line truncation; the counted
        // notice would lie because total_lines == shown_lines == 1.
        assert!(plain.contains("output truncated for display"), "{plain}");
    }

    #[test]
    fn language_token_for_common_paths() {
        assert_eq!(language_token_from_path("src/main.rs"), "rs");
        assert_eq!(language_token_from_path("foo/bar.py"), "py");
        assert_eq!(language_token_from_path("README"), "");
        assert_eq!(language_token_from_path(".gitignore"), "");
    }

    #[test]
    fn format_read_result_includes_highlighted_content() {
        // Wire format matches runtime's TextFilePayload, which uses
        // `#[serde(rename = "filePath")]` etc. — i.e. camelCase.
        let json = serde_json::json!({
            "kind": "text",
            "file": {
                "filePath": "src/main.rs",
                "content": "fn main() {\n    println!(\"hi\");\n}\n",
                "numLines": 3,
                "startLine": 1,
                "totalLines": 3
            }
        });
        let rendered =
            render_tool_card(&read_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        // Header still present with line count.
        assert!(plain.contains("Read src/main.rs"), "{plain}");
        assert!(plain.contains("(3 lines)"), "{plain}");
        // Content shows up indented under the header.
        assert!(plain.contains("fn main()"), "{plain}");
        assert!(plain.contains("println!"), "{plain}");
    }

    #[test]
    fn format_read_result_reads_camel_case_total_lines() {
        // Regression: real scode wire format uses `totalLines` (camelCase).
        // Code previously looked up only `total_lines`, so the `(N lines)`
        // count silently never appeared.
        let json = serde_json::json!({
            "file": {
                "filePath": "src/main.rs",
                "content": "fn main() {}\n",
                "totalLines": 137
            }
        });
        let rendered =
            render_tool_card(&read_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("(137 lines)"), "{plain}");
    }

    #[test]
    fn format_read_result_snake_case_total_lines_still_works() {
        // Defensive fallback for tools that emit snake_case (e.g. external
        // MCP servers that don't follow the camelCase convention).
        let json = serde_json::json!({
            "file": {
                "filePath": "x.txt",
                "content": "x\n",
                "total_lines": 42
            }
        });
        let rendered =
            render_tool_card(&read_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("(42 lines)"), "{plain}");
    }

    #[test]
    fn format_read_result_truncation_notice_survives_syntect_highlighting() {
        // Regression: when content exceeds READ_DISPLAY_MAX_LINES (10), the
        // body and the truncation notice both used to flow through syntect
        // together. syntect split the leading `\x1b` from the trailing `[2m`
        // and the terminal rendered `[2m… +N more lines …[0m` as literal
        // text. Compute truncation before highlighting and append the notice
        // afterwards so the escape stays intact.
        let big_content = (1..=30)
            .map(|n| format!("fn line_{n}() {{}}"))
            .collect::<Vec<_>>()
            .join("\n");
        let json = serde_json::json!({
            "kind": "text",
            "file": {
                "filePath": "src/main.rs",
                "content": big_content,
                "numLines": 30,
                "startLine": 1,
                "totalLines": 30
            }
        });
        let rendered =
            render_tool_card(&read_card(&serde_json::Value::Null, &json), ToolStatus::Ok);

        // The literal text `[2m` and `[0m` must NOT appear without their
        // leading ESC byte — that's the visible-corruption signature.
        let unescaped_text = strip_ansi(&rendered);
        assert!(
            !unescaped_text.contains("[2m"),
            "found literal `[2m` (the ESC got stripped): {unescaped_text}"
        );
        assert!(
            !unescaped_text.contains("[0m"),
            "found literal `[0m` (the ESC got stripped): {unescaped_text}"
        );

        // The intact escape sequence must be present in the raw rendered
        // string and adjacent to the notice text — syntect must not have
        // split them.
        let needle = "\u{1b}[2m… +20 more lines · full output preserved in session\u{1b}[0m";
        assert!(
            rendered.contains(needle),
            "intact dim-styled notice missing; rendered:\n{rendered}"
        );

        // Sanity: the first ten body lines are present, the eleventh is not.
        assert!(unescaped_text.contains("fn line_1()"));
        assert!(unescaped_text.contains("fn line_10()"));
        assert!(!unescaped_text.contains("fn line_11()"));
    }

    #[test]
    fn format_read_result_renders_header_only_for_empty_content() {
        let json = serde_json::json!({
            "kind": "text",
            "file": {
                "filePath": "empty.txt",
                "content": "",
                "numLines": 0,
                "startLine": 1,
                "totalLines": 0
            }
        });
        let rendered =
            render_tool_card(&read_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("Read empty.txt"), "{plain}");
        // No content body indented underneath.
        assert!(!plain.contains("\n  "), "{plain}");
    }

    #[test]
    fn truncate_output_no_truncation_returns_input_clean() {
        let input = "line 1\nline 2\nline 3";
        let preview = truncate_output_for_display(input, 15, 4_000);
        let plain = strip_ansi(&preview);
        assert_eq!(plain, input);
    }

    #[test]
    fn permission_prompt_box_omits_reason_when_none() {
        let rendered = format_permission_prompt_box(
            "read_file",
            "{\"path\":\"src/main.rs\"}",
            "read-only",
            "workspace-write",
            None,
        );
        let plain = strip_ansi(&rendered);
        assert!(!plain.contains("Reason"), "{plain}");
        assert!(plain.contains("Tool      read_file"), "{plain}");
    }

    #[test]
    fn split_hook_feedback_no_marker_returns_original() {
        let s = "{\"filePath\":\"foo\"}";
        let (head, tail) = split_hook_feedback(s);
        assert_eq!(head, s);
        assert!(tail.is_none());
    }

    #[test]
    fn split_hook_feedback_strips_normal_marker() {
        let s = "{\"filePath\":\"foo\"}\n\nHook feedback:\nlinted";
        let (head, tail) = split_hook_feedback(s);
        assert_eq!(head, "{\"filePath\":\"foo\"}");
        assert_eq!(tail, Some("Hook feedback:\nlinted"));
    }

    #[test]
    fn split_hook_feedback_strips_error_marker() {
        let s = "{\"filePath\":\"foo\"}\n\nHook feedback (error):\ndenied";
        let (head, tail) = split_hook_feedback(s);
        assert_eq!(head, "{\"filePath\":\"foo\"}");
        assert_eq!(tail, Some("Hook feedback (error):\ndenied"));
    }

    #[test]
    fn format_tool_result_recovers_edit_preview_when_hook_appends_feedback() {
        // Regression: merge_hook_feedback wraps the JSON output with
        // "\n\nHook feedback:\n...", which used to break serde_json::from_str
        // and silently disable the structured edit preview.
        let edit_json = serde_json::json!({
            "filePath": "src/main.rs",
            "oldString": "alpha",
            "newString": "omega",
            "originalFile": "alpha beta\n",
            "userModified": false,
            "replaceAll": false,
        })
        .to_string();
        let polluted = format!("{edit_json}\n\nHook feedback:\nformatter clean");
        let rendered = format_tool_result("edit_file", "", &polluted, false);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("Edited src/main.rs"), "{plain}");
        // Diff preview survives — the +/- lines come from oldString/newString
        // against originalFile, which only parse if we stripped the suffix.
        assert!(plain.contains("- alpha"), "{plain}");
        assert!(plain.contains("+ omega"), "{plain}");
        // Hook feedback should still be rendered, but as a separate line.
        assert!(plain.contains("Hook feedback:"), "{plain}");
        assert!(plain.contains("formatter clean"), "{plain}");
    }

    #[test]
    fn format_edit_diff_preview_shows_context_around_change() {
        let original = "line 1\nline 2\nline 3\nold line\nline 5\nline 6\nline 7\n";
        let preview = format_edit_diff_preview(original, "old line", "new line").unwrap();
        let plain = strip_ansi(&preview);
        // Hunk header pointing at line 4.
        assert!(plain.contains("@@ -4,1 +4,1 @@"), "{plain}");
        // Three lines of pre-context.
        assert!(plain.contains("  line 1"), "{plain}");
        assert!(plain.contains("  line 2"), "{plain}");
        assert!(plain.contains("  line 3"), "{plain}");
        // The change itself.
        assert!(plain.contains("- old line"), "{plain}");
        assert!(plain.contains("+ new line"), "{plain}");
        // Three lines of post-context.
        assert!(plain.contains("  line 5"), "{plain}");
        assert!(plain.contains("  line 6"), "{plain}");
        assert!(plain.contains("  line 7"), "{plain}");
    }

    #[test]
    fn format_edit_diff_preview_collapses_oversized_bodies() {
        let original = (1..=20)
            .map(|n| format!("old{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let old_str = original.clone();
        let new_str = (1..=20)
            .map(|n| format!("new{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = format_edit_diff_preview(&original, &old_str, &new_str).unwrap();
        let plain = strip_ansi(&preview);
        // Body collapses at DIFF_PREVIEW_MAX_BODY_LINES (8) per side.
        assert!(plain.contains("- old1"), "{plain}");
        assert!(plain.contains("- old7"), "{plain}");
        assert!(!plain.contains("- old8"), "{plain}");
        assert!(plain.contains("- … +13 more lines"), "{plain}");
        assert!(plain.contains("+ new1"), "{plain}");
        assert!(plain.contains("+ … +13 more lines"), "{plain}");
    }

    #[test]
    fn format_edit_diff_preview_returns_none_without_anchor() {
        // Empty original / old_string means we cannot anchor the window —
        // caller falls back to single-line summary.
        assert!(format_edit_diff_preview("", "x", "y").is_none());
        assert!(format_edit_diff_preview("foo", "", "y").is_none());
        assert!(format_edit_diff_preview("foo", "not present", "y").is_none());
    }

    #[test]
    fn format_edit_diff_preview_widens_substring_match_to_whole_lines() {
        // Regression caught during manual inspection: when `old_string` is
        // a mid-line substring (very common — replacing a function name
        // inside `    foo();`), the body must show the WHOLE affected line
        // with the replacement applied in place, not just the literal
        // fragment. Otherwise the rendered diff lies about which line is
        // changing and the line number is off-by-one.
        let original =
            "fn header() {}\n\nfn caller() {\n    let x = 1;\n    old_function();\n    return x;\n}\n";
        let preview =
            format_edit_diff_preview(original, "old_function()", "new_function()").unwrap();
        let plain = strip_ansi(&preview);
        // Hunk header points at line 5 (the line containing the match),
        // not line 6.
        assert!(plain.contains("@@ -5,1 +5,1 @@"), "{plain}");
        // The `-`/`+` rows show the full affected line with indentation,
        // not the bare fragment.
        assert!(plain.contains("-     old_function();"), "{plain}");
        assert!(plain.contains("+     new_function();"), "{plain}");
        // The full line must NOT also appear as a dim context row above
        // the change.
        let context_dup = "  \x1b[0m";
        let _ = context_dup; // anchor for the reader; assertion below
        assert!(
            !plain.contains("    old_function();\n-"),
            "affected line leaked into pre-context: {plain}"
        );
    }

    #[test]
    fn format_edit_result_renders_replace_all_count() {
        let json = serde_json::json!({
            "filePath": "src/main.rs",
            "oldString": "foo",
            "newString": "bar",
            "originalFile": "foo a\nfoo b\nfoo c\n",
            "replaceAll": true,
            "userModified": false,
        });
        let rendered =
            render_tool_card(&edit_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("(replace all, 3 occurrences)"), "{plain}");
    }

    #[test]
    fn format_write_result_shows_line_delta_on_update() {
        let json = serde_json::json!({
            "type": "update",
            "filePath": "src/main.rs",
            "content": "a\nb\nc\nd\n",
            "originalFile": "a\nx\nc\n",
        });
        let rendered =
            render_tool_card(&write_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("Updated src/main.rs"), "{plain}");
        // 4 new lines, was 3, delta +1.
        assert!(plain.contains("(4 lines, was 3 +1)"), "{plain}");
        // Diff body should show the changed middle line.
        assert!(plain.contains("- x"), "{plain}");
        assert!(plain.contains("+ b"), "{plain}");
    }

    #[test]
    fn format_write_result_create_skips_delta_and_diff() {
        let json = serde_json::json!({
            "type": "create",
            "filePath": "new.txt",
            "content": "hello\nworld\n",
        });
        let rendered =
            render_tool_card(&write_card(&serde_json::Value::Null, &json), ToolStatus::Ok);
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("Wrote new.txt"), "{plain}");
        assert!(plain.contains("(2 lines)"), "{plain}");
        assert!(!plain.contains("was"), "{plain}");
        assert!(!plain.contains("@@"), "{plain}");
    }

    /// Visual-inspection-only: print four representative tool results so the
    /// rendered ANSI output can be eyeballed during manual verification. Run
    /// with `cargo test -p rusty-sudocode-cli --bin scode -- \
    /// cli::format::tests::PREVIEW_render_samples_for_manual_inspection \
    /// --nocapture --include-ignored`. Marked `#[ignore]` so it never runs
    /// in the default suite.
    #[test]
    #[ignore = "visual-inspection only; run with --include-ignored --nocapture"]
    #[allow(non_snake_case)]
    fn PREVIEW_render_samples_for_manual_inspection() {
        let edit_with_context = serde_json::json!({
            "filePath": "src/main.rs",
            "oldString": "old_function()",
            "newString": "new_function()",
            "originalFile": "fn header() {}\n\nfn caller() {\n    let x = 1;\n    old_function();\n    return x;\n}\n\nfn footer() {}\n",
            "userModified": false,
            "replaceAll": false,
        })
        .to_string();
        println!("\n=== SAMPLE 1: edit_file with surrounding context ===");
        println!(
            "{}",
            format_tool_result("edit_file", "", &edit_with_context, false)
        );

        let edit_replace_all = serde_json::json!({
            "filePath": "src/lib.rs",
            "oldString": "foo",
            "newString": "bar",
            "originalFile": "foo one\nfoo two\nfoo three\n",
            "userModified": false,
            "replaceAll": true,
        })
        .to_string();
        println!("\n=== SAMPLE 2: edit_file with replaceAll + occurrence count ===");
        println!(
            "{}",
            format_tool_result("edit_file", "", &edit_replace_all, false)
        );

        let write_update = serde_json::json!({
            "type": "update",
            "filePath": "config.toml",
            "content": "[server]\nport = 8080\nhost = \"0.0.0.0\"\nmax_connections = 100\n",
            "originalFile": "[server]\nport = 3000\nhost = \"127.0.0.1\"\n",
        })
        .to_string();
        println!("\n=== SAMPLE 3: write_file update with line-count delta ===");
        println!(
            "{}",
            format_tool_result("write_file", "", &write_update, false)
        );

        let with_hook_feedback = format!(
            "{}\n\nHook feedback:\nrustfmt clean\nclippy clean",
            edit_with_context
        );
        println!("\n=== SAMPLE 4: edit_file with hook feedback suffix (regression #1) ===");
        println!(
            "{}",
            format_tool_result("edit_file", "", &with_hook_feedback, false)
        );
        println!();
    }
}
