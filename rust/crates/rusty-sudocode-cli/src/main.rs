#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    clippy::doc_markdown,
    clippy::manual_string_new,
    clippy::match_same_arms,
    clippy::result_large_err,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
mod cli;
mod init;
mod input;
mod render;
mod vlm_describe;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use api::{
    base_url_for_mode, model_family_identity_for, resolve_startup_auth_source, AnthropicClient,
    AuthMode, AuthSource, ContentBlockDelta, InputContentBlock, InputMessage, MessageRequest,
    MessageResponse, OutputContentBlock, PromptCache, ProviderClient as ApiProviderClient,
    ProviderKind, StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition,
    ToolResultContentBlock,
};

use cli::api_client::{
    collect_prompt_cache_events, collect_tool_results, collect_tool_uses, final_assistant_text,
    AnthropicRuntimeClient,
};
use cli::args::{
    config_model_for_current_dir, default_permission_mode, format_unknown_slash_command,
    load_sudocode_config_for_current_dir, load_sudocode_config_for_cwd, parse_args,
    permission_mode_from_label, require_sudocode_config_for_cwd, resolve_model_alias,
    resolve_model_alias_with_config, resolve_repl_model, try_resolve_bare_skill_prompt,
    try_resolve_bare_skill_prompt_with_plugins, AllowedToolSet, CliAction, CliOutputFormat,
    LocalHelpTopic,
};
use cli::export::{
    collect_session_prompt_history, parse_history_count, render_export_text,
    render_prompt_history_report, resolve_export_path, run_export, truncate_for_prompt,
    PromptHistoryEntry,
};
use cli::format::{
    describe_tool_progress, first_visible_line, format_auth_report, format_auth_switch_report,
    format_auto_compaction_notice, format_bughunter_report, format_commit_preflight_report,
    format_commit_skipped_report, format_compact_report, format_cost_report, format_input_echo,
    format_internal_prompt_progress_line, format_issue_report, format_model_report,
    format_model_switch_report, format_permission_prompt_box, format_permissions_report,
    format_permissions_switch_report, format_pr_report, format_resume_report,
    format_sandbox_report, format_tool_timeline, format_turn_status_line_with_branch,
    format_ultraplan_report, render_resume_usage, render_version_report, truncate_for_summary,
};
use cli::git::{
    enforce_broad_cwd_policy, git_output, parse_git_status_branch, parse_git_status_metadata,
    parse_git_workspace_summary, resolve_git_branch_for, GitWorkspaceSummary,
};
use cli::help::{
    print_help, print_help_topic, render_config_json, render_config_report, render_diff_json_for,
    render_diff_report, render_diff_report_for, render_last_tool_debug_report, render_memory_json,
    render_memory_report, render_repl_help, render_teleport_report, validate_no_args,
};
use cli::mcp::{build_runtime_mcp_state, RuntimeMcpState};
use cli::pager::print_with_pager;
use cli::session::{
    confirm_session_deletion, create_managed_session_handle, create_managed_session_handle_for,
    delete_managed_session, format_session_picker_entry, list_managed_sessions,
    load_session_reference, new_cli_session, new_cli_session_for, render_session_list,
    resolve_session_reference, write_session_clear_backup, SessionHandle, LATEST_SESSION_REFERENCE,
};
use cli::status::{
    format_status_report, normalize_permission_mode, print_sandbox_status_snapshot,
    print_status_snapshot, print_version, sandbox_json_value, status_context, status_json_value,
    version_json_value, StatusContext, StatusUsage,
};
use cli::tool_executor::{permission_policy, CliToolExecutor};
use commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command_json_with_plugins, handle_mcp_slash_command_with_plugins,
    handle_plugins_slash_command, handle_skills_slash_command, handle_skills_slash_command_json,
    handle_skills_slash_command_json_with_plugins, handle_skills_slash_command_with_plugins,
    render_slash_command_help, render_slash_command_help_filtered, resolve_skill_invocation,
    resolve_skill_invocation_with_plugins, resume_supported_slash_commands, slash_command_specs,
    validate_slash_command_input, SkillSlashDispatch, SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use dialoguer::{FuzzySelect, Select};
use init::initialize_repo;
use plugins::{
    render_plugin_capabilities_section, PluginLoadOutcome, PluginManager, PluginRegistry,
};
use render::{MarkdownStreamState, Spinner, TerminalRenderer};
use runtime::{
    check_base_commit, compact_session, estimate_block_tokens, estimate_session_tokens,
    format_stale_base_warning, format_usd, load_oauth_credentials, load_system_prompt,
    pricing_for_model, resolve_expected_base, resolve_sandbox_status, should_compact, AcpError,
    ApiClient, ApiRequest, AssistantEvent, CompactionConfig, ConfigLoader, ConfigSource,
    ContentBlock, ConversationMessage, ConversationRuntime, McpServer, McpServerManager,
    McpServerSpec, McpTool, MessageRole, ModelPricing, PermissionMode, PermissionPolicy,
    ProjectContext, PromptCacheEvent, ResolvedPermissionMode, RuntimeError, Session, SystemPrompt,
    TokenUsage, ToolError, ToolExecutor, UsageTracker,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tools::{
    execute_tool, mvp_tool_specs, GlobalToolRegistry, RuntimeToolDefinition, ToolSearchOutput,
};

const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// #148: Model provenance for `scode status` JSON/text output. Records where
/// the resolved model string came from so consumers don't have to re-read argv
/// to audit whether their `--model` flag was honored vs falling back to env
/// or config or default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelSource {
    /// Explicit `--model` / `--model=` CLI flag.
    Flag,
    /// ANTHROPIC_MODEL environment variable (when no flag was passed).
    Env,
    /// `model` key in `.scode.json` / `.nexus/sudocode/settings.json` (when neither
    /// flag nor env set it).
    Config,
    /// Compiled-in DEFAULT_MODEL fallback.
    Default,
}

impl ModelSource {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            ModelSource::Flag => "flag",
            ModelSource::Env => "env",
            ModelSource::Config => "config",
            ModelSource::Default => "default",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelProvenance {
    /// Resolved model string (after alias expansion).
    pub(crate) resolved: String,
    /// Raw user input before alias resolution. None when source is Default.
    pub(crate) raw: Option<String>,
    /// Where the resolved model string originated.
    pub(crate) source: ModelSource,
}

impl ModelProvenance {
    fn default_fallback() -> Self {
        Self {
            resolved: DEFAULT_MODEL.to_string(),
            raw: None,
            source: ModelSource::Default,
        }
    }

    fn from_flag(raw: &str) -> Self {
        Self {
            resolved: resolve_model_alias_with_config(raw),
            raw: Some(raw.to_string()),
            source: ModelSource::Flag,
        }
    }

    /// Look up the default model from env, then cwd config, then the compiled-in
    /// fallback. Called when no `--model` flag was passed. Shares its primitive
    /// (`lookup_default_model`) with `resolve_repl_model`, so the splash, the
    /// one-shot Prompt action, and the status banner all agree on the active
    /// model.
    fn from_default_lookup() -> Self {
        lookup_default_model().map_or_else(Self::default_fallback, |(resolved, raw, source)| Self {
            resolved,
            raw: Some(raw),
            source,
        })
    }
}

/// Single source of truth for the env-or-config default model lookup. Returns
/// `(resolved, raw, source)` when env or config wins, `None` to defer to the
/// compiled-in default.
pub(crate) fn lookup_default_model() -> Option<(String, String, ModelSource)> {
    if let Some(env_model) = env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some((
            resolve_model_alias_with_config(&env_model),
            env_model,
            ModelSource::Env,
        ));
    }
    if let Some(config_model) = config_model_for_current_dir() {
        return Some((
            resolve_model_alias_with_config(&config_model),
            config_model,
            ModelSource::Config,
        ));
    }
    None
}

// Build-time constants injected by build.rs (fall back to static values when
// build.rs hasn't run, e.g. in doc-test or unusual toolchain environments).
pub(crate) const DEFAULT_DATE: &str = match option_env!("BUILD_DATE") {
    Some(d) => d,
    None => "unknown",
};
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 4545;
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
pub(crate) const BUILD_TARGET: Option<&str> = option_env!("TARGET");
pub(crate) const GIT_SHA: Option<&str> = option_env!("GIT_SHA");
const INTERNAL_PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const PRIMARY_SESSION_EXTENSION: &str = "jsonl";
const LEGACY_SESSION_EXTENSION: &str = "json";
pub(crate) const OFFICIAL_REPO_URL: &str = "https://github.com/sudoprivacy/sudocode";
pub(crate) const OFFICIAL_REPO_SLUG: &str = "sudoprivacy/sudocode";
pub(crate) const DEPRECATED_INSTALL_COMMAND: &str = "cargo install sudocode";
type RuntimePluginStateBuildOutput = (
    Option<Arc<Mutex<RuntimeMcpState>>>,
    Vec<RuntimeToolDefinition>,
);

/// Enable ANSI/VT escape-sequence processing on the Windows console.
///
/// Much of the CLI emits raw ANSI escapes via `println!`/`write!` (banner,
/// status bar, tool output, separators, etc.) instead of routing every byte
/// through crossterm. On Windows the console has virtual-terminal processing
/// disabled by default, so those escapes render as literal garbage (e.g.
/// `[2m`, `[38;5;245m`, `[0m`). crossterm only flips the VT flag on its first
/// command execution — which, via `Spinner::start()`, happens deep inside
/// `run_turn`, long after the banner and other early output have already been
/// written with raw escapes. Calling this at the very top of `main` triggers
/// crossterm's `enable_vt_processing()` up front so all subsequent raw escapes
/// are interpreted correctly. No-op on non-Windows platforms.
#[cfg(windows)]
fn enable_windows_ansi_support() {
    // Side effect: on first call this enables ENABLE_VIRTUAL_TERMINAL_PROCESSING
    // on the current stdout console handle. We ignore the returned support flag.
    let _ = crossterm::ansi_support::supports_ansi();
}

#[cfg(not(windows))]
fn enable_windows_ansi_support() {}

fn main() {
    // Must run before any output so early raw ANSI escapes render correctly on
    // the Windows console (see `enable_windows_ansi_support`).
    enable_windows_ansi_support();

    if let Err(error) = run() {
        let message = error.to_string();
        // When --output-format json is active, emit errors as JSON so downstream
        // tools can parse failures the same way they parse successes (ROADMAP #42).
        let argv: Vec<String> = std::env::args().collect();
        let json_output = argv
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "json")
            || argv.iter().any(|a| a == "--output-format=json");
        if json_output {
            // #77: classify error by prefix so downstream consumers can route without
            // regex-scraping the prose. Split short-reason from hint-runbook.
            // #64: emit to stdout (not stderr) so JSON-mode consumers capturing only
            // stdout receive errors with the same envelope as success responses.
            let kind = classify_error_kind(&message);
            let (short_reason, hint) = split_error_hint(&message);
            println!(
                "{}",
                serde_json::json!({
                    "type": "error",
                    "error": short_reason,
                    "kind": kind,
                    "hint": hint,
                })
            );
        } else {
            // #156: Add machine-readable error kind to text output so stderr observers
            // don't need to regex-scrape the prose.
            let kind = classify_error_kind(&message);
            if message.contains("`scode --help`") {
                eprintln!(
                    "[error-kind: {kind}]
error: {message}"
                );
            } else {
                eprintln!(
                    "[error-kind: {kind}]
error: {message}

Run `scode --help` for usage."
                );
            }
        }
        std::process::exit(1);
    }
}

/// #77: Classify a stringified error message into a machine-readable kind.
///
/// Returns a snake_case token that downstream consumers can switch on instead
/// of regex-scraping the prose. The classification is best-effort prefix/keyword
/// matching against the error messages produced throughout the CLI surface.
fn classify_error_kind(message: &str) -> &'static str {
    // Check specific patterns first (more specific before generic)
    if message.contains("missing sudocode.json") {
        "missing_config"
    } else if message.contains("missing Anthropic credentials") {
        "missing_credentials"
    } else if message.contains("Manifest source files are missing") {
        "missing_manifests"
    } else if message.contains("no worker state file found") {
        "missing_worker_state"
    } else if message.contains("session not found") {
        "session_not_found"
    } else if message.contains("failed to restore session") {
        "session_load_failed"
    } else if message.contains("no managed sessions found") {
        "no_managed_sessions"
    } else if message.contains("unrecognized argument") || message.contains("unknown option") {
        "cli_parse"
    } else if message.contains("invalid model syntax") {
        "invalid_model_syntax"
    } else if message.contains("is not yet implemented") {
        "unsupported_command"
    } else if message.contains("unsupported resumed command") {
        "unsupported_resumed_command"
    } else if message.contains("confirmation required") {
        "confirmation_required"
    } else if message.contains("api failed") || message.contains("api returned") {
        "api_http_error"
    } else {
        "unknown"
    }
}

/// #77: Split a multi-line error message into (short_reason, optional_hint).
///
/// The short_reason is the first line (up to the first newline), and the hint
/// is the remaining text or `None` if there's no newline. This prevents the
/// runbook prose from being stuffed into the `error` field that downstream
/// parsers expect to be the short reason alone.
fn split_error_hint(message: &str) -> (String, Option<String>) {
    match message.split_once('\n') {
        Some((short, hint)) => (short.to_string(), Some(hint.trim().to_string())),
        None => (message.to_string(), None),
    }
}

/// Read piped stdin content when stdin is not a terminal.
///
/// Returns `None` when stdin is attached to a terminal (interactive REPL use),
/// when reading fails, or when the piped content is empty after trimming.
/// Returns `Some(raw_content)` when a pipe delivered non-empty content.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    if io::stdin().read_to_string(&mut buffer).is_err() {
        return None;
    }
    if buffer.trim().is_empty() {
        return None;
    }
    Some(buffer)
}

/// Merge a piped stdin payload into a prompt argument.
///
/// When `stdin_content` is `None` or empty after trimming, the prompt is
/// returned unchanged. Otherwise the trimmed stdin content is appended to the
/// prompt separated by a blank line so the model sees the prompt first and the
/// piped context immediately after it.
fn merge_prompt_with_stdin(prompt: &str, stdin_content: Option<&str>) -> String {
    let Some(raw) = stdin_content else {
        return prompt.to_string();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return prompt.to_string();
    }
    if prompt.is_empty() {
        return trimmed.to_string();
    }
    format!("{prompt}\n\n{trimmed}")
}

/// Extract sudorouter base URL and API key from the sudocode config.
fn extract_sudorouter_credentials(config: &api::SudoCodeConfig) -> Option<(String, String)> {
    let proxy = config.auth_modes.get("proxy")?;
    let sr = proxy.get("sudorouter")?;
    let base_url = &sr.base_url;
    let api_key = sr.api_key.as_deref()?;
    if base_url.is_empty() || api_key.is_empty() {
        return None;
    }
    Some((base_url.clone(), api_key.to_string()))
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let action = parse_args(&args)?;
    // Informational commands (help, version, config, login, logout) are
    // dispatched immediately and must never block on a credential check.
    // If an ensure_authenticated() call is ever added below this point it
    // MUST be guarded by `if !action.is_informational()`.
    match action {
        CliAction::DumpManifests {
            output_format,
            manifests_dir,
        } => dump_manifests(manifests_dir.as_deref(), output_format)?,
        CliAction::BootstrapPlan { output_format } => print_bootstrap_plan(output_format)?,
        CliAction::Agents {
            args,
            output_format,
        } => LiveCli::print_agents(args.as_deref(), output_format)?,
        CliAction::Mcp {
            args,
            output_format,
        } => LiveCli::print_mcp(args.as_deref(), output_format)?,
        CliAction::Skills {
            args,
            output_format,
        } => LiveCli::print_skills(args.as_deref(), output_format)?,
        CliAction::Plugins {
            action,
            target,
            output_format,
        } => LiveCli::print_plugins(action.as_deref(), target.as_deref(), output_format)?,
        CliAction::Cron {
            args,
            output_format,
        } => cli::cron::run(&args, output_format)?,
        CliAction::PrintSystemPrompt {
            cwd,
            date,
            model,
            output_format,
        } => print_system_prompt(cwd, date, &model, output_format)?,
        CliAction::Version { output_format } => print_version(output_format)?,
        CliAction::ResumeSession {
            session_path,
            commands,
            output_format,
        } => resume_session(&session_path, &commands, output_format),
        CliAction::Status {
            model,
            model_flag_raw,
            permission_mode,
            output_format,
        } => print_status_snapshot(
            &model,
            model_flag_raw.as_deref(),
            permission_mode,
            output_format,
        )?,
        CliAction::Sandbox { output_format } => print_sandbox_status_snapshot(output_format)?,
        CliAction::Prompt {
            prompt,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            auth_mode,
        } => {
            enforce_broad_cwd_policy(allow_broad_cwd, output_format)?;
            run_stale_base_preflight(base_commit.as_deref());
            // Only consume piped stdin as prompt context when the permission
            // mode is fully unattended. In modes where the permission
            // prompter may invoke CliPermissionPrompter::decide(), stdin
            // must remain available for interactive approval; otherwise the
            // prompter's read_line() would hit EOF and deny every request.
            let stdin_context = if matches!(permission_mode, PermissionMode::DangerFullAccess) {
                read_piped_stdin()
            } else {
                None
            };
            let effective_prompt = merge_prompt_with_stdin(&prompt, stdin_context.as_deref());
            let session_start = Instant::now();
            // Share the splash's env/config resolution so the one-shot prompt
            // can't disagree with the REPL banner.
            let resolved_model = resolve_repl_model(model);
            let mut cli = LiveCli::new(
                resolved_model,
                true,
                allowed_tools,
                permission_mode,
                auth_mode,
            )?;
            cli.set_reasoning_effort(reasoning_effort);
            cli.run_turn_with_output(&effective_prompt, output_format, compact)?;

            // Record token usage and session ended event for non-interactive prompt mode
            let duration_ms = session_start.elapsed().as_millis() as u64;
            let usage = cli.runtime.usage().cumulative_usage();
            let total_turns = cli.runtime.usage().turns();
            if let Some(tracer) = cli.session_tracer() {
                tracer.record_usage(
                    "session_summary".to_string(),
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                );
                tracer.record_session_ended(
                    total_turns,
                    usage.input_tokens as u64,
                    usage.output_tokens as u64,
                    duration_ms,
                );
            }
        }
        CliAction::Doctor { output_format } => run_doctor(output_format)?,
        CliAction::Acp {
            model,
            model_flag_raw,
            allowed_tools,
            permission_mode_override,
            reasoning_effort,
            auth_mode,
            ws_port,
        } => {
            run_acp_server(
                model,
                model_flag_raw,
                allowed_tools,
                permission_mode_override,
                reasoning_effort,
                auth_mode,
                ws_port,
            )?;
        }
        CliAction::State { output_format } => run_worker_state(output_format)?,
        CliAction::Init { output_format } => run_init(output_format)?,
        // #146: dispatch pure-local introspection. Text mode uses existing
        // render_config_report/render_diff_report; JSON mode uses the
        // corresponding _json helpers already exposed for resume sessions.
        CliAction::Config {
            section,
            output_format,
        } => match output_format {
            CliOutputFormat::Text => {
                println!("{}", render_config_report(section.as_deref())?);
            }
            CliOutputFormat::Json => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&render_config_json(section.as_deref())?)?
                );
            }
        },
        CliAction::Diff { output_format } => match output_format {
            CliOutputFormat::Text => {
                println!("{}", render_diff_report()?);
            }
            CliOutputFormat::Json => {
                let cwd = env::current_dir()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&render_diff_json_for(&cwd)?)?
                );
            }
        },
        CliAction::Export {
            session_reference,
            output_path,
            output_format,
        } => run_export(&session_reference, output_path.as_deref(), output_format)?,
        CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            auth_mode,
        } => run_repl(
            model,
            allowed_tools,
            permission_mode,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            auth_mode,
        )?,
        CliAction::HelpTopic {
            topic,
            output_format,
        } => print_help_topic(topic, output_format)?,
        CliAction::Help { output_format } => print_help(output_format)?,
        CliAction::Login => run_login()?,
        CliAction::Logout => run_logout()?,
    }
    Ok(())
}

