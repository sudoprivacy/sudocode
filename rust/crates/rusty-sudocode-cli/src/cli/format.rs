//! Pure formatting and report functions extracted from `main.rs`.

use std::fmt::Write as _;

use api::{self, AuthMode, ProviderKind};
use runtime::{self, TokenUsage};
use std::time::Duration;

use crate::{
    load_sudocode_config_for_current_dir, GitWorkspaceSummary, InternalPromptProgressEvent,
    InternalPromptProgressState, BUILD_TARGET, DEFAULT_DATE, GIT_SHA, LATEST_SESSION_REFERENCE,
    PRIMARY_SESSION_EXTENSION, VERSION,
};

pub(crate) const DISPLAY_TRUNCATION_NOTICE: &str =
    "\x1b[2m… output truncated for display; full result preserved in session.\x1b[0m";
pub(crate) const READ_DISPLAY_MAX_LINES: usize = 10;
pub(crate) const READ_DISPLAY_MAX_CHARS: usize = 2_000;
/// Default upper bound on lines shown inline when summarizing tool results.
/// Anything beyond this is replaced with a "+N more lines" notice; the full
/// result is still preserved in the session file.
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 15;
pub(crate) const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;

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
    sudocode_config: &api::SudoCodeConfig,
) -> String {
    // Try to get provider label from sudocode.json config.
    let resolved_mode = mode.or_else(|| {
        // Auto-detect from model config: first available in priority order.
        const PRIORITY: &[&str] = &["subscription", "proxy", "api-key"];
        let entry = api::resolve_model(sudocode_config, model)?;
        let mode_str = PRIORITY
            .iter()
            .find(|m| entry.providers.contains_key(**m))?;
        AuthMode::parse(mode_str).ok()
    });
    let provider = {
        // Look up provider name from config entry's mapping for the resolved mode.
        let mode_key = resolved_mode.map(|m| m.label().to_string());
        api::resolve_model(sudocode_config, model)
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
        Some(m) => api::base_url_for_mode(m),
        None => api::read_base_url(),
    };
    let endpoint_hint = if base_url == api::DEFAULT_BASE_URL {
        String::new()
    } else {
        format!("\nEndpoint:  {base_url}")
    };
    format!("Connected: {model} via {provider}{auth_hint}{endpoint_hint}")
}

pub(crate) fn format_model_report(model: &str, message_count: usize, turns: u32) -> String {
    let config = load_sudocode_config_for_current_dir();
    let mut available_lines = String::new();
    for (alias, entry) in &config.models {
        let marker = if alias == &model.to_ascii_lowercase() {
            " *"
        } else {
            ""
        };
        let provider_modes: Vec<&str> = entry.providers.keys().map(String::as_str).collect();
        write!(
            available_lines,
            "\n    {:<16} {} ({}){marker}",
            alias,
            entry.name,
            provider_modes.join(", ")
        )
        .expect("write to string");
    }
    let available = if available_lines.is_empty() {
        String::from("opus, sonnet, haiku")
    } else {
        available_lines
    };
    format!(
        "Model
  Current model    {model}
  Available models{available}
  Session messages {message_count}
  Session turns    {turns}

Usage
  Switch models with /model <name>"
    )
}

pub(crate) fn format_model_switch_report(
    previous: &str,
    next: &str,
    message_count: usize,
) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved msgs   {message_count}"
    )
}

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
  Messages kept    {resulting_messages}"
        )
    }
}

pub(crate) fn format_auto_compaction_notice(removed: usize) -> String {
    format!("[auto-compacted: removed {removed} messages]")
}

