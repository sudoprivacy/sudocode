use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use commands::{render_slash_command_help_filtered, resume_supported_slash_commands};
use runtime::{ConfigLoader, ConfigSource, ContentBlock, ProjectContext, Session};
use serde_json::json;

use crate::cli::session::LATEST_SESSION_REFERENCE;
use crate::PRIMARY_SESSION_EXTENSION;
use crate::{
    truncate_for_prompt, CliOutputFormat, LocalHelpTopic, DEPRECATED_INSTALL_COMMAND,
    OFFICIAL_REPO_SLUG, OFFICIAL_REPO_URL, STUB_COMMANDS, VERSION,
};

pub(crate) fn render_repl_help() -> String {
    [
        "REPL".to_string(),
        "  /exit                Quit the REPL".to_string(),
        "  /quit                Quit the REPL".to_string(),
        "  Up/Down              Navigate prompt history".to_string(),
        "  Ctrl-R               Reverse-search prompt history".to_string(),
        "  Tab                  Complete commands, modes, and recent sessions".to_string(),
        "  Ctrl-C               Clear input (double Ctrl-C to exit)".to_string(),
        "  Shift+Enter/Ctrl+J   Insert a newline".to_string(),
        "  Auto-save            .scode/sessions/<session-id>.jsonl".to_string(),
        "  Resume latest        /resume latest".to_string(),
        "  Browse sessions      /session list".to_string(),
        "  Show prompt history  /history [count]".to_string(),
        String::new(),
        render_slash_command_help_filtered(STUB_COMMANDS),
    ]
    .join(
        "
",
    )
}

pub(crate) fn render_help_topic(topic: LocalHelpTopic) -> String {
    match topic {
        LocalHelpTopic::Status => "Status
  Usage            scode status [--output-format <format>]
  Purpose          show the local workspace snapshot without entering the REPL
  Output           model, permissions, git state, config files, and sandbox status
  Formats          text (default), json
  Related          /status · scode --resume latest /status"
            .to_string(),
        LocalHelpTopic::Sandbox => "Sandbox
  Usage            scode sandbox [--output-format <format>]
  Purpose          inspect the resolved sandbox and isolation state for the current directory
  Output           namespace, network, filesystem, and fallback details
  Formats          text (default), json
  Related          /sandbox · scode status"
            .to_string(),
        LocalHelpTopic::Doctor => "Doctor
  Usage            scode doctor [--output-format <format>]
  Purpose          diagnose local auth, config, workspace, sandbox, and build metadata
  Output           local-only health report; no provider request or session resume required
  Formats          text (default), json
  Related          /doctor · scode --resume latest /doctor"
            .to_string(),
        LocalHelpTopic::Acp => "ACP / Zed
  Usage            scode acp [serve]
  Aliases          scode --acp · scode -acp
  Purpose          run the ACP JSON-RPC agent server over stdio for editor integrations
  Transport        LSP-style Content-Length framed JSON-RPC on stdin/stdout
  Sessions         session/new creates managed .scode sessions for the requested cwd
  Related          scode --help"
            .to_string(),
        LocalHelpTopic::Init => "Init
  Usage            scode init [--output-format <format>]
  Purpose          create .nexus/sudocode/, .scode.json, .gitignore, and CLAUDE.md in the current project
  Output           list of created vs. skipped files (idempotent: safe to re-run)
  Formats          text (default), json
  Related          scode status · scode doctor"
            .to_string(),
        LocalHelpTopic::State => "State
  Usage            scode state [--output-format <format>]
  Purpose          read .nexus/sudocode/worker-state.json written by the interactive REPL or a one-shot prompt
  Output           worker id, model, permissions, session reference (text or json)
  Formats          text (default), json
  Produces state   `scode` (interactive REPL) or `scode prompt <text>` (one non-interactive turn)
  Observes state   `scode state` reads; sudocodehip/CI may poll this file without HTTP
  Exit codes       0 if state file exists and parses; 1 with actionable hint otherwise
  Related          scode status · ROADMAP #139 (this worker-concept contract)"
            .to_string(),
        LocalHelpTopic::Export => "Export
  Usage            scode export [--session <id|latest>] [--output <path>] [--output-format <format>]
  Purpose          serialize a managed session to JSON for review, transfer, or archival
  Defaults         --session latest (most recent managed session in .scode/sessions/)
  Formats          text (default), json
  Related          /session list · scode --resume latest"
            .to_string(),
        LocalHelpTopic::Version => "Version
  Usage            scode version [--output-format <format>]
  Aliases          scode --version · scode -V
  Purpose          print the scode CLI version and build metadata
  Formats          text (default), json
  Related          scode doctor (full build/auth/config diagnostic)"
            .to_string(),
        LocalHelpTopic::SystemPrompt => "System Prompt
  Usage            scode system-prompt [--cwd <path>] [--date YYYY-MM-DD] [--output-format <format>]
  Purpose          render the resolved system prompt that `scode` would send for the given cwd + date
  Options          --cwd overrides the workspace dir · --date injects a deterministic date stamp
  Formats          text (default), json
  Related          scode doctor · scode dump-manifests"
            .to_string(),
        LocalHelpTopic::DumpManifests => "Dump Manifests
  Usage            scode dump-manifests [--manifests-dir <path>] [--output-format <format>]
  Purpose          emit every skill/agent/tool manifest the resolver would load for the current cwd
  Options          --manifests-dir scopes discovery to a specific directory
  Formats          text (default), json
  Related          scode skills · scode agents · scode doctor"
            .to_string(),
        LocalHelpTopic::BootstrapPlan => "Bootstrap Plan
  Usage            scode bootstrap-plan [--output-format <format>]
  Purpose          list the ordered startup phases the CLI would execute before dispatch
  Output           phase names (text) or structured phase list (json) — primary output is the plan itself
  Formats          text (default), json
  Related          scode doctor · scode status"
            .to_string(),
    }
}

