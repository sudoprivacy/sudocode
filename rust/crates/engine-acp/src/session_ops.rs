//! Session lifecycle + turn glue for the ACP renderer, on top of the seam.
//!
//! The ACP server is multi-session; the seam ([`engine_host::SessionEngine`] =
//! `EngineDelegate` + `SessionLifecycle`) is single-session. This module is the
//! thin ACP-side layer that WRAPS the seam: it builds / loads / forks a
//! `SessionEngine` per ACP session and translates the shared driver's
//! [`engine_core::EngineEvent`]s + `TurnComplete` back onto the ACP wire. All the
//! turn engine itself lives below the seam; nothing here re-implements it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol_schema::{
    ContentBlock as AcpContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent,
    ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use commands::acp_slash_commands;
use engine_core::engine_events::{
    CompactionStatus, SubagentEvent, SubagentLifecycle, SubagentStreamRef, SubagentUpdate,
};
use engine_core::{EngineEvent, TurnComplete};
use engine_host::prompt::build_acp_system_prompt;
use engine_host::session::{
    canonical_session_cwd, create_managed_session_handle_for, load_session_reference,
};
use engine_host::{SessionEngine, SessionLifecycle};
use runtime::WorkspaceRootScope;
use serde_json::{json, Map, Value};

use crate::acp_sdk_server::{
    CumulativeUsage, PromptUsage, SdkAcpConfig, SessionForkSource, SessionRegistry,
};

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
///
/// `subagent_events` is whether the client opted into the sub-agent contract
/// (`docs/acp.md` § Sub-agent events). Off, the output is exactly what it was
/// before that contract existed; on, spawn calls carry
/// `_meta.sudocode.agentSpawn` and [`EngineEvent::Subagent`] is forwarded.
#[must_use]
pub(crate) fn engine_event_to_session_update(
    session_id: &str,
    event: EngineEvent,
    subagent_events: bool,
) -> Option<SessionNotification> {
    let update = match event {
        // ACP's standard operation lifecycle represents engine-owned
        // maintenance too. No tool is registered or offered to the model.
        EngineEvent::Compaction(progress) => {
            if progress.status == CompactionStatus::Started {
                SessionUpdate::ToolCall(
                    ToolCall::new(progress.id.clone(), "context_compaction")
                        .kind(ToolKind::Other)
                        .status(ToolCallStatus::InProgress)
                        .raw_input(serde_json::json!({
                            "trigger": progress.trigger, "before_tokens": progress.before_tokens,
                        })),
                )
            } else {
                let status = if progress.status == CompactionStatus::Failed {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    progress.id.clone(),
                    ToolCallUpdateFields::new().status(status).raw_output(
                        serde_json::to_value(progress)
                            .expect("compaction progress is serializable"),
                    ),
                ))
            }
        }
        EngineEvent::ThinkingDelta { text } => SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            AcpContentBlock::Text(TextContent::new(text)),
        )),
        EngineEvent::TextDelta { text } => SessionUpdate::AgentMessageChunk(ContentChunk::new(
            AcpContentBlock::Text(TextContent::new(text)),
        )),
        EngineEvent::ToolCall { id, name, input } => {
            tool_call_update(id, &name, &input, subagent_events, None)
        }
        EngineEvent::ToolResult {
            id,
            name,
            output,
            is_error,
            ..
        } => tool_result_update(id, &name, output, is_error, subagent_events, None),
        EngineEvent::Subagent(event) if subagent_events => subagent_update(event),
        // Usage/ModelResolved/PromptCache/AutoCompaction/ToolProgress/HookProgress/
        // State/TurnStarted/Notice/etc. have no ACP session/update equivalent:
        // usage rides the prompt response `_meta`, the rest are REPL-only.
        _ => return None,
    };
    Some(SessionNotification::new(session_id.to_string(), update))
}

// ===========================================================================
// Sub-agent events (the `subagentEvents` contract, docs/acp.md)
// ===========================================================================

/// `{"sudocode": entries}` as an ACP `_meta`, or `None` when there is nothing
/// to say (so an update without sub-agent data serializes exactly as before).
fn sudocode_meta(entries: Map<String, Value>) -> Option<Map<String, Value>> {
    if entries.is_empty() {
        return None;
    }
    let mut meta = Map::new();
    meta.insert("sudocode".to_string(), Value::Object(entries));
    Some(meta)
}

