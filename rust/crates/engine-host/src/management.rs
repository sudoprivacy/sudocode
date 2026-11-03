//! Session management runs inside the same admitted engine turn as model work.
use crate::config::{default_permission_mode, load_sudocode_config_for_current_dir};
use crate::{SessionEngine, SessionLifecycle};
use commands::reports::{
    format_acp_compact_report, format_compaction_report_with_messages, format_model_report,
    format_model_switch_report, format_status_report, render_config_report, render_doctor_report,
    status_context, BuildInfo, StatusUsage,
};
use commands::{format_acp_unsupported_slash_command, render_acp_slash_command_help, SlashCommand};
use runtime::{UsageTracker, WorkspaceRootScope};
use std::path::Path;

pub(crate) fn execute(
    engine: &SessionEngine,
    line: &str,
    build: &engine_core::engine_events::ManagementContext,
    observer: &mut dyn runtime::RuntimeObserver,
) -> Result<bool, String> {
    let cwd = engine.session_cwd();
    let _scope = WorkspaceRootScope::enter(&cwd);
    let (text, cancelled) = match commands::bash_mode::parse_bang_prompt(line) {
        Some(command) => (run_bang_command(engine, command)?, false),
        None => handle_slash_command(engine, build, &cwd, line, observer)?,
    };
    observer.on_text_delta(&text);
    Ok(cancelled)
}

fn run_bang_command(engine: &SessionEngine, command: &str) -> Result<String, String> {
    let (text, blocks) = match commands::bash_mode::run(command) {
        Ok(output) => (
            commands::bash_mode::render_plain(
                command,
                &output.stdout,
                &output.stderr,
                output.return_code_interpretation.as_ref(),
            ),
            commands::bash_mode::history_blocks(command, &output.stdout, &output.stderr),
        ),
        Err(error) => {
            let error = error.to_string();
            (
                commands::bash_mode::render_plain(
                    command,
                    "",
                    &format!("Command failed: {error}"),
                    None,
                ),
                commands::bash_mode::failure_blocks(command, &error),
            )
        }
    };
    engine
        .push_user_blocks(blocks)
        .map_err(|e| format!("failed to record ! command in the transcript: {e}"))?;
    engine
        .persist()
        .map_err(|e| format!("failed to persist session: {e}"))?;
    Ok(text)
}

fn handle_slash_command(
    engine: &SessionEngine,
    config: &engine_core::engine_events::ManagementContext,
    cwd: &Path,
    input: &str,
    observer: &mut dyn runtime::RuntimeObserver,
) -> Result<(String, bool), String> {
    let command = match SlashCommand::parse(input) {
        Ok(Some(command)) => command,
        Ok(None) => return Ok((format_acp_unsupported_slash_command(input), false)),
        Err(error) => {
            return Ok((
                format!(
                    "{error}\n  Help             /help lists the commands available in ACP mode"
                ),
                false,
            ))
        }
    };

    let mut stop = false;
    let response = match &command {
        SlashCommand::Model { model } => model_report(engine, cwd, model.as_deref())?,
        SlashCommand::Effort { level } => {
            if let Some(level) = level {
                engine.set_effort(level)?;
            }
            let (preference, effective) = engine.current_effort();
            format!(
                "Effort: {} · Effective: {}",
                preference.as_deref().unwrap_or("auto"),
                effective.as_deref().unwrap_or("model default")
            )
        }
        SlashCommand::Help => render_acp_slash_command_help(),
        SlashCommand::Compact => {
            let outcome = engine.compact_admitted(observer)?;
            if outcome.cancelled {
                stop = true;
            }
            if let Some(report) = &outcome.report {
                format_compaction_report_with_messages(report, outcome.removed, outcome.kept)
            } else if outcome.cancelled {
                "Compact\n  Result           cancelled\n  Transcript       unchanged".to_string()
            } else {
                format_acp_compact_report(
                    outcome.before_tokens,
                    outcome.after_tokens,
                    outcome.removed,
                    outcome.kept,
                    outcome
                        .method
                        .as_ref()
                        .map(|(method, source)| (*method, source)),
                )
            }
        }
        SlashCommand::Status => status_report(engine, cwd)?,
        SlashCommand::Cost => {
            let usage = UsageTracker::from_session(&engine.session_snapshot()).cumulative_usage();
            format!(
                "Token usage: {} input, {} output, {} cache-create, {} cache-read",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
            )
        }
        SlashCommand::Config { section } => {
            let _scope = WorkspaceRootScope::enter(cwd);
            render_config_report(section.as_deref()).map_err(|e| e.to_string())?
        }
        SlashCommand::ConfigSet { .. } => {
            "/config set is only available in interactive REPL mode".to_string()
        }
        SlashCommand::Diff => git_diff_report(cwd)?,
        SlashCommand::Doctor => {
            let _scope = WorkspaceRootScope::enter(cwd);
            render_doctor_report(&BuildInfo {
                version: &config.agent_version,
                git_sha: config.git_sha.as_deref(),
                build_target: config.build_target.as_deref(),
            })
            .map(|report| report.render())
            .map_err(|e| e.to_string())?
        }
        _ => format_acp_unsupported_slash_command(input),
    };
    Ok((response, stop))
}