fn local_help_topic_command(topic: LocalHelpTopic) -> &'static str {
    match topic {
        LocalHelpTopic::Status => "status",
        LocalHelpTopic::Sandbox => "sandbox",
        LocalHelpTopic::Doctor => "doctor",
        LocalHelpTopic::Acp => "acp",
        LocalHelpTopic::Init => "init",
        LocalHelpTopic::State => "state",
        LocalHelpTopic::Export => "export",
        LocalHelpTopic::Version => "version",
        LocalHelpTopic::SystemPrompt => "system-prompt",
        LocalHelpTopic::DumpManifests => "dump-manifests",
        LocalHelpTopic::BootstrapPlan => "bootstrap-plan",
    }
}

fn render_export_help_json() -> serde_json::Value {
    json!({
        "kind": "help",
        "topic": "export",
        "command": "export",
        "usage": "scode export [--session <id|latest>] [--output <path>] [--output-format <format>]",
        "purpose": "serialize a managed session to JSON for review, transfer, or archival",
        "defaults": {
            "session": LATEST_SESSION_REFERENCE,
            "session_source": ".scode/sessions/",
            "output": "derived from the selected session when omitted"
        },
        "formats": ["text", "json"],
        "options": [
            {
                "name": "--session",
                "value": "<id|latest>",
                "default": LATEST_SESSION_REFERENCE,
                "description": "managed session to export"
            },
            {
                "name": "--output",
                "aliases": ["-o"],
                "value": "<path>",
                "description": "write the exported transcript to this path"
            },
            {
                "name": "--output-format",
                "value": "<format>",
                "values": ["text", "json"],
                "default": "text",
                "description": "format for the command result envelope"
            },
            {
                "name": "--help",
                "aliases": ["-h"],
                "description": "show help for the export command"
            }
        ],
        "related": ["/session list", "scode --resume latest"]
    })
}

pub(crate) fn render_help_topic_json(topic: LocalHelpTopic) -> serde_json::Value {
    if topic == LocalHelpTopic::Export {
        return render_export_help_json();
    }

    json!({
        "kind": "help",
        "topic": local_help_topic_command(topic),
        "command": local_help_topic_command(topic),
        "message": render_help_topic(topic),
    })
}