fn run_login() -> Result<(), Box<dyn std::error::Error>> {
    let token_set = runtime::import_claude_code_credentials()
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    eprintln!("Login successful — imported credentials from Claude Code.");
    if !token_set.scopes.is_empty() {
        eprintln!("Scopes: {}", token_set.scopes.join(", "));
    }
    Ok(())
}

fn run_logout() -> Result<(), Box<dyn std::error::Error>> {
    runtime::clear_oauth_credentials()?;
    eprintln!("Logged out. Credentials cleared from keychain and file.");
    Ok(())
}

use cli::doctor::{render_doctor_report, run_doctor};

/// Starts a minimal Model Context Protocol server that exposes scode's
/// built-in tools over stdio.
///
/// Tool descriptors come from [`tools::mvp_tool_specs`] and calls are
/// dispatched through [`tools::execute_tool`], so this server exposes exactly
/// Read `.nexus/sudocode/worker-state.json` from the current working directory and print it.
/// This is the file-based worker observability surface: `push_event()` in `worker_boot.rs`
/// atomically writes state transitions here so external observers (sudocodehip, orchestrators)
/// can poll current `WorkerStatus` without needing an HTTP route on the opencode binary.
fn run_worker_state(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let state_path = cwd
        .join(".nexus")
        .join("sudocode")
        .join("worker-state.json");
    if !state_path.exists() {
        // #139: this error used to say "run a worker first" without telling
        // callers how to run one. "worker" is an internal concept (there is
        // no `scode worker` subcommand), so consumers/CI had no discoverable path
        // from the error to a fix. Emit an actionable, structured error that
        // names the two concrete commands that produce worker state.
        //
        // Format in both text and JSON modes is stable so scripts can match:
        //   error: no worker state file found at <path>
        //     Hint: worker state is written by the interactive REPL or a non-interactive prompt.
        //     Run:   scode               # start the REPL (writes state on first turn)
        //     Or:    scode prompt <text> # run one non-interactive turn
        //     Then rerun: scode state [--output-format json]
        return Err(format!(
            "no worker state file found at {path}\n  Hint: worker state is written by the interactive REPL or a non-interactive prompt.\n  Run:   scode               # start the REPL (writes state on first turn)\n  Or:    scode prompt <text> # run one non-interactive turn\n  Then rerun: scode state [--output-format json]",
            path = state_path.display()
        )
        .into());
    }
    let raw = std::fs::read_to_string(&state_path)?;
    match output_format {
        CliOutputFormat::Text => println!("{raw}"),
        CliOutputFormat::Json => {
            // Validate it parses as JSON before re-emitting
            let _: serde_json::Value = serde_json::from_str(&raw)?;
            println!("{raw}");
        }
    }
    Ok(())
}

/// the same surface the in-process agent loop uses.
fn run_mcp_serve() -> Result<(), Box<dyn std::error::Error>> {
    let tools = mvp_tool_specs()
        .into_iter()
        .map(|spec| McpTool {
            name: spec.name.to_string(),
            description: Some(spec.description.to_string()),
            input_schema: Some(spec.input_schema),
            annotations: None,
            meta: None,
        })
        .collect();

    let spec = McpServerSpec {
        server_name: "scode".to_string(),
        server_version: VERSION.to_string(),
        tools,
        tool_handler: Box::new(execute_tool),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut server = McpServer::new(spec);
        server.run().await
    })?;
    Ok(())
}

fn dump_manifests(
    manifests_dir: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    dump_manifests_at_path(&workspace_dir, manifests_dir, output_format)
}

const DUMP_MANIFESTS_OVERRIDE_HINT: &str =
    "Hint: set CLAUDE_CODE_UPSTREAM=/path/to/upstream or pass `scode dump-manifests --manifests-dir /path/to/upstream`.";

// Internal function for testing that accepts a workspace directory path.
fn dump_manifests_at_path(
    workspace_dir: &std::path::Path,
    manifests_dir: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths = if let Some(dir) = manifests_dir {
        let resolved = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        UpstreamPaths::from_repo_root(resolved)
    } else {
        // Surface the resolved path in the error so users can diagnose missing
        // manifest files without guessing what path the binary expected.
        let resolved = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());
        UpstreamPaths::from_workspace_dir(&resolved)
    };

    let source_root = paths.repo_root();
    if !source_root.exists() {
        return Err(format!(
            "Manifest source directory does not exist.\n  looked in: {}\n  {DUMP_MANIFESTS_OVERRIDE_HINT}",
            source_root.display(),
        )
        .into());
    }

    let required_paths = [
        ("src/commands.ts", paths.commands_path()),
        ("src/tools.ts", paths.tools_path()),
        ("src/entrypoints/cli.tsx", paths.cli_path()),
    ];
    let missing = required_paths
        .iter()
        .filter_map(|(label, path)| (!path.is_file()).then_some(*label))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "Manifest source files are missing.\n  repo root: {}\n  missing: {}\n  {DUMP_MANIFESTS_OVERRIDE_HINT}",
            source_root.display(),
            missing.join(", "),
        )
        .into());
    }

    match extract_manifest(&paths) {
        Ok(manifest) => {
            match output_format {
                CliOutputFormat::Text => {
                    println!("commands: {}", manifest.commands.entries().len());
                    println!("tools: {}", manifest.tools.entries().len());
                    println!("bootstrap phases: {}", manifest.bootstrap.phases().len());
                }
                CliOutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "kind": "dump-manifests",
                        "commands": manifest.commands.entries().len(),
                        "tools": manifest.tools.entries().len(),
                        "bootstrap_phases": manifest.bootstrap.phases().len(),
                    }))?
                ),
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to extract manifests: {error}\n  looked in: {path}\n  {DUMP_MANIFESTS_OVERRIDE_HINT}",
            path = paths.repo_root().display()
        )
        .into()),
    }
}

fn print_bootstrap_plan(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let phases = runtime::BootstrapPlan::claude_code_default()
        .phases()
        .iter()
        .map(|phase| format!("{phase:?}"))
        .collect::<Vec<_>>();
    match output_format {
        CliOutputFormat::Text => {
            for phase in &phases {
                println!("- {phase}");
            }
        }
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "bootstrap-plan",
                "phases": phases,
            }))?
        ),
    }
    Ok(())
}

fn print_system_prompt(
    cwd: PathBuf,
    date: String,
    model: &str,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut prompt = load_system_prompt(
        cwd.clone(),
        date,
        env::consts::OS,
        "unknown",
        model_family_identity_for(model),
    )?;
    // Mirror what build_runtime_with_plugin_state does for live sessions:
    // append active SudoCode plugin capabilities so system-prompt output
    // matches what the runtime actually sends.  Load failures captured inside
    // PluginLoadOutcome are excluded naturally; Result errors propagate.
    let outcome = plugin_load_outcome_for_cwd(&cwd)?;
    if let Some(section) = render_plugin_capabilities_section(&outcome.loaded_plugins) {
        prompt.dynamic_sections.push(section);
    }
    // Coordinator mode: when SUDOCODE_COORDINATOR_MODE is set,
    // prepend the CC-fork coordinator role prompt so `scode
    // print-system-prompt` reflects what the runtime would send.
    runtime::coordinator_mode::apply_coordinator_prompt_if_enabled(&mut prompt);
    let message = prompt.render();
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json => {
            let mut all_sections = prompt.static_sections.clone();
            all_sections.extend(prompt.dynamic_sections.iter().cloned());
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "kind": "system-prompt",
                    "message": message,
                    "sections": all_sections,
                }))?
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn resume_session(session_path: &Path, commands: &[String], output_format: CliOutputFormat) {
    let session_reference = session_path.display().to_string();
    let (handle, session) = match load_session_reference(&session_reference) {
        Ok(loaded) => loaded,
        Err(error) => {
            if output_format == CliOutputFormat::Json {
                // #77: classify session load errors for downstream consumers
                let full_message = format!("failed to restore session: {error}");
                let kind = classify_error_kind(&full_message);
                let (short_reason, hint) = split_error_hint(&full_message);
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "error": short_reason,
                        "kind": kind,
                        "hint": hint,
                    })
                );
            } else {
                eprintln!("failed to restore session: {error}");
            }
            std::process::exit(1);
        }
    };
    let resolved_path = handle.path.clone();

    if commands.is_empty() {
        if output_format == CliOutputFormat::Json {
            println!(
                "{}",
                serde_json::json!({
                    "kind": "restored",
                    "session_id": session.session_id,
                    "path": handle.path.display().to_string(),
                    "message_count": session.messages.len(),
                })
            );
        } else {
            println!(
                "Restored session from {} ({} messages).",
                handle.path.display(),
                session.messages.len()
            );
        }
        return;
    }

    let mut session = session;
    for raw_command in commands {
        // Intercept spec commands that have no parse arm before calling
        // SlashCommand::parse — they return Err(SlashCommandParseError) which
        // formats as the confusing circular "Did you mean /X?" message.
        // STUB_COMMANDS covers both completions-filtered stubs and parse-less
        // spec entries; treat both as unsupported in resume mode.
        {
            let cmd_root = raw_command
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            if STUB_COMMANDS.contains(&cmd_root) {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("/{cmd_root} is not yet implemented in this build"),
                            "kind": "unsupported_command",
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("/{cmd_root} is not yet implemented in this build");
                }
                std::process::exit(2);
            }
        }
        let command = match SlashCommand::parse(raw_command) {
            Ok(Some(command)) => command,
            Ok(None) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("unsupported resumed command: {raw_command}"),
                            "kind": "unsupported_resumed_command",
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("unsupported resumed command: {raw_command}");
                }
                std::process::exit(2);
            }
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": error.to_string(),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                std::process::exit(2);
            }
        };
        match run_resume_command(&resolved_path, &session, &command) {
            Ok(ResumeCommandOutcome {
                session: next_session,
                message,
                json,
            }) => {
                session = next_session;
                if output_format == CliOutputFormat::Json {
                    if let Some(value) = json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&value)
                                .expect("resume command json output")
                        );
                    } else if let Some(message) = message {
                        println!("{message}");
                    }
                } else if let Some(message) = message {
                    println!("{message}");
                }
            }
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": error.to_string(),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                std::process::exit(2);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ResumeCommandOutcome {
    session: Session,
    message: Option<String>,
    json: Option<serde_json::Value>,
}

