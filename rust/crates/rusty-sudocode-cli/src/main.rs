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
mod render_engine;
mod repl_ui;

use engine_acp::AcpError;
use engine_core::{
    EngineApiClient, EngineCommand, EngineDelegate, EngineEvent, EngineHandle, EngineSession,
    TurnComplete,
};
use render_engine::{EngineEventRenderer, RenderOutcome};

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

use engine_core::{
    base_url_for_mode, resolve_startup_auth_source, AuthMode, AuthSource, ProviderKind,
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
    describe_tool_progress, first_visible_line, format_account_report,
    format_account_switch_report, format_acp_compact_report, format_auth_report,
    format_auth_switch_report, format_auto_compaction_notice, format_bughunter_report,
    format_commit_preflight_report, format_commit_skipped_report, format_compact_report,
    format_cost_report, format_internal_prompt_progress_line, format_issue_report,
    format_model_report, format_model_switch_report, format_permission_prompt_box,
    format_permissions_report, format_permissions_switch_report, format_pr_report,
    format_resume_report, format_sandbox_report, format_tool_call_start, format_tool_result,
    format_turn_status_line, format_ultraplan_report, render_messages, render_resume_usage,
    render_version_report, truncate_for_summary, TurnStatus,
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
use cli::pager::print_with_pager;
use cli::session::{
    confirm_session_deletion, format_session_picker_entry, list_managed_sessions,
    render_session_list, LATEST_SESSION_REFERENCE,
};
use cli::status::{
    format_status_report, print_sandbox_status_snapshot, print_status_snapshot, print_version,
    sandbox_json_value, status_context, status_json_value, version_json_value, StatusContext,
    StatusUsage,
};
use commands::{
    acp_slash_commands, classify_skills_slash_command, cwd_prompt_sections,
    format_acp_unsupported_slash_command, handle_agents_slash_command,
    handle_agents_slash_command_json, handle_mcp_slash_command_json_with_plugins,
    handle_mcp_slash_command_with_plugins, handle_plugins_slash_command,
    handle_skills_slash_command, handle_skills_slash_command_json,
    handle_skills_slash_command_json_with_plugins, handle_skills_slash_command_with_plugins,
    render_acp_slash_command_help, render_slash_command_help, render_slash_command_help_filtered,
    resolve_skill_invocation, resolve_skill_invocation_with_plugins,
    resume_supported_slash_commands, slash_command_specs, validate_slash_command_input,
    SkillSlashDispatch, SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use dialoguer::{FuzzySelect, Select};
use engine_host::config::{resolve_auth_mode, resolve_model_switch_auth_mode};
use engine_host::mcp::{
    build_runtime_mcp_state, session_mcp_tool_names, shutdown_mcp_state_best_effort,
    RuntimeMcpState,
};
use engine_host::prompt::{
    apply_cli_prompt_overrides, build_acp_system_prompt, build_system_prompt_for,
    set_cli_prompt_overrides,
};
use engine_host::session::{
    canonical_session_cwd, context_overflow_user_message, create_managed_session_handle,
    create_managed_session_handle_for, delete_managed_session, load_session_reference,
    new_cli_session, new_cli_session_for, resolve_session_reference, write_session_clear_backup,
    SessionHandle,
};
use engine_host::tool_executor::{
    clear_pending_plan_execution, permission_policy, take_pending_plan_execution, CliToolExecutor,
};
// The engine CORE cluster now lives below the seam in `engine-host`
// (`runtime_build` + `session_engine`, re-exported at the crate root). The
// renderer names these to build a session (`SessionEngine`), drive its non-turn
// lifecycle (`SessionLifecycle`), and — on the ACP path only — construct/hold a
// runtime directly (`BuiltRuntime` / `RuntimeConfig` / `AcpCliSession` + the
// `build_*` helpers). `RuntimeConfig` is the engine-side config struct; it does
// not shadow `runtime::RuntimeConfig` (always named fully-qualified).
use engine_host::{
    build_engine_runtime, build_plugin_manager, build_runtime_for_cwd,
    build_runtime_plugin_state_with_loader, plugin_load_outcome_for_cwd, AcpCliSession,
    BuiltRuntime, ModelSwitchReport, RuntimeConfig, RuntimePluginState, SessionEngine,
    SessionLifecycle,
};
use init::initialize_repo;
use plugins::{PluginLoadOutcome, PluginManager, PluginRegistry};
use render::{
    ansi_bold_fg, ansi_fg, theme, MarkdownStreamState, SpinnerHandle, TerminalRenderer, DIM, RESET,
};
use runtime::{
    check_base_commit, compact_session_sync, estimate_block_tokens, estimate_session_tokens,
    format_stale_base_warning, format_usd, load_oauth_credentials, load_system_prompt,
    pricing_for_model, resolve_expected_base, resolve_sandbox_status, should_compact, ApiClient,
    ApiRequest, AssistantEvent, CompactionConfig, ConfigLoader, ConfigScope, ConfigSource,
    ContentBlock, ConversationMessage, ConversationRuntime, McpServer, McpServerManager,
    McpServerSpec, McpTool, MessageRole, MigrationScope, ModelPricing, PermissionMode,
    PermissionPolicy, ProjectContext, PromptCacheEvent, ResolvedPermissionMode, RuntimeError,
    Session, SystemPrompt, TokenUsage, ToolError, ToolExecutor, UsageTracker,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tools::{
    execute_tool, mvp_tool_specs, GlobalToolRegistry, RuntimeToolDefinition, ToolSearchOutput,
};

// The config / model / permission web moved below the seam into
// `engine_host::config`. Re-export the handful the CLI names by their bare
// (`crate::`) path so the renderer keeps one import surface for them; the
// resolution logic itself is the engine's input, not the renderer's concern.
pub(crate) use engine_host::config::{
    lookup_default_model, normalize_permission_mode, ModelSource, DEFAULT_MODEL,
};

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

    /// A borrowed, renderer-neutral view for the shared `commands::reports`
    /// status formatter (whose `provenance` parameter is crate-agnostic).
    pub(crate) fn view(&self) -> commands::reports::ProvenanceView<'_> {
        commands::reports::ProvenanceView {
            resolved: &self.resolved,
            raw: self.raw.as_deref(),
            source: self.source.as_str(),
        }
    }
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
/// This crate's build metadata, packaged for the shared `commands::reports`
/// doctor report (which is crate-agnostic and takes it as a parameter).
pub(crate) fn build_info() -> commands::reports::BuildInfo<'static> {
    commands::reports::BuildInfo {
        version: VERSION,
        git_sha: GIT_SHA,
        build_target: BUILD_TARGET,
    }
}

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

/// Environment variable that suppresses the startup config migration.
const SKIP_CONFIG_MIGRATION_ENV: &str = "SCODE_SKIP_CONFIG_MIGRATION";

/// How long a one-shot turn may run before the first "still waiting" line.
/// Long enough that an ordinary turn never prints one, short enough that a
/// person does not start doubting the process.
const WAIT_NOTICE_FIRST: Duration = Duration::from_secs(15);
/// Cadence after the first line. Slow: this is reassurance, not telemetry.
const WAIT_NOTICE_REPEAT: Duration = Duration::from_secs(30);

/// Tells the user, on stderr, that a one-shot turn is still waiting — and on
/// what — when it takes long enough to look like a hang.
///
/// The one-shot path renders nothing until the model answers, and the default
/// read timeout allows a stalled connection five minutes of silence. Those two
/// facts together make a slow upstream indistinguishable from a hung process.
/// It is not a hypothetical confusion: it sent one debugging session down six
/// rounds of bisecting and three throwaway builds looking for a regression that
/// did not exist, because "no output at all" reads as "broken", never as
/// "waiting".
///
/// Writes to stderr, never stdout — stdout carries `--output-format json`, and
/// one stray line there would turn a working run into a parse error.
struct WaitNotice {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl WaitNotice {
    /// Start announcing on stderr once a turn passes [`WAIT_NOTICE_FIRST`].
    ///
    /// The upstream is described lazily, inside the thread, on the first line it
    /// actually prints. Resolving it up front would load the whole config on
    /// every run — including the overwhelming majority that never wait long
    /// enough to say anything — and that load warms the model-capabilities SSOT
    /// as a side effect. Pulling an initialization earlier than it would
    /// otherwise happen is how a previous change quietly broke an unrelated
    /// test; a feature that only speaks in the slow case should also only do its
    /// work there.
    fn start(model: String) -> Self {
        let cwd = env::current_dir().unwrap_or_default();
        Self::start_lazily(
            move || commands::reports::describe_upstream_for(&cwd, &model),
            WAIT_NOTICE_FIRST,
            WAIT_NOTICE_REPEAT,
            |line| eprintln!("{line}"),
        )
    }

    /// Seam for tests: injectable description, clock intervals, and sink.
    ///
    /// `describe` runs at most once, and only if the wait lasts long enough to
    /// report — a turn that finishes normally never calls it.
    fn start_lazily(
        describe: impl FnOnce() -> String + Send + 'static,
        first: Duration,
        repeat: Duration,
        emit: impl Fn(String) + Send + 'static,
    ) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let mut interval = first;
            let mut waited = Duration::ZERO;
            let mut target: Option<String> = None;
            // `FnOnce` in an `Option` so the loop can take it exactly once and
            // the compiler enforces that, rather than a comment promising it.
            let mut describe = Some(describe);
            loop {
                match stopped.recv_timeout(interval) {
                    // Turn finished, or the guard was dropped: say nothing more.
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {
                        waited += interval;
                        let described = match &target {
                            Some(known) => known.clone(),
                            None => {
                                let described = describe
                                    .take()
                                    .map_or_else(String::new, |describe| describe());
                                target = Some(described.clone());
                                described
                            }
                        };
                        emit(format!(
                            "scode: still waiting on {described} ({}s)",
                            waited.as_secs()
                        ));
                        interval = repeat;
                    }
                }
            }
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for WaitNotice {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One-time repair of the legacy config shape, run before any command.
///
/// **Temporary shim — added 2026-09, delete once installs have turned over.**
/// `scode config migrate` stays the explicit entry point; this exists because
/// the shape it repairs costs every user money silently. A stale `api` override
/// keeps prompt caching off, and the only evidence is a bill, so expecting each
/// user to learn about a command they have no reason to run would leave most
/// configs wrong indefinitely.
///
/// Three properties make it safe to run unattended, and all three are load
/// bearing:
///
/// * **Non-fatal.** Any failure leaves the config untouched and the command
///   proceeds. A migration that can block startup is worse than the shape it
///   repairs.
/// * **Quiet unless it acts**, and never on stdout — that carries
///   `--output-format json`, which a stray line would corrupt.
/// * **Visible when it acts.** It writes a backup and says what it changed. A
///   silent fixer would reproduce exactly the invisibility that let the original
///   problem run for months.
///
/// The migration itself holds a lock across its whole read-modify-write, so
/// several agents starting at once converge instead of clobbering each other.
fn auto_migrate_legacy_config() {
    if env::var_os(SKIP_CONFIG_MIGRATION_ENV).is_some() {
        return;
    }
    let Ok(cwd) = env::current_dir() else {
        return;
    };
    // The narrow scope, not `Full`. Two reasons pointing the same way: dropping
    // `provider` produces a file older builds refuse to load, which is not
    // something to do to someone unasked; and deciding about `api` in general
    // reads the capabilities SSOT, a `OnceLock` that freezes empty if touched
    // before the program loads the real file. What is left is safe for other
    // builds, answerable from the model id — and is the half with a running cost.
    match ConfigLoader::default_for(&cwd)
        .migrate_legacy_config_shape(MigrationScope::CacheDisablingApiOverrides)
    {
        Ok(report) if report.changed() => {
            eprintln!(
                "scode: removed {} api override(s) from {} that were disabling prompt caching",
                report.cleared_apis.len(),
                report.path.display()
            );
            eprintln!("  affected: {}", report.cleared_apis.join(", "));
            if let Some(backup) = &report.backup {
                eprintln!("  previous version: {}", backup.display());
            }
            eprintln!("  run `scode config migrate` to also collapse the per-model account copies");
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!(
                "scode: left the config alone ({error}) — run `scode config migrate` for detail, \
                 or set {SKIP_CONFIG_MIGRATION_ENV}=1 to stop trying"
            );
        }
    }
}

/// `scode config migrate` — drop the per-model copies of the account and wire
/// format, leaving each fact stated once.
///
/// Prints exactly what changed and where the backup went. A fixer that edits
/// config silently would reproduce the problem it is fixing: config drift is
/// invisible, which is why it went unnoticed long enough to misroute a whole
/// session's spending.
fn run_config_migrate(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let report =
        ConfigLoader::default_for(&cwd).migrate_legacy_config_shape(MigrationScope::Full)?;
    match output_format {
        CliOutputFormat::Text => {
            if !report.changed() {
                println!(
                    "nothing to migrate — {} already states each fact once",
                    report.path.display()
                );
                return Ok(());
            }
            println!("migrated {}", report.path.display());
            if let Some(account) = &report.wrote_auth_profile {
                println!("  auth_profile = {account}  (recorded before dropping the pins, so routing never lapses)");
            }
            if !report.cleared_providers.is_empty() {
                println!(
                    "  dropped pinned provider from {} model(s): {}",
                    report.cleared_providers.len(),
                    report.cleared_providers.join(", ")
                );
            }
            if !report.cleared_apis.is_empty() {
                println!(
                    "  dropped redundant api override from {} model(s): {}",
                    report.cleared_apis.len(),
                    report.cleared_apis.join(", ")
                );
                println!("  (wire format now comes from the model capabilities SSOT — this is what re-enables prompt caching)");
            }
            if let Some(backup) = &report.backup {
                println!("  backup      {}", backup.display());
            }
            if !report.cleared_providers.is_empty() {
                // Dropping `provider` is the irreversible half: builds predating
                // the optional-`provider` parser refuse to load a file without
                // it. That is why it happens here, where someone asked for it,
                // and never on the startup path — and why it is said out loud
                // rather than left to surface as an unattributable boot error on
                // whichever other scode build shares this config.
                println!(
                    "  note        scode builds older than this one cannot read the result; \
                     restore the backup above if you need one to run"
                );
            }
        }
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "kind": "config-migrate",
                "changed": report.changed(),
                "path": report.path.display().to_string(),
                "auth_profile": report.wrote_auth_profile,
                "cleared_providers": report.cleared_providers,
                "cleared_apis": report.cleared_apis,
                "backup": report.backup.map(|path| path.display().to_string()),
            }))?
        ),
    }
    Ok(())
}