pub(crate) fn print_help_topic(
    topic: LocalHelpTopic,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!("{}", render_help_topic(topic)),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&render_help_topic_json(topic))?
        ),
    }
    Ok(())
}

pub(crate) fn render_config_report(
    section: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config
  Working directory {}
  Loaded files      {}
  Merged keys       {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "Discovered files".to_string(),
    ];
    for entry in discovered {
        let source = match entry.source {
            ConfigSource::User => "user",
            ConfigSource::Project => "project",
            ConfigSource::Local => "local",
        };
        let status = if runtime_config
            .loaded_entries()
            .iter()
            .any(|loaded_entry| loaded_entry.path == entry.path)
        {
            "loaded"
        } else {
            "missing"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("Merged section: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins")),
            other => {
                lines.push(format!(
                    "  Unsupported config section '{other}'. Use env, hooks, model, or plugins."
                ));
                return Ok(lines.join(
                    "
",
                ));
            }
        };
        lines.push(format!(
            "  {}",
            match value {
                Some(value) => value.render(),
                None => "<unset>".to_string(),
            }
        ));
        return Ok(lines.join(
            "
",
        ));
    }

    lines.push("Merged JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join(
        "
",
    ))
}

pub(crate) fn render_config_json(
    section: Option<&str>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let loaded_paths: Vec<_> = runtime_config
        .loaded_entries()
        .iter()
        .map(|e| e.path.display().to_string())
        .collect();

    let files: Vec<_> = discovered
        .iter()
        .map(|e| {
            let source = match e.source {
                ConfigSource::User => "user",
                ConfigSource::Project => "project",
                ConfigSource::Local => "local",
            };
            let is_loaded = runtime_config
                .loaded_entries()
                .iter()
                .any(|le| le.path == e.path);
            serde_json::json!({
                "path": e.path.display().to_string(),
                "source": source,
                "loaded": is_loaded,
            })
        })
        .collect();

    let base = serde_json::json!({
        "kind": "config",
        "cwd": cwd.display().to_string(),
        "loaded_files": loaded_paths.len(),
        "merged_keys": runtime_config.merged().len(),
        "files": files,
    });

    if let Some(section) = section {
        let section_rendered: Option<String> = match section {
            "env" => runtime_config.get("env").map(|v| v.render()),
            "hooks" => runtime_config.get("hooks").map(|v| v.render()),
            "model" => runtime_config.get("model").map(|v| v.render()),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins"))
                .map(|v| v.render()),
            other => {
                return Ok(serde_json::json!({
                    "kind": "config",
                    "section": other,
                    "ok": false,
                    "error": format!("Unsupported config section '{other}'. Use env, hooks, model, or plugins."),
                    "cwd": cwd.display().to_string(),
                    "loaded_files": loaded_paths.len(),
                    "files": files,
                }));
            }
        };
        // Parse the rendered JSON string back into serde_json::Value so that
        // section_value is a real JSON object/array in the envelope, not a quoted string.
        let section_value: serde_json::Value = section_rendered
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        let mut obj = base;
        let map = obj.as_object_mut().expect("base is object");
        map.insert(
            "section".to_string(),
            serde_json::Value::String(section.to_string()),
        );
        map.insert("section_value".to_string(), section_value);
        return Ok(obj);
    }

    Ok(base)
}

pub(crate) fn render_memory_report() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, runtime::today_local())?;
    let mut lines = vec![format!(
        "Memory
  Working directory {}
  Instruction files {}",
        cwd.display(),
        project_context.instruction_files.len()
    )];
    if project_context.instruction_files.is_empty() {
        lines.push("Discovered files".to_string());
        lines.push(
            "  No CLAUDE instruction files discovered in the current directory ancestry."
                .to_string(),
        );
    } else {
        lines.push("Discovered files".to_string());
        for (index, file) in project_context.instruction_files.iter().enumerate() {
            let preview = file.content.lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<empty>"
            } else {
                preview
            };
            lines.push(format!("  {}. {}", index + 1, file.path.display(),));
            lines.push(format!(
                "     lines={} preview={}",
                file.content.lines().count(),
                preview
            ));
        }
    }
    Ok(lines.join(
        "
",
    ))
}