/// The tools that spawn a sub-agent, under any spelling the model uses.
fn is_spawn_tool(name: &str) -> bool {
    matches!(
        runtime::tool_names::canonicalize_tool_name(name).as_str(),
        "agent_spawn" | "pid_fork"
    )
}

/// `agentSpawn` for the call as the model issued it: the agent does not exist
/// yet, so only what its arguments say.
fn spawn_meta_from_input(name: &str, input: &Value) -> Value {
    let mut spawn = Map::new();
    let str_field = |key: &str| input.get(key).and_then(Value::as_str);
    if let Some(name) = str_field("name") {
        spawn.insert("name".into(), json!(name));
    }
    if let Some(description) = str_field("description") {
        spawn.insert("description".into(), json!(description));
    }
    let subagent_type = if runtime::tool_names::canonicalize_tool_name(name) == "pid_fork" {
        Some("fork")
    } else {
        str_field("subagent_type").or_else(|| str_field("agent"))
    };
    if let Some(subagent_type) = subagent_type {
        spawn.insert("subagentType".into(), json!(subagent_type));
    }
    let background = input
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    spawn.insert("background".into(), json!(background));
    Value::Object(spawn)
}

/// `agentSpawn` for the call's result, from the `AgentOutput` it returned.
/// `None` when the agent never came to exist (the result is an error string).
fn spawn_meta_from_output(output: &Value) -> Option<Value> {
    output.get("agentId").and_then(Value::as_str)?;
    let mut spawn = Map::new();
    for key in [
        "agentId",
        "name",
        "description",
        "subagentType",
        "model",
        "color",
    ] {
        if let Some(value) = output.get(key).filter(|value| !value.is_null()) {
            spawn.insert(key.into(), value.clone());
        }
    }
    Some(Value::Object(spawn))
}

fn stream_meta(stream: &SubagentStreamRef) -> Value {
    json!({
        "parentToolCallId": stream.parent_tool_call_id,
        "agentId": stream.agent_id,
        "seq": stream.seq,
    })
}

/// A model-issued tool call. `stream` is `_meta.sudocode.subagent` when the
/// call belongs to a sub-agent's stream.
fn tool_call_update(
    id: String,
    name: &str,
    input: &str,
    subagent_events: bool,
    stream: Option<Value>,
) -> SessionUpdate {
    let raw_input =
        serde_json::from_str(input).unwrap_or_else(|_| Value::String(input.to_string()));
    let mut sudocode = Map::new();
    if let Some(stream) = stream {
        sudocode.insert("subagent".into(), stream);
    }
    if subagent_events && is_spawn_tool(name) {
        sudocode.insert("agentSpawn".into(), spawn_meta_from_input(name, &raw_input));
    }
    let mut call = ToolCall::new(id, name)
        .kind(ToolKind::Other)
        .status(ToolCallStatus::InProgress)
        .raw_input(raw_input);
    if let Some(meta) = sudocode_meta(sudocode) {
        call = call.meta(meta);
    }
    SessionUpdate::ToolCall(call)
}

/// A tool call's result.
fn tool_result_update(
    id: String,
    name: &str,
    output: String,
    is_error: bool,
    subagent_events: bool,
    stream: Option<Value>,
) -> SessionUpdate {
    let raw_output =
        serde_json::from_str(&output).unwrap_or_else(|_| Value::String(output.clone()));
    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    let mut sudocode = Map::new();
    if let Some(stream) = stream {
        sudocode.insert("subagent".into(), stream);
    }
    if subagent_events && is_spawn_tool(name) {
        if let Some(spawn) = spawn_meta_from_output(&raw_output) {
            sudocode.insert("agentSpawn".into(), spawn);
        }
    }
    // `content` carries the RAW output string (exactly what the model
    // saw), never the re-serialized Value — re-serializing would reformat
    // JSON and quote plain text. Empty output emits no content field
    // (Some(vec![]) would tell the client to clear its rendering).
    let content = (!output.is_empty()).then(|| {
        vec![ToolCallContent::from(AcpContentBlock::Text(
            TextContent::new(output),
        ))]
    });
    let mut update = ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new()
            .status(status)
            .content(content)
            .raw_output(raw_output),
    );
    if let Some(meta) = sudocode_meta(sudocode) {
        update = update.meta(meta);
    }
    SessionUpdate::ToolCallUpdate(update)
}