pub(crate) fn format_sandbox_report(status: &runtime::SandboxStatus) -> String {
    format!(
        "Sandbox
  Enabled           {}
  Active            {}
  Supported         {}
  In container      {}
  Requested ns      {}
  Active ns         {}
  Requested net     {}
  Active net        {}
  Filesystem mode   {}
  Filesystem active {}
  Allowed mounts    {}
  Markers           {}
  Fallback reason   {}",
        status.enabled,
        status.active,
        status.supported,
        status.in_container,
        status.requested.namespace_restrictions,
        status.namespace_active,
        status.requested.network_isolation,
        status.network_active,
        status.filesystem_mode.as_str(),
        status.filesystem_active,
        if status.allowed_mounts.is_empty() {
            "<none>".to_string()
        } else {
            status.allowed_mounts.join(", ")
        },
        if status.container_markers.is_empty() {
            "<none>".to_string()
        } else {
            status.container_markers.join(", ")
        },
        status
            .fallback_reason
            .clone()
            .unwrap_or_else(|| "<none>".to_string()),
    )
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

pub(crate) fn format_user_visible_api_error(session_id: &str, error: &api::ApiError) -> String {
    if error.is_context_window_failure() {
        format_context_window_blocked_error(session_id, error)
    } else if error.is_generic_fatal_wrapper() {
        let mut qualifiers = vec![format!("session {session_id}")];
        if let Some(request_id) = error.request_id() {
            qualifiers.push(format!("trace {request_id}"));
        }
        format!(
            "{} ({}): {}",
            error.safe_failure_class(),
            qualifiers.join(", "),
            error
        )
    } else {
        error.to_string()
    }
}

pub(crate) fn format_context_window_blocked_error(
    session_id: &str,
    error: &api::ApiError,
) -> String {
    let mut lines = vec![
        "Context window blocked".to_string(),
        "  Failure class    context_window_blocked".to_string(),
        format!("  Session          {session_id}"),
    ];

    if let Some(request_id) = error.request_id() {
        lines.push(format!("  Trace            {request_id}"));
    }

    match error {
        api::ApiError::ContextWindowExceeded {
            model,
            estimated_input_tokens,
            requested_output_tokens,
            estimated_total_tokens,
            context_window_tokens,
        } => {
            lines.push(format!("  Model            {model}"));
            lines.push(format!(
                "  Input estimate   ~{estimated_input_tokens} tokens (heuristic)"
            ));
            lines.push(format!(
                "  Requested output {requested_output_tokens} tokens"
            ));
            lines.push(format!(
                "  Total estimate   ~{estimated_total_tokens} tokens (heuristic)"
            ));
            lines.push(format!("  Context window   {context_window_tokens} tokens"));
        }
        api::ApiError::Api { message, body, .. } => {
            let detail = message.as_deref().unwrap_or(body).trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        api::ApiError::RetriesExhausted { last_error, .. } => {
            let detail = match last_error.as_ref() {
                api::ApiError::Api { message, body, .. } => message.as_deref().unwrap_or(body),
                other => return format_context_window_blocked_error(session_id, other),
            }
            .trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        _ => {}
    }

    lines.push(String::new());
    lines.push("Recovery".to_string());
    lines.push("  Compact          /compact".to_string());
    lines.push(format!(
        "  Resume compact   scode --resume {session_id} /compact"
    ));
    lines.push("  Fresh session    /clear --confirm".to_string());
    lines.push(
        "  Reduce scope     remove large pasted context/files or ask for a smaller slice"
            .to_string(),
    );
    lines.push("  Retry            rerun after compacting or reducing the request".to_string());

    lines.join("\n")
}

pub(crate) fn format_tool_call_start(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    let detail = match name {
        "bash" | "Bash" => format_bash_call(&parsed),
        "read_file" | "Read" => {
            let path = extract_tool_path(&parsed);
            format!("\x1b[2m📄 Reading {path}…\x1b[0m")
        }
        "write_file" | "Write" => {
            let path = extract_tool_path(&parsed);
            let lines = parsed
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            format!("\x1b[1;32m✏️ Writing {path}\x1b[0m \x1b[2m({lines} lines)\x1b[0m")
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
            format!(
                "\x1b[1;33m📝 Editing {path}\x1b[0m{}",
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

    let border = "─".repeat(name.len() + 8);
    let detail_indented = detail.replace('\n', "\n  ");
    format!(
        "  \x1b[38;5;245m╭─ \x1b[1;36m{name}\x1b[0;38;5;245m ─╮\x1b[0m\n  \x1b[38;5;245m│\x1b[0m {detail_indented}\n  \x1b[38;5;245m╰{border}╯\x1b[0m"
    )
}

pub(crate) fn format_tool_result(name: &str, output: &str, is_error: bool) -> String {
    let icon = if is_error {
        "\x1b[1;31m⏺\x1b[0m"
    } else {
        "\x1b[1;32m⏺\x1b[0m"
    };
    if is_error {
        let summary = truncate_for_summary(output.trim(), 160);
        if summary.is_empty() {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
        } else {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n  \x1b[38;5;203m{summary}\x1b[0m")
        }
    } else {
        let parsed: serde_json::Value =
            serde_json::from_str(output).unwrap_or(serde_json::Value::String(output.to_string()));
        match name {
            "bash" | "Bash" => format_bash_result(icon, &parsed),
            "read_file" | "Read" => format_read_result(icon, &parsed),
            "write_file" | "Write" => format_write_result(icon, &parsed),
            "edit_file" | "Edit" => format_edit_result(icon, &parsed),
            "glob_search" | "Glob" => format_glob_result(icon, &parsed),
            "grep_search" | "Grep" => format_grep_result(icon, &parsed),
            _ => format_generic_tool_result(icon, name, &parsed),
        }
    }
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
    format!("{label} {pattern}\n\x1b[2min {scope}\x1b[0m")
}

pub(crate) fn format_patch_preview(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    Some(format!(
        "\x1b[38;5;203m- {}\x1b[0m\n\x1b[38;5;70m+ {}\x1b[0m",
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
        format!(
            "\x1b[48;5;236;38;5;255m $ {} \x1b[0m",
            truncate_for_summary(command, 160)
        )
    }
}

pub(crate) fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

pub(crate) fn format_bash_result(icon: &str, parsed: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    // Extract command from input for the header.
    let command = parsed
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();

    let mut header = if command.is_empty() {
        format!("{icon} \x1b[38;5;245mBash\x1b[0m")
    } else {
        format!(
            "{icon} \x1b[38;5;245mBash\x1b[0m({})",
            truncate_for_summary(command, 120)
        )
    };

    if let Some(task_id) = parsed
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
    {
        write!(&mut header, " backgrounded ({task_id})").expect("write to string");
    } else if let Some(status) = parsed
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        write!(&mut header, " {status}").expect("write to string");
    }

    let stdout_text = parsed
        .get("stdout")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let stderr_text = parsed
        .get("stderr")
        .and_then(|value| value.as_str())
        .unwrap_or_default();

    // Collect all output lines.
    let all_output: Vec<&str> = stdout_text
        .lines()
        .chain(stderr_text.lines())
        .filter(|line| !line.trim().is_empty())
        .collect();

    if all_output.is_empty() {
        return header;
    }

    let preview_count = TOOL_OUTPUT_DISPLAY_MAX_LINES;
    let mut result = header;

    for (i, line) in all_output.iter().take(preview_count).enumerate() {
        if i == 0 {
            write!(&mut result, "\n  └ {line}").expect("write to string");
        } else {
            write!(&mut result, "\n    {line}").expect("write to string");
        }
    }

    if all_output.len() > preview_count {
        let remaining = all_output.len() - preview_count;
        let line_or_lines = if remaining == 1 { "line" } else { "lines" };
        write!(
            &mut result,
            "\n  \x1b[2m… +{remaining} more {line_or_lines} · full output preserved in session\x1b[0m"
        )
        .expect("write to string");
    }

    result
}

pub(crate) fn format_read_result(icon: &str, parsed: &serde_json::Value) -> String {
    let file = parsed.get("file").unwrap_or(parsed);
    let path = extract_tool_path(file);
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
    let header = format!("{icon} \x1b[2mRead {path}\x1b[0m");
    if content.is_empty() {
        return header;
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
        return header;
    }
    let language = language_token_from_path(&path);
    let renderer = crate::render::TerminalRenderer::new();
    let highlighted = renderer.highlight_code(&visible_body, language);
    let mut indented = highlighted
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let remaining_lines = total_input_lines.saturating_sub(visible_count);
    if remaining_lines > 0 {
        let line_or_lines = if remaining_lines == 1 {
            "line"
        } else {
            "lines"
        };
        let _ = write!(
            indented,
            "\n  \x1b[2m… +{remaining_lines} more {line_or_lines} · full output preserved in session\x1b[0m"
        );
    } else if char_truncated {
        let _ = write!(indented, "\n  {DISPLAY_TRUNCATION_NOTICE}");
    }

    let mut out = String::with_capacity(header.len() + indented.len() + 8);
    out.push_str(&header);
    if let Some(total) = total_lines {
        let _ = write!(out, " \x1b[2m({total} lines)\x1b[0m");
    }
    out.push('\n');
    out.push_str(&indented);
    out
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

pub(crate) fn format_write_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let kind = parsed
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("write");
    let line_count = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .map_or(0, |content| content.lines().count());
    format!(
        "{icon} \x1b[1;32m✏️ {} {path}\x1b[0m \x1b[2m({line_count} lines)\x1b[0m",
        if kind == "create" { "Wrote" } else { "Updated" },
    )
}

pub(crate) fn format_structured_patch_preview(parsed: &serde_json::Value) -> Option<String> {
    let hunks = parsed.get("structuredPatch")?.as_array()?;
    let mut preview = Vec::new();
    for hunk in hunks.iter().take(2) {
        let lines = hunk.get("lines")?.as_array()?;
        for line in lines.iter().filter_map(|value| value.as_str()).take(6) {
            match line.chars().next() {
                Some('+') => preview.push(format!("\x1b[38;5;70m{line}\x1b[0m")),
                Some('-') => preview.push(format!("\x1b[38;5;203m{line}\x1b[0m")),
                _ => preview.push(line.to_string()),
            }
        }
    }
    if preview.is_empty() {
        None
    } else {
        Some(preview.join("\n"))
    }
}

pub(crate) fn format_edit_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let suffix = if parsed
        .get("replaceAll")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        " (replace all)"
    } else {
        ""
    };
    let preview = format_structured_patch_preview(parsed).or_else(|| {
        let old_value = parsed
            .get("oldString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let new_value = parsed
            .get("newString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        format_patch_preview(old_value, new_value)
    });

    match preview {
        Some(preview) => {
            let indented = preview.replace('\n', "\n  ");
            format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m\n  {indented}")
        }
        None => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m"),
    }
}

pub(crate) fn format_glob_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    format!("{icon} \x1b[2mFound {num_files} files\x1b[0m")
}

pub(crate) fn format_grep_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_matches = parsed
        .get("numMatches")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    format!("{icon} \x1b[2m{num_matches} matches across {num_files} files\x1b[0m")
}

pub(crate) fn format_generic_tool_result(
    icon: &str,
    name: &str,
    parsed: &serde_json::Value,
) -> String {
    let rendered_output = match parsed {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(parsed).unwrap_or_else(|_| parsed.to_string())
        }
        _ => parsed.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );

    if preview.is_empty() {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
    } else if preview.contains('\n') {
        let indented = preview.replace('\n', "\n  ");
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n  {indented}")
    } else {
        format!("{icon} \x1b[38;5;245m{name}:\x1b[0m {preview}")
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

pub(crate) fn format_turn_status_line(
    model: &str,
    turn: u32,
    usage: &TokenUsage,
    elapsed: Duration,
) -> String {
    format_turn_status_line_with_branch(model, turn, usage, elapsed, None)
}

/// Render the dim per-turn status line shown after each interactive turn.
///
/// Contains, in order: model name, turn number, cumulative token count,
/// estimated cost (when pricing for the model is known), elapsed wall-clock
/// time for the turn, and the current git branch (when one is available).
/// All fields are dimmed; turn and tokens are kept compact (`turn 3`,
/// `3.2k tokens`) so the line stays single-row even at narrow widths.
pub(crate) fn format_turn_status_line_with_branch(
    model: &str,
    turn: u32,
    usage: &TokenUsage,
    elapsed: Duration,
    branch: Option<&str>,
) -> String {
    let total = usage.total_tokens();
    let tokens_display = if total >= 1000 {
        format!("{:.1}k", f64::from(total) / 1000.0)
    } else {
        total.to_string()
    };
    let cost = usage.estimate_cost_usd().total_cost_usd();
    let cost_display = if cost > 0.0 {
        Some(format!("${cost:.2}"))
    } else {
        None
    };
    let secs = elapsed.as_secs_f64();

    let mut segments: Vec<String> = Vec::with_capacity(6);
    segments.push(format!("[{model}]"));
    segments.push(format!("turn {turn}"));
    segments.push(format!("{tokens_display} tokens"));
    if let Some(cost) = cost_display {
        segments.push(cost);
    }
    segments.push(format!("{secs:.1}s"));
    if let Some(branch) = branch.filter(|b| !b.is_empty()) {
        segments.push(branch.to_string());
    }
    format!("\x1b[2m{}\x1b[0m", segments.join(" · "))
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

    let grey = "\x1b[38;5;245m";
    let reset = "\x1b[0m";
    let bold_yellow = "\x1b[1;33m";
    let bold_cyan = "\x1b[1;36m";
    let dim = "\x1b[2m";

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
    let parts: Vec<String> = entries
        .into_iter()
        .map(|(name, ok)| {
            // Bold tool name; green check or red cross.
            let glyph = if ok {
                "\x1b[32m✓\x1b[0m"
            } else {
                "\x1b[31m✗\x1b[0m"
            };
            format!("\x1b[1m{name}\x1b[0m {glyph}")
        })
        .collect();
    let body = parts.join("  ");
    let plural = if count == 1 { "tool" } else { "tools" };
    let secs = elapsed.as_secs_f64();
    Some(format!(
        "🔧 {body} \x1b[2m({count} {plural}, {secs:.1}s)\x1b[0m"
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
                "\x1b[2m… +{remaining} more {line_or_lines} · full output preserved in session\x1b[0m",
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
        let error = api::ApiError::Api {
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

        let rendered = format_user_visible_api_error("session-issue-32", &error);
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
        };
        let rendered = format_turn_status_line_with_branch(
            "claude-opus-4-6",
            3,
            &usage,
            Duration::from_secs_f64(1.2),
            None,
        );
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("[claude-opus-4-6]"), "{plain}");
        assert!(plain.contains("turn 3"), "{plain}");
        assert!(plain.contains("1.5k tokens"), "{plain}");
        assert!(plain.contains("$"), "expected cost segment in {plain}");
        assert!(plain.contains("1.2s"), "{plain}");
    }

    #[test]
    fn turn_status_line_omits_cost_when_zero() {
        // With no tokens recorded the estimated cost is 0.0 and the cost
        // segment is omitted entirely rather than printing "$0.00".
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line_with_branch(
            "claude-opus-4-6",
            1,
            &usage,
            Duration::from_secs_f64(0.3),
            None,
        );
        let plain = strip_ansi(&rendered);
        assert!(!plain.contains("$"), "{plain}");
    }

    #[test]
    fn turn_status_line_appends_branch_when_present() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line_with_branch(
            "claude-opus-4-6",
            1,
            &usage,
            Duration::from_millis(800),
            Some("feat/tui-backlog-179"),
        );
        let plain = strip_ansi(&rendered);
        assert!(plain.ends_with("feat/tui-backlog-179"), "{plain}");
    }

    #[test]
    fn turn_status_line_omits_branch_when_empty() {
        let usage = TokenUsage::default();
        let rendered = format_turn_status_line_with_branch(
            "claude-opus-4-6",
            1,
            &usage,
            Duration::from_millis(800),
            Some(""),
        );
        let plain = strip_ansi(&rendered);
        // Trailing segment should be the duration, not an empty " · ".
        assert!(plain.ends_with("0.8s"), "{plain}");
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
        let rendered = format_read_result("⏺", &json);
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
        let rendered = format_read_result("⏺", &json);
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
        let rendered = format_read_result("⏺", &json);
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
        let rendered = format_read_result("⏺", &json);

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
        let rendered = format_read_result("⏺", &json);
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
}
