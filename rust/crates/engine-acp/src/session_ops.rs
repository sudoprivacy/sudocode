//! Session lifecycle + turn glue for the ACP renderer, on top of the seam.
//!
//! The ACP server is multi-session; the seam ([`engine_host::SessionEngine`] =
//! `EngineDelegate` + `SessionLifecycle`) is single-session. This module is the
//! thin ACP-side layer that WRAPS the seam: it builds / loads / forks a
//! `SessionEngine` per ACP session, drives one `run_turn` per `session/prompt`
//! through [`engine_core::ObserverAdapter`], and translates the resulting
//! [`engine_core::EngineEvent`]s + `TurnComplete` back onto the ACP wire. All the
//! turn engine itself lives below the seam; nothing here re-implements it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema::{
    ContentBlock as AcpContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
    ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use commands::reports::{
    format_acp_compact_report, format_model_report, format_model_switch_report,
    format_status_report, render_config_report, render_doctor_report, status_context, BuildInfo,
    StatusUsage,
};
use commands::{
    acp_slash_commands, format_acp_unsupported_slash_command, render_acp_slash_command_help,
    SlashCommand,
};
use engine_core::{EngineEvent, TurnComplete};
use engine_host::config::{
    default_permission_mode, extract_sudorouter_credentials, load_sudocode_config_for_current_dir,
    load_sudocode_config_for_cwd,
};
use engine_host::prompt::build_acp_system_prompt;
use engine_host::session::{
    canonical_session_cwd, create_managed_session_handle_for, load_session_reference,
};
use engine_host::{SessionEngine, SessionLifecycle};
use runtime::{ContentBlock, UsageTracker, WorkspaceRootScope};

use crate::acp_sdk_server::{
    AcpStopReason, CumulativeUsage, PromptUsage, SdkAcpConfig, SessionForkSource, SessionRegistry,
};
use crate::vlm_describe;

// ===========================================================================
// EngineEvent → ACP SessionUpdate
// ===========================================================================

/// Translate one streaming [`EngineEvent`] into an ACP `session/update`
/// notification, or `None` for events the ACP wire has no slot for
/// (usage/model/state/progress ride the prompt response `_meta` or are dropped).
///
/// The four streaming shapes map verbatim as the old `SdkSessionObserver` did:
/// thinking → `AgentThoughtChunk`, text → `AgentMessageChunk`, tool-call →
/// `ToolCall`, tool-result → `ToolCallUpdate{Completed|Failed}`.
#[must_use]
pub(crate) fn engine_event_to_session_update(
    session_id: &str,
    event: EngineEvent,
) -> Option<SessionNotification> {
    let update = match event {
        EngineEvent::ThinkingDelta { text } => SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            AcpContentBlock::Text(TextContent::new(text)),
        )),
        EngineEvent::TextDelta { text } => SessionUpdate::AgentMessageChunk(ContentChunk::new(
            AcpContentBlock::Text(TextContent::new(text)),
        )),
        EngineEvent::ToolCall { id, name, input } => {
            let raw_input = serde_json::from_str(&input)
                .unwrap_or_else(|_| serde_json::Value::String(input.clone()));
            SessionUpdate::ToolCall(
                ToolCall::new(id, name)
                    .kind(ToolKind::Other)
                    .status(ToolCallStatus::InProgress)
                    .raw_input(raw_input),
            )
        }
        EngineEvent::ToolResult {
            id,
            name: _,
            output,
            is_error,
        } => {
            let raw_output = serde_json::from_str(&output)
                .unwrap_or_else(|_| serde_json::Value::String(output.clone()));
            let status = if is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            // `content` carries the RAW output string (exactly what the model
            // saw), never the re-serialized Value — re-serializing would reformat
            // JSON and quote plain text. Empty output emits no content field
            // (Some(vec![]) would tell the client to clear its rendering).
            let content = (!output.is_empty()).then(|| {
                vec![ToolCallContent::from(AcpContentBlock::Text(
                    TextContent::new(output),
                ))]
            });
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id,
                ToolCallUpdateFields::new()
                    .status(status)
                    .content(content)
                    .raw_output(raw_output),
            ))
        }
        // Usage/ModelResolved/PromptCache/AutoCompaction/ToolProgress/HookProgress/
        // State/TurnStarted/Notice/etc. have no ACP session/update equivalent:
        // usage rides the prompt response `_meta`, the rest are REPL-only.
        _ => return None,
    };
    Some(SessionNotification::new(session_id.to_string(), update))
}

// ===========================================================================
// Session construction (new / load / fork)
// ===========================================================================