/// `scode config account [<name>] [--global]` — show, or set, the account that
/// requests are billed to.
///
/// Writing goes through `ConfigLoader::set_auth_profile`, the same entry point
/// the interactive picker uses, so "record which account pays" has one
/// implementation rather than one per surface — the drift this whole area is
/// being repaired for.
///
/// An unknown name is rejected here rather than written: a typo that reaches the
/// config file turns into a refusal at the next request, far from the command
/// that caused it.
fn run_config_account(
    account: Option<&str>,
    global: bool,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let config = loader.load_sudocode_config()?;
    let known: Vec<&str> = config
        .auth_modes
        .get("proxy")
        .map(|accounts| accounts.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let candidates = || {
        if known.is_empty() {
            "<none configured under auth_modes.proxy>".to_string()
        } else {
            known.join(", ")
        }
    };

    let Some(account) = account else {
        let (global_path, project_path) = loader.auth_profile_paths();
        match output_format {
            CliOutputFormat::Text => {
                println!(
                    "account   {}",
                    config
                        .selected_account
                        .as_deref()
                        .unwrap_or("<none selected>")
                );
                println!("available {}", candidates());
                println!(
                    "set here  scode config account <name>            → {}",
                    project_path.display()
                );
                println!(
                    "machine   scode config account <name> --global   → {}",
                    global_path.display()
                );
                for conflict in &config.auth_profile_conflicts {
                    println!(
                        "warning   auth_profile in {} is ignored — remove it",
                        conflict.display()
                    );
                }
            }
            CliOutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "kind": "config-account",
                    "account": config.selected_account,
                    "available": known,
                    "project_path": project_path.display().to_string(),
                    "global_path": global_path.display().to_string(),
                    "ignored": config
                        .auth_profile_conflicts
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>(),
                }))?
            ),
        }
        return Ok(());
    };

    if !known.contains(&account) {
        return Err(format!("unknown account '{account}'. Available: {}", candidates()).into());
    }
    let scope = if global {
        ConfigScope::Global
    } else {
        ConfigScope::Project
    };
    let written = loader.set_auth_profile(account, scope)?;
    match output_format {
        CliOutputFormat::Text => println!("account = {account}  ({})", written.display()),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "kind": "config-account",
                "account": account,
                "scope": if global { "global" } else { "project" },
                "path": written.display().to_string(),
            }))?
        ),
    }
    Ok(())
}

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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (action, prompt_overrides) = parse_args_with_prompt_overrides(&args)?;
    // Only writer in the process; a second `set` cannot happen.
    set_cli_prompt_overrides(prompt_overrides);
    auto_migrate_legacy_config();
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
            let wait_model = resolved_model.clone();
            let mut cli = LiveCli::new(
                resolved_model,
                true,
                allowed_tools,
                permission_mode,
                reasoning_effort,
                auth_mode,
            )?;
            // Non-interactive one-shot: SIGINT / Ctrl-C must gracefully cancel
            // the turn across the seam (the running tool returns interrupted and
            // the turn ends cancelled) rather than hard-kill the process. The
            // REPL wires this through its input path; the one-shot path has no
            // key reader, so install a scoped signal→Cancel monitor here.
            let _cancel_guard = SignalCancelGuard::install(cli.engine_handle.commands.clone());
            // Nothing renders until the model answers, so a slow upstream looks
            // exactly like a hang. Say what is being waited on instead.
            let _wait_notice = WaitNotice::start(wait_model);
            cli.run_turn_with_output(&effective_prompt, output_format, compact)?;
            drop(_wait_notice);

            // Record token usage and session ended event for non-interactive prompt mode
            let duration_ms = session_start.elapsed().as_millis() as u64;
            let usage_tracker = cli.lifecycle.usage_snapshot();
            let usage = usage_tracker.cumulative_usage();
            let total_turns = usage_tracker.turns();
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
            // The engine owns its tokio runtime and tears it down on Close /
            // drop; the renderer keeps none, so there is nothing to shut down
            // here.
        }
        CliAction::Doctor { fix, output_format } => {
            // Repair before reporting, so the report a user reads is the state
            // they are actually left in rather than the one just replaced.
            if fix {
                run_config_migrate(output_format)?;
            }
            run_doctor(output_format)?;
        }
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
            value,
            global,
            output_format,
        } if section.as_deref() == Some("account") => {
            run_config_account(value.as_deref(), global, output_format)?;
        }
        CliAction::Config {
            section,
            output_format,
            ..
        } if section.as_deref() == Some("migrate") => {
            run_config_migrate(output_format)?;
        }
        CliAction::Config {
            section,
            output_format,
            ..
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
        CliAction::Update {
            version,
            check,
            yes,
        } => cli::update::run(version, check, yes)?,
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
    // cwd-derived sections.
    apply_cli_prompt_overrides(&mut prompt);
    // `commands::cwd_prompt_sections` is the SAME list the live runtime
    // extends, so this preview cannot claim a prompt a session does not send.
    // It is a subset by design: the deferred-tool listing needs a tool registry
    // and the A2A identity needs a dialed session, neither of which a preview
    // has.
    //
    // Load failures captured inside PluginLoadOutcome are excluded naturally;
    // Result errors propagate, so a broken plugin install fails this preview
    // exactly as it fails a live session.
    let outcome = plugin_load_outcome_for_cwd(&cwd)?;
    prompt
        .dynamic_sections
        .extend(cwd_prompt_sections(&cwd, Some(&outcome)));
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

/// The `--resume list` session browser: prints available sessions with id,
/// age, message count, and branch — enough context to pick a specific older
/// session. (Bare `--resume` resumes the latest session directly.)
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

    println!("Available sessions (`scode --resume <id>`, or `scode --resume` for the latest):\n");
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
    println!("Tip: `scode --resume` (no id) resumes the most recent session.");
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
        let mut cli =
            match LiveCli::new(resolved_model, true, None, permission_mode, None, auth_mode) {
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
                message: Some(format_compact_report(
                    removed,
                    kept,
                    skipped,
                    &result.summary_source,
                )),
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
            let model = session.model.as_deref().unwrap_or("restored-session");
            // Resolved for the model the session was restored with: a resumed
            // session bills whoever the current config says, which need not be
            // whoever paid for the turns already in the transcript.
            let account = engine_host::billing_account_for_model(model, None);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_status_report(
                    model,
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
                    &account.describe(),
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
                    &account,
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
            let report = render_doctor_report(&build_info())?;
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
        | SlashCommand::Account { .. }
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
    let cli = LiveCli::new(
        resolved_model,
        true,
        allowed_tools,
        permission_mode,
        reasoning_effort,
        auth_mode,
    )?;

    // Env-gated opt-in to the async REPL that accepts input during a running
    // turn (see `input_queue` module docs and
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
    let session = cli.lifecycle.session_snapshot();
    let messages = &session.messages;
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
        input_chrome::print_before_prompt(cli.lifecycle.current_permission_mode().as_str());
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
                let plugin_outcome = cli.lifecycle.plugin_load_outcome();
                if let Some(prompt) = try_resolve_bare_skill_prompt_with_plugins(
                    &cwd,
                    &trimmed,
                    Some(&plugin_outcome),
                ) {
                    cli.record_prompt_history(&trimmed);
                    if let Err(e) = cli.run_turn_interactive(&prompt) {
                        eprintln!("{}{e}{}", ansi_fg(theme().error), RESET);
                    }
                    continue;
                }
                cli.record_prompt_history(&trimmed);
                if let Err(e) = cli.run_turn_interactive(&trimmed) {
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
    let usage_tracker = cli.lifecycle.usage_snapshot();
    let usage = usage_tracker.cumulative_usage();
    let total_turns = usage_tracker.turns();
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

/// Resolves when the OS delivers Ctrl-Break; on non-Windows it never resolves.
///
/// Windows-only cancellation channel — see [`SignalCancelGuard`] for why a
/// Ctrl-C-only monitor is unreachable from a parent that used
/// `CREATE_NEW_PROCESS_GROUP`. If the handler can't be registered we fall back
/// to pending so the `select!` still runs on Ctrl-C alone.
#[cfg(windows)]
async fn ctrl_break_cancel_signal() {
    match tokio::signal::windows::ctrl_break() {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(windows))]
async fn ctrl_break_cancel_signal() {
    std::future::pending().await
}

/// Installs a process SIGINT / Ctrl-C handler for the duration of a
/// **non-interactive one-shot turn** (`scode <prompt>`, incl. `--output-format
/// json`), translating the signal into an `EngineCommand::Cancel` across the
/// seam. The pump then aborts the in-flight turn — the running tool returns an
/// interrupted result and the turn ends `cancelled`, so the process still
/// prints its final text / JSON and exits cleanly (exit 0), matching the
/// pre-seam behavior that the old `HookAbortMonitor` provided.
///
/// Interactive REPL turns cancel through the input path
/// (`LiveCliDriver::abort_current_turn`), so this guard is scoped to the
/// one-shot entrypoint, which has no key-reader thread and would otherwise take
/// SIGINT's default disposition (hard-kill, non-zero exit, no output).
///
/// The monitor runs on its own current-thread tokio runtime (the renderer is
/// otherwise tokio-free). Dropping the guard stops it: the stop channel wakes
/// the blocking waiter, which wins the `select!` and lets the runtime unwind.
///
/// On Windows it also listens for Ctrl-Break, because a parent that spawns
/// `scode` with `CREATE_NEW_PROCESS_GROUP` — the standard way a job runner
/// isolates cancellation so it doesn't signal its whole console — *cannot*
/// deliver Ctrl-C: Windows disables Ctrl-C for a new process group, leaving
/// `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` as the only way to reach
/// it. Both mean the same thing here, so both raise `Cancel`.
struct SignalCancelGuard {
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    join_handle: Option<std::thread::JoinHandle<()>>,
}

impl SignalCancelGuard {
    fn install(commands: std::sync::mpsc::Sender<EngineCommand>) -> Self {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let join_handle = thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                // Blocking waiter for the drop signal; it returns the moment the
                // guard is dropped (or its sender disconnects), ending the turn.
                let stop = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                });
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            let _ = commands.send(EngineCommand::Cancel);
                        }
                    }
                    () = ctrl_break_cancel_signal() => {
                        let _ = commands.send(EngineCommand::Cancel);
                    }
                    _ = stop => {}
                }
            });
        });
        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }
}