/// The `/diff` report (staged + unstaged working-tree diff).
fn git_diff_report(cwd: &Path) -> Result<String, String> {
    let run = |args: &[&str]| -> Result<String, String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|e| e.to_string())?;
        Ok(process_text::decode_process_output(&output.stdout).text)
    };
    let cached = run(&["diff", "--cached", "--no-color"])?;
    let unstaged = run(&["diff", "--no-color"])?;
    if cached.is_empty() && unstaged.is_empty() {
        return Ok("No changes detected.".to_string());
    }
    let staged = if cached.is_empty() {
        String::new()
    } else {
        format!("**Staged:**\n```diff\n{cached}```\n\n")
    };
    let unstaged = if unstaged.is_empty() {
        String::new()
    } else {
        format!("**Unstaged:**\n```diff\n{unstaged}```")
    };
    Ok(format!("{staged}{unstaged}"))
}

fn status_report(engine: &SessionEngine, cwd: &Path) -> Result<String, String> {
    let _scope = WorkspaceRootScope::enter(cwd);
    let snapshot = engine.session_snapshot();
    let tracker = UsageTracker::from_session(&snapshot);
    let handle = engine.session_handle();
    let account = engine.current_billing_account();
    let mut context = status_context(Some(&handle.path)).map_err(|e| e.to_string())?;
    context.peer = Some(crate::peer::status(&engine.agent_name()));
    Ok(format_status_report(
        &engine.current_model(),
        StatusUsage {
            message_count: snapshot.messages().len(),
            turns: tracker.turns(),
            latest: tracker.current_turn_usage(),
            cumulative: tracker.cumulative_usage(),
            estimated_tokens: 0,
        },
        default_permission_mode().as_str(),
        &context,
        None,
        &account.describe(),
    ))
}

fn model_report(engine: &SessionEngine, cwd: &Path, model: Option<&str>) -> Result<String, String> {
    let catalog = engine.model_catalog();
    if let Some(catalog) = &catalog {
        catalog.refresh_in_background();
    }
    let _catalog_scope = catalog
        .as_ref()
        .map(runtime::model_discovery::ModelCatalog::enter);
    let _scope = WorkspaceRootScope::enter(cwd);
    Ok(match model {
        None => {
            let snapshot = engine.session_snapshot();
            format_model_report(
                &engine.current_model(),
                snapshot.messages().len(),
                engine.usage_snapshot().turns(),
                &load_sudocode_config_for_current_dir(),
            )
        }
        Some(name) => {
            let report = SessionLifecycle::set_model(engine, name)?;
            if report.changed {
                format_model_switch_report(&report.previous, &report.resolved, report.message_count)
            } else {
                format_model_report(
                    &report.resolved,
                    report.message_count,
                    report.turns,
                    &load_sudocode_config_for_current_dir(),
                )
            }
        }
    })
}