/// Build a brand-new ACP session engine. The caller has already entered the
/// session's cwd lease + workspace-root scope.
pub(crate) fn build_new_session(
    config: &SdkAcpConfig,
    cwd: PathBuf,
    mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
    prompt_overrides: runtime::SystemPromptOverrides,
) -> Result<(Arc<SessionEngine>, PathBuf), crate::AcpError> {
    let cwd = canonical_session_cwd(&cwd).map_err(crate::AcpError::invalid_params)?;
    let system_prompt =
        build_acp_system_prompt(&cwd, &prompt_overrides).map_err(crate::AcpError::internal)?;
    let engine = SessionEngine::build(
        &cwd,
        &mcp_servers,
        prompt_overrides,
        system_prompt,
        config.model.clone(),
        config.model_flag_raw.clone(),
        config.allowed_tools.clone(),
        config.permission_mode_override,
        config.reasoning_effort.clone(),
        config.auth_mode,
    )
    .map_err(crate::AcpError::internal)?;
    Ok((Arc::new(engine), cwd))
}

/// Open a persisted session (`session/load`). The caller has entered the cwd
/// lease + scope.
pub(crate) fn open_loaded_session(
    config: &SdkAcpConfig,
    session_id: &str,
    cwd: PathBuf,
    mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
    prompt_overrides: runtime::SystemPromptOverrides,
) -> Result<(Arc<SessionEngine>, PathBuf), crate::AcpError> {
    let cwd = canonical_session_cwd(&cwd).map_err(crate::AcpError::invalid_params)?;
    let (handle, session) = load_session_reference(session_id)
        .map_err(|e| crate::AcpError::internal(format!("failed to load session: {e}")))?;
    let system_prompt =
        build_acp_system_prompt(&cwd, &prompt_overrides).map_err(crate::AcpError::internal)?;
    let engine = SessionEngine::open_persisted(
        &cwd,
        handle,
        session,
        &mcp_servers,
        prompt_overrides,
        system_prompt,
        config.model.clone(),
        config.model_flag_raw.clone(),
        config.allowed_tools.clone(),
        config.permission_mode_override,
        config.reasoning_effort.clone(),
        config.auth_mode,
    )
    .map_err(crate::AcpError::internal)?;
    Ok((Arc::new(engine), cwd))
}

/// Fork a session (`session/new { _meta.sudocode.forkFrom }`). Reads the source
/// transcript, re-homes it under `cwd`, persists the fork, and opens it.
pub(crate) fn open_forked_session(
    config: &SdkAcpConfig,
    source: &SessionForkSource,
    registry: &SessionRegistry,
    cwd: PathBuf,
    mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
    prompt_overrides: runtime::SystemPromptOverrides,
) -> Result<(Arc<SessionEngine>, PathBuf), crate::AcpError> {
    let cwd = canonical_session_cwd(&cwd).map_err(crate::AcpError::invalid_params)?;
    let parent = resolve_fork_source(source, registry)?;
    let parent_tool_results = parent.tool_results_dir();

    let _scope = WorkspaceRootScope::enter(&cwd);
    // `Session::fork` keeps the transcript, compaction state, model and prompt
    // history, mints a fresh id and records the parent; the workspace root is
    // re-homed to the new cwd so the fork is a first-class session there.
    let forked = parent.fork(None).with_workspace_root(cwd.clone());
    let handle = create_managed_session_handle_for(&cwd, &forked.session_id).map_err(|error| {
        crate::AcpError::internal(format!("failed to create session handle: {error}"))
    })?;
    let forked = forked.with_persistence_path(handle.path.clone());
    forked
        .save_to_path(&handle.path)
        .map_err(|error| crate::AcpError::internal(format!("failed to persist fork: {error}")))?;
    // Offloaded tool results live beside the transcript and are referenced from
    // it by id; carry them over so the fork's "read more" paths keep resolving.
    if let (Some(from), Some(to)) = (parent_tool_results, forked.tool_results_dir()) {
        if from.is_dir() {
            if let Err(error) = copy_dir_recursive(&from, &to) {
                eprintln!(
                    "warning: fork of {} could not copy offloaded tool results: {error}",
                    parent.session_id
                );
            }
        }
    }
    let system_prompt =
        build_acp_system_prompt(&cwd, &prompt_overrides).map_err(crate::AcpError::internal)?;
    let engine = SessionEngine::open_persisted(
        &cwd,
        handle,
        forked,
        &mcp_servers,
        prompt_overrides,
        system_prompt,
        config.model.clone(),
        config.model_flag_raw.clone(),
        config.allowed_tools.clone(),
        config.permission_mode_override,
        config.reasoning_effort.clone(),
        config.auth_mode,
    )
    .map_err(crate::AcpError::internal)?;
    Ok((Arc::new(engine), cwd))
}