#[allow(clippy::too_many_lines)]
fn run_resume_command(
    session_path: &Path,
    session: &Session,
    command: &SlashCommand,
) -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
    match command {
        SlashCommand::Help => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_repl_help()),
            json: Some(serde_json::json!({ "kind": "help", "text": render_repl_help() })),
        }),
        SlashCommand::Compact => {
            let result = runtime::compact_session(
                session,
                CompactionConfig {
                    max_estimated_tokens: 0,
                    ..CompactionConfig::default()
                },
            );
            let removed = result.removed_message_count;
            let kept = result.compacted_session.messages.len();
            let skipped = removed == 0;
            result.compacted_session.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: result.compacted_session,
                message: Some(format_compact_report(removed, kept, skipped)),
                json: Some(serde_json::json!({
                    "kind": "compact",
                    "skipped": skipped,
                    "removed_messages": removed,
                    "kept_messages": kept,
                })),
            })
        }
        SlashCommand::Clear { confirm } => {
            if !confirm {
                return Ok(ResumeCommandOutcome {
                    session: session.clone(),
                    message: Some(
                        "clear: confirmation required; rerun with /clear --confirm".to_string(),
                    ),
                    json: Some(serde_json::json!({
                        "kind": "error",
                        "error": "confirmation required",
                        "hint": "rerun with /clear --confirm",
                    })),
                });
            }
            let backup_path = write_session_clear_backup(session, session_path)?;
            let previous_session_id = session.session_id.clone();
            let cleared = new_cli_session()?;
            let new_session_id = cleared.session_id.clone();
            cleared.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: cleared,
                message: Some(format!(
                    "Session cleared\n  Mode             resumed session reset\n  Previous session {previous_session_id}\n  Backup           {}\n  Resume previous  scode --resume {}\n  New session      {new_session_id}\n  Session file     {}",
                    backup_path.display(),
                    backup_path.display(),
                    session_path.display()
                )),
                json: Some(serde_json::json!({
                    "kind": "clear",
                    "previous_session_id": previous_session_id,
                    "new_session_id": new_session_id,
                    "backup": backup_path.display().to_string(),
                    "session_file": session_path.display().to_string(),
                })),
            })
        }
        SlashCommand::Status => {
            let tracker = UsageTracker::from_session(session);
            let usage = tracker.cumulative_usage();
            let context = status_context(Some(session_path))?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_status_report(
                    session.model.as_deref().unwrap_or("restored-session"),
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &context,
                    None, // #148: resumed sessions don't have flag provenance
                )),
                json: Some(status_json_value(
                    session.model.as_deref(),
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &context,
                    None, // #148: resumed sessions don't have flag provenance
                )),
            })
        }
        SlashCommand::Sandbox => {
            let cwd = env::current_dir()?;
            let loader = ConfigLoader::default_for(&cwd);
            let runtime_config = loader.load()?;
            let status = resolve_sandbox_status(runtime_config.sandbox(), &cwd);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_sandbox_report(&status)),
                json: Some(sandbox_json_value(&status)),
            })
        }
        SlashCommand::Cost => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
                json: Some(serde_json::json!({
                    "kind": "cost",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "total_tokens": usage.total_tokens(),
                })),
            })
        }
        SlashCommand::Config { section } => {
            let message = render_config_report(section.as_deref())?;
            let json = render_config_json(section.as_deref())?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json),
            })
        }
        SlashCommand::Mcp { action, target } => {
            let cwd = env::current_dir()?;
            let args = match (action.as_deref(), target.as_deref()) {
                (None, None) => None,
                (Some(action), None) => Some(action.to_string()),
                (Some(action), Some(target)) => Some(format!("{action} {target}")),
                (None, Some(target)) => Some(target.to_string()),
            };
            let plugin_load_outcome = plugin_load_outcome_for_cwd(&cwd).ok();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_mcp_slash_command_with_plugins(
                    args.as_deref(),
                    &cwd,
                    plugin_load_outcome.as_ref(),
                )?),
                json: Some(handle_mcp_slash_command_json_with_plugins(
                    args.as_deref(),
                    &cwd,
                    plugin_load_outcome.as_ref(),
                )?),
            })
        }
        SlashCommand::Memory => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_memory_report()?),
            json: Some(render_memory_json()?),
        }),
        SlashCommand::Init => {
            // #142: run the init once, then render both text + structured JSON
            // from the same InitReport so both surfaces stay in sync.
            let cwd = env::current_dir()?;
            let report = crate::init::initialize_repo(&cwd)?;
            let message = report.render();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message.clone()),
                json: Some(init_json_value(&report, &message)),
            })
        }
        SlashCommand::Diff => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let message = render_diff_report_for(&cwd)?;
            let json = render_diff_json_for(&cwd)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json),
            })
        }
        SlashCommand::Undo => {
            let already_undone = std::collections::HashSet::new();
            match crate::cli::undo::find_last_undoable_edit(&session.messages, &already_undone) {
                None => Ok(ResumeCommandOutcome {
                    session: session.clone(),
                    message: Some(
                        "Nothing to undo in this session. /undo only restores edit_file and write_file results recorded in the loaded session.".to_string(),
                    ),
                    json: Some(serde_json::json!({
                        "kind": "undo",
                        "applied": false,
                        "reason": "no eligible tool result",
                    })),
                }),
                Some(edit) => {
                    let summary = crate::cli::undo::apply_undo(&edit)?;
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(summary),
                        json: Some(serde_json::json!({
                            "kind": "undo",
                            "applied": true,
                            "tool_name": edit.tool_name,
                            "tool_use_id": edit.tool_use_id,
                            "file_path": edit.file_path,
                            "deleted": edit.original_file.is_none(),
                        })),
                    })
                }
            }
        }
        SlashCommand::Version => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_version_report()),
            json: Some(version_json_value()),
        }),
        SlashCommand::Export { path } => {
            let export_path = resolve_export_path(path.as_deref(), session)?;
            fs::write(&export_path, render_export_text(session))?;
            let msg_count = session.messages.len();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
                    export_path.display(),
                    msg_count,
                )),
                json: Some(serde_json::json!({
                    "kind": "export",
                    "file": export_path.display().to_string(),
                    "message_count": msg_count,
                })),
            })
        }
        SlashCommand::Agents { args } => {
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_agents_slash_command(args.as_deref(), &cwd)?),
                json: Some(
                    serde_json::to_value(handle_agents_slash_command_json(args.as_deref(), &cwd)?)
                        .unwrap_or_else(|_| serde_json::json!(null)),
                ),
            })
        }
        SlashCommand::Skills { args } => {
            if let SkillSlashDispatch::Invoke(_) = classify_skills_slash_command(args.as_deref()) {
                return Err(
                    "resumed /skills invocations are interactive-only; start `scode` and run `/skills <skill>` in the REPL".into(),
                );
            }
            let cwd = env::current_dir()?;
            let plugin_load_outcome = plugin_load_outcome_for_cwd(&cwd)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_skills_slash_command_with_plugins(
                    args.as_deref(),
                    &cwd,
                    Some(&plugin_load_outcome),
                )?),
                json: Some(handle_skills_slash_command_json_with_plugins(
                    args.as_deref(),
                    &cwd,
                    Some(&plugin_load_outcome),
                )?),
            })
        }
        SlashCommand::Doctor => {
            let report = render_doctor_report()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(report.render()),
                json: Some(report.json_value()),
            })
        }
        SlashCommand::Stats => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
                json: Some(serde_json::json!({
                    "kind": "stats",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "total_tokens": usage.total_tokens(),
                })),
            })
        }
        SlashCommand::History { count } => {
            let limit = parse_history_count(count.as_deref())
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            let entries = collect_session_prompt_history(session);
            let shown: Vec<_> = entries.iter().rev().take(limit).rev().collect();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_prompt_history_report(&entries, limit)),
                json: Some(serde_json::json!({
                    "kind": "history",
                    "total": entries.len(),
                    "showing": shown.len(),
                    "entries": shown.iter().map(|e| serde_json::json!({
                        "timestamp_ms": e.timestamp_ms,
                        "text": e.text,
                    })).collect::<Vec<_>>(),
                })),
            })
        }
        SlashCommand::Unknown(name) => Err(format_unknown_slash_command(name).into()),
        // /session list can be served from the sessions directory without a live session.
        SlashCommand::Session {
            action: Some(ref act),
            ..
        } if act == "list" => {
            let sessions = list_managed_sessions().unwrap_or_default();
            let session_ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
            let session_details: Vec<serde_json::Value> = sessions
                .iter()
                .map(|session| {
                    serde_json::json!({
                        "id": session.id,
                        "path": session.path.display().to_string(),
                        "message_count": session.message_count,
                        "updated_at_ms": session.updated_at_ms,
                        "lifecycle": session.lifecycle.json_value(),
                    })
                })
                .collect();
            let active_id = session.session_id.clone();
            let text = render_session_list(&active_id).unwrap_or_else(|e| format!("error: {e}"));
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(text),
                json: Some(serde_json::json!({
                    "kind": "session_list",
                    "sessions": session_ids,
                    "session_details": session_details,
                    "active": active_id,
                })),
            })
        }
        SlashCommand::Bughunter { .. }
        | SlashCommand::Commit { .. }
        | SlashCommand::Pr { .. }
        | SlashCommand::Issue { .. }
        | SlashCommand::Ultraplan { .. }
        | SlashCommand::Teleport { .. }
        | SlashCommand::DebugToolCall { .. }
        | SlashCommand::Resume { .. }
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Auth { .. }
        | SlashCommand::Session { .. }
        | SlashCommand::Plugins { .. }
        | SlashCommand::Login
        | SlashCommand::Logout
        | SlashCommand::Vim
        | SlashCommand::Upgrade
        | SlashCommand::Share
        | SlashCommand::Feedback
        | SlashCommand::Files
        | SlashCommand::Fast
        | SlashCommand::Exit
        | SlashCommand::Summary
        | SlashCommand::Desktop
        | SlashCommand::Brief
        | SlashCommand::Advisor
        | SlashCommand::Stickers
        | SlashCommand::Insights
        | SlashCommand::Thinkback
        | SlashCommand::ReleaseNotes
        | SlashCommand::SecurityReview
        | SlashCommand::Keybindings
        | SlashCommand::PrivacySettings
        | SlashCommand::Plan { .. }
        | SlashCommand::Review { .. }
        | SlashCommand::Tasks { .. }
        | SlashCommand::Theme { .. }
        | SlashCommand::Voice { .. }
        | SlashCommand::Usage { .. }
        | SlashCommand::Rename { .. }
        | SlashCommand::Copy { .. }
        | SlashCommand::Hooks { .. }
        | SlashCommand::Context { .. }
        | SlashCommand::Color { .. }
        | SlashCommand::Effort { .. }
        | SlashCommand::Branch { .. }
        | SlashCommand::Rewind { .. }
        | SlashCommand::Ide { .. }
        | SlashCommand::Tag { .. }
        | SlashCommand::OutputStyle { .. }
        | SlashCommand::AddDir { .. } => Err("unsupported resumed slash command".into()),
    }
}

fn run_stale_base_preflight(flag_value: Option<&str>) {
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    let source = resolve_expected_base(flag_value, &cwd);
    let state = check_base_commit(&cwd, source.as_ref());
    if let Some(warning) = format_stale_base_warning(&state) {
        eprintln!("{warning}");
    }
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
    auth_mode: Option<AuthMode>,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::Text)?;
    run_stale_base_preflight(base_commit.as_deref());
    let resolved_model = resolve_repl_model(model);
    let mut cli = LiveCli::new(
        resolved_model,
        true,
        allowed_tools,
        permission_mode,
        auth_mode,
    )?;
    cli.set_reasoning_effort(reasoning_effort);
    let mut editor =
        input::LineEditor::new("❯ ", cli.repl_completion_candidates().unwrap_or_default());
    println!("{}", cli.startup_banner());

    // Track session metrics for session_ended event
    let session_start = Instant::now();

    loop {
        editor.set_completions(cli.repl_completion_candidates().unwrap_or_default());
        let term_width = crossterm::terminal::size()
            .map(|(cols, _)| cols as usize)
            .unwrap_or(80);
        let separator = format!("\x1b[2m{}\x1b[0m", "─".repeat(term_width));
        let footer = "  \x1b[2m/help · /status · Tab for /commands\x1b[0m";
        // Print the entire input chrome block: top sep, prompt placeholder,
        // bottom sep, footer.  Then move the cursor back to the prompt line
        // so read_line() renders there — the user sees all four elements at once.
        println!("{separator}");
        println!();
        println!("{separator}");
        print!("{footer}");
        print!("\x1b[2F\x1b[2K"); // cursor up 2 lines, clear prompt placeholder
        std::io::Write::flush(&mut std::io::stdout())?;
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                // Clear pre-printed bottom sep + footer
                print!("\x1b[J");
                // Replace prompt line with a gray-background echo of the user
                // input.  Multi-line input renders one styled row per line so
                // the echo matches what the user actually typed (#182 item 3).
                let trimmed = input.trim().to_string();
                let (echo_block, line_count) = format_input_echo(&trimmed, term_width);
                // Move the cursor up past every row rustyline rendered for the
                // input and clear each one, then write the echo block in their
                // place.
                for _ in 0..line_count {
                    print!("\x1b[1F\x1b[2K");
                }
                print!("{echo_block}");
                println!();
                println!("{separator}");
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                match SlashCommand::parse(&trimmed) {
                    Ok(Some(command)) => {
                        match cli.handle_repl_command(command) {
                            Ok(true) => {
                                if let Err(e) = cli.persist_session() {
                                    eprintln!("\x1b[31m{e}\x1b[0m");
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                eprintln!("\x1b[31m{e}\x1b[0m");
                            }
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("\x1b[31m{error}\x1b[0m");
                        continue;
                    }
                }
                // Bare-word skill dispatch: if the first token of the input
                // matches a known skill name, invoke it as `/skills <input>`
                // rather than forwarding raw text to the LLM (ROADMAP #36).
                let cwd = std::env::current_dir().unwrap_or_default();
                if let Some(prompt) = try_resolve_bare_skill_prompt_with_plugins(
                    &cwd,
                    &trimmed,
                    Some(cli.runtime.plugin_load_outcome()),
                ) {
                    editor.push_history(input);
                    cli.record_prompt_history(&trimmed);
                    if let Err(e) = cli.run_turn(&prompt) {
                        eprintln!("\x1b[31m{e}\x1b[0m");
                    }
                    continue;
                }
                editor.push_history(input);
                cli.record_prompt_history(&trimmed);
                if let Err(e) = cli.run_turn(&trimmed) {
                    eprintln!("\x1b[31m{e}\x1b[0m");
                }
            }
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }

    // Record token usage and session ended event
    let duration_ms = session_start.elapsed().as_millis() as u64;
    let usage = cli.runtime.usage().cumulative_usage();
    let total_turns = cli.runtime.usage().turns();
    if let Some(tracer) = cli.session_tracer() {
        tracer.record_usage(
            "session_summary".to_string(),
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
        tracer.record_session_ended(
            total_turns,
            usage.input_tokens as u64,
            usage.output_tokens as u64,
            duration_ms,
        );
    }

    Ok(())
}

struct LiveCli {
    config: RuntimeConfig,
    runtime: BuiltRuntime,
    session: SessionHandle,
    prompt_history: Vec<PromptHistoryEntry>,
    /// Tool-use ids already restored by `/undo`. Used to make repeated
    /// `/undo` calls step further back rather than re-undoing the same edit.
    undone_tool_use_ids: std::collections::HashSet<String>,
    /// Shared tokio runtime used to drive async `run_turn` calls.
    tokio_runtime: tokio::runtime::Runtime,
}

pub(crate) struct RuntimePluginState {
    pub(crate) feature_config: runtime::RuntimeFeatureConfig,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) plugin_registry: PluginRegistry,
    pub(crate) plugin_load_outcome: PluginLoadOutcome,
    pub(crate) mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

/// Groups the non-session parameters threaded through the `build_runtime*`
/// call chain so that adding a new knob only touches one struct instead of
/// 3-4 function signatures and 10+ call sites.
#[derive(Clone)]
struct RuntimeConfig {
    model: String,
    system_prompt: SystemPrompt,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
    auth_mode: AuthMode,
    sudocode_config: api::SudoCodeConfig,
}

struct BuiltRuntime {
    runtime: Option<ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>>,
    plugin_registry: PluginRegistry,
    plugin_load_outcome: PluginLoadOutcome,
    plugins_active: bool,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    mcp_active: bool,
}

impl BuiltRuntime {
    fn new(
        runtime: ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>,
        plugin_registry: PluginRegistry,
        plugin_load_outcome: PluginLoadOutcome,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            plugin_registry,
            plugin_load_outcome,
            plugins_active: true,
            mcp_state,
            mcp_active: true,
        }
    }

    fn with_hook_abort_signal(mut self, hook_abort_signal: runtime::HookAbortSignal) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing hook abort signal");
        self.runtime = Some(runtime.with_hook_abort_signal(hook_abort_signal));
        self
    }

    fn with_session_known_date(mut self, date: impl Into<String>) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before overriding session known date");
        self.runtime = Some(runtime.with_session_known_date(date));
        self
    }

    /// Set the trace ID for the next request.
    fn set_trace_id(&mut self, trace_id: impl Into<String>) {
        if let Some(ref mut runtime) = self.runtime {
            runtime.set_trace_id(trace_id);
        }
    }

    fn plugin_load_outcome(&self) -> &PluginLoadOutcome {
        &self.plugin_load_outcome
    }

    fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.plugins_active {
            self.plugin_registry.shutdown()?;
            self.plugins_active = false;
        }
        Ok(())
    }

    fn shutdown_mcp(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp_active {
            if let Some(mcp_state) = &self.mcp_state {
                mcp_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shutdown()?;
            }
            self.mcp_active = false;
        }
        Ok(())
    }

    /// Returns a reference to the session tracer, if available.
    fn session_tracer(&self) -> Option<&telemetry::SessionTracer> {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
            .api_client()
            .session_tracer()
    }
}

impl Deref for BuiltRuntime {
    type Target = ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>;

    fn deref(&self) -> &Self::Target {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl DerefMut for BuiltRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.runtime
            .as_mut()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl Drop for BuiltRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_mcp();
        let _ = self.shutdown_plugins();
    }
}

struct AcpCliSession {
    cwd: PathBuf,
    handle: SessionHandle,
    runtime: BuiltRuntime,
    abort_signal: runtime::HookAbortSignal,
    /// Session start time for duration tracking.
    started_at: Instant,
}

struct AcpCliAgent {
    model: String,
    model_flag_raw: Option<String>,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode_override: Option<PermissionMode>,
    reasoning_effort: Option<String>,
    auth_mode: Option<AuthMode>,
    sessions: HashMap<String, AcpCliSession>,
    tokio_runtime: tokio::runtime::Runtime,
}

impl AcpCliAgent {
    fn new(
        model: String,
        model_flag_raw: Option<String>,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode_override: Option<PermissionMode>,
        reasoning_effort: Option<String>,
        auth_mode: Option<AuthMode>,
    ) -> Self {
        Self {
            model,
            model_flag_raw,
            allowed_tools,
            permission_mode_override,
            reasoning_effort,
            auth_mode,
            sessions: HashMap::new(),
            tokio_runtime: tokio::runtime::Runtime::new()
                .expect("failed to create tokio runtime for ACP agent"),
        }
    }

    fn build_session(&self, cwd: &Path) -> Result<AcpCliSession, AcpError> {
        let cwd = canonical_session_cwd(cwd)?;
        let model = self.resolve_model_for_cwd(&cwd)?;
        let permission_mode = self.resolve_permission_mode_for_cwd(&cwd)?;
        let system_prompt = build_system_prompt_for(&cwd, &model).map_err(|error| {
            AcpError::internal(format!("failed to build system prompt: {error}"))
        })?;
        let session_state = new_cli_session_for(&cwd)
            .map_err(|error| AcpError::internal(format!("failed to create session: {error}")))?;
        let handle = create_managed_session_handle_for(&cwd, &session_state.session_id).map_err(
            |error| AcpError::internal(format!("failed to create session handle: {error}")),
        )?;
        let mut runtime = build_runtime_for_cwd(
            &cwd,
            session_state.with_persistence_path(handle.path.clone()),
            &handle.id,
            {
                let sudocode_config =
                    require_sudocode_config_for_cwd(&cwd).map_err(AcpError::internal)?;
                let auth_mode = resolve_auth_mode(&model, self.auth_mode, &sudocode_config)
                    .map_err(|e| AcpError::internal(format!("failed to resolve auth mode: {e}")))?;
                RuntimeConfig {
                    model: model.clone(),
                    system_prompt,
                    enable_tools: true,
                    emit_output: false,
                    allowed_tools: self.allowed_tools.clone(),
                    permission_mode,
                    progress_reporter: None,
                    auth_mode,
                    sudocode_config,
                }
            },
        )
        .map_err(|error| AcpError::internal(format!("failed to build runtime: {error}")))?;
        let abort_signal = runtime::HookAbortSignal::new();
        runtime = runtime.with_hook_abort_signal(abort_signal.clone());
        if let Some(rt) = runtime.runtime.as_mut() {
            rt.api_client_mut()
                .set_reasoning_effort(self.reasoning_effort.clone());
        }
        runtime
            .session()
            .save_to_path(&handle.path)
            .map_err(|error| AcpError::internal(format!("failed to persist session: {error}")))?;

        // Record session started event
        let is_child_process = std::env::var("SUDOWORK_CHILD_PROCESS").is_ok();
        let mode = if is_child_process {
            "child"
        } else {
            "standalone"
        };
        if let Some(tracer) = runtime.session_tracer() {
            tracer.record_session_started(VERSION, cwd.to_string_lossy(), mode, &model);
        }

        Ok(AcpCliSession {
            cwd,
            handle,
            runtime,
            abort_signal,
            started_at: Instant::now(),
        })
    }

    fn resolve_model_for_cwd(&self, cwd: &Path) -> Result<String, AcpError> {
        if self.model_flag_raw.is_some() {
            return Ok(self.model.clone());
        }
        let _guard = ScopedCurrentDir::change_to(cwd)
            .map_err(|error| AcpError::internal(format!("failed to enter cwd: {error}")))?;
        Ok(resolve_repl_model(self.model.clone()))
    }