/// One sub-agent event on the parent session's wire.
fn subagent_update(event: SubagentEvent) -> SessionUpdate {
    let stream = event.stream.as_ref().map(stream_meta);
    let chunk = |text: String, stream: Option<Value>| {
        let mut chunk = ContentChunk::new(AcpContentBlock::Text(TextContent::new(text)));
        let mut sudocode = Map::new();
        if let Some(stream) = stream {
            sudocode.insert("subagent".into(), stream);
        }
        chunk.meta = sudocode_meta(sudocode);
        chunk
    };
    match event.update {
        SubagentUpdate::TextDelta { text } => SessionUpdate::AgentMessageChunk(chunk(text, stream)),
        SubagentUpdate::ThinkingDelta { text } => {
            SessionUpdate::AgentThoughtChunk(chunk(text, stream))
        }
        SubagentUpdate::ToolCall { id, name, input } => {
            tool_call_update(id, &name, &input, true, stream)
        }
        SubagentUpdate::ToolResult {
            id,
            name,
            output,
            is_error,
        } => tool_result_update(id, &name, output, is_error, true, stream),
        SubagentUpdate::Lifecycle(lifecycle) => lifecycle_update(*lifecycle, stream),
    }
}

/// `started` / `finished` of a spawned agent: a status-less update of the
/// spawning call.
fn lifecycle_update(lifecycle: SubagentLifecycle, stream: Option<Value>) -> SessionUpdate {
    let agent = &lifecycle.agent;
    let mut spawn = Map::new();
    spawn.insert("agentId".into(), json!(agent.agent_id));
    spawn.insert("name".into(), json!(agent.name));
    spawn.insert("description".into(), json!(agent.description));
    for (key, value) in [
        ("subagentType", &agent.subagent_type),
        ("model", &agent.model),
        ("color", &agent.color),
    ] {
        if let Some(value) = value {
            spawn.insert(key.into(), json!(value));
        }
    }
    spawn.insert("background".into(), json!(agent.background));
    let mut phase = Map::new();
    phase.insert("phase".into(), json!(lifecycle.phase.as_str()));
    phase.insert("seq".into(), json!(lifecycle.seq));
    for (key, value) in [
        ("status", &lifecycle.status),
        ("startedAt", &lifecycle.started_at),
        ("completedAt", &lifecycle.completed_at),
    ] {
        if let Some(value) = value {
            phase.insert(key.into(), json!(value));
        }
    }
    spawn.insert("lifecycle".into(), Value::Object(phase));

    let mut sudocode = Map::new();
    if let Some(stream) = stream {
        sudocode.insert("subagent".into(), stream);
    }
    sudocode.insert("agentSpawn".into(), Value::Object(spawn));
    let fields = match lifecycle.raw_output {
        Some(raw_output) => ToolCallUpdateFields::new().raw_output(raw_output),
        None => ToolCallUpdateFields::new(),
    };
    let mut update = ToolCallUpdate::new(lifecycle.tool_call_id, fields);
    if let Some(meta) = sudocode_meta(sudocode) {
        update = update.meta(meta);
    }
    SessionUpdate::ToolCallUpdate(update)
}

// ===========================================================================
// Session construction (new / load / fork)
// ===========================================================================