/// The transcript a `forkFrom` copies: a live session's freshest in-memory
/// state (via the registry's engine snapshot) when the source is open in this
/// process, a persisted transcript otherwise. Ports
/// `AcpSdkDelegate::resolve_fork_source`.
fn resolve_fork_source(
    source: &SessionForkSource,
    registry: &SessionRegistry,
) -> Result<runtime::Session, crate::AcpError> {
    let invalid = crate::AcpError::invalid_params;
    let source_cwd = source
        .cwd
        .as_deref()
        .map(canonical_session_cwd)
        .transpose()
        .map_err(|e| invalid(format!("forkFrom.cwd: {e}")))?;
    if let Some(session_id) = &source.session_id {
        // Open in this process → read its freshest state (the snapshot blocks
        // only if the source is mid-turn, then returns the completed transcript).
        if let (Some(engine), Some(live_cwd)) =
            (registry.engine(session_id), registry.cwd(session_id))
        {
            if let Some(expected) = &source_cwd {
                if live_cwd != *expected {
                    return Err(invalid(format!(
                        "forkFrom.sessionId {session_id} is open in {} not {}",
                        live_cwd.display(),
                        expected.display()
                    )));
                }
            }
            return Ok(engine.session_snapshot());
        }
        let Some(store_cwd) = &source_cwd else {
            return Err(invalid(format!(
                "forkFrom.sessionId {session_id} is not open in this process; pass forkFrom.cwd to fork a persisted session"
            )));
        };
        return load_persisted_session(store_cwd, session_id).map_err(invalid);
    }
    let store_cwd =
        source_cwd.ok_or_else(|| invalid("forkFrom needs sessionId and/or cwd".to_string()))?;
    let store = runtime::SessionStore::from_cwd(&store_cwd)
        .map_err(|e| invalid(format!("forkFrom.cwd: {e}")))?;
    let latest = store.latest_session().map_err(|e| {
        invalid(format!(
            "forkFrom.cwd {}: no persisted session to fork ({e})",
            store_cwd.display()
        ))
    })?;
    load_persisted_session(&store_cwd, &latest.id).map_err(invalid)
}

/// Read the persisted session `session_id` from `cwd`'s session store.
fn load_persisted_session(cwd: &Path, session_id: &str) -> Result<runtime::Session, String> {
    let store = runtime::SessionStore::from_cwd(cwd)
        .map_err(|e| format!("forkFrom.cwd {}: {e}", cwd.display()))?;
    store
        .load_session(session_id)
        .map(|loaded| loaded.session)
        .map_err(|e| {
            format!(
                "forkFrom.sessionId {session_id} not found under {}: {e}",
                cwd.display()
            )
        })
}

/// Copy a directory tree (files + sub-directories; symlinks followed).
fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

// ===========================================================================
// Images (VLM describe for text-only models)
// ===========================================================================

/// Prepare `images` for the session's active model and push them as `User`
/// messages before the turn runs. Native for vision-capable models; VLM-routed
/// (or preflight-downsampled) otherwise, so a text-only model still gets useful
/// content. Ports `AcpSdkDelegate::push_images`.
pub(crate) fn prepare_and_push_images(
    engine: &SessionEngine,
    images: &[(String, String)],
    cwd: &Path,
) -> Result<(), crate::AcpError> {
    let active_model = engine.current_model();
    let vision_capable = runtime::model_capabilities::vision_capable(&active_model);
    let sudocode_config = load_sudocode_config_for_cwd(cwd);
    let sudorouter_creds = extract_sudorouter_credentials(&sudocode_config);
    // Diagnostics on stderr (stdout is the JSON-RPC wire): the image-handling
    // e2e asserts on these lines to prove the VLM route actually fired.
    eprintln!("[push_images] entered — {} images", images.len());
    eprintln!("[push_images] active_model={active_model:?} vision_capable={vision_capable}");
    eprintln!(
        "[push_images] sudorouter_creds_present={}",
        sudorouter_creds.is_some()
    );

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(images.len());
    for (index, (data, mime_type)) in images.iter().enumerate() {
        let block = if vision_capable {
            match runtime::image_registry::preflight_base64(data, mime_type) {
                Ok((final_data, final_mime)) => ContentBlock::Image {
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
                Err(_) => ContentBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                },
            }
        } else {
            vlm_describe_block_or_placeholder(data, mime_type, index, sudorouter_creds.as_ref())
        };
        blocks.push(block);
    }
    engine
        .push_user_blocks(blocks)
        .map_err(crate::AcpError::internal)
}