    fn resolve_permission_mode_for_cwd(&self, cwd: &Path) -> Result<PermissionMode, AcpError> {
        if let Some(mode) = self.permission_mode_override {
            return Ok(mode);
        }
        let _guard = ScopedCurrentDir::change_to(cwd)
            .map_err(|error| AcpError::internal(format!("failed to enter cwd: {error}")))?;
        Ok(default_permission_mode())
    }
}

impl AcpCliAgent {
    fn handle_acp_model_switch(
        &mut self,
        session_id: &str,
        model: Option<String>,
    ) -> Result<String, AcpError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| AcpError::invalid_params(format!("unknown sessionId: {session_id}")))?;

        let Some(new_model) = model else {
            return Ok(format_model_report(
                &self.model,
                session.runtime.session().messages.len(),
                UsageTracker::from_session(session.runtime.session()).turns(),
            ));
        };

        let resolved = resolve_model_alias_with_config(&new_model);
        if resolved == self.model {
            let session = self.sessions.get(session_id).unwrap();
            return Ok(format_model_report(
                &self.model,
                session.runtime.session().messages.len(),
                UsageTracker::from_session(session.runtime.session()).turns(),
            ));
        }

        let previous = self.model.clone();
        let session = self.sessions.get(session_id).unwrap();
        let message_count = session.runtime.session().messages.len();
        let mut cloned_session = session.runtime.session().clone();
        // Keep the session's own model in sync with the switch. `build_runtime_with_plugin_state`
        // only fills `session.model` when it is None (correct for a brand-new session), so without
        // this the resumed/switched session keeps its OLD model — which then drives the wrong
        // context-window in the pre-turn auto-compaction and can wedge the session on overflow.
        cloned_session.model = Some(resolved.clone());
        let cwd = session.cwd.clone();
        let handle_id = session.handle.id.clone();

        let sudocode_config = load_sudocode_config_for_cwd(&cwd);
        let permission_mode = self.resolve_permission_mode_for_cwd(&cwd)?;
        let auth_mode = resolve_auth_mode(&resolved, self.auth_mode, &sudocode_config)
            .map_err(|e| AcpError::internal(format!("failed to resolve auth mode: {e}")))?;
        let system_prompt = build_system_prompt_for(&cwd, &resolved)
            .map_err(|e| AcpError::internal(format!("failed to build system prompt: {e}")))?;
        let runtime = build_runtime_for_cwd(
            &cwd,
            cloned_session,
            &handle_id,
            RuntimeConfig {
                model: resolved.clone(),
                system_prompt,
                enable_tools: true,
                emit_output: false,
                allowed_tools: self.allowed_tools.clone(),
                permission_mode,
                progress_reporter: None,
                auth_mode,
                sudocode_config,
            },
        )
        .map_err(|e| AcpError::internal(e.to_string()))?;

        let session = self.sessions.get_mut(session_id).unwrap();
        session.runtime = runtime;
        self.model.clone_from(&resolved);

        Ok(format_model_switch_report(
            &previous,
            &resolved,
            message_count,
        ))
    }
}

struct ScopedCurrentDir {
    previous: PathBuf,
}

impl ScopedCurrentDir {
    fn change_to(cwd: &Path) -> io::Result<Self> {
        let previous = env::current_dir()?;
        env::set_current_dir(cwd)?;
        Ok(Self { previous })
    }
}

impl Drop for ScopedCurrentDir {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.previous);
    }
}

fn canonical_session_cwd(cwd: &Path) -> Result<PathBuf, AcpError> {
    let canonical = fs::canonicalize(cwd).map_err(|error| {
        AcpError::invalid_params(format!("params.cwd is not accessible: {error}"))
    })?;
    if !canonical.is_dir() {
        return Err(AcpError::invalid_params("params.cwd must be a directory"));
    }
    Ok(canonical)
}

fn run_acp_server(
    model: String,
    model_flag_raw: Option<String>,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode_override: Option<PermissionMode>,
    reasoning_effort: Option<String>,
    auth_mode: Option<AuthMode>,
    ws_port: Option<u16>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Load model capabilities SSOT before serving so vision_capable /
    // per_model_image_cap see sudorouter's populated data (falls back to
    // bundled defaults if the cache file doesn't exist). Without this,
    // the ACP server would always use the bundled fallback and never
    // pick up documented text-only models — the wrong-model VLM route
    // would never fire in production. Missing this call cost ~40 min of
    // real-e2e debugging 2026-07-01.
    let config_home = runtime::default_config_home();
    runtime::model_capabilities::load(&config_home, &runtime::fs_backend::StdFsBackend);

    let config = runtime::acp_sdk_server::SdkAcpConfig {
        agent_version: VERSION.to_string(),
        model: model.clone(),
        model_flag_raw: model_flag_raw.clone(),
        permission_mode_override,
        reasoning_effort: reasoning_effort.clone(),
    };
    let delegate = Box::new(AcpSdkDelegate::new(
        model,
        model_flag_raw,
        allowed_tools,
        permission_mode_override,
        reasoning_effort,
        auth_mode,
    ));
    let rt = tokio::runtime::Runtime::new()?;
    if let Some(port) = ws_port {
        rt.block_on(runtime::acp_ws_server::run_acp_ws_server(
            config, delegate, port,
        ))
    } else {
        rt.block_on(runtime::acp_stdio_server::run_acp_stdio_server(
            config, delegate,
        ))
    }
}

/// Delegate implementation that bridges the SDK ACP server to the existing
/// CLI session/runtime machinery.
struct AcpSdkDelegate {
    inner: AcpCliAgent,
}

/// Route an image through a VLM (via sudorouter) and return a
/// `ContentBlock::Text` containing the description, or — if the VLM call
/// fails for any reason (creds missing, network error, bad response) —
/// fall back to a placeholder so the conversation still has *something*
/// to reference for that slot.
///
/// **Runtime-nesting fix (v2, 2026-07-01)**: push_images is a sync trait
/// method called from within the ACP server's async handler. Two earlier
/// attempts BOTH hung sudowork's real UI e2e (only surfaced by driving the
/// actual Electron app via ai-dev-browser, not the mocked Rust integration
/// test or the direct CLI path):
///
///   - v0 (`std::thread::scope` + fresh current_thread rt): hung.
///   - v1 (`block_in_place` + `Handle::current().block_on`): also hung —
///     the outer ACP-server runtime's worker pool starved once we blocked
///     one worker on the VLM future while reqwest needed workers too.
///
/// v2 uses **a dedicated OS thread** with **its own current_thread
/// runtime**. Fully decouples the VLM leg from the ACP runtime's task
/// pool, so no nesting/starvation is possible regardless of which context
/// push_images is called from. Trade-off is one extra OS-thread spawn per
/// image (still cheap next to the VLM HTTP round-trip).
fn vlm_describe_block_or_placeholder(
    image_b64: &str,
    mime_type: &str,
    index: usize,
    sudorouter_creds: Option<&(String, String)>,
) -> runtime::ContentBlock {
    let human_idx = index + 1;
    let Some((base_url, api_key)) = sudorouter_creds else {
        eprintln!(
            "[push_images] image #{human_idx} — no sudorouter creds, falling back to placeholder"
        );
        return runtime::ContentBlock::Text {
            text: format!(
                "[Image #{human_idx} could not be sent (sudorouter not configured) — please configure proxy.sudorouter or use a vision-capable model.]"
            ),
        };
    };
    eprintln!(
        "[push_images] image #{human_idx} — VLM-route start, {} b64 bytes",
        image_b64.len()
    );

    let base_url = base_url.clone();
    let api_key = api_key.clone();
    let image_b64 = image_b64.to_string();
    let mime_type = mime_type.to_string();

    let spawn_result = std::thread::Builder::new()
        .name(format!("vlm-describe-{human_idx}"))
        .spawn(move || -> Result<String, String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("failed to build VLM runtime: {e}"))?;
            rt.block_on(vlm_describe::describe_image_via_vlm(
                &base_url,
                &api_key,
                vlm_describe::DEFAULT_VISION_MODEL,
                &image_b64,
                &mime_type,
            ))
            .map_err(|e| e.to_string())
        });

    let result: Result<String, String> = match spawn_result {
        Ok(join) => match join.join() {
            Ok(inner) => inner,
            Err(_) => Err("VLM worker thread panicked".to_string()),
        },
        Err(e) => Err(format!("failed to spawn VLM worker thread: {e}")),
    };

    match result {
        Ok(description) => {
            eprintln!(
                "[push_images] image #{human_idx} — VLM done, {} desc chars",
                description.len()
            );
            runtime::ContentBlock::Text {
                text: format!("[Image #{human_idx}: {description}]"),
            }
        }
        Err(e) => {
            eprintln!("[push_images] image #{human_idx} — VLM describe failed: {e}");
            runtime::ContentBlock::Text {
                text: format!(
                    "[Image #{human_idx} could not be described automatically ({e}) — please retype your question with the image's key contents in text.]"
                ),
            }
        }
    }
}

impl AcpSdkDelegate {
    fn new(
        model: String,
        model_flag_raw: Option<String>,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode_override: Option<PermissionMode>,
        reasoning_effort: Option<String>,
        auth_mode: Option<AuthMode>,
    ) -> Self {
        Self {
            inner: AcpCliAgent::new(
                model,
                model_flag_raw,
                allowed_tools,
                permission_mode_override,
                reasoning_effort,
                auth_mode,
            ),
        }
    }
}

impl runtime::acp_sdk_server::SdkAcpDelegate for AcpSdkDelegate {
    fn new_session(
        &mut self,
        cwd: PathBuf,
    ) -> Result<(String, PathBuf, runtime::HookAbortSignal), runtime::AcpError> {
        let session = self.inner.build_session(&cwd)?;
        let session_id = session.handle.id.clone();
        let session_cwd = session.cwd.clone();
        let abort_signal = session.abort_signal.clone();
        self.inner.sessions.insert(session_id.clone(), session);
        Ok((session_id, session_cwd, abort_signal))
    }

    fn run_prompt(
        &mut self,
        session_id: &str,
        prompt: String,
        observer: &mut runtime::acp_sdk_server::SdkSessionObserver,
        trace_id: Option<&str>,
    ) -> Result<
        (
            runtime::acp_sdk_server::AcpStopReason,
            Option<runtime::acp_sdk_server::PromptUsage>,
        ),
        runtime::AcpError,
    > {
        self.run_prompt_impl(session_id, prompt, observer, None, trace_id)
    }

    fn run_prompt_with_prompter(
        &mut self,
        session_id: &str,
        prompt: String,
        observer: &mut runtime::acp_sdk_server::SdkSessionObserver,
        prompter: &mut dyn runtime::PermissionPrompter,
        trace_id: Option<&str>,
    ) -> Result<
        (
            runtime::acp_sdk_server::AcpStopReason,
            Option<runtime::acp_sdk_server::PromptUsage>,
        ),
        runtime::AcpError,
    > {
        self.run_prompt_impl(session_id, prompt, observer, Some(prompter), trace_id)
    }

    fn set_question_prompter(
        &mut self,
        session_id: &str,
        prompter: Box<dyn runtime::QuestionPrompter>,
    ) -> Result<(), runtime::AcpError> {
        let session = self.inner.sessions.get_mut(session_id).ok_or_else(|| {
            runtime::AcpError::invalid_params(format!("unknown sessionId: {session_id}"))
        })?;
        session
            .runtime
            .tool_executor_mut()
            .set_question_prompter(prompter);
        Ok(())
    }

    fn handle_slash_command(
        &mut self,
        session_id: &str,
        input: &str,
        observer: &mut runtime::acp_sdk_server::SdkSessionObserver,
    ) -> Result<(), runtime::AcpError> {
        use runtime::RuntimeObserver as _;
        let Ok(Some(command)) = SlashCommand::parse(input) else {
            observer.on_text_delta(&format!(
                "Unknown slash command: `{input}`. Type `/help` for available commands."
            ));
            return Ok(());
        };

        let response = match &command {
            SlashCommand::Model { model } => {
                self.inner.handle_acp_model_switch(session_id, model.clone())?
            }
            SlashCommand::Help => render_repl_help(),
            SlashCommand::Status => {
                let session = self.inner.sessions.get(session_id).ok_or_else(|| {
                    runtime::AcpError::invalid_params(format!(
                        "unknown sessionId: {session_id}"
                    ))
                })?;
                let _guard = ScopedCurrentDir::change_to(&session.cwd)
                    .map_err(|e| runtime::AcpError::internal(e.to_string()))?;
                let tracker = UsageTracker::from_session(session.runtime.session());
                format_status_report(
                    &self.inner.model,
                    StatusUsage {
                        message_count: session.runtime.session().messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: tracker.cumulative_usage(),
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &status_context(Some(&session.handle.path))
                        .map_err(|e| runtime::AcpError::internal(e.to_string()))?,
                    None,
                )
            }
            SlashCommand::Cost => {
                let session = self.inner.sessions.get(session_id).ok_or_else(|| {
                    runtime::AcpError::invalid_params(format!(
                        "unknown sessionId: {session_id}"
                    ))
                })?;
                let usage = UsageTracker::from_session(session.runtime.session())
                    .cumulative_usage();
                format!(
                    "Token usage: {} input, {} output, {} cache-create, {} cache-read",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                )
            }
            SlashCommand::Config { section } => render_config_report(section.as_deref())
                .map_err(|e| runtime::AcpError::internal(e.to_string()))?,
            SlashCommand::Diff => {
                let output = std::process::Command::new("git")
                    .args(["diff", "--cached", "--no-color"])
                    .output()
                    .map_err(|e| runtime::AcpError::internal(e.to_string()))?;
                let cached = String::from_utf8_lossy(&output.stdout);
                let output2 = std::process::Command::new("git")
                    .args(["diff", "--no-color"])
                    .output()
                    .map_err(|e| runtime::AcpError::internal(e.to_string()))?;
                let unstaged = String::from_utf8_lossy(&output2.stdout);
                if cached.is_empty() && unstaged.is_empty() {
                    "No changes detected.".to_string()
                } else {
                    format!(
                        "{}{}",
                        if cached.is_empty() {
                            String::new()
                        } else {
                            format!("**Staged:**\n```diff\n{cached}```\n\n")
                        },
                        if unstaged.is_empty() {
                            String::new()
                        } else {
                            format!("**Unstaged:**\n```diff\n{unstaged}```")
                        }
                    )
                }
            }
            SlashCommand::Doctor => render_doctor_report()
                .map(|report| report.render())
                .map_err(|e| runtime::AcpError::internal(e.to_string()))?,
            _ => format!(
                "`{}` is not supported in ACP mode. Available: /model, /status, /cost, /config, /diff, /doctor, /help",
                input.split_whitespace().next().unwrap_or(input)
            ),
        };

        observer.on_text_delta(&response);
        Ok(())
    }

    fn list_sessions(&self) -> Vec<(String, PathBuf)> {
        self.inner
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.cwd.clone()))
            .collect()
    }

    fn close_session(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.inner.sessions.remove(session_id) {
            // Record token usage and session ended event
            let duration_ms = session.started_at.elapsed().as_millis() as u64;
            let usage = session.runtime.usage().cumulative_usage();
            let total_turns = session.runtime.usage().turns();
            if let Some(tracer) = session.runtime.session_tracer() {
                tracer.record_usage(
                    "session_summary".to_string(),
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                );
                tracer.record_session_ended(
                    total_turns,
                    usage.input_tokens as u64,
                    usage.output_tokens as u64,
                    duration_ms,
                );
            }
            true
        } else {
            false
        }
    }

    fn set_model(&mut self, session_id: &str, model_id: &str) -> Result<String, runtime::AcpError> {
        self.inner
            .handle_acp_model_switch(session_id, Some(model_id.to_string()))
    }

    fn get_model_info(&self) -> (String, Vec<String>) {
        let config = load_sudocode_config_for_current_dir();
        let mut models: Vec<String> = config.models.keys().cloned().collect();
        // Ensure the current model is always present.
        if !models.contains(&self.inner.model) {
            models.insert(0, self.inner.model.clone());
        }
        (self.inner.model.clone(), models)
    }

    fn set_permission_mode(
        &mut self,
        session_id: &str,
        mode: PermissionMode,
    ) -> Result<(), runtime::AcpError> {
        let session = self.inner.sessions.get_mut(session_id).ok_or_else(|| {
            runtime::AcpError::invalid_params(format!("unknown sessionId: {session_id}"))
        })?;
        if let Some(rt) = session.runtime.runtime.as_mut() {
            rt.permission_policy_mut().set_active_mode(mode);
        }
        Ok(())
    }

    fn push_images(
        &mut self,
        session_id: &str,
        images: &[(String, String)],
    ) -> Result<(), runtime::AcpError> {
        eprintln!(
            "[push_images] entered — session={session_id}, {} images",
            images.len()
        );
        // Resolve everything that needs runtime-level state BEFORE taking the
        // mutable session borrow: the active model + sudorouter creds.
        let active_model = self.inner.model.clone();
        let active_model_is_vision_capable =
            runtime::model_capabilities::vision_capable(&active_model);
        eprintln!(
            "[push_images] active_model={active_model:?} vision_capable={active_model_is_vision_capable}"
        );
        let sudocode_config = load_sudocode_config_for_current_dir();
        let sudorouter_creds = extract_sudorouter_credentials(&sudocode_config);
        eprintln!(
            "[push_images] sudorouter_creds_present={}",
            sudorouter_creds.is_some()
        );

        // The push_images path now has THREE failure modes to recover from —
        // each substitutes ContentBlock::Image → ContentBlock::Text so the
        // conversation continues, the model gets something useful, and the
        // user never sees a "model doesn't support images" / "image too large"
        // tip leak through. Design:
        // docs/design/image-handling-non-user-facing.html (Decision 2).
        //
        // 1. Active model is text-only → route via VLM (gemini-2.5-flash by
        //    default), splice description text. Checked BEFORE preflight: no
        //    point spending CPU on downsample if bytes aren't going natively.
        // 2. preflight returns ImageTooLargeError (pathological input where
        //    even 400×400 @ q30 exceeds the 5 MB cap) → route via VLM as
        //    well; REPLACES the old static "[Image #N too large]" placeholder.
        // 3. Generic decode failure → fall through with original bytes; never
        //    silently DROP a presumed-valid image.
        let mut blocks: Vec<runtime::ContentBlock> = Vec::with_capacity(images.len());
        for (index, (data, mime_type)) in images.iter().enumerate() {
            let block = if !active_model_is_vision_capable {
                vlm_describe_block_or_placeholder(data, mime_type, index, sudorouter_creds.as_ref())
            } else {
                match runtime::image_registry::preflight_base64(data, mime_type) {
                    Ok((final_data, final_mime)) => runtime::ContentBlock::Image {
                        data: final_data,
                        mime_type: final_mime,
                    },
                    Err(err) if runtime::image_registry::is_image_too_large(&err) => {
                        vlm_describe_block_or_placeholder(
                            data,
                            mime_type,
                            index,
                            sudorouter_creds.as_ref(),
                        )
                    }
                    Err(_) => runtime::ContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    },
                }
            };
            blocks.push(block);
        }

        // Single critical section: take the session mut and push all messages.
        let session = self.inner.sessions.get_mut(session_id).ok_or_else(|| {
            runtime::AcpError::invalid_params(format!("unknown sessionId: {session_id}"))
        })?;
        for block in blocks {
            let msg = runtime::ConversationMessage {
                role: runtime::MessageRole::User,
                blocks: vec![block],
                usage: None,
                model: None,
            };
            session
                .runtime
                .session_mut()
                .push_message(msg)
                .map_err(|e| runtime::AcpError::internal(e.to_string()))?;
        }
        Ok(())
    }

    fn load_session(
        &mut self,
        session_id: &str,
        cwd: PathBuf,
    ) -> Result<(String, PathBuf, runtime::HookAbortSignal), runtime::AcpError> {
        let cwd = canonical_session_cwd(&cwd)?;
        let _guard = ScopedCurrentDir::change_to(&cwd)
            .map_err(|e| runtime::AcpError::internal(format!("failed to enter cwd: {e}")))?;

        let (handle, session) = load_session_reference(session_id)
            .map_err(|e| runtime::AcpError::internal(format!("failed to load session: {e}")))?;

        let model = self.inner.resolve_model_for_cwd(&cwd)?;
        let permission_mode = self.inner.resolve_permission_mode_for_cwd(&cwd)?;
        let system_prompt = build_system_prompt_for(&cwd, &model).map_err(|e| {
            runtime::AcpError::internal(format!("failed to build system prompt: {e}"))
        })?;
        let sudocode_config =
            require_sudocode_config_for_cwd(&cwd).map_err(runtime::AcpError::internal)?;
        let auth_mode =
            resolve_auth_mode(&model, self.inner.auth_mode, &sudocode_config).map_err(|e| {
                runtime::AcpError::internal(format!("failed to resolve auth mode: {e}"))
            })?;

        let mut runtime = build_runtime_for_cwd(
            &cwd,
            session,
            &handle.id,
            RuntimeConfig {
                model,
                system_prompt,
                enable_tools: true,
                emit_output: false,
                allowed_tools: self.inner.allowed_tools.clone(),
                permission_mode,
                progress_reporter: None,
                auth_mode,
                sudocode_config,
            },
        )
        .map_err(|e| runtime::AcpError::internal(format!("failed to build runtime: {e}")))?;

        let abort_signal = runtime::HookAbortSignal::new();
        runtime = runtime.with_hook_abort_signal(abort_signal.clone());
        if let Some(rt) = runtime.runtime.as_mut() {
            rt.api_client_mut()
                .set_reasoning_effort(self.inner.reasoning_effort.clone());
        }

        let loaded_session_id = handle.id.clone();
        let signal = abort_signal.clone();
        self.inner.sessions.insert(
            loaded_session_id.clone(),
            AcpCliSession {
                cwd: cwd.clone(),
                handle,
                runtime,
                abort_signal,
                started_at: Instant::now(),
            },
        );
        Ok((loaded_session_id, cwd, signal))
    }
}