impl Drop for SignalCancelGuard {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            // The monitor exits within a few ms of the stop signal (or right
            // after it delivered a Cancel). Bound the wait so a wedged signal
            // driver can never hang process teardown — mirrors the old
            // HookAbortMonitor's timed join.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while !join_handle.is_finished() {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = join_handle.join();
        }
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

/// Events from all sources into the coordinator REPL loop.
///
/// Multi-producer single-consumer: iocraft UI produces `Human`, the
/// A2A poller produces `PeerMessage`, and the turn runner produces
/// `TurnComplete`.
enum CoordinatorEvent {
    Human(repl_ui::InputEvent),
    /// A peer's message, plus the acknowledgement its receiver waits on.
    ///
    /// The receiver must not advance its durable cursor past a message that is
    /// only sitting in this channel: a crash then loses it silently, with the
    /// sender already told "delivered". So it blocks on this ack until the
    /// coordinator loop has taken the message, which makes the channel
    /// back-pressured rather than a place messages accumulate behind the
    /// cursor. Dropping the sender is a refusal and re-delivers.
    PeerMessage(runtime::agent_mailbox::MailboxEnvelope, mpsc::Sender<()>),
    TurnComplete,
}

/// Hand a peer's message to the coordinator loop and block until it is taken.
///
/// The return value is what the inbox receiver uses to decide whether to
/// advance its durable cursor, so "handed over" is not good enough — a message
/// queued behind a long turn with the cursor already past it is lost on a crash,
/// silently, after its sender was told it was delivered. Blocking the receive
/// thread here is the point: it is the back-pressure that keeps the cursor and
/// the consumer in step.
///
/// `false` when the coordinator loop is gone (shutting down) or dropped the ack
/// without handling the message; either way the receiver re-delivers.
fn ack_after_coordinator_takes(
    tx: &mpsc::Sender<CoordinatorEvent>,
    msg: &runtime::agent_mailbox::MailboxEnvelope,
) -> bool {
    let (ack_tx, ack_rx) = mpsc::channel();
    if tx
        .send(CoordinatorEvent::PeerMessage(msg.clone(), ack_tx))
        .is_err()
    {
        return false;
    }
    ack_rx.recv().is_ok()
}

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

// ──────────────────────────────────────────────────────────────────────
// Sync-REPL per-turn cancel monitor.
//
// The sync rustyline REPL runs a turn by blocking on the engine seam, so it
// needs a side channel to catch ESC / Ctrl-C mid-turn and cancel. On Unix a
// bare ESC (0x1b) can't be reliably read through crossterm's event system
// after rustyline toggles raw mode, so we poll termios directly — exactly what
// the pre-seam HookAbortMonitor did. The only change: the sink is now the seam
// (`EngineCommand::Cancel`), not a raw abort-signal handle.
// ──────────────────────────────────────────────────────────────────────

#[cfg(unix)]
std::thread_local! {
    static ORIGINAL_TERMIOS: std::cell::RefCell<Option<nix::sys::termios::Termios>> =
        const { std::cell::RefCell::new(None) };
}

/// Enable raw mode via `nix::sys::termios` (no crossterm). Returns `true` on
/// success; call `disable_raw_mode_unix()` to restore.
#[cfg(unix)]
fn enable_raw_mode_unix() -> bool {
    use nix::sys::termios::{self, SetArg, SpecialCharacterIndices};

    let stdin = std::io::stdin();
    let Ok(original) = termios::tcgetattr(&stdin) else {
        return false;
    };
    ORIGINAL_TERMIOS.with(|cell| *cell.borrow_mut() = Some(original.clone()));

    let mut raw = original;
    termios::cfmakeraw(&mut raw);
    // Non-blocking: VMIN=0, VTIME=0 → read() returns immediately with 0 bytes
    // when nothing is available; poll() handles the wait.
    raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
    raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0;
    termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw).is_ok()
}

/// Restore terminal settings saved by `enable_raw_mode_unix`.
#[cfg(unix)]
fn disable_raw_mode_unix() {
    use nix::sys::termios::{self, SetArg};

    let stdin = std::io::stdin();
    ORIGINAL_TERMIOS.with(|cell| {
        if let Some(original) = cell.borrow().as_ref() {
            let _ = termios::tcsetattr(&stdin, SetArg::TCSANOW, original);
        }
    });
}

/// Which abort key `poll_abort_key` detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbortKey {
    None,
    /// ESC (0x1b) — cancels the current turn, never exits.
    Esc,
    /// Ctrl-C (0x03) — cancels the current turn; a second press within 800ms
    /// exits the process (CC parity).
    CtrlC,
}

/// Poll stdin for an abort key (ESC = 0x1b, Ctrl-C = 0x03) on Unix.
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

/// Poll stdin for an abort key via crossterm's event system (Windows).
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

#[derive(Default)]
struct MonitorInner {
    /// Set by `suspend()` — the monitor should stop polling + restore cooked mode.
    suspend_requested: bool,
    /// Set by the monitor once it has parked (cooked mode restored).
    suspended: bool,
    /// Set by `Drop` — the monitor should exit.
    stop: bool,
}

struct MonitorCtl {
    inner: Mutex<MonitorInner>,
    cv: std::sync::Condvar,
}

/// A per-turn ESC / Ctrl-C monitor for the sync REPL. Polls the terminal on a
/// dedicated thread and sends `EngineCommand::Cancel` across the seam when the
/// user aborts. `suspend()`/`resume()` let the turn hand stdin to an interactive
/// prompter (permission / question dialog) without the monitor eating the reply.
struct ReplTurnCancelMonitor {
    ctl: Arc<MonitorCtl>,
    join: Option<thread::JoinHandle<()>>,
}