pub(crate) fn render_memory_json() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, runtime::today_local())?;
    let files: Vec<_> = project_context
        .instruction_files
        .iter()
        .map(|f| {
            json!({
                "path": f.path.display().to_string(),
                "lines": f.content.lines().count(),
                "preview": f.content.lines().next().unwrap_or("").trim(),
            })
        })
        .collect();
    Ok(json!({
        "kind": "memory",
        "cwd": cwd.display().to_string(),
        "instruction_files": files.len(),
        "files": files,
    }))
}

pub(crate) fn render_diff_report() -> Result<String, Box<dyn std::error::Error>> {
    render_diff_report_for(&env::current_dir()?)
}

pub(crate) fn render_diff_report_for(cwd: &Path) -> Result<String, Box<dyn std::error::Error>> {
    // Verify we are inside a git repository before calling `git diff`.
    // Running `git diff --cached` outside a git tree produces a misleading
    // "unknown option `cached`" error because git falls back to --no-index mode.
    let in_git_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(format!(
            "Diff\n  Result           no git repository\n  Detail           {} is not inside a git project",
            cwd.display()
        ));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    if staged.trim().is_empty() && unstaged.trim().is_empty() {
        return Ok(
            "Diff\n  Result           clean working tree\n  Detail           no current changes"
                .to_string(),
        );
    }

    let mut sections = Vec::new();
    if !staged.trim().is_empty() {
        sections.push(format!(
            "\x1b[1mStaged changes:\x1b[0m\n{}",
            colorize_unified_diff(staged.trim_end())
        ));
    }
    if !unstaged.trim().is_empty() {
        sections.push(format!(
            "\x1b[1mUnstaged changes:\x1b[0m\n{}",
            colorize_unified_diff(unstaged.trim_end())
        ));
    }

    Ok(format!("\x1b[1mDiff\x1b[0m\n\n{}", sections.join("\n\n")))
}

/// Apply per-line color to a unified-diff string.
///
/// - `+++` / `---` file headers and `diff --git` lines: bold.
/// - `@@` hunk headers: cyan.
/// - Lines starting with a lone `+`: green (added).
/// - Lines starting with a lone `-`: red (removed).
/// - Everything else: unchanged.
///
/// Preserves the original line endings so `colorize_unified_diff(s) ==
/// s` modulo escape sequences for terminal-emulator behavior.
pub(crate) fn colorize_unified_diff(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len() + diff.lines().count() * 8);
    for line in diff.split_inclusive('\n') {
        let trailing_newline = line.ends_with('\n');
        let body = line.trim_end_matches('\n');
        let color = if body.starts_with("+++")
            || body.starts_with("---")
            || body.starts_with("diff --git ")
            || body.starts_with("index ")
        {
            Some("\x1b[1m")
        } else if body.starts_with("@@") {
            Some("\x1b[36m")
        } else if body.starts_with('+') {
            Some("\x1b[32m")
        } else if body.starts_with('-') {
            Some("\x1b[31m")
        } else {
            None
        };
        match color {
            Some(prefix) => {
                out.push_str(prefix);
                out.push_str(body);
                out.push_str("\x1b[0m");
            }
            None => out.push_str(body),
        }
        if trailing_newline {
            out.push('\n');
        }
    }
    out
}

pub(crate) fn render_diff_json_for(
    cwd: &Path,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let in_git_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(serde_json::json!({
            "kind": "diff",
            "result": "no_git_repo",
            "detail": format!("{} is not inside a git project", cwd.display()),
        }));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    Ok(serde_json::json!({
        "kind": "diff",
        "result": if staged.trim().is_empty() && unstaged.trim().is_empty() { "clean" } else { "changes" },
        "staged": staged.trim(),
        "unstaged": unstaged.trim(),
    }))
}