impl AcpSdkDelegate {
    fn run_prompt_impl(
        &mut self,
        session_id: &str,
        prompt: String,
        observer: &mut runtime::acp_sdk_server::SdkSessionObserver,
        prompter: Option<&mut dyn runtime::PermissionPrompter>,
        trace_id: Option<&str>,
    ) -> Result<
        (
            runtime::acp_sdk_server::AcpStopReason,
            Option<runtime::acp_sdk_server::PromptUsage>,
        ),
        runtime::AcpError,
    > {
        let session = self.inner.sessions.get_mut(session_id).ok_or_else(|| {
            runtime::AcpError::invalid_params(format!("unknown sessionId: {session_id}"))
        })?;
        // Reset abort signal for this new turn.
        session.abort_signal.reset();
        let _guard = ScopedCurrentDir::change_to(&session.cwd).map_err(|e| {
            runtime::AcpError::internal(format!("failed to enter session cwd: {e}"))
        })?;

        // Set trace_id on the runtime if provided
        if let Some(tid) = trace_id {
            session.runtime.set_trace_id(tid);
        }

        // Pre-send token estimation and auto-compact logic
        let model = session
            .runtime
            .session()
            .model
            .as_ref()
            .unwrap_or(&self.inner.model);
        // Context window comes from the model-capabilities SSOT file (per-model
        // entry, else the file's `default`). No hardcoded fallback here.
        let context_limit = runtime::model_capabilities::context_window_or_default(model) as usize;

        // Estimate current session tokens
        let estimated_tokens = estimate_session_tokens(session.runtime.session());
        let threshold = (context_limit as f64 * 0.85) as usize; // 85% threshold

        // If approaching limit, try auto-compact
        if estimated_tokens > threshold {
            // Check if we have enough messages to compact
            let message_count = session.runtime.session().messages.len();
            let can_compact = message_count > 4; // Need more than preserve_recent_messages

            if let Some(tracer) = session.runtime.session_tracer() {
                tracer.record("auto_compact_check", {
                    let mut attrs = Map::new();
                    attrs.insert(
                        "estimated_tokens".to_string(),
                        Value::Number(estimated_tokens.into()),
                    );
                    attrs.insert("threshold".to_string(), Value::Number(threshold.into()));
                    attrs.insert(
                        "context_limit".to_string(),
                        Value::Number(context_limit.into()),
                    );
                    attrs.insert(
                        "message_count".to_string(),
                        Value::Number(message_count.into()),
                    );
                    attrs.insert("can_compact".to_string(), Value::Bool(can_compact));
                    attrs
                });
            }

            if can_compact {
                // Perform compaction with aggressive settings for overflow scenario
                let compaction_config = CompactionConfig {
                    preserve_recent_messages: 2,
                    max_estimated_tokens: 0, // Force compaction
                };
                let result = compact_session(session.runtime.session(), compaction_config);
                if result.removed_message_count > 0 {
                    // Update session with compacted version
                    *session.runtime.session_mut() = result.compacted_session.clone();
                    // Persist the compacted state immediately. The end-of-turn save_to_path is
                    // skipped by the still-over-limit early return below (and by any later turn
                    // error), which would otherwise leave the on-disk JSONL holding the full
                    // uncompacted history — so the next resume reloads the pre-compaction session
                    // and overflows again. Best-effort: a persist hiccup must not abort the turn.
                    if let Err(persist_err) =
                        session.runtime.session().save_to_path(&session.handle.path)
                    {
                        if let Some(tracer) = session.runtime.session_tracer() {
                            tracer.record("auto_compact_persist_error", {
                                let mut attrs = Map::new();
                                attrs.insert(
                                    "error".to_string(),
                                    Value::String(persist_err.to_string()),
                                );
                                attrs
                            });
                        }
                    }
                    if let Some(tracer) = session.runtime.session_tracer() {
                        tracer.record("auto_compact_result", {
                            let mut attrs = Map::new();
                            attrs.insert(
                                "removed_messages".to_string(),
                                Value::Number(result.removed_message_count.into()),
                            );
                            attrs
                        });
                    }
                }

                // Re-estimate after compaction
                let new_estimated_tokens = estimate_session_tokens(session.runtime.session());

                // If still over limit after compaction, return friendly error
                if new_estimated_tokens > context_limit {
                    let user_message = format!(
                        "[context_window_exceeded][history_context_too_large] 对话内容过长，即使压缩后仍超出模型限制。\n\n\
                        当前估算: {} tokens\n\
                        模型限制: {} tokens\n\n\
                        建议解决方案：\n\
                        1. 开始新对话\n\
                        2. 使用支持更大上下文的模型\n\
                        3. 减少图片或大文本内容的发送",
                        new_estimated_tokens, context_limit
                    );
                    return Err(runtime::AcpError::internal(user_message));
                }
            } else {
                // No messages to compact, but request is too large
                let user_message = format!(
                    "[context_window_exceeded][single_request_too_large] 当前请求内容过大，超出模型处理限制。\n\n\
                    当前估算: {} tokens\n\
                    模型限制: {} tokens\n\n\
                    建议解决方案：\n\
                    1. 使用较小的图片（压缩或缩小图片尺寸）\n\
                    2. 简化输入内容\n\
                    3. 使用支持更大上下文的模型",
                    estimated_tokens, context_limit
                );
                return Err(runtime::AcpError::internal(user_message));
            }
        }
        // Run the turn and get the TurnSummary directly
        let turn_summary = self
            .inner
            .tokio_runtime
            .block_on(session.runtime.run_turn(prompt, prompter, Some(observer)))
            .map_err(|e| {
                if let Some(tracer) = session.runtime.session_tracer() {
                    tracer.record_prompt_error("runtime_error", e.to_string());
                }
                runtime::AcpError::internal(e.to_string())
            })?;
        // Use turn_usage for PromptUsage, session_usage for cumulative
        let per_turn_usage =
            (turn_summary.turn_usage.total_tokens() > 0).then_some(turn_summary.turn_usage);
        let cumulative_usage = turn_summary.session_usage;
        // Build PromptUsage if we have per-turn data, otherwise return None for usage
        let prompt_usage = per_turn_usage.map(|u| runtime::acp_sdk_server::PromptUsage {
            input_tokens: u64::from(u.input_tokens),
            output_tokens: u64::from(u.output_tokens),
            total_tokens: u64::from(u.total_tokens()),
            cache_read_tokens: Some(u64::from(u.cache_read_input_tokens)),
            cache_write_tokens: Some(u64::from(u.cache_creation_input_tokens)),
            context_window_tokens: Some(context_limit as u64),
            estimated_session_tokens: Some(
                estimate_session_tokens(session.runtime.session()) as u64
            ),
            cost_units: u.cost_units,
            cost_currency: u.cost_currency,
            cumulative_usage: Some(runtime::acp_sdk_server::CumulativeUsage {
                input_tokens: u64::from(cumulative_usage.input_tokens),
                output_tokens: u64::from(cumulative_usage.output_tokens),
                total_tokens: u64::from(cumulative_usage.total_tokens()),
                cached_read_tokens: Some(u64::from(cumulative_usage.cache_read_input_tokens)),
                cached_write_tokens: Some(u64::from(cumulative_usage.cache_creation_input_tokens)),
            }),
        });
        // Record token usage to telemetry log
        if let Some(tracer) = session.runtime.session_tracer() {
            // Record turn-level usage for this prompt
            tracer.record_usage_with_cost(
                "prompt_turn".to_string(),
                turn_summary.turn_usage.input_tokens,
                turn_summary.turn_usage.output_tokens,
                turn_summary.turn_usage.cache_creation_input_tokens,
                turn_summary.turn_usage.cache_read_input_tokens,
                turn_summary.turn_usage.cost_units,
                turn_summary
                    .turn_usage
                    .cost_currency
                    .map(runtime::UsageCostCurrency::as_str),
            );
            // Record cumulative session usage
            tracer.record_usage_with_cost(
                "session_summary".to_string(),
                cumulative_usage.input_tokens,
                cumulative_usage.output_tokens,
                cumulative_usage.cache_creation_input_tokens,
                cumulative_usage.cache_read_input_tokens,
                cumulative_usage.cost_units,
                cumulative_usage
                    .cost_currency
                    .map(runtime::UsageCostCurrency::as_str),
            );
        }
        session
            .runtime
            .session()
            .save_to_path(&session.handle.path)
            .map_err(|e| runtime::AcpError::internal(format!("failed to persist session: {e}")))?;
        Ok((
            runtime::acp_sdk_server::AcpStopReason::EndTurn,
            prompt_usage,
        ))
    }
}

struct HookAbortMonitor {
    stop_tx: Option<Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl HookAbortMonitor {
    fn spawn(abort_signal: runtime::HookAbortSignal) -> Self {
        Self::spawn_with_waiter(abort_signal, move |stop_rx, abort_signal| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };

            // Enable raw mode so crossterm can detect ESC keypresses
            // during streaming / tool execution (CC parity). The
            // rendering layer uses ANSI cursor control (not println!)
            // during a turn, so raw mode is safe here. Restored when
            // the monitor stops.
            let is_tty = io::stdin().is_terminal();
            let raw_enabled = is_tty && crossterm::terminal::enable_raw_mode().is_ok();

            runtime.block_on(async move {
                let esc_abort = abort_signal.clone();
                let wait_for_esc_or_stop = tokio::task::spawn_blocking(move || {
                    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
                    loop {
                        if stop_rx.try_recv().is_ok() {
                            return;
                        }
                        if !raw_enabled {
                            match stop_rx.recv_timeout(Duration::from_millis(50)) {
                                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                                Err(RecvTimeoutError::Timeout) => continue,
                            }
                        }
                        if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                            if let Ok(Event::Key(key)) = event::read() {
                                if key.kind != KeyEventKind::Press {
                                    continue;
                                }
                                let is_esc = key.code == KeyCode::Esc;
                                let is_ctrl_c = key.code == KeyCode::Char('c')
                                    && key.modifiers.contains(event::KeyModifiers::CONTROL);
                                if is_esc || is_ctrl_c {
                                    esc_abort.abort();
                                    return;
                                }
                            }
                        }
                    }
                });

                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            abort_signal.abort();
                        }
                    }
                    _ = wait_for_esc_or_stop => {}
                }
            });

            if raw_enabled {
                let _ = crossterm::terminal::disable_raw_mode();
            }
        })
    }

    fn spawn_with_waiter<F>(abort_signal: runtime::HookAbortSignal, wait_for_interrupt: F) -> Self
    where
        F: FnOnce(Receiver<()>, runtime::HookAbortSignal) + Send + 'static,
    {
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || wait_for_interrupt(stop_rx, abort_signal));

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }

    fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

/// Measure visible string width by stripping ANSI escape sequences.
fn strip_ansi_width(s: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            width += 1;
        }
    }
    width
}

impl LiveCli {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        auth_mode: Option<AuthMode>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let system_prompt = build_system_prompt(&model)?;
        let session_state = new_cli_session()?;
        let session = create_managed_session_handle(&session_state.session_id)?;
        let cwd = env::current_dir()?;
        let sudocode_config = require_sudocode_config_for_cwd(&cwd)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        // Load model capabilities SSOT (bundled fallback or cached from last refresh).
        let config_home = runtime::default_config_home();
        runtime::model_capabilities::load(&config_home, &runtime::fs_backend::StdFsBackend);

        let auth_mode = resolve_auth_mode(&model, auth_mode, &sudocode_config)?;
        tools::set_global_auth_mode(auth_mode);
        let config = RuntimeConfig {
            model,
            system_prompt,
            enable_tools,
            emit_output: true,
            allowed_tools,
            permission_mode,
            progress_reporter: None,
            auth_mode,
            sudocode_config: sudocode_config.clone(),
        };
        let runtime = build_runtime(
            session_state.with_persistence_path(session.path.clone()),
            &session.id,
            config.clone(),
        )?;
        let tokio_runtime = tokio::runtime::Runtime::new()?;