/// Build a brand-new ACP session engine. The caller has already entered the
/// session's cwd lease + workspace-root scope.
pub(crate) fn build_new_session(
    config: &SdkAcpConfig,
    cwd: &Path,
    mcp_servers: &BTreeMap<String, runtime::ScopedMcpServerConfig>,
    prompt_overrides: runtime::SystemPromptOverrides,
    memory: runtime::memory::MemoryMode,
) -> Result<(Arc<SessionEngine>, PathBuf), crate::AcpError> {
    let cwd = canonical_session_cwd(cwd).map_err(crate::AcpError::invalid_params)?;
    let system_prompt = build_acp_system_prompt(&cwd, &prompt_overrides, memory)
        .map_err(crate::AcpError::internal)?;
    let engine = SessionEngine::build(
        &cwd,
        mcp_servers,
        prompt_overrides,
        memory,
        system_prompt,
        &config.model,
        config.model_flag_raw.as_deref(),
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
    registry: &SessionRegistry,
    session_id: &str,
    cwd: &Path,
    mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
    prompt_overrides: runtime::SystemPromptOverrides,
    memory: runtime::memory::MemoryMode,
) -> Result<(Arc<SessionEngine>, PathBuf), crate::AcpError> {
    let cwd = canonical_session_cwd(cwd).map_err(crate::AcpError::invalid_params)?;
    if let Some(engine) = registry.engine(session_id) {
        if registry.cwd(session_id).as_ref() != Some(&cwd) {
            return Err(crate::AcpError::invalid_params("session cwd mismatch"));
        }
        engine
            .reconfigure(mcp_servers, prompt_overrides, memory)
            .map_err(crate::AcpError::internal)?;
        return Ok((engine, cwd));
    }
    let (handle, session) = load_session_reference(session_id)
        .map_err(|e| crate::AcpError::internal(format!("failed to load session: {e}")))?;
    registry.attach_completion_writer(&session, &cwd)?;
    let system_prompt = build_acp_system_prompt(&cwd, &prompt_overrides, memory)
        .map_err(crate::AcpError::internal)?;
    let engine = SessionEngine::open_persisted(
        &cwd,
        handle,
        session,
        &mcp_servers,
        prompt_overrides,
        memory,
        system_prompt,
        &config.model,
        config.model_flag_raw.as_deref(),
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
    cwd: &Path,
    mcp_servers: &BTreeMap<String, runtime::ScopedMcpServerConfig>,
    prompt_overrides: runtime::SystemPromptOverrides,
    memory: runtime::memory::MemoryMode,
) -> Result<(Arc<SessionEngine>, PathBuf), crate::AcpError> {
    let cwd = canonical_session_cwd(cwd).map_err(crate::AcpError::invalid_params)?;
    let parent = resolve_fork_source(source, registry)?;

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
        .save_fork_to_path(&parent, &handle.path)
        .map_err(|error| crate::AcpError::internal(format!("failed to persist fork: {error}")))?;
    let system_prompt = build_acp_system_prompt(&cwd, &prompt_overrides, memory)
        .map_err(crate::AcpError::internal)?;
    let engine = SessionEngine::open_persisted(
        &cwd,
        handle,
        forked,
        mcp_servers,
        prompt_overrides,
        memory,
        system_prompt,
        &config.model,
        config.model_flag_raw.as_deref(),
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

// ===========================================================================
// Images (VLM describe for text-only models)
// ===========================================================================

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
    let catalog = engine.model_catalog();
    let _scope = catalog
        .as_ref()
        .map(runtime::model_discovery::ModelCatalog::enter);
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
        token_breakdown_complete: (!per_turn.has_complete_token_breakdown()).then_some(false),
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
    tracer.record_usage_snapshot(
        "prompt_turn".to_string(),
        engine_core::UsageTokens {
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            cache_creation_input_tokens: turn.cache_creation_input_tokens,
            cache_read_input_tokens: turn.cache_read_input_tokens,
            reported_total_tokens: turn.reported_total_tokens,
        },
        engine_core::UsageCost {
            units: turn.cost_units,
            currency: turn.cost_currency.map(runtime::UsageCostCurrency::as_str),
        },
    );
    let cumulative = complete.session_usage;
    tracer.record_usage_snapshot(
        "session_summary".to_string(),
        engine_core::UsageTokens {
            input_tokens: cumulative.input_tokens,
            output_tokens: cumulative.output_tokens,
            cache_creation_input_tokens: cumulative.cache_creation_input_tokens,
            cache_read_input_tokens: cumulative.cache_read_input_tokens,
            reported_total_tokens: cumulative.reported_total_tokens,
        },
        engine_core::UsageCost {
            units: cumulative.cost_units,
            currency: cumulative
                .cost_currency
                .map(runtime::UsageCostCurrency::as_str),
        },
    );
}