pub(crate) fn run_git_diff_command_in(
    cwd: &Path,
    args: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git").args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

pub(crate) fn render_teleport_report(target: &str) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;

    let file_list = Command::new("rg")
        .args(["--files"])
        .current_dir(&cwd)
        .output()?;
    let file_matches = if file_list.status.success() {
        String::from_utf8(file_list.stdout)?
            .lines()
            .filter(|line| line.contains(target))
            .take(10)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let content_output = Command::new("rg")
        .args(["-n", "-S", "--color", "never", target, "."])
        .current_dir(&cwd)
        .output()?;

    let mut lines = vec![
        "Teleport".to_string(),
        format!("  Target           {target}"),
        "  Action           search workspace files and content for the target".to_string(),
    ];
    if !file_matches.is_empty() {
        lines.push(String::new());
        lines.push("File matches".to_string());
        lines.extend(file_matches.into_iter().map(|path| format!("  {path}")));
    }

    if content_output.status.success() {
        let matches = String::from_utf8(content_output.stdout)?;
        if !matches.trim().is_empty() {
            lines.push(String::new());
            lines.push("Content matches".to_string());
            lines.push(truncate_for_prompt(&matches, 4_000));
        }
    }

    if lines.len() == 1 {
        lines.push("  Result           no matches found".to_string());
    }

    Ok(lines.join("\n"))
}

pub(crate) fn render_last_tool_debug_report(
    session: &Session,
) -> Result<String, Box<dyn std::error::Error>> {
    let last_tool_use = session
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            message.blocks.iter().rev().find_map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
        })
        .ok_or_else(|| "no prior tool call found in session".to_string())?;

    let tool_result = session.messages.iter().rev().find_map(|message| {
        message.blocks.iter().rev().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } if tool_use_id == &last_tool_use.0 => {
                Some((tool_name.clone(), output.clone(), *is_error))
            }
            _ => None,
        })
    });

    let mut lines = vec![
        "Debug tool call".to_string(),
        "  Action           inspect the last recorded tool call and its result".to_string(),
        format!("  Tool id          {}", last_tool_use.0),
        format!("  Tool name        {}", last_tool_use.1),
        "  Input".to_string(),
        indent_block(&last_tool_use.2, 4),
    ];

    match tool_result {
        Some((tool_name, output, is_error)) => {
            lines.push("  Result".to_string());
            lines.push(format!("    name           {tool_name}"));
            lines.push(format!(
                "    status         {}",
                if is_error { "error" } else { "ok" }
            ));
            lines.push(indent_block(&output, 4));
        }
        None => lines.push("  Result           missing tool result".to_string()),
    }

    Ok(lines.join("\n"))
}