        // Fire-and-forget: refresh model capabilities from sudorouter if stale.
        if runtime::model_capabilities::is_stale(&config_home, &runtime::fs_backend::StdFsBackend) {
            if let Some((base_url, api_key)) = extract_sudorouter_credentials(&sudocode_config) {
                let ch = config_home.clone();
                tokio_runtime.spawn(async move {
                    let client = match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(10))
                        .build()
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let url = format!("{}/models", base_url.trim_end_matches('/'));
                    let resp = match client
                        .get(&url)
                        .header("Authorization", format!("Bearer {api_key}"))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => r,
                        _ => return,
                    };
                    let body: serde_json::Value = match resp.json().await {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    let entries = runtime::model_capabilities::parse_api_response(&body);
                    let _ = runtime::model_capabilities::merge_and_write(
                        &ch,
                        &runtime::fs_backend::StdFsBackend,
                        &entries,
                    );
                });
            }
        }

        let cli = Self {
            config,
            runtime,
            session,
            prompt_history: Vec::new(),
            undone_tool_use_ids: std::collections::HashSet::new(),
            tokio_runtime,
        };
        cli.persist_session()?;

        // Record session started event
        let is_child_process = std::env::var("SUDOWORK_CHILD_PROCESS").is_ok();
        let mode = if is_child_process {
            "child"
        } else {
            "standalone"
        };
        if let Some(tracer) = cli.runtime.session_tracer() {
            tracer.record_session_started(VERSION, cwd.to_string_lossy(), mode, &cli.config.model);
        }

        Ok(cli)
    }

    /// Returns a reference to the session tracer, if available.
    fn session_tracer(&self) -> Option<&telemetry::SessionTracer> {
        self.runtime.session_tracer()
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_reasoning_effort(effort);
        }
    }

    fn startup_banner(&self) -> String {
        let cwd = env::current_dir().map_or_else(
            |_| "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let status = status_context(None).ok();
        let git_branch = status
            .as_ref()
            .and_then(|context| context.git_branch.as_deref())
            .unwrap_or("unknown");
        let workspace = status.as_ref().map_or_else(
            || "unknown".to_string(),
            |context| context.git_summary.headline(),
        );
        let session_path = self.session.path.strip_prefix(Path::new(&cwd)).map_or_else(
            |_| self.session.path.display().to_string(),
            |path| path.display().to_string(),
        );

        // Auth mode line.
        let auth_mode_str = self.config.auth_mode.label().to_string();

        // Endpoint from config-driven resolution.
        let config = &self.config.sudocode_config;
        let endpoint = api::resolve_provider_from_config(
            &self.config.model,
            Some(self.config.auth_mode),
            config,
        )
        .ok()
        .map(|r| r.base_url)
        .unwrap_or_default();

        let logo = "\x1b[38;5;117m\
███████╗██╗   ██╗██████╗  ██████╗ \n\
██╔════╝██║   ██║██╔══██╗██╔═══██╗\n\
███████╗██║   ██║██║  ██║██║   ██║\n\
╚════██║██║   ██║██║  ██║██║   ██║\n\
███████║╚██████╔╝██████╔╝╚██████╔╝\n\
╚══════╝ ╚═════╝ ╚═════╝  ╚═════╝\x1b[0m \x1b[38;5;208mCode\x1b[0m";

        let lines = [
            format!("  \x1b[2mModel\x1b[0m            {}", self.config.model),
            format!("  \x1b[2mAuth mode\x1b[0m        {}", auth_mode_str),
            format!("  \x1b[2mEndpoint\x1b[0m         {}", endpoint),
            format!(
                "  \x1b[2mPermissions\x1b[0m      {}",
                self.config.permission_mode.as_str()
            ),
            format!("  \x1b[2mBranch\x1b[0m           {}", git_branch),
            format!("  \x1b[2mWorkspace\x1b[0m        {}", workspace),
            format!("  \x1b[2mDirectory\x1b[0m        {}", cwd),
            format!("  \x1b[2mSession\x1b[0m          {}", self.session.id),
            format!("  \x1b[2mAuto-save\x1b[0m        {}", session_path),
        ];

        let max_width = lines.iter().map(|l| strip_ansi_width(l)).max().unwrap_or(0);
        let box_width = max_width + 2; // 1 space padding on each side

        let grey = "\x1b[38;5;245m";
        let reset = "\x1b[0m";

        let top = format!("{grey}╭{}╮{reset}", "─".repeat(box_width));
        let bottom = format!("{grey}╰{}╯{reset}", "─".repeat(box_width));

        let boxed_lines: Vec<String> = lines
            .iter()
            .map(|line| {
                let visible_width = strip_ansi_width(line);
                let padding = max_width - visible_width;
                format!(
                    "{grey}│{reset} {}{} {grey}│{reset}",
                    line,
                    " ".repeat(padding)
                )
            })
            .collect();

        let hint = "  Type \x1b[1m/help\x1b[0m for commands · \x1b[1m/status\x1b[0m for live context · \x1b[2m/resume latest\x1b[0m jumps back to the newest session · \x1b[1m/diff\x1b[0m then \x1b[1m/commit\x1b[0m to ship · \x1b[2mTab\x1b[0m for /command completions";

        format!(
            "{}\n\n{}\n{}\n{}\n\n{}",
            logo,
            top,
            boxed_lines.join("\n"),
            bottom,
            hint,
        )
    }

    fn repl_completion_candidates(
        &self,
    ) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
        Ok(slash_command_completion_candidates_with_sessions(
            &self.config.model,
            Some(&self.session.id),
            list_managed_sessions()?
                .into_iter()
                .map(|session| session.id)
                .collect(),
        ))
    }

    fn prepare_turn_runtime(
        &mut self,
        emit_output: bool,
    ) -> Result<(BuiltRuntime, HookAbortMonitor), Box<dyn std::error::Error>> {
        let hook_abort_signal = runtime::HookAbortSignal::new();
        // `build_runtime` stamps `prompt_known_date` with today's local date,
        // which is correct only for a freshly-created runtime. The REPL
        // rebuilds the runtime on every turn, so without carrying this date
        // forward a long-running session that crosses midnight would have its
        // known date silently advanced to today on every turn — suppressing
        // the date-rollover reminder added in #128 (see issue #135).
        let inherited_known_date = self.runtime.prompt_known_date().map(str::to_string);
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        self.shutdown_runtime_resources()?;
        let mut runtime = build_runtime(
            session,
            &session_id,
            RuntimeConfig {
                emit_output,
                ..self.config.clone()
            },
        )?
        .with_hook_abort_signal(hook_abort_signal.clone());
        if let Some(known) = inherited_known_date {
            runtime = runtime.with_session_known_date(known);
        }
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);

        Ok((runtime, hook_abort_monitor))
    }

    fn shutdown_runtime_resources(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_mcp()?;
        self.runtime.shutdown_plugins()?;
        Ok(())
    }

    fn build_replacement_runtime(
        &mut self,
        session: Session,
        session_id: String,
        config: RuntimeConfig,
    ) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
        self.shutdown_runtime_resources()?;
        build_runtime(session, &session_id, config)
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.shutdown_runtime_resources()?;
        self.runtime = runtime;
        self.undone_tool_use_ids.clear();
        Ok(())
    }

    fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let turn_start = Instant::now();
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true)?;
        // Coordinator push: before starting this turn, drain any
        // `<task-notification>` blocks that background sub-agents
        // have deposited in `<workspace>/.sudocode-inbox/coordinator.jsonl`
        // since the previous turn, and prepend them to the user's
        // input. Under non-coordinator sessions the drain returns
        // empty so `input` is unchanged.
        let workspace_root = std::env::current_dir().unwrap_or_default();
        let notifications =
            runtime::coordinator_notification::drain(&workspace_root).unwrap_or_default();
        let prepended_input = if notifications.is_empty() {
            input.to_string()
        } else {
            let mut prefixed =
                runtime::coordinator_notification::format_drain_batch(&notifications);
            prefixed.push_str(input);
            prefixed
        };
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.start(
            "🦀 Thinking...",
            Some(self.config.model.as_str()),
            TerminalRenderer::new().color_theme(),
        );
        let pause_flag = spinner.pause_flag();
        let thinking_flag = spinner.thinking_flag();
        runtime
            .api_client_mut()
            .set_spinner_pause(pause_flag.clone());
        runtime.api_client_mut().set_spinner_thinking(thinking_flag);
        runtime.tool_executor_mut().set_spinner_pause(pause_flag);
        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let result = self.tokio_runtime.block_on(runtime.run_turn(
            prepended_input.as_str(),
            Some(&mut permission_prompter),
            None,
        ));
        hook_abort_monitor.stop();
        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                if summary.cancelled {
                    spinner.fail(
                        "⏹ Cancelled",
                        TerminalRenderer::new().color_theme(),
                        &mut stdout,
                    )?;
                } else {
                    spinner.clear(&mut stdout)?;
                    if let Some(event) = summary.auto_compaction {
                        println!(
                            "{}",
                            format_auto_compaction_notice(event.removed_message_count)
                        );
                    }
                    let elapsed = turn_start.elapsed();
                    if let Some(timeline) = format_tool_timeline(&summary.tool_results, elapsed) {
                        println!("{timeline}");
                    }
                    let usage = self.runtime.usage().current_turn_usage();
                    let turns = self.runtime.usage().turns();
                    let branch = env::current_dir()
                        .ok()
                        .and_then(|cwd| resolve_git_branch_for(&cwd));
                    println!(
                        "{}",
                        format_turn_status_line_with_branch(
                            &self.config.model,
                            turns,
                            &usage,
                            elapsed,
                            branch.as_deref(),
                        )
                    );
                }
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                runtime.shutdown_mcp()?;
                runtime.shutdown_plugins()?;
                spinner.fail(
                    "❌ Request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                Err(Box::new(error))
            }
        }
    }

    fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
        compact: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match output_format {
            CliOutputFormat::Json if compact => self.run_prompt_compact_json(input),
            CliOutputFormat::Text if compact => self.run_prompt_compact(input),
            CliOutputFormat::Text => self.run_turn(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
        }
    }

    fn run_prompt_compact(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let result = self.tokio_runtime.block_on(runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            None,
        ));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        let final_text = final_assistant_text(&summary);
        println!("{final_text}");
        Ok(())
    }

    fn run_prompt_compact_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let result = self.tokio_runtime.block_on(runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            None,
        ));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "compact": true,
                "model": self.config.model,
                "usage": {
                    "input_tokens": summary.turn_usage.input_tokens,
                    "output_tokens": summary.turn_usage.output_tokens,
                    "cache_creation_input_tokens": summary.turn_usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.turn_usage.cache_read_input_tokens,
                },
            })
        );
        Ok(())
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let result = self.tokio_runtime.block_on(runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            None,
        ));
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "model": self.config.model,
                "iterations": summary.iterations,
                "auto_compaction": summary.auto_compaction.map(|event| json!({
                    "removed_messages": event.removed_message_count,
                    "notice": format_auto_compaction_notice(event.removed_message_count),
                })),
                "tool_uses": collect_tool_uses(&summary),
                "tool_results": collect_tool_results(&summary),
                "prompt_cache_events": collect_prompt_cache_events(&summary),
                "usage": {
                    "input_tokens": summary.turn_usage.input_tokens,
                    "output_tokens": summary.turn_usage.output_tokens,
                    "cache_creation_input_tokens": summary.turn_usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.turn_usage.cache_read_input_tokens,
                },
                "estimated_cost": format_usd(
                    summary.turn_usage.estimate_cost_usd_with_pricing(
                        pricing_for_model(&self.config.model)
                            .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier)
                    ).total_cost_usd()
                )
            })
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                println!("{}", render_repl_help());
                false
            }
            SlashCommand::Status => {
                self.print_status();
                false
            }
            SlashCommand::Bughunter { scope } => {
                self.run_bughunter(scope.as_deref())?;
                false
            }
            SlashCommand::Commit => {
                self.run_commit(None)?;
                false
            }
            SlashCommand::Pr { context } => {
                self.run_pr(context.as_deref())?;
                false
            }
            SlashCommand::Issue { context } => {
                self.run_issue(context.as_deref())?;
                false
            }
            SlashCommand::Ultraplan { task } => {
                self.run_ultraplan(task.as_deref())?;
                false
            }
            SlashCommand::Teleport { target } => {
                Self::run_teleport(target.as_deref())?;
                false
            }
            SlashCommand::DebugToolCall => {
                self.run_debug_tool_call(None)?;
                false
            }
            SlashCommand::Sandbox => {
                Self::print_sandbox_status();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => self.set_model(model)?,
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Auth { mode } => self.set_auth(mode)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section } => {
                Self::print_config(section.as_deref())?;
                false
            }
            SlashCommand::Mcp { action, target } => {
                let args = match (action.as_deref(), target.as_deref()) {
                    (None, None) => None,
                    (Some(action), None) => Some(action.to_string()),
                    (Some(action), Some(target)) => Some(format!("{action} {target}")),
                    (None, Some(target)) => Some(target.to_string()),
                };
                Self::print_mcp(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Memory => {
                Self::edit_memory()?;
                false
            }
            SlashCommand::Init => {
                run_init(CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Diff => {
                Self::print_diff()?;
                false
            }
            SlashCommand::Undo => {
                self.handle_undo();
                false
            }
            SlashCommand::Version => {
                Self::print_version(CliOutputFormat::Text);
                false
            }
            SlashCommand::Export { path } => {
                self.export_session(path.as_deref())?;
                false
            }
            SlashCommand::Session { action, target } => {
                self.handle_session_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Plugins { action, target } => {
                self.handle_plugins_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Agents { args } => {
                Self::print_agents(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Skills { args } => {
                let cwd = env::current_dir()?;
                match resolve_skill_invocation_with_plugins(
                    &cwd,
                    args.as_deref(),
                    Some(self.runtime.plugin_load_outcome()),
                )
                .map_err(std::io::Error::other)?
                {
                    SkillSlashDispatch::Invoke(prompt) => self.run_turn(&prompt)?,
                    SkillSlashDispatch::Local => {
                        self.print_skills_with_plugins(args.as_deref(), CliOutputFormat::Text)?;
                    }
                }
                false
            }
            SlashCommand::Doctor => {
                println!("{}", render_doctor_report()?.render());
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Stats => {
                let usage = UsageTracker::from_session(self.runtime.session()).cumulative_usage();
                println!("{}", format_cost_report(usage));
                false
            }
            SlashCommand::Login
            | SlashCommand::Logout
            | SlashCommand::Vim
            | SlashCommand::Upgrade
            | SlashCommand::Share
            | SlashCommand::Feedback
            | SlashCommand::Files
            | SlashCommand::Fast
            | SlashCommand::Exit
            | SlashCommand::Summary
            | SlashCommand::Desktop
            | SlashCommand::Brief
            | SlashCommand::Advisor
            | SlashCommand::Stickers
            | SlashCommand::Insights
            | SlashCommand::Thinkback
            | SlashCommand::ReleaseNotes
            | SlashCommand::SecurityReview
            | SlashCommand::Keybindings
            | SlashCommand::PrivacySettings
            | SlashCommand::Plan { .. }
            | SlashCommand::Review { .. }
            | SlashCommand::Tasks { .. }
            | SlashCommand::Theme { .. }
            | SlashCommand::Voice { .. }
            | SlashCommand::Usage { .. }
            | SlashCommand::Rename { .. }
            | SlashCommand::Copy { .. }
            | SlashCommand::Hooks { .. }
            | SlashCommand::Context { .. }
            | SlashCommand::Color { .. }
            | SlashCommand::Effort { .. }
            | SlashCommand::Branch { .. }
            | SlashCommand::Rewind { .. }
            | SlashCommand::Ide { .. }
            | SlashCommand::Tag { .. }
            | SlashCommand::OutputStyle { .. }
            | SlashCommand::AddDir { .. } => {
                let cmd_name = command.slash_name();
                eprintln!("{cmd_name} is not yet implemented in this build.");
                false
            }
            SlashCommand::Unknown(name) => {
                eprintln!("{}", format_unknown_slash_command(&name));
                false
            }
        })
    }

    fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        let report = format_status_report(
            &self.config.model,
            StatusUsage {
                message_count: self.runtime.session().messages.len(),
                turns: self.runtime.usage().turns(),
                latest,
                cumulative,
                estimated_tokens: self.runtime.estimated_tokens(),
            },
            self.config.permission_mode.as_str(),
            &status_context(Some(&self.session.path)).expect("status context should load"),
            None, // #148: REPL /status doesn't carry flag provenance
        );
        print_with_pager(&report);
    }

    fn record_prompt_history(&mut self, prompt: &str) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(self.runtime.session().updated_at_ms, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = PromptHistoryEntry {
            timestamp_ms,
            text: prompt.to_string(),
        };
        self.prompt_history.push(entry);
        if let Err(error) = self.runtime.session_mut().push_prompt_entry(prompt) {
            eprintln!("warning: failed to persist prompt history: {error}");
        }
    }

    fn print_prompt_history(&self, count: Option<&str>) {
        let limit = match parse_history_count(count) {
            Ok(limit) => limit,
            Err(message) => {
                eprintln!("{message}");
                return;
            }
        };
        let session_entries = &self.runtime.session().prompt_history;
        let entries = if session_entries.is_empty() {
            if self.prompt_history.is_empty() {
                collect_session_prompt_history(self.runtime.session())
            } else {
                self.prompt_history
                    .iter()
                    .map(|entry| PromptHistoryEntry {
                        timestamp_ms: entry.timestamp_ms,
                        text: entry.text.clone(),
                    })
                    .collect()
            }
        } else {
            session_entries
                .iter()
                .map(|entry| PromptHistoryEntry {
                    timestamp_ms: entry.timestamp_ms,
                    text: entry.text.clone(),
                })
                .collect()
        };
        println!("{}", render_prompt_history_report(&entries, limit));
    }

    fn print_sandbox_status() {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        println!(
            "{}",
            format_sandbox_report(&resolve_sandbox_status(runtime_config.sandbox(), &cwd))
        );
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            let sudocode_config = load_sudocode_config_for_current_dir();
            let models: Vec<String> = sudocode_config.models.keys().cloned().collect();
            if models.is_empty() {
                println!("No models configured in sudocode.json");
                return Ok(false);
            }
            let selection = FuzzySelect::new()
                .with_prompt("Select model")
                .items(&models)
                .default(0)
                .interact_opt()?;
            return match selection {
                Some(idx) => self.set_model(Some(models[idx].clone())),
                None => Ok(false),
            };
        };

        let model = resolve_model_alias_with_config(&model);

        if model == self.config.model {
            println!(
                "{}",
                format_model_report(
                    &self.config.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.config.model.clone();
        let mut session = self.runtime.session().clone();
        // Keep the session's own model in sync with the switch (see handle_acp_model_switch): the
        // runtime builder only fills `session.model` when None, so otherwise it would retain the
        // old model and mis-compute the context window for auto-compaction.
        session.model = Some(model.clone());
        let session_id = self.session.id.clone();
        let message_count = session.messages.len();
        let runtime = self.build_replacement_runtime(
            session,
            session_id,
            RuntimeConfig {
                model: model.clone(),
                ..self.config.clone()
            },
        )?;
        self.replace_runtime(runtime)?;
        self.config.model.clone_from(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(true)
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(self.config.permission_mode.as_str())
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if normalized == self.config.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.config.permission_mode.as_str().to_string();
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        self.config.permission_mode = permission_mode_from_label(normalized);
        let runtime = self.build_replacement_runtime(session, session_id, self.config.clone())?;
        self.replace_runtime(runtime)?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    fn set_auth(&mut self, mode: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let current_str = self.config.auth_mode.as_str().to_string();

        let Some(mode) = mode else {
            println!("{}", format_auth_report(&current_str));
            return Ok(false);
        };

        let parsed = AuthMode::parse(&mode)?;

        if parsed.as_str() == current_str {
            println!("{}", format_auth_report(&current_str));
            return Ok(false);
        }

        let previous = current_str;
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        self.config.auth_mode = parsed;
        let runtime = self.build_replacement_runtime(session, session_id, self.config.clone())?;
        self.replace_runtime(runtime)?;
        println!("{}", format_auth_switch_report(&previous, parsed.as_str()));
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        let previous_session = self.session.clone();
        let session_state = new_cli_session()?;
        let next_handle = create_managed_session_handle(&session_state.session_id)?;
        let runtime = self.build_replacement_runtime(
            session_state.with_persistence_path(next_handle.path.clone()),
            next_handle.id.clone(),
            self.config.clone(),
        )?;
        self.session = next_handle;
        self.replace_runtime(runtime)?;
        println!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.config.model,
            self.config.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        );
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        println!("{}", format_cost_report(cumulative));
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            let sessions = list_managed_sessions()?;
            if sessions.is_empty() {
                println!("No sessions found.");
                return Ok(false);
            }
            let labels: Vec<String> = sessions
                .iter()
                .map(|s| format!("{} ({} msgs)", s.id, s.message_count))
                .collect();
            let selection = Select::new()
                .with_prompt("Select session to resume")
                .items(&labels)
                .default(0)
                .interact_opt()?;
            return match selection {
                Some(idx) => self.resume_session(Some(sessions[idx].id.clone())),
                None => Ok(false),
            };
        };

        let (handle, session) = load_session_reference(&session_ref)?;
        let message_count = session.messages.len();
        let session_id = session.session_id.clone();
        let runtime =
            self.build_replacement_runtime(session, handle.id.clone(), self.config.clone())?;
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        Ok(true)
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        print_with_pager(&render_config_report(section)?);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        print_with_pager(&render_memory_report()?);
        Ok(())
    }

    fn open_in_editor(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            fs::write(path, "")?;
        }
        let (editor, source) = if let Ok(v) = env::var("VISUAL") {
            (v, "$VISUAL")
        } else if let Ok(e) = env::var("EDITOR") {
            (e, "$EDITOR")
        } else {
            ("vi".to_string(), "default")
        };
        let status = std::process::Command::new(&editor).arg(path).status()?;
        if !status.success() {
            return Err(format!("Editor '{}' exited with {}", editor, status).into());
        }
        println!("Opened memory file at {}", path.display());
        if source == "default" {
            println!(
                "> To use a different editor, set the $EDITOR or $VISUAL environment variable."
            );
        } else {
            println!(
                "> Using {}=\"{}\". To change editor, set $EDITOR or $VISUAL environment variable.",
                source, editor
            );
        }
        Ok(())
    }

    fn edit_memory() -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let project_context = ProjectContext::discover(&cwd, runtime::today_local())?;
        let files = &project_context.instruction_files;
        let target: PathBuf = if files.is_empty() {
            // No instruction files found — default to AGENTS.md in cwd.
            println!("No instruction files found. Creating AGENTS.md in the current directory.");
            cwd.join("AGENTS.md")
        } else if files.len() == 1 {
            files[0].path.clone()
        } else {
            let labels: Vec<String> = files.iter().map(|f| f.path.display().to_string()).collect();
            let selection = Select::new()
                .with_prompt("Select memory file to edit")
                .items(&labels)
                .default(0)
                .interact_opt()?;
            match selection {
                Some(idx) => files[idx].path.clone(),
                None => return Ok(()),
            }
        };
        Self::open_in_editor(&target)
    }

    fn print_agents(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_agents_slash_command(args, &cwd)?),
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&handle_agents_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    fn print_mcp(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // `scode mcp serve` starts a stdio MCP server exposing scode's built-in
        // tools. All other `mcp` subcommands fall through to the existing
        // configured-server reporter (`list`, `status`, ...).
        if matches!(args.map(str::trim), Some("serve")) {
            return run_mcp_serve();
        }
        let cwd = env::current_dir()?;
        // Include plugin-provided MCP servers so `scode mcp` matches what the
        // runtime actually wires up. Plugin discovery may fail (e.g. malformed
        // installed.json) — degrade to runtime-only view instead of erroring,
        // matching the contract of the underlying handlers.
        let plugin_load_outcome = plugin_load_outcome_for_cwd(&cwd).ok();
        match output_format {
            CliOutputFormat::Text => println!(
                "{}",
                handle_mcp_slash_command_with_plugins(args, &cwd, plugin_load_outcome.as_ref())?
            ),
            CliOutputFormat::Json => {
                let value = handle_mcp_slash_command_json_with_plugins(
                    args,
                    &cwd,
                    plugin_load_outcome.as_ref(),
                )?;
                // Propagate ok:false → non-zero exit so automation callers
                // can rely on exit code instead of inspecting the envelope.
                let is_error = value.get("ok").and_then(|v| v.as_bool()) == Some(false);
                println!("{}", serde_json::to_string_pretty(&value)?);
                if is_error {
                    std::process::exit(1);
                }
            }
        }
        Ok(())
    }

    fn print_skills(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let plugin_load_outcome = plugin_load_outcome_for_cwd(&cwd)?;
        print_skills_for_outcome(args, output_format, &cwd, Some(&plugin_load_outcome))
    }

    fn print_skills_with_plugins(
        &self,
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        print_skills_for_outcome(
            args,
            output_format,
            &cwd,
            Some(self.runtime.plugin_load_outcome()),
        )
    }

    fn print_plugins(
        action: Option<&str>,
        target: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager, &cwd)?;
        match output_format {
            CliOutputFormat::Text => println!("{}", result.message),
            CliOutputFormat::Json => {
                // For list-style actions, emit a structured `plugins` array
                // alongside the rendered text so scripts/CI can consume the
                // data without re-parsing the text payload.
                let action_name = action.unwrap_or("list");
                let plugins_array = matches!(action_name, "list").then(|| {
                    manager
                        .list_installed_plugins()
                        .ok()
                        .map(|plugins| {
                            plugins
                                .iter()
                                .map(|plugin| {
                                    let mut entry = serde_json::Map::new();
                                    entry.insert(
                                        "id".to_string(),
                                        Value::String(plugin.metadata.id.clone()),
                                    );
                                    entry.insert(
                                        "name".to_string(),
                                        Value::String(plugin.metadata.name.clone()),
                                    );
                                    if let Some(display_name) = &plugin.metadata.display_name {
                                        entry.insert(
                                            "display_name".to_string(),
                                            Value::String(display_name.clone()),
                                        );
                                    }
                                    entry.insert(
                                        "version".to_string(),
                                        Value::String(plugin.metadata.version.clone()),
                                    );
                                    entry.insert(
                                        "description".to_string(),
                                        Value::String(plugin.metadata.description.clone()),
                                    );
                                    entry.insert(
                                        "kind".to_string(),
                                        Value::String(plugin.metadata.kind.to_string()),
                                    );
                                    entry.insert(
                                        "source".to_string(),
                                        Value::String(plugin.metadata.source.clone()),
                                    );
                                    entry
                                        .insert("enabled".to_string(), Value::Bool(plugin.enabled));
                                    if let Some(root) = &plugin.metadata.root {
                                        entry.insert(
                                            "root".to_string(),
                                            Value::String(root.display().to_string()),
                                        );
                                    }
                                    Value::Object(entry)
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                });
                let mut envelope = json!({
                    "kind": "plugin",
                    "action": action_name,
                    "target": target,
                    "message": result.message,
                    "reload_runtime": result.reload_runtime,
                });
                if let Some(array) = plugins_array {
                    envelope["plugins"] = Value::Array(array);
                }
                println!("{}", serde_json::to_string_pretty(&envelope)?);
            }
        }
        Ok(())
    }

    fn print_diff() -> Result<(), Box<dyn std::error::Error>> {
        print_with_pager(&render_diff_report()?);
        Ok(())
    }

    fn handle_undo(&mut self) {
        let messages = &self.runtime.session().messages;
        match crate::cli::undo::find_last_undoable_edit(messages, &self.undone_tool_use_ids) {
            None => {
                println!(
                    "Nothing to undo in this session. /undo only restores edit_file and write_file results recorded in the live session."
                );
            }
            Some(edit) => match crate::cli::undo::apply_undo(&edit) {
                Ok(message) => {
                    self.undone_tool_use_ids.insert(edit.tool_use_id.clone());
                    println!("{message}");
                }
                Err(error) => {
                    eprintln!("undo failed for {}: {error}", edit.file_path);
                }
            },
        }
    }

    fn print_version(output_format: CliOutputFormat) {
        let _ = crate::print_version(output_format);
    }

    fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                // On a TTY, present a fuzzy picker that switches on Enter and
                // is silent on Esc. Non-interactive callers (CI, scripted
                // pipes, `--output-format json` paths) keep the original
                // text-table listing.
                if io::stdin().is_terminal() && io::stdout().is_terminal() {
                    let sessions = list_managed_sessions()?;
                    if sessions.is_empty() {
                        println!("{}", render_session_list(&self.session.id)?);
                        return Ok(false);
                    }
                    let default_idx = sessions
                        .iter()
                        .position(|session| session.id == self.session.id)
                        .unwrap_or(0);
                    let items: Vec<String> = sessions
                        .iter()
                        .map(|session| format_session_picker_entry(session, &self.session.id))
                        .collect();
                    let selection = FuzzySelect::new()
                        .with_prompt("Select a session (type to filter, Esc to cancel)")
                        .items(&items)
                        .default(default_idx)
                        .interact_opt()?;
                    let Some(idx) = selection else {
                        return Ok(false);
                    };
                    let target = sessions[idx].id.clone();
                    if target == self.session.id {
                        println!("Session unchanged (already active: {target}).");
                        return Ok(false);
                    }
                    return self.handle_session_command(Some("switch"), Some(&target));
                }
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let (handle, session) = load_session_reference(target)?;
                let message_count = session.messages.len();
                let session_id = session.session_id.clone();
                let runtime = self.build_replacement_runtime(
                    session,
                    handle.id.clone(),
                    self.config.clone(),
                )?;
                self.replace_runtime(runtime)?;
                self.session = SessionHandle {
                    id: session_id,
                    path: handle.path,
                };
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("fork") => {
                let forked = self.runtime.fork_session(target.map(ToOwned::to_owned));
                let parent_session_id = self.session.id.clone();
                let handle = create_managed_session_handle(&forked.session_id)?;
                let branch_name = forked
                    .fork
                    .as_ref()
                    .and_then(|fork| fork.branch_name.clone());
                let forked = forked.with_persistence_path(handle.path.clone());
                let message_count = forked.messages.len();
                forked.save_to_path(&handle.path)?;
                let runtime =
                    self.build_replacement_runtime(forked, handle.id.clone(), self.config.clone())?;
                self.replace_runtime(runtime)?;
                self.session = handle;
                println!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                if !confirm_session_deletion(&handle.id) {
                    println!("delete: cancelled.");
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some(other) => {
                println!(
                    "Unknown /session action '{other}'. Use /session list, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                );
                Ok(false)
            }
        }
    }

    fn handle_plugins_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager, &cwd)?;
        println!("{}", result.message);
        if result.reload_runtime {
            self.reload_runtime_features()?;
        }
        Ok(false)
    }

    fn reload_runtime_features(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        let runtime = self.build_replacement_runtime(session, session_id, self.config.clone())?;
        self.replace_runtime(runtime)?;
        self.persist_session()
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.runtime.compact(CompactionConfig::default());
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        let session_id = self.session.id.clone();
        let runtime = self.build_replacement_runtime(
            result.compacted_session,
            session_id,
            self.config.clone(),
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!("{}", format_compact_report(removed, kept, skipped));
        Ok(())
    }

    fn run_internal_prompt_text_with_progress(
        &mut self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        let mut runtime = self.build_replacement_runtime(
            session,
            session_id,
            RuntimeConfig {
                enable_tools,
                emit_output: false,
                progress_reporter: progress,
                ..self.config.clone()
            },
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let summary = self.tokio_runtime.block_on(runtime.run_turn(
            prompt,
            Some(&mut permission_prompter),
            None,
        ))?;
        let text = final_assistant_text(&summary).trim().to_string();
        runtime.shutdown_mcp()?;
        runtime.shutdown_plugins()?;
        Ok(text)
    }

    fn run_internal_prompt_text(
        &mut self,
        prompt: &str,
        enable_tools: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_internal_prompt_text_with_progress(prompt, enable_tools, None)
    }

    fn run_bughunter(&self, scope: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_bughunter_report(scope));
        Ok(())
    }

    fn run_ultraplan(&self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_ultraplan_report(task));
        Ok(())
    }

    fn run_teleport(target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            println!("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        println!("{}", render_teleport_report(target)?);
        Ok(())
    }

    fn run_debug_tool_call(&self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/debug-tool-call", args)?;
        println!("{}", render_last_tool_debug_report(self.runtime.session())?);
        Ok(())
    }

    fn run_commit(&mut self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/commit", args)?;
        let status = git_output(&["status", "--short", "--branch"])?;
        let summary = parse_git_workspace_summary(Some(&status));
        let branch = parse_git_status_branch(Some(&status));
        if summary.is_clean() {
            println!("{}", format_commit_skipped_report());
            return Ok(());
        }

        println!(
            "{}",
            format_commit_preflight_report(branch.as_deref(), summary)
        );
        Ok(())
    }

    fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let branch =
            resolve_git_branch_for(&env::current_dir()?).unwrap_or_else(|| "unknown".to_string());
        println!("{}", format_pr_report(&branch, context));
        Ok(())
    }

    fn run_issue(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_issue_report(context));
        Ok(())
    }
}

fn print_skills_for_outcome(
    args: Option<&str>,
    output_format: CliOutputFormat,
    cwd: &Path,
    plugin_load_outcome: Option<&PluginLoadOutcome>,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!(
            "{}",
            handle_skills_slash_command_with_plugins(args, cwd, plugin_load_outcome)?
        ),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&handle_skills_slash_command_json_with_plugins(
                args,
                cwd,
                plugin_load_outcome,
            )?)?
        ),
    }
    Ok(())
}

fn init_claude_md() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(initialize_repo(&cwd)?.render())
}

fn run_init(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let report = initialize_repo(&cwd)?;
    let message = report.render();
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&init_json_value(&report, &message))?
        ),
    }
    Ok(())
}

