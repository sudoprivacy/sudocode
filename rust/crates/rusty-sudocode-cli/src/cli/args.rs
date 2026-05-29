use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use api::AuthMode;
use clap::{Parser, Subcommand, ValueEnum};
use commands::{
    classify_skills_slash_command, resolve_skill_invocation, resolve_skill_invocation_with_plugins,
    slash_command_specs, SkillSlashDispatch, SlashCommand,
};
use plugins::PluginLoadOutcome;
use runtime::{ConfigLoader, PermissionMode, ResolvedPermissionMode};
use tools::GlobalToolRegistry;

use super::session::LATEST_SESSION_REFERENCE;
use crate::{normalize_permission_mode, DEFAULT_MODEL};

pub(crate) type AllowedToolSet = BTreeSet<String>;

// ---------------------------------------------------------------------------
// clap-derived CLI definition
// ---------------------------------------------------------------------------

/// Sudo Code — AI-powered coding assistant
#[derive(Debug, Parser)]
#[command(
    name = "scode",
    version,
    disable_help_subcommand = true,
    subcommand_negates_reqs = true
)]
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    /// Model to use for inference (e.g. opus, sonnet, anthropic/claude-opus-4-6)
    #[arg(long, global = true)]
    model: Option<String>,

    /// Authentication mode
    #[arg(long, global = true, value_parser = parse_auth_mode)]
    auth: Option<AuthMode>,

    /// Output format
    #[arg(long, value_enum, global = true, default_value_t = OutputFormat::Text)]
    output_format: OutputFormat,

    /// Permission mode (read-only, workspace-write, danger-full-access)
    #[arg(long, global = true, value_parser = parse_permission_mode_value)]
    permission_mode: Option<PermissionMode>,

    /// Skip all permission checks (alias for --permission-mode danger-full-access)
    #[arg(long, global = true)]
    dangerously_skip_permissions: bool,

    /// Allowed tools (repeatable)
    #[arg(long = "allowedTools", alias = "allowed-tools", global = true)]
    allowed_tools: Vec<String>,

    /// Enable compact output
    #[arg(long, global = true)]
    compact: bool,

    /// Base git commit for diff context
    #[arg(long, global = true)]
    base_commit: Option<String>,

    /// Reasoning effort level
    #[arg(long, global = true, value_parser = ["low", "medium", "high"])]
    reasoning_effort: Option<String>,

    /// Allow running in a broad (non-project) working directory
    #[arg(long, global = true)]
    allow_broad_cwd: bool,

    /// Non-interactive print mode
    #[arg(long, global = true)]
    print: bool,

    /// Resume a saved session (optionally specify session path)
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "")]
    resume: Option<String>,

    #[command(subcommand)]
    command: Option<Cmd>,

    /// Prompt text (when no subcommand is given)
    #[arg(trailing_var_arg = true)]
    prompt_words: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