pub(crate) fn indent_block(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn validate_no_args(
    command_name: &str,
    args: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(args) = args.map(str::trim).filter(|value| !value.is_empty()) {
        return Err(format!(
            "{command_name} does not accept arguments. Received: {args}\nUsage: {command_name}"
        )
        .into());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(crate) fn print_help_to(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "scode v{VERSION}")?;
    writeln!(out)?;
    writeln!(out, "Usage:")?;
    writeln!(
        out,
        "  scode [--model MODEL] [--allowedTools TOOL[,TOOL...]]"
    )?;
    writeln!(out, "      Start the interactive REPL")?;
    writeln!(
        out,
        "  scode [--model MODEL] [--output-format text|json] prompt TEXT"
    )?;
    writeln!(out, "      Send one prompt and exit")?;
    writeln!(
        out,
        "  scode [--model MODEL] [--output-format text|json] TEXT"
    )?;
    writeln!(out, "      Shorthand non-interactive prompt mode")?;
    writeln!(
        out,
        "  scode --resume [SESSION.jsonl|session-id|latest] [/status] [/compact] [...]"
    )?;
    writeln!(
        out,
        "      Inspect or maintain a saved session without entering the REPL"
    )?;
    writeln!(out, "  scode help")?;
    writeln!(out, "      Alias for --help")?;
    writeln!(out, "  scode version")?;
    writeln!(out, "      Alias for --version")?;
    writeln!(out, "  scode status")?;
    writeln!(
        out,
        "      Show the current local workspace status snapshot"
    )?;
    writeln!(out, "  scode sandbox")?;
    writeln!(out, "      Show the current sandbox isolation snapshot")?;
    writeln!(out, "  scode doctor")?;
    writeln!(
        out,
        "      Diagnose local auth, config, workspace, and sandbox health"
    )?;
    writeln!(out, "  scode login")?;
    writeln!(
        out,
        "      Import OAuth credentials from Claude Code's keychain"
    )?;
    writeln!(out, "  scode logout")?;
    writeln!(
        out,
        "      Clear saved OAuth credentials from keychain and file"
    )?;
    writeln!(out, "  scode acp [serve]")?;
    writeln!(
        out,
        "      Run the ACP JSON-RPC stdio server for editor integrations (aliases: --acp, -acp)"
    )?;
    writeln!(out, "      Source of truth: {OFFICIAL_REPO_SLUG}")?;
    writeln!(
        out,
        "      Warning: do not `{DEPRECATED_INSTALL_COMMAND}` (deprecated stub)"
    )?;
    writeln!(out, "  scode dump-manifests [--manifests-dir PATH]")?;
    writeln!(out, "  scode bootstrap-plan")?;
    writeln!(out, "  scode agents")?;
    writeln!(out, "  scode mcp")?;
    writeln!(out, "  scode skills")?;
    writeln!(
        out,
        "  scode system-prompt [--cwd PATH] [--date YYYY-MM-DD]"
    )?;
    writeln!(out, "  scode init")?;
    writeln!(
        out,
        "  scode export [PATH] [--session SESSION] [--output PATH]"
    )?;
    writeln!(
        out,
        "      Dump the latest (or named) session as markdown; writes to PATH or stdout"
    )?;
    writeln!(out)?;
    writeln!(out, "Flags:")?;
    writeln!(
        out,
        "  --model MODEL              Override the active model"
    )?;
    writeln!(
        out,
        "  --auth MODE                Auth mode: subscription, proxy, or api-key"
    )?;
    writeln!(
        out,
        "  --output-format FORMAT     Non-interactive output format: text or json"
    )?;
    writeln!(
        out,
        "  --compact                  Strip tool call details; print only the final assistant text (text mode only; useful for piping)"
    )?;
    writeln!(
        out,
        "  --permission-mode MODE     Set read-only, workspace-write, or danger-full-access"
    )?;
    writeln!(
        out,
        "  --dangerously-skip-permissions  Skip all permission checks"
    )?;
    writeln!(out, "  --allowedTools TOOLS       Restrict enabled tools (repeatable; comma-separated aliases supported)")?;
    writeln!(
        out,
        "  --version, -V              Print version and build information locally"
    )?;
    writeln!(out)?;
    writeln!(out, "Interactive slash commands:")?;
    writeln!(out, "{}", render_slash_command_help_filtered(STUB_COMMANDS))?;
    writeln!(out)?;
    let resume_commands = resume_supported_slash_commands()
        .into_iter()
        .filter(|spec| !STUB_COMMANDS.contains(&spec.name))
        .map(|spec| match spec.argument_hint {
            Some(argument_hint) => format!("/{} {}", spec.name, argument_hint),
            None => format!("/{}", spec.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Resume-safe commands: {resume_commands}")?;
    writeln!(out)?;
    writeln!(out, "Session shortcuts:")?;
    writeln!(
        out,
        "  REPL turns auto-save to .scode/sessions/<session-id>.{PRIMARY_SESSION_EXTENSION}"
    )?;
    writeln!(
        out,
        "  Use `{LATEST_SESSION_REFERENCE}` with --resume, /resume, or /session switch to target the newest saved session"
    )?;
    writeln!(
        out,
        "  Use /session list in the REPL to browse managed sessions"
    )?;
    writeln!(out, "Examples:")?;
    writeln!(out, "  scode --model claude-opus \"summarize this repo\"")?;
    writeln!(
        out,
        "  scode --output-format json prompt \"explain src/main.rs\""
    )?;
    writeln!(out, "  scode --compact \"summarize Cargo.toml\" | wc -l")?;
    writeln!(
        out,
        "  scode --allowedTools read,glob \"summarize Cargo.toml\""
    )?;
    writeln!(out, "  scode --resume {LATEST_SESSION_REFERENCE}")?;
    writeln!(
        out,
        "  scode --resume {LATEST_SESSION_REFERENCE} /status /diff /export notes.txt"
    )?;
    writeln!(out, "  scode agents")?;
    writeln!(out, "  scode mcp show my-server")?;
    writeln!(out, "  scode /skills")?;
    writeln!(out, "  scode doctor")?;
    writeln!(out, "  source of truth: {OFFICIAL_REPO_URL}")?;
    writeln!(
        out,
        "  do not run `{DEPRECATED_INSTALL_COMMAND}` — it installs a deprecated stub"
    )?;
    writeln!(out, "  scode init")?;
    writeln!(out, "  scode export")?;
    writeln!(out, "  scode export conversation.md")?;
    Ok(())
}

pub(crate) fn print_help(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    print_help_to(&mut buffer)?;
    let message = String::from_utf8(buffer)?;
    match output_format {
        CliOutputFormat::Text => print!("{message}"),
        CliOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "help",
                "message": message,
            }))?
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_commands_absent_from_resume_safe_help() {
        let mut help = Vec::new();
        print_help_to(&mut help).expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf8");
        let resume_line = help
            .lines()
            .find(|line| line.starts_with("Resume-safe commands:"))
            .expect("resume-safe command line should exist");
        let resume_roots = resume_line
            .trim_start_matches("Resume-safe commands:")
            .split(',')
            .filter_map(|entry| entry.trim().strip_prefix('/'))
            .filter_map(|entry| entry.split_whitespace().next())
            .collect::<Vec<_>>();

        for stub in STUB_COMMANDS {
            assert!(
                !resume_roots.contains(stub),
                "stub command /{stub} should not appear in resume-safe command list"
            );
        }

        assert!(resume_roots.contains(&"status"));
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
    fn colorize_diff_paints_added_and_removed_lines() {
        let diff = "diff --git a/foo b/foo\n--- a/foo\n+++ b/foo\n@@ -1,1 +1,1 @@\n-old\n+new\n unchanged\n";
        let painted = colorize_unified_diff(diff);
        // Plain text content is preserved exactly when ANSI escapes are stripped.
        assert_eq!(strip_ansi(&painted), diff);
        // The added line gets the green escape, the removed line the red one.
        assert!(painted.contains("\u{1b}[32m+new\u{1b}[0m"));
        assert!(painted.contains("\u{1b}[31m-old\u{1b}[0m"));
        // Hunk header is cyan.
        assert!(painted.contains("\u{1b}[36m@@ -1,1 +1,1 @@\u{1b}[0m"));
        // File header line `diff --git` is bold.
        assert!(painted.contains("\u{1b}[1mdiff --git a/foo b/foo\u{1b}[0m"));
    }

    #[test]
    fn colorize_diff_leaves_context_lines_unstyled() {
        let diff = " context line\n";
        let painted = colorize_unified_diff(diff);
        assert_eq!(painted, diff);
    }

    #[test]
    fn colorize_diff_does_not_repaint_plus_plus_plus() {
        // The `+++ b/file` and `--- a/file` headers should be bold (file
        // header style), not green/red (added/removed line style).
        let diff = "+++ b/foo\n--- a/foo\n";
        let painted = colorize_unified_diff(diff);
        assert!(painted.contains("\u{1b}[1m+++ b/foo\u{1b}[0m"));
        assert!(painted.contains("\u{1b}[1m--- a/foo\u{1b}[0m"));
        assert!(!painted.contains("\u{1b}[32m+++"));
        assert!(!painted.contains("\u{1b}[31m---"));
    }
}