/// Route an image through a VLM (via sudorouter) on a dedicated OS thread with
/// its own current-thread runtime (decoupled from the ACP task pool — see the
/// v2 runtime-nesting fix), returning a `ContentBlock::Text` description or a
/// graceful placeholder on any failure. Ports the same-named CLI helper.
fn vlm_describe_block_or_placeholder(
    image_b64: &str,
    mime_type: &str,
    index: usize,
    sudorouter_creds: Option<&(String, String)>,
) -> ContentBlock {
    let human_idx = index + 1;
    let Some((base_url, api_key)) = sudorouter_creds else {
        eprintln!(
            "[push_images] image #{human_idx} — no sudorouter creds, falling back to placeholder"
        );
        return ContentBlock::Text {
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
        Ok(join) => join
            .join()
            .unwrap_or_else(|_| Err("VLM worker thread panicked".to_string())),
        Err(e) => Err(format!("failed to spawn VLM worker thread: {e}")),
    };

    match result {
        Ok(description) => {
            eprintln!(
                "[push_images] image #{human_idx} — VLM done, {} desc chars",
                description.len()
            );
            ContentBlock::Text {
                text: format!("[Image #{human_idx}: {description}]"),
            }
        }
        Err(e) => {
            eprintln!("[push_images] image #{human_idx} — VLM describe failed: {e}");
            ContentBlock::Text {
                text: format!(
                    "[Image #{human_idx} could not be described automatically ({e}) — please retype your question with the image's key contents in text.]"
                ),
            }
        }
    }
}

// ===========================================================================
// Slash commands
// ===========================================================================

/// The commands the ACP server advertises + accepts.
#[must_use]
pub(crate) fn available_commands() -> &'static [commands::AcpSlashCommandSpec] {
    acp_slash_commands()
}

/// Whether `prompt` (`/name …`) is a slash command that must run under the
/// process-cwd lease (a runtime-construction path). Unknown commands only print
/// a hint, so they do not.
#[must_use]
pub(crate) fn slash_command_holds_cwd_lease(prompt: &str) -> bool {
    let name = prompt
        .trim_start()
        .strip_prefix('/')
        .and_then(|rest| rest.split_whitespace().next());
    name.is_some_and(|name| {
        acp_slash_commands()
            .iter()
            .any(|spec| spec.name == name && spec.holds_cwd_lease)
    })
}