impl From<OutputFormat> for CliOutputFormat {
    fn from(f: OutputFormat) -> Self {
        match f {
            OutputFormat::Text => Self::Text,
            OutputFormat::Json => Self::Json,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Show help information
    Help,
    /// Show version information
    Version,
    /// Show workspace status snapshot
    Status,
    /// Show sandbox status
    Sandbox,
    /// Run local-only health report
    Doctor,
    /// Show session state
    State,
    /// Initialize workspace
    Init,
    /// Show or inspect merged configuration
    Config {
        /// Config section to inspect (env, hooks, model, plugins)
        section: Option<String>,
    },
    /// Show working tree diff
    Diff,
    /// Log in to the service
    Login,
    /// Log out from the service
    Logout,
    /// Export a session transcript
    Export {
        /// Session reference (defaults to latest)
        #[arg(long, default_value_t = String::from(LATEST_SESSION_REFERENCE))]
        session: String,
        /// Output file path
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Dump upstream manifests
    #[command(name = "dump-manifests")]
    DumpManifests {
        /// Directory to write manifests to
        #[arg(long)]
        manifests_dir: Option<PathBuf>,
    },
    /// Generate a bootstrap plan
    #[command(name = "bootstrap-plan")]
    BootstrapPlan,
    /// Manage agents
    Agents {
        /// Agent command arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage MCP connections
    Mcp {
        /// MCP command arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage skills
    Skills {
        /// Skills command arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage plugins
    #[command(alias = "marketplace")]
    Plugins {
        /// Plugin action (list, enable, disable, …)
        action: Option<String>,
        /// Plugin target
        target: Option<String>,
    },
    /// Print the full system prompt
    #[command(name = "system-prompt")]
    SystemPrompt {
        /// Working directory override
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Date override
        #[arg(long)]
        date: Option<String>,
    },
    /// Start ACP (Agent Control Protocol) mode
    Acp {
        #[command(subcommand)]
        sub: Option<AcpSub>,
    },
    /// Send a one-shot prompt to the model
    Prompt {
        /// Prompt text
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AcpSub {
    /// Start ACP in WebSocket server mode
    Serve {
        /// Port for the WebSocket server
        #[arg(long, default_value_t = 8080)]
        port: u16,
    },
}

// ---------------------------------------------------------------------------
// Public CliAction types (unchanged contract for downstream consumers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CliAction {
    DumpManifests {
        output_format: CliOutputFormat,
        manifests_dir: Option<PathBuf>,
    },
    BootstrapPlan {
        output_format: CliOutputFormat,
    },
    Agents {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Mcp {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Skills {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Plugins {
        action: Option<String>,
        target: Option<String>,
        output_format: CliOutputFormat,
    },
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
        model: String,
        output_format: CliOutputFormat,
    },
    Version {
        output_format: CliOutputFormat,
    },
    ResumeSession {
        session_path: PathBuf,
        commands: Vec<String>,
        output_format: CliOutputFormat,
    },
    Status {
        model: String,
        model_flag_raw: Option<String>,
        permission_mode: PermissionMode,
        output_format: CliOutputFormat,
    },
    Sandbox {
        output_format: CliOutputFormat,
    },
    Prompt {
        prompt: String,
        model: String,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        compact: bool,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        auth_mode: Option<AuthMode>,
    },
    Doctor {
        output_format: CliOutputFormat,
    },
    Acp {
        model: String,
        model_flag_raw: Option<String>,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode_override: Option<PermissionMode>,
        reasoning_effort: Option<String>,
        auth_mode: Option<AuthMode>,
        ws_port: Option<u16>,
    },
    State {
        output_format: CliOutputFormat,
    },
    Init {
        output_format: CliOutputFormat,
    },
    Config {
        section: Option<String>,
        output_format: CliOutputFormat,
    },
    Diff {
        output_format: CliOutputFormat,
    },
    Export {
        session_reference: String,
        output_path: Option<PathBuf>,
        output_format: CliOutputFormat,
    },
    Repl {
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        auth_mode: Option<AuthMode>,
    },
    HelpTopic {
        topic: LocalHelpTopic,
        output_format: CliOutputFormat,
    },
    Help {
        output_format: CliOutputFormat,
    },
    Login,
    Logout,
}

impl CliAction {
    /// Returns `true` for commands that report local state or usage information
    /// and must never require authentication to run. These variants are
    /// dispatched before any credential check so they work even when the user
    /// has not logged in yet.
    pub(crate) fn is_informational(&self) -> bool {
        matches!(
            self,
            CliAction::Help { .. }
                | CliAction::Version { .. }
                | CliAction::HelpTopic { .. }
                | CliAction::Config { .. }
                | CliAction::Login
                | CliAction::Logout
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalHelpTopic {
    Status,
    Sandbox,
    Doctor,
    Acp,
    Init,
    State,
    Export,
    Version,
    SystemPrompt,
    DumpManifests,
    BootstrapPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliOutputFormat {
    Text,
    Json,
}

impl CliOutputFormat {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unsupported value for --output-format: {other} (expected text or json)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Value parsers for clap
// ---------------------------------------------------------------------------

fn parse_auth_mode(s: &str) -> Result<AuthMode, String> {
    AuthMode::parse(s)
}

fn parse_permission_mode_value(value: &str) -> Result<PermissionMode, String> {
    normalize_permission_mode(value)
        .ok_or_else(|| {
            format!(
                "unsupported permission mode '{value}'. \
                 Use read-only, workspace-write, or danger-full-access."
            )
        })
        .map(permission_mode_from_label)
}

// ---------------------------------------------------------------------------
// Main entry point: parse CLI args via clap → CliAction
// ---------------------------------------------------------------------------

pub(crate) fn parse_args(args: &[String]) -> Result<CliAction, String> {
    // Slash-commands (`scode /help`) use a `/` prefix that clap can't handle.
    // Route them before calling clap.
    if let Some(first) = args.first() {
        if first.starts_with('/') {
            return parse_slash_command_invocation(args);
        }
    }

    // Intercept `<subcommand> --help [--output-format json]` before clap
    // so we can emit structured JSON help instead of clap's default text.
    if let Some(action) = parse_local_help_action(args) {
        return action;
    }

    let cli = Cli::try_parse_from(std::iter::once("scode".to_string()).chain(args.iter().cloned()));

    match cli {
        Ok(cli) => convert_cli_to_action(cli),
        Err(e) => {
            // clap returns special error kinds for --help and --version.
            // Let them print to stdout and exit cleanly instead of going
            // through our error path (which prints to stderr).
            match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    e.exit();
                }
                _ => Err(e.to_string()),
            }
        }
    }
}

/// Convert the parsed `Cli` struct into the application's `CliAction`.
#[allow(clippy::too_many_lines)]
fn convert_cli_to_action(cli: Cli) -> Result<CliAction, String> {
    let output_format: CliOutputFormat = cli.output_format.into();
    let model_flag_raw = cli.model.clone();
    let model = match &cli.model {
        Some(m) => {
            validate_model_syntax(m)?;
            resolve_model_alias_with_config(m)
        }
        None => DEFAULT_MODEL.to_string(),
    };
    let auth_mode = cli.auth;
    let permission_mode_override = if cli.dangerously_skip_permissions {
        Some(PermissionMode::DangerFullAccess)
    } else {
        cli.permission_mode
    };
    let allowed_tools = normalize_allowed_tools(&cli.allowed_tools)?;
    let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);

    // --resume takes priority over subcommands
    if let Some(resume_value) = cli.resume {
        return parse_resume_from_clap(&resume_value, &cli.prompt_words, output_format);
    }

    // Subcommand dispatch
    if let Some(cmd) = cli.command {
        return match cmd {
            Cmd::Help => Ok(CliAction::Help { output_format }),
            Cmd::Version => Ok(CliAction::Version { output_format }),
            Cmd::Status => Ok(CliAction::Status {
                model: model.clone(),
                model_flag_raw,
                permission_mode,
                output_format,
            }),
            Cmd::Sandbox => Ok(CliAction::Sandbox { output_format }),
            Cmd::Doctor => Ok(CliAction::Doctor { output_format }),
            Cmd::State => Ok(CliAction::State { output_format }),
            Cmd::Init => Ok(CliAction::Init { output_format }),
            Cmd::Config { section } => Ok(CliAction::Config {
                section,
                output_format,
            }),
            Cmd::Diff => Ok(CliAction::Diff { output_format }),
            Cmd::Login => Ok(CliAction::Login),
            Cmd::Logout => Ok(CliAction::Logout),
            Cmd::Export { session, output } => Ok(CliAction::Export {
                session_reference: session,
                output_path: output,
                output_format,
            }),
            Cmd::DumpManifests { manifests_dir } => Ok(CliAction::DumpManifests {
                output_format,
                manifests_dir,
            }),
            Cmd::BootstrapPlan => Ok(CliAction::BootstrapPlan { output_format }),
            Cmd::Agents { args } => Ok(CliAction::Agents {
                args: join_optional_args(&args),
                output_format,
            }),
            Cmd::Mcp { args } => Ok(CliAction::Mcp {
                args: join_optional_args(&args),
                output_format,
            }),
            Cmd::Skills { args } => {
                let joined = join_optional_args(&args);
                match classify_skills_slash_command(joined.as_deref()) {
                    SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                        prompt,
                        model,
                        output_format,
                        allowed_tools,
                        permission_mode,
                        compact: cli.compact,
                        base_commit: cli.base_commit,
                        reasoning_effort: cli.reasoning_effort,
                        allow_broad_cwd: cli.allow_broad_cwd,
                        auth_mode,
                    }),
                    SkillSlashDispatch::Local => Ok(CliAction::Skills {
                        args: joined,
                        output_format,
                    }),
                }
            }
            Cmd::Plugins { action, target } => {
                let (action, target) = normalize_plugin_cli_action(action, target);
                Ok(CliAction::Plugins {
                    action,
                    target,
                    output_format,
                })
            }
            Cmd::SystemPrompt { cwd, date } => {
                let resolved_cwd = cwd.unwrap_or(env::current_dir().map_err(|e| e.to_string())?);
                let resolved_date = date.unwrap_or_else(runtime::today_local);
                Ok(CliAction::PrintSystemPrompt {
                    cwd: resolved_cwd,
                    date: resolved_date,
                    model: model.clone(),
                    output_format,
                })
            }
            Cmd::Acp { sub } => {
                let ws_port = sub.map(|AcpSub::Serve { port }| port);
                Ok(CliAction::Acp {
                    model,
                    model_flag_raw,
                    allowed_tools,
                    permission_mode_override,
                    reasoning_effort: cli.reasoning_effort,
                    auth_mode,
                    ws_port,
                })
            }
            Cmd::Prompt { text } => {
                let prompt = text.join(" ");
                Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools,
                    permission_mode,
                    compact: cli.compact,
                    base_commit: cli.base_commit,
                    reasoning_effort: cli.reasoning_effort,
                    allow_broad_cwd: cli.allow_broad_cwd,
                    auth_mode,
                })
            }
        };
    }

    // No subcommand — check for bare prompt words
    if !cli.prompt_words.is_empty() {
        let joined = cli.prompt_words.join(" ");
        if joined.trim().is_empty() {
            return Err(
                "empty prompt: provide a subcommand (run `scode --help`) or a prompt string"
                    .to_string(),
            );
        }
        return Ok(CliAction::Prompt {
            prompt: joined,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact: cli.compact,
            base_commit: cli.base_commit,
            reasoning_effort: cli.reasoning_effort,
            allow_broad_cwd: cli.allow_broad_cwd,
            auth_mode,
        });
    }

    // No subcommand, no prompt — try piped stdin or start REPL
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
        let piped = buf.trim().to_string();
        if !piped.is_empty() {
            return Ok(CliAction::Prompt {
                model,
                prompt: piped,
                allowed_tools,
                permission_mode,
                output_format,
                compact: false,
                base_commit: cli.base_commit,
                reasoning_effort: cli.reasoning_effort,
                allow_broad_cwd: cli.allow_broad_cwd,
                auth_mode,
            });
        }
    }

    Ok(CliAction::Repl {
        model,
        allowed_tools,
        permission_mode,
        base_commit: cli.base_commit,
        reasoning_effort: cli.reasoning_effort,
        allow_broad_cwd: cli.allow_broad_cwd,
        auth_mode,
    })
}

// ---------------------------------------------------------------------------
// Slash-command invocation (`scode /help`, `scode /status`)
// ---------------------------------------------------------------------------

fn parse_slash_command_invocation(args: &[String]) -> Result<CliAction, String> {
    let raw = args.join(" ");
    let output_format = CliOutputFormat::Text;

    match SlashCommand::parse(&raw) {
        Ok(Some(SlashCommand::Help)) => Ok(CliAction::Help { output_format }),
        Ok(Some(SlashCommand::Agents { args })) => Ok(CliAction::Agents {
            args,
            output_format,
        }),
        Ok(Some(SlashCommand::Mcp { action, target })) => Ok(CliAction::Mcp {
            args: match (action, target) {
                (None, None) => None,
                (Some(a), None) => Some(a),
                (Some(a), Some(t)) => Some(format!("{a} {t}")),
                (None, Some(t)) => Some(t),
            },
            output_format,
        }),
        Ok(Some(SlashCommand::Skills { args })) => {
            match classify_skills_slash_command(args.as_deref()) {
                SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                    prompt,
                    model: DEFAULT_MODEL.to_string(),
                    output_format,
                    allowed_tools: None,
                    permission_mode: default_permission_mode(),
                    compact: false,
                    base_commit: None,
                    reasoning_effort: None,
                    allow_broad_cwd: false,
                    auth_mode: None,
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        Ok(Some(SlashCommand::Unknown(name))) => {
            Err(format_unknown_slash_command_outside_repl(&name))
        }
        Ok(Some(_)) => Err(format!(
            "slash command {} is interactive-only. Start `scode` and run it there, \
             or use `scode --resume SESSION.jsonl {}` when the command supports resume.",
            args[0], args[0],
        )),
        Ok(None) => Err(format!("unknown subcommand: {}", args[0])),
        Err(error) => Err(error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// --resume handling
// ---------------------------------------------------------------------------

fn parse_resume_from_clap(
    session_str: &str,
    trailing: &[String],
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    let (session_path, command_tokens) = if session_str.is_empty() {
        // `--resume` without value
        (PathBuf::from(LATEST_SESSION_REFERENCE), trailing)
    } else if looks_like_slash_command_token(session_str) {
        // `--resume /status` — session_str is actually a command
        let all: Vec<String> = std::iter::once(session_str.to_string())
            .chain(trailing.iter().cloned())
            .collect();
        return parse_resume_commands(PathBuf::from(LATEST_SESSION_REFERENCE), &all, output_format);
    } else {
        (PathBuf::from(session_str), trailing)
    };

    parse_resume_commands(session_path, command_tokens, output_format)
}

fn parse_resume_commands(
    session_path: PathBuf,
    command_tokens: &[String],
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    let mut commands = Vec::new();
    let mut current_command = String::new();

    for token in command_tokens {
        if token.trim_start().starts_with('/') {
            if resume_command_can_absorb_token(&current_command, token) {
                current_command.push(' ');
                current_command.push_str(token);
                continue;
            }
            if !current_command.is_empty() {
                commands.push(std::mem::take(&mut current_command));
            }
            current_command.clone_from(token);
            continue;
        }

        if current_command.is_empty() {
            return Err("--resume trailing arguments must be slash commands".to_string());
        }

        current_command.push(' ');
        current_command.push_str(token);
    }

    if !current_command.is_empty() {
        commands.push(current_command);
    }

    Ok(CliAction::ResumeSession {
        session_path,
        commands,
        output_format,
    })
}

fn resume_command_can_absorb_token(current_command: &str, token: &str) -> bool {
    matches!(
        SlashCommand::parse(current_command),
        Ok(Some(SlashCommand::Export { path: None }))
    ) && !looks_like_slash_command_token(token)
}

fn looks_like_slash_command_token(token: &str) -> bool {
    let trimmed = token.trim_start();
    let Some(name) = trimmed.strip_prefix('/').and_then(|v| {
        v.split_whitespace()
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }) else {
        return false;
    };
    slash_command_specs()
        .iter()
        .any(|spec| spec.name == name || spec.aliases.contains(&name))
}

// ---------------------------------------------------------------------------
// Slash-command error formatting (used by REPL in main.rs too)
// ---------------------------------------------------------------------------

fn format_unknown_slash_command_outside_repl(name: &str) -> String {
    let mut message = format!("unknown slash command outside the REPL: /{name}");
    if let Some(line) = render_suggestion_line("Did you mean", &suggest_slash_commands(name)) {
        message.push('\n');
        message.push_str(&line);
    }
    if let Some(note) = omc_compatibility_note(name) {
        message.push('\n');
        message.push_str(note);
    }
    message.push_str("\nRun `scode --help` for CLI usage, or start `scode` and use /help.");
    message
}

pub(crate) fn format_unknown_slash_command(name: &str) -> String {
    let mut message = format!("Unknown slash command: /{name}");
    if let Some(line) = render_suggestion_line("Did you mean", &suggest_slash_commands(name)) {
        message.push('\n');
        message.push_str(&line);
    }
    if let Some(note) = omc_compatibility_note(name) {
        message.push('\n');
        message.push_str(note);
    }
    message.push_str("\n  Help             /help lists available slash commands");
    message
}

fn omc_compatibility_note(name: &str) -> Option<&'static str> {
    name.starts_with("oh-my-claudecode:").then_some(
        "Compatibility note: `/oh-my-claudecode:*` is a Sudo Code/OMC plugin command. \
         `scode` does not yet load plugin slash commands, Claude statusline stdin, or OMC session hooks.",
    )
}

fn render_suggestion_line(label: &str, suggestions: &[String]) -> Option<String> {
    (!suggestions.is_empty()).then(|| format!("  {label:<16} {}", suggestions.join(", ")))
}

fn suggest_slash_commands(input: &str) -> Vec<String> {
    let mut candidates: Vec<String> = slash_command_specs()
        .iter()
        .flat_map(|spec| {
            std::iter::once(spec.name)
                .chain(spec.aliases.iter().copied())
                .map(|name| format!("/{name}"))
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    ranked_suggestions(input.trim_start_matches('/'), &refs)
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn ranked_suggestions<'a>(input: &str, candidates: &'a [&'a str]) -> Vec<&'a str> {
    let norm = input.trim_start_matches('/').to_ascii_lowercase();
    let mut scored: Vec<(usize, &str)> = candidates
        .iter()
        .filter_map(|c| {
            let cn = c.trim_start_matches('/').to_ascii_lowercase();
            let dist = levenshtein_distance(&norm, &cn);
            let bonus = usize::from(!(cn.starts_with(&norm) || norm.starts_with(&cn)));
            let score = dist + bonus;
            (score <= 4).then_some((score, *c))
        })
        .collect();
    scored.sort_by(|a, b| a.cmp(b).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, c)| c).take(3).collect()
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    if left.is_empty() {
        return right.chars().count();
    }
    if right.is_empty() {
        return left.chars().count();
    }
    let rc: Vec<char> = right.chars().collect();
    let mut prev: Vec<usize> = (0..=rc.len()).collect();
    let mut curr = vec![0; rc.len() + 1];
    for (i, lc) in left.chars().enumerate() {
        curr[0] = i + 1;
        for (j, &r) in rc.iter().enumerate() {
            let cost = usize::from(lc != r);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        prev.clone_from(&curr);
    }
    prev[rc.len()]
}

// ---------------------------------------------------------------------------
// Local help interception
// ---------------------------------------------------------------------------

fn is_help_flag(value: &str) -> bool {
    value == "--help" || value == "-h"
}

/// Detect `<subcommand> --help [--output-format json]` before clap parses args.
/// Returns `Some(Ok(..))` when matched, `None` otherwise.
fn parse_local_help_action(args: &[String]) -> Option<Result<CliAction, String>> {
    // We need at least 2 args: `<subcommand> --help`
    if args.len() < 2 {
        return None;
    }

    // Find the help flag position and extract output_format
    let has_help = args.iter().any(|a| is_help_flag(a));
    if !has_help {
        return None;
    }

    // Determine output format from remaining args
    let mut output_format = CliOutputFormat::Text;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--output-format" {
            if let Some(fmt) = args.get(i + 1) {
                match CliOutputFormat::parse(fmt) {
                    Ok(f) => output_format = f,
                    Err(e) => return Some(Err(e)),
                }
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    // The first non-flag arg should be the subcommand
    let subcommand = args[0].as_str();
    let topic = match subcommand {
        "status" => LocalHelpTopic::Status,
        "sandbox" => LocalHelpTopic::Sandbox,
        "doctor" => LocalHelpTopic::Doctor,
        "acp" => LocalHelpTopic::Acp,
        "init" => LocalHelpTopic::Init,
        "state" => LocalHelpTopic::State,
        "export" => LocalHelpTopic::Export,
        "version" => LocalHelpTopic::Version,
        "system-prompt" => LocalHelpTopic::SystemPrompt,
        "dump-manifests" => LocalHelpTopic::DumpManifests,
        "bootstrap-plan" => LocalHelpTopic::BootstrapPlan,
        _ => return None,
    };
    Some(Ok(CliAction::HelpTopic {
        topic,
        output_format,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize_plugin_cli_action(
    action: Option<String>,
    target: Option<String>,
) -> (Option<String>, Option<String>) {
    match (action.as_deref(), target.as_deref()) {
        (Some("marketplace"), None | Some("available")) => (Some("available".to_string()), None),
        _ => (action, target),
    }
}

pub(crate) fn join_optional_args(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(crate) fn try_resolve_bare_skill_prompt(cwd: &Path, trimmed: &str) -> Option<String> {
    try_resolve_bare_skill_prompt_with_plugins(cwd, trimmed, None)
}

pub(crate) fn try_resolve_bare_skill_prompt_with_plugins(
    cwd: &Path,
    trimmed: &str,
    plugin_load_outcome: Option<&PluginLoadOutcome>,
) -> Option<String> {
    let bare = trimmed.split_whitespace().next().unwrap_or_default();
    let looks_like_skill = !bare.is_empty()
        && !bare.starts_with('/')
        && bare
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !looks_like_skill {
        return None;
    }
    match resolve_skill_invocation_with_plugins(cwd, Some(trimmed), plugin_load_outcome) {
        Ok(SkillSlashDispatch::Invoke(prompt)) => Some(prompt),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Model / alias resolution
// ---------------------------------------------------------------------------

pub(crate) fn resolve_model_alias(model: &str) -> &str {
    match model {
        "claude-opus" | "opus" => "claude-opus-4-6",
        "claude-sonnet" | "sonnet" => "claude-sonnet-4-6",
        "claude-haiku" | "haiku" => "claude-haiku-4-5-20251213",
        _ => model,
    }
}

pub(crate) fn resolve_model_alias_with_config(model: &str) -> String {
    let trimmed = model.trim();
    let config = load_sudocode_config_for_current_dir();
    if let Some(alias) = resolve_config_model_alias(trimmed, &config) {
        return alias;
    }
    if let Some(resolved) = config_alias_for_current_dir(trimmed) {
        return resolve_model_alias(&resolved).to_string();
    }
    resolve_model_alias(trimmed).to_string()
}

fn resolve_config_model_alias(model: &str, config: &api::SudoCodeConfig) -> Option<String> {
    let trimmed = model.trim();
    let entry = config.models.get(&trimmed.to_ascii_lowercase())?;
    if entry.alias.trim().is_empty() {
        Some(trimmed.to_string())
    } else {
        Some(entry.alias.clone())
    }
}

pub(crate) fn validate_model_syntax(model: &str) -> Result<(), String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return Err("model string cannot be empty".to_string());
    }
    match trimmed {
        "claude-opus" | "claude-sonnet" | "claude-haiku" | "opus" | "sonnet" | "haiku" => {
            return Ok(())
        }
        _ => {}
    }
    let config = load_sudocode_config_for_current_dir();
    if api::resolve_model(&config, trimmed).is_some() {
        return Ok(());
    }
    if trimmed.contains(' ') {
        return Err(format!(
            "invalid model syntax: '{trimmed}' contains spaces. Use provider/model format or known alias"
        ));
    }
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        let mut msg = format!(
            "invalid model syntax: '{trimmed}'. \
             Expected provider/model (e.g., anthropic/claude-opus-4-6) or known alias (opus, sonnet, haiku)"
        );
        if trimmed.starts_with("gpt-") || trimmed.starts_with("gpt_") {
            let _ = write!(
                msg,
                "\nDid you mean `openai/{trimmed}`? (Requires OPENAI_API_KEY env var)"
            );
        } else if trimmed.starts_with("qwen") {
            let _ = write!(
                msg,
                "\nDid you mean `qwen/{trimmed}`? (Requires DASHSCOPE_API_KEY env var)"
            );
        } else if trimmed.starts_with("grok") {
            let _ = write!(
                msg,
                "\nDid you mean `xai/{trimmed}`? (Requires XAI_API_KEY env var)"
            );
        }
        return Err(msg);
    }
    Ok(())
}

fn config_alias_for_current_dir(alias: &str) -> Option<String> {
    if alias.is_empty() {
        return None;
    }
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    let config = loader.load().ok()?;
    config.aliases().get(alias).cloned()
}

pub(crate) fn load_sudocode_config_for_current_dir() -> api::SudoCodeConfig {
    let Ok(cwd) = env::current_dir() else {
        return api::SudoCodeConfig::default();
    };
    load_sudocode_config_for_cwd(&cwd)
}

pub(crate) fn load_sudocode_config_for_cwd(cwd: &Path) -> api::SudoCodeConfig {
    let loader = ConfigLoader::default_for(cwd);
    loader.load_sudocode_config().unwrap_or_default()
}

pub(crate) fn require_sudocode_config_for_cwd(cwd: &Path) -> Result<api::SudoCodeConfig, String> {
    let loader = ConfigLoader::default_for(cwd);
    loader.load_sudocode_config().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Allowed tools
// ---------------------------------------------------------------------------

pub(crate) fn normalize_allowed_tools(values: &[String]) -> Result<Option<AllowedToolSet>, String> {
    if values.is_empty() {
        return Ok(None);
    }
    current_tool_registry()?.normalize_allowed_tools(values)
}

fn current_tool_registry() -> Result<GlobalToolRegistry, String> {
    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load().map_err(|e| e.to_string())?;
    let state =
        super::super::build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
            .map_err(|e| e.to_string())?;
    let registry = state.tool_registry.clone();
    if let Some(mcp_state) = state.mcp_state {
        mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown()
            .map_err(|e| e.to_string())?;
    }
    Ok(registry)
}

// ---------------------------------------------------------------------------
// Permission mode helpers
// ---------------------------------------------------------------------------

pub(crate) fn permission_mode_from_label(mode: &str) -> PermissionMode {
    match mode {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

pub(crate) fn permission_mode_from_resolved(mode: ResolvedPermissionMode) -> PermissionMode {
    match mode {
        ResolvedPermissionMode::ReadOnly => PermissionMode::ReadOnly,
        ResolvedPermissionMode::WorkspaceWrite => PermissionMode::WorkspaceWrite,
        ResolvedPermissionMode::DangerFullAccess => PermissionMode::DangerFullAccess,
    }
}

pub(crate) fn default_permission_mode() -> PermissionMode {
    env::var("SUDO_CODE_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(normalize_permission_mode)
        .map(permission_mode_from_label)
        .or_else(config_permission_mode_for_current_dir)
        .unwrap_or(PermissionMode::DangerFullAccess)
}

fn config_permission_mode_for_current_dir() -> Option<PermissionMode> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader
        .load()
        .ok()?
        .permission_mode()
        .map(permission_mode_from_resolved)
}

pub(crate) fn config_model_for_current_dir() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader.load().ok()?.model().map(ToOwned::to_owned)
}

pub(crate) fn resolve_repl_model(cli_model: String) -> String {
    if cli_model != DEFAULT_MODEL {
        return cli_model;
    }
    if let Some(env_model) = env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return resolve_model_alias_with_config(&env_model);
    }
    if let Some(config_model) = config_model_for_current_dir() {
        return resolve_model_alias_with_config(&config_model);
    }
    cli_model
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse_words(words: &[&str]) -> CliAction {
        let args = words
            .iter()
            .map(|word| (*word).to_string())
            .collect::<Vec<_>>();
        parse_args(&args).expect("parse cli args")
    }

    #[test]
    fn config_model_alias_is_preserved_instead_of_expanding_to_wire_model() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "api-key".to_string(),
            api::ModelProviderMapping {
                provider: "custom-openai".to_string(),
                model: "gpt-5.4".to_string(),
                api: Some("openai-completions".to_string()),
            },
        );

        let mut models = BTreeMap::new();
        models.insert(
            "custom-openai/gpt-5.4".to_string(),
            api::ModelConfigEntry {
                alias: "custom-openai/gpt-5.4".to_string(),
                name: "custom-openai/gpt-5.4".to_string(),
                input: vec!["text".to_string()],
                providers,
            },
        );

        let config = api::SudoCodeConfig {
            auth_modes: BTreeMap::new(),
            models,
            web_search: Default::default(),
        };

        assert_eq!(
            resolve_config_model_alias("custom-openai/gpt-5.4", &config).as_deref(),
            Some("custom-openai/gpt-5.4")
        );
    }

    #[test]
    fn informational_variants_are_whitelisted() {
        assert!(CliAction::Help {
            output_format: CliOutputFormat::Text
        }
        .is_informational());
        assert!(CliAction::Version {
            output_format: CliOutputFormat::Json
        }
        .is_informational());
        assert!(CliAction::HelpTopic {
            topic: LocalHelpTopic::Status,
            output_format: CliOutputFormat::Text,
        }
        .is_informational());
        assert!(CliAction::Config {
            section: None,
            output_format: CliOutputFormat::Text
        }
        .is_informational());
        assert!(CliAction::Login.is_informational());
        assert!(CliAction::Logout.is_informational());
    }

    #[test]
    fn non_informational_variants_are_not_whitelisted() {
        assert!(!CliAction::Doctor {
            output_format: CliOutputFormat::Text
        }
        .is_informational());
        assert!(!CliAction::Repl {
            model: "opus".to_string(),
            allowed_tools: None,
            permission_mode: PermissionMode::DangerFullAccess,
            base_commit: None,
            reasoning_effort: None,
            allow_broad_cwd: false,
            auth_mode: None,
        }
        .is_informational());
    }

    #[test]
    fn marketplace_cli_aliases_route_to_plugins_available() {
        let expected = CliAction::Plugins {
            action: Some("available".to_string()),
            target: None,
            output_format: CliOutputFormat::Text,
        };

        assert_eq!(parse_words(&["marketplace", "available"]), expected);
        assert_eq!(parse_words(&["plugins", "marketplace"]), expected);
        assert_eq!(
            parse_words(&["plugins", "marketplace", "available"]),
            expected
        );
    }

    #[test]
    fn marketplace_cli_alias_without_action_routes_to_plugins_list() {
        assert_eq!(
            parse_words(&["marketplace"]),
            CliAction::Plugins {
                action: None,
                target: None,
                output_format: CliOutputFormat::Text,
            }
        );
    }
}
