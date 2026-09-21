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

use crate::render::{ansi_bold_fg, ansi_fg, theme, BOLD, DIM, PROMPT_PREFIX, RESET};
use crate::{
    load_sudocode_config_for_current_dir, GitWorkspaceSummary, InternalPromptProgressEvent,
    InternalPromptProgressState, BUILD_TARGET, DEFAULT_DATE, GIT_SHA, LATEST_SESSION_REFERENCE,
    PRIMARY_SESSION_EXTENSION, VERSION,
};

// ---------------------------------------------------------------------------
// Unified message rendering pipeline
// ---------------------------------------------------------------------------

/// Pairs each tool call's `input` with the `ToolResult` that follows it,
/// keyed by tool-use id. A tool's identity fields — bash's command,
/// edit/read/write's path, grep/glob's pattern — live only in the call
/// `input`; the result payload never echoes them. Both render paths (live
/// streaming in `render_engine`, and session replay in [`render_message`])
/// use this one type so the pairing rule has a single home.
#[derive(Default)]
pub(crate) struct ToolInputRegistry {
    inputs: std::collections::HashMap<String, String>,
}

impl ToolInputRegistry {
    /// Record a call's input under its tool-use id (seen on `ToolUse`).
    pub(crate) fn remember(&mut self, id: &str, input: &str) {
        self.inputs.insert(id.to_string(), input.to_string());
    }

    /// Take the input remembered for `id` (seen on the matching `ToolResult`),
    /// removing it. Returns `""` when the call is missing (e.g. a truncated
    /// session), which the card extractors tolerate by falling back to the
    /// result payload.
    pub(crate) fn take(&mut self, id: &str) -> String {
        self.inputs.remove(id).unwrap_or_default()
    }
}