/// #142: emit first-class structured fields alongside the legacy `message`
/// string so consumers can detect per-artifact state without substring matching.
fn init_json_value(report: &crate::init::InitReport, message: &str) -> serde_json::Value {
    use crate::init::InitStatus;
    json!({
        "kind": "init",
        "project_path": report.project_root.display().to_string(),
        "created": report.artifacts_with_status(InitStatus::Created),
        "updated": report.artifacts_with_status(InitStatus::Updated),
        "skipped": report.artifacts_with_status(InitStatus::Skipped),
        "artifacts": report.artifact_json_entries(),
        "next_step": crate::init::InitReport::NEXT_STEP,
        "message": message,
    })
}

fn build_system_prompt(model: &str) -> Result<SystemPrompt, Box<dyn std::error::Error>> {
    build_system_prompt_for(&env::current_dir()?, model)
}

fn build_system_prompt_for(
    cwd: &Path,
    model: &str,
) -> Result<SystemPrompt, Box<dyn std::error::Error>> {
    // Use the local date at session-start time (not the build date baked
    // into DEFAULT_DATE) so the cacheable system prompt reflects when the
    // user actually started talking. ConversationRuntime separately tracks
    // this date and emits a system-reminder if the date rolls over
    // mid-session, keeping the prompt cache prefix warm.
    let mut prompt = load_system_prompt(
        cwd.to_path_buf(),
        runtime::today_local(),
        env::consts::OS,
        "unknown",
        model_family_identity_for(model),
    )?;
    // Coordinator mode: when the SUDOCODE_COORDINATOR_MODE env var is
    // set, prepend the ported CC-fork coordinator role prompt so it
    // takes primacy over the default identity. See
    // runtime::coordinator_mode for the full port.
    runtime::coordinator_mode::apply_coordinator_prompt_if_enabled(&mut prompt);
    Ok(prompt)
}

fn build_runtime_plugin_state() -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
}

fn plugin_load_outcome_for_cwd(
    cwd: &Path,
) -> Result<PluginLoadOutcome, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let runtime_config = loader.load()?;
    let plugin_manager = build_plugin_manager(cwd, &loader, &runtime_config);
    Ok(plugin_manager.plugin_registry_report()?.load_outcome())
}

