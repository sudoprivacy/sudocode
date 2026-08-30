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
mod cancel;
mod cli;
mod init;
mod input;
mod input_chrome;
mod input_queue;
mod render;
mod repl_async;
mod repl_ui;
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
    base_url_for_mode, resolve_startup_auth_source, AnthropicClient, AuthMode, AuthSource,
    ContentBlockDelta, InputContentBlock, InputMessage, MessageRequest, MessageResponse,
    OutputContentBlock, PromptCache, ProviderClient as ApiProviderClient, ProviderKind,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};

use cli::api_client::{
    collect_prompt_cache_events, collect_tool_results, collect_tool_uses, final_assistant_text,
    AnthropicRuntimeClient,
};
use cli::args::{
    config_model_for_current_dir, default_permission_mode, format_unknown_slash_command,
    load_sudocode_config_for_current_dir, load_sudocode_config_for_cwd,
    parse_args_with_prompt_overrides, permission_mode_from_label, require_sudocode_config_for_cwd,
    resolve_model_alias, resolve_model_alias_with_config, resolve_repl_model,
    try_resolve_bare_skill_prompt, try_resolve_bare_skill_prompt_with_plugins, AllowedToolSet,
    CliAction, CliOutputFormat, LocalHelpTopic,
};
use cli::export::{
    collect_session_prompt_history, parse_history_count, render_export_text,
    render_prompt_history_report, resolve_export_path, run_export, truncate_for_prompt,
    PromptHistoryEntry,
};
use cli::format::{
    describe_tool_progress, first_visible_line, format_auth_report, format_auth_switch_report,
    format_auto_compaction_notice, format_bughunter_report, format_commit_preflight_report,
    format_commit_skipped_report, format_compact_report, format_cost_report,
    format_internal_prompt_progress_line, format_issue_report, format_model_report,
    format_model_switch_report, format_permission_prompt_box, format_permissions_report,
    format_permissions_switch_report, format_pr_report, format_resume_report,
    format_sandbox_report, format_tool_call_start, format_tool_result,
    format_turn_status_line_with_branch, format_ultraplan_report, render_messages,
    render_resume_usage, render_version_report, truncate_for_summary,
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
use cli::mcp::{build_runtime_mcp_state, session_mcp_tool_names, RuntimeMcpState};
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
use cli::tool_executor::{
    clear_pending_plan_execution, permission_policy, take_pending_plan_execution, CliToolExecutor,
};
use commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command_json_with_plugins, handle_mcp_slash_command_with_plugins,
    handle_plugins_slash_command, handle_skills_slash_command, handle_skills_slash_command_json,
    handle_skills_slash_command_json_with_plugins, handle_skills_slash_command_with_plugins,
    render_skills_prompt_section, render_slash_command_help, render_slash_command_help_filtered,
    resolve_skill_invocation, resolve_skill_invocation_with_plugins,
    resume_supported_slash_commands, slash_command_specs, validate_slash_command_input,
    SkillSlashDispatch, SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use dialoguer::{FuzzySelect, Select};
use init::initialize_repo;
use plugins::{PluginLoadOutcome, PluginManager, PluginRegistry};
use render::{
    ansi_bold_fg, ansi_fg, theme, MarkdownStreamState, SpinnerHandle, TerminalRenderer, DIM, RESET,
};
use runtime::{
    check_base_commit, compact_session_sync, estimate_block_tokens, estimate_session_tokens,
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

const DEFAULT_MODEL: &str = "claude-opus-4-8";

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
/// command execution — which, via `SpinnerHandle::new()`, happens deep inside
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
        // (error handling below — success path returns from main normally)
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
    // NOTE: `run_repl_iocraft_dispatch` calls `process::exit(0)` itself
    // because the iocraft render loop thread cannot be joined portably.
    // All other paths (single-turn prompt, doctor, etc.) return here and
    // exit naturally via `main` returning.
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
    let (action, prompt_overrides) = parse_args_with_prompt_overrides(&args)?;
    // Only writer in the process; a second `set` cannot happen.
    let _ = CLI_PROMPT_OVERRIDES.set(prompt_overrides);
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
            output_format,
        } => print_system_prompt(cwd, date, output_format)?,
        CliAction::Version { output_format } => print_version(output_format)?,
        CliAction::ResumeSession {
            session_path,
            commands,
            output_format,
            model,
            permission_mode,
            auth_mode,
        } => run_resume(
            &session_path,
            &commands,
            output_format,
            model,
            permission_mode,
            auth_mode,
        ),
        CliAction::ListSessions { output_format } => {
            list_sessions_cli(output_format)?;
        }
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
            // Initiate background shutdown of the tokio runtime so that
            // lingering tasks (reqwest connection pool, fire-and-forget spawns)
            // don't block the Runtime::drop that happens when `cli` goes out
            // of scope.
            cli.tokio_runtime
                .shutdown_timeout(Duration::from_millis(500));
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
    eprintln!("Login via Claude Code credential import is no longer supported. Use ANTHROPIC_API_KEY or PROXY_AUTH_TOKEN instead.");
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
    let phases = runtime::BootstrapPlan::default_plan()
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
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut prompt = load_system_prompt(cwd.clone(), date, env::consts::OS, "unknown")?;
    // Coordinator mode: when SUDOCODE_COORDINATOR_MODE is set,
    // prepend the CC-fork coordinator role prompt so `scode
    // print-system-prompt` reflects what the runtime would send.
    runtime::coordinator_mode::apply_coordinator_prompt_if_enabled(&mut prompt);
    // Same order as a live session (`build_system_prompt_for` →
    // `build_runtime_with_plugin_state`): CLI prompt flags first, then the
    // available-skills listing.
    apply_cli_prompt_overrides(&mut prompt);
    // Mirror what build_runtime_with_plugin_state does for live sessions.
    // Load failures captured inside PluginLoadOutcome are excluded naturally;
    // Result errors propagate, so a broken plugin install fails this preview
    // exactly as it fails a live session.
    let outcome = plugin_load_outcome_for_cwd(&cwd)?;
    if let Some(section) = render_skills_prompt_section(&cwd, Some(&outcome)) {
        prompt.dynamic_sections.push(section);
    }
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

/// `--resume` without arguments: list available sessions so the user can
/// pick one to resume.  Prints id, age, message count, and branch — enough
/// context to identify the right session.
fn list_sessions_cli(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    use cli::session::list_managed_sessions;

    let sessions = list_managed_sessions()?;
    if sessions.is_empty() {
        if output_format == CliOutputFormat::Json {
            println!("{}", serde_json::json!({ "sessions": [] }));
        } else {
            println!("No saved sessions found.");
            println!(
                "Start a session first, then use `scode --resume latest` or `scode --resume <id>`."
            );
        }
        return Ok(());
    }

    if output_format == CliOutputFormat::Json {
        let entries: Vec<serde_json::Value> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "messages": s.message_count,
                    "modified_ms": s.modified_epoch_millis as u64,
                    "branch": s.branch_name,
                    "path": s.path.display().to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "sessions": entries }));
        return Ok(());
    }

    println!("Available sessions (use `scode --resume <id>`):\n");
    for (i, session) in sessions.iter().enumerate() {
        let age = cli::session::format_session_modified_age(session.modified_epoch_millis);
        let branch = session
            .branch_name
            .as_deref()
            .map(|b| format!("  branch={b}"))
            .unwrap_or_default();
        let latest = if i == 0 { "  (latest)" } else { "" };
        println!(
            "  {id}  {msgs} msgs  {age}{branch}{latest}",
            id = session.id,
            msgs = session.message_count,
        );
    }
    println!();
    println!("Tip: `scode --resume latest` resumes the most recent session.");
    Ok(())
}

#[allow(clippy::too_many_lines)]
/// CLI entry point for `--resume <id> [commands...]`.
///
/// - With commands: load session, run commands, exit (non-interactive).
/// - Without commands: load session, enter REPL with messages rendered.
///
/// This is the CLI dispatch function; `LiveCli::load_session` handles the
/// data-only operation (no I/O side effects). Display is the caller's job.
fn run_resume(
    session_path: &Path,
    commands: &[String],
    output_format: CliOutputFormat,
    model: String,
    permission_mode: PermissionMode,
    auth_mode: Option<AuthMode>,
) {
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
            return;
        }
        // No commands — enter the interactive REPL with the restored session.
        let resolved_model = resolve_repl_model(model);
        let mut cli = match LiveCli::new(resolved_model, true, None, permission_mode, auth_mode) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to initialize: {e}");
                std::process::exit(1);
            }
        };
        // Load the restored session into the running CLI.
        let session_ref = resolved_path.display().to_string();
        if let Err(e) = cli.load_session(Some(session_ref)) {
            eprintln!("failed to resume: {e}");
            std::process::exit(1);
        }
        // Enter the REPL loop (it renders banner + any existing messages).
        let mode = input_queue::QueueMode::from_env();
        if !matches!(mode, input_queue::QueueMode::Off) {
            if let Err(e) = run_repl_iocraft_dispatch(cli, mode) {
                eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
            }
        } else if let Err(e) = run_repl_loop(cli) {
            eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
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
            let result = runtime::compact_session_sync(
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
        SlashCommand::ConfigSet { .. } => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some("/config set is only available in interactive REPL mode".to_string()),
            json: None,
        }),
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
        SlashCommand::Cron { args } => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(cli::cron::run_slash(args.as_deref()).map_err(std::io::Error::other)?),
            json: None,
        }),
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

    // Env-gated opt-in to the async REPL that accepts input during a running
    // turn (see `repl_async` module docs and
    // `notes/plans/conversation-interrupt-queue-sudocode.md`). When set to
    // anything other than `off` / unset, dispatch to the async loop. Default
    // path below stays byte-identical to today's sync behavior.
    let mode = input_queue::QueueMode::from_env();
    if !matches!(mode, input_queue::QueueMode::Off) {
        return run_repl_iocraft_dispatch(cli, mode);
    }

    run_repl_loop(cli)
}