/// Handle an ACP slash command against `engine`, returning the report text +
/// the turn's stop reason (`Cancelled` only when `/compact` honoured a
/// `session/cancel`). Ports `AcpSdkDelegate::handle_slash_command`, now driving
/// the seam (SessionEngine) + the shared `commands::reports` formatters.
pub(crate) fn handle_slash_command(
    engine: &SessionEngine,
    config: &SdkAcpConfig,
    cwd: &Path,
    input: &str,
) -> Result<(String, AcpStopReason), crate::AcpError> {
    let command = match SlashCommand::parse(input) {
        Ok(Some(command)) => command,
        Ok(None) => {
            return Ok((
                format_acp_unsupported_slash_command(input),
                AcpStopReason::EndTurn,
            ))
        }
        Err(error) => {
            return Ok((
                format!(
                    "{error}\n  Help             /help lists the commands available in ACP mode"
                ),
                AcpStopReason::EndTurn,
            ))
        }
    };

    let mut stop = AcpStopReason::EndTurn;
    let response = match &command {
        SlashCommand::Model { model } => {
            let _scope = WorkspaceRootScope::enter(cwd);
            match model {
                None => {
                    let snapshot = engine.session_snapshot();
                    format_model_report(
                        &engine.current_model(),
                        snapshot.messages.len(),
                        engine.usage_snapshot().turns(),
                        &load_sudocode_config_for_current_dir(),
                    )
                }
                Some(name) => {
                    let report = SessionLifecycle::set_model(engine, name)
                        .map_err(crate::AcpError::internal)?;
                    if report.changed {
                        format_model_switch_report(
                            &report.previous,
                            &report.resolved,
                            report.message_count,
                        )
                    } else {
                        format_model_report(
                            &report.resolved,
                            report.message_count,
                            report.turns,
                            &load_sudocode_config_for_current_dir(),
                        )
                    }
                }
            }
        }
        SlashCommand::Help => render_acp_slash_command_help(),
        SlashCommand::Compact => {
            let outcome = engine
                .compact_cancellable()
                .map_err(crate::AcpError::internal)?;
            if outcome.cancelled {
                stop = AcpStopReason::Cancelled;
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
        SlashCommand::Status => {
            let _scope = WorkspaceRootScope::enter(cwd);
            let snapshot = engine.session_snapshot();
            let tracker = UsageTracker::from_session(&snapshot);
            let handle = engine.session_handle();
            let account = engine.current_billing_account();
            format_status_report(
                &engine.current_model(),
                StatusUsage {
                    message_count: snapshot.messages.len(),
                    turns: tracker.turns(),
                    latest: tracker.current_turn_usage(),
                    cumulative: tracker.cumulative_usage(),
                    estimated_tokens: 0,
                },
                default_permission_mode().as_str(),
                &status_context(Some(&handle.path))
                    .map_err(|e| crate::AcpError::internal(e.to_string()))?,
                None,
                &account.describe(),
            )
        }
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
            render_config_report(section.as_deref())
                .map_err(|e| crate::AcpError::internal(e.to_string()))?
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
            .map_err(|e| crate::AcpError::internal(e.to_string()))?
        }
        _ => format_acp_unsupported_slash_command(input),
    };
    Ok((response, stop))
}

/// The `/diff` report (staged + unstaged working-tree diff).
fn git_diff_report(cwd: &Path) -> Result<String, crate::AcpError> {
    let run = |args: &[&str]| -> Result<String, crate::AcpError> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|e| crate::AcpError::internal(e.to_string()))?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

// ===========================================================================
// Prompt usage (built from the seam's TurnComplete)
// ===========================================================================

/// Build the `PromptUsage` the ACP prompt response carries, from the turn's
/// return value + a couple of engine reads. Ports the `PromptUsage` assembly in
/// `run_prompt_impl`; `None` when the turn reported no per-turn tokens.
#[must_use]
pub(crate) fn build_prompt_usage(
    engine: &SessionEngine,
    complete: &TurnComplete,
    auto_compacted: bool,
) -> Option<PromptUsage> {
    let per_turn = complete.turn_usage;
    if per_turn.total_tokens() == 0 {
        return None;
    }
    let model = complete
        .response_model
        .clone()
        .unwrap_or_else(|| engine.current_model());
    let context_window = u64::from(runtime::model_capabilities::context_window_or_default(
        &model,
    ));
    let cumulative = complete.session_usage;
    Some(PromptUsage {
        input_tokens: u64::from(per_turn.input_tokens),
        output_tokens: u64::from(per_turn.output_tokens),
        total_tokens: u64::from(per_turn.total_tokens()),
        cache_read_tokens: Some(u64::from(per_turn.cache_read_input_tokens)),
        cache_write_tokens: Some(u64::from(per_turn.cache_creation_input_tokens)),
        context_window_tokens: Some(context_window),
        estimated_session_tokens: Some(engine.estimated_tokens() as u64),
        cost_units: per_turn.cost_units,
        cost_currency: per_turn.cost_currency,
        cumulative_usage: Some(CumulativeUsage {
            input_tokens: u64::from(cumulative.input_tokens),
            output_tokens: u64::from(cumulative.output_tokens),
            total_tokens: u64::from(cumulative.total_tokens()),
            cached_read_tokens: Some(u64::from(cumulative.cache_read_input_tokens)),
            cached_write_tokens: Some(u64::from(cumulative.cache_creation_input_tokens)),
        }),
        auto_compacted,
    })
}

/// Record the turn's per-turn + cumulative usage to the session tracer, exactly
/// as `run_prompt_impl` did (the seam's `run_turn` does not, so the ACP path
/// keeps doing it here — the REPL's telemetry is unaffected).
pub(crate) fn record_turn_usage(engine: &SessionEngine, complete: &TurnComplete) {
    let Some(tracer) = engine.session_tracer() else {
        return;
    };
    let turn = complete.turn_usage;
    tracer.record_usage_with_cost(
        "prompt_turn".to_string(),
        turn.input_tokens,
        turn.output_tokens,
        turn.cache_creation_input_tokens,
        turn.cache_read_input_tokens,
        turn.cost_units,
        turn.cost_currency.map(runtime::UsageCostCurrency::as_str),
    );
    let cumulative = complete.session_usage;
    tracer.record_usage_with_cost(
        "session_summary".to_string(),
        cumulative.input_tokens,
        cumulative.output_tokens,
        cumulative.cache_creation_input_tokens,
        cumulative.cache_read_input_tokens,
        cumulative.cost_units,
        cumulative
            .cost_currency
            .map(runtime::UsageCostCurrency::as_str),
    );
}