pub(crate) fn build_runtime_plugin_state_with_loader(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let plugin_manager = build_plugin_manager(cwd, loader, runtime_config);
    let plugin_registry_report = plugin_manager.plugin_registry_report()?;
    let plugin_load_outcome = plugin_registry_report.load_outcome();
    let plugin_registry = plugin_registry_report.into_registry()?;
    let plugin_hook_config =
        runtime_hook_config_from_plugin_hooks(plugin_registry.projected_hooks()?);
    let feature_config = runtime_config
        .feature_config()
        .clone()
        .with_hooks(runtime_config.hooks().merged(&plugin_hook_config));
    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?;
    let (mcp_state, runtime_tools) = build_runtime_mcp_state(runtime_config, &plugin_load_outcome)?;
    let tool_registry = match tool_registry.with_runtime_tools(runtime_tools) {
        Ok(tool_registry) => tool_registry,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(Box::new(std::io::Error::other(error)));
        }
    };
    Ok(RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        plugin_load_outcome,
        mcp_state,
    })
}

fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_config = runtime_config
        .plugins()
        .to_plugin_manager_config(cwd, loader.config_home());
    PluginManager::new(plugin_config)
}

fn runtime_hook_config_from_plugin_hooks(
    hooks: plugins::ProjectedPluginHooks,
) -> runtime::RuntimeHookConfig {
    runtime::RuntimeHookConfig::new_with_sources(
        hooks
            .pre_tool_use
            .into_iter()
            .map(|entry| (entry.command, entry.plugin_id))
            .collect(),
        hooks
            .post_tool_use
            .into_iter()
            .map(|entry| (entry.command, entry.plugin_id))
            .collect(),
        hooks
            .post_tool_use_failure
            .into_iter()
            .map(|entry| (entry.command, entry.plugin_id))
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InternalPromptProgressState {
    pub(crate) command_label: &'static str,
    pub(crate) task_label: String,
    pub(crate) step: usize,
    pub(crate) phase: String,
    pub(crate) detail: Option<String>,
    pub(crate) saw_final_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InternalPromptProgressEvent {
    Started,
    Update,
    Heartbeat,
    Complete,
    Failed,
}

#[derive(Debug)]
struct InternalPromptProgressShared {
    state: Mutex<InternalPromptProgressState>,
    output_lock: Mutex<()>,
    started_at: Instant,
}

#[derive(Debug, Clone)]
struct InternalPromptProgressReporter {
    shared: Arc<InternalPromptProgressShared>,
}

#[derive(Debug)]
struct InternalPromptProgressRun {
    reporter: InternalPromptProgressReporter,
    heartbeat_stop: Option<mpsc::Sender<()>>,
    heartbeat_handle: Option<thread::JoinHandle<()>>,
}

impl InternalPromptProgressReporter {
    fn ultraplan(task: &str) -> Self {
        Self {
            shared: Arc::new(InternalPromptProgressShared {
                state: Mutex::new(InternalPromptProgressState {
                    command_label: "Ultraplan",
                    task_label: task.to_string(),
                    step: 0,
                    phase: "planning started".to_string(),
                    detail: Some(format!("task: {task}")),
                    saw_final_text: false,
                }),
                output_lock: Mutex::new(()),
                started_at: Instant::now(),
            }),
        }
    }

    fn emit(&self, event: InternalPromptProgressEvent, error: Option<&str>) {
        let snapshot = self.snapshot();
        let line = format_internal_prompt_progress_line(event, &snapshot, self.elapsed(), error);
        self.write_line(&line);
    }

    fn mark_model_phase(&self) {
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = if state.step == 1 {
                "analyzing request".to_string()
            } else {
                "reviewing findings".to_string()
            };
            state.detail = Some(format!("task: {}", state.task_label));
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn mark_tool_phase(&self, name: &str, input: &str) {
        let detail = describe_tool_progress(name, input);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = format!("running {name}");
            state.detail = Some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn mark_text_phase(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let detail = truncate_for_summary(first_visible_line(trimmed), 120);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            if state.saw_final_text {
                return;
            }
            state.saw_final_text = true;
            state.step += 1;
            state.phase = "drafting final plan".to_string();
            state.detail = (!detail.is_empty()).then_some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn emit_heartbeat(&self) {
        let snapshot = self.snapshot();
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Heartbeat,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn snapshot(&self) -> InternalPromptProgressState {
        self.shared
            .state
            .lock()
            .expect("internal prompt progress state poisoned")
            .clone()
    }

    fn elapsed(&self) -> Duration {
        self.shared.started_at.elapsed()
    }

    fn write_line(&self, line: &str) {
        let _guard = self
            .shared
            .output_lock
            .lock()
            .expect("internal prompt progress output lock poisoned");
        let mut stdout = io::stdout();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

impl InternalPromptProgressRun {
    fn start_ultraplan(task: &str) -> Self {
        let reporter = InternalPromptProgressReporter::ultraplan(task);
        reporter.emit(InternalPromptProgressEvent::Started, None);

        let (heartbeat_stop, heartbeat_rx) = mpsc::channel();
        let heartbeat_reporter = reporter.clone();
        let heartbeat_handle = thread::spawn(move || loop {
            match heartbeat_rx.recv_timeout(INTERNAL_PROGRESS_HEARTBEAT_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => heartbeat_reporter.emit_heartbeat(),
            }
        });

        Self {
            reporter,
            heartbeat_stop: Some(heartbeat_stop),
            heartbeat_handle: Some(heartbeat_handle),
        }
    }

    fn reporter(&self) -> InternalPromptProgressReporter {
        self.reporter.clone()
    }

    fn finish_success(&mut self) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Complete, None);
    }

    fn finish_failure(&mut self, error: &str) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Failed, Some(error));
    }

    fn stop_heartbeat(&mut self) {
        if let Some(sender) = self.heartbeat_stop.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.heartbeat_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for InternalPromptProgressRun {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

fn build_runtime(
    session: Session,
    session_id: &str,
    config: RuntimeConfig,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    build_runtime_for_cwd(&cwd, session, session_id, config)
}

fn build_runtime_for_cwd(
    cwd: &Path,
    session: Session,
    session_id: &str,
    config: RuntimeConfig,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let file_config = loader.load()?;
    let runtime_plugin_state = build_runtime_plugin_state_with_loader(cwd, &loader, &file_config)?;
    build_runtime_with_plugin_state(cwd, session, session_id, config, runtime_plugin_state)
}

fn build_runtime_with_plugin_state(
    cwd: &Path,
    mut session: Session,
    session_id: &str,
    config: RuntimeConfig,
    runtime_plugin_state: RuntimePluginState,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    // Persist the model in session metadata so resumed sessions can report it.
    if session.model.is_none() {
        session.model = Some(config.model.clone());
    }
    let RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        plugin_load_outcome,
        mcp_state,
    } = runtime_plugin_state;
    let policy =
        match permission_policy(config.permission_mode, &feature_config, &tool_registry, cwd) {
            Ok(policy) => policy,
            Err(error) => {
                shutdown_mcp_state_best_effort(&mcp_state);
                return Err(Box::new(std::io::Error::other(error)));
            }
        };
    let mut system_prompt = config.system_prompt.clone();
    if let Some(section) = render_plugin_capabilities_section(&plugin_load_outcome.loaded_plugins) {
        system_prompt.dynamic_sections.push(section);
    }
    let emit_output = config.emit_output;
    let client = match AnthropicRuntimeClient::new(session_id, &config, tool_registry.clone()) {
        Ok(client) => client,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(error);
        }
    };
    let mut runtime = ConversationRuntime::new_with_features(
        session,
        client,
        CliToolExecutor::new(
            config.allowed_tools,
            emit_output,
            tool_registry.clone(),
            mcp_state.clone(),
        ),
        policy,
        system_prompt,
        &feature_config,
    )
    .with_session_known_date(runtime::today_local());
    if emit_output {
        runtime = runtime.with_hook_progress_reporter(Box::new(CliHookProgressReporter));
    }
    if let Err(error) = plugin_registry.initialize() {
        shutdown_mcp_state_best_effort(&mcp_state);
        return Err(Box::new(error));
    }
    Ok(BuiltRuntime::new(
        runtime,
        plugin_registry,
        plugin_load_outcome,
        mcp_state,
    ))
}

fn shutdown_mcp_state_best_effort(mcp_state: &Option<Arc<Mutex<RuntimeMcpState>>>) {
    if let Some(state) = mcp_state {
        let _ = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
    }
}

struct CliHookProgressReporter;

impl runtime::HookProgressReporter for CliHookProgressReporter {
    fn on_event(&mut self, event: &runtime::HookProgressEvent) {
        // Format SudoCode plugin attribution once; each outcome line includes
        // it so the user sees *who* ran the hook in addition to *what* happened.
        fn attribution(plugin_source: Option<&str>) -> String {
            match plugin_source {
                Some(plugin_id) => format!(" (SudoCode plugin {plugin_id})"),
                None => String::new(),
            }
        }
        match event {
            runtime::HookProgressEvent::Started {
                event,
                tool_name,
                command,
                plugin_source,
            } => eprintln!(
                "[hook {event_name}] {tool_name}: {command}{attr}",
                event_name = event.as_str(),
                attr = attribution(plugin_source.as_deref())
            ),
            runtime::HookProgressEvent::Completed {
                event,
                tool_name,
                command,
                plugin_source,
            } => eprintln!(
                "[hook done {event_name}] {tool_name}: {command}{attr}",
                event_name = event.as_str(),
                attr = attribution(plugin_source.as_deref())
            ),
            runtime::HookProgressEvent::Denied {
                event,
                tool_name,
                command,
                plugin_source,
            } => eprintln!(
                "[hook DENIED {event_name}] {tool_name}: {command}{attr}",
                event_name = event.as_str(),
                attr = attribution(plugin_source.as_deref())
            ),
            runtime::HookProgressEvent::Failed {
                event,
                tool_name,
                command,
                plugin_source,
            } => eprintln!(
                "[hook FAILED {event_name}] {tool_name}: {command}{attr}",
                event_name = event.as_str(),
                attr = attribution(plugin_source.as_deref())
            ),
            runtime::HookProgressEvent::Cancelled {
                event,
                tool_name,
                command,
                plugin_source,
            } => eprintln!(
                "[hook cancelled {event_name}] {tool_name}: {command}{attr}",
                event_name = event.as_str(),
                attr = attribution(plugin_source.as_deref())
            ),
        }
    }
}

struct CliPermissionPrompter {
    current_mode: PermissionMode,
}

impl CliPermissionPrompter {
    fn new(current_mode: PermissionMode) -> Self {
        Self { current_mode }
    }
}

impl runtime::PermissionPrompter for CliPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        println!();
        println!(
            "{}",
            format_permission_prompt_box(
                &request.tool_name,
                &request.input,
                request.current_mode.as_str(),
                request.required_mode.as_str(),
                request.reason.as_deref(),
            )
        );

        if !io::stdin().is_terminal() {
            // Non-interactive fallback: read a line from stdin.
            print!("Approve this tool call? [y/N]: ");
            let _ = io::stdout().flush();
            let mut response = String::new();
            return match io::stdin().read_line(&mut response) {
                Ok(_) => {
                    let normalized = response.trim().to_ascii_lowercase();
                    if matches!(normalized.as_str(), "y" | "yes") {
                        runtime::PermissionPromptDecision::Allow
                    } else {
                        runtime::PermissionPromptDecision::Deny {
                            reason: format!(
                                "tool '{}' denied by user approval prompt",
                                request.tool_name
                            ),
                        }
                    }
                }
                Err(error) => runtime::PermissionPromptDecision::Deny {
                    reason: format!("permission approval failed: {error}"),
                },
            };
        }

        let items = &["Allow once", "Deny"];
        let selection = Select::new()
            .with_prompt("Approve this tool call?")
            .items(items)
            .default(0)
            .interact_opt();

        match selection {
            Ok(Some(0)) => runtime::PermissionPromptDecision::Allow,
            Ok(Some(_) | None) => runtime::PermissionPromptDecision::Deny {
                reason: format!(
                    "tool '{}' denied by user approval prompt",
                    request.tool_name
                ),
            },
            Err(error) => runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}

/// Slash commands that are registered in the spec list but not yet implemented
/// in this build. Used to filter both REPL completions and help output so the
/// discovery surface only shows commands that actually work (ROADMAP #39).
pub(crate) const STUB_COMMANDS: &[&str] = &[
    "login",
    "logout",
    "vim",
    "upgrade",
    "share",
    "feedback",
    "files",
    "fast",
    "exit",
    "summary",
    "desktop",
    "brief",
    "advisor",
    "stickers",
    "insights",
    "thinkback",
    "release-notes",
    "security-review",
    "keybindings",
    "privacy-settings",
    "plan",
    "review",
    "tasks",
    "theme",
    "voice",
    "usage",
    "rename",
    "copy",
    "hooks",
    "context",
    "color",
    "effort",
    "branch",
    "rewind",
    "ide",
    "tag",
    "output-style",
    "add-dir",
    // Spec entries with no parse arm — produce circular "Did you mean" error
    // without this guard. Adding here routes them to the proper unsupported
    // message and excludes them from REPL completions / help.
    // NOTE: do NOT add "stats", "tokens", "cache" — they are implemented.
    "allowed-tools",
    "bookmarks",
    "workspace",
    "reasoning",
    "budget",
    "rate-limit",
    "changelog",
    "diagnostics",
    "metrics",
    "tool-details",
    "focus",
    "unfocus",
    "pin",
    "unpin",
    "language",
    "profile",
    "max-tokens",
    "temperature",
    "system-prompt",
    "notifications",
    "telemetry",
    "env",
    "project",
    "terminal-setup",
    "api-key",
    "reset",
    "stop",
    "retry",
    "paste",
    "screenshot",
    "image",
    "search",
    "listen",
    "speak",
    "format",
    "test",
    "lint",
    "build",
    "run",
    "git",
    "stash",
    "blame",
    "log",
    "cron",
    "team",
    "benchmark",
    "migrate",
    "templates",
    "explain",
    "refactor",
    "docs",
    "fix",
    "perf",
    "chat",
    "web",
    "map",
    "symbols",
    "references",
    "definition",
    "hover",
    "autofix",
    "multi",
    "macro",
    "alias",
    "parallel",
    "subagent",
    "agent",
];

fn slash_command_completion_candidates_with_sessions(
    model: &str,
    active_session_id: Option<&str>,
    recent_session_ids: Vec<String>,
) -> Vec<(String, String)> {
    let mut completions = BTreeMap::new();

    for spec in slash_command_specs() {
        if STUB_COMMANDS.contains(&spec.name) {
            continue;
        }
        completions.insert(format!("/{}", spec.name), spec.summary.to_string());
        for alias in spec.aliases {
            if !STUB_COMMANDS.contains(alias) {
                completions.insert(format!("/{alias}"), spec.summary.to_string());
            }
        }
    }

    for candidate in [
        "/bughunter ",
        "/clear --confirm",
        "/config ",
        "/config env",
        "/config hooks",
        "/config model",
        "/config plugins",
        "/mcp ",
        "/mcp list",
        "/mcp show ",
        "/export ",
        "/issue ",
        "/model ",
        "/model opus",
        "/model sonnet",
        "/model haiku",
        "/permissions ",
        "/permissions read-only",
        "/permissions workspace-write",
        "/permissions danger-full-access",
        "/auth ",
        "/auth subscription",
        "/auth proxy",
        "/auth api-key",
        "/plugin list",
        "/plugin install ",
        "/plugin enable ",
        "/plugin disable ",
        "/plugin uninstall ",
        "/plugin update ",
        "/plugins list",
        "/pr ",
        "/resume ",
        "/session list",
        "/session switch ",
        "/session fork ",
        "/teleport ",
        "/ultraplan ",
        "/agents help",
        "/mcp help",
        "/skills help",
    ] {
        completions
            .entry(candidate.to_string())
            .or_insert_with(String::new);
    }

    // Add config-driven model aliases to /model completions.
    let sudocode_config = load_sudocode_config_for_current_dir();
    for alias in sudocode_config.models.keys() {
        completions
            .entry(format!("/model {alias}"))
            .or_insert_with(String::new);
    }

    if !model.trim().is_empty() {
        completions
            .entry(format!("/model {}", resolve_model_alias_with_config(model)))
            .or_insert_with(String::new);
        completions
            .entry(format!("/model {model}"))
            .or_insert_with(String::new);
    }

    if let Some(active_session_id) = active_session_id.filter(|value| !value.trim().is_empty()) {
        completions
            .entry(format!("/resume {active_session_id}"))
            .or_insert_with(String::new);
        completions
            .entry(format!("/session switch {active_session_id}"))
            .or_insert_with(String::new);
    }

    for session_id in recent_session_ids
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .take(10)
    {
        completions
            .entry(format!("/resume {session_id}"))
            .or_insert_with(String::new);
        completions
            .entry(format!("/session switch {session_id}"))
            .or_insert_with(String::new);
    }

    completions.into_iter().collect()
}

fn resolve_auth_mode(
    model: &str,
    explicit: Option<AuthMode>,
    config: &api::SudoCodeConfig,
) -> Result<AuthMode, String> {
    const PRIORITY: &[&str] = &["subscription", "proxy", "api-key"];
    if let Some(mode) = explicit {
        return Ok(mode);
    }
    let entry = api::resolve_model(config, model).ok_or_else(|| {
        format!(
            "model '{model}' not found in config. Run /model to configure it, \
             or pass --auth=<subscription|proxy|api-key> explicitly."
        )
    })?;
    for mode_str in PRIORITY {
        if entry.providers.contains_key(*mode_str) {
            return AuthMode::parse(mode_str);
        }
    }
    Err(format!(
        "no auth mode available for model '{model}'. Run /model to configure it, \
         or pass --auth=<subscription|proxy|api-key> explicitly."
    ))
}