/// The synchronous REPL loop. Handles both new sessions and resumed
/// sessions identically: banner → existing messages (if any) → prompt.
fn run_repl_loop(mut cli: LiveCli) -> Result<(), Box<dyn std::error::Error>> {
    cli.is_repl = true;
    let mut editor =
        input::LineEditor::new("❯ ", cli.repl_completion_candidates().unwrap_or_default());
    println!("{}", cli.startup_banner());

    // Render existing messages and seed editor history from user prompts.
    // Same code path for new sessions (messages empty → no-op) and resumed
    // sessions (messages present → render + populate history).
    let messages = &cli.runtime.session().messages;
    if !messages.is_empty() {
        let term_width = crossterm::terminal::size()
            .map(|(cols, _)| cols as usize)
            .unwrap_or(80);
        let renderer = render::TerminalRenderer::new();
        let rendered = render_messages(messages, term_width, &renderer);
        if !rendered.is_empty() {
            println!("{rendered}");
        }
        // Seed rustyline history so ↑ recalls previous prompts. Skip
        // runtime-injected `<system-reminder>` blocks (date announcements,
        // rollover reminders) — they ride inside user messages for the
        // model but were never typed by the user, and a multi-line history
        // entry would auto-submit its first line when recalled.
        for msg in messages {
            if msg.role == runtime::MessageRole::User {
                let text = msg
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        runtime::ContentBlock::Text { text }
                            if !cli::format::is_system_reminder_text(text) =>
                        {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    editor.push_history(text);
                }
            }
        }
    }

    // Track session metrics for session_ended event
    let session_start = Instant::now();

    loop {
        editor.set_completions(cli.repl_completion_candidates().unwrap_or_default());
        input_chrome::print_before_prompt(cli.config.permission_mode.as_str());
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                // Clear the pre-printed bottom sep + footer. After
                // readline, cursor is at the start of the bottom sep
                // line. \x1b[J clears from cursor to end of screen.
                print!("\x1b[J");
                let _ = io::stdout().flush();
                let trimmed = input.trim().to_string();
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                match SlashCommand::parse(&trimmed) {
                    Ok(Some(command)) => {
                        match cli.handle_repl_command(command) {
                            Ok(true) => {
                                if let Err(e) = cli.persist_session() {
                                    eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
                            }
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("{}{error}{}", ansi_fg(theme().error), RESET);
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
                    cli.record_prompt_history(&trimmed);
                    if let Err(e) = cli.run_turn(&prompt) {
                        eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
                    }
                    continue;
                }
                cli.record_prompt_history(&trimmed);
                if let Err(e) = cli.run_turn(&trimmed) {
                    eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
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

/// Async REPL dispatch — takes ownership of the constructed `LiveCli`, wraps it
/// in `Arc<Mutex<>>` for the coordinator + runner thread, drives the loop, then
/// unwraps and finalizes the session on exit. Called by default (queue mode)
/// or when `SUDOCODE_INTERRUPT_QUEUE_MODE` is explicitly set to a non-off value.
fn run_repl_async_dispatch(
    mut cli: LiveCli,
    mode: input_queue::QueueMode,
) -> Result<(), Box<dyn std::error::Error>> {
    cli.is_repl = true;
    let banner = cli.startup_banner();
    let completions = cli.repl_completion_candidates().unwrap_or_default();

    // Re-enable the raw-mode ESC/Ctrl-C listener (HookAbortMonitor).
    // With prompt_ready gating, rustyline is NOT in readline during turns,
    // so HookAbortMonitor can safely own stdin for abort key detection.
    cli.esc_monitor_enabled = true;

    let abort_signal = runtime::HookAbortSignal::new();
    cli.persistent_abort_signal = Some(abort_signal.clone());

    // Shared atomic queue mode — the coordinator reads it each turn,
    // and `/config set auto-interrupt on|off` writes to it.
    let shared_mode = repl_async::shared_queue_mode(mode);
    cli.shared_queue_mode = Some(Arc::clone(&shared_mode));

    // ESC abort hook — wired into rustyline's ConditionalEventHandler so ESC
    // cancels the running turn without a separate raw-mode stdin thread.
    let esc_abort_hook: input::EscAbortHook = {
        let sig = abort_signal.clone();
        Arc::new(move || sig.abort())
    };

    let permission_label = cli.config.permission_mode.as_str().to_string();

    let cli_shared = std::sync::Arc::new(std::sync::Mutex::new(cli));
    let driver: std::sync::Arc<LiveCliDriver> = std::sync::Arc::new(LiveCliDriver {
        cli: std::sync::Arc::clone(&cli_shared),
        abort_signal,
    });

    let session_start = Instant::now();
    repl_async::run_coordinator_loop(
        driver,
        shared_mode,
        banner,
        completions,
        Some(esc_abort_hook),
        &permission_label,
    )?;

    // All threads spawned by the coordinator loop have already been joined
    // inside the loop (Exit branch + TurnDone reap). Unwrapping the Arc here
    // is a debug assertion of that invariant — if it fails, a thread leaked.
    let cli = std::sync::Arc::try_unwrap(cli_shared)
        .map_err(|_| "LiveCli still shared after coordinator loop exit — thread leak")?
        .into_inner()
        .map_err(|e| format!("LiveCli mutex poisoned: {e}"))?;

    // Session_ended telemetry parity with the sync path (`run_repl`
    // trailing block just above): record cumulative usage + duration so the
    // async path's users don't lose observability.
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

/// Adapts `LiveCli::run_turn` (which takes `&mut self`) to the
/// `repl_async::TurnDriver` trait (which is Sync + call by `&self`). The
/// mutex guarantees only one turn runs at a time, matching the coordinator's
/// single-runner-thread invariant. Holds a clone of the persistent abort
/// signal so `abort_current_turn` can fire without ever touching the mutex.
struct LiveCliDriver {
    cli: std::sync::Arc<std::sync::Mutex<LiveCli>>,
    abort_signal: runtime::HookAbortSignal,
}

impl repl_async::TurnDriver for LiveCliDriver {
    fn try_handle_slash_command(&self, input: &str) -> bool {
        let trimmed = input.trim();
        match SlashCommand::parse(trimmed) {
            Ok(Some(command)) => {
                let mut cli = self.cli.lock().expect("LiveCli mutex poisoned");
                match cli.handle_repl_command(command) {
                    Ok(true) => {
                        if let Err(e) = cli.persist_session() {
                            eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("{}{e}{}", ansi_fg(theme().error), RESET),
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!("{}{error}{}", ansi_fg(theme().error), RESET);
                true
            }
        }
    }

    fn run_turn(&self, prompt: &str) {
        let mut cli = self.cli.lock().expect("LiveCli mutex poisoned");
        if let Err(e) = cli.run_turn(prompt) {
            eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
        }
    }

    fn on_exit(&self) {
        // Parity with sync REPL — persist the session on /exit / /quit /
        // Ctrl-D so the next `--resume` sees the last turn's assistant
        // reply (see the identical `cli.persist_session()?` line at the
        // sync run_repl's /exit branch).
        let cli = self.cli.lock().expect("LiveCli mutex poisoned");
        if let Err(e) = cli.persist_session() {
            eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
        }
    }

    fn abort_current_turn(&self) {
        // Idempotent by design (HookAbortSignal::abort just sets a bool +
        // notifies waiters). No cli-lock needed — the runner thread already
        // holds it while streaming, and would deadlock if we tried.
        self.abort_signal.abort();
    }
}

/// iocraft-based REPL dispatch. Spawns the iocraft render loop on a
/// dedicated thread and runs the coordinator loop on the current thread.
/// The coordinator reads `InputEvent`s from the iocraft UI and dispatches
/// turns on runner threads, identical to the rustyline-based coordinator
/// but with iocraft owning stdin+stdout.
type PendingQuestionAnswer = Arc<Mutex<Option<mpsc::SyncSender<String>>>>;

fn consume_pending_question_answer(pending: &PendingQuestionAnswer, text: String) -> bool {
    let Some(tx) = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    else {
        return false;
    };
    let _ = tx.send(text);
    true
}

fn cancel_pending_question_answer(pending: &PendingQuestionAnswer) {
    let _ = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
}

/// Callback type for interactive slash commands that need user selection
/// via iocraft's InputSlot (replaces dialoguer FuzzySelect/Select in the
/// iocraft REPL path).
///
/// Returns `Option<SlashSelectionHandler>` to support chained interactions
/// (tree navigation). A struct wrapper breaks the recursive type alias cycle.
struct SlashSelectionHandler(
    Box<
        dyn FnOnce(
            &str,
            &Arc<Mutex<LiveCli>>,
            &repl_ui::OutputSender,
        ) -> Option<SlashSelectionHandler>,
    >,
);

/// Show an interactive selection question via iocraft's InputSlot and
/// register a callback to handle the answer. The coordinator loop routes
/// the `QuestionAnswer` event to the returned handler.
///
/// `resolve_answer` maps the raw answer string (1-indexed option number
/// or custom text) to the value to pass to `on_selected`.
#[inline]
fn show_slash_selection(
    ui: &repl_ui::UiCommandSender,
    question: repl_ui::QuestionPromptView,
    items: Vec<String>,
    on_selected: impl FnOnce(
            String,
            &Arc<Mutex<LiveCli>>,
            &repl_ui::OutputSender,
        ) -> Option<SlashSelectionHandler>
        + 'static,
) -> SlashSelectionHandler {
    ui.show_question(question);
    SlashSelectionHandler(Box::new(move |answer: &str, cli, out| {
        let resolved = answer
            .parse::<usize>()
            .ok()
            .and_then(|idx| items.get(idx.wrapping_sub(1)).cloned())
            .unwrap_or_else(|| answer.to_string());
        on_selected(resolved, cli, out)
    }))
}

struct IocraftQuestionPrompter {
    ui: repl_ui::UiCommandSender,
    pending_answer: PendingQuestionAnswer,
}

impl IocraftQuestionPrompter {
    fn new(ui: repl_ui::UiCommandSender, pending_answer: PendingQuestionAnswer) -> Self {
        Self { ui, pending_answer }
    }

    fn show_field(&self, request: &runtime::QuestionPromptRequest, index: usize) {
        let field = &request.fields[index];
        self.ui.show_question(repl_ui::QuestionPromptView {
            title: request.title.clone(),
            description: request.description.clone(),
            index,
            total: request.fields.len(),
            prompt: field.prompt.clone(),
            options: field
                .options
                .iter()
                .map(|option| repl_ui::QuestionOptionView {
                    label: option.label.clone(),
                    value: option.value.clone(),
                    description: option.description.clone(),
                    recommended: option.recommended,
                    is_navigable: false,
                })
                .collect(),
            allow_custom_input: field.allow_custom_input,
            custom_input_hint: field.custom_input_hint.clone(),
            force_fuzzy_select: false,
            back_value: None,
        });
    }

    fn prepare_answer_receiver(&self) -> Result<mpsc::Receiver<String>, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut pending = self
                .pending_answer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.is_some() {
                return Err("question prompt already pending".to_string());
            }
            *pending = Some(tx);
        }
        Ok(rx)
    }

    fn wait_for_answer(rx: mpsc::Receiver<String>) -> Result<String, String> {
        rx.recv()
            .map(|answer| answer.trim().to_string())
            .map_err(|_| "question prompt cancelled".to_string())
    }

    fn answer_for_field(
        field: &runtime::QuestionField,
        raw_answer: String,
    ) -> runtime::QuestionPromptAnswer {
        let matched = if field.options.is_empty() {
            None
        } else if let Ok(index) = raw_answer.parse::<usize>() {
            index
                .checked_sub(1)
                .and_then(|zero_based| field.options.get(zero_based))
        } else {
            field
                .options
                .iter()
                .find(|option| option.label == raw_answer || option.value == raw_answer)
        };

        runtime::QuestionPromptAnswer {
            id: field.id.clone(),
            value: matched
                .map(|option| option.value.clone())
                .unwrap_or_else(|| raw_answer.clone()),
            label: matched
                .map(|option| option.label.clone())
                .or_else(|| (!raw_answer.is_empty()).then_some(raw_answer)),
        }
    }
}

impl runtime::QuestionPrompter for IocraftQuestionPrompter {
    fn ask(
        &mut self,
        request: &runtime::QuestionPromptRequest,
    ) -> Result<Vec<runtime::QuestionPromptAnswer>, String> {
        let mut answers = Vec::with_capacity(request.fields.len());
        for index in 0..request.fields.len() {
            let rx = self.prepare_answer_receiver()?;
            self.show_field(request, index);
            let raw_answer = match Self::wait_for_answer(rx) {
                Ok(answer) => answer,
                Err(error) => {
                    self.ui.clear_question();
                    return Err(error);
                }
            };
            answers.push(Self::answer_for_field(&request.fields[index], raw_answer));
        }
        self.ui.clear_question();
        Ok(answers)
    }
}

fn run_repl_iocraft_dispatch(
    mut cli: LiveCli,
    mode: input_queue::QueueMode,
) -> Result<(), Box<dyn std::error::Error>> {
    cli.is_repl = true;
    let banner = cli.startup_banner();
    let permission_label = cli.config.permission_mode.as_str().to_string();

    // iocraft owns stdin (raw mode), so:
    // 1. ESC-key abort monitor must NOT compete for stdin.
    // 2. Ignore SIGINT — iocraft delivers Ctrl-C as a key event in raw
    //    mode. Without this, a Ctrl-C arriving before raw mode is entered
    //    (timing race) kills the process.
    cli.esc_monitor_enabled = false;

    let abort_signal = runtime::HookAbortSignal::new();
    cli.persistent_abort_signal = Some(abort_signal.clone());

    let shared_mode = repl_async::shared_queue_mode(mode);
    cli.shared_queue_mode = Some(Arc::clone(&shared_mode));

    // Spawn the iocraft REPL UI on a dedicated thread.
    let repl = repl_ui::spawn_repl_ui(&permission_label, &banner);
    let pending_question_answer: PendingQuestionAnswer = Arc::new(Mutex::new(None));

    // Route LiveCli output through iocraft's OutputSender so it goes
    // through split_for_iocraft and renders correctly in raw mode.
    cli.iocraft_output = Some(repl.output.clone());
    let cli_shared = Arc::new(Mutex::new(cli));
    let session_start = Instant::now();

    // nexus A2A receive-half: when configured, surface peer messages into the
    // REPL as they arrive. The poller runs for the whole interactive session;
    // its daemon thread is reaped by the `process::exit(0)` at the end of this
    // dispatch (the render-loop thread is left the same way), so it needs no
    // explicit shutdown. The session was already dialed in
    // `build_runtime_for_cwd`, so this just reuses the cached handle.
    if let Ok(Some(a2a_session)) = cli::nexus_a2a::session() {
        let output = repl.output.clone();
        let _poller = cli::nexus_a2a::spawn_poller(
            a2a_session,
            runtime::HookAbortSignal::new(),
            move |msg| {
                output.println(&format!("\n\u{1f4e8} A2A from {}: {}", msg.from, msg.body));
            },
        );
    }

    // Coordinator loop on the current thread. Reads InputEvents from the
    // iocraft UI and dispatches turns via the same TurnInputCoordinator +
    // runner-thread pattern as the rustyline-based coordinator.
    let coord = Arc::new(Mutex::new(input_queue::TurnInputCoordinator::new()));
    let (turn_tx, turn_rx) = mpsc::sync_channel::<()>(1);
    let mut turn_active = false;
    let mut runner_handle: Option<thread::JoinHandle<()>> = None;
    // Pending interactive slash command state: when a slash command needs
    // user selection (e.g. `/model` without args), we show a question via
    // iocraft's InputSlot and store the callback here. The coordinator
    // loop routes the QuestionAnswer to this closure instead of the
    // tool-question path.
    let mut pending_slash_selection: Option<SlashSelectionHandler> = None;

    loop {
        // When idle, block on input; when a turn is running, poll both
        // channels with a 100ms timeout.
        let event = if !turn_active {
            match repl.input_rx.recv() {
                Ok(evt) => Some(evt),
                Err(_) => {
                    cancel_pending_question_answer(&pending_question_answer);
                    repl.ui.clear_question();
                    if let Some(h) = runner_handle.take() {
                        abort_signal.abort();
                        let _ = h.join();
                    }
                    break;
                }
            }
        } else {
            match repl.input_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => Some(evt),
                Err(RecvTimeoutError::Timeout) => {
                    // Check if a turn finished.
                    if turn_rx.try_recv().is_ok() {
                        turn_active = false;
                        if let Some(h) = runner_handle.take() {
                            let _ = h.join();
                        }
                        let next = coord.lock().unwrap().drain_next();
                        if let Some(next) = next {
                            turn_active = true;
                            runner_handle = Some(spawn_iocraft_turn(
                                Arc::clone(&cli_shared),
                                &abort_signal,
                                next.prompt,
                                repl.output.clone(),
                                repl.ui.clone(),
                                repl.spinner.clone(),
                                Arc::clone(&pending_question_answer),
                                turn_tx.clone(),
                            ));
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    cancel_pending_question_answer(&pending_question_answer);
                    repl.ui.clear_question();
                    // Render loop exited — clean up runner if active.
                    if let Some(h) = runner_handle.take() {
                        abort_signal.abort();
                        let _ = h.join();
                    }
                    break;
                }
            }
        };

        let Some(event) = event else { continue };

        match event {
            repl_ui::InputEvent::Exit => {
                cancel_pending_question_answer(&pending_question_answer);
                repl.ui.clear_question();
                if let Some(h) = runner_handle.take() {
                    abort_signal.abort();
                    let _ = h.join();
                }
                let cli_lock = cli_shared.lock().expect("LiveCli mutex poisoned");
                if let Err(e) = cli_lock.persist_session() {
                    repl.output
                        .println(&format!("{}{e}{}", ansi_fg(theme().error), RESET));
                }
                break;
            }
            repl_ui::InputEvent::Abort => {
                cancel_pending_question_answer(&pending_question_answer);
                repl.ui.clear_question();
                if runner_handle.is_some() {
                    abort_signal.abort();
                }
            }
            repl_ui::InputEvent::Submit(text) => {
                if text.trim() == "/exit" || text.trim() == "/quit" {
                    cancel_pending_question_answer(&pending_question_answer);
                    repl.ui.clear_question();
                    if runner_handle.is_some() {
                        abort_signal.abort();
                    }
                    if let Some(h) = runner_handle.take() {
                        let _ = h.join();
                    }
                    let cli_lock = cli_shared.lock().expect("LiveCli mutex poisoned");
                    if let Err(e) = cli_lock.persist_session() {
                        repl.output
                            .println(&format!("{}{e}{}", ansi_fg(theme().error), RESET));
                    }
                    break;
                }

                // Try slash command dispatch.
                let trimmed = text.trim();
                let is_slash = match SlashCommand::parse(trimmed) {
                    Ok(Some(SlashCommand::Config { section: None })) => {
                        // Interactive config tree browser via FieldSchema SSOT.
                        let cwd = env::current_dir().unwrap_or_default();
                        let loader = runtime::ConfigLoader::default_for(&cwd);
                        let settings_path = loader.config_home().join("settings.json");
                        let sudocode_path = loader.config_home().join("sudocode.json");
                        pending_slash_selection = Some(cli::config_ui::build_config_tree_handler(
                            &repl.ui,
                            settings_path,
                            sudocode_path,
                        ));
                        true
                    }
                    Ok(Some(SlashCommand::Model { model: None })) => {
                        // Interactive model picker via iocraft InputSlot.
                        let cli_lock = cli_shared.lock().expect("LiveCli mutex poisoned");
                        let sudocode_config = load_sudocode_config_for_current_dir();
                        let config_keys: Vec<String> =
                            sudocode_config.models.keys().cloned().collect();
                        let models = runtime::model_capabilities::merge_discovery_ids(&config_keys);
                        let current = cli_lock.config.model.clone();
                        drop(cli_lock);

                        let options = models
                            .iter()
                            .map(|m| repl_ui::QuestionOptionView {
                                label: m.clone(),
                                value: m.clone(),
                                description: None,
                                recommended: *m == current,
                                is_navigable: false,
                            })
                            .collect();
                        pending_slash_selection = Some(show_slash_selection(
                            &repl.ui,
                            repl_ui::QuestionPromptView {
                                title: Some("Model".to_string()),
                                description: Some(format!("Current: {current}")),
                                index: 0,
                                total: 1,
                                prompt: "Select model".to_string(),
                                options,
                                allow_custom_input: true,
                                custom_input_hint: Some("or type a model name".to_string()),
                                force_fuzzy_select: false,
                                back_value: None,
                            },
                            models,
                            |model_name, cli, out| {
                                let mut cli_lock = cli.lock().expect("LiveCli mutex poisoned");
                                match cli_lock.set_model(Some(model_name)) {
                                    Ok(true) => {
                                        if let Err(e) = cli_lock.persist_session() {
                                            out.println(&format!(
                                                "{}{e}{}",
                                                ansi_fg(theme().error),
                                                RESET
                                            ));
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(e) => out.println(&format!(
                                        "{}{e}{}",
                                        ansi_fg(theme().error),
                                        RESET
                                    )),
                                }
                                None
                            },
                        ));
                        true
                    }
                    Ok(Some(command)) => {
                        let mut cli_lock = cli_shared.lock().expect("LiveCli mutex poisoned");
                        match cli_lock.handle_repl_command(command) {
                            Ok(true) => {
                                if let Err(e) = cli_lock.persist_session() {
                                    repl.output.println(&format!(
                                        "{}{e}{}",
                                        ansi_fg(theme().error),
                                        RESET
                                    ));
                                }
                            }
                            Ok(false) => {}
                            Err(e) => repl.output.println(&format!(
                                "{}{e}{}",
                                ansi_fg(theme().error),
                                RESET
                            )),
                        }
                        true
                    }
                    Ok(None) => false,
                    Err(error) => {
                        repl.output
                            .println(&format!("{}{error}{}", ansi_fg(theme().error), RESET));
                        true
                    }
                };
                if is_slash {
                    continue;
                }

                // Route to turn.
                if !turn_active {
                    let next = coord.lock().unwrap().submit_when_idle(text);
                    turn_active = true;
                    runner_handle = Some(spawn_iocraft_turn(
                        Arc::clone(&cli_shared),
                        &abort_signal,
                        next.prompt,
                        repl.output.clone(),
                        repl.ui.clone(),
                        repl.spinner.clone(),
                        Arc::clone(&pending_question_answer),
                        turn_tx.clone(),
                    ));
                } else {
                    let outcome = coord
                        .lock()
                        .unwrap()
                        .submit_during_turn(text, repl_async::load_queue_mode(&shared_mode));
                    match outcome {
                        input_queue::SubmitOutcome::Queued => {}
                        input_queue::SubmitOutcome::Interrupt => {
                            abort_signal.abort();
                        }
                        input_queue::SubmitOutcome::Rejected => {
                            repl.output.println(
                                &format!("{DIM}(a turn is running; set SUDOCODE_INTERRUPT_QUEUE_MODE=queue to queue instead){RESET}"),
                            );
                        }
                    }
                }
            }
            repl_ui::InputEvent::QuestionAnswer(text) => {
                if let Some(handler) = pending_slash_selection.take() {
                    pending_slash_selection = (handler.0)(&text, &cli_shared, &repl.output);
                } else if !consume_pending_question_answer(&pending_question_answer, text) {
                    repl.output.println(&format!(
                        "{DIM}(no question is waiting for an answer){RESET}"
                    ));
                }
            }
        }
    }

    // The iocraft render loop thread may not exit cleanly on all
    // platforms (Windows PTY). Drop the channels to signal it, then
    // proceed — process exit will clean up the thread.
    // Persist session before dropping the repl — the UI thread may still
    // be alive and we need the mutex accessible.
    {
        let cli_lock = cli_shared.lock().expect("LiveCli mutex");
        let _ = cli_lock.persist_session();
    }
    drop(repl);

    // Unwrap the Arc and finalize telemetry. If the runner thread
    // still holds a clone, force-exit — session is already persisted.
    let cli = match Arc::try_unwrap(cli_shared) {
        Ok(m) => m.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(_) => std::process::exit(0),
    };

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

    // The iocraft render loop thread may still be alive (it blocks on
    // terminal events). Force process exit — all persistent state has
    // already been flushed above.
    std::process::exit(0);
}

/// Spawn a runner thread for the iocraft REPL path. The runner locks
/// `LiveCli`, calls `run_turn`, and sends the result via the output
/// channel. The spinner state is wired so the streaming/tool layers
/// can update it atomically.
fn spawn_iocraft_turn(
    cli_shared: Arc<Mutex<LiveCli>>,
    abort_signal: &runtime::HookAbortSignal,
    prompt: String,
    output: repl_ui::OutputSender,
    ui: repl_ui::UiCommandSender,
    spinner: repl_ui::SpinnerState,
    pending_question_answer: PendingQuestionAnswer,
    done_tx: mpsc::SyncSender<()>,
) -> thread::JoinHandle<()> {
    let abort = abort_signal.clone();
    thread::Builder::new()
        .name("repl-runner".into())
        .spawn(move || {
            abort.reset();
            let mut cli = cli_shared.lock().expect("LiveCli mutex poisoned");
            if let Err(e) =
                cli.run_turn_iocraft(&prompt, &output, &ui, &spinner, pending_question_answer)
            {
                output.println(&format!("{}{e}{}", ansi_fg(theme().error), RESET));
            }
            let _ = done_tx.send(());
        })
        .expect("spawn repl-runner thread")
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
    /// When false, `prepare_turn_runtime` spawns a no-op abort monitor instead
    /// of the ESC-key stdin listener. Set by the async REPL dispatch so the
    /// runner thread does NOT put stdin into raw mode — the input thread's
    /// rustyline is the sole stdin consumer in that mode, and a competing
    /// crossterm listener leaves the terminal wedged after the turn ends
    /// (deadlocked the `/exit` path on POSIX CI runners in PR #298 v1).
    esc_monitor_enabled: bool,
    /// When `Some`, `prepare_turn_runtime` resets and reuses THIS signal
    /// instead of creating a fresh one per turn. Set by the async REPL
    /// dispatch so main can hold a clone and call `.abort()` mid-turn
    /// without ever locking the `LiveCli` mutex — the runner thread holds
    /// that lock while `run_turn` streams.
    persistent_abort_signal: Option<runtime::HookAbortSignal>,
    /// Shared atomic queue mode for the async REPL. `/config set auto-interrupt`
    /// writes to this; the coordinator reads it each `submit_during_turn`.
    shared_queue_mode: Option<repl_async::SharedQueueMode>,
    /// True in REPL mode. Plan mode confirmation dialog only shows in REPL.
    is_repl: bool,
    /// When set, `out_println` routes through this sender instead of bare
    /// `println!`. Set by the iocraft REPL dispatch so slash command output
    /// goes through `split_for_iocraft` and renders correctly in raw mode.
    iocraft_output: Option<repl_ui::OutputSender>,
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
    /// per-session injected MCP servers (from session/new or session/load),
    /// reused when the runtime is rebuilt (e.g. model switch) so they
    /// survive across the session's lifetime.
    session_mcp_servers: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
    /// Caller-supplied system-prompt adjustments (`_meta.sudocode.systemPrompt`
    /// / `appendSystemPrompt` on session/new or session/load). Kept on the
    /// session so a runtime rebuild (model switch) re-applies them.
    prompt_overrides: runtime::SystemPromptOverrides,
}

/// One live ACP session as held by [`AcpCliAgent`].
///
/// The session proper sits behind its **own** mutex so that turns of
/// different sessions run concurrently; the ACP server additionally
/// serializes requests within one session, so this lock is uncontended in
/// practice and exists for memory safety. `cwd` is duplicated here so
/// `session/list` can answer without touching a session that is mid-turn.
struct AcpCliSessionSlot {
    cwd: PathBuf,
    session: Arc<Mutex<AcpCliSession>>,
}

/// Locked view of a session; deref gives `&mut AcpCliSession`.
type AcpCliSessionGuard<'a> = std::sync::MutexGuard<'a, AcpCliSession>;

struct AcpCliAgent {
    /// Process-wide "current model" (`/model`, `session/setModel` move it).
    /// Read at the start of every turn as the fallback for sessions that
    /// carry no model of their own, hence its own short-held lock rather
    /// than living under any session's lock.
    model: Mutex<String>,
    model_flag_raw: Option<String>,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode_override: Option<PermissionMode>,
    reasoning_effort: Option<String>,
    auth_mode: Option<AuthMode>,
    /// Session registry. Only ever locked briefly to look a slot up or to
    /// insert / remove one — never while a session lock is held, and never
    /// across a turn.
    sessions: Mutex<HashMap<String, AcpCliSessionSlot>>,
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
            model: Mutex::new(model),
            model_flag_raw,
            allowed_tools,
            permission_mode_override,
            reasoning_effort,
            auth_mode,
            sessions: Mutex::new(HashMap::new()),
            tokio_runtime: tokio::runtime::Runtime::new()
                .expect("failed to create tokio runtime for ACP agent"),
        }
    }

    /// Snapshot of the process-wide current model.
    fn current_model(&self) -> String {
        self.model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, AcpCliSessionSlot>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Fetch the shared handle of a session (the registry lock is released
    /// before the caller locks the session itself).
    fn session_handle(&self, session_id: &str) -> Result<Arc<Mutex<AcpCliSession>>, AcpError> {
        self.lock_sessions()
            .get(session_id)
            .map(|slot| Arc::clone(&slot.session))
            .ok_or_else(|| AcpError::invalid_params(format!("unknown sessionId: {session_id}")))
    }

    fn insert_session(&self, session_id: String, session: AcpCliSession) {
        let slot = AcpCliSessionSlot {
            cwd: session.cwd.clone(),
            session: Arc::new(Mutex::new(session)),
        };
        self.lock_sessions().insert(session_id, slot);
    }

    fn build_session(
        &self,
        cwd: &Path,
        mcp_servers: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
    ) -> Result<AcpCliSession, AcpError> {
        let cwd = canonical_session_cwd(cwd)?;
        // Config, plugin/MCP state and the API client's `.env` lookup all
        // resolve against the workspace root (see `runtime::workspace_root`).
        let _scope = runtime::WorkspaceRootScope::enter(&cwd);
        let model = self.resolve_model_for_cwd(&cwd)?;
        let permission_mode = self.resolve_permission_mode_for_cwd(&cwd)?;
        let system_prompt = build_acp_system_prompt(&cwd, &prompt_overrides)?;
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
            mcp_servers,
        )
        .map_err(|error| AcpError::internal(format!("failed to build runtime: {error}")))?;
        let abort_signal = runtime::HookAbortSignal::new();
        runtime = runtime.with_hook_abort_signal(abort_signal.clone());
        if let Some(rt) = runtime.runtime.as_mut() {
            rt.api_client_mut()
                .set_reasoning_effort(self.reasoning_effort.clone());
            let thinking = ConfigLoader::default_for(&cwd)
                .load()
                .map_or(true, |cfg| cfg.thinking());
            rt.api_client_mut().set_thinking_enabled(thinking);
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
            session_mcp_servers: mcp_servers.clone(),
            prompt_overrides,
        })
    }

    fn resolve_model_for_cwd(&self, cwd: &Path) -> Result<String, AcpError> {
        let model = self.current_model();
        if self.model_flag_raw.is_some() {
            return Ok(model);
        }
        // `resolve_repl_model` reads project config from the workspace root
        // (see `runtime::workspace_root`), so scope it to this session's cwd.
        let _scope = runtime::WorkspaceRootScope::enter(cwd);
        Ok(resolve_repl_model(model))
    }

    fn resolve_permission_mode_for_cwd(&self, cwd: &Path) -> Result<PermissionMode, AcpError> {
        if let Some(mode) = self.permission_mode_override {
            return Ok(mode);
        }
        let _scope = runtime::WorkspaceRootScope::enter(cwd);
        Ok(default_permission_mode())
    }
}

impl AcpCliAgent {
    /// Switch the process-wide model and rebuild `session`'s runtime for it.
    /// The caller holds the session lock (`session` is the locked session).
    fn handle_acp_model_switch(
        &self,
        session: &mut AcpCliSession,
        model: Option<String>,
    ) -> Result<String, AcpError> {
        // Everything below (model report, alias lookup, config, plugin/MCP
        // state, the API client's `.env` lookup) resolves against the
        // workspace root.
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
        let current = self.current_model();

        let Some(new_model) = model else {
            return Ok(format_model_report(
                &current,
                session.runtime.session().messages.len(),
                UsageTracker::from_session(session.runtime.session()).turns(),
            ));
        };

        let resolved = resolve_model_alias_with_config(&new_model);
        if resolved == current {
            return Ok(format_model_report(
                &current,
                session.runtime.session().messages.len(),
                UsageTracker::from_session(session.runtime.session()).turns(),
            ));
        }

        let previous = current;
        let message_count = session.runtime.session().messages.len();
        let mut cloned_session = session.runtime.session().clone();
        // Keep the session's own model in sync with the switch. `build_runtime_with_plugin_state`
        // only fills `session.model` when it is None (correct for a brand-new session), so without
        // this the resumed/switched session keeps its OLD model — which then drives the wrong
        // context-window in the pre-turn auto-compaction and can wedge the session on overflow.
        cloned_session.model = Some(resolved.clone());
        let cwd = session.cwd.clone();
        let handle_id = session.handle.id.clone();
        let session_mcp = session.session_mcp_servers.clone();

        let sudocode_config = load_sudocode_config_for_cwd(&cwd);
        let permission_mode = self.resolve_permission_mode_for_cwd(&cwd)?;
        let auth_mode = resolve_model_switch_auth_mode(&resolved, self.auth_mode, &sudocode_config)
            .map_err(|e| AcpError::internal(format!("failed to resolve auth mode: {e}")))?;
        let system_prompt = build_acp_system_prompt(&cwd, &session.prompt_overrides)?;
        let mut runtime = build_runtime_for_cwd(
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
            &session_mcp,
        )
        .map_err(|e| AcpError::internal(e.to_string()))?;
        runtime = runtime.with_hook_abort_signal(session.abort_signal.clone());
        if let Some(rt) = runtime.runtime.as_mut() {
            rt.api_client_mut()
                .set_reasoning_effort(self.reasoning_effort.clone());
            let thinking = ConfigLoader::default_for(&cwd)
                .load()
                .map_or(true, |cfg| cfg.thinking());
            rt.api_client_mut().set_thinking_enabled(thinking);
        }

        session.runtime = runtime;
        self.model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone_from(&resolved);

        Ok(format_model_switch_report(
            &previous,
            &resolved,
            message_count,
        ))
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

impl AcpSdkDelegate {
    /// Lock one session for the duration of a delegate call. Different
    /// sessions lock independently, so a session parked on user input never
    /// holds up another one; the ACP server serializes calls on the same
    /// session, so this normally never waits.
    fn lock_session(&self, session_id: &str) -> Result<LockedAcpSession, runtime::AcpError> {
        let handle = self.inner.session_handle(session_id)?;
        Ok(LockedAcpSession { handle })
    }

    /// The working directory of a session, read from the registry slot so
    /// it never waits on the session's own lock.
    fn session_cwd(&self, session_id: &str) -> Result<PathBuf, runtime::AcpError> {
        self.inner
            .lock_sessions()
            .get(session_id)
            .map(|slot| slot.cwd.clone())
            .ok_or_else(|| {
                runtime::AcpError::invalid_params(format!("unknown sessionId: {session_id}"))
            })
    }
}

/// Owner of a session handle that hands out the locked session. Kept as a
/// separate value so the `Arc` outlives the guard borrowed from it.
struct LockedAcpSession {
    handle: Arc<Mutex<AcpCliSession>>,
}

impl LockedAcpSession {
    fn get(&self) -> AcpCliSessionGuard<'_> {
        self.handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl runtime::acp_sdk_server::SdkAcpDelegate for AcpSdkDelegate {
    fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
    ) -> Result<(String, PathBuf, runtime::HookAbortSignal), runtime::AcpError> {
        let session = self
            .inner
            .build_session(&cwd, &mcp_servers, prompt_overrides)?;
        let session_id = session.handle.id.clone();
        let session_cwd = session.cwd.clone();
        let abort_signal = session.abort_signal.clone();
        self.inner.insert_session(session_id.clone(), session);
        Ok((session_id, session_cwd, abort_signal))
    }

    fn run_prompt(
        &self,
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
        let locked = self.lock_session(session_id)?;
        let mut session = locked.get();
        self.run_prompt_impl(&mut session, prompt, observer, None, trace_id)
    }

    fn run_prompt_with_prompter(
        &self,
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
        let locked = self.lock_session(session_id)?;
        let mut session = locked.get();
        self.run_prompt_impl(&mut session, prompt, observer, Some(prompter), trace_id)
    }

    fn set_question_prompter(
        &self,
        session_id: &str,
        prompter: Box<dyn runtime::QuestionPrompter>,
    ) -> Result<(), runtime::AcpError> {
        let locked = self.lock_session(session_id)?;
        locked
            .get()
            .runtime
            .tool_executor_mut()
            .set_question_prompter(prompter);
        Ok(())
    }

    fn handle_slash_command(
        &self,
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
                let locked = self.lock_session(session_id)?;
                let mut session = locked.get();
                self.inner
                    .handle_acp_model_switch(&mut session, model.clone())?
            }
            SlashCommand::Help => render_repl_help(),
            SlashCommand::Status => {
                let locked = self.lock_session(session_id)?;
                let session = locked.get();
                let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
                let tracker = UsageTracker::from_session(session.runtime.session());
                format_status_report(
                    &self.inner.current_model(),
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
                let locked = self.lock_session(session_id)?;
                let session = locked.get();
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
            SlashCommand::Config { section } => {
                let _scope = runtime::WorkspaceRootScope::enter(self.session_cwd(session_id)?);
                render_config_report(section.as_deref())
                    .map_err(|e| runtime::AcpError::internal(e.to_string()))?
            }
            SlashCommand::ConfigSet { .. } => {
                "/config set is only available in interactive REPL mode".to_string()
            }
            SlashCommand::Diff => {
                let cwd = self.session_cwd(session_id)?;
                let output = std::process::Command::new("git")
                    .args(["diff", "--cached", "--no-color"])
                    .current_dir(&cwd)
                    .output()
                    .map_err(|e| runtime::AcpError::internal(e.to_string()))?;
                let cached = String::from_utf8_lossy(&output.stdout);
                let output2 = std::process::Command::new("git")
                    .args(["diff", "--no-color"])
                    .current_dir(&cwd)
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
            SlashCommand::Doctor => {
                let _scope = runtime::WorkspaceRootScope::enter(self.session_cwd(session_id)?);
                render_doctor_report()
                    .map(|report| report.render())
                    .map_err(|e| runtime::AcpError::internal(e.to_string()))?
            }
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
            .lock_sessions()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.cwd.clone()))
            .collect()
    }

    fn close_session(&self, session_id: &str) -> bool {
        // Unregister first (new requests see `unknown sessionId` right away),
        // then wait for the session itself: a turn still running on it keeps
        // the session alive through its `Arc` until it returns.
        let Some(slot) = self.inner.lock_sessions().remove(session_id) else {
            return false;
        };
        {
            let session = slot
                .session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        }
        drop(slot);
        true
    }

    fn set_model(&self, session_id: &str, model_id: &str) -> Result<String, runtime::AcpError> {
        let locked = self.lock_session(session_id)?;
        let mut session = locked.get();
        self.inner
            .handle_acp_model_switch(&mut session, Some(model_id.to_string()))
    }

    fn get_model_info(&self) -> (String, Vec<String>) {
        let current = self.inner.current_model();
        let config = load_sudocode_config_for_current_dir();
        let config_keys: Vec<String> = config.models.keys().cloned().collect();
        let mut models = runtime::model_capabilities::merge_discovery_ids(&config_keys);
        // Ensure the current model is always present.
        if !models.iter().any(|m| m.eq_ignore_ascii_case(&current)) {
            models.insert(0, current.clone());
        }
        (current, models)
    }

    fn set_permission_mode(
        &self,
        session_id: &str,
        mode: PermissionMode,
    ) -> Result<(), runtime::AcpError> {
        let locked = self.lock_session(session_id)?;
        let mut session = locked.get();
        if let Some(rt) = session.runtime.runtime.as_mut() {
            rt.permission_policy_mut().set_active_mode(mode);
        }
        Ok(())
    }

    fn push_images(
        &self,
        session_id: &str,
        images: &[(String, String)],
    ) -> Result<(), runtime::AcpError> {
        eprintln!(
            "[push_images] entered — session={session_id}, {} images",
            images.len()
        );
        // Resolve everything that needs runtime-level state BEFORE taking the
        // session lock: the active model + sudorouter creds.
        let active_model = self.inner.current_model();
        let active_model_is_vision_capable =
            runtime::model_capabilities::vision_capable(&active_model);
        eprintln!(
            "[push_images] active_model={active_model:?} vision_capable={active_model_is_vision_capable}"
        );
        let sudocode_config = load_sudocode_config_for_cwd(&self.session_cwd(session_id)?);
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

        // Single critical section: lock the session and push all messages.
        let locked = self.lock_session(session_id)?;
        let mut session = locked.get();
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
        &self,
        session_id: &str,
        cwd: PathBuf,
        mcp_servers: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
    ) -> Result<(String, PathBuf, runtime::HookAbortSignal), runtime::AcpError> {
        let cwd = canonical_session_cwd(&cwd)?;
        // The session store, config and system prompt all resolve against
        // the workspace root; scope this thread to the requested cwd.
        let _scope = runtime::WorkspaceRootScope::enter(&cwd);

        let (handle, session) = load_session_reference(session_id)
            .map_err(|e| runtime::AcpError::internal(format!("failed to load session: {e}")))?;

        let model = self.inner.resolve_model_for_cwd(&cwd)?;
        let permission_mode = self.inner.resolve_permission_mode_for_cwd(&cwd)?;
        let system_prompt = build_acp_system_prompt(&cwd, &prompt_overrides)?;
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
            &mcp_servers,
        )
        .map_err(|e| runtime::AcpError::internal(format!("failed to build runtime: {e}")))?;

        let abort_signal = runtime::HookAbortSignal::new();
        runtime = runtime.with_hook_abort_signal(abort_signal.clone());
        if let Some(rt) = runtime.runtime.as_mut() {
            rt.api_client_mut()
                .set_reasoning_effort(self.inner.reasoning_effort.clone());
            let thinking = ConfigLoader::default_for(&cwd)
                .load()
                .map_or(true, |cfg| cfg.thinking());
            rt.api_client_mut().set_thinking_enabled(thinking);
        }

        let loaded_session_id = handle.id.clone();
        let signal = abort_signal.clone();
        self.inner.insert_session(
            loaded_session_id.clone(),
            AcpCliSession {
                cwd: cwd.clone(),
                handle,
                runtime,
                abort_signal,
                started_at: Instant::now(),
                session_mcp_servers: mcp_servers,
                prompt_overrides,
            },
        );
        Ok((loaded_session_id, cwd, signal))
    }
}

impl AcpSdkDelegate {
    /// Run one turn on an already-locked session.
    fn run_prompt_impl(
        &self,
        session: &mut AcpCliSession,
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
        // Reset abort signal for this new turn.
        session.abort_signal.reset();
        // The whole turn — tool loop, hooks, config, session store — resolves
        // paths against this session's workspace root through
        // `runtime::workspace_root`, never the process cwd. That is what lets
        // turns of sessions in different directories run concurrently in
        // one process (the ACP server also enters this scope around the
        // blocking closure; nesting is harmless and keeps `run_prompt`
        // correct for callers that do not).
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);

        // Set trace_id on the runtime if provided
        if let Some(tid) = trace_id {
            session.runtime.set_trace_id(tid);
        }

        // Pre-send token estimation and auto-compact logic
        let fallback_model = self.inner.current_model();
        let model = session
            .runtime
            .session()
            .model
            .as_ref()
            .unwrap_or(&fallback_model);
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
                let result = compact_session_sync(session.runtime.session(), compaction_config);
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

/// Parse an on/off toggle value. Accepts `on|true|1` and `off|false|0`
/// (case-insensitive). Returns `None` for unrecognized input.
fn parse_on_off(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
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

            let is_tty = io::stdin().is_terminal();

            // On Unix, bypass crossterm for raw mode and key detection.
            // Crossterm's lazy global `InternalEventSource` becomes stale
            // after rustyline (which also uses crossterm internally) toggles
            // raw mode on the main thread between REPL prompts — causing
            // `event::poll` to miss keypresses in subsequent turns.
            // Reading raw bytes via termios + `poll(2)` is immune to this.
            #[cfg(unix)]
            let raw_enabled = is_tty && enable_raw_mode_unix();

            #[cfg(not(unix))]
            let raw_enabled = is_tty && crossterm::terminal::enable_raw_mode().is_ok();

            runtime.block_on(async move {
                let esc_abort = abort_signal.clone();
                let wait_for_esc_or_stop = tokio::task::spawn_blocking(move || loop {
                    if stop_rx.try_recv().is_ok() {
                        return;
                    }
                    if !raw_enabled {
                        match stop_rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                            Err(RecvTimeoutError::Timeout) => continue,
                        }
                    }
                    match poll_abort_key(Duration::from_millis(50)) {
                        AbortKey::None => {}
                        AbortKey::Esc => {
                            esc_abort.abort();
                            return;
                        }
                        AbortKey::CtrlC => {
                            if cancel::is_double_ctrlc() {
                                #[cfg(unix)]
                                disable_raw_mode_unix();
                                #[cfg(not(unix))]
                                let _ = crossterm::terminal::disable_raw_mode();
                                eprintln!();
                                std::process::exit(0);
                            }
                            cancel::record_ctrlc();
                            esc_abort.abort();
                            return;
                        }
                    }
                });

                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            if cancel::is_double_ctrlc() {
                                #[cfg(unix)]
                                disable_raw_mode_unix();
                                #[cfg(not(unix))]
                                let _ = crossterm::terminal::disable_raw_mode();
                                eprintln!();
                                std::process::exit(0);
                            }
                            cancel::record_ctrlc();
                            abort_signal.abort();
                        }
                    }
                    _ = wait_for_esc_or_stop => {}
                }
            });

            #[cfg(unix)]
            if raw_enabled {
                disable_raw_mode_unix();
            }

            #[cfg(not(unix))]
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
            // Timed join: the monitor thread should exit within 100ms
            // after receiving the stop signal. If it hangs (e.g. tokio
            // signal handler keeping the runtime alive), abandon it.
            let deadline = std::time::Instant::now() + Duration::from_millis(200);
            while !join_handle.is_finished() {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = join_handle.join();
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Raw-mode helpers for `HookAbortMonitor`
//
// On Unix, we manage raw mode via `nix` (safe termios wrappers) instead
// of crossterm to avoid the stale-event-source bug described in
// `HookAbortMonitor::spawn`. On Windows, crossterm is used unchanged.
// ──────────────────────────────────────────────────────────────────────

// Thread-local storage for the original termios settings saved by
// `enable_raw_mode_unix`. Each monitor thread saves its own copy so
// concurrent monitors (hypothetical) don't clobber each other.
#[cfg(unix)]
std::thread_local! {
    static ORIGINAL_TERMIOS: std::cell::RefCell<Option<nix::sys::termios::Termios>> =
        const { std::cell::RefCell::new(None) };
}

/// Enable raw mode via `nix::sys::termios` — no crossterm involvement.
/// Returns `true` on success. Call `disable_raw_mode_unix()` to restore.
#[cfg(unix)]
fn enable_raw_mode_unix() -> bool {
    use nix::sys::termios::{self, SetArg, SpecialCharacterIndices};
    use std::os::fd::AsFd;

    let stdin = std::io::stdin();
    let Ok(original) = termios::tcgetattr(&stdin) else {
        return false;
    };
    ORIGINAL_TERMIOS.with(|cell| *cell.borrow_mut() = Some(original.clone()));

    let mut raw = original;
    termios::cfmakeraw(&mut raw);
    // Non-blocking: VMIN=0, VTIME=0 means read() returns immediately
    // with 0 bytes if nothing is available — poll() handles the wait.
    raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
    termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).is_ok()
}

/// Restore original terminal settings saved by `enable_raw_mode_unix`.
#[cfg(unix)]
fn disable_raw_mode_unix() {
    use nix::sys::termios::{self, SetArg};
    use std::os::fd::AsFd;

    let stdin = std::io::stdin();
    ORIGINAL_TERMIOS.with(|cell| {
        if let Some(original) = cell.borrow().as_ref() {
            let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, original);
        }
    });
}

/// Which abort key was detected by `poll_abort_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortKey {
    /// No abort key pressed within the poll window.
    None,
    /// ESC (0x1b) — cancels the current turn but never triggers exit.
    Esc,
    /// Ctrl-C (0x03) — cancels the current turn; double-press within
    /// 800ms exits the process (CC parity).
    CtrlC,
}

/// Poll stdin for an abort key (ESC = 0x1b, Ctrl-C = 0x03).
#[cfg(unix)]
fn poll_abort_key(timeout: Duration) -> AbortKey {
    use nix::poll::{self, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;
    use std::os::unix::io::AsRawFd;

    let stdin = std::io::stdin();
    let poll_timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::from(50u16));
    let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
    let ready = poll::poll(&mut fds, poll_timeout).unwrap_or(0);
    if ready <= 0 {
        return AbortKey::None;
    }
    let revents = fds[0].revents().unwrap_or(PollFlags::empty());
    if !revents.contains(PollFlags::POLLIN) {
        return AbortKey::None;
    }
    let mut buf = [0u8; 1];
    match nix::unistd::read(stdin.as_raw_fd(), &mut buf) {
        Ok(1) if buf[0] == 0x03 => AbortKey::CtrlC,
        Ok(1) if buf[0] == 0x1b => AbortKey::Esc,
        _ => AbortKey::None,
    }
}

/// Poll stdin for an abort key using crossterm's event system (Windows).
#[cfg(not(unix))]
fn poll_abort_key(timeout: Duration) -> AbortKey {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    if !event::poll(timeout).unwrap_or(false) {
        return AbortKey::None;
    }
    if let Ok(Event::Key(key)) = event::read() {
        if key.kind != KeyEventKind::Press {
            return AbortKey::None;
        }
        if key.code == KeyCode::Esc {
            return AbortKey::Esc;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(event::KeyModifiers::CONTROL) {
            return AbortKey::CtrlC;
        }
    }
    AbortKey::None
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
    /// True when the async REPL (queue mode) is active. In this mode the
    /// input thread owns stdin via rustyline, so interactive widgets
    /// (FuzzySelect, Select) cannot be used on the runner thread.
    fn is_async_mode(&self) -> bool {
        self.persistent_abort_signal.is_some()
    }

    fn out_println(&self, msg: impl AsRef<str>) {
        if let Some(ref out) = self.iocraft_output {
            out.println(msg.as_ref());
        } else {
            println!("{}", msg.as_ref());
        }
    }

    fn out_suspend<F: FnOnce() -> R, R>(&self, f: F) -> R {
        f()
    }

    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        auth_mode: Option<AuthMode>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let system_prompt = build_system_prompt()?;
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

        let mut cli = Self {
            config,
            runtime,
            session,
            prompt_history: Vec::new(),
            undone_tool_use_ids: std::collections::HashSet::new(),
            tokio_runtime,
            esc_monitor_enabled: true,
            persistent_abort_signal: None,
            shared_queue_mode: None,
            is_repl: false,
            iocraft_output: None,
        };

        // Apply thinking config from settings.json (default: enabled).
        let thinking_enabled = ConfigLoader::default_for(&cwd)
            .load()
            .map_or(true, |cfg| cfg.thinking());
        cli.set_thinking_enabled(thinking_enabled);

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

    fn set_thinking_enabled(&mut self, enabled: bool) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_thinking_enabled(enabled);
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

        let t = theme();
        let logo_fg = ansi_fg(t.logo);
        let accent_fg = ansi_fg(t.logo_accent);
        let logo = format!(
            "{logo_fg}\
███████╗██╗   ██╗██████╗  ██████╗ \n\
██╔════╝██║   ██║██╔══██╗██╔═══██╗\n\
███████╗██║   ██║██║  ██║██║   ██║\n\
╚════██║██║   ██║██║  ██║██║   ██║\n\
███████║╚██████╔╝██████╔╝╚██████╔╝\n\
╚══════╝ ╚═════╝ ╚═════╝  ╚═════╝{RESET} {accent_fg}Code{RESET}"
        );

        let lines = [
            format!("  {DIM}Model{RESET}            {}", self.config.model),
            format!("  {DIM}Auth mode{RESET}        {}", auth_mode_str),
            format!("  {DIM}Endpoint{RESET}         {}", endpoint),
            format!(
                "  {DIM}Permissions{RESET}      {}",
                self.config.permission_mode.as_str()
            ),
            format!("  {DIM}Branch{RESET}           {}", git_branch),
            format!("  {DIM}Workspace{RESET}        {}", workspace),
            format!("  {DIM}Directory{RESET}        {}", cwd),
            format!("  {DIM}Session{RESET}          {}", self.session.id),
            format!("  {DIM}Auto-save{RESET}        {}", session_path),
        ];

        let max_width = lines.iter().map(|l| strip_ansi_width(l)).max().unwrap_or(0);
        let box_width = max_width + 2; // 1 space padding on each side

        let grey = t.border_fg();
        let reset = RESET;

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

        format!(
            "{}\n\n{}\n{}\n{}",
            logo,
            top,
            boxed_lines.join("\n"),
            bottom,
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
        // Async REPL mode installs a persistent abort signal so main can call
        // `.abort()` mid-turn without racing the runner thread. Reset the flag
        // here — the previous turn may have aborted it (that's how we got
        // this new turn scheduled), and the runtime treats `is_aborted() ==
        // true` as "cancel immediately", which would collapse the fresh turn.
        let hook_abort_signal = match &self.persistent_abort_signal {
            Some(sig) => {
                sig.reset();
                sig.clone()
            }
            None => runtime::HookAbortSignal::new(),
        };
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
        let hook_abort_monitor = if self.esc_monitor_enabled {
            HookAbortMonitor::spawn(hook_abort_signal)
        } else {
            // Async REPL mode: skip the ESC-key stdin listener. The input
            // thread's rustyline is the only stdin consumer; a competing
            // crossterm listener wedges the terminal on POSIX (raw mode is
            // process-wide via termios). Waiter is a plain sleep loop that
            // just observes the stop channel — no stdin touched.
            HookAbortMonitor::spawn_with_waiter(hook_abort_signal, |stop_rx, _abort| loop {
                match stop_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => continue,
                }
            })
        };

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
        let token_budget = crate::render::parse_token_budget(input);
        let mut spinner = SpinnerHandle::new(
            "🦀 Thinking...",
            Some(self.config.model.as_str()),
            TerminalRenderer::new().color_theme(),
            token_budget,
        );
        let spinner_ref = spinner.spinner_ref();
        runtime.api_client_mut().set_spinner(spinner_ref.clone());
        runtime.tool_executor_mut().set_spinner(spinner_ref);
        runtime.tool_executor_mut().set_repl_mode(self.is_repl);

        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let result = self.tokio_runtime.block_on(runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            None,
        ));
        hook_abort_monitor.stop();
        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                if summary.cancelled {
                    spinner.fail("⏹ Cancelled");
                } else {
                    spinner.clear();
                    if let Some(event) = summary.auto_compaction {
                        self.out_println(format_auto_compaction_notice(
                            event.removed_message_count,
                        ));
                    }
                    let elapsed = turn_start.elapsed();
                    let usage = self.runtime.usage().current_turn_usage();
                    let cumulative = self.runtime.usage().cumulative_usage();
                    let turns = self.runtime.usage().turns();
                    let model_for_caps = summary
                        .response_model
                        .as_deref()
                        .unwrap_or(&self.config.model);
                    let context_window =
                        runtime::model_capabilities::context_window_or_default(model_for_caps);
                    let branch = env::current_dir()
                        .ok()
                        .and_then(|cwd| resolve_git_branch_for(&cwd));
                    self.out_println(format_turn_status_line_with_branch(
                        &self.config.model,
                        turns,
                        &usage,
                        Some(&cumulative),
                        Some(context_window),
                        elapsed,
                        branch.as_deref(),
                    ));
                }
                self.persist_session()?;
                // If the plan confirmation dialog chose "clear context &
                // execute", pick up the plan and re-run in a fresh session.
                if let Some(plan) = take_pending_plan_execution() {
                    let session = runtime::Session::new();
                    let session_id = self.session.id.clone();
                    let fresh =
                        self.build_replacement_runtime(session, session_id, self.config.clone())?;
                    self.replace_runtime(fresh)?;
                    let prompt = format!("Implement the following plan:\n\n{plan}");
                    return self.run_turn(&prompt);
                }
                Ok(())
            }
            Err(error) => {
                clear_pending_plan_execution();
                runtime.shutdown_mcp()?;
                runtime.shutdown_plugins()?;
                spinner.fail("❌ Request failed");
                Err(Box::new(error))
            }
        }
    }

    /// Run a turn using the iocraft REPL path. Output is routed through
    /// `OutputSender` and the spinner is managed via the shared
    /// `SpinnerState` atomics instead of indicatif.
    fn run_turn_iocraft(
        &mut self,
        input: &str,
        output: &repl_ui::OutputSender,
        ui: &repl_ui::UiCommandSender,
        spinner_state: &repl_ui::SpinnerState,
        pending_question_answer: PendingQuestionAnswer,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let turn_start = Instant::now();
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true)?;
        let token_budget = crate::render::parse_token_budget(input);

        // Activate the shared spinner state for the iocraft render loop.
        spinner_state.start_turn(
            "\u{1f980} Thinking...",
            Some(self.config.model.as_str()),
            token_budget,
        );

        // Wire the spinner state into the SpinnerRef API so the streaming
        // and tool layers can update bytes + thinking flags atomically.
        let spinner_ref = render::SpinnerRef::from_spinner_state(spinner_state);
        runtime.api_client_mut().set_spinner(spinner_ref.clone());
        runtime.api_client_mut().set_output_writer(output.clone());
        runtime.tool_executor_mut().set_spinner(spinner_ref);
        runtime
            .tool_executor_mut()
            .set_output_writer(output.clone());
        runtime.tool_executor_mut().set_repl_mode(self.is_repl);
        runtime.tool_executor_mut().set_ui_sender(ui.clone());
        runtime
            .tool_executor_mut()
            .set_question_prompter(Box::new(IocraftQuestionPrompter::new(
                ui.clone(),
                pending_question_answer,
            )));

        let mut permission_prompter = CliPermissionPrompter::new(self.config.permission_mode);
        let result = self.tokio_runtime.block_on(runtime.run_turn(
            input,
            Some(&mut permission_prompter),
            None,
        ));
        hook_abort_monitor.stop();
        spinner_state.stop_turn();

        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                if summary.cancelled {
                    output.println(&format!(
                        "{}\u{23f9} Cancelled{}",
                        ansi_fg(theme().error),
                        RESET
                    ));
                } else {
                    if let Some(event) = summary.auto_compaction {
                        output.println(&format_auto_compaction_notice(event.removed_message_count));
                    }
                    let elapsed = turn_start.elapsed();
                    let usage = self.runtime.usage().current_turn_usage();
                    let cumulative = self.runtime.usage().cumulative_usage();
                    let turns = self.runtime.usage().turns();
                    let model_for_caps = summary
                        .response_model
                        .as_deref()
                        .unwrap_or(&self.config.model);
                    let context_window =
                        runtime::model_capabilities::context_window_or_default(model_for_caps);
                    let branch = env::current_dir()
                        .ok()
                        .and_then(|cwd| resolve_git_branch_for(&cwd));
                    ui.set_turn_result(&format_turn_status_line_with_branch(
                        &self.config.model,
                        turns,
                        &usage,
                        Some(&cumulative),
                        Some(context_window),
                        elapsed,
                        branch.as_deref(),
                    ));
                }
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                runtime.shutdown_mcp()?;
                runtime.shutdown_plugins()?;
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
        let result = if io::stdin().is_terminal() {
            let mut prompter = CliPermissionPrompter::new(self.config.permission_mode);
            self.tokio_runtime
                .block_on(runtime.run_turn(input, Some(&mut prompter), None))
        } else {
            let mut prompter = AutoDenyPermissionPrompter;
            self.tokio_runtime
                .block_on(runtime.run_turn(input, Some(&mut prompter), None))
        };
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        let final_text = final_assistant_text(&summary);
        self.out_println(final_text);
        Ok(())
    }

    fn run_prompt_compact_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false)?;
        let result = if io::stdin().is_terminal() {
            let mut prompter = CliPermissionPrompter::new(self.config.permission_mode);
            self.tokio_runtime
                .block_on(runtime.run_turn(input, Some(&mut prompter), None))
        } else {
            let mut prompter = AutoDenyPermissionPrompter;
            self.tokio_runtime
                .block_on(runtime.run_turn(input, Some(&mut prompter), None))
        };
        hook_abort_monitor.stop();
        let summary = result?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        self.out_println(
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
            .to_string(),
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
        self.out_println(
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
            .to_string(),
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
                self.out_println(render_repl_help());
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
                self.run_teleport(target.as_deref())?;
                false
            }
            SlashCommand::DebugToolCall => {
                self.run_debug_tool_call(None)?;
                false
            }
            SlashCommand::Sandbox => {
                self.print_sandbox_status();
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
            SlashCommand::Resume { session_path } => {
                let resumed = self.load_session(session_path)?;
                if resumed {
                    self.out_println(format_resume_report(
                        &self.session.path.display().to_string(),
                        self.runtime.session().messages.len(),
                        self.runtime.usage().turns(),
                    ));
                }
                resumed
            }
            SlashCommand::Config { section } => {
                let report = render_config_report(section.as_deref())?;
                self.out_println(report);
                false
            }
            SlashCommand::ConfigSet { key, value } => {
                self.handle_config_set(&key, &value)?;
                false
            }
            SlashCommand::Mcp { action, target } => {
                match action.as_deref() {
                    Some("reconnect") | Some("enable") | Some("disable") => {
                        let action_str = action.as_deref().unwrap();
                        let Some(server_name) = target.as_deref() else {
                            self.out_println(format!("usage: /mcp {action_str} <server>"));
                            return Ok(false);
                        };
                        if let Some(mcp_state) = &self.runtime.mcp_state {
                            let mut mcp = mcp_state.lock().unwrap_or_else(|e| e.into_inner());
                            let result = match action_str {
                                "reconnect" => mcp.reconnect_server(server_name),
                                "enable" => mcp.enable_server(server_name),
                                "disable" => mcp.disable_server(server_name),
                                _ => unreachable!(),
                            };
                            match result {
                                Ok(msg) => self.out_println(msg),
                                Err(err) => self.out_println(format!("Error: {err}")),
                            }
                        } else {
                            self.out_println(
                                "No MCP servers are running in this session.\n\
                                 Hint: if you just added a server via `/mcp add-json`, \
                                 restart scode to load it.",
                            );
                        }
                    }
                    _ => {
                        let args = match (action.as_deref(), target.as_deref()) {
                            (None, None) => None,
                            (Some(action), None) => Some(action.to_string()),
                            (Some(action), Some(target)) => Some(format!("{action} {target}")),
                            (None, Some(target)) => Some(target.to_string()),
                        };
                        self.out_suspend(|| {
                            Self::print_mcp(args.as_deref(), CliOutputFormat::Text)
                        })?;
                    }
                }
                false
            }
            SlashCommand::Memory => {
                self.edit_memory()?;
                false
            }
            SlashCommand::Init => {
                self.out_suspend(|| run_init(CliOutputFormat::Text))?;
                false
            }
            SlashCommand::Diff => {
                self.out_suspend(|| Self::print_diff())?;
                false
            }
            SlashCommand::Undo => {
                self.handle_undo();
                false
            }
            SlashCommand::Version => {
                self.out_suspend(|| Self::print_version(CliOutputFormat::Text));
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
                self.out_suspend(|| Self::print_agents(args.as_deref(), CliOutputFormat::Text))?;
                false
            }
            SlashCommand::Cron { args } => {
                match cli::cron::run_slash(args.as_deref()) {
                    Ok(text) => self.out_println(text),
                    Err(e) => self.out_println(format!("cron error: {e}")),
                }
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
                        self.out_suspend(|| {
                            self.print_skills_with_plugins(args.as_deref(), CliOutputFormat::Text)
                        })?;
                    }
                }
                false
            }
            SlashCommand::Doctor => {
                self.out_println(render_doctor_report()?.render());
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Stats => {
                let usage = UsageTracker::from_session(self.runtime.session()).cumulative_usage();
                self.out_println(format_cost_report(usage));
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
        self.out_suspend(|| print_with_pager(&report));
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
        self.out_println(render_prompt_history_report(&entries, limit));
    }

    fn print_sandbox_status(&self) {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        self.out_println(format_sandbox_report(&resolve_sandbox_status(
            runtime_config.sandbox(),
            &cwd,
        )));
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            let sudocode_config = load_sudocode_config_for_current_dir();
            let config_keys: Vec<String> = sudocode_config.models.keys().cloned().collect();
            let models = runtime::model_capabilities::merge_discovery_ids(&config_keys);
            let default_idx = models
                .iter()
                .position(|m| *m == self.config.model)
                .unwrap_or(0);
            let selection = self.out_suspend(|| {
                FuzzySelect::new()
                    .with_prompt("Select model (type to filter)")
                    .items(&models)
                    .default(default_idx)
                    .interact_opt()
            })?;
            return match selection {
                Some(idx) => self.set_model(Some(models[idx].clone())),
                None => Ok(false),
            };
        };

        let model = resolve_model_alias_with_config(&model);

        if model == self.config.model {
            self.out_println(format_model_report(
                &self.config.model,
                self.runtime.session().messages.len(),
                self.runtime.usage().turns(),
            ));
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
        let cwd = env::current_dir().unwrap_or_default();
        let system_prompt = build_system_prompt_for(&cwd)?;
        let runtime = self.build_replacement_runtime(
            session,
            session_id,
            RuntimeConfig {
                model: model.clone(),
                system_prompt,
                ..self.config.clone()
            },
        )?;
        self.replace_runtime(runtime)?;
        self.config.model.clone_from(&model);
        self.out_println(format_model_switch_report(&previous, &model, message_count));
        Ok(true)
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            self.out_println(format_permissions_report(
                self.config.permission_mode.as_str(),
            ));
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if normalized == self.config.permission_mode.as_str() {
            self.out_println(format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.config.permission_mode.as_str().to_string();
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        self.config.permission_mode = permission_mode_from_label(normalized);
        let runtime = self.build_replacement_runtime(session, session_id, self.config.clone())?;
        self.replace_runtime(runtime)?;
        self.out_println(format_permissions_switch_report(&previous, normalized));
        Ok(true)
    }

    fn set_auth(&mut self, mode: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let current_str = self.config.auth_mode.as_str().to_string();

        let Some(mode) = mode else {
            self.out_println(format_auth_report(&current_str));
            return Ok(false);
        };

        let parsed = AuthMode::parse(&mode)?;

        if parsed.as_str() == current_str {
            self.out_println(format_auth_report(&current_str));
            return Ok(false);
        }

        let previous = current_str;
        let session = self.runtime.session().clone();
        let session_id = self.session.id.clone();
        self.config.auth_mode = parsed;
        let runtime = self.build_replacement_runtime(session, session_id, self.config.clone())?;
        self.replace_runtime(runtime)?;
        self.out_println(format_auth_switch_report(&previous, parsed.as_str()));
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            self.out_println(
                "clear: confirmation required; run /clear --confirm to start a fresh session.",
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
        self.out_println(format!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.config.model,
            self.config.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        ));
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        self.out_println(format_cost_report(cumulative));
    }

    /// Load a session by reference (id, path, or "latest"), replacing the
    /// current runtime. Pure data operation — no terminal output.
    /// Callers decide how to report the result.
    fn load_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            let sessions = list_managed_sessions()?;
            if sessions.is_empty() {
                self.out_println("No sessions found.");
                return Ok(false);
            }
            let labels: Vec<String> = sessions
                .iter()
                .map(|s| format!("{} ({} msgs)", s.id, s.message_count))
                .collect();
            let selection = self.out_suspend(|| {
                Select::new()
                    .with_prompt("Select session to resume")
                    .items(&labels)
                    .default(0)
                    .interact_opt()
            })?;
            return match selection {
                Some(idx) => self.load_session(Some(sessions[idx].id.clone())),
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
        Ok(true)
    }

    /// Replace the current session with a pre-loaded one (for `--resume`).
    fn replace_with_session(
        &mut self,
        session: runtime::Session,
        handle: SessionHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let session_id = session.session_id.clone();
        let runtime =
            self.build_replacement_runtime(session, handle.id.clone(), self.config.clone())?;
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        Ok(())
    }

    fn handle_config_set(
        &mut self,
        key: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match key {
            "auto-interrupt" | "autoInterrupt" => {
                let Some(on) = parse_on_off(value) else {
                    eprintln!("Usage: /config set auto-interrupt on|off");
                    return Ok(());
                };
                if let Some(shared) = &self.shared_queue_mode {
                    use std::sync::atomic::Ordering;
                    let current = input_queue::QueueMode::from_u8(shared.load(Ordering::Relaxed));
                    let new_mode = if on {
                        if current.queue_enabled() {
                            input_queue::QueueMode::Both
                        } else {
                            input_queue::QueueMode::Interrupt
                        }
                    } else if current.queue_enabled() {
                        input_queue::QueueMode::Queue
                    } else {
                        input_queue::QueueMode::Off
                    };
                    shared.store(new_mode.to_u8(), Ordering::Relaxed);
                    self.out_println(format!(
                        "{DIM}auto-interrupt: {}{RESET}",
                        if on { "on" } else { "off" }
                    ));
                } else {
                    eprintln!("auto-interrupt is only available in async REPL mode");
                }
                Ok(())
            }
            "queue" | "messageQueue" => {
                let Some(on) = parse_on_off(value) else {
                    eprintln!("Usage: /config set queue on|off");
                    return Ok(());
                };
                if let Some(shared) = &self.shared_queue_mode {
                    use std::sync::atomic::Ordering;
                    let current = input_queue::QueueMode::from_u8(shared.load(Ordering::Relaxed));
                    let new_mode = if on {
                        if current.interrupt_enabled() {
                            input_queue::QueueMode::Both
                        } else {
                            input_queue::QueueMode::Queue
                        }
                    } else if current.interrupt_enabled() {
                        input_queue::QueueMode::Interrupt
                    } else {
                        input_queue::QueueMode::Off
                    };
                    shared.store(new_mode.to_u8(), Ordering::Relaxed);
                    self.out_println(format!(
                        "{DIM}queue: {}{RESET}",
                        if on { "on" } else { "off" }
                    ));
                } else {
                    eprintln!("queue is only available in async REPL mode");
                }
                Ok(())
            }
            _ => {
                eprintln!("Unknown config key '{key}'. Available: auto-interrupt, queue");
                Ok(())
            }
        }
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        print_with_pager(&render_config_report(section)?);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        print_with_pager(&render_memory_report()?);
        Ok(())
    }

    fn open_in_editor(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
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
        let mut msg = format!("Opened memory file at {}", path.display());
        if source == "default" {
            msg.push_str(
                "\n> To use a different editor, set the $EDITOR or $VISUAL environment variable.",
            );
        } else {
            msg.push_str(&format!(
                "\n> Using {}=\"{}\". To change editor, set $EDITOR or $VISUAL environment variable.",
                source, editor
            ));
        }
        Ok(msg)
    }

    fn edit_memory(&self) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let project_context = ProjectContext::discover(&cwd, runtime::today_local())?;
        let files = &project_context.instruction_files;
        let target: PathBuf = if files.is_empty() {
            self.out_println(
                "No instruction files found. Creating AGENTS.md in the current directory.",
            );
            cwd.join("AGENTS.md")
        } else if files.len() == 1 {
            files[0].path.clone()
        } else {
            let labels: Vec<String> = files.iter().map(|f| f.path.display().to_string()).collect();
            let selection = self.out_suspend(|| {
                Select::new()
                    .with_prompt("Select memory file to edit")
                    .items(&labels)
                    .default(0)
                    .interact_opt()
            })?;
            match selection {
                Some(idx) => files[idx].path.clone(),
                None => return Ok(()),
            }
        };
        let msg = self.out_suspend(|| Self::open_in_editor(&target))?;
        self.out_println(msg);
        Ok(())
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
                self.out_println(
                    "Nothing to undo in this session. /undo only restores edit_file and write_file results recorded in the live session."
                );
            }
            Some(edit) => match crate::cli::undo::apply_undo(&edit) {
                Ok(message) => {
                    self.undone_tool_use_ids.insert(edit.tool_use_id.clone());
                    self.out_println(message);
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
        self.out_println(format!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        ));
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
                // On a TTY (sync mode), present a fuzzy picker that switches
                // on Enter and is silent on Esc. In async mode, the input
                if io::stdin().is_terminal() && io::stdout().is_terminal() {
                    let sessions = list_managed_sessions()?;
                    if sessions.is_empty() {
                        self.out_println(render_session_list(&self.session.id)?);
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
                    let selection = self.out_suspend(|| {
                        FuzzySelect::new()
                            .with_prompt("Select a session (type to filter, Esc to cancel)")
                            .items(&items)
                            .default(default_idx)
                            .interact_opt()
                    })?;
                    let Some(idx) = selection else {
                        return Ok(false);
                    };
                    let target = sessions[idx].id.clone();
                    if target == self.session.id {
                        self.out_println(format!("Session unchanged (already active: {target})."));
                        return Ok(false);
                    }
                    return self.handle_session_command(Some("switch"), Some(&target));
                }
                self.out_println(render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    self.out_println("Usage: /session switch <session-id>");
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
                self.out_println(format!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                ));
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
                self.out_println(format!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                ));
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    self.out_println("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    self.out_println(format!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    ));
                    return Ok(false);
                }
                if !self.out_suspend(|| confirm_session_deletion(&handle.id)) {
                    self.out_println("delete: cancelled.");
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                self.out_println(format!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                ));
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    self.out_println("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    self.out_println(format!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    ));
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                self.out_println(format!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                ));
                Ok(false)
            }
            Some(other) => {
                self.out_println(format!(
                    "Unknown /session action '{other}'. Use /session list, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                ));
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
        self.out_println(&result.message);
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
        let result = self
            .tokio_runtime
            .block_on(self.runtime.compact(CompactionConfig::default(), None));
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
        self.out_println(format_compact_report(removed, kept, skipped));
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
        self.out_println(format_bughunter_report(scope));
        Ok(())
    }

    fn run_ultraplan(&self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        self.out_println(format_ultraplan_report(task));
        Ok(())
    }

    fn run_teleport(&self, target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            self.out_println("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        self.out_println(render_teleport_report(target)?);
        Ok(())
    }

    fn run_debug_tool_call(&self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/debug-tool-call", args)?;
        self.out_println(render_last_tool_debug_report(self.runtime.session())?);
        Ok(())
    }

    fn run_commit(&mut self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/commit", args)?;
        let status = git_output(&["status", "--short", "--branch"])?;
        let summary = parse_git_workspace_summary(Some(&status));
        let branch = parse_git_status_branch(Some(&status));
        if summary.is_clean() {
            self.out_println(format_commit_skipped_report());
            return Ok(());
        }

        self.out_println(format_commit_preflight_report(branch.as_deref(), summary));
        Ok(())
    }

    fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let branch =
            resolve_git_branch_for(&env::current_dir()?).unwrap_or_else(|| "unknown".to_string());
        self.out_println(format_pr_report(&branch, context));
        Ok(())
    }

    fn run_issue(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        self.out_println(format_issue_report(context));
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

fn build_system_prompt() -> Result<SystemPrompt, Box<dyn std::error::Error>> {
    build_system_prompt_for(&env::current_dir()?)
}

/// ACP variant of [`build_system_prompt_for`]: builds the process-default
/// prompt for `cwd`/`model` (including any `--system-prompt` /
/// `--append-system-prompt` CLI flags), then layers the session's
/// `_meta.sudocode.systemPrompt` / `appendSystemPrompt` on top: the former
/// swaps the static blocks, the latter appends a trailing dynamic block.
/// Workspace-derived dynamic blocks (environment, `AGENTS.md`, memory,
/// plugins) stay, so the caller's prompt still knows where it is running.
fn build_acp_system_prompt(
    cwd: &Path,
    prompt_overrides: &runtime::SystemPromptOverrides,
) -> Result<SystemPrompt, AcpError> {
    let mut prompt = build_system_prompt_for(cwd)
        .map_err(|e| AcpError::internal(format!("failed to build system prompt: {e}")))?;
    prompt_overrides.apply(&mut prompt);
    Ok(prompt)
}

/// Process-wide `--system-prompt` / `--append-system-prompt` flags, set once
/// from `run()` and applied by every prompt build in this process (REPL,
/// `--print`, `scode system-prompt`, and the ACP default a session starts
/// from before its own `_meta` adjustments).
static CLI_PROMPT_OVERRIDES: std::sync::OnceLock<runtime::SystemPromptOverrides> =
    std::sync::OnceLock::new();

fn apply_cli_prompt_overrides(prompt: &mut SystemPrompt) {
    if let Some(overrides) = CLI_PROMPT_OVERRIDES.get() {
        overrides.apply(prompt);
    }
}

fn build_system_prompt_for(cwd: &Path) -> Result<SystemPrompt, Box<dyn std::error::Error>> {
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
    )?;
    // Coordinator mode: when the SUDOCODE_COORDINATOR_MODE env var is
    // set, prepend the ported CC-fork coordinator role prompt so it
    // takes primacy over the default identity. See
    // runtime::coordinator_mode for the full port.
    runtime::coordinator_mode::apply_coordinator_prompt_if_enabled(&mut prompt);
    apply_cli_prompt_overrides(&mut prompt);
    Ok(prompt)
}

fn build_runtime_plugin_state() -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    let session_mcp: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig> =
        std::collections::BTreeMap::new();
    build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config, &session_mcp)
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
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
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
    let (mcp_state, runtime_tools) =
        build_runtime_mcp_state(runtime_config, &plugin_load_outcome, session_mcp)?;
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
    let session_mcp: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig> =
        std::collections::BTreeMap::new();
    build_runtime_for_cwd(&cwd, session, session_id, config, &session_mcp)
}

fn build_runtime_for_cwd(
    cwd: &Path,
    session: Session,
    session_id: &str,
    config: RuntimeConfig,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let file_config = loader.load()?;
    let runtime_plugin_state =
        build_runtime_plugin_state_with_loader(cwd, &loader, &file_config, session_mcp)?;
    build_runtime_with_plugin_state(
        cwd,
        session,
        session_id,
        config,
        runtime_plugin_state,
        session_mcp,
    )
}

fn build_runtime_with_plugin_state(
    cwd: &Path,
    mut session: Session,
    session_id: &str,
    mut config: RuntimeConfig,
    runtime_plugin_state: RuntimePluginState,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
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
    // Resolve the standalone nexus-A2A session once (fail loud on a partial
    // config or a dial failure). `None` when A2A is off — the fast path that
    // leaves scode behaviour unchanged. Held as `Option<&'static Session>`
    // (Copy) and reused below to advertise, prompt, and wire the send half.
    let a2a = match cli::nexus_a2a::session() {
        Ok(a2a) => a2a,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(Box::new(std::io::Error::other(error)));
        }
    };
    // per-session injected MCP tools bypass the global --allowed-tools gate:
    // they are explicitly requested for this session and their names are only
    // known at runtime, so add their qualified names to the allow-list when
    // one is active. The prefix uses `runtime::mcp_tool_prefix`, which
    // normalizes server names the same way the tool index does (e.g.
    // `github.com` -> `mcp__github_com__`), so non-alphanumeric server names
    // are matched correctly.
    if let Some(allowed) = config.allowed_tools.as_mut() {
        if let Some(mcp_state) = &mcp_state {
            let tools = mcp_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .manager
                .tools_with_server();
            allowed.extend(session_mcp_tool_names(tools, session_mcp));
        }
    }
    // nexus A2A: when configured, keep the peer-reply tool available even under
    // an explicit --allowedTools restriction (absent a restriction it is
    // already advertised). Its handler is the CliToolExecutor intercept wired
    // below; the co-host advertises the same tool the same way.
    if a2a.is_some() {
        if let Some(allowed) = config.allowed_tools.as_mut() {
            allowed.extend(["send_message".to_string()]);
        }
    }
    let policy =
        match permission_policy(config.permission_mode, &feature_config, &tool_registry, cwd) {
            Ok(policy) => policy,
            Err(error) => {
                shutdown_mcp_state_best_effort(&mcp_state);
                return Err(Box::new(std::io::Error::other(error)));
            }
        };
    let mut system_prompt = config.system_prompt.clone();
    // Skills are listed so the model can name and load one without the user
    // having to know it exists. Plugin-provided skill roots are included via
    // `plugin_load_outcome`, so a plugin can inject skills that the prompt then
    // advertises. This runs for the REPL, `--print`, and ACP sessions alike:
    // they all land in this function via `build_runtime_for_cwd`.
    if let Some(section) = render_skills_prompt_section(cwd, Some(&plugin_load_outcome)) {
        system_prompt.dynamic_sections.push(section);
    }
    // nexus A2A: teach the model its A2A identity + how to reach peers, so the
    // standalone loop knows it can `send_message` to a named peer.
    if let Some(session) = a2a {
        system_prompt
            .dynamic_sections
            .push(session.peer_system_prompt());
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
    // nexus A2A: give the CLI executor the send half so `send_message` routes
    // to the peer's replicated DT_STREAM inbox (the shared handler the co-host
    // uses). Set only when configured; absent it the tool is never advertised.
    if let Some(session) = a2a {
        runtime
            .tool_executor_mut()
            .set_mailbox_sender(session.sender());
    }
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

/// Permission prompter that auto-denies all requests. Used in compact/pipe
/// mode where interactive prompts would corrupt the output stream.
struct AutoDenyPermissionPrompter;

impl runtime::PermissionPrompter for AutoDenyPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        runtime::PermissionPromptDecision::Deny {
            reason: format!(
                "tool '{}' auto-denied in non-interactive compact mode",
                request.tool_name
            ),
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
    // Add capabilities SSOT model IDs to /model completions.
    for id in runtime::model_capabilities::all_model_ids() {
        completions
            .entry(format!("/model {id}"))
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
    if let Some(mode) = explicit {
        return Ok(mode);
    }
    resolve_configured_auth_mode(model, config)
}

fn resolve_model_switch_auth_mode(
    model: &str,
    explicit: Option<AuthMode>,
    config: &api::SudoCodeConfig,
) -> Result<AuthMode, String> {
    let Some(entry) = api::resolve_model(config, model) else {
        if let Some(mode) = explicit {
            return Ok(mode);
        }
        // Proxy passthrough fallback — same logic as resolve_configured_auth_mode.
        if config.auth_modes.contains_key("proxy") {
            return AuthMode::parse("proxy");
        }
        return Err(format!(
            "model '{model}' not found in config. Run /model to configure it, \
             or pass --auth=<subscription|proxy|api-key> explicitly."
        ));
    };

    if let Some(mode) = explicit {
        if entry.providers.contains_key(mode.as_str()) {
            return Ok(mode);
        }
    }

    resolve_configured_auth_mode_for_entry(model, entry)
}

fn resolve_configured_auth_mode(
    model: &str,
    config: &api::SudoCodeConfig,
) -> Result<AuthMode, String> {
    if let Some(entry) = api::resolve_model(config, model) {
        return resolve_configured_auth_mode_for_entry(model, entry);
    }
    // Model not in sudocode.json — if a proxy provider is configured,
    // default to proxy auth mode and let proxy passthrough route it.
    // This avoids requiring every model to be registered in sudocode.json
    // when sudorouter already knows how to route it.
    if config.auth_modes.contains_key("proxy") {
        return AuthMode::parse("proxy");
    }
    Err(format!(
        "model '{model}' not found in config. Run /model to configure it, \
         or pass --auth=<subscription|proxy|api-key> explicitly."
    ))
}

fn resolve_configured_auth_mode_for_entry(
    model: &str,
    entry: &api::ModelConfigEntry,
) -> Result<AuthMode, String> {
    const PRIORITY: &[&str] = &["subscription", "proxy", "api-key"];
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

#[cfg(test)]
mod auth_mode_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn connection(base_url: &str) -> api::ProviderConnectionConfig {
        api::ProviderConnectionConfig {
            base_url: base_url.to_string(),
            api_key: Some("test-key".to_string()),
            api_key_env: None,
            token: None,
            token_env: None,
            auth_file: None,
        }
    }

    fn model_entry(
        alias: &str,
        mode: &str,
        provider: &str,
        wire_model: &str,
        api_format: &str,
    ) -> api::ModelConfigEntry {
        let mut providers = BTreeMap::new();
        providers.insert(
            mode.to_string(),
            api::ModelProviderMapping {
                provider: provider.to_string(),
                model: wire_model.to_string(),
                api: Some(api_format.to_string()),
            },
        );

        api::ModelConfigEntry {
            alias: alias.to_string(),
            name: alias.to_string(),
            input: vec!["text".to_string()],
            providers,
            ..Default::default()
        }
    }

    fn mixed_auth_config() -> api::SudoCodeConfig {
        let mut auth_modes = BTreeMap::new();
        auth_modes.insert(
            "proxy".to_string(),
            BTreeMap::from([(
                "sudorouter".to_string(),
                connection("https://hk.sudorouter.ai/v1"),
            )]),
        );
        auth_modes.insert(
            "api-key".to_string(),
            BTreeMap::from([(
                "deepseek-anthropic".to_string(),
                connection("https://api.deepseek.com/anthropic"),
            )]),
        );

        let mut models = BTreeMap::new();
        models.insert(
            "minimax-m2.5".to_string(),
            model_entry(
                "MiniMax-M2.5",
                "proxy",
                "sudorouter",
                "MiniMax-M2.5",
                "openai-completions",
            ),
        );
        models.insert(
            "deepseek-anthropic/deepseek-v4-flash".to_string(),
            model_entry(
                "deepseek-anthropic/deepseek-v4-flash",
                "api-key",
                "deepseek-anthropic",
                "deepseek-v4-flash",
                "anthropic-messages",
            ),
        );

        api::SudoCodeConfig {
            auth_modes,
            models,
            ..Default::default()
        }
    }

    #[test]
    fn configured_api_key_model_wins_over_stale_proxy_auth_mode() {
        let config = mixed_auth_config();

        let mode = resolve_model_switch_auth_mode(
            "deepseek-anthropic/deepseek-v4-flash",
            Some(AuthMode::Proxy),
            &config,
        )
        .expect("configured api-key model should resolve");

        assert_eq!(mode, AuthMode::ApiKey);
    }

    #[test]
    fn model_switch_keeps_explicit_mode_when_target_supports_it() {
        let config = mixed_auth_config();

        let mode = resolve_model_switch_auth_mode(
            "deepseek-anthropic/deepseek-v4-flash",
            Some(AuthMode::ApiKey),
            &config,
        )
        .expect("deepseek should support api-key auth");

        assert_eq!(mode, AuthMode::ApiKey);
    }

    #[test]
    fn model_switch_falls_back_to_explicit_mode_for_unknown_proxy_model() {
        let config = mixed_auth_config();

        let mode = resolve_model_switch_auth_mode(
            "unconfigured-proxy-model",
            Some(AuthMode::Proxy),
            &config,
        )
        .expect("explicit proxy auth should allow passthrough models");

        assert_eq!(mode, AuthMode::Proxy);
    }

    #[test]
    fn pending_question_consumes_next_iocraft_question_answer() {
        let (tx, rx) = mpsc::sync_channel(1);
        let pending = Arc::new(Mutex::new(Some(tx)));

        assert!(consume_pending_question_answer(
            &pending,
            "answer from ui".to_string()
        ));
        assert_eq!(
            rx.recv().expect("answer should be routed"),
            "answer from ui"
        );
        assert!(pending.lock().expect("pending lock").is_none());
    }

    #[test]
    fn absent_pending_question_rejects_unexpected_iocraft_question_answer() {
        let pending = Arc::new(Mutex::new(None));

        assert!(!consume_pending_question_answer(
            &pending,
            "normal prompt".to_string()
        ));
    }
}