/// Render a single `ConversationMessage` into styled terminal output.
///
/// This is the SSOT for "how does a completed message look on screen."
/// Both session replay (`--resume`) and any future message display
/// (e.g. `/history`, export) should call this instead of hand-rolling
/// role/block matching.
///
/// `tool_inputs` carries call inputs forward from each `ToolUse` message to
/// the `ToolResult` message that follows it (they are separate messages), so
/// completed cards can show the identity fields the result omits.
///
/// The live REPL uses a different path (streaming event callbacks) for
/// progressive rendering during a turn, but the *final* visual result
/// is the same because both paths call the same per-block format
/// functions (`format_input_echo`, `format_tool_call_start`, etc.).
pub(crate) fn render_message(
    msg: &runtime::ConversationMessage,
    term_width: usize,
    renderer: &crate::render::TerminalRenderer,
    tool_inputs: &mut ToolInputRegistry,
) -> Option<String> {
    let mut out = String::new();

    match msg.role {
        runtime::MessageRole::User => {
            let text = text_from_blocks(&msg.blocks);
            if text.is_empty() {
                return None;
            }
            // Render exactly the plain `❯ text` prompt echo — no surrounding
            // horizontal rules. The rules were added in the original resume
            // work to "match live output", but the live iocraft REPL never
            // commits a ruled box to scrollback (the input widget is a canvas
            // that redraws away); the rules only made a resumed message look
            // like the live input box below it.
            let (echo, _) = format_input_echo(&text, term_width);
            out.push_str(&echo);
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
                    runtime::ContentBlock::ToolUse { id, input, .. } => {
                        // Mirror the live render engine exactly: a ToolUse only
                        // *remembers* its input so the matching ToolResult can
                        // show the identity fields (command, path, pattern) the
                        // result payload never echoes. It renders NO card here —
                        // the single durable card per call is the completed
                        // (green/red) one drawn by the ToolResult below. (A
                        // running/amber card is a live overlay-only status and
                        // must never be committed to scrollback; on replay a
                        // resultless call is simply not shown, same as live.)
                        tool_inputs.remember(id, input);
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
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } = block
                {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    // Pair this result with the input remembered from its call
                    // (`""` if the call is missing, e.g. a truncated session) —
                    // the same pairing the live render engine does.
                    let input = tool_inputs.take(tool_use_id);
                    out.push_str(&format_tool_result(tool_name, &input, output, *is_error));
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
/// over [`render_message`] for session replay. Owns the [`ToolInputRegistry`]
/// so call inputs carry forward from each `ToolUse` message to the
/// `ToolResult` message that follows it.
pub(crate) fn render_messages(
    messages: &[runtime::ConversationMessage],
    term_width: usize,
    renderer: &crate::render::TerminalRenderer,
) -> String {
    let mut parts = Vec::new();
    let mut tool_inputs = ToolInputRegistry::default();
    for msg in messages {
        if let Some(rendered) = render_message(msg, term_width, renderer, &mut tool_inputs) {
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
    let mut rendered = String::new();
    for (idx, line) in raw_lines.iter().enumerate() {
        // Prefix the first line with the same `❯` prompt glyph the live input
        // box uses, so a message echoed here (session-history replay on resume)
        // is visually identical to how it appeared when you typed it: the plain
        // `❯ text` prompt line, no styled background. Continuation lines indent
        // by two spaces to align under the text.
        let prefix = if idx == 0 { PROMPT_PREFIX } else { "  " };
        let body = format!("{prefix}{line}");
        if idx > 0 {
            rendered.push('\n');
        }
        rendered.push_str(&body);
    }
    (rendered, raw_lines.len())
}

/// The built-in tools the card renderer recognizes, each matched from *both*
/// its snake_case wire name (`bash`, `read_file`) and its TitleCase alias
/// (`Bash`, `Read`). This is the SSOT that both the running header
/// ([`format_tool_call_start`]) and the completed card ([`format_tool_result`])
/// resolve through, so the two moments can never disagree on the tool's
/// display label — the casing drift that showed a lowercase `bash` while
/// running and a TitleCase `Bash` once done.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Bash,
    Read,
    Write,
    Edit,
    Glob,
    Grep,
    WebSearch,
    Skill,
    ReadToolOutput,
}

impl ToolKind {
    /// Parse a wire tool name (either spelling). `None` for tools without a
    /// bespoke card — they fall through to the generic renderer.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "bash" | "Bash" => Some(Self::Bash),
            "read_file" | "Read" => Some(Self::Read),
            "write_file" | "Write" => Some(Self::Write),
            "edit_file" | "Edit" => Some(Self::Edit),
            "glob_search" | "Glob" => Some(Self::Glob),
            "grep_search" | "Grep" => Some(Self::Grep),
            "web_search" | "WebSearch" => Some(Self::WebSearch),
            "Skill" => Some(Self::Skill),
            "read_tool_output" => Some(Self::ReadToolOutput),
            _ => None,
        }
    }
}

pub(crate) fn describe_tool_progress(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));
    match ToolKind::from_name(name) {
        Some(ToolKind::Bash) => {
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
        Some(ToolKind::Read) => format!("reading {}", extract_tool_path(&parsed)),
        Some(ToolKind::Write) => format!("writing {}", extract_tool_path(&parsed)),
        Some(ToolKind::Edit) => format!("editing {}", extract_tool_path(&parsed)),
        Some(ToolKind::Glob) => {
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
        Some(ToolKind::Grep) => {
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
        Some(ToolKind::WebSearch) => parsed
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

/// Staging (in-flight) render for a tool call: the SAME card the completed
/// scrollback render draws, but with no `output` yet and a yellow Running
/// frame. A thin shell over [`tool_card_content`] — the SSOT that guarantees a
/// tool looks identical running and done, differing only in frame color and
/// whether the result body is present.
///
/// **Invariant: overlay-only.** `Running` (amber) is a live status; the sole
/// caller is the iocraft staging overlay ([`crate::repl_ui`]), a self-clearing
/// region that removes the card on completion. It must NEVER be committed to
/// durable scrollback — that would freeze an amber "in-flight" card above the
/// real result forever. Scrollback carries exactly one card per call: the
/// completed (green/red) [`format_tool_result`].
pub(crate) fn format_tool_call_start(name: &str, input: &str) -> String {
    let in_val: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));
    let content = tool_card_content(name, &in_val, None, ToolStatus::Running);
    render_tool_card(&content, ToolStatus::Running)
}

/// The single source of truth for a tool card's semantic content, shared by the
/// staging render ([`format_tool_call_start`], `output = None`) and the
/// completed render ([`format_tool_result`], `output = Some`). One `match` over
/// [`ToolKind`], one extractor per tool — there is no second per-tool code path.
///
/// Every card's first line (`identity`) is mandatory and always carries the
/// tool's real arguments (command, path, pattern…), so a tool can never render
/// a bodyless summary that hides what it was called with. The optional `body`
/// carries the result (stdout, diff, matches). On error the identity still
/// shows the arguments and the error text becomes the body.
pub(crate) fn tool_card_content(
    name: &str,
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    status: ToolStatus,
) -> ToolCardContent {
    match ToolKind::from_name(name) {
        Some(ToolKind::Bash) => bash_card(input, output, status),
        Some(ToolKind::Read) => read_card(input, output, status),
        Some(ToolKind::Write) => write_card(input, output, status),
        Some(ToolKind::Edit) => edit_card(input, output, status),
        Some(ToolKind::Glob) => glob_card(input, output, status),
        Some(ToolKind::Grep) => grep_card(input, output, status),
        Some(ToolKind::Skill) => skill_card(input, output, status),
        Some(ToolKind::ReadToolOutput) => read_tool_output_card(input, output, status),
        Some(ToolKind::WebSearch) => web_search_card(input, output, status),
        None => generic_tool_card(name, input, output, status),
    }
}

/// The optional, LLM-authored one-line annotation for a tool call, rendered as
/// a dim `· <description>` suffix after the identity. Only some tools expose a
/// `description` field in their schema (e.g. `bash`); when the model fills it,
/// showing it puts the model's own words next to the real command — more
/// transparency, closer to the user. Absent → nothing is appended.
#[inline]
fn identity_annotation(input: &serde_json::Value) -> String {
    input
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| format!(" {DIM}· {d}{RESET}"))
        .unwrap_or_default()
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
    // Header may itself contain newlines (e.g. a multi-line command echoed in
    // the identity). Split on them first — exactly like the body — so every
    // physical line gets a frame prefix; otherwise an embedded `\n` produces a
    // row with no `│` that spills past the left frame.
    let mut first_row = true;
    for line in content.header.split('\n') {
        for seg in wrap_ansi_to_width(line, content_width) {
            let prefix = if first_row { &top } else { &bar };
            if !first_row {
                out.push('\n');
            }
            let _ = write!(out, "{prefix} {seg}");
            first_row = false;
        }
    }
    if let Some(body) = &content.body {
        for line in body.split('\n') {
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
        // A tab has no intrinsic display width (`UnicodeWidthChar::width`
        // returns `None`), but visually advances to the next 8-column tab stop.
        // Counting it as 0 undercounts the row, so the terminal wraps it and
        // the continuation escapes the frame. Expand it to spaces up to the
        // next tab stop instead.
        if ch == '\t' {
            let advance = 8 - (vis % 8);
            if vis + advance > width && vis > 0 {
                if carried_style {
                    cur.push_str(RESET);
                }
                rows.push(std::mem::take(&mut cur));
                vis = 0;
            }
            let advance = 8 - (vis % 8);
            for _ in 0..advance {
                cur.push(' ');
            }
            vis += advance;
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
    let (payload, hook_feedback) = split_hook_feedback(output);
    let status = if is_error {
        ToolStatus::Error
    } else {
        ToolStatus::Ok
    };
    // Every card is built from BOTH the tool's `input` (the arguments the model
    // sent — command, path, oldString…) and its `output` (the result — stdout,
    // diff, content). The identity fields live in `input`; the body lives in
    // `output`. Routing errors through the SAME per-tool extractor keeps the
    // arguments on screen when a call fails (an error card used to collapse to
    // just the tool name), with the error text supplied as the body.
    let in_val: serde_json::Value = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
    let out_val: serde_json::Value =
        serde_json::from_str(payload).unwrap_or(serde_json::Value::String(payload.to_string()));
    let mut content = tool_card_content(name, &in_val, Some(&out_val), status);
    if is_error {
        // The extractor's success body (if any) is meaningless on failure;
        // replace it with the raw error text so the identity + error read
        // cleanly.
        let summary = truncate_for_summary(output.trim(), 160);
        content.body = if summary.is_empty() {
            None
        } else {
            let removed = ansi_fg(theme().diff_removed);
            Some(format!("{removed}{summary}{RESET}"))
        };
    }
    if let (Some(feedback), false) = (hook_feedback, is_error) {
        let hf = ansi_fg(theme().hook_feedback);
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

pub(crate) fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

pub(crate) fn bash_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    use std::fmt::Write as _;

    // Command lives in `input` (never echoed in the result payload).
    let command = input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let muted = ansi_fg(theme().muted);
    let mut header = if command.is_empty() {
        format!("{muted}Bash{RESET}")
    } else {
        format!("{muted}Bash{RESET}({})", truncate_for_summary(command, 120))
    };
    header.push_str(&identity_annotation(input));

    let Some(output) = output else {
        // Staging: no result yet — identity only.
        return ToolCardContent::header_only(header);
    };

    // When the header could not show the whole command — it spans multiple
    // lines, or a single line longer than the 120-char summary cap — echo the
    // full command verbatim at the top of the body, each line `$ `-prefixed
    // (shell convention) so it reads distinctly from the output below. A hacker
    // tool favors transparency: the exact command run is never hidden.
    let command_preamble = command_body_preamble(command);

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

    stdout_stderr_card(header, command_preamble, stdout_text, stderr_text)
}

/// Build the `$ `-prefixed full-command preamble for a bash card body, or
/// `None` when the header already shows the whole command (single line ≤120
/// chars). Multi-line commands get one `$ ` per line; the preamble is closed
/// with a dim horizontal rule that separates the command from the output.
fn command_body_preamble(command: &str) -> Option<String> {
    use std::fmt::Write as _;
    let is_multiline = command.contains('\n');
    let is_overlong = command.chars().count() > 120;
    if !is_multiline && !is_overlong {
        return None;
    }
    let mut preamble = String::new();
    for line in command.split('\n') {
        let _ = writeln!(preamble, "{DIM}${RESET} {line}");
    }
    // Dim rule (structure, not status) dividing command from output.
    let rule_width = crossterm::terminal::size()
        .map_or(24, |(cols, _)| (cols as usize).saturating_sub(6).min(24));
    let _ = write!(
        preamble,
        "{DIM}{}{RESET}",
        "\u{2500}".repeat(rule_width.max(1))
    );
    Some(preamble)
}

/// Shared stdout/stderr body builder used by [`bash_card`]. Combines the two
/// streams, drops blank lines, and applies the per-tool line cap from
/// `TOOL_OUTPUT_DISPLAY_MAX_LINES`. When `command_preamble` is present it is
/// placed above the output (the verbatim `$ `-prefixed command + a dim rule).
/// Returns a [`ToolCardContent`]; the L-frame prefix is applied later by
/// [`render_tool_card`].
fn stdout_stderr_card(
    header: String,
    command_preamble: Option<String>,
    stdout: &str,
    stderr: &str,
) -> ToolCardContent {
    use std::fmt::Write as _;

    let all_output: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();

    if all_output.is_empty() {
        // No output: still show the full command if the header truncated it.
        return match command_preamble {
            Some(preamble) => ToolCardContent::new(header, preamble),
            None => ToolCardContent::header_only(header),
        };
    }

    let term_width = crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80);
    // 4 = frame + space prefix applied by render_tool_card, plus safety margin
    // to avoid wrapping.
    let max_content_width = term_width.saturating_sub(6);

    let preview_count = TOOL_OUTPUT_DISPLAY_MAX_LINES;
    let mut body = String::new();

    // Verbatim command sits above the output, separated by its own dim rule.
    if let Some(preamble) = &command_preamble {
        body.push_str(preamble);
        body.push('\n');
    }

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

pub(crate) fn read_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let path = extract_tool_path(input);
    let Some(output) = output else {
        // Staging: identity only.
        let mut header = format!("{DIM}Read {path}{RESET}");
        header.push_str(&identity_annotation(input));
        return ToolCardContent::header_only(header);
    };
    let file = output.get("file").unwrap_or(output);
    // Path is authoritative in `input`; fall back to the result envelope.
    let path = if path == "?" {
        extract_tool_path(file)
    } else {
        path
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
    header.push_str(&identity_annotation(input));
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

pub(crate) fn write_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let success = ansi_bold_fg(theme().success);
    let Some(output) = output else {
        // Staging: identity from input (path + content line count).
        let path = extract_tool_path(input);
        let lines = input
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map_or(0, |content| content.lines().count());
        let mut header = format!("{success}✏️ Write {path}{RESET} {DIM}({lines} lines){RESET}");
        header.push_str(&identity_annotation(input));
        return ToolCardContent::header_only(header);
    };
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
    let mut header = match original {
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
    header.push_str(&identity_annotation(input));
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

pub(crate) fn edit_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    status: ToolStatus,
) -> ToolCardContent {
    // path / oldString / newString / replaceAll are what the model sent → input.
    // originalFile (the pre-edit file, for the diff) is only in the result → output.
    let null = serde_json::Value::Null;
    let out = output.unwrap_or(&null);
    let path = {
        let p = extract_tool_path(input);
        if p == "?" {
            extract_tool_path(out)
        } else {
            p
        }
    };
    let replace_all = input
        .get("replaceAll")
        .or_else(|| out.get("replaceAll"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let original = out
        .get("originalFile")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let old_value = field_any(input, out, &["oldString", "old_string"]);
    let new_value = field_any(input, out, &["newString", "new_string"]);

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

    // One diff renderer for both moments. When we have the pre-edit file
    // (completed), render the proper context-windowed diff; otherwise (staging,
    // before the result arrives) render the full old→new block — never just the
    // first line, which used to make a multi-line edit look like a one-liner.
    let preview = format_edit_diff_preview(original, old_value, new_value)
        .or_else(|| format_old_new_diff(old_value, new_value));

    let warning = ansi_bold_fg(theme().warning);
    let verb = if status == ToolStatus::Running {
        "Editing"
    } else {
        "Edited"
    };
    let mut header = format!("{warning}📝 {verb} {path}{suffix}{RESET}");
    header.push_str(&identity_annotation(input));
    match preview {
        Some(preview) => ToolCardContent::new(header, preview),
        None => ToolCardContent::header_only(header),
    }
}

/// Full old→new diff for when the pre-edit file is not available (staging).
/// Every changed line is shown (capped per side by
/// `DIFF_PREVIEW_MAX_BODY_LINES`) — the SSOT diff-body renderer, not a
/// first-line-only summary. Returns `None` when both sides are empty.
fn format_old_new_diff(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    let t = theme();
    let removed = ansi_fg(t.diff_removed);
    let added = ansi_fg(t.diff_added);
    let mut out: Vec<String> = Vec::new();
    push_body_lines(
        &mut out,
        &old_value.lines().collect::<Vec<_>>(),
        '-',
        &removed,
    );
    push_body_lines(
        &mut out,
        &new_value.lines().collect::<Vec<_>>(),
        '+',
        &added,
    );
    Some(out.join("\n"))
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

/// Identity line for the search tools (`Glob`, `Grep`): `Label(pattern)` with a
/// dim ` in <scope>` when a path was given. Reads the pattern from `input` — the
/// bug this replaces silently dropped it, so a search card read `0 matches`
/// without ever saying what was searched for.
#[inline]
fn search_identity(label: &str, input: &serde_json::Value) -> String {
    let muted = ansi_fg(theme().muted);
    let pattern = input
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let mut header = format!("{muted}{label}{RESET}({pattern})");
    if let Some(scope) = input.get("path").and_then(serde_json::Value::as_str) {
        if !scope.is_empty() {
            header.push_str(&format!(" {DIM}in {scope}{RESET}"));
        }
    }
    header.push_str(&identity_annotation(input));
    header
}

pub(crate) fn glob_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let mut header = search_identity("Glob", input);
    if let Some(output) = output {
        let num_files = output
            .get("numFiles")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        header.push_str(&format!(" {DIM}→ {num_files} files{RESET}"));
    }
    ToolCardContent::header_only(header)
}

pub(crate) fn grep_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let mut header = search_identity("Grep", input);
    if let Some(output) = output {
        let num_matches = output
            .get("numMatches")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let num_files = output
            .get("numFiles")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        header.push_str(&format!(
            " {DIM}→ {num_matches} matches across {num_files} files{RESET}"
        ));
    }
    ToolCardContent::header_only(header)
}

pub(crate) fn generic_tool_card(
    name: &str,
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let muted = ansi_fg(theme().muted);
    let mut header = format!("{muted}{name}{RESET}");
    // Show any scalar arguments so an unknown tool still says what it was
    // called with, then any LLM description.
    let args = summarize_scalar_args(input);
    if !args.is_empty() {
        header.push_str(&format!("({args})"));
    }
    header.push_str(&identity_annotation(input));
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
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

    if preview.is_empty() {
        ToolCardContent::header_only(header)
    } else {
        ToolCardContent::new(header, preview)
    }
}

/// A compact `k=v` join of an input object's scalar fields (string/number/bool),
/// for tools without a bespoke card so their identity still shows arguments.
/// Skips `description` (rendered separately) and non-scalar values.
#[inline]
fn summarize_scalar_args(input: &serde_json::Value) -> String {
    let Some(map) = input.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (k, v) in map {
        if k == "description" {
            continue;
        }
        let rendered = match v {
            serde_json::Value::String(s) => truncate_for_summary(s, 60),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        parts.push(format!("{k}={rendered}"));
    }
    parts.join(", ")
}

/// Completed card for `web_search`: identity `WebSearch(query)` plus the result
/// digest as body.
fn web_search_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let muted = ansi_fg(theme().muted);
    let query = input
        .get("query")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let mut header = format!("{muted}WebSearch{RESET}({query})");
    header.push_str(&identity_annotation(input));
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    let rendered_output = match output {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(map) => digest_json_object(map),
        _ => output.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );
    if preview.is_empty() {
        ToolCardContent::header_only(header)
    } else {
        ToolCardContent::new(header, preview)
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

fn skill_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let muted = ansi_fg(theme().muted);
    let null = serde_json::Value::Null;
    let out = output.unwrap_or(&null);
    let path = {
        let p = field(input, out, "path");
        if p.is_empty() {
            "?"
        } else {
            p
        }
    };
    let mut header = format!("{muted}Skill{RESET}({path})");
    header.push_str(&identity_annotation(input));
    let Some(output) = output else {
        return ToolCardContent::header_only(header);
    };
    let prompt = output.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let lines = prompt.lines().count();
    ToolCardContent::header_only(format!("{header} {DIM}loaded ({lines} lines){RESET}"))
}

fn read_tool_output_card(
    input: &serde_json::Value,
    output: Option<&serde_json::Value>,
    _status: ToolStatus,
) -> ToolCardContent {
    let muted = ansi_fg(theme().muted);
    let Some(output) = output else {
        // Staging: identity from input (the id being paged).
        let id = field(input, &serde_json::Value::Null, "id");
        let mut header = if id.is_empty() {
            format!("{muted}read_tool_output{RESET}")
        } else {
            format!("{muted}read_tool_output{RESET}({id})")
        };
        header.push_str(&identity_annotation(input));
        return ToolCardContent::header_only(header);
    };
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
    let n = f64::from(n);
    if n >= 1_000_000_000.0 {
        format!("{:.1}B", n / 1_000_000_000.0)
    } else if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1000.0 {
        format!("{:.1}k", n / 1000.0)
    } else {
        format!("{n:.0}")
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
    /// 1-based turn number within the session (cumulative).
    pub turn: u32,
    /// The most recent turn's usage — its token counts, cost, and the KV-cache
    /// figures. Rendered with a `+` prefix (this-turn) in the status line.
    pub usage: &'a TokenUsage,
    /// Session-cumulative usage across every turn (tokens + cost). Rendered with
    /// a `Σ` prefix. On resume this is rebuilt from the persisted session so the
    /// running totals continue seamlessly.
    pub cumulative_usage: &'a TokenUsage,
    /// Current context-window occupancy (`TokenUsage::context_tokens` of the
    /// latest turn), NOT the session-cumulative total. See
    /// [`format_context_usage_segment`].
    pub context_tokens: Option<u32>,
    /// The model's context window, the denominator for the occupancy segment.
    pub context_window: Option<u32>,
    /// Wall-clock time the most recent turn took (the `+Ns` field). `None` on a
    /// resume where the last turn's duration was not persisted.
    pub elapsed: Option<Duration>,
    /// Session-cumulative wall-clock time across every turn (the `Σs` field).
    pub cumulative_duration: Option<Duration>,
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
        cumulative_usage,
        context_tokens,
        context_window,
        elapsed,
        cumulative_duration,
        branch,
        account,
    } = status;

    let mut segments: Vec<String> = Vec::with_capacity(8);
    segments.push(format!("[{model}]"));
    // Next to the model: together they answer "who served this turn, and on
    // whose account".
    if let Some(account) = account.filter(|a| !a.is_empty()) {
        segments.push(format!("acct {account}"));
    }
    segments.push(format!("turn {turn}"));

    // Cost and tokens carry BOTH the last turn (`+`) and the session total
    // (`Σ`), so the line reads the same live or resumed: `+` is "just now", `Σ`
    // is "this whole session". Cost is omitted entirely when neither side has a
    // figure (unknown-pricing model with no real cost reported).
    let turn_cost = cost_for(usage, model);
    let cum_cost = cost_for(cumulative_usage, model);
    if let Some(cost) = combine_turn_and_cumulative(turn_cost.as_deref(), cum_cost.as_deref()) {
        segments.push(cost);
    }
    let turn_tokens = format!("+{}", format_token_count(usage.total_tokens()));
    let cum_tokens = format!(
        "\u{3a3}{}",
        format_token_count(cumulative_usage.total_tokens())
    );
    segments.push(format!("{turn_tokens} {cum_tokens} tokens"));

    // Wall-clock: the last turn (`+Ns`) and the session total (`Σs`). Both are
    // omitted on a resume where no duration was persisted.
    if let Some(dur) = combine_durations(elapsed, cumulative_duration) {
        segments.push(dur);
    }

    // Context-window usage: current occupancy / model window. Uses the same
    // occupancy metric as auto-compaction (see format_context_usage_segment).
    if let (Some(used), Some(window)) = (context_tokens, context_window) {
        if let Some(segment) = format_context_usage_segment(used, window) {
            segments.push(segment);
        }
    }
    // KV-cache efficiency: hit rate + write rate over the most recent turn's
    // prompt total.
    if let Some(segment) = format_cache_efficiency_segment(usage) {
        segments.push(segment);
    }
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        segments.push(branch.to_string());
    }
    format!("{DIM}{}{RESET}", segments.join(" · "))
}

/// The cost of one `usage`, as a display string without any prefix: the real
/// billed amount (`$X`), or a per-model estimate marked `~$X`, or `None` when
/// there is nothing to bill (zero, or an unknown-pricing model with no real
/// cost). Shared by the per-turn and cumulative cost fields.
#[inline]
fn cost_for(usage: &TokenUsage, model: &str) -> Option<String> {
    if let Some(real) = usage.real_cost_usd() {
        (real > 0.0).then(|| format_usd_compact(real))
    } else {
        let pricing = runtime::pricing_for_model(model)
            .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier);
        let est = usage
            .estimate_cost_usd_with_pricing(pricing)
            .total_cost_usd();
        (est > 0.0).then(|| format!("~{}", format_usd_compact(est)))
    }
}

/// A compact USD amount for the status line: cents below `$1000`
/// (`$0.48`, `$42.10`), then `k`/`M`/`B` with two decimals
/// (`$5.05k`, `$1.23M`) so a long-running session's total does not grow
/// without bound.
#[inline]
fn format_usd_compact(usd: f64) -> String {
    if usd >= 1_000_000_000.0 {
        format!("${:.2}B", usd / 1_000_000_000.0)
    } else if usd >= 1_000_000.0 {
        format!("${:.2}M", usd / 1_000_000.0)
    } else if usd >= 1000.0 {
        format!("${:.2}k", usd / 1000.0)
    } else {
        format!("${usd:.2}")
    }
}

/// Render the paired `+turn Σcumulative` cost field. Either side may be absent
/// (e.g. a free turn inside a paid session); the field is omitted only when
/// both are.
#[inline]
fn combine_turn_and_cumulative(turn: Option<&str>, cumulative: Option<&str>) -> Option<String> {
    match (turn, cumulative) {
        (Some(t), Some(c)) => Some(format!("+{t} \u{3a3}{c}")),
        (Some(t), None) => Some(format!("+{t}")),
        (None, Some(c)) => Some(format!("\u{3a3}{c}")),
        (None, None) => None,
    }
}

/// Render the paired `+Ns Σs` wall-clock field from the last-turn and
/// cumulative durations. Omitted when neither is known (a resume with no
/// persisted duration).
#[inline]
fn combine_durations(turn: Option<Duration>, cumulative: Option<Duration>) -> Option<String> {
    let fmt = |d: Duration| {
        let secs = d.as_secs_f64();
        if secs >= 60.0 {
            format!("{:.0}m{:02.0}s", (secs / 60.0).floor(), secs % 60.0)
        } else {
            format!("{secs:.1}s")
        }
    };
    match (turn, cumulative) {
        (Some(t), Some(c)) => Some(format!("+{} \u{3a3}{}", fmt(t), fmt(c))),
        (Some(t), None) => Some(format!("+{}", fmt(t))),
        (None, Some(c)) => Some(format!("\u{3a3}{}", fmt(c))),
        (None, None) => None,
    }
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
            duration_ms: None,
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

    /// Render a completed (`Ok`) tool card and strip ANSI, in one step — the
    /// shape every card test needs. Collapses the repeated
    /// `render_tool_card(&x_card(…), ToolStatus::Ok)` + `strip_ansi` pair.
    fn ok_card_plain(content: ToolCardContent) -> String {
        strip_ansi(&render_tool_card(&content, ToolStatus::Ok))
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
    fn render_tool_card_frames_every_line_of_multiline_header_and_tabs() {
        // Regression: a header carrying its own newlines (e.g. a multi-line
        // command echoed in the identity) used to spill past the left frame,
        // because only the body was split on `\n`. And a tab was counted as 0
        // width, so a tabbed row overflowed and the terminal-wrapped remainder
        // escaped the frame. Every physical row must now start with a frame
        // glyph (top `╭`, bar `│`, or bottom `╰`).
        let header = "PowerShell(command=cd foo\ngit status)".to_string();
        let body = "col1\tcol2\tcol3".to_string();
        let content = ToolCardContent::new(header, body);
        let plain = ok_card_plain(content);
        for line in plain.lines() {
            let first = line.trim_start().chars().next().unwrap_or(' ');
            assert!(
                matches!(first, '\u{256d}' | '\u{2502}' | '\u{2570}'),
                "every card row must start with a frame glyph: {line:?}"
            );
        }
        // The second header line survived as its own framed row.
        assert!(
            plain.lines().any(|l| l.contains("git status")),
            "multi-line header second line must render: {plain:?}"
        );
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
    fn running_header_and_completed_card_agree_on_tool_label() {
        // Bug: the running card built its header from the raw wire name
        // (lowercase `bash`), while the completed card hardcoded `Bash` — so a
        // tool changed case the moment it finished. Both must resolve the same
        // canonical label via ToolKind.
        let running = strip_ansi(&format_tool_call_start("bash", r#"{"command":"echo hi"}"#));
        let done = strip_ansi(&format_tool_result(
            "bash",
            r#"{"command":"echo hi"}"#,
            r#"{"stdout":"hi","stderr":""}"#,
            false,
        ));
        assert!(
            running.contains("Bash"),
            "running header should show canonical `Bash`: {running}"
        );
        assert!(
            !running.contains("bash"),
            "running header must not show lowercase `bash`: {running}"
        );
        assert!(done.contains("Bash"), "completed card shows `Bash`: {done}");
    }

    #[test]
    fn replay_pairs_tool_result_with_its_call_input() {
        // Regression: the bash command / edit path live only in the ToolUse
        // input, which is a *separate message* from the ToolResult. Replay must
        // carry the input forward (via ToolInputRegistry) so the completed card
        // shows `Bash(<cmd>)`, not an empty `Bash()`.
        let renderer = crate::render::TerminalRenderer::new();
        let messages = vec![
            runtime::ConversationMessage {
                role: runtime::MessageRole::Assistant,
                blocks: vec![runtime::ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    input: r#"{"command":"cargo test --workspace"}"#.to_string(),
                    thought_signature: None,
                }],
                usage: None,
                model: None,
                duration_ms: None,
            },
            runtime::ConversationMessage {
                role: runtime::MessageRole::Tool,
                blocks: vec![runtime::ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    tool_name: "bash".to_string(),
                    output: r#"{"stdout":"ok","stderr":""}"#.to_string(),
                    is_error: false,
                }],
                usage: None,
                model: None,
                duration_ms: None,
            },
        ];
        let plain = strip_ansi(&render_messages(&messages, 80, &renderer));
        assert!(
            plain.contains("Bash(cargo test --workspace)"),
            "replay must show the command from the call input: {plain}"
        );
        // Structural invariant: exactly ONE card per call — the completed one.
        // The ToolUse must NOT also render a running/amber start card (that
        // used to persist an "in-flight" card above the result on resume).
        assert_eq!(
            plain.matches('\u{256d}').count(),
            1,
            "replay must render exactly one card per call (no persisted running card): {plain}"
        );
    }

    #[test]
    fn replay_lone_tool_call_without_result_renders_no_card() {
        // A ToolUse with no following ToolResult (interrupted turn, truncated
        // session) must render NOTHING on replay — not a stranded amber running
        // card. A resumed session is not "in flight"; only completed calls show.
        let renderer = crate::render::TerminalRenderer::new();
        let messages = vec![runtime::ConversationMessage {
            role: runtime::MessageRole::Assistant,
            blocks: vec![runtime::ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                input: r#"{"command":"sleep 30"}"#.to_string(),
                thought_signature: None,
            }],
            usage: None,
            model: None,
            duration_ms: None,
        }];
        let plain = strip_ansi(&render_messages(&messages, 80, &renderer));
        assert!(
            !plain.contains('\u{256d}'),
            "a resultless call must render no card on replay: {plain:?}"
        );
    }

    #[test]
    fn resumed_user_echo_uses_the_shared_prompt_prefix() {
        // DRY guard: the resumed-history echo and the live input line must
        // share the same prompt glyph. Both reference render::PROMPT_PREFIX, so
        // this asserts the echo starts with it — if someone hardcodes a
        // different glyph again (the old `›` bug), this fails.
        let (echo, _) = format_input_echo("hi", 80);
        assert!(
            echo.starts_with(crate::render::PROMPT_PREFIX),
            "echo must start with the shared prompt prefix: {echo:?}"
        );
    }

    #[test]
    fn resumed_user_message_has_no_horizontal_rules() {
        // Regression: a restored user message used to be wrapped in `─`×width
        // rules above and below, making it look like the live input box. It
        // must render as the plain `❯ text` echo — no rules.
        let renderer = crate::render::TerminalRenderer::new();
        let messages = vec![runtime::ConversationMessage {
            role: runtime::MessageRole::User,
            blocks: vec![runtime::ContentBlock::Text {
                text: "hello there".to_string(),
            }],
            usage: None,
            model: None,
            duration_ms: None,
        }];
        let plain = strip_ansi(&render_messages(&messages, 80, &renderer));
        assert!(plain.contains("hello there"), "{plain}");
        assert!(
            !plain.contains('─'),
            "resumed user message must not draw horizontal rules: {plain:?}"
        );
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_secs_f64(1.2)),
            cumulative_duration: None,
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("[claude-opus-4-6]"), "{plain}");
        assert!(plain.contains("turn 3"), "{plain}");
        assert!(plain.contains("1.5k Σ1.5k tokens"), "{plain}");
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_secs_f64(0.3)),
            cumulative_duration: None,
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_secs_f64(1.2)),
            cumulative_duration: None,
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_secs_f64(1.2)),
            cumulative_duration: None,
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_millis(800)),
            cumulative_duration: None,
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_millis(800)),
            cumulative_duration: None,
            branch: Some(""),
            account: None,
        });
        let plain = strip_ansi(&rendered);
        // Trailing segment should be the duration, not an empty " · ".
        assert!(plain.ends_with("+0.8s"), "{plain}");
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
            cumulative_usage: &usage,
            context_tokens: Some(10_000),
            context_window: Some(1_000_000),
            elapsed: Some(Duration::from_secs_f64(0.5)),
            cumulative_duration: None,
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
            cumulative_usage: &usage,
            context_tokens: Some(150_000),
            context_window: Some(1_000_000),
            elapsed: Some(Duration::from_secs_f64(0.5)),
            cumulative_duration: None,
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
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_millis(500)),
            cumulative_duration: None,
            branch: None,
            account: Some("fujitoken"),
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("acct fujitoken"), "{plain}");
    }

    #[test]
    fn compact_units_keep_long_totals_short() {
        // Cost gains k/M/B above $1000; cents kept below it.
        assert_eq!(format_usd_compact(0.48), "$0.48");
        assert_eq!(format_usd_compact(42.1), "$42.10");
        assert_eq!(format_usd_compact(5_052.43), "$5.05k");
        assert_eq!(format_usd_compact(1_230_000.0), "$1.23M");
        // Tokens gain a B tier above 1e9 (a long session's Σ crosses it).
        assert_eq!(format_token_count(908_400), "908.4k");
        assert_eq!(format_token_count(1_400_000_000), "1.4B");
    }

    #[test]
    fn turn_status_line_shows_turn_and_cumulative_with_prefixes() {
        // B2 contract: cost, tokens, and wall-clock each carry the last turn
        // (`+`) and the session total (`Σ`), so the line reads the same live or
        // resumed.
        let turn = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cost_units: Some(385_000), // $0.77
            cost_currency: Some(runtime::UsageCostCurrency::SudoPoint),
            ..TokenUsage::default()
        };
        let cumulative = TokenUsage {
            input_tokens: 8_000,
            output_tokens: 4_000,
            cost_units: Some(2_400_000), // $4.80
            cost_currency: Some(runtime::UsageCostCurrency::SudoPoint),
            ..TokenUsage::default()
        };
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-8",
            turn: 469,
            usage: &turn,
            cumulative_usage: &cumulative,
            context_tokens: None,
            context_window: None,
            elapsed: Some(Duration::from_secs_f64(1.2)),
            cumulative_duration: Some(Duration::from_secs(312)),
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("+$0.77 \u{3a3}$4.80"), "{plain}");
        assert!(plain.contains("+1.5k \u{3a3}12.0k tokens"), "{plain}");
        assert!(plain.contains("+1.2s \u{3a3}5m12s"), "{plain}");
    }

    #[test]
    fn turn_status_line_omits_duration_when_neither_side_known() {
        // A resume with no persisted duration: the wall-clock field disappears
        // entirely rather than showing a bogus 0s.
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line(&TurnStatus {
            model: "claude-opus-4-8",
            turn: 5,
            usage: &usage,
            cumulative_usage: &usage,
            context_tokens: None,
            context_window: None,
            elapsed: None,
            cumulative_duration: None,
            branch: None,
            account: None,
        });
        let plain = strip_ansi(&rendered);
        assert!(!plain.contains("+0.0s"), "{plain}");
        assert!(!plain.contains("\u{3a3}0s"), "{plain}");
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
        let plain = ok_card_plain(read_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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
        let plain = ok_card_plain(read_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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
        let plain = ok_card_plain(read_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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
        let rendered = render_tool_card(
            &read_card(&serde_json::Value::Null, Some(&json), ToolStatus::Ok),
            ToolStatus::Ok,
        );

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
        let plain = ok_card_plain(read_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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
        let plain = ok_card_plain(edit_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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
        let plain = ok_card_plain(write_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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
        let plain = ok_card_plain(write_card(
            &serde_json::Value::Null,
            Some(&json),
            ToolStatus::Ok,
        ));
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

    #[test]
    fn grep_card_shows_the_search_pattern() {
        // Regression: the completed grep card dropped its input entirely and
        // rendered only "N matches across M files" — you couldn't tell what was
        // searched. The pattern (from input) must appear.
        let input = serde_json::json!({ "pattern": "TODO", "path": "src/" });
        let output = serde_json::json!({ "numMatches": 0, "numFiles": 1 });
        let plain = ok_card_plain(grep_card(&input, Some(&output), ToolStatus::Ok));
        assert!(plain.contains("Grep(TODO)"), "{plain}");
        assert!(plain.contains("in src/"), "{plain}");
        assert!(plain.contains("0 matches across 1 files"), "{plain}");
    }

    #[test]
    fn glob_card_shows_the_search_pattern() {
        let input = serde_json::json!({ "pattern": "*.rs" });
        let output = serde_json::json!({ "numFiles": 3 });
        let plain = ok_card_plain(glob_card(&input, Some(&output), ToolStatus::Ok));
        assert!(plain.contains("Glob(*.rs)"), "{plain}");
        assert!(plain.contains("3 files"), "{plain}");
    }

    #[test]
    fn staging_edit_shows_full_multiline_diff_not_just_first_line() {
        // Regression: the in-flight edit card only rendered the first line of
        // old/new, making a multi-line edit look like a one-liner. With no
        // originalFile (staging), the full old→new block must render.
        let input = serde_json::json!({
            "file_path": "src/main.rs",
            "old_string": "line one\nline two\nline three",
            "new_string": "new one\nnew two",
        });
        let content = tool_card_content("edit_file", &input, None, ToolStatus::Running);
        let plain = strip_ansi(&render_tool_card(&content, ToolStatus::Running));
        assert!(plain.contains("Editing src/main.rs"), "{plain}");
        assert!(plain.contains("- line one"), "{plain}");
        assert!(plain.contains("- line two"), "{plain}");
        assert!(plain.contains("- line three"), "{plain}");
        assert!(plain.contains("+ new one"), "{plain}");
        assert!(plain.contains("+ new two"), "{plain}");
    }

    #[test]
    fn tool_identity_shows_llm_description_when_present() {
        // The optional `description` (bash schema supports it) renders as a
        // ` · ...` suffix — LLM-authored transparency next to the real command.
        let input = serde_json::json!({ "command": "ls -la", "description": "list files" });
        let plain = ok_card_plain(bash_card(&input, None, ToolStatus::Running));
        assert!(plain.contains("Bash(ls -la)"), "{plain}");
        assert!(plain.contains("· list files"), "{plain}");
    }

    #[test]
    fn bash_card_echoes_full_multiline_command_above_output() {
        // A multi-line command can't fit the single-line header, so the body
        // shows it verbatim, one `$ ` per line, above the output — hacker-tool
        // transparency: the exact command run is never hidden.
        let input = serde_json::json!({ "command": "cd foo\ngit status" });
        let output = serde_json::json!({ "stdout": "On branch main", "stderr": "" });
        let plain = ok_card_plain(bash_card(&input, Some(&output), ToolStatus::Ok));
        assert!(plain.contains("$ cd foo"), "{plain}");
        assert!(plain.contains("$ git status"), "{plain}");
        assert!(plain.contains("On branch main"), "{plain}");
        // A dim rule (U+2500) separates command from output.
        assert!(
            plain.contains('\u{2500}'),
            "expected a separator rule: {plain}"
        );
    }

    #[test]
    fn bash_card_single_line_command_has_no_preamble() {
        // A short single-line command is fully shown in the header, so the body
        // is just the output — no redundant `$ ` echo.
        let input = serde_json::json!({ "command": "ls -la" });
        let output = serde_json::json!({ "stdout": "file.txt", "stderr": "" });
        let plain = ok_card_plain(bash_card(&input, Some(&output), ToolStatus::Ok));
        assert!(plain.contains("Bash(ls -la)"), "{plain}");
        assert!(
            !plain.contains("$ ls -la"),
            "single-line must not repeat: {plain}"
        );
        assert!(plain.contains("file.txt"), "{plain}");
    }

    #[test]
    fn error_card_keeps_the_arguments() {
        // Regression: an errored tool used to collapse to just the tool name;
        // the identity must still show what was called (the command).
        let plain = strip_ansi(&format_tool_result(
            "bash",
            r#"{"command":"false"}"#,
            "exit code 1",
            true,
        ));
        assert!(plain.contains("Bash(false)"), "{plain}");
        assert!(plain.contains("exit code 1"), "{plain}");
    }
}