impl ReplTurnCancelMonitor {
    fn install(commands: mpsc::Sender<EngineCommand>) -> Self {
        let ctl = Arc::new(MonitorCtl {
            inner: Mutex::new(MonitorInner::default()),
            cv: std::sync::Condvar::new(),
        });
        let ctl_thread = Arc::clone(&ctl);
        let join = thread::Builder::new()
            .name("repl-cancel-monitor".into())
            .spawn(move || {
                // Without an interactive terminal there is nothing to poll; mark
                // parked so `suspend()` never blocks, and idle until stopped.
                if !io::stdin().is_terminal() {
                    let mut guard = ctl_thread
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.suspended = true;
                    ctl_thread.cv.notify_all();
                    while !guard.stop {
                        guard = ctl_thread
                            .cv
                            .wait(guard)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    return;
                }

                #[cfg(unix)]
                let mut raw = enable_raw_mode_unix();
                #[cfg(not(unix))]
                let mut raw = crossterm::terminal::enable_raw_mode().is_ok();

                loop {
                    {
                        let mut guard = ctl_thread
                            .inner
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if guard.stop {
                            break;
                        }
                        if guard.suspend_requested {
                            if raw {
                                #[cfg(unix)]
                                disable_raw_mode_unix();
                                #[cfg(not(unix))]
                                {
                                    let _ = crossterm::terminal::disable_raw_mode();
                                }
                                raw = false;
                            }
                            guard.suspended = true;
                            ctl_thread.cv.notify_all();
                            while guard.suspend_requested && !guard.stop {
                                guard = ctl_thread
                                    .cv
                                    .wait(guard)
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                            }
                            guard.suspended = false;
                            if guard.stop {
                                break;
                            }
                            #[cfg(unix)]
                            {
                                raw = enable_raw_mode_unix();
                            }
                            #[cfg(not(unix))]
                            {
                                raw = crossterm::terminal::enable_raw_mode().is_ok();
                            }
                            continue;
                        }
                    }
                    match poll_abort_key(Duration::from_millis(50)) {
                        AbortKey::None => {}
                        AbortKey::Esc => {
                            let _ = commands.send(EngineCommand::Cancel);
                        }
                        AbortKey::CtrlC => {
                            if cancel::is_double_ctrlc() {
                                if raw {
                                    #[cfg(unix)]
                                    disable_raw_mode_unix();
                                    #[cfg(not(unix))]
                                    {
                                        let _ = crossterm::terminal::disable_raw_mode();
                                    }
                                }
                                eprintln!();
                                std::process::exit(0);
                            }
                            cancel::record_ctrlc();
                            let _ = commands.send(EngineCommand::Cancel);
                        }
                    }
                }

                if raw {
                    #[cfg(unix)]
                    disable_raw_mode_unix();
                    #[cfg(not(unix))]
                    {
                        let _ = crossterm::terminal::disable_raw_mode();
                    }
                }
            })
            .expect("spawn repl-cancel-monitor thread");
        Self {
            ctl,
            join: Some(join),
        }
    }

    /// Pause polling + restore cooked mode so an interactive prompter can read
    /// stdin. Blocks until the monitor has parked (or already inert).
    fn suspend(&self) {
        let mut guard = self
            .ctl
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.stop {
            return;
        }
        guard.suspend_requested = true;
        self.ctl.cv.notify_all();
        while !guard.suspended && !guard.stop {
            guard = self
                .ctl
                .cv
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Resume key polling after an interactive prompt.
    fn resume(&self) {
        let mut guard = self
            .ctl
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.suspend_requested = false;
        self.ctl.cv.notify_all();
    }
}

impl Drop for ReplTurnCancelMonitor {
    fn drop(&mut self) {
        {
            let mut guard = self
                .ctl
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.stop = true;
            self.ctl.cv.notify_all();
        }
        if let Some(join) = self.join.take() {
            // The monitor wakes within ~50ms of the stop signal. Bound the wait
            // so a wedged terminal can't hang the REPL between turns.
            let deadline = Instant::now() + Duration::from_millis(300);
            while !join.is_finished() {
                if Instant::now() >= deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            let _ = join.join();
        }
    }
}

fn run_repl_iocraft_dispatch(
    mut cli: LiveCli,
    mode: input_queue::QueueMode,
) -> Result<(), Box<dyn std::error::Error>> {
    cli.is_repl = true;
    let banner = cli.startup_banner();
    let permission_label = cli.lifecycle.current_permission_mode().as_str().to_string();

    // iocraft owns stdin (raw mode) and delivers Ctrl-C / ESC as key events;
    // those `InputEvent::Abort`s cancel the in-flight turn by sending Cancel
    // across the seam (no separate raw-mode HookAbortMonitor).
    let commands = cli.engine_handle.commands.clone();

    let shared_mode = input_queue::shared_queue_mode(mode);
    cli.shared_queue_mode = Some(Arc::clone(&shared_mode));

    // Spawn the iocraft REPL UI on a dedicated thread, then decompose the
    // handle so `input_rx` can be forwarded into the unified event channel.
    let repl = repl_ui::spawn_repl_ui(&permission_label, &banner);
    let (repl_output, repl_ui_cmd, input_rx, repl_spinner, repl_join) = repl.split();
    let pending_question_answer: PendingQuestionAnswer = Arc::new(Mutex::new(None));

    // Route LiveCli output through iocraft's OutputSender so it goes
    // through split_for_iocraft and renders correctly in raw mode.
    cli.iocraft_output = Some(repl_output.clone());
    let cli_shared = Arc::new(Mutex::new(cli));
    let session_start = Instant::now();

    // Unified coordinator event channel. All event sources (UI input,
    // A2A peer messages, turn completion) converge here so the loop
    // blocks on a single recv() with no timeout-based polling.
    let (coord_tx, coord_rx) = mpsc::channel::<CoordinatorEvent>();

    // Bridge: forward iocraft InputEvents as CoordinatorEvent::Human.
    let coord_tx_input = coord_tx.clone();
    let _input_bridge = thread::Builder::new()
        .name("input-bridge".into())
        .spawn(move || {
            while let Ok(evt) = input_rx.recv() {
                if coord_tx_input.send(CoordinatorEvent::Human(evt)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn input bridge");

    // nexus A2A receive-half: peer messages feed into the coordinator
    // event channel, replacing the old println side-channel. The REPL
    // loop handles display and (future) turn injection.
    if let Ok(Some(a2a_session)) = engine_host::nexus_a2a::session() {
        let coord_tx_a2a = coord_tx.clone();
        let _poller = engine_host::nexus_a2a::spawn_poller(
            a2a_session,
            runtime::HookAbortSignal::new(),
            move |msg| ack_after_coordinator_takes(&coord_tx_a2a, msg),
        );
    }

    // Local JSONL inbox poller: picks up messages that sub-agents
    // write to `.sudocode-inbox/team-lead.jsonl` via the `send` tool.
    // Complements the nexus A2A poller above — together they close the
    // receive loop for both local and cross-machine messaging.
    {
        let coord_tx_local = coord_tx.clone();
        let workspace = env::current_dir().unwrap_or_default();
        let _local_poller = runtime::mailbox::spawn_local_poller(
            workspace,
            "team-lead".to_string(),
            runtime::HookAbortSignal::new(),
            move |msg| ack_after_coordinator_takes(&coord_tx_local, msg),
        );
    }

    // Coordinator loop on the current thread. All events arrive through
    // `coord_rx` — no timeout-based polling needed.
    let coord = Arc::new(Mutex::new(input_queue::TurnInputCoordinator::new()));
    let mut turn_active = false;
    let mut runner_handle: Option<thread::JoinHandle<()>> = None;
    // Pending interactive slash command state: when a slash command needs
    // user selection (e.g. `/model` without args), we show a question via
    // iocraft's InputSlot and store the callback here. The coordinator
    // loop routes the QuestionAnswer to this closure instead of the
    // tool-question path.
    let mut pending_slash_selection: Option<SlashSelectionHandler> = None;

    loop {
        let event = match coord_rx.recv() {
            Ok(evt) => evt,
            Err(_) => {
                cancel_pending_question_answer(&pending_question_answer);
                repl_ui_cmd.clear_question();
                if let Some(h) = runner_handle.take() {
                    let _ = commands.send(EngineCommand::Cancel);
                    let _ = h.join();
                }
                break;
            }
        };

        match event {
            CoordinatorEvent::TurnComplete => {
                turn_active = false;
                if let Some(h) = runner_handle.take() {
                    let _ = h.join();
                }
                let next = coord.lock().unwrap().drain_next();
                if let Some(next) = next {
                    turn_active = true;
                    runner_handle = Some(spawn_iocraft_turn(
                        Arc::clone(&cli_shared),
                        next.prompt,
                        repl_output.clone(),
                        repl_ui_cmd.clone(),
                        repl_spinner.clone(),
                        Arc::clone(&pending_question_answer),
                        coord_tx.clone(),
                    ));
                }
                continue;
            }
            CoordinatorEvent::PeerMessage(msg, ack) => {
                repl_output.println(&format!("\n\u{1f4e8} A2A from {}: {}", msg.from, msg.body));
                let prompt = tools::compose_next_turn_from_envelopes(&[msg]);
                if !turn_active {
                    turn_active = true;
                    runner_handle = Some(spawn_iocraft_turn(
                        Arc::clone(&cli_shared),
                        prompt,
                        repl_output.clone(),
                        repl_ui_cmd.clone(),
                        repl_spinner.clone(),
                        Arc::clone(&pending_question_answer),
                        coord_tx.clone(),
                    ));
                } else {
                    coord
                        .lock()
                        .unwrap()
                        .submit_during_turn(prompt, input_queue::QueueMode::Queue);
                }
                // Taken: the message is this process's responsibility now, so
                // the receiver may advance its cursor. What remains — the
                // turn-input queue, an in-flight turn — are the same windows
                // human input has, and a human can retype.
                let _ = ack.send(());
                continue;
            }
            CoordinatorEvent::Human(input_event) => match input_event {
                repl_ui::InputEvent::Exit => {
                    cancel_pending_question_answer(&pending_question_answer);
                    repl_ui_cmd.clear_question();
                    if let Some(h) = runner_handle.take() {
                        let _ = commands.send(EngineCommand::Cancel);
                        let _ = h.join();
                    }
                    let cli_lock = cli_shared.lock().expect("LiveCli mutex poisoned");
                    if let Err(e) = cli_lock.persist_session() {
                        repl_output.println(&format!("{}{e}{}", ansi_fg(theme().error), RESET));
                    }
                    break;
                }
                repl_ui::InputEvent::Abort => {
                    cancel_pending_question_answer(&pending_question_answer);
                    repl_ui_cmd.clear_question();
                    if runner_handle.is_some() {
                        let _ = commands.send(EngineCommand::Cancel);
                    }
                }
                repl_ui::InputEvent::Submit(text) => {
                    if text.trim() == "/exit" || text.trim() == "/quit" {
                        cancel_pending_question_answer(&pending_question_answer);
                        repl_ui_cmd.clear_question();
                        if runner_handle.is_some() {
                            let _ = commands.send(EngineCommand::Cancel);
                        }
                        if let Some(h) = runner_handle.take() {
                            let _ = h.join();
                        }
                        let cli_lock = cli_shared.lock().expect("LiveCli mutex poisoned");
                        if let Err(e) = cli_lock.persist_session() {
                            repl_output.println(&format!("{}{e}{}", ansi_fg(theme().error), RESET));
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
                            pending_slash_selection =
                                Some(cli::config_ui::build_config_tree_handler(
                                    &repl_ui_cmd,
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
                            let models =
                                runtime::model_capabilities::merge_discovery_ids(&config_keys);
                            let current = cli_lock.lifecycle.current_model();
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
                                &repl_ui_cmd,
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
                                        repl_output.println(&format!(
                                            "{}{e}{}",
                                            ansi_fg(theme().error),
                                            RESET
                                        ));
                                    }
                                }
                                Ok(false) => {}
                                Err(e) => repl_output.println(&format!(
                                    "{}{e}{}",
                                    ansi_fg(theme().error),
                                    RESET
                                )),
                            }
                            true
                        }
                        Ok(None) => false,
                        Err(error) => {
                            repl_output.println(&format!(
                                "{}{error}{}",
                                ansi_fg(theme().error),
                                RESET
                            ));
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
                            next.prompt,
                            repl_output.clone(),
                            repl_ui_cmd.clone(),
                            repl_spinner.clone(),
                            Arc::clone(&pending_question_answer),
                            coord_tx.clone(),
                        ));
                    } else {
                        let outcome = coord
                            .lock()
                            .unwrap()
                            .submit_during_turn(text, input_queue::load_queue_mode(&shared_mode));
                        match outcome {
                            input_queue::SubmitOutcome::Queued => {}
                            input_queue::SubmitOutcome::Interrupt => {
                                let _ = commands.send(EngineCommand::Cancel);
                            }
                            input_queue::SubmitOutcome::Rejected => {
                                repl_output.println(
                                    &format!("{DIM}(a turn is running; set SUDOCODE_INTERRUPT_QUEUE_MODE=queue to queue instead){RESET}"),
                                );
                            }
                        }
                    }
                }
                repl_ui::InputEvent::QuestionAnswer(text) => {
                    if let Some(handler) = pending_slash_selection.take() {
                        pending_slash_selection = (handler.0)(&text, &cli_shared, &repl_output);
                    } else if !consume_pending_question_answer(&pending_question_answer, text) {
                        repl_output.println(&format!(
                            "{DIM}(no question is waiting for an answer){RESET}"
                        ));
                    }
                }
            },
        }
    }

    // Persist the session before shutting down the REPL — the UI thread may
    // still be alive and the shared CLI mutex must remain accessible.
    {
        let cli_lock = cli_shared.lock().expect("LiveCli mutex");
        let _ = cli_lock.persist_session();
    }
    // Drop coordinator sender so bridge/poller threads see Disconnected,
    // which drops input_rx and lets the iocraft render loop exit.
    drop(coord_tx);
    // Let iocraft unwind its render loop before exiting. Its terminal guard
    // restores raw mode, bracketed paste, cursor visibility, and mouse mode.
    // On Windows PTYs that do not exit promptly, the join closure abandons
    // the thread after a bounded wait and process exit remains the fallback.
    (repl_join)();

    // Unwrap the Arc and finalize telemetry. If the runner thread
    // still holds a clone, force-exit — session is already persisted.
    let cli = match Arc::try_unwrap(cli_shared) {
        Ok(m) => m.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(_) => std::process::exit(0),
    };

    let duration_ms = session_start.elapsed().as_millis() as u64;
    let usage_tracker = cli.lifecycle.usage_snapshot();
    let usage = usage_tracker.cumulative_usage();
    let total_turns = usage_tracker.turns();
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
    prompt: String,
    output: repl_ui::OutputSender,
    ui: repl_ui::UiCommandSender,
    spinner: repl_ui::SpinnerState,
    pending_question_answer: PendingQuestionAnswer,
    done_tx: mpsc::Sender<CoordinatorEvent>,
) -> thread::JoinHandle<()> {
    // The engine owns the abort signal (reset at the start of each turn, fired
    // by the pump on EngineCommand::Cancel), so the runner no longer manages it.
    thread::Builder::new()
        .name("repl-runner".into())
        .spawn(move || {
            let mut cli = cli_shared.lock().expect("LiveCli mutex poisoned");
            if let Err(e) =
                cli.run_turn_iocraft(&prompt, &output, &ui, &spinner, pending_question_answer)
            {
                output.println(&format!("{}{e}{}", ansi_fg(theme().error), RESET));
            }
            let _ = done_tx.send(CoordinatorEvent::TurnComplete);
        })
        .expect("spawn repl-runner thread")
}

/// The composition root above the seam. Holds ONLY the turn handle + the
/// session-lifecycle port — the engine owns the config/session/runtime SSOT
/// (audit finding B). It physically cannot drive a turn except through
/// `engine_handle` (no `EngineDelegate`), and cannot reach into the runtime.
struct LiveCli {
    /// The turn seam: `commands.send(Prompt/Cancel/PermissionAnswer/…)`,
    /// `events.recv()`. The ONLY way turns cross.
    engine_handle: EngineHandle,
    /// Non-turn session lifecycle (model/permission/auth switch, /clear,
    /// /resume, fork, compaction, reads). The engine holds the SSOT; this is
    /// the renderer's port to it. `Arc<dyn SessionLifecycle>` gives NO access
    /// to `run_turn` — the cut is compiler-enforced.
    lifecycle: Arc<dyn SessionLifecycle>,
    prompt_history: Vec<PromptHistoryEntry>,
    /// Tool-use ids already restored by `/undo`. Used to make repeated
    /// `/undo` calls step further back rather than re-undoing the same edit.
    undone_tool_use_ids: std::collections::HashSet<String>,
    /// Shared atomic queue mode for the async REPL. `/config set auto-interrupt`
    /// writes to this; the coordinator reads it each `submit_during_turn`.
    /// `Some` ⇔ async REPL mode is active.
    shared_queue_mode: Option<input_queue::SharedQueueMode>,
    /// True in REPL mode. Plan mode confirmation dialog only shows in REPL.
    is_repl: bool,
    /// When set, `out_println` routes through this sender instead of bare
    /// `println!`. Set by the iocraft REPL dispatch so slash command output
    /// goes through `split_for_iocraft` and renders correctly in raw mode.
    iocraft_output: Option<repl_ui::OutputSender>,
}

/// Outcome of [`clear_session_state`].
struct ClearedSession {
    /// Empty transcript to rebuild the runtime around (persistence path set).
    fresh: Session,
    /// Where `fresh` lives.
    handle: SessionHandle,
    /// Where the previous transcript lives — a managed session, so it stays
    /// resumable (`/resume <id>`).
    previous: SessionHandle,
}

/// REPL `/clear` core: the previous transcript stays on disk under its own
/// id, and a fresh one with a new id and file keeps the workspace root. The
/// caller rebuilds its runtime around `fresh` (model and permission mode
/// live in the runtime config, so they carry over).
fn clear_session_state(
    cwd: &Path,
    current_handle: &SessionHandle,
) -> Result<ClearedSession, Box<dyn std::error::Error>> {
    let fresh = new_cli_session_for(cwd)?;
    let handle = create_managed_session_handle_for(cwd, &fresh.session_id)?;
    Ok(ClearedSession {
        fresh: fresh.with_persistence_path(handle.path.clone()),
        handle,
        previous: current_handle.clone(),
    })
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

    let config = engine_acp::acp_sdk_server::SdkAcpConfig {
        agent_version: VERSION.to_string(),
        model,
        model_flag_raw,
        permission_mode_override,
        reasoning_effort,
        allowed_tools,
        auth_mode,
        git_sha: GIT_SHA.map(str::to_string),
        build_target: BUILD_TARGET.map(str::to_string),
    };
    let rt = tokio::runtime::Runtime::new()?;
    if let Some(port) = ws_port {
        rt.block_on(engine_acp::acp_ws_server::run_acp_ws_server(config, port))
    } else {
        rt.block_on(engine_acp::acp_stdio_server::run_acp_stdio_server(config))
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

/// The renderer-side capture of one turn: the `TurnComplete` aggregate plus the
/// JSON-output fields re-derived from the event stream (the old paths read these
/// off `TurnSummary`, which `TurnComplete` no longer carries — message vecs were
/// dropped from the seam type).
#[derive(Default)]
struct TurnOutcome {
    complete: Option<TurnComplete>,
    /// Set when the turn ended in an `EngineEvent::Error`; the non-interactive
    /// paths surface it as an `Err` (the old paths propagated the runtime error).
    error: Option<String>,
    final_text: String,
    tool_uses: Vec<serde_json::Value>,
    tool_results: Vec<serde_json::Value>,
    prompt_cache_events: Vec<serde_json::Value>,
}

/// A question prompter that declines to answer (empty selection). The
/// non-interactive turn paths install no interactive question UI, but the seam's
/// pump always sets a question adapter, so `AskUserQuestion` resolves to "no
/// answer" instead of wedging on a prompt nobody will service.
struct NoopQuestionPrompter;

impl runtime::QuestionPrompter for NoopQuestionPrompter {
    fn ask(
        &mut self,
        _request: &runtime::QuestionPromptRequest,
    ) -> Result<Vec<runtime::QuestionPromptAnswer>, String> {
        Ok(Vec::new())
    }
}

/// Renderer-side question prompter for the SYNC REPL. Draws the question +
/// numbered options and reads the choice via rustyline — a raw
/// `io::stdin().read_line` is unreliable under Windows ConPTY (it silently
/// dropped stdin writes during the old ExitPlanMode dialog); rustyline reads the
/// console the same Windows-safe way the REPL prompt does. Answers both
/// `AskUserQuestion` and the `ExitPlanMode` "Choose an action" dialog, now that
/// both cross the seam as `QuestionRequest`s.
struct CliQuestionPrompter;

impl runtime::QuestionPrompter for CliQuestionPrompter {
    fn ask(
        &mut self,
        request: &runtime::QuestionPromptRequest,
    ) -> Result<Vec<runtime::QuestionPromptAnswer>, String> {
        if let Some(title) = &request.title {
            println!();
            println!("{title}");
        }
        if let Some(description) = &request.description {
            for line in description.lines() {
                println!("  {line}");
            }
        }
        let mut answers = Vec::new();
        for field in &request.fields {
            if !field.prompt.is_empty() && request.title.as_deref() != Some(field.prompt.as_str()) {
                println!();
                println!("{}", field.prompt);
            }
            for (idx, option) in field.options.iter().enumerate() {
                println!("  [{}] {}", idx + 1, option.label);
            }
            let mut editor = rustyline::DefaultEditor::new().map_err(|e| e.to_string())?;
            let line = editor
                .readline("Your choice: ")
                .map_err(|e| e.to_string())?;
            let trimmed = line.trim();
            // A 1-indexed digit picks the option's value; otherwise the raw text
            // (custom-input fields).
            let value = trimmed
                .parse::<usize>()
                .ok()
                .and_then(|idx| field.options.get(idx.wrapping_sub(1)))
                .map_or_else(|| trimmed.to_string(), |option| option.value.clone());
            let label = field
                .options
                .iter()
                .find(|option| option.value == value)
                .map(|option| option.label.clone());
            answers.push(runtime::QuestionPromptAnswer {
                id: field.id.clone(),
                value,
                label,
            });
        }
        Ok(answers)
    }
}

impl LiveCli {
    /// True when the async REPL (queue mode) is active. In this mode the
    /// input thread owns stdin via rustyline, so interactive widgets
    /// (FuzzySelect, Select) cannot be used on the runner thread.
    fn is_async_mode(&self) -> bool {
        self.shared_queue_mode.is_some()
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
        reasoning_effort: Option<String>,
        auth_mode: Option<AuthMode>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // The engine always builds with tools enabled for the interactive
        // REPL; `enable_tools=false` is a non-interactive one-shot knob the
        // seam doesn't model (SessionEngine is single-purpose here).
        let _ = enable_tools;
        let system_prompt = build_system_prompt()?;
        let cwd = env::current_dir()?;
        let sudocode_config = require_sudocode_config_for_cwd(&cwd)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        // Load model capabilities SSOT (bundled fallback or cached from last refresh).
        let config_home = runtime::default_config_home();
        runtime::model_capabilities::load(&config_home, &runtime::fs_backend::StdFsBackend);

        let auth_resolved = resolve_auth_mode(&model, auth_mode, &sudocode_config)?;
        tools::set_global_auth_mode(auth_resolved);

        // Fire-and-forget: refresh model capabilities from sudorouter if stale.
        // The engine owns its own tokio runtime; the renderer keeps none, so
        // this rides a detached thread with its own short-lived current-thread rt.
        if runtime::model_capabilities::is_stale(&config_home, &runtime::fs_backend::StdFsBackend) {
            if let Some((base_url, api_key)) =
                engine_host::config::extract_sudorouter_credentials(&sudocode_config)
            {
                let ch = config_home.clone();
                std::thread::spawn(move || {
                    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    else {
                        return;
                    };
                    rt.block_on(async move {
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
                });
            }
        }

        // The one engine owns config + session + runtime SSOT (thinking config,
        // persistence, tracer are all applied inside `SessionEngine::build`).
        let mcp_servers = std::collections::BTreeMap::new();
        let engine = Arc::new(
            SessionEngine::build(
                &cwd,
                &mcp_servers,
                runtime::SystemPromptOverrides::default(),
                // The REPL always uses memory; the per-session switch is an
                // ACP-client knob (`_meta.sudocode.memory`).
                runtime::memory::MemoryMode::Enabled,
                system_prompt,
                model.clone(),
                Some(model),
                allowed_tools,
                Some(permission_mode),
                reasoning_effort,
                auth_mode,
            )
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?,
        );
        let engine_handle = EngineSession::spawn(engine.clone() as Arc<dyn EngineDelegate>);
        let lifecycle = engine as Arc<dyn SessionLifecycle>;

        // Record session started event.
        let is_child_process = std::env::var("SUDOWORK_CHILD_PROCESS").is_ok();
        let mode = if is_child_process {
            "child"
        } else {
            "standalone"
        };
        if let Some(tracer) = lifecycle.session_tracer() {
            tracer.record_session_started(
                VERSION,
                cwd.to_string_lossy(),
                mode,
                &lifecycle.current_model(),
            );
        }

        Ok(Self {
            engine_handle,
            lifecycle,
            prompt_history: Vec::new(),
            undone_tool_use_ids: std::collections::HashSet::new(),
            shared_queue_mode: None,
            is_repl: false,
            iocraft_output: None,
        })
    }

    /// A clone of the session tracer, if telemetry is active (owned — the
    /// tracer is `Arc`-backed and cheap to clone).
    fn session_tracer(&self) -> Option<telemetry::SessionTracer> {
        self.lifecycle.session_tracer()
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
        let handle = self.lifecycle.session_handle();
        let model = self.lifecycle.current_model();
        let auth_mode = self.lifecycle.current_auth_mode();
        let permission_mode = self.lifecycle.current_permission_mode();
        let sudocode_config = load_sudocode_config_for_current_dir();
        let session_path = handle.path.strip_prefix(Path::new(&cwd)).map_or_else(
            |_| handle.path.display().to_string(),
            |path| path.display().to_string(),
        );

        // Auth mode line.
        let auth_mode_str = auth_mode.label().to_string();

        // Endpoint from config-driven resolution.
        let endpoint =
            engine_core::resolve_provider_from_config(&model, Some(auth_mode), &sudocode_config)
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
            format!("  {DIM}Model{RESET}            {}", model),
            format!("  {DIM}Auth mode{RESET}        {}", auth_mode_str),
            format!("  {DIM}Endpoint{RESET}         {}", endpoint),
            format!(
                "  {DIM}Permissions{RESET}      {}",
                permission_mode.as_str()
            ),
            format!("  {DIM}Branch{RESET}           {}", git_branch),
            format!("  {DIM}Workspace{RESET}        {}", workspace),
            format!("  {DIM}Directory{RESET}        {}", cwd),
            format!("  {DIM}Session{RESET}          {}", handle.id),
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
        let handle = self.lifecycle.session_handle();
        Ok(slash_command_completion_candidates_with_sessions(
            &self.lifecycle.current_model(),
            Some(&handle.id),
            list_managed_sessions()?
                .into_iter()
                .map(|session| session.id)
                .collect(),
        ))
    }

    /// Drive one turn across the seam: send the prompt, pump events into the
    /// renderer, and answer permission / question requests using the supplied
    /// prompters (the SAME `CliPermissionPrompter` / `IocraftQuestionPrompter`
    /// the old paths installed on the runtime — now consulted above the seam).
    /// `output` routes the renderer to the iocraft `OutputSender` or bare stdout
    /// (`None`). Returns the captured `TurnComplete` (`None` on error before
    /// completion). The end-of-turn status line is the caller's job (it varies
    /// per path).
    fn drive_turn(
        &self,
        input: &str,
        spinner_ref: Option<render::SpinnerRef>,
        output: Option<&repl_ui::OutputSender>,
        ui: Option<&repl_ui::UiCommandSender>,
        render: bool,
        permission_prompter: &mut dyn runtime::PermissionPrompter,
        question_prompter: &mut dyn runtime::QuestionPrompter,
        cancel_monitor: Option<&ReplTurnCancelMonitor>,
    ) -> Result<TurnOutcome, Box<dyn std::error::Error>> {
        // The interactive paths draw the stream; the `--output-format` paths
        // collect silently (they print only the final text / JSON), so the
        // renderer is optional. Without it we still detect the same outcomes
        // (Done / permission / question) straight from the event kinds.
        let mut renderer = render.then(|| EngineEventRenderer::new(spinner_ref, output.cloned()));
        let blocks = vec![runtime::ContentBlock::Text {
            text: input.to_string(),
        }];
        self.engine_handle
            .commands
            .send(EngineCommand::Prompt { blocks })?;

        let mut outcome = TurnOutcome::default();
        loop {
            let Ok(ev) = self.engine_handle.events.recv() else {
                break;
            };
            // Collect the JSON-output data from the event stream (the old paths
            // read it off the finished turn; TurnComplete drops the message
            // vecs, so we re-derive here). `final_text` tracks the LAST assistant
            // message only — reset it at each ToolCall (a message boundary), the
            // last-assistant-message semantics the JSON output has always used.
            match &ev {
                EngineEvent::TurnComplete(tc) => outcome.complete = Some(tc.clone()),
                EngineEvent::Error { message } => outcome.error = Some(message.clone()),
                EngineEvent::TextDelta { text } => outcome.final_text.push_str(text),
                EngineEvent::ToolCall { id, name, input } => {
                    outcome.final_text.clear();
                    // Parity: `--output-format json` emits the tool input as the
                    // raw argument STRING exactly as the model produced it — the
                    // pre-seam `collect_tool_uses` serialized `ToolUse.input`
                    // verbatim and the mock parity harness pins that shape. Do
                    // NOT parse it into a nested object.
                    outcome.tool_uses.push(serde_json::json!({
                        "id": id,
                        "name": name,
                        "input": input,
                    }));
                }
                EngineEvent::ToolResult {
                    id,
                    name,
                    output,
                    is_error,
                } => {
                    // A successful task mutation changes the shared task list.
                    // The iocraft REPL's context panel derives live from the
                    // tool-result stream it already receives across the seam —
                    // NOT from an engine-side side-channel into the executor
                    // (that was a boundary leak, removed with `set_ui_sender`).
                    //
                    // Canonicalize first: this name is whatever the model
                    // spelled, and matching it raw is how the `Task*` → `pid_*`
                    // rename would leave the panel stale on every `pid_kill`.
                    if let Some(ui) = ui {
                        if !*is_error
                            && matches!(
                                tools::canonicalize_tool_name(name).as_str(),
                                "TaskCreate" | "TaskUpdate" | "pid_status" | "pid_kill"
                            )
                        {
                            ui.update_context(tools::global_task_list());
                        }
                    }
                    outcome.tool_results.push(serde_json::json!({
                        "tool_use_id": id,
                        "tool_name": name,
                        "output": output,
                        "is_error": is_error,
                    }));
                }
                EngineEvent::PromptCache(event) => {
                    outcome.prompt_cache_events.push(serde_json::json!({
                        "unexpected": event.unexpected,
                        "reason": event.reason,
                        "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                        "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                        "token_drop": event.token_drop,
                    }));
                }
                _ => {}
            }
            let action = match renderer.as_mut() {
                Some(r) => r.render(ev),
                None => match ev {
                    EngineEvent::TurnComplete(_) | EngineEvent::Error { .. } => RenderOutcome::Done,
                    EngineEvent::PermissionRequest { id, request } => {
                        RenderOutcome::NeedPermission { id, request }
                    }
                    EngineEvent::QuestionRequest { id, request } => {
                        RenderOutcome::NeedQuestion { id, request }
                    }
                    _ => RenderOutcome::Continue,
                },
            };
            match action {
                RenderOutcome::Continue => {}
                RenderOutcome::NeedPermission { id, request } => {
                    // The prompter reads stdin (cooked mode); pause the sync-REPL
                    // key monitor so it doesn't steal the approval keystrokes.
                    if let Some(monitor) = cancel_monitor {
                        monitor.suspend();
                    }
                    let decision = permission_prompter.decide(&request);
                    if let Some(monitor) = cancel_monitor {
                        monitor.resume();
                    }
                    self.engine_handle
                        .commands
                        .send(EngineCommand::PermissionAnswer { id, decision })?;
                }
                RenderOutcome::NeedQuestion { id, request } => {
                    if let Some(monitor) = cancel_monitor {
                        monitor.suspend();
                    }
                    let answers = question_prompter.ask(&request).unwrap_or_default();
                    if let Some(monitor) = cancel_monitor {
                        monitor.resume();
                    }
                    self.engine_handle
                        .commands
                        .send(EngineCommand::QuestionAnswer { id, answers })?;
                }
                RenderOutcome::Done => break,
            }
        }
        Ok(outcome)
    }

    /// Async-REPL driver entrypoint: no interactive key monitor (the async
    /// iocraft REPL owns stdin and cancels via its own input path).
    fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.run_turn_impl(input, false)
    }

    /// Interactive-terminal entrypoint (sync REPL + one-shot text): install a
    /// per-turn ESC / Ctrl-C key monitor that cancels the turn across the seam.
    /// The turn otherwise blocks on the engine with no key reader; the monitor
    /// is inert off a pipe, so non-interactive runs are unaffected (they cancel
    /// via SIGINT through the `SignalCancelGuard`).
    fn run_turn_interactive(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.run_turn_impl(input, true)
    }

    fn run_turn_impl(
        &mut self,
        input: &str,
        interactive_cancel: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let turn_start = Instant::now();
        let token_budget = crate::render::parse_token_budget(input);
        let model = self.lifecycle.current_model();
        let mut spinner = SpinnerHandle::new(
            "🦀 Thinking...",
            Some(model.as_str()),
            TerminalRenderer::new().color_theme(),
            token_budget,
        );
        let spinner_ref = spinner.spinner_ref();

        let mut permission_prompter: Box<dyn runtime::PermissionPrompter> =
            if io::stdin().is_terminal() {
                Box::new(CliPermissionPrompter::new(
                    self.lifecycle.current_permission_mode(),
                ))
            } else {
                Box::new(AutoDenyPermissionPrompter)
            };
        // Interactive REPL: draw AskUserQuestion / ExitPlanMode dialogs and read
        // the choice (Windows-safe via rustyline). One-shot: no interactive user.
        let mut question_prompter: Box<dyn runtime::QuestionPrompter> = if self.is_repl {
            Box::new(CliQuestionPrompter)
        } else {
            Box::new(NoopQuestionPrompter)
        };

        // Sync REPL only: catch ESC / Ctrl-C during the (otherwise blocking)
        // turn and cancel across the seam. Dropped right after the turn so the
        // next rustyline prompt owns stdin again.
        let cancel_monitor = interactive_cancel
            .then(|| ReplTurnCancelMonitor::install(self.engine_handle.commands.clone()));
        let outcome = self.drive_turn(
            input,
            Some(spinner_ref),
            None,
            None,
            true,
            permission_prompter.as_mut(),
            question_prompter.as_mut(),
            cancel_monitor.as_ref(),
        )?;
        drop(cancel_monitor);

        match outcome.complete {
            Some(tc) if tc.cancelled => spinner.fail("⏹ Cancelled"),
            Some(tc) => {
                spinner.clear();
                if let Some(event) = tc.auto_compaction {
                    self.out_println(format_auto_compaction_notice(event.removed_message_count));
                }
                self.print_turn_status_line(&model, turn_start.elapsed(), None, None);
            }
            None => {
                clear_pending_plan_execution();
                spinner.fail("❌ Request failed");
            }
        }

        // If the plan confirmation dialog chose "clear context & execute", pick
        // up the plan and re-run in a fresh session (the engine preserves the
        // current model across the reset).
        if let Some(plan) = take_pending_plan_execution() {
            self.lifecycle.reset_session()?;
            let prompt = format!("Implement the following plan:\n\n{plan}");
            return self.run_turn_impl(&prompt, interactive_cancel);
        }
        Ok(())
    }

    /// Print the end-of-turn status line. Usage/turns come from a lifecycle
    /// snapshot (the engine's live `UsageTracker`), context-window from the
    /// response model (falling back to the active model). Routes to the iocraft
    /// `OutputSender` when present, else the plain `out_println`.
    fn print_turn_status_line(
        &self,
        model: &str,
        elapsed: Duration,
        output: Option<&repl_ui::OutputSender>,
        ui: Option<&repl_ui::UiCommandSender>,
    ) {
        let usage_tracker = self.lifecycle.usage_snapshot();
        let usage = usage_tracker.current_turn_usage();
        let turns = usage_tracker.turns();
        // Current context-window occupancy (what the provider just processed),
        // the same metric auto-compaction uses — not the session-cumulative
        // total, which never shrinks and overshoots the window. Window is sized
        // off the session model so the percentage and the compaction trigger
        // share one denominator.
        let context_tokens = usage.context_tokens();
        let context_window = runtime::model_capabilities::context_window_or_default(model);
        let branch = env::current_dir()
            .ok()
            .and_then(|cwd| resolve_git_branch_for(&cwd));
        // Read once per turn, from the engine's own selector rather than a
        // second copy of the precedence rules. A turn takes seconds; the config
        // read behind this does not register next to it.
        let account = self.lifecycle.current_billing_account();
        let line = format_turn_status_line(&TurnStatus {
            model,
            turn: turns,
            usage: &usage,
            context_tokens: Some(context_tokens),
            context_window: Some(context_window),
            elapsed,
            branch: branch.as_deref(),
            account: account.name(),
        });
        match (ui, output) {
            // Show in the ChromeSlot only (visible until the next turn). The
            // status line is deliberately NOT echoed to scrollback: the
            // ChromeSlot already renders it above the separator, and printing
            // it again duplicated the line in the terminal history.
            (Some(ui), Some(_)) => {
                ui.set_turn_result(&line);
            }
            (Some(ui), None) => ui.set_turn_result(&line),
            (None, Some(out)) => out.println(&line),
            (None, None) => self.out_println(line),
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
        let token_budget = crate::render::parse_token_budget(input);
        let model = self.lifecycle.current_model();

        // Activate the shared spinner state for the iocraft render loop, then
        // bridge it into the SpinnerRef the renderer drives (bytes + thinking).
        spinner_state.start_turn("\u{1f980} Thinking...", Some(model.as_str()), token_budget);
        let spinner_ref = render::SpinnerRef::from_spinner_state(spinner_state);

        let mut permission_prompter =
            CliPermissionPrompter::new(self.lifecycle.current_permission_mode());
        let mut question_prompter =
            IocraftQuestionPrompter::new(ui.clone(), pending_question_answer);

        let outcome = self.drive_turn(
            input,
            Some(spinner_ref),
            Some(output),
            Some(&ui),
            true,
            &mut permission_prompter,
            &mut question_prompter,
            None,
        )?;
        spinner_state.stop_turn();

        match outcome.complete {
            Some(tc) if tc.cancelled => output.println(&format!(
                "{}\u{23f9} Cancelled{}",
                ansi_fg(theme().error),
                RESET
            )),
            Some(tc) => {
                if let Some(event) = tc.auto_compaction {
                    output.println(&format_auto_compaction_notice(event.removed_message_count));
                }
                // The status line is part of the transcript: printed once, in
                // order, above the next prompt.
                self.print_turn_status_line(&model, turn_start.elapsed(), Some(output), Some(ui));
            }
            // Error already rendered by the EngineEventRenderer (Error event).
            None => {}
        }
        Ok(())
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
            CliOutputFormat::Text => self.run_turn_interactive(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
        }
    }

    /// Drive one non-interactive turn (no stream render), returning the captured
    /// outcome. The caller supplies the permission prompter because the parity
    /// contract differs per format: `--output-format json` ALWAYS uses the
    /// interactive `CliPermissionPrompter` (it prints the permission box + reads
    /// y/N from stdin, piped or TTY), whereas the compact paths auto-deny off a
    /// pipe so the prompt can't corrupt the single-line output. Surfaces a turn
    /// error as an `Err` (parity with the old `result?`). The engine persists the
    /// session itself, so callers only format the result.
    fn run_noninteractive_turn(
        &self,
        input: &str,
        permission_prompter: &mut dyn runtime::PermissionPrompter,
    ) -> Result<TurnOutcome, Box<dyn std::error::Error>> {
        let mut question_prompter = NoopQuestionPrompter;
        let outcome = self.drive_turn(
            input,
            None,
            None,
            None,
            false,
            permission_prompter,
            &mut question_prompter,
            None,
        )?;
        if let Some(message) = outcome.error {
            return Err(message.into());
        }
        Ok(outcome)
    }

    /// Permission prompter for the compact one-shot paths: interactive when
    /// stdin is a TTY, else auto-deny (an approval box printed to a pipe would
    /// corrupt compact mode's single-line stdout). Matches the pre-seam
    /// `run_prompt_compact*` selection.
    fn compact_permission_prompter(&self) -> Box<dyn runtime::PermissionPrompter> {
        if io::stdin().is_terminal() {
            Box::new(CliPermissionPrompter::new(
                self.lifecycle.current_permission_mode(),
            ))
        } else {
            Box::new(AutoDenyPermissionPrompter)
        }
    }

    fn run_prompt_compact(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut permission_prompter = self.compact_permission_prompter();
        let outcome = self.run_noninteractive_turn(input, permission_prompter.as_mut())?;
        self.out_println(outcome.final_text);
        Ok(())
    }

    fn run_prompt_compact_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let model = self.lifecycle.current_model();
        let mut permission_prompter = self.compact_permission_prompter();
        let outcome = self.run_noninteractive_turn(input, permission_prompter.as_mut())?;
        let tc = outcome
            .complete
            .ok_or_else(|| "engine turn did not complete".to_string())?;
        self.out_println(
            json!({
                "message": outcome.final_text,
                "compact": true,
                "model": model,
                "usage": {
                    "input_tokens": tc.turn_usage.input_tokens,
                    "output_tokens": tc.turn_usage.output_tokens,
                    "cache_creation_input_tokens": tc.turn_usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": tc.turn_usage.cache_read_input_tokens,
                },
            })
            .to_string(),
        );
        Ok(())
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let model = self.lifecycle.current_model();
        // Parity: the JSON one-shot ALWAYS uses the interactive permission
        // prompter, even off a pipe — it prints the permission box and reads
        // the y/N approval from stdin (the mock parity harness feeds "y\n" and
        // asserts "Permission required" / "Approve this tool call?" on stdout).
        let mut permission_prompter =
            CliPermissionPrompter::new(self.lifecycle.current_permission_mode());
        let outcome = self.run_noninteractive_turn(input, &mut permission_prompter)?;
        let tc = outcome
            .complete
            .ok_or_else(|| "engine turn did not complete".to_string())?;
        let auto_compaction_json = tc.auto_compaction.map(|event| {
            json!({
                "removed_messages": event.removed_message_count,
                "notice": format_auto_compaction_notice(event.removed_message_count),
            })
        });
        let estimated_cost = format_usd(
            tc.turn_usage
                .estimate_cost_usd_with_pricing(
                    pricing_for_model(&model)
                        .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier),
                )
                .total_cost_usd(),
        );
        self.out_println(
            json!({
                "message": outcome.final_text,
                "model": model,
                "iterations": tc.iterations,
                "auto_compaction": auto_compaction_json,
                "tool_uses": outcome.tool_uses,
                "tool_results": outcome.tool_results,
                "prompt_cache_events": outcome.prompt_cache_events,
                "usage": {
                    "input_tokens": tc.turn_usage.input_tokens,
                    "output_tokens": tc.turn_usage.output_tokens,
                    "cache_creation_input_tokens": tc.turn_usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": tc.turn_usage.cache_read_input_tokens,
                },
                "estimated_cost": estimated_cost,
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
            SlashCommand::Account { account } => self.set_billing_account(account)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => {
                let resumed = self.load_session(session_path)?;
                if resumed {
                    let handle = self.lifecycle.session_handle();
                    let session = self.lifecycle.session_snapshot();
                    self.out_println(format_resume_report(
                        &handle.path.display().to_string(),
                        session.messages.len(),
                        self.lifecycle.usage_snapshot().turns(),
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
                        match self.lifecycle.mcp_command(action_str, server_name) {
                            Some(Ok(msg)) => self.out_println(msg),
                            Some(Err(err)) => self.out_println(format!("Error: {err}")),
                            None => self.out_println(
                                "No MCP servers are running in this session.\n\
                                 Hint: if you just added a server via `/mcp add-json`, \
                                 restart scode to load it.",
                            ),
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
                    Some(&self.lifecycle.plugin_load_outcome()),
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
                self.out_println(render_doctor_report(&build_info())?.render());
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Stats => {
                let session = self.lifecycle.session_snapshot();
                let usage = UsageTracker::from_session(&session).cumulative_usage();
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
                self.out_println(format!("{cmd_name} is not yet implemented in this build."));
                false
            }
            SlashCommand::Unknown(name) => {
                self.out_println(format_unknown_slash_command(&name));
                false
            }
        })
    }

    fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.lifecycle.persist().map_err(Into::into)
    }

    fn print_status(&self) {
        let usage = self.lifecycle.usage_snapshot();
        let cumulative = usage.cumulative_usage();
        let latest = usage.current_turn_usage();
        let session = self.lifecycle.session_snapshot();
        let handle = self.lifecycle.session_handle();
        let report = format_status_report(
            &self.lifecycle.current_model(),
            StatusUsage {
                message_count: session.messages.len(),
                turns: usage.turns(),
                latest,
                cumulative,
                estimated_tokens: self.lifecycle.estimated_tokens(),
            },
            self.lifecycle.current_permission_mode().as_str(),
            &status_context(Some(&handle.path)).expect("status context should load"),
            None, // #148: REPL /status doesn't carry flag provenance
            &self.lifecycle.current_billing_account().describe(),
        );
        self.out_suspend(|| print_with_pager(&report));
    }

    fn record_prompt_history(&mut self, prompt: &str) {
        let updated_at_ms = self.lifecycle.session_snapshot().updated_at_ms;
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(updated_at_ms, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = PromptHistoryEntry {
            timestamp_ms,
            text: prompt.to_string(),
        };
        self.prompt_history.push(entry);
        let mut push_error = None;
        self.lifecycle.with_session_mut(&mut |session| {
            if let Err(error) = session.push_prompt_entry(prompt) {
                push_error = Some(error.to_string());
            }
        });
        if let Some(error) = push_error {
            eprintln!("warning: failed to persist prompt history: {error}");
        }
    }

    fn print_prompt_history(&self, count: Option<&str>) {
        let limit = match parse_history_count(count) {
            Ok(limit) => limit,
            Err(message) => {
                self.out_println(message);
                return;
            }
        };
        let session = self.lifecycle.session_snapshot();
        let session_entries = &session.prompt_history;
        let entries = if session_entries.is_empty() {
            if self.prompt_history.is_empty() {
                collect_session_prompt_history(&session)
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
            let current = self.lifecycle.current_model();
            let default_idx = models.iter().position(|m| *m == current).unwrap_or(0);
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

        // The engine owns the switch (resolve + rebuild + keep session.model in
        // sync); it returns report DATA and we format it.
        let report = self.lifecycle.set_model(&model)?;
        if report.changed {
            self.undone_tool_use_ids.clear();
            self.out_println(format_model_switch_report(
                &report.previous,
                &report.resolved,
                report.message_count,
            ));
            Ok(true)
        } else {
            self.out_println(format_model_report(
                &report.resolved,
                report.message_count,
                report.turns,
                &load_sudocode_config_for_current_dir(),
            ));
            Ok(false)
        }
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let current = self.lifecycle.current_permission_mode();
        let Some(mode) = mode else {
            self.out_println(format_permissions_report(current.as_str()));
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if normalized == current.as_str() {
            self.out_println(format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = current.as_str().to_string();
        self.lifecycle
            .set_permission_mode(permission_mode_from_label(normalized))?;
        self.out_println(format_permissions_switch_report(&previous, normalized));
        Ok(true)
    }

    /// `/account [name]` — report who pays, or switch to another configured
    /// account. Returns whether the session was rebuilt.
    fn set_billing_account(
        &mut self,
        account: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let current = self.lifecycle.current_billing_account();

        let Some(account) = account else {
            self.out_println(format_account_report(
                &current.describe(),
                current.name(),
                &self.lifecycle.billing_accounts(),
            ));
            return Ok(false);
        };

        if current.name() == Some(account.trim()) {
            self.out_println(format_account_report(
                &current.describe(),
                current.name(),
                &self.lifecycle.billing_accounts(),
            ));
            return Ok(false);
        }

        let previous = current.describe();
        let switched = self.lifecycle.set_billing_account(&account)?;
        self.out_println(format_account_switch_report(
            &previous,
            &switched.describe(),
        ));
        Ok(true)
    }

    fn set_auth(&mut self, mode: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let current_str = self.lifecycle.current_auth_mode().as_str().to_string();

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
        self.lifecycle.set_auth(parsed)?;
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

        let previous_session = self.lifecycle.session_handle();
        let model = self.lifecycle.current_model();
        let permission_mode = self.lifecycle.current_permission_mode();
        let new_handle = self.lifecycle.reset_session()?;
        self.undone_tool_use_ids.clear();
        self.out_println(format!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            model,
            permission_mode.as_str(),
            new_handle.id,
            new_handle.path.display(),
        ));
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.lifecycle.usage_snapshot().cumulative_usage();
        self.out_println(format_cost_report(cumulative));
    }

    /// Load a session by reference (id, path, or "latest") — the engine loads
    /// it and swaps it in. Pure data operation; callers report the result.
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

        self.lifecycle.resume_session(&session_ref)?;
        self.undone_tool_use_ids.clear();
        Ok(true)
    }

    /// Resume a pre-resolved session (for `--resume` at startup). The engine
    /// reloads it by reference and swaps it in.
    fn replace_with_session(
        &mut self,
        _session: runtime::Session,
        handle: SessionHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.lifecycle.resume_session(&handle.id)?;
        self.undone_tool_use_ids.clear();
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
                    self.out_println("Usage: /config set auto-interrupt on|off");
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
                // Everything else routes through the single SSOT config writer
                // (`tools::set_config_setting`) so `/config set` persists to the
                // scope-appropriate settings file instead of a session-only,
                // divergent in-memory copy. `auto-interrupt`/`queue` above stay
                // session toggles by design (no on-disk representation).
                match tools::set_config_setting(key, value) {
                    Ok(msg) => self.out_println(format!("{DIM}{msg}{RESET}")),
                    Err(err) => eprintln!("Error: {err}"),
                }
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
        let outcome = self.lifecycle.plugin_load_outcome();
        print_skills_for_outcome(args, output_format, &cwd, Some(&outcome))
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
        let session = self.lifecycle.session_snapshot();
        let messages = &session.messages;
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
        let session = self.lifecycle.session_snapshot();
        let export_path = resolve_export_path(requested_path, &session)?;
        fs::write(&export_path, render_export_text(&session))?;
        self.out_println(format!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            session.messages.len(),
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
                // On a TTY, present a fuzzy picker that switches on Enter and
                // is silent on Esc. In async (iocraft) mode the render loop
                // owns the TTY, so a dialoguer widget on this thread would
                // fight it and corrupt the terminal — issue #577. Fall back
                // to the plain list + `/session switch <id>` hint there.
                if !self.is_async_mode() && io::stdin().is_terminal() && io::stdout().is_terminal()
                {
                    let sessions = list_managed_sessions()?;
                    if sessions.is_empty() {
                        self.out_println(render_session_list(&self.lifecycle.session_handle().id)?);
                        return Ok(false);
                    }
                    let default_idx = sessions
                        .iter()
                        .position(|session| session.id == self.lifecycle.session_handle().id)
                        .unwrap_or(0);
                    let items: Vec<String> = sessions
                        .iter()
                        .map(|session| {
                            format_session_picker_entry(
                                session,
                                &self.lifecycle.session_handle().id,
                            )
                        })
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
                    if target == self.lifecycle.session_handle().id {
                        self.out_println(format!("Session unchanged (already active: {target})."));
                        return Ok(false);
                    }
                    return self.handle_session_command(Some("switch"), Some(&target));
                }
                self.out_println(render_session_list(&self.lifecycle.session_handle().id)?);
                if self.is_async_mode() {
                    self.out_println("Use `/session switch <session-id>` to switch to a session.");
                }
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    self.out_println("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let (handle, message_count) = self.lifecycle.resume_session(target)?;
                self.undone_tool_use_ids.clear();
                self.out_println(format!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    handle.id,
                    handle.path.display(),
                    message_count,
                ));
                Ok(true)
            }
            Some("fork") => {
                let parent_session_id = self.lifecycle.session_handle().id;
                let (handle, message_count, branch_name) =
                    self.lifecycle.fork_session(target.map(ToOwned::to_owned))?;
                self.undone_tool_use_ids.clear();
                self.out_println(format!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    handle.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    handle.path.display(),
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
                if handle.id == self.lifecycle.session_handle().id {
                    self.out_println(format!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    ));
                    return Ok(false);
                }
                if self.is_async_mode() {
                    // In async (iocraft) mode the render loop owns the TTY;
                    // a blocking `read_line` prompt on this thread deadlocks
                    // with it — see issue #577. Require `--force` instead.
                    self.out_println(format!(
                        "delete: interactive confirmation is not available in the async REPL.\nRun `/session delete {} --force` to skip confirmation.",
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
                if handle.id == self.lifecycle.session_handle().id {
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
        self.lifecycle.reload_features().map_err(Into::into)
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (removed, kept, skipped, summary_source) = self.lifecycle.run_compaction()?;
        self.out_println(format_compact_report(
            removed,
            kept,
            skipped,
            &summary_source,
        ));
        Ok(())
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
        let session = self.lifecycle.session_snapshot();
        self.out_println(render_last_tool_debug_report(&session)?);
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

#[cfg(test)]
mod wait_notice_tests {
    use super::*;

    /// A turn that finishes normally must print nothing. The notice exists for
    /// the pathological case; if it spoke on every run it would be noise, and
    /// noise is what people learn to ignore.
    #[test]
    fn says_nothing_when_the_turn_finishes_promptly() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        {
            let sink = Arc::clone(&lines);
            let described = Arc::new(Mutex::new(false));
            let flag = Arc::clone(&described);
            let _notice = WaitNotice::start_lazily(
                move || {
                    *flag.lock().expect("flag") = true;
                    "example.test".to_string()
                },
                Duration::from_secs(2),
                Duration::from_secs(2),
                move |line| sink.lock().expect("sink").push(line),
            );
            thread::sleep(Duration::from_millis(20));
            assert!(
                !*described.lock().expect("flag"),
                "a fast turn must not pay for describing an upstream it never names"
            );
        }
        assert!(
            lines.lock().expect("sink").is_empty(),
            "a fast turn should be silent, got: {:?}",
            lines.lock().expect("sink")
        );
    }

    /// A turn that keeps running must keep saying so, and must name what it is
    /// waiting on — "still running" without a target leaves the reader exactly
    /// as stuck as silence does.
    ///
    /// Uses channel-based synchronization instead of sleep to avoid flaky
    /// timing on loaded CI runners.
    #[test]
    fn keeps_reporting_while_the_turn_runs_then_stops_on_drop() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let calls = Arc::new(Mutex::new(0usize));
        {
            let counter = Arc::clone(&calls);
            let _notice = WaitNotice::start_lazily(
                move || {
                    *counter.lock().expect("counter") += 1;
                    "example.test".to_string()
                },
                Duration::from_millis(20),
                Duration::from_millis(20),
                move |line| {
                    tx.send(line).ok();
                },
            );

            // Wait for at least 2 lines deterministically via channel recv.
            let line1 = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("should receive first notice line");
            let line2 = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("should receive second notice line");

            assert!(
                line1.contains("example.test"),
                "should name the target, got: {line1}",
            );
            assert!(
                line2.contains("example.test"),
                "should name the target, got: {line2}",
            );
        }
        // The guard's Drop joins the thread, so nothing can arrive after this.

        assert_eq!(
            *calls.lock().expect("counter"),
            1,
            "the upstream should be described once and reused, not re-resolved per line"
        );

        // After drop, the sender is gone and the thread is joined.
        // Verify no more lines arrive.
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "dropping the guard must stop the thread, not just detach it"
        );
    }
}

#[cfg(test)]
mod auth_mode_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn connection(base_url: &str) -> engine_core::ProviderConnectionConfig {
        engine_core::ProviderConnectionConfig {
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
    ) -> engine_core::ModelConfigEntry {
        let mut providers = BTreeMap::new();
        providers.insert(
            mode.to_string(),
            engine_core::ModelProviderMapping {
                provider: provider.to_string(),
                model: wire_model.to_string(),
                api: Some(api_format.to_string()),
            },
        );

        engine_core::ModelConfigEntry {
            alias: alias.to_string(),
            name: alias.to_string(),
            input: vec!["text".to_string()],
            providers,
            ..Default::default()
        }
    }

    fn mixed_auth_config() -> engine_core::SudoCodeConfig {
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

        engine_core::SudoCodeConfig {
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
