pub mod managed_agent;

/// Test-only seams exposed for integration tests.
///
/// These wrappers cross the crate boundary so `tools/tests/*.rs`
/// integration tests can drive internals like the sub-agent
/// multi-turn loop without waiting on a live LLM. Do NOT rely on
/// this module from production code — the shape may change without
/// a semver bump; runtime code should use the non-testing entry
/// points instead.
pub mod testing {
    use std::path::Path;

    use runtime::HookAbortSignal;

    /// Test seam for [`crate::run_multi_turn_loop`] — same contract,
    /// just re-exported under a non-underscore name.
    pub fn run_multi_turn_loop_for_test<F>(
        agent_id: &str,
        workspace_root: &Path,
        abort_signal: HookAbortSignal,
        initial_prompt: String,
        max_multi_turns: usize,
        run_turn_fn: F,
    ) -> Result<String, String>
    where
        F: FnMut(String) -> Result<String, String>,
    {
        crate::run_multi_turn_loop(
            agent_id,
            workspace_root,
            abort_signal,
            initial_prompt,
            max_multi_turns,
            run_turn_fn,
        )
    }

    /// Test seam for the pure XML formatter used to synthesise the
    /// resume prompt from mailbox envelopes.
    #[must_use]
    pub fn compose_next_turn_from_envelopes_for_test(
        envelopes: &[runtime::agent_mailbox::MailboxEnvelope],
    ) -> String {
        crate::compose_next_turn_from_envelopes(envelopes)
    }

    /// Test seam for `execute_todo_write`, returning the raw JSON
    /// output string so callers can grep for the streak-nudge
    /// substring without depending on the private `TodoWriteOutput`
    /// struct shape.
    pub fn execute_todo_write_for_test(input_json: &str) -> Result<String, String> {
        let input: crate::TodoWriteInput =
            serde_json::from_str(input_json).map_err(|e| e.to_string())?;
        let output = crate::execute_todo_write(input)?;
        serde_json::to_string(&output).map_err(|e| e.to_string())
    }

    /// Test seam for `prepare_agent_job`. Callers just want to fire
    /// the side effect (Verification -> reset_streak); the manifest
    /// return value is discarded because the test env has no real
    /// workspace to spawn into. Returns `Ok(())` when the job was
    /// prepared successfully (i.e. the reset side-effect fired),
    /// otherwise the wrapped error.
    pub fn prepare_agent_job_for_test(subagent_type: &str, prompt: &str) -> Result<(), String> {
        let input = crate::AgentInput {
            description: format!("test-{subagent_type}"),
            prompt: prompt.to_string(),
            subagent_type: Some(subagent_type.to_string()),
            name: None,
            model: None,
            run_in_background: Some(true),
            auth_mode: None,
            permission_mode: None,
        };
        crate::prepare_agent_job(input, None).map(|_| ())
    }

    /// Test seam for `build_forked_messages`. Produces the same
    /// ConversationMessage list that a real fork spawn would inject
    /// into its child's Session before the first API turn — the
    /// callers just want to inspect the shape for
    /// prompt-cache-prefix assertions.
    pub fn build_forked_messages_for_test(
        directive: &str,
        parent_assistant: &runtime::ConversationMessage,
    ) -> Vec<runtime::ConversationMessage> {
        crate::build_forked_messages(directive, parent_assistant)
    }

    /// Public projection of [`crate::AgentRunTelemetry`] for the
    /// telemetry-pipeline integration tests.
    #[derive(Debug, Clone, Copy)]
    pub struct AgentRunTelemetryView {
        pub total_tokens: u64,
        pub tool_uses: u64,
    }

    impl From<AgentRunTelemetryView> for crate::AgentRunTelemetry {
        fn from(v: AgentRunTelemetryView) -> Self {
            Self {
                total_tokens: v.total_tokens,
                tool_uses: v.tool_uses,
            }
        }
    }

    /// Seed a bare-minimum AgentOutput manifest on disk for the
    /// telemetry-pipeline tests. Returns the path to the JSON file.
    pub fn seed_agent_manifest_for_test(
        dir: &std::path::Path,
        agent_id: &str,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let output_md = dir.join(format!("{agent_id}.md"));
        let manifest_path = dir.join(format!("{agent_id}.json"));
        std::fs::write(&output_md, "# seed\n").expect("seed output_md");
        let value = serde_json::json!({
            "agentId": agent_id,
            "name": format!("test-{agent_id}"),
            "description": "seed",
            "subagentType": "general-purpose",
            "model": "test-model",
            "status": "running",
            "outputFile": output_md.display().to_string(),
            "manifestFile": manifest_path.display().to_string(),
            "createdAt": "0",
            "derivedState": "working",
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .expect("seed manifest");
        manifest_path
    }

    /// Test seam for `record_agent_telemetry`.
    pub fn record_agent_telemetry_for_test(
        manifest_path: &std::path::Path,
        telemetry: AgentRunTelemetryView,
    ) -> Result<(), String> {
        let raw = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
        let manifest: crate::AgentOutput = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        crate::record_agent_telemetry(&manifest, telemetry.into())
    }

    /// Test seam for `record_full_result_path` — mirrors the private
    /// helper used by the AgentSummary sidecar path.
    pub fn record_full_result_path_for_test(
        manifest_path: &std::path::Path,
        full_path: &std::path::Path,
    ) -> Result<(), String> {
        let raw = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
        let manifest: crate::AgentOutput = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        crate::record_full_result_path(&manifest, full_path)
    }

    /// Test seam for `persist_agent_terminal_state_with_telemetry`.
    pub fn persist_terminal_with_telemetry_for_test(
        manifest_path: &std::path::Path,
        status: &str,
        result: Option<&str>,
        error: Option<String>,
        telemetry: Option<AgentRunTelemetryView>,
    ) -> Result<(), String> {
        let raw = std::fs::read_to_string(manifest_path).map_err(|e| e.to_string())?;
        let manifest: crate::AgentOutput = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        crate::persist_agent_terminal_state_with_telemetry(
            &manifest,
            status,
            result,
            error,
            telemetry.map(Into::into),
        )
    }

    /// Test seam: does the summary-threshold gate — mirrors the
    /// production `maybe_summarize_agent_result` decision but does
    /// NOT invoke the LLM summarizer. Instead, when the text
    /// exceeds the threshold, it (a) writes the full text to the
    /// `.full.md` sibling, (b) updates the on-disk manifest with
    /// `result_full_path`, and (c) returns `(placeholder_summary,
    /// Some(full_path))`. Tests can then assert the sibling file
    /// exists + the manifest reflects the pointer without paying
    /// for a live LLM.
    pub fn maybe_summarize_for_test(
        manifest_json_path: &std::path::Path,
        full_text: &str,
        placeholder_summary: &str,
    ) -> Result<(String, Option<std::path::PathBuf>), String> {
        let raw = std::fs::read_to_string(manifest_json_path)
            .map_err(|e| format!("read manifest {}: {e}", manifest_json_path.display()))?;
        let manifest: crate::AgentOutput = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let Some(threshold) = crate::agent_summary_threshold_chars() else {
            return Ok((full_text.to_string(), None));
        };
        if full_text.chars().count() <= threshold {
            return Ok((full_text.to_string(), None));
        }
        let full_path = crate::write_full_result_and_update_manifest(&manifest, full_text)?;
        crate::record_full_result_path(&manifest, &full_path)?;
        Ok((placeholder_summary.to_string(), Some(full_path)))
    }
}

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use command_group::CommandGroup;

use api::{
    max_tokens_for_model, model_family_identity_for, resolve_provider_from_config, ApiError,
    ContentBlockDelta, InputContentBlock, InputMessage, MessageRequest, MessageResponse,
    OutputContentBlock, ProviderClient, StreamEvent as ApiStreamEvent, SudoCodeConfig, ToolChoice,
    ToolDefinition, ToolResultContentBlock,
};
use plugins::{PluginLoadOutcome, PluginManager, PluginTool};
use reqwest::blocking::Client;
use runtime::{
    agent_mailbox::{self, kinds as mailbox_kinds, MailboxEnvelope},
    check_freshness,
    cron_registry::CronRegistry,
    dedupe_superseded_commit_events, edit_file, execute_bash_with_abort, glob_search, grep_search,
    lsp_client::LspRegistry,
    mcp_tool_bridge::McpToolRegistry,
    permission_enforcer::{EnforcementResult, PermissionEnforcer},
    read_file,
    summary_compression::compress_summary_text,
    task_registry::TaskRegistry,
    write_file, ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, BashCommandInput,
    BashCommandOutput, BranchFreshness, ConfigLoader, ContentBlock, ConversationMessage,
    ConversationRuntime, GrepSearchInput, HookAbortSignal, LaneCommitProvenance, LaneEvent,
    LaneEventBlocker, LaneEventName, LaneEventStatus, LaneFailureClass, McpDegradedReport,
    MessageRole, PermissionMode, PermissionPolicy, PromptCacheEvent, ProviderFallbackConfig,
    RuntimeError, Session, StdFsBackend, SystemPrompt, ToolDispatchContext, ToolError,
    ToolExecutor, FORK_BOILERPLATE_TAG,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Global task registry shared across tool invocations within a session.
fn global_lsp_registry() -> &'static LspRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<LspRegistry> = OnceLock::new();
    REGISTRY.get_or_init(LspRegistry::new)
}

fn global_mcp_registry() -> &'static McpToolRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<McpToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(McpToolRegistry::new)
}

fn global_cron_registry() -> &'static CronRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<CronRegistry> = OnceLock::new();
    // Persistent store shared with the `scode cron` CLI and the scheduler,
    // so a cron the agent creates survives restart and actually fires —
    // rather than living only for this process. `SUDOCODE_CRON_STORE`
    // overrides the path (tests point it at a temp file; it is cron-only,
    // so isolating cron never perturbs other config-home consumers).
    REGISTRY.get_or_init(|| {
        let path = std::env::var_os("SUDOCODE_CRON_STORE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| runtime::default_config_home().join("crons.json"));
        CronRegistry::open(path)
    })
}

fn global_task_registry() -> &'static TaskRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<TaskRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TaskRegistry::new)
}

/// Global auth mode set by the CLI at startup. Subagents inherit this so they
/// use the same credential path as the main agent.
static GLOBAL_AUTH_MODE: std::sync::OnceLock<api::AuthMode> = std::sync::OnceLock::new();

/// Called by the CLI at startup to set the auth mode for the entire process.
/// Subagents automatically inherit this unless explicitly overridden.
pub fn set_global_auth_mode(mode: api::AuthMode) {
    let _ = GLOBAL_AUTH_MODE.set(mode);
}

/// In-process registry that tracks running agent threads and allows callers
/// to block until an agent reaches a terminal state ("completed" or "failed").
struct AgentCompletionRegistry {
    inner: std::sync::Mutex<BTreeMap<String, AgentCompletionEntry>>,
    condvar: std::sync::Condvar,
}

struct AgentCompletionEntry {
    terminal_manifest: Option<AgentOutput>,
}

impl AgentCompletionRegistry {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(BTreeMap::new()),
            condvar: std::sync::Condvar::new(),
        }
    }

    /// Register an agent before spawning its thread.
    fn register(&self, agent_id: &str) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(
            agent_id.to_string(),
            AgentCompletionEntry {
                terminal_manifest: None,
            },
        );
    }

    /// Mark an agent as finished and wake any waiters.
    fn mark_done(&self, manifest: &AgentOutput) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = map.get_mut(&manifest.agent_id) {
            entry.terminal_manifest = Some(manifest.clone());
        }
        self.condvar.notify_all();
    }

    /// Block until the agent reaches a terminal state or the timeout expires.
    /// Returns the terminal manifest on success, or an error message.
    fn await_agent(&self, agent_id: &str, timeout: Duration) -> Result<AgentOutput, String> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !map.contains_key(agent_id) {
            // Not registered in-process — try to read from the manifest file.
            drop(map);
            return read_manifest_from_store(agent_id);
        }
        let (map, wait_result) = self
            .condvar
            .wait_timeout_while(map, timeout, |map| {
                map.get(agent_id)
                    .is_none_or(|e| e.terminal_manifest.is_none())
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if wait_result.timed_out() {
            return Err(format!(
                "timed out waiting for agent {agent_id} after {}s",
                timeout.as_secs()
            ));
        }
        map.get(agent_id)
            .and_then(|e| e.terminal_manifest.clone())
            .ok_or_else(|| format!("agent {agent_id} not found after wait"))
    }
}

/// Fall back to reading the manifest from the agent store on disk.
fn read_manifest_from_store(agent_id: &str) -> Result<AgentOutput, String> {
    let store = agent_store_dir()?;
    let manifest_path = store.join(format!("{agent_id}.json"));
    let contents = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!("agent {agent_id} not found (no in-process record, no manifest file): {e}")
    })?;
    let manifest: AgentOutput = serde_json::from_str(&contents).map_err(|e| e.to_string())?;
    if manifest.status == "completed" || manifest.status == "failed" {
        Ok(manifest)
    } else {
        Err(format!(
            "agent {agent_id} exists on disk but is still '{}'",
            manifest.status
        ))
    }
}

fn global_agent_registry() -> &'static AgentCompletionRegistry {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<AgentCompletionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AgentCompletionRegistry::new)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolManifestEntry {
    pub name: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Base,
    Conditional,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRegistry {
    entries: Vec<ToolManifestEntry>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new(entries: Vec<ToolManifestEntry>) -> Self {
        Self { entries }
    }

    #[must_use]
    pub fn entries(&self) -> &[ToolManifestEntry] {
        &self.entries
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub required_permission: PermissionMode,
}

#[derive(Debug, Clone)]
pub struct GlobalToolRegistry {
    plugin_tools: Vec<PluginTool>,
    runtime_tools: Vec<RuntimeToolDefinition>,
    enforcer: Option<PermissionEnforcer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeToolDefinition {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub required_permission: PermissionMode,
}

impl GlobalToolRegistry {
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            plugin_tools: Vec::new(),
            runtime_tools: Vec::new(),
            enforcer: None,
        }
    }

    pub fn with_plugin_tools(plugin_tools: Vec<PluginTool>) -> Result<Self, String> {
        let builtin_names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .collect::<BTreeSet<_>>();
        let mut seen_plugin_names = BTreeSet::new();

        for tool in &plugin_tools {
            let name = tool.definition().name.clone();
            if builtin_names.contains(&name) {
                return Err(format!(
                    "plugin tool `{name}` conflicts with a built-in tool name"
                ));
            }
            if !seen_plugin_names.insert(name.clone()) {
                return Err(format!("duplicate plugin tool name `{name}`"));
            }
        }

        Ok(Self {
            plugin_tools,
            runtime_tools: Vec::new(),
            enforcer: None,
        })
    }

    pub fn with_runtime_tools(
        mut self,
        runtime_tools: Vec<RuntimeToolDefinition>,
    ) -> Result<Self, String> {
        let mut seen_names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name.to_string())
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .collect::<BTreeSet<_>>();

        for tool in &runtime_tools {
            if !seen_names.insert(tool.name.clone()) {
                return Err(format!(
                    "runtime tool `{}` conflicts with an existing tool name",
                    tool.name
                ));
            }
        }

        self.runtime_tools = runtime_tools;
        Ok(self)
    }

    #[must_use]
    pub fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.set_enforcer(enforcer);
        self
    }

    pub fn normalize_allowed_tools(
        &self,
        values: &[String],
    ) -> Result<Option<BTreeSet<String>>, String> {
        if values.is_empty() {
            return Ok(None);
        }

        let builtin_specs = mvp_tool_specs();
        let canonical_names = builtin_specs
            .iter()
            .map(|spec| spec.name.to_string())
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.definition().name.clone()),
            )
            .chain(self.runtime_tools.iter().map(|tool| tool.name.clone()))
            .collect::<Vec<_>>();
        let mut name_map = canonical_names
            .iter()
            .map(|name| (normalize_tool_name(name), name.clone()))
            .collect::<BTreeMap<_, _>>();

        for (alias, canonical) in [
            ("read", "read_file"),
            ("write", "write_file"),
            ("edit", "edit_file"),
            ("glob", "glob_search"),
            ("grep", "grep_search"),
        ] {
            name_map.insert(alias.to_string(), canonical.to_string());
        }

        let mut allowed = BTreeSet::new();
        for value in values {
            for token in value
                .split(|ch: char| ch == ',' || ch.is_whitespace())
                .filter(|token| !token.is_empty())
            {
                let normalized = normalize_tool_name(token);
                let canonical = name_map.get(&normalized).ok_or_else(|| {
                    format!(
                        "unsupported tool in --allowedTools: {token} (expected one of: {})",
                        canonical_names.join(", ")
                    )
                })?;
                allowed.insert(canonical.clone());
            }
        }

        Ok(Some(allowed))
    }

    #[must_use]
    pub fn definitions(&self, allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolDefinition> {
        // Coordinator hard tool-gate: when coordinator mode is on,
        // hide write-side tools from the LLM's schema entirely so it
        // can't even name them. Belt-and-suspenders enforcement also
        // fires at dispatch time in `execute_tool_with_enforcer`. See
        // `runtime::coordinator_mode` for the allowlist SSOT.
        let coord_gate =
            |name: &str| runtime::coordinator_mode::is_tool_allowed_in_coordinator_mode(name);
        let builtin = mvp_tool_specs()
            .into_iter()
            .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
            .filter(|spec| coord_gate(spec.name))
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            });
        let runtime = self
            .runtime_tools
            .iter()
            .filter(|tool| allowed_tools.is_none_or(|allowed| allowed.contains(tool.name.as_str())))
            .filter(|tool| coord_gate(tool.name.as_str()))
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            });
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(tool.definition().name.as_str()))
            })
            .filter(|tool| coord_gate(tool.definition().name.as_str()))
            .map(|tool| ToolDefinition {
                name: tool.definition().name.clone(),
                description: tool.definition().description.clone(),
                input_schema: tool.definition().input_schema.clone(),
            });
        builtin.chain(runtime).chain(plugin).collect()
    }

    pub fn permission_specs(
        &self,
        allowed_tools: Option<&BTreeSet<String>>,
    ) -> Result<Vec<(String, PermissionMode)>, String> {
        let builtin = mvp_tool_specs()
            .into_iter()
            .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
            .map(|spec| (spec.name.to_string(), spec.required_permission));
        let runtime = self
            .runtime_tools
            .iter()
            .filter(|tool| allowed_tools.is_none_or(|allowed| allowed.contains(tool.name.as_str())))
            .map(|tool| (tool.name.clone(), tool.required_permission));
        let plugin = self
            .plugin_tools
            .iter()
            .filter(|tool| {
                allowed_tools
                    .is_none_or(|allowed| allowed.contains(tool.definition().name.as_str()))
            })
            .map(|tool| {
                permission_mode_from_plugin(tool.required_permission())
                    .map(|permission| (tool.definition().name.clone(), permission))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(builtin.chain(runtime).chain(plugin).collect())
    }

    #[must_use]
    pub fn has_runtime_tool(&self, name: &str) -> bool {
        self.runtime_tools.iter().any(|tool| tool.name == name)
    }

    #[must_use]
    pub fn search(
        &self,
        query: &str,
        max_results: usize,
        pending_mcp_servers: Option<Vec<String>>,
        mcp_degraded: Option<McpDegradedReport>,
    ) -> ToolSearchOutput {
        let query = query.trim().to_string();
        let normalized_query = normalize_tool_search_query(&query);
        let matches = search_tool_specs(&query, max_results.max(1), &self.searchable_tool_specs());

        ToolSearchOutput {
            matches,
            query,
            normalized_query,
            total_deferred_tools: self.searchable_tool_specs().len(),
            pending_mcp_servers,
            mcp_degraded,
        }
    }

    pub fn set_enforcer(&mut self, enforcer: PermissionEnforcer) {
        self.enforcer = Some(enforcer);
    }

    pub fn execute(&self, name: &str, input: &Value) -> Result<String, String> {
        self.execute_with_abort(name, input, None)
    }

    pub fn execute_with_abort(
        &self,
        name: &str,
        input: &Value,
        abort_signal: Option<&HookAbortSignal>,
    ) -> Result<String, String> {
        self.execute_with_abort_and_context(name, input, abort_signal, None)
    }

    /// Dispatch a tool with the per-call context threaded through from
    /// the runtime tool loop. Used by [`CliToolExecutor`] to give the
    /// Agent tool's fork branch access to the parent's assistant
    /// message (see [`runtime::ToolDispatchContext`]).
    pub fn execute_with_abort_and_context(
        &self,
        name: &str,
        input: &Value,
        abort_signal: Option<&HookAbortSignal>,
        ctx: Option<&ToolDispatchContext>,
    ) -> Result<String, String> {
        if mvp_tool_specs().iter().any(|spec| spec.name == name) {
            return execute_tool_with_enforcer(
                self.enforcer.as_ref(),
                name,
                input,
                abort_signal,
                ctx,
            );
        }
        self.plugin_tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .ok_or_else(|| format!("unsupported tool: {name}"))?
            .execute(input)
            .map_err(|error| error.to_string())
    }

    fn searchable_tool_specs(&self) -> Vec<SearchableToolSpec> {
        let builtin = deferred_tool_specs()
            .into_iter()
            .map(|spec| SearchableToolSpec {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
            });
        let runtime = self.runtime_tools.iter().map(|tool| SearchableToolSpec {
            name: tool.name.clone(),
            description: tool.description.clone().unwrap_or_default(),
        });
        let plugin = self.plugin_tools.iter().map(|tool| SearchableToolSpec {
            name: tool.definition().name.clone(),
            description: tool.definition().description.clone().unwrap_or_default(),
        });
        builtin.chain(runtime).chain(plugin).collect()
    }
}

fn normalize_tool_name(value: &str) -> String {
    value.trim().replace('-', "_").to_ascii_lowercase()
}

fn permission_mode_from_plugin(value: &str) -> Result<PermissionMode, String> {
    match value {
        "read-only" => Ok(PermissionMode::ReadOnly),
        "workspace-write" => Ok(PermissionMode::WorkspaceWrite),
        "danger-full-access" => Ok(PermissionMode::DangerFullAccess),
        other => Err(format!("unsupported plugin permission: {other}")),
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn mvp_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "bash",
            description: "Execute a shell command in the current workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Maximum milliseconds to wait before interrupting the command (default 120000)." },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" },
                    "dangerouslyDisableSandbox": { "type": "boolean" },
                    "namespaceRestrictions": { "type": "boolean" },
                    "isolateNetwork": { "type": "boolean" },
                    "filesystemMode": { "type": "string", "enum": ["off", "workspace-only", "allow-list"] },
                    "allowedMounts": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "read_file",
            description: "Read a text file from the workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "write_file",
            description: "Write a text file in the workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "edit_file",
            description: "Replace text in a workspace file.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "glob_search",
            description: "Find files by glob pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "grep_search",
            description: "Search file contents with a regex pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "glob": { "type": "string" },
                    "output_mode": { "type": "string" },
                    "-B": { "type": "integer", "minimum": 0 },
                    "-A": { "type": "integer", "minimum": 0 },
                    "-C": { "type": "integer", "minimum": 0 },
                    "context": { "type": "integer", "minimum": 0 },
                    "-n": { "type": "boolean" },
                    "-i": { "type": "boolean" },
                    "type": { "type": "string" },
                    "head_limit": { "type": "integer", "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "multiline": { "type": "boolean" }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WebFetch",
            description:
                "Fetch a URL, convert it into readable text, and answer a prompt about it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "format": "uri" },
                    "prompt": { "type": "string" }
                },
                "required": ["url", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "WebSearch",
            description: "Search the web for current information and return cited results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                    "allowed_domains": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "blocked_domains": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TodoWrite",
            description: "Update the structured task list for the current session.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "activeForm": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"]
                                }
                            },
                            "required": ["content", "activeForm", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Skill",
            description: "Load a local skill definition and its instructions.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "skill": { "type": "string" },
                    "args": { "type": "string" }
                },
                "required": ["skill"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Agent",
            description: "Launch a specialized agent task. By default runs in the background and returns immediately. Set run_in_background=false to run synchronously. Use TaskOutput(agent_id=..., block=true) to await a background agent.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "A short (3-5 word) description of the task" },
                    "prompt": { "type": "string", "description": "The full task prompt for the agent" },
                    "subagent_type": { "type": "string", "description": "Agent type specialization: general-purpose (default), Explore (read-only research), Plan (planning), Verification (bash + read), scode-guide, statusline-setup." },
                    "name": { "type": "string", "description": "Optional human-readable label for this agent" },
                    "model": { "type": "string", "description": "Model ID override; defaults to the system default" },
                    "run_in_background": { "type": "boolean", "description": "When true (default), launch async and retrieve result later with TaskOutput(agent_id=..., block=true). When false, run synchronously and return the result." },
                    "auth_mode": { "type": "string", "enum": ["api-key", "proxy", "subscription"], "description": "Explicit auth mode for the subagent. Overrides auto-detection from config." },
                    "permission_mode": { "type": "string", "enum": ["bubble"], "description": "Permission escalation mode. `bubble` (the default and only currently-supported value) routes any permission prompt the sub-agent would show up to the parent process's terminal/ACP prompter — the parent human (or the driving ACP client) approves on the sub-agent's behalf. Reserved for future modes." }
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "ToolSearch",
            description: "Search for deferred or specialized tools by exact name or keywords.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "NotebookEdit",
            description: "Replace, insert, or delete a cell in a Jupyter notebook.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "notebook_path": { "type": "string" },
                    "cell_id": { "type": "string" },
                    "new_source": { "type": "string" },
                    "cell_type": { "type": "string", "enum": ["code", "markdown"] },
                    "edit_mode": { "type": "string", "enum": ["replace", "insert", "delete"] }
                },
                "required": ["notebook_path"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "Sleep",
            description: "Wait for a specified duration without holding a shell process.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "duration_ms": { "type": "integer", "minimum": 0 }
                },
                "required": ["duration_ms"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendUserMessage",
            description: "Send a message to the user.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "attachments": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "status": {
                        "type": "string",
                        "enum": ["normal", "proactive"]
                    }
                },
                "required": ["message", "status"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "Config",
            description: "Get or set Sudo Code settings.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "setting": { "type": "string" },
                    "value": {
                        "type": ["string", "boolean", "number"]
                    }
                },
                "required": ["setting"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "EnterPlanMode",
            description: "Enable a worktree-local planning mode override and remember the previous local setting for ExitPlanMode.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "ExitPlanMode",
            description: "Restore or clear the worktree-local planning mode override created by EnterPlanMode.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "StructuredOutput",
            description: "Return structured output in the requested format.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": true
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "REPL",
            description: "Execute code in a REPL-like subprocess.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string" },
                    "language": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "description": "Maximum milliseconds to wait before interrupting execution (default 120000)." }
                },
                "required": ["code", "language"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "PowerShell",
            description: "Execute a PowerShell command with optional timeout.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Maximum milliseconds to wait before interrupting the command (default 120000)." },
                    "description": { "type": "string" },
                    "run_in_background": { "type": "boolean" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "AskUserQuestion",
            description: "Ask the user structured questions and wait for their response. Use this tool whenever you need user preferences, configuration values, first-run setup answers, or any other structured input. Prefer title/description/questions[] for multi-step forms; simple legacy question/options is still accepted. Do not ask numbered setup questions directly in plain assistant text when this tool is available.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "description": { "type": "string" },
                    "question": { "type": "string" },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "prompt": { "type": "string" },
                                "kind": {
                                    "type": "string",
                                    "enum": ["single_select", "multi_select", "text", "boolean"]
                                },
                                "required": { "type": "boolean" },
                                "allowCustomInput": { "type": "boolean" },
                                "customInputHint": { "type": "string" },
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string" },
                                            "value": { "type": "string" },
                                            "description": { "type": "string" },
                                            "recommended": { "type": "boolean" }
                                        },
                                        "required": ["label", "value"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["id", "prompt"],
                            "additionalProperties": false
                        }
                    }
                }
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskCreate",
            description: "Create a background task that runs in a separate subprocess.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskGet",
            description: "Get the status and details of a background task by ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskList",
            description: "List all background tasks and background sub-agents with their current status. Sub-agents come from the Agent tool and appear under `background_agents` in the response. Use `backgrounded_only=true` to narrow to sub-agents whose status is `backgrounded` or `running` — the useful set when the coordinator wants to switch between live workers.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "backgrounded_only": {
                        "type": "boolean",
                        "description": "When true, `background_agents` includes only sub-agents whose status is `backgrounded` or `running`. Defaults to false (all statuses)."
                    }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "TaskStop",
            description: "Stop a running background task by ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" }
                },
                "required": ["task_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskUpdate",
            description: "Send a message or update to a running background task.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["task_id", "message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "TaskOutput",
            description: "Retrieve output from a background task or agent. Use task_id for TaskRegistry tasks. Use agent_id to retrieve (and optionally await) a background agent launched with Agent(run_in_background=true). Set block=true to wait until the agent finishes. A single blocking call waits at most 60000 ms (clamped); if the agent is still running it returns retrieval_status=\"timeout\" — call TaskOutput again to keep waiting.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "ID of a TaskRegistry task to retrieve output from" },
                    "agent_id": { "type": "string", "description": "ID of a background agent launched with Agent(run_in_background=true)" },
                    "block": { "type": "boolean", "description": "When true (default), wait until the agent finishes before returning" },
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Maximum milliseconds to wait when block=true (default 30000, capped at 60000). On timeout the response has retrieval_status=\"timeout\" — re-call to keep waiting." }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "CronCreate",
            description: "Create a scheduled recurring task.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schedule": { "type": "string" },
                    "prompt": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["schedule", "prompt"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronDelete",
            description: "Delete a scheduled recurring task by ID.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cron_id": { "type": "string" }
                },
                "required": ["cron_id"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "CronList",
            description: "List all scheduled recurring tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "SendMessage",
            description: concat!(
                "Send a message to another agent teammate via the workspace mailbox. ",
                "Recipients receive one JSONL line at ",
                "<workspace>/.sudocode-inbox/<recipient>.jsonl. ",
                "Set `to` to a bare teammate name or \"*\" for broadcast. ",
                "`message` is either a plain string (requires `summary`) or a structured object ",
                "{type: shutdown_request|shutdown_response|plan_approval_response, ...}. ",
                "Structured messages CANNOT be broadcast (`to: \"*\"`). ",
                "Live delivery: `shutdown_request` calls abort() on the target subagent's ",
                "HookAbortSignal via the process-wide agent-abort registry, so an in-process ",
                "background subagent stops on its next tool-loop check. Plain-text messages are ",
                "written to disk for observability but consumption by a running subagent requires ",
                "the multi-turn subagent loop (deferred)."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient: teammate name, or \"*\" for broadcast to all teammates."
                    },
                    "summary": {
                        "type": "string",
                        "description": "A 5-10 word summary shown as a preview in the UI (required when message is a string)."
                    },
                    "message": {
                        "description": "Plain text (string) or a structured message object.",
                        "oneOf": [
                            { "type": "string" },
                            {
                                "type": "object",
                                "properties": {
                                    "type": {
                                        "type": "string",
                                        "enum": ["shutdown_request", "shutdown_response", "plan_approval_response"]
                                    },
                                    "request_id": { "type": "string" },
                                    "approve": { "type": "boolean" },
                                    "reason": { "type": "string" },
                                    "feedback": { "type": "string" }
                                },
                                "required": ["type"]
                            }
                        ]
                    },
                    "sender": {
                        "type": "string",
                        "description": "Optional sender name; defaults to \"team-lead\" for the main agent."
                    }
                },
                "required": ["to", "message"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::WorkspaceWrite,
        },
        ToolSpec {
            name: "LSP",
            description: "Query Language Server Protocol for code intelligence (symbols, references, diagnostics).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["symbols", "references", "diagnostics", "definition", "hover"] },
                    "path": { "type": "string" },
                    "line": { "type": "integer", "minimum": 0 },
                    "character": { "type": "integer", "minimum": 0 },
                    "query": { "type": "string" }
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ListMcpResources",
            description: "List available resources from connected MCP servers.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "ReadMcpResource",
            description: "Read a specific resource from an MCP server by URI.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        ToolSpec {
            name: "McpAuth",
            description: "Authenticate with an MCP server that requires OAuth or credentials.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "required": ["server"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        ToolSpec {
            name: "MCP",
            description: "Execute a tool provided by a connected MCP server.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "tool": { "type": "string" },
                    "arguments": { "type": "object" }
                },
                "required": ["server", "tool"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
    ]
}

/// Check permission before executing a tool. Returns Err with denial reason if blocked.
pub fn enforce_permission_check(
    enforcer: &PermissionEnforcer,
    tool_name: &str,
    input: &Value,
) -> Result<(), String> {
    let input_str = serde_json::to_string(input).unwrap_or_default();
    let result = enforcer.check(tool_name, &input_str);

    match result {
        EnforcementResult::Allowed => Ok(()),
        EnforcementResult::Denied { reason, .. } => Err(reason),
    }
}

pub fn execute_tool(name: &str, input: &Value) -> Result<String, String> {
    execute_tool_with_abort(name, input, None)
}

pub fn execute_tool_with_abort(
    name: &str,
    input: &Value,
    abort_signal: Option<&HookAbortSignal>,
) -> Result<String, String> {
    execute_tool_with_enforcer(None, name, input, abort_signal, None)
}

fn execute_tool_with_enforcer(
    enforcer: Option<&PermissionEnforcer>,
    name: &str,
    input: &Value,
    abort_signal: Option<&HookAbortSignal>,
    ctx: Option<&ToolDispatchContext>,
) -> Result<String, String> {
    // Coordinator hard tool-gate — belt-and-suspenders against a
    // non-compliant model that hallucinates a forbidden tool name
    // even though the LLM schema hides it (see
    // `GlobalToolRegistry::definitions`). Fires only when
    // SUDOCODE_COORDINATOR_MODE is truthy; no cost otherwise.
    if !runtime::coordinator_mode::is_tool_allowed_in_coordinator_mode(name) {
        return Err(format!(
            "tool `{name}` is not available in coordinator mode; delegate write-side work to a worker via `Agent(...)` (or use SendMessage to continue an existing worker)."
        ));
    }
    match name {
        "bash" => {
            // Parse input to get the command for permission classification
            let bash_input: BashCommandInput = from_value(input)?;
            let classified_mode = classify_bash_permission(&bash_input.command);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, classified_mode)?;
            run_bash(bash_input, abort_signal)
        }
        "read_file" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<ReadFileInput>(input).and_then(run_read_file)
        }
        "write_file" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<WriteFileInput>(input).and_then(run_write_file)
        }
        "edit_file" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<EditFileInput>(input).and_then(run_edit_file)
        }
        "glob_search" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<GlobSearchInputValue>(input).and_then(run_glob_search)
        }
        "grep_search" => {
            maybe_enforce_permission_check(enforcer, name, input)?;
            from_value::<GrepSearchInput>(input).and_then(run_grep_search)
        }
        "WebFetch" => from_value::<WebFetchInput>(input).and_then(run_web_fetch),
        "WebSearch" => from_value::<WebSearchInput>(input).and_then(run_web_search),
        "TodoWrite" => from_value::<TodoWriteInput>(input).and_then(run_todo_write),
        "Skill" => from_value::<SkillInput>(input).and_then(run_skill),
        "Agent" => from_value::<AgentInput>(input).and_then(|input| run_agent(input, ctx)),
        "ToolSearch" => from_value::<ToolSearchInput>(input).and_then(run_tool_search),
        "NotebookEdit" => from_value::<NotebookEditInput>(input).and_then(run_notebook_edit),
        "Sleep" => from_value::<SleepInput>(input).and_then(|input| run_sleep(input, abort_signal)),
        "SendUserMessage" | "Brief" => from_value::<BriefInput>(input).and_then(run_brief),
        "Config" => from_value::<ConfigInput>(input).and_then(run_config),
        "EnterPlanMode" => from_value::<EnterPlanModeInput>(input).and_then(run_enter_plan_mode),
        "ExitPlanMode" => from_value::<ExitPlanModeInput>(input).and_then(run_exit_plan_mode),
        "StructuredOutput" => {
            from_value::<StructuredOutputInput>(input).and_then(run_structured_output)
        }
        "REPL" => from_value::<ReplInput>(input).and_then(|input| run_repl(input, abort_signal)),
        "PowerShell" => {
            // Parse input to get the command for permission classification
            let ps_input: PowerShellInput = from_value(input)?;
            let classified_mode = classify_powershell_permission(&ps_input.command);
            maybe_enforce_permission_check_with_mode(enforcer, name, input, classified_mode)?;
            run_powershell(ps_input, abort_signal)
        }
        "AskUserQuestion" => {
            from_value::<AskUserQuestionInput>(input).and_then(run_ask_user_question)
        }
        "TaskCreate" => from_value::<TaskCreateInput>(input).and_then(run_task_create),
        "TaskGet" => from_value::<TaskIdInput>(input).and_then(run_task_get),
        "TaskList" => run_task_list(input.clone()),
        "TaskStop" => from_value::<TaskIdInput>(input).and_then(run_task_stop),
        "TaskUpdate" => from_value::<TaskUpdateInput>(input).and_then(run_task_update),
        "TaskOutput" => from_value::<TaskOutputInput>(input).and_then(run_task_output),
        "CronCreate" => from_value::<CronCreateInput>(input).and_then(run_cron_create),
        "CronDelete" => from_value::<CronDeleteInput>(input).and_then(run_cron_delete),
        "CronList" => run_cron_list(input.clone()),
        "SendMessage" => from_value::<SendMessageInput>(input).and_then(run_send_message),
        "LSP" => from_value::<LspInput>(input).and_then(run_lsp),
        "ListMcpResources" => {
            from_value::<McpResourceInput>(input).and_then(run_list_mcp_resources)
        }
        "ReadMcpResource" => from_value::<McpResourceInput>(input).and_then(run_read_mcp_resource),
        "McpAuth" => from_value::<McpAuthInput>(input).and_then(run_mcp_auth),
        "MCP" => from_value::<McpToolInput>(input).and_then(run_mcp_tool),
        _ => Err(format!("unsupported tool: {name}")),
    }
}

fn maybe_enforce_permission_check(
    enforcer: Option<&PermissionEnforcer>,
    tool_name: &str,
    input: &Value,
) -> Result<(), String> {
    if let Some(enforcer) = enforcer {
        enforce_permission_check(enforcer, tool_name, input)?;
    }
    Ok(())
}

/// Enforce permission check with a dynamically classified permission mode.
/// Used for tools like bash and `PowerShell` where the required permission
/// depends on the actual command being executed.
fn maybe_enforce_permission_check_with_mode(
    enforcer: Option<&PermissionEnforcer>,
    tool_name: &str,
    input: &Value,
    required_mode: PermissionMode,
) -> Result<(), String> {
    if let Some(enforcer) = enforcer {
        let input_str = serde_json::to_string(input).unwrap_or_default();
        let result = enforcer.check_with_required_mode(tool_name, &input_str, required_mode);

        match result {
            EnforcementResult::Allowed => Ok(()),
            EnforcementResult::Denied { reason, .. } => Err(reason),
        }
    } else {
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_ask_user_question(input: AskUserQuestionInput) -> Result<String, String> {
    use std::io;

    let stdout = io::stdout();
    let stdin = io::stdin();
    let mut out = stdout.lock();
    let mut reader = stdin.lock();

    run_ask_user_question_v2(input, &mut out, &mut reader)
}

fn prompt_user_for_answer(
    out: &mut impl Write,
    reader: &mut impl BufRead,
    question: &str,
    options: Option<&Vec<String>>,
) -> Result<String, String> {
    writeln!(out, "\n[Question] {question}").map_err(|e| e.to_string())?;

    if let Some(options) = options {
        for (i, option) in options.iter().enumerate() {
            writeln!(out, "  {}. {}", i + 1, option).map_err(|e| e.to_string())?;
        }
        write!(out, "Enter choice (1-{}): ", options.len()).map_err(|e| e.to_string())?;
    } else {
        write!(out, "Your answer: ").map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())?;

    let mut response = String::new();
    reader.read_line(&mut response).map_err(|e| e.to_string())?;
    let response = response.trim().to_string();

    if let Some(options) = options {
        if let Ok(idx) = response.parse::<usize>() {
            if idx >= 1 && idx <= options.len() {
                return Ok(options[idx - 1].clone());
            }
        }
    }

    Ok(response)
}

fn run_ask_user_question_v2(
    input: AskUserQuestionInput,
    out: &mut impl Write,
    reader: &mut impl BufRead,
) -> Result<String, String> {
    let (title, description, questions) = normalize_ask_user_question_input(input)?;

    if let Some(title) = &title {
        writeln!(out, "\n[Question Set] {title}").map_err(|e| e.to_string())?;
    }
    if let Some(description) = &description {
        writeln!(out, "{description}").map_err(|e| e.to_string())?;
    }

    let mut answers = Vec::with_capacity(questions.len());
    for (index, item) in questions.iter().enumerate() {
        let prompt = format!("{}. {}", index + 1, item.prompt);
        let option_labels = if item.options.is_empty() {
            None
        } else {
            Some(
                item.options
                    .iter()
                    .map(|option| option.label.clone())
                    .collect::<Vec<_>>(),
            )
        };

        let answer = prompt_user_for_answer(out, reader, &prompt, option_labels.as_ref())?;
        let matched_option = item
            .options
            .iter()
            .find(|option| option.label == answer || option.value == answer);

        answers.push(AskUserQuestionAnswer {
            id: item.id.clone(),
            value: matched_option
                .map(|option| option.value.clone())
                .unwrap_or_else(|| answer.clone()),
            label: matched_option
                .map(|option| option.label.clone())
                .or_else(|| (!answer.is_empty()).then_some(answer)),
        });
    }

    to_pretty_json(AskUserQuestionResult {
        status: "answered".to_string(),
        title,
        description,
        questions,
        answers,
    })
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_create(input: TaskCreateInput) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry.create(&input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_get(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.get(&input.task_id) {
        Some(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "prompt": task.prompt,
            "description": task.description,
            "task_packet": task.task_packet,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "messages": task.messages
        })),
        None => Err(format!("task not found: {}", input.task_id)),
    }
}

fn run_task_list(input: Value) -> Result<String, String> {
    let registry = global_task_registry();
    let tasks: Vec<_> = registry
        .list(None)
        .into_iter()
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "status": t.status,
                "prompt": t.prompt,
                "description": t.description,
                "task_packet": t.task_packet,
                "created_at": t.created_at,
                "updated_at": t.updated_at
            })
        })
        .collect();

    // Sub-agents (Agent tool spawns) live in a separate registry —
    // read them off disk so `TaskList` can be a single stop for
    // "everything running in this session," matching the downgraded
    // Background Agent Selector plan (§4.6). `backgrounded_only`
    // narrows to agents whose status is `backgrounded` or `running`
    // — exactly the set a coordinator would want to switch between.
    let backgrounded_only = input
        .get("backgrounded_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let agents = list_agent_snapshots_from_store(backgrounded_only).unwrap_or_default();

    to_pretty_json(json!({
        "tasks": tasks,
        "count": tasks.len(),
        "background_agents": agents,
        "background_agent_count": agents.len(),
    }))
}

/// Sub-agent snapshot suitable for `TaskList` output + a future
/// `/tasks list --backgrounded` slash-command view. Compact JSON
/// shape so the coordinator LLM can scan it without paying a large
/// context cost.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub status: String,
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub created_at: String,
}

/// Read every `<agent_id>.json` manifest from the agent store and
/// return snapshots. When `backgrounded_only == true`, only agents
/// with status `backgrounded` or `running` are included. Sorted by
/// `created_at` DESCENDING so the most recent agents surface first
/// (matches "you probably want the one you just launched" heuristic).
///
/// Errors accessing the directory itself surface as `Err`. Errors
/// reading individual manifests are logged-then-skipped so a
/// corrupt file doesn't wipe the whole list.
pub fn list_agent_snapshots_from_store(
    backgrounded_only: bool,
) -> Result<Vec<AgentSnapshot>, String> {
    let store = agent_store_dir()?;
    let read_dir = match std::fs::read_dir(&store) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read agent store {}: {e}", store.display())),
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<AgentOutput>(&text) else {
            continue;
        };
        if backgrounded_only {
            let status = manifest.status.trim().to_ascii_lowercase();
            if status != "backgrounded" && status != "running" {
                continue;
            }
        }
        out.push(AgentSnapshot {
            agent_id: manifest.agent_id,
            status: manifest.status,
            name: manifest.name,
            description: manifest.description,
            subagent_type: manifest.subagent_type,
            color: manifest.color,
            created_at: manifest.created_at,
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_stop(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.stop(&input.task_id) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message": "Task stopped"
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_update(input: TaskUpdateInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.update(&input.task_id, &input.message) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message_count": task.messages.len(),
            "last_message": input.message
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_task_output(input: TaskOutputInput) -> Result<String, String> {
    if let Some(agent_id) = &input.agent_id {
        return await_agent_output(agent_id, input.block, input.timeout_ms);
    }
    let task_id = input
        .task_id
        .as_deref()
        .ok_or_else(|| String::from("either task_id or agent_id is required"))?;
    let registry = global_task_registry();
    match registry.output(task_id) {
        Ok(output) => to_pretty_json(json!({
            "task_id": task_id,
            "output": output,
            "has_output": !output.is_empty()
        })),
        Err(e) => Err(e),
    }
}

const DEFAULT_AGENT_AWAIT_TIMEOUT_MS: u64 = 30_000;
// Cap a single blocking TaskOutput call so ACP/upper-layer transports don't
// drop the connection while we wait. Callers can re-issue TaskOutput to keep
// polling — see retrieval_status="timeout" in the returned JSON.
const MAX_AGENT_AWAIT_TIMEOUT_MS: u64 = 60_000;

fn agent_await_timeout_cap_ms() -> u64 {
    std::env::var("SUDOCODE_TASKOUTPUT_MAX_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(MAX_AGENT_AWAIT_TIMEOUT_MS)
}

fn await_agent_output(agent_id: &str, block: bool, timeout_ms: u64) -> Result<String, String> {
    let agent_id = agent_id.trim();
    if agent_id.is_empty() {
        return Err(String::from("agent_id must not be empty"));
    }

    if !block {
        // Non-blocking: read manifest from disk and return current state.
        if let Ok(manifest) = read_manifest_from_store(agent_id) {
            return format_agent_output(&manifest, "success");
        }
        // Try reading the manifest even if status is still running.
        let store = agent_store_dir()?;
        let path = store.join(format!("{agent_id}.json"));
        return match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let manifest: AgentOutput =
                    serde_json::from_str(&contents).map_err(|e| e.to_string())?;
                format_agent_output(&manifest, "not_ready")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("agent not found: {agent_id}"))
            }
            Err(e) => Err(e.to_string()),
        };
    }

    // Blocking: use condvar registry for instant wake.
    let timeout = Duration::from_millis(timeout_ms.min(agent_await_timeout_cap_ms()));
    match global_agent_registry().await_agent(agent_id, timeout) {
        Ok(manifest) => format_agent_output(&manifest, "success"),
        Err(e) if e.contains("timed out") => to_pretty_json(json!({
            "agent_id": agent_id,
            "status": "running",
            "retrieval_status": "timeout",
        })),
        Err(e) => Err(e),
    }
}

/// Route the manifest through the coordinator-mode-gated
/// `<task-notification>` XML renderer when coordinator mode is on;
/// otherwise fall back to the legacy JSON manifest shape.
///
/// The XML variant fires only for TERMINAL manifests (i.e. status
/// mapping to one of `completed | failed | killed`) — mid-flight polls
/// (`retrieval_status == "not_ready"`) still receive JSON because the
/// XML shape has no `<status>running</status>` slot.
fn format_agent_output(manifest: &AgentOutput, retrieval_status: &str) -> Result<String, String> {
    if runtime::coordinator_mode::is_coordinator_mode()
        && is_terminal_agent_status(&manifest.status)
    {
        return Ok(render_manifest_task_notification(manifest));
    }
    to_pretty_json(agent_output_json(manifest, retrieval_status))
}

/// Map an internal manifest status onto the terminal set. Mirrors
/// `runtime::coordinator_mode::normalize_task_notification_status` but
/// returns a bool (only terminal states drive the XML path).
///
/// `backgrounded` is deliberately NON-terminal: it means the worker
/// exceeded the sync-call auto-bg threshold but is still running.
/// Emitting `<task-notification><status>failed</status>...` for it
/// would be a lie — the parent should keep polling.
fn is_terminal_agent_status(status: &str) -> bool {
    !matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "" | "running" | "working" | "pending" | "backgrounded"
    )
}

/// Build a `<task-notification>` XML block from a terminal `AgentOutput`
/// manifest. Delegates the actual XML shape to
/// [`runtime::coordinator_mode::render_task_notification`] (SSOT).
fn render_manifest_task_notification(manifest: &AgentOutput) -> String {
    let normalized_status =
        runtime::coordinator_mode::normalize_task_notification_status(&manifest.status);
    let summary = build_notification_summary(manifest, normalized_status);
    let duration_ms = compute_notification_duration_ms(manifest);
    let view = runtime::coordinator_mode::TaskNotificationView {
        agent_id: manifest.agent_id.as_str(),
        status: normalized_status,
        summary: summary.as_str(),
        result: manifest.result.as_deref(),
        color: manifest.color.as_deref(),
        duration_ms,
        tool_uses: manifest.tool_uses,
        total_tokens: manifest.total_tokens,
    };
    runtime::coordinator_mode::render_task_notification(&view)
}

/// Human-readable one-line summary for the notification's `<summary>`
/// tag. Shape mirrors CC-fork's `renderTaskNotification` output:
/// - `completed` → `Agent "{description}" completed`
/// - `failed`    → `Agent "{description}" failed: {error}`
/// - `killed`    → `Agent "{description}" was stopped`
///
/// The status here is the normalized (three-value) form; caller must
/// pre-normalize via [`runtime::coordinator_mode::normalize_task_notification_status`].
fn build_notification_summary(manifest: &AgentOutput, normalized_status: &str) -> String {
    let label = if manifest.description.trim().is_empty() {
        manifest.name.as_str()
    } else {
        manifest.description.as_str()
    };
    match normalized_status {
        "completed" => format!("Agent \"{label}\" completed"),
        "killed" => format!("Agent \"{label}\" was stopped"),
        _ => match manifest.error.as_deref() {
            Some(err) if !err.trim().is_empty() => {
                format!("Agent \"{label}\" failed: {err}")
            }
            _ => format!("Agent \"{label}\" failed"),
        },
    }
}

/// Compute wall-clock duration in ms from the manifest's `created_at`
/// and `completed_at` timestamps. Both are produced by
/// [`iso8601_now`], which despite the name currently emits Unix-epoch
/// SECONDS as a decimal string. Returns `None` when either timestamp
/// is missing or unparseable — the caller then omits the
/// `<duration_ms>` tag.
fn compute_notification_duration_ms(manifest: &AgentOutput) -> Option<u64> {
    let start_secs: u64 = manifest.created_at.parse().ok()?;
    let end_secs: u64 = manifest.completed_at.as_deref()?.parse().ok()?;
    let delta = end_secs.saturating_sub(start_secs);
    Some(delta.saturating_mul(1000))
}

/// Snake_case wire projection of an [`AgentOutput`] manifest that
/// the LLM sees via `TaskOutput`. Separate from the manifest's own
/// on-disk camelCase serde shape because the LLM contract is
/// snake_case (see coordinator prompt + `pty_agent_summary.rs`) and
/// changing the on-disk format would break every existing session's
/// resume.
///
/// Owns `&str` slices back into `AgentOutput` — no allocations for
/// the fields themselves.
///
/// **Compile-time guard against forgotten fields**: build this via
/// [`build_agent_output_view`], which uses an EXHAUSTIVE pattern
/// destructure of `AgentOutput`. When a future commit adds a new
/// field to the manifest, the destructure fails to compile → the
/// developer is forced to decide "expose in TaskOutput or not?"
/// instead of silently omitting it (which is what caused
/// `resultFullPath` to be missing for 4 commits).
#[derive(Serialize)]
struct AgentOutputView<'a> {
    agent_id: &'a str,
    status: &'a str,
    retrieval_status: &'a str,
    output_file: &'a str,
    manifest_file: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_full_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_uses: Option<u64>,
}

fn build_agent_output_view<'a>(
    manifest: &'a AgentOutput,
    retrieval_status: &'a str,
) -> AgentOutputView<'a> {
    // Exhaustive destructure — MUST list every AgentOutput field so
    // rustc fails compilation if a new one appears. Fields deliberately
    // NOT surfaced to TaskOutput consumers (internal bookkeeping the
    // LLM shouldn't burn tokens on) are bound to `_ignored_*` names.
    let AgentOutput {
        agent_id,
        name: _ignored_name,
        description: _ignored_description,
        subagent_type: _ignored_subagent_type,
        model: _ignored_model,
        status,
        output_file,
        manifest_file,
        created_at: _ignored_created_at,
        started_at: _ignored_started_at,
        completed_at: _ignored_completed_at,
        lane_events: _ignored_lane_events,
        current_blocker: _ignored_current_blocker,
        derived_state: _ignored_derived_state,
        error,
        result,
        result_full_path,
        color,
        total_tokens,
        tool_uses,
        // `notified` is internal bookkeeping for the coordinator
        // push idempotency guard — deliberately NOT surfaced to
        // TaskOutput consumers because a coordinator LLM has no
        // business inspecting it.
        notified: _ignored_notified,
    } = manifest;
    AgentOutputView {
        agent_id,
        status,
        retrieval_status,
        output_file,
        manifest_file,
        result: result.as_deref(),
        error: error.as_deref(),
        result_full_path: result_full_path.as_deref(),
        color: color.as_deref(),
        total_tokens: *total_tokens,
        tool_uses: *tool_uses,
    }
}

fn agent_output_json(manifest: &AgentOutput, retrieval_status: &str) -> Value {
    // Serialize the wire view. `to_value` on a Serialize impl can
    // only fail on non-trivial types (Map key encoding); every field
    // here is primitive so `expect` is safe.
    serde_json::to_value(build_agent_output_view(manifest, retrieval_status))
        .expect("AgentOutputView is trivially serializable")
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_create(input: CronCreateInput) -> Result<String, String> {
    // Validate up front so the agent gets an actionable error instead of a
    // cron that silently never fires. The tool schedules standard 5-field
    // cron expressions.
    runtime::cron_schedule::validate(
        runtime::cron_registry::CronKind::Cron,
        &input.schedule,
        None,
    )?;
    let reg = global_cron_registry();
    let entry = reg.create(&input.schedule, &input.prompt, input.description.as_deref());
    // Seed the first fire time so the scheduler/tick considers it.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let next = runtime::cron_schedule::first_run_at(&entry, now);
    if next.is_some() {
        let _ = reg.set_next_run(&entry.cron_id, next);
    }
    to_pretty_json(json!({
        "cron_id": entry.cron_id,
        "schedule": entry.schedule,
        "prompt": entry.prompt,
        "description": entry.description,
        "enabled": entry.enabled,
        "created_at": entry.created_at,
        "next_run_at": next
    }))
}

#[allow(clippy::needless_pass_by_value)]
fn run_cron_delete(input: CronDeleteInput) -> Result<String, String> {
    match global_cron_registry().delete(&input.cron_id) {
        Ok(entry) => to_pretty_json(json!({
            "cron_id": entry.cron_id,
            "schedule": entry.schedule,
            "status": "deleted",
            "message": "Cron entry removed"
        })),
        Err(e) => Err(e),
    }
}

fn run_cron_list(_input: Value) -> Result<String, String> {
    let entries: Vec<_> = global_cron_registry()
        .list(false)
        .into_iter()
        .map(|e| {
            json!({
                "cron_id": e.cron_id,
                "schedule": e.schedule,
                "prompt": e.prompt,
                "description": e.description,
                "enabled": e.enabled,
                "run_count": e.run_count,
                "last_run_at": e.last_run_at,
                "created_at": e.created_at
            })
        })
        .collect();
    to_pretty_json(json!({
        "crons": entries,
        "count": entries.len()
    }))
}

// ── SendMessage ────────────────────────────────────────────────────

/// Resolve the workspace root that receives inbox files. The current
/// working directory is the canonical anchor — matches the way plan-mode,
/// todos, and agent manifests use `env::current_dir()`.
fn send_message_workspace() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("resolve workspace root: {e}"))
}

/// Best-effort sanitizer for a mailbox filename stem. Recipient
/// names can be arbitrary strings from the model — collapse anything
/// outside `[A-Za-z0-9_-]` to `_` so we can't traverse or overwrite
/// files outside `.sudocode-inbox/`.
fn sanitize_recipient(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Extract the sender label, defaulting to `TEAM_LEAD_NAME` (matches
/// CC-fork: `getAgentName() || TEAM_LEAD_NAME`). Callers can override
/// via the `sender` field for subagent contexts that know their own
/// name.
fn resolve_sender(input: &SendMessageInput) -> String {
    input
        .sender
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(TEAM_LEAD_NAME)
        .to_string()
}

fn write_envelope(
    workspace: &Path,
    recipient: &str,
    from: &str,
    text: &str,
    summary: Option<&str>,
    kind: &str,
    request_id: Option<&str>,
) -> Result<PathBuf, String> {
    let recipient_sanitized = sanitize_recipient(recipient);
    let envelope = MailboxEnvelope {
        from: from.to_string(),
        to: recipient.to_string(),
        text: text.to_string(),
        summary: summary.map(str::to_string),
        timestamp: 0, // filled in by append_envelope
        color: None,
        kind: kind.to_string(),
        request_id: request_id.map(str::to_string),
    };
    agent_mailbox::append_envelope(workspace, &recipient_sanitized, envelope)
}

fn generate_request_id(prefix: &str, target: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let target_slug = sanitize_recipient(target);
    format!("{prefix}_{target_slug}_{ts:x}")
}

#[allow(clippy::needless_pass_by_value)]
fn run_send_message(input: SendMessageInput) -> Result<String, String> {
    if input.to.trim().is_empty() {
        return Err("to must not be empty".to_string());
    }
    if input.to.contains('@') {
        return Err(
            "to must be a bare teammate name or \"*\" — there is only one team per session"
                .to_string(),
        );
    }

    let workspace = send_message_workspace()?;
    let sender = resolve_sender(&input);

    // ── Plain text branch ──────────────────────────────────────────
    if let Some(text) = input.message.as_str() {
        // Broadcast
        if input.to == "*" {
            let summary = input.summary.as_deref();
            let mut recipients = agent_mailbox::list_recipients(&workspace)?;
            // Never echo to sender.
            recipients.retain(|r| r != &sender);
            if recipients.is_empty() {
                return to_pretty_json(json!({
                    "success": true,
                    "message": "No teammates to broadcast to (empty inbox directory)",
                    "recipients": [],
                }));
            }
            for r in &recipients {
                write_envelope(
                    &workspace,
                    r,
                    &sender,
                    text,
                    summary,
                    mailbox_kinds::MESSAGE,
                    None,
                )?;
            }
            return to_pretty_json(json!({
                "success": true,
                "message": format!(
                    "Message broadcast to {} teammate(s): {}",
                    recipients.len(),
                    recipients.join(", ")
                ),
                "recipients": recipients,
                "routing": {
                    "sender": sender,
                    "target": "@team",
                    "summary": summary,
                    "content": text,
                }
            }));
        }
        // Point-to-point plain text
        if input.summary.as_deref().unwrap_or("").trim().is_empty() {
            return Err("summary is required when message is a string".to_string());
        }
        let path = write_envelope(
            &workspace,
            &input.to,
            &sender,
            text,
            input.summary.as_deref(),
            mailbox_kinds::MESSAGE,
            None,
        )?;
        return to_pretty_json(json!({
            "success": true,
            "message": format!("Message sent to {}'s inbox", input.to),
            "routing": {
                "sender": sender,
                "target": format!("@{}", input.to),
                "summary": input.summary,
                "content": text,
                "mailbox_path": path.display().to_string(),
            }
        }));
    }

    // ── Structured branch ──────────────────────────────────────────
    if input.to == "*" {
        return Err("structured messages cannot be broadcast (to: \"*\")".to_string());
    }
    let obj = input
        .message
        .as_object()
        .ok_or_else(|| "message must be a string or a structured object".to_string())?;
    let msg_type = obj
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "structured message requires a `type` field".to_string())?;

    match msg_type {
        "shutdown_request" => {
            let reason = obj.get("reason").and_then(|v| v.as_str());
            let request_id = generate_request_id("shutdown", &input.to);
            let body = json!({
                "type": "shutdown_request",
                "request_id": request_id,
                "from": sender,
                "reason": reason,
            })
            .to_string();
            write_envelope(
                &workspace,
                &input.to,
                &sender,
                &body,
                None,
                mailbox_kinds::SHUTDOWN_REQUEST,
                Some(&request_id),
            )?;
            // Live delivery: mirrors CC-fork's
            // `handleShutdownApproval` → `task.abortController.abort()`
            // path in `src/tools/SendMessageTool/SendMessageTool.ts:357`.
            // A background subagent whose HookAbortSignal is registered
            // here stops on its next `abort_signal.is_aborted()` check
            // in the tool loop — no polling required. Foreground /
            // synchronous subagents already share the parent's abort
            // signal; the registry lookup for those may return the
            // parent's signal, which is safe because the parent is
            // itself the caller.
            let aborted = abort_registered_agent(&input.to);
            to_pretty_json(json!({
                "success": true,
                "message": format!("Shutdown request sent to {}. Request ID: {}", input.to, request_id),
                "request_id": request_id,
                "target": input.to,
                "live_abort_signaled": aborted,
            }))
        }
        "shutdown_response" => {
            if input.to != TEAM_LEAD_NAME {
                return Err(format!("shutdown_response must be sent to \"{TEAM_LEAD_NAME}\""));
            }
            let request_id = obj
                .get("request_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "shutdown_response requires `request_id`".to_string())?;
            let approve = obj.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
            let reason = obj.get("reason").and_then(|v| v.as_str());
            if !approve && reason.map(str::trim).unwrap_or("").is_empty() {
                return Err(
                    "reason is required when rejecting a shutdown request".to_string(),
                );
            }
            let body = json!({
                "type": "shutdown_response",
                "request_id": request_id,
                "from": sender,
                "approve": approve,
                "reason": reason,
            })
            .to_string();
            write_envelope(
                &workspace,
                &input.to,
                &sender,
                &body,
                None,
                mailbox_kinds::SHUTDOWN_RESPONSE,
                Some(request_id),
            )?;
            to_pretty_json(json!({
                "success": true,
                "message": if approve {
                    format!("Shutdown approved. Sent confirmation to {}.", input.to)
                } else {
                    format!("Shutdown rejected. Reason sent to {}.", input.to)
                },
                "request_id": request_id,
            }))
        }
        "plan_approval_response" => {
            let request_id = obj
                .get("request_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "plan_approval_response requires `request_id`".to_string())?;
            let approve = obj.get("approve").and_then(|v| v.as_bool()).unwrap_or(false);
            let feedback = obj.get("feedback").and_then(|v| v.as_str());
            let body = json!({
                "type": "plan_approval_response",
                "request_id": request_id,
                "from": sender,
                "approve": approve,
                "feedback": feedback,
            })
            .to_string();
            write_envelope(
                &workspace,
                &input.to,
                &sender,
                &body,
                None,
                mailbox_kinds::PLAN_APPROVAL_RESPONSE,
                Some(request_id),
            )?;
            to_pretty_json(json!({
                "success": true,
                "message": if approve {
                    format!("Plan approved for {}. They will receive the approval.", input.to)
                } else {
                    format!("Plan rejected for {}.", input.to)
                },
                "request_id": request_id,
            }))
        }
        other => Err(format!(
            "unknown structured message type: {other} (expected one of: shutdown_request, shutdown_response, plan_approval_response)"
        )),
    }
}

// ── Agent abort-signal registry ────────────────────────────────────

/// Process-wide map from `agent_id` to that subagent's
/// [`HookAbortSignal`]. Populated by `spawn_agent_job` when a
/// background subagent starts; consumed by
/// `SendMessage(shutdown_request)` for live abort delivery
/// (mirrors CC-fork's `task.abortController.abort()` path in
/// `SendMessageTool.ts:357`).
///
/// Cleared by `run_spawned_agent_job` when the subagent terminates
/// (success, error, or panic) so a subsequent SendMessage to the same
/// (recycled) name doesn't attempt to abort a defunct signal.
fn global_agent_abort_signals() -> &'static Mutex<HashMap<String, HookAbortSignal>> {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<Mutex<HashMap<String, HookAbortSignal>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a subagent's abort signal so `SendMessage(shutdown_request)`
/// can look it up by `agent_id` and call `.abort()`. Called by
/// `spawn_agent_job` before the runtime starts.
pub fn register_agent_abort_signal(agent_id: &str, signal: HookAbortSignal) {
    if let Ok(mut guard) = global_agent_abort_signals().lock() {
        guard.insert(agent_id.to_string(), signal);
    }
}

/// Remove a subagent from the abort-signal registry — invoked on
/// subagent completion so a stale entry can't abort a future agent
/// that gets the same name.
pub fn unregister_agent_abort_signal(agent_id: &str) {
    if let Ok(mut guard) = global_agent_abort_signals().lock() {
        guard.remove(agent_id);
    }
}

/// Look up `agent_id` in the abort registry and, if present, call
/// `abort()` on its signal. Returns `true` when a signal was found and
/// aborted, `false` when the agent isn't registered (either it
/// finished already or was never registered).
///
/// Matches CC-fork's `findTeammateTaskByAgentId(agentId,
/// appState.tasks); if (task?.abortController) task.abortController.abort()`.
pub fn abort_registered_agent(agent_id: &str) -> bool {
    if let Ok(guard) = global_agent_abort_signals().lock() {
        if let Some(signal) = guard.get(agent_id) {
            signal.abort();
            return true;
        }
    }
    false
}

#[allow(clippy::needless_pass_by_value)]
fn run_lsp(input: LspInput) -> Result<String, String> {
    let registry = global_lsp_registry();
    let action = &input.action;
    let path = input.path.as_deref();
    let line = input.line;
    let character = input.character;
    let query = input.query.as_deref();

    match registry.dispatch(action, path, line, character, query) {
        Ok(result) => to_pretty_json(result),
        Err(e) => to_pretty_json(json!({
            "action": action,
            "error": e,
            "status": "error"
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_list_mcp_resources(input: McpResourceInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let server = input.server.as_deref().unwrap_or("default");
    match registry.list_resources(server) {
        Ok(resources) => {
            let items: Vec<_> = resources
                .iter()
                .map(|r| {
                    json!({
                        "uri": r.uri,
                        "name": r.name,
                        "description": r.description,
                        "mime_type": r.mime_type,
                    })
                })
                .collect();
            to_pretty_json(json!({
                "server": server,
                "resources": items,
                "count": items.len()
            }))
        }
        Err(e) => to_pretty_json(json!({
            "server": server,
            "resources": [],
            "error": e
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_mcp_resource(input: McpResourceInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let uri = input.uri.as_deref().unwrap_or("");
    let server = input.server.as_deref().unwrap_or("default");
    match registry.read_resource(server, uri) {
        Ok(resource) => to_pretty_json(json!({
            "server": server,
            "uri": resource.uri,
            "name": resource.name,
            "description": resource.description,
            "mime_type": resource.mime_type
        })),
        Err(e) => to_pretty_json(json!({
            "server": server,
            "uri": uri,
            "error": e
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_auth(input: McpAuthInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    match registry.get_server(&input.server) {
        Some(state) => to_pretty_json(json!({
            "server": input.server,
            "status": state.status,
            "server_info": state.server_info,
            "tool_count": state.tools.len(),
            "resource_count": state.resources.len()
        })),
        None => to_pretty_json(json!({
            "server": input.server,
            "status": "disconnected",
            "message": "Server not registered. Use MCP tool to connect first."
        })),
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_mcp_tool(input: McpToolInput) -> Result<String, String> {
    let registry = global_mcp_registry();
    let args = input.arguments.unwrap_or(serde_json::json!({}));
    match registry.call_tool(&input.server, &input.tool, &args) {
        Ok(result) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "result": result,
            "status": "success"
        })),
        Err(e) => to_pretty_json(json!({
            "server": input.server,
            "tool": input.tool,
            "error": e,
            "status": "error"
        })),
    }
}

fn from_value<T: for<'de> Deserialize<'de>>(input: &Value) -> Result<T, String> {
    serde_json::from_value(input.clone()).map_err(|error| error.to_string())
}

/// Classify bash command permission based on command type and path.
/// ROADMAP #50: Read-only commands targeting CWD paths get `WorkspaceWrite`,
/// all others remain `DangerFullAccess`.
fn classify_bash_permission(command: &str) -> PermissionMode {
    // Read-only commands that are safe when targeting workspace paths
    const READ_ONLY_COMMANDS: &[&str] = &[
        "cat", "head", "tail", "less", "more", "ls", "ll", "dir", "find", "test", "[", "[[",
        "grep", "rg", "awk", "sed", "file", "stat", "readlink", "wc", "sort", "uniq", "cut", "tr",
        "pwd", "echo", "printf",
    ];

    // Get the base command (first word before any args or pipes)
    let base_cmd = command.split_whitespace().next().unwrap_or("");
    let base_cmd = base_cmd.split('|').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split(';').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split('>').next().unwrap_or("").trim();
    let base_cmd = base_cmd.split('<').next().unwrap_or("").trim();

    // Check if it's a read-only command
    let cmd_name = base_cmd.split('/').next_back().unwrap_or(base_cmd);
    let is_read_only = READ_ONLY_COMMANDS.contains(&cmd_name);

    if !is_read_only {
        return PermissionMode::DangerFullAccess;
    }

    // Check if any path argument is outside workspace
    // Simple heuristic: check for absolute paths not starting with CWD
    if has_dangerous_paths(command) {
        return PermissionMode::DangerFullAccess;
    }

    PermissionMode::WorkspaceWrite
}

/// Check if command has dangerous paths (outside workspace).
fn has_dangerous_paths(command: &str) -> bool {
    // Look for absolute paths
    let tokens: Vec<&str> = command.split_whitespace().collect();

    for token in tokens {
        // Skip flags/options
        if token.starts_with('-') {
            continue;
        }

        // Check for absolute paths
        if token.starts_with('/') || token.starts_with("~/") {
            // Check if it's within CWD
            let path =
                PathBuf::from(token.replace('~', &std::env::var("HOME").unwrap_or_default()));
            if let Ok(cwd) = std::env::current_dir() {
                if !path.starts_with(&cwd) {
                    return true; // Path outside workspace
                }
            }
        }

        // Check for parent directory traversal that escapes workspace
        if token.contains("../..") || token.starts_with("../") && !token.starts_with("./") {
            return true;
        }
    }

    false
}

fn run_bash(
    input: BashCommandInput,
    abort_signal: Option<&HookAbortSignal>,
) -> Result<String, String> {
    if let Some(output) = workspace_test_branch_preflight(&input.command) {
        return serde_json::to_string_pretty(&output).map_err(|error| error.to_string());
    }
    serde_json::to_string_pretty(
        &execute_bash_with_abort(input, abort_signal).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn workspace_test_branch_preflight(command: &str) -> Option<BashCommandOutput> {
    if !is_workspace_test_command(command) {
        return None;
    }

    let branch = git_stdout(&["branch", "--show-current"])?;
    let main_ref = resolve_main_ref(&branch)?;
    let freshness = check_freshness(&branch, &main_ref);
    match freshness {
        BranchFreshness::Fresh => None,
        BranchFreshness::Stale {
            commits_behind,
            missing_fixes,
        } => Some(branch_divergence_output(
            command,
            &branch,
            &main_ref,
            commits_behind,
            None,
            &missing_fixes,
        )),
        BranchFreshness::Diverged {
            ahead,
            behind,
            missing_fixes,
        } => Some(branch_divergence_output(
            command,
            &branch,
            &main_ref,
            behind,
            Some(ahead),
            &missing_fixes,
        )),
    }
}

fn is_workspace_test_command(command: &str) -> bool {
    let normalized = normalize_shell_command(command);
    [
        "cargo test --workspace",
        "cargo test --all",
        "cargo nextest run --workspace",
        "cargo nextest run --all",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn normalize_shell_command(command: &str) -> String {
    command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn resolve_main_ref(branch: &str) -> Option<String> {
    let has_local_main = git_ref_exists("main");
    let has_remote_main = git_ref_exists("origin/main");

    if branch == "main" && has_remote_main {
        Some("origin/main".to_string())
    } else if has_local_main {
        Some("main".to_string())
    } else if has_remote_main {
        Some("origin/main".to_string())
    } else {
        None
    }
}

fn git_ref_exists(reference: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git_stdout(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn branch_divergence_output(
    command: &str,
    branch: &str,
    main_ref: &str,
    commits_behind: usize,
    commits_ahead: Option<usize>,
    missing_fixes: &[String],
) -> BashCommandOutput {
    let relation = commits_ahead.map_or_else(
        || format!("is {commits_behind} commit(s) behind"),
        |ahead| format!("has diverged ({ahead} ahead, {commits_behind} behind)"),
    );
    let missing_summary = if missing_fixes.is_empty() {
        "(none surfaced)".to_string()
    } else {
        missing_fixes.join("; ")
    };
    let stderr = format!(
        "branch divergence detected before workspace tests: `{branch}` {relation} `{main_ref}`. Missing commits: {missing_summary}. Merge or rebase `{main_ref}` before re-running `{command}`."
    );

    BashCommandOutput {
        stdout: String::new(),
        stderr: stderr.clone(),
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: None,
        return_code_interpretation: Some("preflight_blocked:branch_divergence".to_string()),
        no_output_expected: Some(false),
        structured_content: Some(vec![serde_json::to_value(
            LaneEvent::new(
                LaneEventName::BranchStaleAgainstMain,
                LaneEventStatus::Blocked,
                iso8601_now(),
            )
            .with_failure_class(LaneFailureClass::BranchDivergence)
            .with_detail(stderr.clone())
            .with_data(json!({
                "branch": branch,
                "mainRef": main_ref,
                "commitsBehind": commits_behind,
                "commitsAhead": commits_ahead,
                "missingCommits": missing_fixes,
                "blockedCommand": command,
                "recommendedAction": format!("merge or rebase {main_ref} before workspace tests")
            })),
        )
        .expect("lane event should serialize")]),
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    to_pretty_json(
        read_file(&StdFsBackend, &input.path, input.offset, input.limit).map_err(io_to_string)?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_write_file(input: WriteFileInput) -> Result<String, String> {
    // Detect file intent and redirect if draft
    let intent = runtime::detect_file_intent(&input.path, &input.content, None);
    let actual_path = match intent {
        runtime::FileIntent::Draft => {
            let workspace_root = std::env::current_dir().unwrap_or_default();
            runtime::redirect_to_drafts(&std::path::PathBuf::from(&input.path), &workspace_root)
        }
        runtime::FileIntent::Final => std::path::PathBuf::from(&input.path),
    };

    to_pretty_json(
        write_file(
            &StdFsBackend,
            actual_path.to_str().unwrap_or(&input.path),
            &input.content,
        )
        .map_err(io_to_string)?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    // Read existing content for intent detection
    let existing_content = std::fs::read_to_string(&input.path).ok();
    let content_for_detection = existing_content.as_deref().unwrap_or(&input.new_string);

    // Detect file intent
    let intent = runtime::detect_file_intent(&input.path, content_for_detection, None);

    // Draft files should not be edited from workspace root - they're in .drafts/
    // If file is draft and not in .drafts/, redirect
    let workspace_root = std::env::current_dir().unwrap_or_default();
    let actual_path = if intent == runtime::FileIntent::Draft
        && !runtime::is_in_drafts(&std::path::PathBuf::from(&input.path), &workspace_root)
    {
        runtime::redirect_to_drafts(&std::path::PathBuf::from(&input.path), &workspace_root)
    } else {
        std::path::PathBuf::from(&input.path)
    };

    to_pretty_json(
        edit_file(
            &StdFsBackend,
            actual_path.to_str().unwrap_or(&input.path),
            &input.old_string,
            &input.new_string,
            input.replace_all.unwrap_or(false),
        )
        .map_err(io_to_string)?,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    to_pretty_json(glob_search(&input.pattern, input.path.as_deref()).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_grep_search(input: GrepSearchInput) -> Result<String, String> {
    to_pretty_json(grep_search(&input).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_fetch(input: WebFetchInput) -> Result<String, String> {
    // Run on a dedicated OS thread to avoid deadlocking reqwest::blocking inside
    // a tokio async runtime (e.g. ACP mode).
    std::thread::spawn(move || to_pretty_json(execute_web_fetch(&input)?))
        .join()
        .unwrap_or_else(|_| Err("web fetch thread panicked".into()))
}

#[allow(clippy::needless_pass_by_value)]
fn run_web_search(input: WebSearchInput) -> Result<String, String> {
    // Run on a dedicated OS thread to avoid deadlocking reqwest::blocking inside
    // a tokio async runtime (e.g. ACP mode).
    std::thread::spawn(move || to_pretty_json(execute_web_search(&input)?))
        .join()
        .unwrap_or_else(|_| Err("web search thread panicked".into()))
}

fn run_todo_write(input: TodoWriteInput) -> Result<String, String> {
    to_pretty_json(execute_todo_write(input)?)
}

fn run_skill(input: SkillInput) -> Result<String, String> {
    to_pretty_json(execute_skill(input)?)
}

fn run_agent(input: AgentInput, ctx: Option<&ToolDispatchContext>) -> Result<String, String> {
    to_pretty_json(execute_agent(input, ctx)?)
}

fn run_tool_search(input: ToolSearchInput) -> Result<String, String> {
    to_pretty_json(execute_tool_search(input))
}

fn run_notebook_edit(input: NotebookEditInput) -> Result<String, String> {
    to_pretty_json(execute_notebook_edit(input)?)
}

fn run_sleep(input: SleepInput, abort_signal: Option<&HookAbortSignal>) -> Result<String, String> {
    to_pretty_json(execute_sleep(input, abort_signal)?)
}

fn run_brief(input: BriefInput) -> Result<String, String> {
    to_pretty_json(execute_brief(input)?)
}

fn run_config(input: ConfigInput) -> Result<String, String> {
    to_pretty_json(execute_config(input)?)
}

fn run_enter_plan_mode(input: EnterPlanModeInput) -> Result<String, String> {
    to_pretty_json(execute_enter_plan_mode(input)?)
}

fn run_exit_plan_mode(input: ExitPlanModeInput) -> Result<String, String> {
    to_pretty_json(execute_exit_plan_mode(input)?)
}

fn run_structured_output(input: StructuredOutputInput) -> Result<String, String> {
    to_pretty_json(execute_structured_output(input)?)
}

fn run_repl(input: ReplInput, abort_signal: Option<&HookAbortSignal>) -> Result<String, String> {
    to_pretty_json(execute_repl(input, abort_signal)?)
}

/// Classify `PowerShell` command permission based on command type and path.
/// ROADMAP #50: Read-only commands targeting CWD paths get `WorkspaceWrite`,
/// all others remain `DangerFullAccess`.
fn classify_powershell_permission(command: &str) -> PermissionMode {
    // Read-only commands that are safe when targeting workspace paths
    const READ_ONLY_COMMANDS: &[&str] = &[
        "Get-Content",
        "Get-ChildItem",
        "Test-Path",
        "Get-Item",
        "Get-ItemProperty",
        "Get-FileHash",
        "Select-String",
    ];

    // Check if command starts with a read-only cmdlet
    let cmd_lower = command.trim().to_lowercase();
    let is_read_only_cmd = READ_ONLY_COMMANDS
        .iter()
        .any(|cmd| cmd_lower.starts_with(&cmd.to_lowercase()));

    if !is_read_only_cmd {
        return PermissionMode::DangerFullAccess;
    }

    // Check if the path is within workspace (CWD or subdirectory)
    // Extract path from command - look for -Path or positional parameter
    let path = extract_powershell_path(command);
    match path {
        Some(p) if is_within_workspace(&p) => PermissionMode::WorkspaceWrite,
        _ => PermissionMode::DangerFullAccess,
    }
}

/// Extract the path argument from a `PowerShell` command.
fn extract_powershell_path(command: &str) -> Option<String> {
    // Look for -Path parameter
    if let Some(idx) = command.to_lowercase().find("-path") {
        let after_path = &command[idx + 5..];
        let path = after_path.split_whitespace().next()?;
        return Some(path.trim_matches('"').trim_matches('\'').to_string());
    }

    // Look for positional path parameter (after command name)
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.len() >= 2 {
        // Skip the cmdlet name and take the first argument
        let first_arg = parts[1];
        // Check if it looks like a path (contains \, /, or .)
        if first_arg.contains(['\\', '/', '.']) {
            return Some(first_arg.trim_matches('"').trim_matches('\'').to_string());
        }
    }

    None
}

/// Check if a path is within the current workspace.
fn is_within_workspace(path: &str) -> bool {
    let path = PathBuf::from(path);

    // If path is absolute, check if it starts with CWD
    if path.is_absolute() {
        if let Ok(cwd) = std::env::current_dir() {
            return path.starts_with(&cwd);
        }
    }

    // Relative paths are assumed to be within workspace
    !path.starts_with("/") && !path.starts_with("\\") && !path.starts_with("..")
}

fn run_powershell(
    input: PowerShellInput,
    abort_signal: Option<&HookAbortSignal>,
) -> Result<String, String> {
    to_pretty_json(execute_powershell(input, abort_signal).map_err(|error| error.to_string())?)
}

fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn io_to_string(error: std::io::Error) -> String {
    error.to_string()
}

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GlobSearchInputValue {
    pattern: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    allowed_domains: Option<Vec<String>>,
    blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
struct TodoItem {
    content: String,
    #[serde(rename = "activeForm")]
    active_form: String,
    status: TodoStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    skill: String,
    args: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AgentInput {
    description: String,
    prompt: String,
    subagent_type: Option<String>,
    name: Option<String>,
    model: Option<String>,
    #[serde(default)]
    run_in_background: Option<bool>,
    /// Explicit auth mode: `"api-key"`, `"proxy"`, or `"subscription"`.
    /// When set, overrides the config's auto-detect priority.
    auth_mode: Option<String>,
    /// Permission escalation mode for the sub-agent's tool calls.
    ///
    /// Currently the only recognized value is `"bubble"`, which
    /// documents (and MUST match) the existing default behavior:
    /// whenever the sub-agent hits a permission-gated tool, the
    /// resulting prompt bubbles up to the parent process's
    /// `PermissionPrompter` (i.e., the human at the terminal or the
    /// ACP/WebUI client driving the parent), NOT to the sub-agent's
    /// own inner prompter. Mirrors CC-fork's
    /// `AgentInput.permission_mode = 'bubble'`. Any other value is
    /// silently ignored — reserved for future modes.
    ///
    /// Accepted at the schema layer for CC-fork parity but not read
    /// at runtime because bubble is the ONLY behavior sudocode
    /// currently offers. The `#[allow(dead_code)]` documents that
    /// this is intentional, not a forgotten wire.
    #[allow(dead_code)]
    permission_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct NotebookEditInput {
    notebook_path: String,
    cell_id: Option<String>,
    new_source: Option<String>,
    cell_type: Option<NotebookCellType>,
    edit_mode: Option<NotebookEditMode>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookCellType {
    Code,
    Markdown,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookEditMode {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Deserialize)]
struct SleepInput {
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct BriefInput {
    message: String,
    attachments: Option<Vec<String>>,
    status: BriefStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BriefStatus {
    Normal,
    Proactive,
}

#[derive(Debug, Deserialize)]
struct ConfigInput {
    setting: String,
    value: Option<ConfigValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EnterPlanModeInput {}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExitPlanModeInput {}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfigValue {
    String(String),
    Bool(bool),
    Number(f64),
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct StructuredOutputInput(BTreeMap<String, Value>);

#[derive(Debug, Deserialize)]
struct ReplInput {
    code: String,
    language: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PowerShellInput {
    command: String,
    timeout: Option<u64>,
    description: Option<String>,
    run_in_background: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionInput {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    options: Option<Vec<String>>,
    #[serde(default)]
    questions: Vec<AskUserQuestionItem>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionItem {
    id: String,
    prompt: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    required: Option<bool>,
    #[serde(default)]
    allow_custom_input: Option<bool>,
    #[serde(default)]
    custom_input_hint: Option<String>,
    #[serde(default)]
    options: Vec<AskUserQuestionOption>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct AskUserQuestionOption {
    label: String,
    value: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    recommended: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionAnswer {
    id: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionResult {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    questions: Vec<AskUserQuestionItem>,
    answers: Vec<AskUserQuestionAnswer>,
}

fn normalize_ask_user_question_input(
    input: AskUserQuestionInput,
) -> Result<(Option<String>, Option<String>, Vec<AskUserQuestionItem>), String> {
    if !input.questions.is_empty() {
        return Ok((input.title, input.description, input.questions));
    }

    let question = input
        .question
        .map(|question| question.trim().to_string())
        .filter(|question| !question.is_empty())
        .ok_or_else(|| "question or questions is required".to_string())?;

    let options = input
        .options
        .unwrap_or_default()
        .into_iter()
        .filter(|option| !option.trim().is_empty())
        .map(|option| AskUserQuestionOption {
            label: option.clone(),
            value: option,
            description: None,
            recommended: None,
        })
        .collect::<Vec<_>>();

    Ok((
        input.title,
        input.description,
        vec![AskUserQuestionItem {
            id: "q1".to_string(),
            prompt: question,
            kind: Some(if options.is_empty() {
                "text".to_string()
            } else {
                "single_select".to_string()
            }),
            required: Some(true),
            allow_custom_input: Some(options.is_empty()),
            custom_input_hint: None,
            options,
        }],
    ))
}

#[derive(Debug, Deserialize)]
struct TaskCreateInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskIdInput {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskUpdateInput {
    task_id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct TaskOutputInput {
    task_id: Option<String>,
    agent_id: Option<String>,
    #[serde(default = "default_block_true")]
    block: bool,
    #[serde(default = "default_agent_await_timeout_ms")]
    timeout_ms: u64,
}

const fn default_block_true() -> bool {
    true
}

const fn default_agent_await_timeout_ms() -> u64 {
    DEFAULT_AGENT_AWAIT_TIMEOUT_MS
}

#[derive(Debug, Deserialize)]
struct CronCreateInput {
    schedule: String,
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CronDeleteInput {
    cron_id: String,
}

// SendMessage input. `message` is a JSON `Value` because it accepts
// either a plain string or a structured object — validated inside
// `run_send_message`, not by serde, so we can produce the
// fork-compatible error messages verbatim.
#[derive(Debug, Deserialize)]
struct SendMessageInput {
    to: String,
    message: Value,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    sender: Option<String>,
}

/// Default sender name used when the caller doesn't supply one.
/// Matches `sudoprivacy/claude-code`'s `TEAM_LEAD_NAME` for the main
/// agent path.
const TEAM_LEAD_NAME: &str = "team-lead";

#[derive(Debug, Deserialize)]
struct LspInput {
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    character: Option<u32>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpResourceInput {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpAuthInput {
    server: String,
}

#[derive(Debug, Deserialize)]
struct McpToolInput {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Serialize)]
struct WebFetchOutput {
    bytes: usize,
    code: u16,
    #[serde(rename = "codeText")]
    code_text: String,
    result: String,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
    url: String,
}

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    query: String,
    results: Vec<WebSearchResultItem>,
    #[serde(rename = "durationSeconds")]
    duration_seconds: f64,
}

#[derive(Debug, Serialize)]
struct TodoWriteOutput {
    #[serde(rename = "oldTodos")]
    old_todos: Vec<TodoItem>,
    #[serde(rename = "newTodos")]
    new_todos: Vec<TodoItem>,
    #[serde(rename = "verificationNudgeNeeded")]
    verification_nudge_needed: Option<bool>,
    /// `<system-reminder>` block injected exactly once when the
    /// `runtime::verification_watcher` streak counter crosses the
    /// threshold (default 3). Consumed inline — subsequent
    /// TodoWrite calls do NOT repeat the nudge until the streak
    /// resets (either by another 3-completion streak or by
    /// dispatching `Agent(subagent_type="Verification")`).
    #[serde(
        rename = "verificationStreakNudge",
        skip_serializing_if = "Option::is_none"
    )]
    verification_streak_nudge: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct SkillOutput {
    skill: String,
    path: String,
    args: Option<String>,
    description: Option<String>,
    prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentOutput {
    #[serde(rename = "agentId")]
    agent_id: String,
    name: String,
    description: String,
    #[serde(rename = "subagentType")]
    subagent_type: Option<String>,
    model: Option<String>,
    status: String,
    #[serde(rename = "outputFile")]
    output_file: String,
    #[serde(rename = "manifestFile")]
    manifest_file: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(rename = "laneEvents", default, skip_serializing_if = "Vec::is_empty")]
    lane_events: Vec<LaneEvent>,
    #[serde(rename = "currentBlocker", skip_serializing_if = "Option::is_none")]
    current_blocker: Option<LaneEventBlocker>,
    #[serde(rename = "derivedState")]
    derived_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    /// Path to the `.full.md` sibling file containing the FULL
    /// unabridged assistant output. Populated only when the result
    /// exceeded `SUDOCODE_AGENT_SUMMARY_THRESHOLD_CHARS` and the
    /// summarizer replaced the parent-visible `result` with a
    /// condensed version. When absent, `result` IS the full text
    /// (no summarization happened).
    #[serde(
        rename = "resultFullPath",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    result_full_path: Option<String>,
    /// Palette color assigned to this agent — one of
    /// `runtime::agent_color::AGENT_COLOR_PALETTE`. Populated in
    /// `prepare_agent_job` so every spawned sub-agent has a
    /// distinguishable label. Rendered as `<color>{name}</color>` in
    /// the task-notification XML when coordinator mode is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    /// Total tokens the sub-agent consumed across its entire run
    /// (input + output + cache-creation + cache-read, summed over
    /// every multi-turn iteration). Populated by
    /// `run_agent_job_returning_text` when the agent completes and
    /// surfaced in the task-notification `<usage>` block.
    #[serde(
        rename = "totalTokens",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    total_tokens: Option<u64>,
    /// Count of tool_use blocks the sub-agent's assistant messages
    /// emitted, summed across every multi-turn iteration.
    #[serde(rename = "toolUses", default, skip_serializing_if = "Option::is_none")]
    tool_uses: Option<u64>,
    /// Per-agent idempotency flag for the coordinator-mode push
    /// notification (see
    /// [`runtime::coordinator_notification::emit`]). `Some(true)`
    /// means the terminal-state emit has already fired for this
    /// agent — subsequent persist calls MUST NOT emit again.
    ///
    /// Mirrors CC-fork's `LocalAgentTaskState.notified` atomic
    /// check-and-set at `src/tasks/LocalAgentTask/LocalAgentTask.tsx:228-237`.
    ///
    /// Persisted on disk in the same manifest write that emits the
    /// envelope, so a crash between "manifest written" and "envelope
    /// appended" biases toward NOT re-emitting on restart (CC's
    /// same trade-off: prefer under-notify to double-notify — the
    /// coordinator can always poll `TaskOutput` if it missed one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notified: Option<bool>,
}

/// Telemetry captured from a sub-agent's completed run and folded
/// into the manifest before terminal-state persistence. Mirrors the
/// `<usage>` sub-tags the coordinator prompt teaches the model.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgentRunTelemetry {
    pub total_tokens: u64,
    pub tool_uses: u64,
}

#[derive(Debug, Clone)]
struct AgentJob {
    manifest: AgentOutput,
    prompt: String,
    system_prompt: SystemPrompt,
    allowed_tools: BTreeSet<String>,
    /// Captured at spawn time so the subagent thread uses the same provider
    /// config as the parent, regardless of CWD changes.
    sudocode_config: SudoCodeConfig,
    fallback_config: ProviderFallbackConfig,
    /// Auth mode detected from env vars at spawn time so the subagent uses the
    /// same credential path (api-key / proxy / subscription) as the parent.
    auth_mode: Option<api::AuthMode>,
    /// Pre-seeded conversation prefix. Threaded into the child's `Session`
    /// via [`Session::with_messages`] before the first API call. Empty for
    /// every non-fork spawn; populated by the fork rebuild in a follow-up
    /// commit with the parent's assistant message + placeholder
    /// tool_results (mirroring CC-fork's `buildForkedMessages`).
    inherited_messages: Vec<ConversationMessage>,
    /// Signal wired into the subagent's `ConversationRuntime` via
    /// `with_hook_abort_signal`. Registered by name in
    /// [`global_agent_abort_signals`] so
    /// `SendMessage(shutdown_request)` can look it up and call
    /// `.abort()` for live delivery — mirrors CC-fork's
    /// `task.abortController.abort()`.
    abort_signal: HookAbortSignal,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolSearchOutput {
    matches: Vec<String>,
    query: String,
    normalized_query: String,
    #[serde(rename = "total_deferred_tools")]
    total_deferred_tools: usize,
    #[serde(rename = "pending_mcp_servers")]
    pending_mcp_servers: Option<Vec<String>>,
    #[serde(rename = "mcp_degraded", skip_serializing_if = "Option::is_none")]
    mcp_degraded: Option<McpDegradedReport>,
}

#[derive(Debug, Serialize)]
struct NotebookEditOutput {
    new_source: String,
    cell_id: Option<String>,
    cell_type: Option<NotebookCellType>,
    language: String,
    edit_mode: String,
    error: Option<String>,
    notebook_path: String,
    original_file: String,
    updated_file: String,
}

#[derive(Debug, Serialize)]
struct SleepOutput {
    duration_ms: u64,
    message: String,
}

#[derive(Debug, Serialize)]
struct BriefOutput {
    message: String,
    attachments: Option<Vec<ResolvedAttachment>>,
    #[serde(rename = "sentAt")]
    sent_at: String,
}

#[derive(Debug, Serialize)]
struct ResolvedAttachment {
    path: String,
    size: u64,
    #[serde(rename = "isImage")]
    is_image: bool,
}

#[derive(Debug, Serialize)]
struct ConfigOutput {
    success: bool,
    operation: Option<String>,
    setting: Option<String>,
    value: Option<Value>,
    #[serde(rename = "previousValue")]
    previous_value: Option<Value>,
    #[serde(rename = "newValue")]
    new_value: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanModeState {
    #[serde(rename = "hadLocalOverride")]
    had_local_override: bool,
    #[serde(rename = "previousLocalMode")]
    previous_local_mode: Option<Value>,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct PlanModeOutput {
    success: bool,
    operation: String,
    changed: bool,
    active: bool,
    managed: bool,
    message: String,
    #[serde(rename = "settingsPath")]
    settings_path: String,
    #[serde(rename = "statePath")]
    state_path: String,
    #[serde(rename = "previousLocalMode")]
    previous_local_mode: Option<Value>,
    #[serde(rename = "currentLocalMode")]
    current_local_mode: Option<Value>,
}

#[derive(Debug, Clone)]
struct SearchableToolSpec {
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct StructuredOutputResult {
    data: String,
    structured_output: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ReplOutput {
    language: String,
    stdout: String,
    stderr: String,
    #[serde(rename = "exitCode")]
    exit_code: i32,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WebSearchResultItem {
    SearchResult {
        tool_use_id: String,
        content: Vec<SearchHit>,
    },
    Commentary(String),
}

#[derive(Debug, Serialize)]
struct SearchHit {
    title: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TavilySearchResponse {
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
    #[allow(dead_code)]
    score: f64,
}

fn execute_web_fetch(input: &WebFetchInput) -> Result<WebFetchOutput, String> {
    let started = Instant::now();
    let client = build_http_client()?;
    let request_url = normalize_fetch_url(&input.url)?;
    let response = client
        .get(request_url.clone())
        .send()
        .map_err(|error| error.to_string())?;

    let status = response.status();
    let final_url = response.url().to_string();
    let code = status.as_u16();
    let code_text = status.canonical_reason().unwrap_or("Unknown").to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().map_err(|error| error.to_string())?;
    let bytes = body.len();
    let normalized = normalize_fetched_content(&body, &content_type);
    let result = summarize_web_fetch(&final_url, &input.prompt, &normalized, &body, &content_type);

    Ok(WebFetchOutput {
        bytes,
        code,
        code_text,
        result,
        duration_ms: started.elapsed().as_millis(),
        url: final_url,
    })
}

fn execute_web_search(input: &WebSearchInput) -> Result<WebSearchOutput, String> {
    let started = Instant::now();
    let config = load_sudocode_config();
    let ws = &config.web_search;

    let provider =
        std::env::var("SUDOCODE_WEB_SEARCH_PROVIDER").unwrap_or_else(|_| ws.provider.clone());

    let mut hits = match provider.as_str() {
        "tavily" => {
            let api_key = std::env::var("SUDOCODE_TAVILY_API_KEY")
                .ok()
                .filter(|k| !k.is_empty())
                .unwrap_or_else(|| {
                    if ws.api_key.is_empty() {
                        config
                            .auth_modes
                            .get("proxy")
                            .and_then(|m| m.get("sudorouter"))
                            .and_then(|c| c.api_key.clone())
                            .unwrap_or_default()
                    } else {
                        ws.api_key.clone()
                    }
                });
            if api_key.is_empty() || api_key.starts_with('<') {
                return Err(
                    "Tavily search requires apiKey in web_search or proxy.sudorouter".into(),
                );
            }
            let api_url =
                std::env::var("SUDOCODE_TAVILY_API_URL").unwrap_or_else(|_| ws.api_url.clone());
            execute_tavily_search(input, &api_url, &api_key)?
        }
        _ => execute_duckduckgo_search(input)?,
    };

    if let Some(allowed) = input.allowed_domains.as_ref() {
        hits.retain(|hit| host_matches_list(&hit.url, allowed));
    }
    if let Some(blocked) = input.blocked_domains.as_ref() {
        hits.retain(|hit| !host_matches_list(&hit.url, blocked));
    }

    dedupe_hits(&mut hits);
    hits.truncate(8);

    let summary = if hits.is_empty() {
        format!("No web search results matched the query {:?}.", input.query)
    } else {
        let rendered_hits = hits
            .iter()
            .map(|hit| match &hit.snippet {
                Some(s) => format!("- [{}]({}): {}", hit.title, hit.url, s),
                None => format!("- [{}]({})", hit.title, hit.url),
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Search results for {:?}. Include a Sources section in the final answer.\n{}",
            input.query, rendered_hits
        )
    };

    Ok(WebSearchOutput {
        query: input.query.clone(),
        results: vec![
            WebSearchResultItem::Commentary(summary),
            WebSearchResultItem::SearchResult {
                tool_use_id: String::from("web_search_1"),
                content: hits,
            },
        ],
        duration_seconds: started.elapsed().as_secs_f64(),
    })
}

fn build_http_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent("sudocode-rust-tools/0.1")
        .build()
        .map_err(|error| error.to_string())
}

fn normalize_fetch_url(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    if parsed.scheme() == "http" {
        let host = parsed.host_str().unwrap_or_default();
        if host != "localhost" && host != "127.0.0.1" && host != "::1" {
            let mut upgraded = parsed;
            upgraded
                .set_scheme("https")
                .map_err(|()| String::from("failed to upgrade URL to https"))?;
            return Ok(upgraded.to_string());
        }
    }
    Ok(parsed.to_string())
}

fn build_search_url(query: &str) -> Result<reqwest::Url, String> {
    if let Ok(base) = std::env::var("SUDOCODE_WEB_SEARCH_BASE_URL") {
        let mut url = reqwest::Url::parse(&base).map_err(|error| error.to_string())?;
        url.query_pairs_mut().append_pair("q", query);
        return Ok(url);
    }

    let mut url = reqwest::Url::parse("https://html.duckduckgo.com/html/")
        .map_err(|error| error.to_string())?;
    url.query_pairs_mut().append_pair("q", query);
    Ok(url)
}

fn execute_duckduckgo_search(input: &WebSearchInput) -> Result<Vec<SearchHit>, String> {
    let client = build_http_client()?;
    let search_url = build_search_url(&input.query)?;
    let response = client
        .get(search_url)
        .send()
        .map_err(|error| error.to_string())?;

    let final_url = response.url().clone();
    let html = response.text().map_err(|error| error.to_string())?;
    let mut hits = extract_search_hits(&html);

    if hits.is_empty() && final_url.host_str().is_some() {
        hits = extract_search_hits_from_generic_links(&html);
    }

    Ok(hits)
}

fn execute_tavily_search(
    input: &WebSearchInput,
    api_url: &str,
    api_key: &str,
) -> Result<Vec<SearchHit>, String> {
    let client = build_http_client()?;

    let mut body = serde_json::json!({
        "query": input.query,
        "search_depth": "basic",
        "max_results": 8,
    });
    if let Some(ref allowed) = input.allowed_domains {
        body["include_domains"] = serde_json::json!(allowed);
    }
    if let Some(ref blocked) = input.blocked_domains {
        body["exclude_domains"] = serde_json::json!(blocked);
    }

    let response = client
        .post(api_url)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .map_err(|e| format!("Tavily request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        return Err(format!("Tavily API returned {status}: {text}"));
    }

    let tavily_resp: TavilySearchResponse = response
        .json()
        .map_err(|e| format!("Failed to parse Tavily response: {e}"))?;

    Ok(tavily_resp
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            url: r.url,
            snippet: Some(r.content),
        })
        .collect())
}

fn normalize_fetched_content(body: &str, content_type: &str) -> String {
    if content_type.contains("html") {
        html_to_text(body)
    } else {
        body.trim().to_string()
    }
}

fn summarize_web_fetch(
    url: &str,
    prompt: &str,
    content: &str,
    raw_body: &str,
    content_type: &str,
) -> String {
    let lower_prompt = prompt.to_lowercase();
    let compact = collapse_whitespace(content);

    let detail = if lower_prompt.contains("title") {
        extract_title(content, raw_body, content_type).map_or_else(
            || preview_text(&compact, 600),
            |title| format!("Title: {title}"),
        )
    } else if lower_prompt.contains("summary") || lower_prompt.contains("summarize") {
        preview_text(&compact, 900)
    } else {
        let preview = preview_text(&compact, 900);
        format!("Prompt: {prompt}\nContent preview:\n{preview}")
    };

    format!("Fetched {url}\n{detail}")
}

fn extract_title(content: &str, raw_body: &str, content_type: &str) -> Option<String> {
    if content_type.contains("html") {
        let lowered = raw_body.to_lowercase();
        if let Some(start) = lowered.find("<title>") {
            let after = start + "<title>".len();
            if let Some(end_rel) = lowered[after..].find("</title>") {
                let title =
                    collapse_whitespace(&decode_html_entities(&raw_body[after..after + end_rel]));
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut previous_was_space = false;

    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            '&' => {
                text.push('&');
                previous_was_space = false;
            }
            ch if ch.is_whitespace() => {
                if !previous_was_space {
                    text.push(' ');
                    previous_was_space = true;
                }
            }
            _ => {
                text.push(ch);
                previous_was_space = false;
            }
        }
    }

    collapse_whitespace(&decode_html_entities(&text))
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn preview_text(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let shortened = input.chars().take(max_chars).collect::<String>();
    format!("{}…", shortened.trim_end())
}

fn extract_search_hits(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut remaining = html;

    while let Some(anchor_start) = remaining.find("result__a") {
        let after_class = &remaining[anchor_start..];
        let Some(href_idx) = after_class.find("href=") else {
            remaining = &after_class[1..];
            continue;
        };
        let href_slice = &after_class[href_idx + 5..];
        let Some((url, rest)) = extract_quoted_value(href_slice) else {
            remaining = &after_class[1..];
            continue;
        };
        let Some(close_tag_idx) = rest.find('>') else {
            remaining = &after_class[1..];
            continue;
        };
        let after_tag = &rest[close_tag_idx + 1..];
        let Some(end_anchor_idx) = after_tag.find("</a>") else {
            remaining = &after_tag[1..];
            continue;
        };
        let title = html_to_text(&after_tag[..end_anchor_idx]);
        if let Some(decoded_url) = decode_duckduckgo_redirect(&url) {
            hits.push(SearchHit {
                title: title.trim().to_string(),
                url: decoded_url,
                snippet: None,
            });
        }
        remaining = &after_tag[end_anchor_idx + 4..];
    }

    hits
}

fn extract_search_hits_from_generic_links(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut remaining = html;

    while let Some(anchor_start) = remaining.find("<a") {
        let after_anchor = &remaining[anchor_start..];
        let Some(href_idx) = after_anchor.find("href=") else {
            remaining = &after_anchor[2..];
            continue;
        };
        let href_slice = &after_anchor[href_idx + 5..];
        let Some((url, rest)) = extract_quoted_value(href_slice) else {
            remaining = &after_anchor[2..];
            continue;
        };
        let Some(close_tag_idx) = rest.find('>') else {
            remaining = &after_anchor[2..];
            continue;
        };
        let after_tag = &rest[close_tag_idx + 1..];
        let Some(end_anchor_idx) = after_tag.find("</a>") else {
            remaining = &after_anchor[2..];
            continue;
        };
        let title = html_to_text(&after_tag[..end_anchor_idx]);
        if title.trim().is_empty() {
            remaining = &after_tag[end_anchor_idx + 4..];
            continue;
        }
        let decoded_url = decode_duckduckgo_redirect(&url).unwrap_or(url);
        if decoded_url.starts_with("http://") || decoded_url.starts_with("https://") {
            hits.push(SearchHit {
                title: title.trim().to_string(),
                url: decoded_url,
                snippet: None,
            });
        }
        remaining = &after_tag[end_anchor_idx + 4..];
    }

    hits
}

fn extract_quoted_value(input: &str) -> Option<(String, &str)> {
    let quote = input.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &input[quote.len_utf8()..];
    let end = rest.find(quote)?;
    Some((rest[..end].to_string(), &rest[end + quote.len_utf8()..]))
}

fn decode_duckduckgo_redirect(url: &str) -> Option<String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(html_entity_decode_url(url));
    }

    let joined = if url.starts_with("//") {
        format!("https:{url}")
    } else if url.starts_with('/') {
        format!("https://duckduckgo.com{url}")
    } else {
        return None;
    };

    let parsed = reqwest::Url::parse(&joined).ok()?;
    if parsed.path() == "/l/" || parsed.path() == "/l" {
        for (key, value) in parsed.query_pairs() {
            if key == "uddg" {
                return Some(html_entity_decode_url(value.as_ref()));
            }
        }
    }
    Some(joined)
}

fn html_entity_decode_url(url: &str) -> String {
    decode_html_entities(url)
}

fn host_matches_list(url: &str, domains: &[String]) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    domains.iter().any(|domain| {
        let normalized = normalize_domain_filter(domain);
        !normalized.is_empty() && (host == normalized || host.ends_with(&format!(".{normalized}")))
    })
}

fn normalize_domain_filter(domain: &str) -> String {
    let trimmed = domain.trim();
    let candidate = reqwest::Url::parse(trimmed)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| trimmed.to_string());
    candidate
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn dedupe_hits(hits: &mut Vec<SearchHit>) {
    let mut seen = BTreeSet::new();
    hits.retain(|hit| seen.insert(hit.url.clone()));
}

fn execute_todo_write(input: TodoWriteInput) -> Result<TodoWriteOutput, String> {
    validate_todos(&input.todos)?;
    let store_path = todo_store_path()?;
    let old_todos = if store_path.exists() {
        serde_json::from_str::<Vec<TodoItem>>(
            &std::fs::read_to_string(&store_path).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };

    let all_done = input
        .todos
        .iter()
        .all(|todo| matches!(todo.status, TodoStatus::Completed));
    let persisted = if all_done {
        Vec::new()
    } else {
        input.todos.clone()
    };

    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&persisted).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    let verification_nudge_needed = (all_done
        && input.todos.len() >= 3
        && !input
            .todos
            .iter()
            .any(|todo| todo.content.to_lowercase().contains("verif")))
    .then_some(true);

    // Streak counter: each newly-completed todo (by content string)
    // bumps the process-global watcher exactly once via
    // `record_completion_by_id` — the dedupe set inside
    // `runtime::verification_watcher` shields us from sudocode's
    // "TodoWrite clears the on-disk store when all todos are
    // completed" behavior, which would otherwise over-count a
    // re-persisted batch. Once the streak crosses the threshold
    // (default 3), the model gets a one-shot `<system-reminder>`
    // nudging it to spawn a Verification sub-agent. Reset happens
    // either automatically on the next Verification spawn
    // (`prepare_agent_job`) or via `should_nudge_and_consume`'s
    // check-and-reset semantics.
    for todo in &input.todos {
        if matches!(todo.status, TodoStatus::Completed) {
            runtime::verification_watcher::record_completion_by_id(&todo.content);
        }
    }
    let verification_streak_nudge = runtime::verification_watcher::should_nudge_and_consume();

    Ok(TodoWriteOutput {
        old_todos,
        new_todos: input.todos,
        verification_nudge_needed,
        verification_streak_nudge,
    })
}

fn execute_skill(input: SkillInput) -> Result<SkillOutput, String> {
    let skill_path = resolve_skill_path(&input.skill)?;
    let prompt = std::fs::read_to_string(&skill_path).map_err(|error| error.to_string())?;
    let description = parse_skill_description(&prompt);

    Ok(SkillOutput {
        skill: input.skill,
        path: skill_path.display().to_string(),
        args: input.args,
        description,
        prompt,
    })
}

fn validate_todos(todos: &[TodoItem]) -> Result<(), String> {
    if todos.is_empty() {
        return Err(String::from("todos must not be empty"));
    }
    // Allow multiple in_progress items for parallel workflows
    if todos.iter().any(|todo| todo.content.trim().is_empty()) {
        return Err(String::from("todo content must not be empty"));
    }
    if todos.iter().any(|todo| todo.active_form.trim().is_empty()) {
        return Err(String::from("todo activeForm must not be empty"));
    }
    Ok(())
}

fn todo_store_path() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("SUDOCODE_TODO_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(cwd.join(".sudocode-todos.json"))
}

fn resolve_skill_path(skill: &str) -> Result<std::path::PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let plugin_load_outcome = load_plugin_outcome_for_cwd(&cwd);
    match commands::resolve_skill_path_with_plugins(&cwd, skill, plugin_load_outcome.as_ref()) {
        Ok(path) => Ok(path),
        Err(_) => resolve_skill_path_from_compat_roots(skill),
    }
}

fn load_plugin_outcome_for_cwd(cwd: &Path) -> Option<PluginLoadOutcome> {
    let loader = ConfigLoader::default_for(cwd);
    let runtime_config = loader.load().ok()?;
    let plugin_config = runtime_config
        .plugins()
        .to_plugin_manager_config(cwd, loader.config_home());

    PluginManager::new(plugin_config)
        .plugin_registry_report()
        .ok()
        .map(|report| report.load_outcome())
}

fn resolve_skill_path_from_compat_roots(skill: &str) -> Result<std::path::PathBuf, String> {
    let requested = skill.trim().trim_start_matches('/').trim_start_matches('$');
    if requested.is_empty() {
        return Err(String::from("skill must not be empty"));
    }

    for root in skill_lookup_roots() {
        if let Some(path) = resolve_skill_path_in_root(&root, requested) {
            return Ok(path);
        }
    }

    Err(format!("unknown skill: {requested}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillLookupOrigin {
    SkillsDir,
    LegacyCommandsDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillLookupRoot {
    path: std::path::PathBuf,
    origin: SkillLookupOrigin,
}

fn skill_lookup_roots() -> Vec<SkillLookupRoot> {
    let mut roots = Vec::new();

    if let Ok(cwd) = std::env::current_dir() {
        push_project_skill_lookup_roots(&mut roots, &cwd);
    }

    if let Ok(sudocode_config_home) = std::env::var("SUDO_CODE_CONFIG_HOME") {
        push_prefixed_skill_lookup_roots(&mut roots, std::path::Path::new(&sudocode_config_home));
    }
    if let Ok(codex_home) = std::env::var("CODEX_HOME") {
        push_prefixed_skill_lookup_roots(&mut roots, std::path::Path::new(&codex_home));
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        push_home_skill_lookup_roots(&mut roots, std::path::Path::new(&home));
    }
    if let Ok(claude_config_dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        let claude_config_dir = std::path::PathBuf::from(claude_config_dir);
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("skills"),
            SkillLookupOrigin::SkillsDir,
        );
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("skills").join("omc-learned"),
            SkillLookupOrigin::SkillsDir,
        );
        push_skill_lookup_root(
            &mut roots,
            claude_config_dir.join("commands"),
            SkillLookupOrigin::LegacyCommandsDir,
        );
    }
    push_skill_lookup_root(
        &mut roots,
        std::path::PathBuf::from("/home/bellman/.nexus/sudocode/skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        &mut roots,
        std::path::PathBuf::from("/home/bellman/.codex/skills"),
        SkillLookupOrigin::SkillsDir,
    );

    roots
}

fn push_project_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, cwd: &std::path::Path) {
    for ancestor in cwd.ancestors() {
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".omc"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".agents"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".nexus").join("sudocode"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".codex"));
        push_prefixed_skill_lookup_roots(roots, &ancestor.join(".claude"));
    }
}

fn push_home_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, home: &std::path::Path) {
    push_prefixed_skill_lookup_roots(roots, &home.join(".omc"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".nexus").join("sudocode"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".codex"));
    push_prefixed_skill_lookup_roots(roots, &home.join(".claude"));
    push_skill_lookup_root(
        roots,
        home.join(".agents").join("skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        roots,
        home.join(".config").join("opencode").join("skills"),
        SkillLookupOrigin::SkillsDir,
    );
    push_skill_lookup_root(
        roots,
        home.join(".claude").join("skills").join("omc-learned"),
        SkillLookupOrigin::SkillsDir,
    );
}

fn push_prefixed_skill_lookup_roots(roots: &mut Vec<SkillLookupRoot>, prefix: &std::path::Path) {
    push_skill_lookup_root(roots, prefix.join("skills"), SkillLookupOrigin::SkillsDir);
    push_skill_lookup_root(
        roots,
        prefix.join("commands"),
        SkillLookupOrigin::LegacyCommandsDir,
    );
}

fn push_skill_lookup_root(
    roots: &mut Vec<SkillLookupRoot>,
    path: std::path::PathBuf,
    origin: SkillLookupOrigin,
) {
    if path.is_dir() && !roots.iter().any(|existing| existing.path == path) {
        roots.push(SkillLookupRoot { path, origin });
    }
}

fn resolve_skill_path_in_root(
    root: &SkillLookupRoot,
    requested: &str,
) -> Option<std::path::PathBuf> {
    match root.origin {
        SkillLookupOrigin::SkillsDir => resolve_skill_path_in_skills_dir(&root.path, requested),
        SkillLookupOrigin::LegacyCommandsDir => {
            resolve_skill_path_in_legacy_commands_dir(&root.path, requested)
        }
    }
}

fn resolve_skill_path_in_skills_dir(
    root: &std::path::Path,
    requested: &str,
) -> Option<std::path::PathBuf> {
    let direct = root.join(requested).join("SKILL.md");
    if direct.is_file() {
        return Some(direct);
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let skill_path = entry.path().join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(requested)
            || skill_frontmatter_name_matches(&skill_path, requested)
        {
            return Some(skill_path);
        }
    }

    None
}

fn resolve_skill_path_in_legacy_commands_dir(
    root: &std::path::Path,
    requested: &str,
) -> Option<std::path::PathBuf> {
    let direct_dir = root.join(requested).join("SKILL.md");
    if direct_dir.is_file() {
        return Some(direct_dir);
    }

    let direct_markdown = root.join(format!("{requested}.md"));
    if direct_markdown.is_file() {
        return Some(direct_markdown);
    }

    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let candidate_path = if path.is_dir() {
            let skill_path = path.join("SKILL.md");
            if !skill_path.is_file() {
                continue;
            }
            skill_path
        } else if path
            .extension()
            .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
        {
            path
        } else {
            continue;
        };

        let matches_entry_name = candidate_path
            .file_stem()
            .is_some_and(|stem| stem.to_string_lossy().eq_ignore_ascii_case(requested))
            || entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".md")
                .eq_ignore_ascii_case(requested);
        if matches_entry_name || skill_frontmatter_name_matches(&candidate_path, requested) {
            return Some(candidate_path);
        }
    }

    None
}

fn skill_frontmatter_name_matches(path: &std::path::Path, requested: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| parse_skill_name(&contents))
        .is_some_and(|name| name.eq_ignore_ascii_case(requested))
}

fn parse_skill_name(contents: &str) -> Option<String> {
    parse_skill_frontmatter_value(contents, "name")
}

fn parse_skill_frontmatter_value(contents: &str, key: &str) -> Option<String> {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }

    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix(&format!("{key}:")) {
            let value = value
                .trim()
                .trim_matches(|ch| matches!(ch, '"' | '\''))
                .trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }

    None
}

const DEFAULT_AGENT_MODEL: &str = "claude-opus-4-6";
const DEFAULT_AGENT_MAX_ITERATIONS: usize = 32;

fn execute_agent(
    input: AgentInput,
    ctx: Option<&ToolDispatchContext>,
) -> Result<AgentOutput, String> {
    if input.run_in_background.unwrap_or(true) {
        execute_agent_with_spawn_and_context(input, ctx, spawn_agent_job)
    } else {
        execute_agent_inline(input, ctx)
    }
}

struct PreparedAgent {
    manifest: AgentOutput,
    job: AgentJob,
}

fn prepare_agent_job(
    input: AgentInput,
    ctx: Option<&ToolDispatchContext>,
) -> Result<PreparedAgent, String> {
    if input.description.trim().is_empty() {
        return Err(String::from("description must not be empty"));
    }
    if input.prompt.trim().is_empty() {
        return Err(String::from("prompt must not be empty"));
    }

    let normalized_subagent_type = normalize_subagent_type(input.subagent_type.as_deref());
    let is_fork = normalized_subagent_type == "fork";

    // Verification-streak reset: whenever the model dispatches a
    // Verification sub-agent, zero the process-global streak counter
    // (`runtime::verification_watcher`) so the nudge that fired at
    // 3-completions-in-a-row doesn't re-fire until another streak
    // accumulates. Placed BEFORE the fork-recursion guard so the
    // reset happens even if the Verification spawn is unreachable
    // for some other unrelated reason later in this function — the
    // signal is "the model tried to verify", not "the spawn
    // succeeded".
    if normalized_subagent_type == "Verification" {
        runtime::verification_watcher::reset_streak();
    }

    // Fork recursion guard: mirrors CC-fork's `isInForkChild(messages)`
    // in forkSubagent.ts. A fork child's session already carries the
    // fork boilerplate tag in its first user message, so scanning ctx's
    // parent_session_messages catches nested fork spawn attempts before
    // any state is allocated.
    if is_fork && ctx.map_or(false, ToolDispatchContext::is_inside_fork_child) {
        return Err(String::from(
            "recursive fork detected: fork subagent may not spawn further fork children",
        ));
    }
    if is_fork
        && ctx
            .and_then(|c| c.parent_assistant_message.as_ref())
            .is_none()
    {
        return Err(String::from(
            "fork subagent requires a parent assistant message context (only spawnable from an in-flight tool loop)",
        ));
    }

    let agent_id = make_agent_id();
    let output_dir = agent_store_dir()?;
    std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    sweep_orphaned_tmp_files(&output_dir);
    let output_file = output_dir.join(format!("{agent_id}.md"));
    let manifest_file = output_dir.join(format!("{agent_id}.json"));

    let model = resolve_agent_model(input.model.as_deref());
    let agent_name = input
        .name
        .as_deref()
        .map(slugify_agent_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| slugify_agent_name(&input.description));
    let created_at = iso8601_now();
    let system_prompt = build_agent_system_prompt(&normalized_subagent_type, &model)?;
    let allowed_tools = allowed_tools_for_subagent(&normalized_subagent_type);

    // Fork subagent: wrap the caller's directive with the non-negotiable
    // rules boilerplate so the child's first turn sees them exactly the
    // way CC-fork's `buildChildMessage()` renders them, and build the
    // inherited message prefix so the child's Session::with_messages
    // pre-seed matches CC-fork's `buildForkedMessages` output.
    let (prompt_body, inherited_messages) = if is_fork {
        let parent_assistant = ctx
            .and_then(|c| c.parent_assistant_message.as_ref())
            .expect("fork ctx presence checked above");
        let messages = build_forked_messages(&input.prompt, parent_assistant);
        (build_fork_child_message(&input.prompt), messages)
    } else {
        (input.prompt.clone(), Vec::new())
    };

    let output_contents = format!(
        "# Agent Task

- id: {}
- name: {}
- description: {}
- subagent_type: {}
- created_at: {}

## Prompt

{}
",
        agent_id, agent_name, input.description, normalized_subagent_type, created_at, input.prompt
    );
    std::fs::write(&output_file, output_contents).map_err(|error| error.to_string())?;

    let assigned_color = runtime::agent_color::assign_agent_color(&agent_id).map(str::to_string);
    let manifest = AgentOutput {
        agent_id,
        name: agent_name,
        description: input.description,
        subagent_type: Some(normalized_subagent_type),
        model: Some(model),
        status: String::from("running"),
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: created_at.clone(),
        started_at: Some(created_at),
        completed_at: None,
        lane_events: vec![LaneEvent::started(iso8601_now())],
        current_blocker: None,
        derived_state: String::from("working"),
        error: None,
        result: None,
        result_full_path: None,
        color: assigned_color,
        total_tokens: None,
        tool_uses: None,
        notified: None,
    };
    write_agent_manifest(&manifest)?;

    // Capture provider config at spawn time so the subagent thread inherits the
    // parent's auth/credential settings rather than re-loading from CWD.
    let sudocode_config = load_sudocode_config();
    let fallback_config = load_provider_fallback_config();
    // Explicit override from the Agent tool call, falling back to the
    // process-wide auth mode set by the CLI at startup.
    let auth_mode = input
        .auth_mode
        .as_deref()
        .map(api::AuthMode::parse)
        .transpose()?
        .or_else(|| GLOBAL_AUTH_MODE.get().copied());
    let job = AgentJob {
        manifest: manifest.clone(),
        prompt: prompt_body,
        system_prompt,
        allowed_tools,
        sudocode_config,
        fallback_config,
        auth_mode,
        inherited_messages,
        abort_signal: HookAbortSignal::default(),
    };
    Ok(PreparedAgent { manifest, job })
}

/// Test-facing wrapper: spawns without a parent-tool-loop context.
/// Fork subagent (which needs the parent's assistant message) is
/// unreachable from this path — asking for `subagent_type = "fork"`
/// via this entry point errors out inside `prepare_agent_job`.
///
/// Only used by the inline `#[cfg(test)] mod tests` block; kept out
/// of non-test builds so the dead-code analyzer stays quiet.
#[cfg(test)]
fn execute_agent_with_spawn<F>(input: AgentInput, spawn_fn: F) -> Result<AgentOutput, String>
where
    F: FnOnce(AgentJob) -> Result<(), String>,
{
    execute_agent_with_spawn_and_context(input, None, spawn_fn)
}

/// Runtime tool-loop entry point: threads the parent's assistant
/// message context through `prepare_agent_job` so fork subagents can
/// build their inherited-message prefix.
fn execute_agent_with_spawn_and_context<F>(
    input: AgentInput,
    ctx: Option<&ToolDispatchContext>,
    spawn_fn: F,
) -> Result<AgentOutput, String>
where
    F: FnOnce(AgentJob) -> Result<(), String>,
{
    let PreparedAgent { manifest, job } = prepare_agent_job(input, ctx)?;
    global_agent_registry().register(&manifest.agent_id);
    if let Err(error) = spawn_fn(job) {
        let error = format!("failed to spawn sub-agent: {error}");
        persist_agent_terminal_state(&manifest, "failed", None, Some(error.clone()))?;
        return Err(error);
    }
    Ok(manifest)
}

fn execute_agent_inline(
    input: AgentInput,
    ctx: Option<&ToolDispatchContext>,
) -> Result<AgentOutput, String> {
    execute_agent_inline_with_work(input, ctx, |job| run_agent_job_returning_text(&job))
}

/// Default auto-background threshold. Mirrors CC-fork's 120-second
/// window for sync `Agent(...)` calls: the parent's tool loop can wait
/// this long for a "quick" delegation; after that the worker is
/// transitioned to background mode so the parent isn't blocked
/// indefinitely.
const DEFAULT_AGENT_AUTO_BG_SECS: u64 = 120;

/// Read the auto-background threshold from the environment, honouring
/// the `SUDOCODE_AGENT_AUTO_BG_SECS` override. Returns `None` when the
/// override is `0` (feature disabled — sync path stays sync
/// indefinitely, matching pre-commit-6 behaviour). Returns
/// `Some(Duration)` otherwise. Unparseable values fall back to the
/// 120 s default rather than disable the safety net.
fn auto_background_threshold() -> Option<Duration> {
    match std::env::var("SUDOCODE_AGENT_AUTO_BG_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(Duration::from_secs(secs)),
            Err(_) => Some(Duration::from_secs(DEFAULT_AGENT_AUTO_BG_SECS)),
        },
        Err(_) => Some(Duration::from_secs(DEFAULT_AGENT_AUTO_BG_SECS)),
    }
}

/// Test-facing seam for [`execute_agent_inline`]: the actual
/// LLM-driving work happens in `work_fn` so integration tests can
/// inject a deterministic sleep/return closure and observe both the
/// "completes within threshold" and "auto-backgrounded on timeout"
/// paths without a live LLM.
///
/// Semantics — when the auto-bg threshold is `Some(d)`:
/// 1. Register the abort signal in the process-wide registry (so
///    `SendMessage(shutdown_request)` reaches the still-running worker
///    if the parent hands out its `agent_id`).
/// 2. Register the agent with the completion registry so a subsequent
///    `TaskOutput(agent_id, block=true)` can await it.
/// 3. Spawn the work on a fresh thread (parallels the tokio-backed
///    `spawn_agent_job` but uses a plain `std::thread` so this path is
///    reachable from non-tokio test harnesses).
/// 4. Block up to `d` on the completion registry. Completion → return
///    the persisted manifest. Timeout → persist status
///    `backgrounded` and return the mutated manifest; the worker
///    continues in its thread and will overwrite the manifest with
///    `completed`/`failed` later.
///
/// When the threshold is `None` (env `SUDOCODE_AGENT_AUTO_BG_SECS=0`),
/// runs the work fully synchronously in the calling thread —
/// bit-for-bit identical to pre-commit-6 behaviour.
fn execute_agent_inline_with_work<W>(
    input: AgentInput,
    ctx: Option<&ToolDispatchContext>,
    work_fn: W,
) -> Result<AgentOutput, String>
where
    W: FnOnce(AgentJob) -> Result<String, String> + Send + 'static,
{
    let PreparedAgent { manifest, job } = prepare_agent_job(input, ctx)?;
    let Some(threshold) = auto_background_threshold() else {
        // Auto-bg disabled — original fully-sync path.
        return match work_fn(job) {
            Ok(final_text) => {
                persist_agent_terminal_state(
                    &manifest,
                    "completed",
                    Some(final_text.as_str()),
                    None,
                )?;
                reload_manifest_or_fallback(manifest)
            }
            Err(error) => {
                let _ =
                    persist_agent_terminal_state(&manifest, "failed", None, Some(error.clone()));
                Err(format!("sub-agent failed: {error}"))
            }
        };
    };

    // Auto-bg enabled: run on a worker thread + await up to threshold.
    let agent_id = manifest.agent_id.clone();
    register_agent_abort_signal(&agent_id, job.abort_signal.clone());
    global_agent_registry().register(&agent_id);

    let bg_manifest = manifest.clone();
    let bg_agent_id = agent_id.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work_fn(job)));
        match result {
            Ok(Ok(final_text)) => {
                let _ = persist_agent_terminal_state(
                    &bg_manifest,
                    "completed",
                    Some(final_text.as_str()),
                    None,
                );
            }
            Ok(Err(err)) => {
                let _ = persist_agent_terminal_state(&bg_manifest, "failed", None, Some(err));
            }
            Err(_) => {
                let _ = persist_agent_terminal_state(
                    &bg_manifest,
                    "failed",
                    None,
                    Some(String::from("sub-agent thread panicked")),
                );
            }
        }
        notify_agent_completion(&bg_manifest);
        unregister_agent_abort_signal(&bg_agent_id);
    });

    match global_agent_registry().await_agent(&agent_id, threshold) {
        Ok(final_manifest) => Ok(final_manifest),
        Err(e) if e.contains("timed out") => Ok(mark_manifest_backgrounded(&manifest)),
        Err(e) => Err(e),
    }
}

/// Persist the manifest with the sentinel `backgrounded` status and
/// return the mutated in-memory copy. Status `backgrounded` signals to
/// the parent: "sync call exceeded the auto-bg threshold; the worker
/// is still running — poll with `TaskOutput(agent_id, block=true)`."
/// It is NOT a terminal state — [`is_terminal_agent_status`] returns
/// false so mid-flight `TaskOutput` queries under coord mode still
/// return JSON (not the `<task-notification>` XML that requires a real
/// terminal outcome).
fn mark_manifest_backgrounded(manifest: &AgentOutput) -> AgentOutput {
    let mut updated = manifest.clone();
    updated.status = String::from("backgrounded");
    let _ = write_agent_manifest(&updated);
    updated
}

fn reload_manifest_or_fallback(manifest: AgentOutput) -> Result<AgentOutput, String> {
    // Re-read so lane events + result written by persist are visible;
    // fall back to the in-memory manifest if the re-read blips.
    std::fs::read_to_string(&manifest.manifest_file)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str::<AgentOutput>(&s).map_err(|e| e.to_string()))
        .or(Ok(manifest))
}

fn spawn_agent_job(job: AgentJob) -> Result<(), String> {
    // Tool executors always run inside a tokio runtime, so spawn on tokio's
    // managed blocking thread pool (default 512, configurable via
    // TOKIO_BLOCKING_THREADS). Scales to 100+ concurrent agents.
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| String::from("no tokio runtime available for spawning agent"))?;
    // Register the subagent's abort signal BEFORE the thread starts so a
    // `SendMessage(shutdown_request)` racing the spawn still finds an entry.
    register_agent_abort_signal(&job.manifest.agent_id, job.abort_signal.clone());
    handle.spawn_blocking(move || run_spawned_agent_job(job));
    Ok(())
}

#[allow(clippy::needless_pass_by_value)] // ownership required for move into spawn_blocking
fn run_spawned_agent_job(job: AgentJob) {
    let agent_id = job.manifest.agent_id.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_agent_job_returning_text(&job).and_then(|text| {
            persist_agent_terminal_state(&job.manifest, "completed", Some(text.as_str()), None)
        })
    }));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = persist_agent_terminal_state(&job.manifest, "failed", None, Some(error));
        }
        Err(_) => {
            let _ = persist_agent_terminal_state(
                &job.manifest,
                "failed",
                None,
                Some(String::from("sub-agent thread panicked")),
            );
        }
    }
    // Signal the completion registry so TaskOutput(agent_id, block=true) callers unblock.
    notify_agent_completion(&job.manifest);
    // Drop the abort-signal registration LAST — a
    // `SendMessage(shutdown_request)` arriving during teardown against
    // an already-completed agent is silently a no-op (returns
    // `live_abort_signaled: false`), which matches CC-fork's
    // `findTeammateTaskByAgentId → task === undefined → warn-and-skip`
    // branch in `SendMessageTool.ts:362`.
    unregister_agent_abort_signal(&agent_id);
}

/// Re-read the persisted manifest and notify the global completion registry.
fn notify_agent_completion(original_manifest: &AgentOutput) {
    let manifest = std::fs::read_to_string(&original_manifest.manifest_file)
        .ok()
        .and_then(|s| serde_json::from_str::<AgentOutput>(&s).ok())
        .unwrap_or_else(|| {
            // Fall back to a minimal failed manifest so waiters always unblock.
            let mut fallback = original_manifest.clone();
            fallback.status = String::from("failed");
            fallback.completed_at = Some(iso8601_now());
            fallback
        });
    global_agent_registry().mark_done(&manifest);
}

/// Default per-subagent cap on how many DISTINCT user-turns the
/// multi-turn loop will service before force-exiting. Not the same as
/// `DEFAULT_AGENT_MAX_ITERATIONS` (which caps tool-loop iterations
/// WITHIN a single turn). Overridable via
/// `SUDOCODE_SUBAGENT_MAX_MULTI_TURNS`.
///
/// Sized to leave plenty of headroom for a coordinator-driven
/// conversation where the parent sends 5–10 follow-ups before the
/// worker completes; at 16 turns × 32 tool-loop iterations that's
/// still bounded well below runaway.
const DEFAULT_SUBAGENT_MAX_MULTI_TURNS: usize = 16;

fn subagent_max_multi_turns() -> usize {
    std::env::var("SUDOCODE_SUBAGENT_MAX_MULTI_TURNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_SUBAGENT_MAX_MULTI_TURNS)
}

fn run_agent_job_returning_text(job: &AgentJob) -> Result<String, String> {
    let mut conv_runtime =
        build_agent_runtime(job)?.with_max_iterations(DEFAULT_AGENT_MAX_ITERATIONS);
    let workspace_root = std::env::current_dir().unwrap_or_default();
    // Accumulate telemetry across every multi-turn iteration so a
    // long coordinator-driven conversation reports one aggregated
    // `<total_tokens>` / `<tool_uses>` count in the task-notification.
    let mut cumulative_telemetry = AgentRunTelemetry::default();
    let final_text = run_multi_turn_loop(
        &job.manifest.agent_id,
        &workspace_root,
        job.abort_signal.clone(),
        job.prompt.clone(),
        subagent_max_multi_turns(),
        |prompt| {
            let summary = run_single_turn(&mut conv_runtime, prompt)?;
            let (turn_tokens, turn_tool_uses) = telemetry_from_turn(&summary);
            cumulative_telemetry.total_tokens = cumulative_telemetry
                .total_tokens
                .saturating_add(turn_tokens);
            cumulative_telemetry.tool_uses = cumulative_telemetry
                .tool_uses
                .saturating_add(turn_tool_uses);
            Ok(final_assistant_text(&summary))
        },
    )?;

    // Fold telemetry into the on-disk manifest BEFORE any downstream
    // step (summarizer, persist) reads it, so the terminal-state
    // write picks up the counts. Best-effort: an IO error here just
    // means the notification will omit `<usage>` counters — not a
    // fatal condition.
    if let Err(err) = record_agent_telemetry(&job.manifest, cumulative_telemetry) {
        eprintln!("sudocode: failed to record agent telemetry on manifest: {err}");
    }

    // If the worker's final text exceeds the summary threshold, save
    // the FULL text to a sibling `.full.md` file (so the parent can
    // `read_file` the raw output) and condense the parent-facing
    // return via a summarizer sub-turn. Aborted mid-work?
    // Skip summarization — abort semantics are "stop everything now."
    if job.abort_signal.is_aborted() {
        return Ok(final_text);
    }
    let (parent_text, full_path) = match maybe_summarize_agent_result(job, &final_text) {
        Ok(bundle) => bundle,
        Err(err) => {
            eprintln!("sudocode: agent summarizer failed ({err}); falling back to full text");
            (final_text.clone(), None)
        }
    };
    if let Some(path) = full_path {
        // Record the sibling path on the manifest so
        // `persist_agent_terminal_state` and downstream consumers can
        // point to the unabridged output. Best-effort: any IO error
        // is logged but not fatal — the parent still receives the
        // summary via the return value.
        if let Err(err) = record_full_result_path(&job.manifest, &path) {
            eprintln!("sudocode: failed to record full-result path on manifest: {err}");
        }
    }
    Ok(parent_text)
}

/// Extract per-turn telemetry from a [`runtime::TurnSummary`].
///
/// - `total_tokens` sums input, output, cache-creation, and
///   cache-read tokens — matches Anthropic's "billable tokens"
///   accounting so a parent glancing at the `<usage>` block sees the
///   number they'll be charged for.
/// - `tool_uses` counts every `ToolUse` content block across every
///   assistant message this turn produced. A single tool-loop
///   iteration typically emits ONE `ToolUse`, but the count is exact
///   so parallel tool_use invocations (rare but valid) don't
///   under-report.
fn telemetry_from_turn(summary: &runtime::TurnSummary) -> (u64, u64) {
    let usage = summary.turn_usage;
    let total_tokens = u64::from(usage.input_tokens)
        .saturating_add(u64::from(usage.output_tokens))
        .saturating_add(u64::from(usage.cache_creation_input_tokens))
        .saturating_add(u64::from(usage.cache_read_input_tokens));
    let tool_uses = summary
        .assistant_messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter(|b| matches!(b, runtime::ContentBlock::ToolUse { .. }))
        .count() as u64;
    (total_tokens, tool_uses)
}

/// Rewrite the on-disk manifest with the accumulated run telemetry.
/// Reads-modifies-writes so a concurrent `record_full_result_path`
/// call doesn't clobber the pointer either direction.
fn record_agent_telemetry(
    manifest: &AgentOutput,
    telemetry: AgentRunTelemetry,
) -> Result<(), String> {
    let existing = std::fs::read_to_string(&manifest.manifest_file).ok();
    let mut updated: AgentOutput = existing
        .as_deref()
        .and_then(|text| serde_json::from_str::<AgentOutput>(text).ok())
        .unwrap_or_else(|| manifest.clone());
    updated.total_tokens = Some(telemetry.total_tokens);
    updated.tool_uses = Some(telemetry.tool_uses);
    write_agent_manifest(&updated)
}

/// Write `full_text` to `<agent_id>.full.md` sibling next to the
/// agent's normal `.md` output and update the on-disk manifest with
/// a `result_full_path` pointer.
fn write_full_result_and_update_manifest(
    manifest: &AgentOutput,
    full_text: &str,
) -> Result<std::path::PathBuf, String> {
    let output_path = std::path::PathBuf::from(&manifest.output_file);
    let sibling = output_path.with_extension("full.md");
    let contents = format!(
        "# Agent Task — full unabridged final response\n\n\
         - agent_id: {agent_id}\n\
         - subagent_type: {subagent_type}\n\n\
         ## Final response (verbatim)\n\n{full_text}\n",
        agent_id = manifest.agent_id,
        subagent_type = manifest
            .subagent_type
            .as_deref()
            .unwrap_or("general-purpose"),
    );
    std::fs::write(&sibling, contents)
        .map_err(|e| format!("write full-result sibling {}: {e}", sibling.display()))?;
    Ok(sibling)
}

/// Update the persisted manifest's `result_full_path` to the given
/// sibling path. Reads-modifies-writes the .json manifest so a race
/// with `persist_agent_terminal_state` doesn't clobber the update:
/// the terminal-state writer starts from a fresh clone anyway, so
/// this call must happen BEFORE terminal-state persistence to be
/// preserved.
fn record_full_result_path(
    manifest: &AgentOutput,
    full_path: &std::path::Path,
) -> Result<(), String> {
    let path_str = full_path.display().to_string();
    let existing = std::fs::read_to_string(&manifest.manifest_file).ok();
    let mut updated: AgentOutput = if let Some(text) = existing {
        serde_json::from_str(&text).unwrap_or_else(|_| manifest.clone())
    } else {
        manifest.clone()
    };
    updated.result_full_path = Some(path_str);
    write_agent_manifest(&updated)
}

fn maybe_summarize_agent_result(
    job: &AgentJob,
    full_text: &str,
) -> Result<(String, Option<std::path::PathBuf>), String> {
    let Some(threshold) = agent_summary_threshold_chars() else {
        return Ok((full_text.to_string(), None));
    };
    if full_text.chars().count() <= threshold {
        return Ok((full_text.to_string(), None));
    }
    // Over threshold: persist full text alongside + summarize.
    let full_path = write_full_result_and_update_manifest(&job.manifest, full_text)?;
    let summary = run_agent_summarizer(job, full_text)?;
    Ok((summary, Some(full_path)))
}

/// Default summary threshold in CHARS (not bytes — we count via
/// `chars().count()` so multi-byte UTF-8 doesn't inflate the count).
/// Mirrors CC-fork's ~8 KB heuristic — a result any longer than this
/// starts to bloat the parent's context per delegated task.
const DEFAULT_AGENT_SUMMARY_THRESHOLD_CHARS: usize = 8000;

/// Env var override for the summary threshold. Setting to `0`
/// disables summarization entirely (parent always receives the raw
/// text — useful for debugging or for callers who explicitly want
/// verbatim results).
pub const AGENT_SUMMARY_THRESHOLD_ENV: &str = "SUDOCODE_AGENT_SUMMARY_THRESHOLD_CHARS";

/// Read the summary threshold. `None` -> feature disabled (either
/// env=0 or explicit opt-out). `Some(n)` -> results with more than
/// `n` chars get summarized.
#[must_use]
pub fn agent_summary_threshold_chars() -> Option<usize> {
    match std::env::var(AGENT_SUMMARY_THRESHOLD_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(DEFAULT_AGENT_SUMMARY_THRESHOLD_CHARS),
        },
        Err(_) => Some(DEFAULT_AGENT_SUMMARY_THRESHOLD_CHARS),
    }
}

/// Condense `final_text` via a one-turn LLM call when it exceeds the
/// configured threshold. Returns the summary on the summarize path,
/// the original text otherwise. Errors bubble up as `Err` so the
/// caller can log + fall back to the raw text (better to ship
/// something than nothing).
///
/// The summarizer reuses the sub-agent's provider config (same
/// Spin a summarizer ConversationRuntime that reuses `job`'s
/// provider config + auth but has an empty tool set + a specialized
/// system prompt, then run ONE turn asking for a ≤500-word summary.
fn run_agent_summarizer(job: &AgentJob, final_text: &str) -> Result<String, String> {
    let model = job
        .manifest
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string());
    let empty_tools: BTreeSet<String> = BTreeSet::new();
    let api_client = ProviderRuntimeClient::new_with_config(
        model,
        empty_tools.clone(),
        &job.sudocode_config,
        &job.fallback_config,
        job.auth_mode,
    )?;
    let permission_policy = agent_permission_policy();
    let tool_executor = SubagentToolExecutor::new(empty_tools);
    let mut system_prompt = SystemPrompt::default();
    system_prompt.dynamic_sections.push(String::from(
        "You are a summarizer. You will be given the full output of a background sub-agent. \
         Summarize it for the parent coordinator in 500 words or fewer. Preserve every concrete \
         file path, line number, error message, PR number, commit hash, and command that appears \
         verbatim — the parent needs those to act. Drop the padding, restatement, and \
         chain-of-thought. Reply with ONLY the summary, no preamble.",
    ));
    let mut summarizer = ConversationRuntime::new(
        Session::new(),
        api_client,
        tool_executor,
        permission_policy,
        system_prompt,
    )
    .with_session_known_date(runtime::today_local())
    .with_max_iterations(2);
    let prompt = format!(
        "Summarize this agent's output for the parent coordinator in \u{2264}500 words:\n\n{final_text}"
    );
    let summary = run_single_turn(&mut summarizer, prompt)?;
    Ok(final_assistant_text(&summary))
}

/// Sub-agent multi-turn state machine, factored out from
/// [`run_agent_job_returning_text`] so integration tests can drive it
/// deterministically with a fake `run_turn_fn`.
///
/// Contract per iteration:
/// 1. Call `run_turn_fn(current_prompt)` — the ONE-turn primitive
///    (does its own tool loop internally, returns the assistant's
///    final text). Errors bubble up.
/// 2. If `abort_signal.is_aborted()` → exit with the last final text.
///    This catches both `shutdown_request` (abort registry flipped
///    mid-turn, `run_turn` returned early with `cancelled: true`) and
///    any out-of-band `HookAbortSignal::abort()`.
/// 3. Drain any new envelopes that arrived on the agent's mailbox
///    since the last drain.
/// 4. If new envelopes include a `shutdown_request`, exit cleanly —
///    the sender already flipped the abort registry before writing,
///    but reading it just-in-case belts-and-suspenders past the
///    small race window in step 2 where the envelope landed AFTER
///    `run_turn` returned but BEFORE the abort flag was checked.
/// 5. Else, if there ARE new envelopes, synthesise the next
///    user-turn prompt from them and loop.
/// 6. Else (no new envelopes) → exit with the last final text.
///
/// A hard `max_multi_turns` cap prevents runaway if the parent
/// keeps sending mail forever — mirrors the tool-loop iteration cap
/// inside a single `run_turn`.
fn run_multi_turn_loop<F>(
    agent_id: &str,
    workspace_root: &std::path::Path,
    abort_signal: HookAbortSignal,
    initial_prompt: String,
    max_multi_turns: usize,
    mut run_turn_fn: F,
) -> Result<String, String>
where
    F: FnMut(String) -> Result<String, String>,
{
    let mut consumed_envelopes = 0usize;
    let mut current_prompt = initial_prompt;
    let mut last_final_text = String::new();

    for _turn_index in 0..max_multi_turns {
        last_final_text = run_turn_fn(current_prompt.clone())?;

        if abort_signal.is_aborted() {
            return Ok(last_final_text);
        }

        let envelopes =
            runtime::agent_mailbox::read_all(workspace_root, agent_id).unwrap_or_default();
        if envelopes.len() > consumed_envelopes {
            let new_envelopes = &envelopes[consumed_envelopes..];
            let has_shutdown = new_envelopes
                .iter()
                .any(|env| env.kind == runtime::agent_mailbox::kinds::SHUTDOWN_REQUEST);
            consumed_envelopes = envelopes.len();
            if has_shutdown {
                return Ok(last_final_text);
            }
            current_prompt = compose_next_turn_from_envelopes(new_envelopes);
            continue;
        }

        return Ok(last_final_text);
    }
    Ok(last_final_text)
}

/// Run one `ConversationRuntime::run_turn` call, handling the
/// tokio-nesting dance (block_in_place would panic on the
/// `current_thread` runtime used by the MCP entry path).
///
/// Extracted from the pre-multi-turn version of
/// [`run_agent_job_returning_text`] so both the first turn and every
/// resumed turn share the exact same nesting path — one place to
/// fix if `run_turn`'s signature or the runtime primitives change.
fn run_single_turn(
    conv_runtime: &mut ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>,
    prompt: String,
) -> Result<runtime::TurnSummary, String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
                rt.block_on(conv_runtime.run_turn(prompt, None, None))
                    .map_err(|e| e.to_string())
            })
            .join()
            .map_err(|_| String::from("sub-agent thread panicked"))?
        })
    } else {
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(conv_runtime.run_turn(prompt, None, None))
            .map_err(|e| e.to_string())
    }
}

/// Render a batch of freshly-arrived mailbox envelopes into a single
/// synthetic user-turn text. Each envelope is wrapped in an
/// XML-shaped `<mailbox-*>` block so the model can tell them apart
/// from ordinary user prompts and — critically — so a follow-up
/// SendMessage in the middle of a stream doesn't look like it came
/// from the human user.
///
/// Multiple envelopes are concatenated with a blank line so the
/// model treats them as distinct messages. Order preserves the
/// mailbox write order (JSONL is append-only).
fn compose_next_turn_from_envelopes(
    envelopes: &[runtime::agent_mailbox::MailboxEnvelope],
) -> String {
    let mut blocks = Vec::with_capacity(envelopes.len());
    for env in envelopes {
        let tag = match env.kind.as_str() {
            runtime::agent_mailbox::kinds::SHUTDOWN_REQUEST => "shutdown-request",
            runtime::agent_mailbox::kinds::SHUTDOWN_RESPONSE => "shutdown-response",
            runtime::agent_mailbox::kinds::PLAN_APPROVAL_RESPONSE => "plan-approval-response",
            _ => "mailbox-message",
        };
        let mut header = format!("<{tag} from=\"{}\"", xml_attr_escape(&env.from));
        if let Some(rid) = &env.request_id {
            header.push_str(&format!(" request-id=\"{}\"", xml_attr_escape(rid)));
        }
        header.push('>');
        blocks.push(format!("{header}\n{}\n</{tag}>", env.text));
    }
    blocks.join("\n\n")
}

/// Minimal XML-attribute escape for the mailbox envelope headers.
/// Only escapes `"` and `&` — the character set for `from` / request-id
/// is `[A-Za-z0-9_-]` in practice, but a hostile envelope shouldn't
/// break the synthetic prompt.
fn xml_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn build_agent_runtime(
    job: &AgentJob,
) -> Result<ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>, String> {
    let model = job
        .manifest
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string());
    let allowed_tools = job.allowed_tools.clone();
    // Use the config captured at spawn time instead of re-loading from disk.
    // This ensures the subagent thread inherits the parent's auth tokens and
    // provider configuration, preventing 401 Unauthorized errors when the CWD
    // or config state differs between threads.
    let api_client = ProviderRuntimeClient::new_with_config(
        model,
        allowed_tools.clone(),
        &job.sudocode_config,
        &job.fallback_config,
        job.auth_mode,
    )?;
    let permission_policy = agent_permission_policy();
    let tool_executor = SubagentToolExecutor::new(allowed_tools)
        .with_enforcer(PermissionEnforcer::new(permission_policy.clone()));
    Ok(ConversationRuntime::new(
        Session::new().with_messages(job.inherited_messages.clone()),
        api_client,
        tool_executor,
        permission_policy,
        job.system_prompt.clone(),
    )
    .with_session_known_date(runtime::today_local())
    .with_hook_abort_signal(job.abort_signal.clone()))
}

fn build_agent_system_prompt(subagent_type: &str, model: &str) -> Result<SystemPrompt, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    // Route sub-agents through the per-agent-type memory scope
    // (`<workspace>/agent-memory/<subagent_type>/`) so one agent's
    // remembered facts don't leak into another's memory index —
    // mirrors CC-fork's `agentMemory.ts` per-agent scoping. Fork is
    // intentionally scoped too so a fork child's memory is separate
    // from its parent's workspace memory.
    let mut prompt = runtime::load_system_prompt_for_agent(
        cwd,
        runtime::today_local(),
        std::env::consts::OS,
        "unknown",
        model_family_identity_for(model),
        subagent_type,
    )
    .map_err(|error| error.to_string())?;
    if subagent_type == "fork" {
        // Fork subagent gets the parent's default system prompt (via
        // load_system_prompt above) plus a fork-specific behavioral
        // hint. The non-negotiable rules travel in the user prompt
        // body (build_fork_child_message) so the child sees them as
        // its FIRST user message rather than buried in the system
        // prompt.
        prompt.dynamic_sections.push(String::from(
            "You are a fork subagent — a background worker inheriting the parent agent's context. Follow the fork rules in the first user message verbatim: execute directly with your tools, do not spawn further sub-agents, report structured facts and stop."
        ));
    } else if let Some(custom) = lookup_custom_agent(subagent_type) {
        // Custom `.md` agent — its body IS the sub-agent's role
        // section. Prepend a compact identity line so the child knows
        // its own type even if the body is terse. Mirrors CC-fork's
        // `parseAgentFromMarkdown` → `getSystemPrompt` closure that
        // returns the raw markdown body as the agent's system prompt.
        prompt.dynamic_sections.push(format!(
            "You are the custom sub-agent `{}` defined at {}.",
            custom.name,
            custom.source_path.display()
        ));
        prompt.dynamic_sections.push(custom.system_prompt);
    } else {
        prompt.dynamic_sections.push(format!(
            "You are a background sub-agent of type `{subagent_type}`. Work only on the delegated task, use only the tools available to you, do not ask the user questions, and finish with a concise result."
        ));
    }
    Ok(prompt)
}

fn resolve_agent_model(model: Option<&str>) -> String {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_AGENT_MODEL)
        .to_string()
}

fn allowed_tools_for_subagent(subagent_type: &str) -> BTreeSet<String> {
    // Custom `.md` agents can restrict their tool pool via the
    // `tools:` frontmatter. `Some(vec)` → explicit allowlist;
    // `Some(vec![])` (i.e. `tools: '*'` or no items) → inherit the
    // maximal general-purpose set; `None` (no `tools:` field) → same
    // inherit fallback.
    if let Some(custom) = lookup_custom_agent(subagent_type) {
        if let Some(ref tools) = custom.tools {
            if !tools.is_empty() {
                return tools.iter().cloned().collect();
            }
        }
        return general_purpose_tools();
    }
    let tools = match subagent_type {
        "Explore" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
        ],
        "Plan" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "Verification" => vec![
            "bash",
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
            "PowerShell",
        ],
        "scode-guide" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "statusline-setup" => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "ToolSearch",
        ],
        // Fork subagent — inherits the parent's exact tool pool
        // (mirrors CC-fork's `tools: ['*']`). Sudocode doesn't thread
        // the parent's allowed_tools into `prepare_agent_job`, so we
        // approximate `*` as the maximal set: every tool a normal
        // general-purpose subagent gets, PLUS Agent so the child can
        // still spawn NON-fork sub-agents. Fork-inside-fork recursion
        // is blocked at call time by
        // `ToolDispatchContext::is_inside_fork_child`.
        "fork" => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Skill",
            "ToolSearch",
            "NotebookEdit",
            "Sleep",
            "SendUserMessage",
            "Config",
            "StructuredOutput",
            "REPL",
            "PowerShell",
            "Agent",
            "SendMessage",
        ],
        _ => return general_purpose_tools(),
    };
    tools.into_iter().map(str::to_string).collect()
}

/// The maximal tool set a general-purpose sub-agent may invoke —
/// SSOT for both the explicit `general-purpose` preset and the
/// fallback path (unknown built-in name AND custom `.md` agents whose
/// frontmatter says `tools: '*'` / omits the field).
fn general_purpose_tools() -> BTreeSet<String> {
    [
        "bash",
        "read_file",
        "write_file",
        "edit_file",
        "glob_search",
        "grep_search",
        "WebFetch",
        "WebSearch",
        "TodoWrite",
        "Skill",
        "ToolSearch",
        "NotebookEdit",
        "Sleep",
        "SendUserMessage",
        "Config",
        "StructuredOutput",
        "REPL",
        "PowerShell",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn agent_permission_policy() -> PermissionPolicy {
    mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    )
}

/// Best-effort removal of `*.tmp` files left behind by a previous crash between
/// `fs::write` and `fs::rename` in `write_agent_manifest`. Called once per agent
/// launch so the directory stays clean without a separate startup sweep.
fn sweep_orphaned_tmp_files(dir: &std::path::Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn write_agent_manifest(manifest: &AgentOutput) -> Result<(), String> {
    let mut normalized = manifest.clone();
    normalized.lane_events = dedupe_superseded_commit_events(&normalized.lane_events);
    let json = serde_json::to_string_pretty(&normalized).map_err(|e| e.to_string())?;
    // Write to a temp file then rename for atomic visibility — prevents a reader
    // seeing a partially-written file during truncate-then-write.
    let tmp_path = format!("{}.tmp", normalized.manifest_file);
    std::fs::write(&tmp_path, &json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp_path, &normalized.manifest_file).map_err(|e| e.to_string())
}

fn persist_agent_terminal_state(
    manifest: &AgentOutput,
    status: &str,
    result: Option<&str>,
    error: Option<String>,
) -> Result<(), String> {
    persist_agent_terminal_state_with_telemetry(manifest, status, result, error, None)
}

/// Terminal-state persistence with optional run telemetry.
///
/// Key behaviour: starts by re-reading the on-disk manifest so any
/// mid-run mutations (result_full_path from AgentSummary, telemetry
/// updates from earlier turns) are preserved instead of clobbered by
/// the in-memory `manifest` snapshot the caller was holding. Falls
/// back to the caller-supplied manifest if the disk read fails (fresh
/// manifest, permission error, etc.).
fn persist_agent_terminal_state_with_telemetry(
    manifest: &AgentOutput,
    status: &str,
    result: Option<&str>,
    error: Option<String>,
    telemetry: Option<AgentRunTelemetry>,
) -> Result<(), String> {
    let blocker = error.as_deref().map(classify_lane_blocker);
    append_agent_output(
        &manifest.output_file,
        &format_agent_terminal_output(status, result, blocker.as_ref(), error.as_deref()),
    )?;
    // Re-read the current on-disk manifest so fields mutated between
    // spawn and terminal-state (result_full_path from AgentSummary,
    // per-turn telemetry updates) survive the write. Fall back to the
    // caller's snapshot when the disk state is unreadable.
    let mut next_manifest = std::fs::read_to_string(&manifest.manifest_file)
        .ok()
        .and_then(|text| serde_json::from_str::<AgentOutput>(&text).ok())
        .unwrap_or_else(|| manifest.clone());
    if let Some(t) = telemetry {
        next_manifest.total_tokens = Some(t.total_tokens);
        next_manifest.tool_uses = Some(t.tool_uses);
    }
    next_manifest.status = status.to_string();
    next_manifest.completed_at = Some(iso8601_now());
    next_manifest.current_blocker.clone_from(&blocker);
    next_manifest.derived_state =
        derive_agent_state(status, result, error.as_deref(), blocker.as_ref()).to_string();
    next_manifest.result = result
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    next_manifest.error = error;
    if let Some(blocker) = blocker {
        next_manifest
            .lane_events
            .push(LaneEvent::blocked(iso8601_now(), &blocker));
        next_manifest
            .lane_events
            .push(LaneEvent::failed(iso8601_now(), &blocker));
    } else {
        next_manifest.current_blocker = None;
        let mut finished_summary = build_lane_finished_summary(&next_manifest, result);
        finished_summary.data.disabled_cron_ids = disable_matching_crons(&next_manifest, result);
        next_manifest.lane_events.push(
            LaneEvent::finished(iso8601_now(), finished_summary.detail).with_data(
                serde_json::to_value(&finished_summary.data)
                    .expect("lane summary metadata should serialize"),
            ),
        );
        if let Some(provenance) = maybe_commit_provenance(result) {
            next_manifest.lane_events.push(LaneEvent::commit_created(
                iso8601_now(),
                Some(format!("commit {}", provenance.commit)),
                provenance,
            ));
        }
    }

    // Coordinator push idempotency: mirror CC-fork's atomic
    // check-and-set on `LocalAgentTaskState.notified`
    // (LocalAgentTask.tsx:228-237). If the flag is already set on
    // the on-disk manifest (from a prior persist call for this same
    // agent — should never happen today, but defensive), skip the
    // emit. Otherwise set the flag and persist it in the SAME write
    // as the terminal state, so a crash between "manifest written"
    // and "envelope appended" biases toward NOT re-emitting
    // (under-notify > double-notify; the coord can always poll
    // TaskOutput to recover).
    let should_emit = !next_manifest.notified.unwrap_or(false);
    if should_emit {
        next_manifest.notified = Some(true);
    }
    write_agent_manifest(&next_manifest)?;

    if should_emit {
        // The emit helper self-guards on `is_coordinator_mode` —
        // no conditional needed here. Best-effort: an IO error on
        // emit shouldn't fail the terminal persist, so we log +
        // swallow. The `notified=true` flag persists regardless so
        // a retry doesn't double-emit.
        let xml = render_manifest_task_notification(&next_manifest);
        let workspace_root = std::env::current_dir().unwrap_or_default();
        if let Err(err) =
            runtime::coordinator_notification::emit(&workspace_root, &next_manifest.agent_id, &xml)
        {
            eprintln!("sudocode: failed to emit coordinator task-notification: {err}");
        }
    }
    Ok(())
}

const MIN_LANE_SUMMARY_WORDS: usize = 7;
const REVIEW_VERDICTS: &[(&str, &str)] = &[
    ("APPROVE", "approve"),
    ("REJECT", "reject"),
    ("BLOCKED", "blocked"),
];
const CONTROL_ONLY_SUMMARY_WORDS: &[&str] = &[
    "ack",
    "commit",
    "continue",
    "everyting",
    "everything",
    "keep",
    "next",
    "push",
    "ralph",
    "resume",
    "retry",
    "run",
    "stop",
    "sweep",
    "sweeping",
    "team",
];
const CONTEXTUAL_SUMMARY_WORDS: &[&str] = &[
    "added",
    "audited",
    "blocked",
    "completed",
    "documented",
    "failed",
    "finished",
    "fixed",
    "implemented",
    "investigated",
    "merged",
    "pushed",
    "refactored",
    "removed",
    "reviewed",
    "tested",
    "updated",
    "verified",
];

#[derive(Debug, Clone, Serialize)]
struct LaneFinishedSummaryData {
    #[serde(rename = "qualityFloorApplied")]
    quality_floor_applied: bool,
    reasons: Vec<String>,
    #[serde(rename = "rawSummary", skip_serializing_if = "Option::is_none")]
    raw_summary: Option<String>,
    #[serde(rename = "wordCount")]
    word_count: usize,
    #[serde(rename = "reviewVerdict", skip_serializing_if = "Option::is_none")]
    review_verdict: Option<String>,
    #[serde(rename = "reviewTarget", skip_serializing_if = "Option::is_none")]
    review_target: Option<String>,
    #[serde(rename = "reviewRationale", skip_serializing_if = "Option::is_none")]
    review_rationale: Option<String>,
    #[serde(rename = "selectionOutcome", skip_serializing_if = "Option::is_none")]
    selection_outcome: Option<SelectionOutcome>,
    #[serde(rename = "recoveryOutcome", skip_serializing_if = "Option::is_none")]
    recovery_outcome: Option<RecoveryOutcome>,
    #[serde(rename = "artifactProvenance", skip_serializing_if = "Option::is_none")]
    artifact_provenance: Option<ArtifactProvenance>,
    #[serde(rename = "disabledCronIds", skip_serializing_if = "Vec::is_empty")]
    disabled_cron_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct LaneFinishedSummary {
    detail: Option<String>,
    data: LaneFinishedSummaryData,
}

#[derive(Debug)]
struct LaneSummaryAssessment {
    apply_quality_floor: bool,
    reasons: Vec<String>,
    word_count: usize,
    review_outcome: Option<ReviewLaneOutcome>,
    recovery_outcome: Option<RecoveryOutcome>,
}

#[derive(Debug, Clone)]
struct ReviewLaneOutcome {
    verdict: String,
    rationale: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SelectionOutcome {
    #[serde(rename = "chosenItems", skip_serializing_if = "Vec::is_empty")]
    chosen_items: Vec<String>,
    #[serde(rename = "skippedItems", skip_serializing_if = "Vec::is_empty")]
    skipped_items: Vec<String>,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rationale: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RecoveryOutcome {
    cause: String,
    #[serde(rename = "targetLane", skip_serializing_if = "Option::is_none")]
    target_lane: Option<String>,
    #[serde(rename = "preservedState", skip_serializing_if = "Option::is_none")]
    preserved_state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ArtifactProvenance {
    #[serde(rename = "sourceLanes", skip_serializing_if = "Vec::is_empty")]
    source_lanes: Vec<String>,
    #[serde(rename = "roadmapIds", skip_serializing_if = "Vec::is_empty")]
    roadmap_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<String>,
    #[serde(rename = "diffStat", skip_serializing_if = "Option::is_none")]
    diff_stat: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verification: Vec<String>,
    #[serde(rename = "commitSha", skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
}

fn build_lane_finished_summary(
    manifest: &AgentOutput,
    result: Option<&str>,
) -> LaneFinishedSummary {
    let raw_summary = result.map(str::trim).filter(|value| !value.is_empty());
    let assessment = assess_lane_summary_quality(raw_summary.unwrap_or_default());
    let detail = match raw_summary {
        Some(summary) if !assessment.apply_quality_floor => Some(compress_summary_text(summary)),
        Some(summary) => Some(compose_lane_summary_fallback(
            manifest,
            Some(summary),
            assessment.recovery_outcome.as_ref(),
        )),
        None => Some(compose_lane_summary_fallback(manifest, None, None)),
    };
    let review_outcome = assessment.review_outcome.clone();
    let recovery_outcome = assessment.recovery_outcome.clone();
    let review_target = review_outcome
        .as_ref()
        .map(|_| manifest.description.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let artifact_provenance = extract_artifact_provenance(manifest, raw_summary);

    LaneFinishedSummary {
        detail,
        data: LaneFinishedSummaryData {
            quality_floor_applied: raw_summary.is_none() || assessment.apply_quality_floor,
            reasons: assessment.reasons,
            raw_summary: raw_summary.map(str::to_string),
            word_count: assessment.word_count,
            review_verdict: review_outcome
                .as_ref()
                .map(|outcome| outcome.verdict.clone()),
            review_target,
            review_rationale: review_outcome.and_then(|outcome| outcome.rationale),
            selection_outcome: extract_selection_outcome(raw_summary.unwrap_or_default()),
            recovery_outcome,
            artifact_provenance,
            disabled_cron_ids: Vec::new(),
        },
    }
}

fn assess_lane_summary_quality(summary: &str) -> LaneSummaryAssessment {
    let words = summary
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '#'))
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    let word_count = words.len();
    let mut reasons = Vec::new();
    if summary.trim().is_empty() {
        reasons.push(String::from("empty"));
    }

    let review_outcome = extract_review_outcome(summary);
    let recovery_outcome = extract_recovery_outcome(summary);
    if recovery_outcome.is_some() {
        reasons.push(String::from("recovery_control_prose"));
    }

    let control_only = !words.is_empty()
        && words
            .iter()
            .all(|word| CONTROL_ONLY_SUMMARY_WORDS.contains(&word.as_str()));
    if control_only && review_outcome.is_none() {
        reasons.push(String::from("control_only"));
    }

    let has_context_signal = summary.contains('`')
        || summary.contains('/')
        || summary.contains(':')
        || summary.contains('#')
        || review_outcome.is_some()
        || words
            .iter()
            .any(|word| CONTEXTUAL_SUMMARY_WORDS.contains(&word.as_str()));
    if word_count < MIN_LANE_SUMMARY_WORDS && !has_context_signal {
        reasons.push(String::from("too_short_without_context"));
    }

    LaneSummaryAssessment {
        apply_quality_floor: !reasons.is_empty(),
        reasons,
        word_count,
        review_outcome,
        recovery_outcome,
    }
}

fn compose_lane_summary_fallback(
    manifest: &AgentOutput,
    raw_summary: Option<&str>,
    recovery_outcome: Option<&RecoveryOutcome>,
) -> String {
    let target = manifest.description.trim();
    let base = format!(
        "Completed lane `{}` for target: {}. Status: completed.",
        manifest.name,
        if target.is_empty() {
            "unspecified task"
        } else {
            target
        }
    );
    if let Some(outcome) = recovery_outcome {
        let mut detail = format!(
            "{base} Recovery handoff observed via tmux reinjection (cause: `{}`).",
            outcome.cause
        );
        if let Some(target_lane) = &outcome.target_lane {
            let _ = std::fmt::Write::write_fmt(
                &mut detail,
                format_args!(" Target lane: `{target_lane}`."),
            );
        }
        if let Some(preserved_state) = &outcome.preserved_state {
            let _ = std::fmt::Write::write_fmt(
                &mut detail,
                format_args!(" Preserved state: {preserved_state}."),
            );
        }
        return detail;
    }
    match raw_summary {
        Some(summary) => format!(
            "{base} Original stop summary was too vague to keep as the lane result: \"{}\".",
            summary.trim()
        ),
        None => format!("{base} No usable stop summary was produced by the lane."),
    }
}

fn extract_review_outcome(summary: &str) -> Option<ReviewLaneOutcome> {
    let mut lines = summary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next()?;
    let verdict = REVIEW_VERDICTS.iter().find_map(|(prefix, verdict)| {
        first
            .eq_ignore_ascii_case(prefix)
            .then(|| (*verdict).to_string())
    })?;
    let rationale = lines.collect::<Vec<_>>().join(" ").trim().to_string();
    Some(ReviewLaneOutcome {
        verdict,
        rationale: (!rationale.is_empty()).then_some(compress_summary_text(&rationale)),
    })
}

fn extract_selection_outcome(summary: &str) -> Option<SelectionOutcome> {
    let mut chosen_items = Vec::new();
    let mut skipped_items = Vec::new();
    let mut action = None;
    let mut rationale = None;

    for line in summary
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let lowered = line.to_ascii_lowercase();
        let roadmap_items = extract_roadmap_items(line);

        if lowered.starts_with("chosen:")
            || lowered.starts_with("picked:")
            || lowered.starts_with("selected:")
            || (lowered.contains("picked") && !roadmap_items.is_empty())
            || (lowered.contains("selected") && !roadmap_items.is_empty())
        {
            chosen_items.extend(roadmap_items);
        } else if lowered.starts_with("skipped:")
            || lowered.starts_with("skip:")
            || (lowered.contains("skipped") && !roadmap_items.is_empty())
        {
            skipped_items.extend(roadmap_items);
        }

        if let Some(rest) = lowered.strip_prefix("action:") {
            if rest.contains("execute") || rest.contains("implement") || rest.contains("fix") {
                action = Some(String::from("execute"));
            } else if rest.contains("review") || rest.contains("audit") {
                action = Some(String::from("review"));
            } else if rest.contains("no-op") || rest.contains("noop") {
                action = Some(String::from("no-op"));
            }
        }

        if let Some(rest) = line.strip_prefix("Rationale:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                rationale = Some(compress_summary_text(trimmed));
            }
        }
    }

    chosen_items.sort();
    chosen_items.dedup();
    skipped_items.sort();
    skipped_items.dedup();

    if chosen_items.is_empty() && skipped_items.is_empty() && action.is_none() {
        return None;
    }

    let default_action = if chosen_items.is_empty() {
        String::from("no-op")
    } else {
        String::from("execute")
    };

    Some(SelectionOutcome {
        chosen_items,
        skipped_items,
        action: action.unwrap_or(default_action),
        rationale,
    })
}

fn extract_recovery_outcome(summary: &str) -> Option<RecoveryOutcome> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lowered = trimmed.to_ascii_lowercase();
    let has_tmux_inject_marker = lowered.contains("omx_tmux_inject");
    let has_recovery_phrase = lowered.contains("continue from current mode state")
        || (lowered.starts_with("team ") && lowered.contains(" next:"));
    if !has_tmux_inject_marker && !has_recovery_phrase {
        return None;
    }

    let cause = if lowered.contains("current mode state") {
        "resume_after_stop"
    } else if lowered.contains("tool failure") {
        "retry_after_tool_failure"
    } else if lowered.contains("worker panes stalled")
        || lowered.contains("no progress")
        || lowered.contains("leader stale")
        || lowered.contains("all workers idle")
        || lowered.contains("all 1 worker idle")
        || lowered.contains("pane(s) active")
    {
        "tmux_reinject_after_idle"
    } else {
        "manual_recovery"
    };

    let target_lane = trimmed.lines().map(str::trim).find_map(|line| {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("team ") {
            return None;
        }
        line[5..]
            .split_once(':')
            .map(|(name, _)| name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    });

    let preserved_state = lowered
        .contains("current mode state")
        .then(|| String::from("current mode state"));

    Some(RecoveryOutcome {
        cause: cause.to_string(),
        target_lane,
        preserved_state,
    })
}

fn extract_roadmap_items(line: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '#' {
            let mut digits = String::new();
            while let Some(next) = chars.peek() {
                if next.is_ascii_digit() {
                    digits.push(*next);
                    chars.next();
                } else {
                    break;
                }
            }
            if !digits.is_empty() {
                items.push(format!("ROADMAP #{digits}"));
            }
        }
    }
    items
}

fn extract_artifact_provenance(
    manifest: &AgentOutput,
    raw_summary: Option<&str>,
) -> Option<ArtifactProvenance> {
    let summary = raw_summary?;
    let mut roadmap_ids = extract_roadmap_items(summary);
    roadmap_ids.extend(extract_roadmap_items(&manifest.description));
    roadmap_ids.sort();
    roadmap_ids.dedup();

    let mut files = extract_file_paths(summary);
    files.sort();
    files.dedup();

    let mut verification = Vec::new();
    let lowered = summary.to_ascii_lowercase();
    for (needle, label) in [
        ("tested", "tested"),
        ("committed", "committed"),
        ("pushed", "pushed"),
        ("merged", "merged"),
    ] {
        if lowered.contains(needle) {
            verification.push(label.to_string());
        }
    }

    let commit_sha = extract_commit_sha(summary);
    let diff_stat = extract_diff_stat(summary);
    let source_lanes = vec![manifest.name.clone()];

    if roadmap_ids.is_empty()
        && files.is_empty()
        && verification.is_empty()
        && commit_sha.is_none()
        && diff_stat.is_none()
    {
        return None;
    }

    Some(ArtifactProvenance {
        source_lanes,
        roadmap_ids,
        files,
        diff_stat,
        verification,
        commit_sha,
    })
}

fn extract_file_paths(summary: &str) -> Vec<String> {
    summary
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | '(' | ')' | '[' | ']'))
        .map(|token| {
            token
                .trim_matches('`')
                .trim_matches('"')
                .trim_matches('\'')
                .trim_end_matches('.')
        })
        .filter(|token| {
            token.contains('.')
                && !token.starts_with("http")
                && !token
                    .chars()
                    .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '+' || ch == '-')
        })
        .map(str::to_string)
        .collect()
}

fn extract_diff_stat(summary: &str) -> Option<String> {
    summary
        .split('\n')
        .map(str::trim)
        .find_map(|line| {
            line.find("Diff stat:")
                .map(|index| normalize_diff_stat(&line[(index + "Diff stat:".len())..]))
                .or_else(|| {
                    line.find("Diff:")
                        .map(|index| normalize_diff_stat(&line[(index + "Diff:".len())..]))
                })
        })
        .filter(|value| !value.is_empty())
}

fn normalize_diff_stat(value: &str) -> String {
    let trimmed = value.trim();
    for marker in [" Tested", " Committed", " committed", " pushed", " merged"] {
        if let Some((prefix, _)) = trimmed.split_once(marker) {
            return prefix.trim().to_string();
        }
    }
    trimmed.to_string()
}

fn disable_matching_crons(manifest: &AgentOutput, result: Option<&str>) -> Vec<String> {
    let tokens = cron_match_tokens(manifest, result);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut disabled = Vec::new();
    for entry in global_cron_registry().list(true) {
        let haystack = format!(
            "{} {}",
            entry.prompt,
            entry.description.as_deref().unwrap_or_default()
        )
        .to_ascii_lowercase();
        if tokens.iter().any(|token| haystack.contains(token))
            && global_cron_registry().disable(&entry.cron_id).is_ok()
        {
            disabled.push(entry.cron_id);
        }
    }
    disabled.sort();
    disabled
}

fn cron_match_tokens(manifest: &AgentOutput, result: Option<&str>) -> Vec<String> {
    let mut tokens = extract_roadmap_items(manifest.description.as_str())
        .into_iter()
        .chain(extract_roadmap_items(result.unwrap_or_default()))
        .map(|item| item.to_ascii_lowercase())
        .collect::<Vec<_>>();

    if tokens.is_empty() && !manifest.name.trim().is_empty() {
        tokens.push(manifest.name.trim().to_ascii_lowercase());
    }

    tokens.sort();
    tokens.dedup();
    tokens
}

fn derive_agent_state(
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
) -> &'static str {
    let normalized_status = status.trim().to_ascii_lowercase();
    let normalized_error = error.unwrap_or_default().to_ascii_lowercase();

    if normalized_status == "running" {
        return "working";
    }
    if normalized_status == "completed" {
        return if result.is_some_and(|value| !value.trim().is_empty()) {
            "finished_cleanable"
        } else {
            "finished_pending_report"
        };
    }
    if normalized_error.contains("background") {
        return "blocked_background_job";
    }
    if normalized_error.contains("merge conflict") || normalized_error.contains("cherry-pick") {
        return "blocked_merge_conflict";
    }
    if normalized_error.contains("mcp") {
        return "degraded_mcp";
    }
    if normalized_error.contains("transport")
        || normalized_error.contains("broken pipe")
        || normalized_error.contains("connection")
        || normalized_error.contains("interrupted")
    {
        return "interrupted_transport";
    }
    if blocker.is_some() {
        return "truly_idle";
    }
    "truly_idle"
}

fn maybe_commit_provenance(result: Option<&str>) -> Option<LaneCommitProvenance> {
    let commit = extract_commit_sha(result?)?;
    let branch = current_git_branch().unwrap_or_else(|| "unknown".to_string());
    let worktree = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    Some(LaneCommitProvenance {
        commit: commit.clone(),
        branch,
        worktree,
        canonical_commit: Some(commit.clone()),
        superseded_by: None,
        lineage: vec![commit],
    })
}

fn extract_commit_sha(result: &str) -> Option<String> {
    result
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|token| token.len() >= 7 && token.len() <= 40)
        .map(str::to_string)
}

fn current_git_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn append_agent_output(path: &str, suffix: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(suffix.as_bytes())
        .map_err(|error| error.to_string())
}

fn format_agent_terminal_output(
    status: &str,
    result: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    error: Option<&str>,
) -> String {
    let mut sections = vec![format!("\n## Result\n\n- status: {status}\n")];
    if let Some(blocker) = blocker {
        sections.push(format!(
            "\n### Blocker\n\n- failure_class: {}\n- detail: {}\n",
            serde_json::to_string(&blocker.failure_class)
                .unwrap_or_else(|_| "\"infra\"".to_string())
                .trim_matches('"'),
            blocker.detail.trim()
        ));
    }
    if let Some(result) = result.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Final response\n\n{}\n", result.trim()));
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Error\n\n{}\n", error.trim()));
    }
    sections.join("")
}

fn classify_lane_blocker(error: &str) -> LaneEventBlocker {
    let detail = error.trim().to_string();
    LaneEventBlocker {
        failure_class: classify_lane_failure(error),
        detail,
        subphase: None,
    }
}

fn classify_lane_failure(error: &str) -> LaneFailureClass {
    let normalized = error.to_ascii_lowercase();

    if normalized.contains("prompt") && normalized.contains("deliver") {
        LaneFailureClass::PromptDelivery
    } else if normalized.contains("trust") {
        LaneFailureClass::TrustGate
    } else if normalized.contains("branch")
        && (normalized.contains("stale") || normalized.contains("diverg"))
    {
        LaneFailureClass::BranchDivergence
    } else if normalized.contains("gateway") || normalized.contains("routing") {
        LaneFailureClass::GatewayRouting
    } else if normalized.contains("compile")
        || normalized.contains("build failed")
        || normalized.contains("cargo check")
    {
        LaneFailureClass::Compile
    } else if normalized.contains("test") {
        LaneFailureClass::Test
    } else if normalized.contains("tool failed")
        || normalized.contains("runtime tool")
        || normalized.contains("tool runtime")
    {
        LaneFailureClass::ToolRuntime
    } else if normalized.contains("workspace") && normalized.contains("mismatch") {
        LaneFailureClass::WorkspaceMismatch
    } else if normalized.contains("plugin") {
        LaneFailureClass::PluginStartup
    } else if normalized.contains("mcp") && normalized.contains("handshake") {
        LaneFailureClass::McpHandshake
    } else if normalized.contains("mcp") {
        LaneFailureClass::McpStartup
    } else {
        LaneFailureClass::Infra
    }
}

struct ProviderEntry {
    model: String,
    client: ProviderClient,
}

pub(crate) struct ProviderRuntimeClient {
    chain: Vec<ProviderEntry>,
    allowed_tools: BTreeSet<String>,
}

impl ProviderRuntimeClient {
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new(model: String, allowed_tools: BTreeSet<String>) -> Result<Self, String> {
        let fallback_config = load_provider_fallback_config();
        Self::new_with_fallback_config(model, allowed_tools, &fallback_config)
    }

    /// Build a client using explicitly provided configs instead of loading from
    /// the current working directory.  Used by subagent threads to inherit the
    /// parent's auth / provider settings.
    #[allow(clippy::needless_pass_by_value)]
    fn new_with_config(
        model: String,
        allowed_tools: BTreeSet<String>,
        sudocode_config: &SudoCodeConfig,
        fallback_config: &ProviderFallbackConfig,
        auth_mode: Option<api::AuthMode>,
    ) -> Result<Self, String> {
        let primary_model = fallback_config.primary().map_or(model, str::to_string);
        let primary = build_provider_entry_with_config(&primary_model, sudocode_config, auth_mode)?;
        let mut chain = vec![primary];
        for fallback_model in fallback_config.fallbacks() {
            match build_provider_entry_with_config(fallback_model, sudocode_config, auth_mode) {
                Ok(entry) => chain.push(entry),
                Err(error) => {
                    eprintln!(
                        "warning: skipping unavailable fallback provider {fallback_model}: {error}"
                    );
                }
            }
        }
        Ok(Self {
            chain,
            allowed_tools,
        })
    }

    #[allow(dead_code, clippy::needless_pass_by_value)]
    fn new_with_fallback_config(
        model: String,
        allowed_tools: BTreeSet<String>,
        fallback_config: &ProviderFallbackConfig,
    ) -> Result<Self, String> {
        let primary_model = fallback_config.primary().map_or(model, str::to_string);
        let primary = build_provider_entry(&primary_model)?;
        let mut chain = vec![primary];
        for fallback_model in fallback_config.fallbacks() {
            match build_provider_entry(fallback_model) {
                Ok(entry) => chain.push(entry),
                Err(error) => {
                    eprintln!(
                        "warning: skipping unavailable fallback provider {fallback_model}: {error}"
                    );
                }
            }
        }
        Ok(Self {
            chain,
            allowed_tools,
        })
    }
}

#[allow(dead_code)]
fn build_provider_entry(model: &str) -> Result<ProviderEntry, String> {
    let sudocode_config = load_sudocode_config();
    build_provider_entry_with_config(model, &sudocode_config, None)
}

fn build_provider_entry_with_config(
    model: &str,
    sudocode_config: &SudoCodeConfig,
    auth_mode: Option<api::AuthMode>,
) -> Result<ProviderEntry, String> {
    let resolved_provider = resolve_provider_from_config(model, auth_mode, sudocode_config)
        .map_err(|e| e.to_string())?;
    let wire_model = resolved_provider.model_id.clone();
    let client =
        ProviderClient::from_resolved(&resolved_provider, auth_mode).map_err(|e| e.to_string())?;
    Ok(ProviderEntry {
        model: wire_model,
        client,
    })
}

fn load_sudocode_config() -> SudoCodeConfig {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            runtime::config::ConfigLoader::default_for(cwd)
                .load_sudocode_config()
                .ok()
        })
        .unwrap_or_default()
}

fn load_provider_fallback_config() -> ProviderFallbackConfig {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| ConfigLoader::default_for(cwd).load().ok())
        .map_or_else(ProviderFallbackConfig::default, |config| {
            config.provider_fallbacks().clone()
        })
}

#[async_trait::async_trait]
impl ApiClient for ProviderRuntimeClient {
    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        let tools = tool_specs_for_allowed_tools(Some(&self.allowed_tools))
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name.to_string(),
                description: Some(spec.description.to_string()),
                input_schema: spec.input_schema,
            })
            .collect::<Vec<_>>();
        let messages = convert_messages(&request.messages);
        let system = (!request.system_prompt.is_empty()).then(|| request.system_prompt.render());
        let tool_choice = (!self.allowed_tools.is_empty()).then_some(ToolChoice::Auto);

        let chain = &self.chain;
        let mut last_error: Option<ApiError> = None;
        for (index, entry) in chain.iter().enumerate() {
            let message_request = MessageRequest {
                model: entry.model.clone(),
                max_tokens: max_tokens_for_model(&entry.model),
                messages: messages.clone(),
                system: system.clone(),
                tools: (!tools.is_empty()).then(|| tools.clone()),
                tool_choice: tool_choice.clone(),
                stream: true,
                ..Default::default()
            };

            let attempt = stream_with_provider(&entry.client, &message_request).await;
            match attempt {
                Ok(events) => {
                    return Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))));
                }
                Err(error) if error.is_retryable() && index + 1 < chain.len() => {
                    eprintln!(
                        "provider {} failed with retryable error, falling back: {error}",
                        entry.model
                    );
                    last_error = Some(error);
                }
                Err(error) => return Err(RuntimeError::new(error.to_string())),
            }
        }

        Err(RuntimeError::new(last_error.map_or_else(
            || String::from("provider chain exhausted with no attempts"),
            |error| error.to_string(),
        )))
    }
}

#[allow(clippy::too_many_lines)]
async fn stream_with_provider(
    client: &ProviderClient,
    message_request: &MessageRequest,
) -> Result<Vec<AssistantEvent>, ApiError> {
    let mut stream = client.stream_message(message_request, None).await?;
    let mut events = Vec::new();
    let mut pending_tools: BTreeMap<u32, (String, String, String, Option<String>)> =
        BTreeMap::new();
    let mut saw_stop = false;

    while let Some(event) = stream.next_event().await? {
        match event {
            ApiStreamEvent::MessageStart(start) => {
                for block in start.message.content {
                    push_output_block(block, 0, &mut events, &mut pending_tools, true);
                }
            }
            ApiStreamEvent::ContentBlockStart(start) => {
                push_output_block(
                    start.content_block,
                    start.index,
                    &mut events,
                    &mut pending_tools,
                    true,
                );
            }
            ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                ContentBlockDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        events.push(AssistantEvent::TextDelta(text));
                    }
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    if let Some((_, _, input, _)) = pending_tools.get_mut(&delta.index) {
                        input.push_str(&partial_json);
                    }
                }
                ContentBlockDelta::ThinkingDelta { .. }
                | ContentBlockDelta::SignatureDelta { .. } => {}
            },
            ApiStreamEvent::ContentBlockStop(stop) => {
                if let Some((id, name, input, thought_signature)) =
                    pending_tools.remove(&stop.index)
                {
                    events.push(AssistantEvent::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                    });
                }
            }
            ApiStreamEvent::MessageDelta(delta) => {
                events.push(AssistantEvent::Usage(delta.usage.token_usage()));
            }
            ApiStreamEvent::MessageStop(_) => {
                saw_stop = true;
                events.push(AssistantEvent::MessageStop);
            }
        }
    }

    push_prompt_cache_record(client, &mut events);

    if !saw_stop
        && events.iter().any(|event| {
            matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                || matches!(event, AssistantEvent::ToolUse { .. })
        })
    {
        events.push(AssistantEvent::MessageStop);
    }

    if events
        .iter()
        .any(|event| matches!(event, AssistantEvent::MessageStop))
    {
        return Ok(events);
    }

    let response = client
        .send_message(
            &MessageRequest {
                stream: false,
                ..message_request.clone()
            },
            None,
        )
        .await?;
    let mut events = response_to_events(response);
    push_prompt_cache_record(client, &mut events);
    Ok(events)
}

struct SubagentToolExecutor {
    allowed_tools: BTreeSet<String>,
    enforcer: Option<PermissionEnforcer>,
}

impl SubagentToolExecutor {
    fn new(allowed_tools: BTreeSet<String>) -> Self {
        Self {
            allowed_tools,
            enforcer: None,
        }
    }

    fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.enforcer = Some(enforcer);
        self
    }
}

impl ToolExecutor for SubagentToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.execute_with_context(tool_name, input, &ToolDispatchContext::default())
    }

    fn execute_with_context(
        &mut self,
        tool_name: &str,
        input: &str,
        ctx: &ToolDispatchContext,
    ) -> Result<String, ToolError> {
        if !self.allowed_tools.contains(tool_name) {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this sub-agent"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        execute_tool_with_enforcer(self.enforcer.as_ref(), tool_name, &value, None, Some(ctx))
            .map_err(ToolError::new)
    }
}

fn tool_specs_for_allowed_tools(allowed_tools: Option<&BTreeSet<String>>) -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
        .collect()
}

fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    let mut result: Vec<InputMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        let content = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(InputContentBlock::Text { text: text.clone() }),
                ContentBlock::Thinking { .. } => None,
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                } => Some(InputContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::from_str(input)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    thought_signature: thought_signature.clone(),
                }),
                ContentBlock::ToolResult {
                    tool_use_id,
                    output,
                    is_error,
                    ..
                } => Some(InputContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: vec![ToolResultContentBlock::Text {
                        text: output.clone(),
                    }],
                    is_error: *is_error,
                }),
                ContentBlock::Image { data, mime_type } => Some(InputContentBlock::Image {
                    source: api::ImageSource {
                        source_type: "base64".to_string(),
                        media_type: mime_type.clone(),
                        data: data.clone(),
                    },
                }),
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            continue;
        }

        // Merge consecutive Tool-role messages into the previous user-role
        // InputMessage. Anthropic requires every `tool_use` in an assistant
        // turn to have its matching `tool_result` in the SAME next user
        // message; emitting one user message per tool_result breaks this.
        if matches!(message.role, MessageRole::Tool) {
            if let Some(last) = result.last_mut() {
                if last.role == "user"
                    && last
                        .content
                        .iter()
                        .all(|block| matches!(block, InputContentBlock::ToolResult { .. }))
                {
                    last.content.extend(content);
                    continue;
                }
            }
        }
        result.push(InputMessage {
            role: role.to_string(),
            content,
        });
    }
    result
}

fn push_output_block(
    block: OutputContentBlock,
    block_index: u32,
    events: &mut Vec<AssistantEvent>,
    pending_tools: &mut BTreeMap<u32, (String, String, String, Option<String>)>,
    streaming_tool_input: bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature,
        } => {
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            pending_tools.insert(block_index, (id, name, initial_input, thought_signature));
        }
        OutputContentBlock::Thinking { .. } | OutputContentBlock::RedactedThinking { .. } => {}
    }
}

fn response_to_events(response: MessageResponse) -> Vec<AssistantEvent> {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();

    for (index, block) in response.content.into_iter().enumerate() {
        let index = u32::try_from(index).expect("response block index overflow");
        push_output_block(block, index, &mut events, &mut pending_tools, false);
        if let Some((id, name, input, thought_signature)) = pending_tools.remove(&index) {
            events.push(AssistantEvent::ToolUse {
                id,
                name,
                input,
                thought_signature,
            });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    events
}

fn push_prompt_cache_record(client: &ProviderClient, events: &mut Vec<AssistantEvent>) {
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_tool_search(input: ToolSearchInput) -> ToolSearchOutput {
    GlobalToolRegistry::builtin().search(&input.query, input.max_results.unwrap_or(5), None, None)
}

fn deferred_tool_specs() -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| {
            !matches!(
                spec.name,
                "bash" | "read_file" | "write_file" | "edit_file" | "glob_search" | "grep_search"
            )
        })
        .collect()
}

fn search_tool_specs(query: &str, max_results: usize, specs: &[SearchableToolSpec]) -> Vec<String> {
    let lowered = query.to_lowercase();
    if let Some(selection) = lowered.strip_prefix("select:") {
        return selection
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .filter_map(|wanted| {
                let wanted = canonical_tool_token(wanted);
                specs
                    .iter()
                    .find(|spec| canonical_tool_token(&spec.name) == wanted)
                    .map(|spec| spec.name.clone())
            })
            .take(max_results)
            .collect();
    }

    let mut required = Vec::new();
    let mut optional = Vec::new();
    for term in lowered.split_whitespace() {
        if let Some(rest) = term.strip_prefix('+') {
            if !rest.is_empty() {
                required.push(rest);
            }
        } else {
            optional.push(term);
        }
    }
    let terms = if required.is_empty() {
        optional.clone()
    } else {
        required.iter().chain(optional.iter()).copied().collect()
    };

    let mut scored = specs
        .iter()
        .filter_map(|spec| {
            let name = spec.name.to_lowercase();
            let canonical_name = canonical_tool_token(&spec.name);
            let normalized_description = normalize_tool_search_query(&spec.description);
            let haystack = format!(
                "{name} {} {canonical_name}",
                spec.description.to_lowercase()
            );
            let normalized_haystack = format!("{canonical_name} {normalized_description}");
            if required.iter().any(|term| !haystack.contains(term)) {
                return None;
            }

            let mut score = 0_i32;
            for term in &terms {
                let canonical_term = canonical_tool_token(term);
                if haystack.contains(term) {
                    score += 2;
                }
                if name == *term {
                    score += 8;
                }
                if name.contains(term) {
                    score += 4;
                }
                if canonical_name == canonical_term {
                    score += 12;
                }
                if normalized_haystack.contains(&canonical_term) {
                    score += 3;
                }
            }

            if score == 0 && !lowered.is_empty() {
                return None;
            }
            Some((score, spec.name.clone()))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .map(|(_, name)| name)
        .take(max_results)
        .collect()
}

fn normalize_tool_search_query(query: &str) -> String {
    query
        .trim()
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .filter(|term| !term.is_empty())
        .map(canonical_tool_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn canonical_tool_token(value: &str) -> String {
    let mut canonical = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if let Some(stripped) = canonical.strip_suffix("tool") {
        canonical = stripped.to_string();
    }
    canonical
}

fn agent_store_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(raw) = std::env::var("SUDOCODE_AGENT_STORE") {
        let path = std::path::PathBuf::from(&raw);
        // Ensure the returned path is always absolute so that the output_file
        // and manifest_file fields in AgentOutput are reliable regardless of
        // any subsequent CWD changes.
        if path.is_absolute() {
            return Ok(path);
        }
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        return Ok(cwd.join(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(cwd.join(".sudocode-agents"))
}

fn make_agent_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("agent-{nanos}")
}

fn slugify_agent_name(description: &str) -> String {
    let mut out = description
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(32).collect()
}

fn normalize_subagent_type(subagent_type: Option<&str>) -> String {
    let trimmed = subagent_type.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return String::from("general-purpose");
    }

    match canonical_tool_token(trimmed).as_str() {
        "general" | "generalpurpose" | "generalpurposeagent" => String::from("general-purpose"),
        "explore" | "explorer" | "exploreagent" => String::from("Explore"),
        "plan" | "planagent" => String::from("Plan"),
        "verification" | "verificationagent" | "verify" | "verifier" => {
            String::from("Verification")
        }
        "scodeguide" | "scodeguideagent" | "guide" => String::from("scode-guide"),
        "statusline" | "statuslinesetup" => String::from("statusline-setup"),
        "fork" | "forksubagent" => String::from("fork"),
        // Unknown token — could be a custom `.md` agent
        // (`runtime::custom_agents`). Preserve the caller's exact
        // string so the downstream lookup by name succeeds even for
        // agents whose frontmatter `name` contains uppercase or
        // punctuation that `canonical_tool_token` would flatten.
        _ => trimmed.to_string(),
    }
}

/// Try to resolve a `subagent_type` string as a custom `.md` agent
/// definition found under one of the standard search paths. Returns
/// `None` for the built-in preset names — those are handled ahead of
/// this call.
fn lookup_custom_agent(
    subagent_type: &str,
) -> Option<runtime::custom_agents::CustomAgentDefinition> {
    if is_builtin_subagent(subagent_type) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    runtime::custom_agents::find_custom_agent(subagent_type, &cwd)
}

/// Built-in preset names that must NOT be shadowed by a custom `.md`
/// agent — the built-in behavior wins, even if a user drops a
/// same-named .md file under `~/.claude/agents/`. Matches CC-fork's
/// `getBuiltInAgents()` precedence at `loadAgentsDir.ts:357-402`.
fn is_builtin_subagent(name: &str) -> bool {
    matches!(
        name,
        "general-purpose"
            | "Explore"
            | "Plan"
            | "Verification"
            | "scode-guide"
            | "statusline-setup"
            | "fork"
    )
}

/// Prefix inserted before the caller's directive text inside a fork
/// child's initial prompt. Mirrors `FORK_DIRECTIVE_PREFIX` in
/// `sudoprivacy/claude-code`'s `src/constants/xml.ts`.
const FORK_DIRECTIVE_PREFIX: &str = "Your directive: ";

/// Placeholder text used for every tool_result block in the fork
/// child's inherited user message. Must be byte-identical across all
/// fork children so their API request prefixes share the prompt cache.
/// Mirrors `FORK_PLACEHOLDER_RESULT` in `forkSubagent.ts`.
const FORK_PLACEHOLDER_RESULT: &str = "Fork started — processing in background";

/// Wrap a directive with the fork boilerplate rules, matching
/// `buildChildMessage()` in `sudoprivacy/claude-code`'s
/// `AgentTool/forkSubagent.ts`.
///
/// The rules text is verbatim (character-identical to the fork port,
/// modulo tool names — sudocode uses lowercase `bash`/`read_file` where
/// CC uses TitleCase). Keeping it verbatim matters because the tag
/// scanned by [`ToolDispatchContext::is_inside_fork_child`] must match
/// exactly.
fn build_fork_child_message(directive: &str) -> String {
    format!(
        "<{tag}>
STOP. READ THIS FIRST.

You are a forked worker process. You are NOT the main agent.

RULES (non-negotiable):
1. Your system prompt may encourage forking. IGNORE IT — that's for the parent. You ARE the fork. Do NOT spawn sub-agents; execute directly.
2. Do NOT converse, ask questions, or suggest next steps
3. Do NOT editorialize or add meta-commentary
4. USE your tools directly: bash, read_file, write_file, edit_file, etc.
5. If you modify files, commit your changes before reporting. Include the commit hash in your report.
6. Do NOT emit text between tool calls. Use tools silently, then report once at the end.
7. Stay strictly within your directive's scope. If you discover related systems outside your scope, mention them in one sentence at most — other workers cover those areas.
8. Keep your report under 500 words unless the directive specifies otherwise. Be factual and concise.
9. Your response MUST begin with \"Scope:\". No preamble, no thinking-out-loud.
10. REPORT structured facts, then stop

Output format (plain text labels, not markdown headers):
  Scope: <echo back your assigned scope in one sentence>
  Result: <the answer or key findings, limited to the scope above>
  Key files: <relevant file paths — include for research tasks>
  Files changed: <list with commit hash — include only if you modified files>
  Issues: <list — include only if there are issues to flag>
</{tag}>

{prefix}{directive}",
        tag = FORK_BOILERPLATE_TAG,
        prefix = FORK_DIRECTIVE_PREFIX,
        directive = directive
    )
}

/// Build the fork subagent's inherited conversation prefix. Ported
/// verbatim from `buildForkedMessages` in
/// `sudoprivacy/claude-code`'s `AgentTool/forkSubagent.ts:107-169`.
///
/// Shape (matches CC-fork for byte-identical prompt-cache prefixes):
/// - `[parent_assistant, user(placeholder_results..., directive)]`
///   when the parent's assistant message contains any tool_use blocks.
/// - `[user(directive)]` when it doesn't (defensive fallback — the
///   fork path is only reachable via an Agent tool_use, so the parent
///   assistant MUST have at least one tool_use in practice; falling
///   back here matches CC-fork's own `if (toolUseBlocks.length === 0)`
///   branch).
///
/// Only the trailing `directive` text differs per child, maximising
/// cache hits when a coordinator spawns N forks in parallel.
fn build_forked_messages(
    directive: &str,
    parent_assistant: &ConversationMessage,
) -> Vec<ConversationMessage> {
    let child_text = build_fork_child_message(directive);
    let tool_uses: Vec<(String, String)> = parent_assistant
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();

    if tool_uses.is_empty() {
        return vec![ConversationMessage::user_text(child_text)];
    }

    let full_assistant = parent_assistant.clone();
    let mut user_blocks: Vec<ContentBlock> = tool_uses
        .into_iter()
        .map(|(id, name)| ContentBlock::ToolResult {
            tool_use_id: id,
            tool_name: name,
            output: FORK_PLACEHOLDER_RESULT.to_string(),
            is_error: false,
        })
        .collect();
    user_blocks.push(ContentBlock::Text { text: child_text });

    let user_message = ConversationMessage {
        role: MessageRole::User,
        blocks: user_blocks,
        usage: None,
        model: None,
    };

    vec![full_assistant, user_message]
}

fn iso8601_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[allow(clippy::too_many_lines)]
fn execute_notebook_edit(input: NotebookEditInput) -> Result<NotebookEditOutput, String> {
    let path = std::path::PathBuf::from(&input.notebook_path);
    if path.extension().and_then(|ext| ext.to_str()) != Some("ipynb") {
        return Err(String::from(
            "File must be a Jupyter notebook (.ipynb file).",
        ));
    }

    let original_file = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut notebook: serde_json::Value =
        serde_json::from_str(&original_file).map_err(|error| error.to_string())?;
    let language = notebook
        .get("metadata")
        .and_then(|metadata| metadata.get("kernelspec"))
        .and_then(|kernelspec| kernelspec.get("language"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("python")
        .to_string();
    let cells = notebook
        .get_mut("cells")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| String::from("Notebook cells array not found"))?;

    let edit_mode = input.edit_mode.unwrap_or(NotebookEditMode::Replace);
    let target_index = match input.cell_id.as_deref() {
        Some(cell_id) => Some(resolve_cell_index(cells, Some(cell_id), edit_mode)?),
        None if matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        ) =>
        {
            Some(resolve_cell_index(cells, None, edit_mode)?)
        }
        None => None,
    };
    let resolved_cell_type = match edit_mode {
        NotebookEditMode::Delete => None,
        NotebookEditMode::Insert => Some(input.cell_type.unwrap_or(NotebookCellType::Code)),
        NotebookEditMode::Replace => Some(input.cell_type.unwrap_or_else(|| {
            target_index
                .and_then(|index| cells.get(index))
                .and_then(cell_kind)
                .unwrap_or(NotebookCellType::Code)
        })),
    };
    let new_source = require_notebook_source(input.new_source, edit_mode)?;

    let cell_id = match edit_mode {
        NotebookEditMode::Insert => {
            let resolved_cell_type = resolved_cell_type
                .ok_or_else(|| String::from("insert mode requires a cell type"))?;
            let new_id = make_cell_id(cells.len());
            let new_cell = build_notebook_cell(&new_id, resolved_cell_type, &new_source);
            let insert_at = target_index.map_or(cells.len(), |index| index + 1);
            cells.insert(insert_at, new_cell);
            cells
                .get(insert_at)
                .and_then(|cell| cell.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Delete => {
            let idx = target_index
                .ok_or_else(|| String::from("delete mode requires a target cell index"))?;
            let removed = cells.remove(idx);
            removed
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
        NotebookEditMode::Replace => {
            let resolved_cell_type = resolved_cell_type
                .ok_or_else(|| String::from("replace mode requires a cell type"))?;
            let idx = target_index
                .ok_or_else(|| String::from("replace mode requires a target cell index"))?;
            let cell = cells
                .get_mut(idx)
                .ok_or_else(|| String::from("Cell index out of range"))?;
            cell["source"] = serde_json::Value::Array(source_lines(&new_source));
            cell["cell_type"] = serde_json::Value::String(match resolved_cell_type {
                NotebookCellType::Code => String::from("code"),
                NotebookCellType::Markdown => String::from("markdown"),
            });
            match resolved_cell_type {
                NotebookCellType::Code => {
                    if !cell.get("outputs").is_some_and(serde_json::Value::is_array) {
                        cell["outputs"] = json!([]);
                    }
                    if cell.get("execution_count").is_none() {
                        cell["execution_count"] = serde_json::Value::Null;
                    }
                }
                NotebookCellType::Markdown => {
                    if let Some(object) = cell.as_object_mut() {
                        object.remove("outputs");
                        object.remove("execution_count");
                    }
                }
            }
            cell.get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        }
    };

    let updated_file =
        serde_json::to_string_pretty(&notebook).map_err(|error| error.to_string())?;
    std::fs::write(&path, &updated_file).map_err(|error| error.to_string())?;

    Ok(NotebookEditOutput {
        new_source,
        cell_id,
        cell_type: resolved_cell_type,
        language,
        edit_mode: format_notebook_edit_mode(edit_mode),
        error: None,
        notebook_path: path.display().to_string(),
        original_file,
        updated_file,
    })
}

fn require_notebook_source(
    source: Option<String>,
    edit_mode: NotebookEditMode,
) -> Result<String, String> {
    match edit_mode {
        NotebookEditMode::Delete => Ok(source.unwrap_or_default()),
        NotebookEditMode::Insert | NotebookEditMode::Replace => source
            .ok_or_else(|| String::from("new_source is required for insert and replace edits")),
    }
}

fn build_notebook_cell(cell_id: &str, cell_type: NotebookCellType, source: &str) -> Value {
    let mut cell = json!({
        "cell_type": match cell_type {
            NotebookCellType::Code => "code",
            NotebookCellType::Markdown => "markdown",
        },
        "id": cell_id,
        "metadata": {},
        "source": source_lines(source),
    });
    if let Some(object) = cell.as_object_mut() {
        match cell_type {
            NotebookCellType::Code => {
                object.insert(String::from("outputs"), json!([]));
                object.insert(String::from("execution_count"), Value::Null);
            }
            NotebookCellType::Markdown => {}
        }
    }
    cell
}

fn cell_kind(cell: &serde_json::Value) -> Option<NotebookCellType> {
    cell.get("cell_type")
        .and_then(serde_json::Value::as_str)
        .map(|kind| {
            if kind == "markdown" {
                NotebookCellType::Markdown
            } else {
                NotebookCellType::Code
            }
        })
}

const MAX_SLEEP_DURATION_MS: u64 = 300_000;

#[allow(clippy::needless_pass_by_value)]
fn execute_sleep(
    input: SleepInput,
    abort_signal: Option<&HookAbortSignal>,
) -> Result<SleepOutput, String> {
    if input.duration_ms > MAX_SLEEP_DURATION_MS {
        return Err(format!(
            "duration_ms {} exceeds maximum allowed sleep of {MAX_SLEEP_DURATION_MS}ms",
            input.duration_ms,
        ));
    }
    let started = Instant::now();
    let duration = Duration::from_millis(input.duration_ms);
    while started.elapsed() < duration {
        if abort_signal.is_some_and(HookAbortSignal::is_aborted) {
            return Err(String::from("Sleep interrupted by user"));
        }
        let remaining = duration.saturating_sub(started.elapsed());
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
    Ok(SleepOutput {
        duration_ms: input.duration_ms,
        message: format!("Slept for {}ms", input.duration_ms),
    })
}

fn execute_brief(input: BriefInput) -> Result<BriefOutput, String> {
    if input.message.trim().is_empty() {
        return Err(String::from("message must not be empty"));
    }

    let attachments = input
        .attachments
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|path| resolve_attachment(path))
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?;

    let message = match input.status {
        BriefStatus::Normal | BriefStatus::Proactive => input.message,
    };

    Ok(BriefOutput {
        message,
        attachments,
        sent_at: iso8601_timestamp(),
    })
}

fn resolve_attachment(path: &str) -> Result<ResolvedAttachment, String> {
    let resolved = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(&resolved).map_err(|error| error.to_string())?;
    Ok(ResolvedAttachment {
        path: resolved.display().to_string(),
        size: metadata.len(),
        is_image: is_image_path(&resolved),
    })
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg")
    )
}

fn execute_config(input: ConfigInput) -> Result<ConfigOutput, String> {
    let setting = input.setting.trim();
    if setting.is_empty() {
        return Err(String::from("setting must not be empty"));
    }
    let Some(spec) = supported_config_setting(setting) else {
        return Ok(ConfigOutput {
            success: false,
            operation: None,
            setting: None,
            value: None,
            previous_value: None,
            new_value: None,
            error: Some(format!("Unknown setting: \"{setting}\"")),
        });
    };

    let path = config_file_for_scope(spec.scope)?;
    let mut document = read_json_object(&path)?;

    if let Some(value) = input.value {
        let normalized = normalize_config_value(spec, value)?;
        let previous_value = get_nested_value(&document, spec.path).cloned();
        set_nested_value(&mut document, spec.path, normalized.clone());
        write_json_object(&path, &document)?;
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("set")),
            setting: Some(setting.to_string()),
            value: Some(normalized.clone()),
            previous_value,
            new_value: Some(normalized),
            error: None,
        })
    } else {
        Ok(ConfigOutput {
            success: true,
            operation: Some(String::from("get")),
            setting: Some(setting.to_string()),
            value: get_nested_value(&document, spec.path).cloned(),
            previous_value: None,
            new_value: None,
            error: None,
        })
    }
}

const PERMISSION_DEFAULT_MODE_PATH: &[&str] = &["permissions", "defaultMode"];

fn execute_enter_plan_mode(_input: EnterPlanModeInput) -> Result<PlanModeOutput, String> {
    let settings_path = config_file_for_scope(ConfigScope::Settings)?;
    let state_path = plan_mode_state_file()?;
    let mut document = read_json_object(&settings_path)?;
    let current_local_mode = get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned();
    let current_is_plan =
        matches!(current_local_mode.as_ref(), Some(Value::String(value)) if value == "plan");

    if let Some(state) = read_plan_mode_state(&state_path)? {
        if current_is_plan {
            return Ok(PlanModeOutput {
                success: true,
                operation: String::from("enter"),
                changed: false,
                active: true,
                managed: true,
                message: String::from("Plan mode override is already active for this worktree."),
                settings_path: settings_path.display().to_string(),
                state_path: state_path.display().to_string(),
                previous_local_mode: state.previous_local_mode,
                current_local_mode,
            });
        }
        clear_plan_mode_state(&state_path)?;
    }

    if current_is_plan {
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("enter"),
            changed: false,
            active: true,
            managed: false,
            message: String::from(
                "Worktree-local plan mode is already enabled outside EnterPlanMode; leaving it unchanged.",
            ),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: None,
            current_local_mode,
        });
    }

    let state = PlanModeState {
        had_local_override: current_local_mode.is_some(),
        previous_local_mode: current_local_mode.clone(),
    };
    write_plan_mode_state(&state_path, &state)?;
    set_nested_value(
        &mut document,
        PERMISSION_DEFAULT_MODE_PATH,
        Value::String(String::from("plan")),
    );
    write_json_object(&settings_path, &document)?;

    Ok(PlanModeOutput {
        success: true,
        operation: String::from("enter"),
        changed: true,
        active: true,
        managed: true,
        message: String::from("Enabled worktree-local plan mode override."),
        settings_path: settings_path.display().to_string(),
        state_path: state_path.display().to_string(),
        previous_local_mode: state.previous_local_mode,
        current_local_mode: get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned(),
    })
}

fn execute_exit_plan_mode(_input: ExitPlanModeInput) -> Result<PlanModeOutput, String> {
    let settings_path = config_file_for_scope(ConfigScope::Settings)?;
    let state_path = plan_mode_state_file()?;
    let mut document = read_json_object(&settings_path)?;
    let current_local_mode = get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned();
    let current_is_plan =
        matches!(current_local_mode.as_ref(), Some(Value::String(value)) if value == "plan");

    let Some(state) = read_plan_mode_state(&state_path)? else {
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("exit"),
            changed: false,
            active: current_is_plan,
            managed: false,
            message: String::from("No EnterPlanMode override is active for this worktree."),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: None,
            current_local_mode,
        });
    };

    if !current_is_plan {
        clear_plan_mode_state(&state_path)?;
        return Ok(PlanModeOutput {
            success: true,
            operation: String::from("exit"),
            changed: false,
            active: false,
            managed: false,
            message: String::from(
                "Cleared stale EnterPlanMode state because plan mode was already changed outside the tool.",
            ),
            settings_path: settings_path.display().to_string(),
            state_path: state_path.display().to_string(),
            previous_local_mode: state.previous_local_mode,
            current_local_mode,
        });
    }

    if state.had_local_override {
        if let Some(previous_local_mode) = state.previous_local_mode.clone() {
            set_nested_value(
                &mut document,
                PERMISSION_DEFAULT_MODE_PATH,
                previous_local_mode,
            );
        } else {
            remove_nested_value(&mut document, PERMISSION_DEFAULT_MODE_PATH);
        }
    } else {
        remove_nested_value(&mut document, PERMISSION_DEFAULT_MODE_PATH);
    }
    write_json_object(&settings_path, &document)?;
    clear_plan_mode_state(&state_path)?;

    Ok(PlanModeOutput {
        success: true,
        operation: String::from("exit"),
        changed: true,
        active: false,
        managed: false,
        message: String::from("Restored the prior worktree-local plan mode setting."),
        settings_path: settings_path.display().to_string(),
        state_path: state_path.display().to_string(),
        previous_local_mode: state.previous_local_mode,
        current_local_mode: get_nested_value(&document, PERMISSION_DEFAULT_MODE_PATH).cloned(),
    })
}

fn execute_structured_output(
    input: StructuredOutputInput,
) -> Result<StructuredOutputResult, String> {
    if input.0.is_empty() {
        return Err(String::from("structured output payload must not be empty"));
    }
    Ok(StructuredOutputResult {
        data: String::from("Structured output provided successfully"),
        structured_output: input.0,
    })
}

fn execute_repl(
    input: ReplInput,
    abort_signal: Option<&HookAbortSignal>,
) -> Result<ReplOutput, String> {
    if input.code.trim().is_empty() {
        return Err(String::from("code must not be empty"));
    }
    let runtime = resolve_repl_runtime(&input.language)?;
    let started = Instant::now();
    let mut process = Command::new(runtime.program);
    process
        .args(runtime.args)
        .arg(&input.code)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let timeout_ms = input
        .timeout_ms
        .unwrap_or(runtime::DEFAULT_TOOL_SUBPROCESS_TIMEOUT_MS);
    let mut child = process.spawn().map_err(|error| error.to_string())?;
    let output = loop {
        if abort_signal.is_some_and(HookAbortSignal::is_aborted) {
            child.kill().map_err(|error| error.to_string())?;
            child
                .wait_with_output()
                .map_err(|error| error.to_string())?;
            return Err(String::from("REPL execution interrupted by user"));
        }
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            break child
                .wait_with_output()
                .map_err(|error| error.to_string())?;
        }
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            child.kill().map_err(|error| error.to_string())?;
            child
                .wait_with_output()
                .map_err(|error| error.to_string())?;
            return Err(format!(
                "REPL execution exceeded timeout of {timeout_ms} ms"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    Ok(ReplOutput {
        language: input.language,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(1),
        duration_ms: started.elapsed().as_millis(),
    })
}

struct ReplRuntime {
    program: &'static str,
    args: &'static [&'static str],
}

fn resolve_repl_runtime(language: &str) -> Result<ReplRuntime, String> {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "py" => Ok(ReplRuntime {
            program: detect_first_command(&["python3", "python"])
                .ok_or_else(|| String::from("python runtime not found"))?,
            args: &["-c"],
        }),
        "javascript" | "js" | "node" => Ok(ReplRuntime {
            program: detect_first_command(&["node"])
                .ok_or_else(|| String::from("node runtime not found"))?,
            args: &["-e"],
        }),
        "sh" | "shell" | "bash" => Ok(ReplRuntime {
            program: detect_first_command(&["bash", "sh"])
                .ok_or_else(|| String::from("shell runtime not found"))?,
            args: &["-lc"],
        }),
        other => Err(format!("unsupported REPL language: {other}")),
    }
}

fn detect_first_command(commands: &[&'static str]) -> Option<&'static str> {
    commands
        .iter()
        .copied()
        .find(|command| command_exists(command))
}

#[derive(Clone, Copy)]
enum ConfigScope {
    Global,
    Settings,
}

#[derive(Clone, Copy)]
struct ConfigSettingSpec {
    scope: ConfigScope,
    kind: ConfigKind,
    path: &'static [&'static str],
    options: Option<&'static [&'static str]>,
}

#[derive(Clone, Copy)]
enum ConfigKind {
    Boolean,
    String,
}

fn supported_config_setting(setting: &str) -> Option<ConfigSettingSpec> {
    Some(match setting {
        "theme" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["theme"],
            options: None,
        },
        "editorMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["editorMode"],
            options: Some(&["default", "vim", "emacs"]),
        },
        "verbose" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["verbose"],
            options: None,
        },
        "preferredNotifChannel" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["preferredNotifChannel"],
            options: None,
        },
        "autoCompactEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["autoCompactEnabled"],
            options: None,
        },
        "autoMemoryEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoMemoryEnabled"],
            options: None,
        },
        "autoDreamEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["autoDreamEnabled"],
            options: None,
        },
        "fileCheckpointingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["fileCheckpointingEnabled"],
            options: None,
        },
        "showTurnDuration" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["showTurnDuration"],
            options: None,
        },
        "terminalProgressBarEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["terminalProgressBarEnabled"],
            options: None,
        },
        "todoFeatureEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::Boolean,
            path: &["todoFeatureEnabled"],
            options: None,
        },
        "model" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["model"],
            options: None,
        },
        "alwaysThinkingEnabled" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::Boolean,
            path: &["alwaysThinkingEnabled"],
            options: None,
        },
        "permissions.defaultMode" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["permissions", "defaultMode"],
            options: Some(&["default", "plan", "acceptEdits", "dontAsk", "auto"]),
        },
        "language" => ConfigSettingSpec {
            scope: ConfigScope::Settings,
            kind: ConfigKind::String,
            path: &["language"],
            options: None,
        },
        "teammateMode" => ConfigSettingSpec {
            scope: ConfigScope::Global,
            kind: ConfigKind::String,
            path: &["teammateMode"],
            options: Some(&["tmux", "in-process", "auto"]),
        },
        _ => return None,
    })
}

fn normalize_config_value(spec: ConfigSettingSpec, value: ConfigValue) -> Result<Value, String> {
    let normalized = match (spec.kind, value) {
        (ConfigKind::Boolean, ConfigValue::Bool(value)) => Value::Bool(value),
        (ConfigKind::Boolean, ConfigValue::String(value)) => {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(String::from("setting requires true or false")),
            }
        }
        (ConfigKind::Boolean, ConfigValue::Number(_)) => {
            return Err(String::from("setting requires true or false"))
        }
        (ConfigKind::String, ConfigValue::String(value)) => Value::String(value),
        (ConfigKind::String, ConfigValue::Bool(value)) => Value::String(value.to_string()),
        (ConfigKind::String, ConfigValue::Number(value)) => json!(value),
    };

    if let Some(options) = spec.options {
        let Some(as_str) = normalized.as_str() else {
            return Err(String::from("setting requires a string value"));
        };
        if !options.iter().any(|option| option == &as_str) {
            return Err(format!(
                "Invalid value \"{as_str}\". Options: {}",
                options.join(", ")
            ));
        }
    }

    Ok(normalized)
}

fn config_file_for_scope(scope: ConfigScope) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    Ok(match scope {
        ConfigScope::Global => config_home_dir()?.join("settings.json"),
        ConfigScope::Settings => cwd
            .join(".nexus")
            .join("sudocode")
            .join("settings.local.json"),
    })
}

fn config_home_dir() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("SUDO_CODE_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| {
            String::from(
                "HOME is not set (on Windows, set USERPROFILE or HOME, \
                 or use SUDO_CODE_CONFIG_HOME to point directly at the config directory)",
            )
        })?;
    Ok(PathBuf::from(home).join(".nexus").join("sudocode"))
}

fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(serde_json::Map::new());
            }
            serde_json::from_str::<Value>(&contents)
                .map_err(|error| error.to_string())?
                .as_object()
                .cloned()
                .ok_or_else(|| String::from("config file must contain a JSON object"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn write_json_object(path: &Path, value: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn get_nested_value<'a>(
    value: &'a serde_json::Map<String, Value>,
    path: &[&str],
) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut current = value.get(*first)?;
    for key in rest {
        current = current.as_object()?.get(*key)?;
    }
    Some(current)
}

fn set_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str], new_value: Value) {
    let (first, rest) = path.split_first().expect("config path must not be empty");
    if rest.is_empty() {
        root.insert((*first).to_string(), new_value);
        return;
    }

    let entry = root
        .entry((*first).to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(serde_json::Map::new());
    }
    let map = entry.as_object_mut().expect("object inserted");
    set_nested_value(map, rest, new_value);
}

fn remove_nested_value(root: &mut serde_json::Map<String, Value>, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return root.remove(*first).is_some();
    }

    let mut should_remove_parent = false;
    let removed = root.get_mut(*first).is_some_and(|entry| {
        entry.as_object_mut().is_some_and(|map| {
            let removed = remove_nested_value(map, rest);
            should_remove_parent = removed && map.is_empty();
            removed
        })
    });

    if should_remove_parent {
        root.remove(*first);
    }

    removed
}

fn plan_mode_state_file() -> Result<PathBuf, String> {
    Ok(config_file_for_scope(ConfigScope::Settings)?
        .parent()
        .ok_or_else(|| String::from("settings.local.json has no parent directory"))?
        .join("tool-state")
        .join("plan-mode.json"))
}

fn read_plan_mode_state(path: &Path) -> Result<Option<PlanModeState>, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(None);
            }
            serde_json::from_str(&contents)
                .map(Some)
                .map_err(|error| error.to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_plan_mode_state(path: &Path, state: &PlanModeState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(state).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn clear_plan_mode_state(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn iso8601_timestamp() -> String {
    if let Ok(output) = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
    }
    iso8601_now()
}

#[allow(clippy::needless_pass_by_value)]
fn execute_powershell(
    input: PowerShellInput,
    abort_signal: Option<&HookAbortSignal>,
) -> std::io::Result<runtime::BashCommandOutput> {
    let _ = &input.description;
    if let Some(output) = workspace_test_branch_preflight(&input.command) {
        return Ok(output);
    }
    let shell = detect_powershell_shell()?;
    execute_shell_command(
        shell,
        &input.command,
        input.timeout,
        input.run_in_background,
        abort_signal,
    )
}

fn detect_powershell_shell() -> std::io::Result<&'static str> {
    if command_exists("pwsh") {
        Ok("pwsh")
    } else if command_exists("powershell") {
        Ok("powershell")
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "PowerShell executable not found (expected `pwsh` or `powershell` in PATH)",
        ))
    }
}

fn command_exists(command: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(command);
                candidate.is_file()
                    || (cfg!(windows) && dir.join(format!("{command}.exe")).is_file())
            })
        })
        .unwrap_or(false)
}

fn execute_shell_command(
    shell: &str,
    command: &str,
    timeout: Option<u64>,
    run_in_background: Option<bool>,
    abort_signal: Option<&HookAbortSignal>,
) -> std::io::Result<runtime::BashCommandOutput> {
    if run_in_background.unwrap_or(false) {
        // Spawn detached but still inside a fresh process group / Job so anyone signalling
        // the leader later reaps the entire descendant tree together.
        let mut process = std::process::Command::new(shell);
        process
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = process.group_spawn()?;
        let pid = child.id();
        drop(child);
        return Ok(runtime::BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(pid.to_string()),
            backgrounded_by_user: Some(true),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: None,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
        });
    }

    let mut process = std::process::Command::new(shell);
    process
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg(command);

    let timeout_ms = timeout.unwrap_or(runtime::DEFAULT_TOOL_SUBPROCESS_TIMEOUT_MS);
    let result = run_in_process_group(&mut process, timeout_ms, abort_signal)?;
    Ok(shell_run_to_bash_output(result, timeout_ms))
}

/// Outcome of polling a `GroupChild` to completion / abort / timeout.
enum ShellOutcome {
    Completed(std::process::ExitStatus),
    Interrupted,
    TimedOut,
}

/// Result of running a command inside a managed process group.
struct ShellRunResult {
    outcome: ShellOutcome,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Spawn `process` inside a fresh process group (Unix) / Job Object (Windows), poll until
/// it completes, the abort signal fires, or `timeout_ms` elapses, then kill the entire
/// group on interrupt/timeout and drain stdio under a hard watchdog.
///
/// `process` must have its stdio configured by the caller; `stdin` is forced to `null`
/// here so a descendant never blocks reading from the parent's protocol pipe (which is
/// what made the old single-process `wait_with_output()` hang on Windows when a
/// grandchild — e.g. `python` spawned by `py` — inherited the inherited handle).
fn run_in_process_group(
    process: &mut std::process::Command,
    timeout_ms: u64,
    abort_signal: Option<&HookAbortSignal>,
) -> std::io::Result<ShellRunResult> {
    process
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // group_spawn places the leader in a new process group / Job Object. A subsequent
    // `kill()` then takes down the entire descendant tree in one shot — the old
    // single-process kill could leave grandchildren running that kept inherited stdout
    // pipes open, hanging the drain forever.
    let mut child = process.group_spawn()?;

    let started = Instant::now();
    let outcome = loop {
        if abort_signal.is_some_and(HookAbortSignal::is_aborted) {
            let _ = child.kill();
            break ShellOutcome::Interrupted;
        }
        if let Some(status) = child.try_wait()? {
            break ShellOutcome::Completed(status);
        }
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            let _ = child.kill();
            break ShellOutcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let drained = drain_group_child(child, drain_budget_for(timeout_ms));
    Ok(ShellRunResult {
        outcome,
        stdout: drained.stdout,
        stderr: drained.stderr,
    })
}

fn shell_run_to_bash_output(result: ShellRunResult, timeout_ms: u64) -> runtime::BashCommandOutput {
    let stdout_text = String::from_utf8_lossy(&result.stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&result.stderr).into_owned();
    let stdio_empty = result.stdout.is_empty() && result.stderr.is_empty();

    match result.outcome {
        ShellOutcome::Completed(status) => runtime::BashCommandOutput {
            stdout: stdout_text,
            stderr: stderr_text,
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: None,
            backgrounded_by_user: None,
            assistant_auto_backgrounded: None,
            dangerously_disable_sandbox: None,
            return_code_interpretation: status
                .code()
                .filter(|code| *code != 0)
                .map(|code| format!("exit_code:{code}")),
            no_output_expected: Some(stdio_empty),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
        },
        ShellOutcome::Interrupted => runtime::BashCommandOutput {
            stdout: stdout_text,
            stderr: append_status_line(&stderr_text, "Command interrupted by user"),
            raw_output_path: None,
            interrupted: true,
            is_image: None,
            background_task_id: None,
            backgrounded_by_user: None,
            assistant_auto_backgrounded: None,
            dangerously_disable_sandbox: None,
            return_code_interpretation: Some(String::from("interrupted")),
            no_output_expected: Some(false),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
        },
        ShellOutcome::TimedOut => runtime::BashCommandOutput {
            stdout: stdout_text,
            stderr: append_status_line(
                &stderr_text,
                &format!("Command exceeded timeout of {timeout_ms} ms"),
            ),
            raw_output_path: None,
            interrupted: true,
            is_image: None,
            background_task_id: None,
            backgrounded_by_user: None,
            assistant_auto_backgrounded: None,
            dangerously_disable_sandbox: None,
            return_code_interpretation: Some(String::from("timeout")),
            no_output_expected: Some(false),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: None,
        },
    }
}

struct DrainedShellOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Drain stdout/stderr from a killed-or-exited `GroupChild` with a hard wall-clock cap.
///
/// The reader threads run on a detached worker so a stuck pipe (e.g. a descendant we did
/// not manage to kill that still holds the write end) cannot pin the calling thread
/// past `budget`. If the watchdog fires, callers get empty stdio rather than a hang.
fn drain_group_child(child: command_group::GroupChild, budget: Duration) -> DrainedShellOutput {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut child = child;
        let stdout_pipe = child.inner().stdout.take();
        let stderr_pipe = child.inner().stderr.take();

        let stdout_handle = stdout_pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
                buf
            })
        });
        let stderr_handle = stderr_pipe.map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
                buf
            })
        });

        // Reap the leader; the polling loop already decided the outcome.
        let _ = child.wait();

        let stdout = stdout_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr = stderr_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let _ = tx.send(DrainedShellOutput { stdout, stderr });
    });

    rx.recv_timeout(budget).unwrap_or(DrainedShellOutput {
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

/// Drain budget for the post-kill stdio cleanup. Capped so even a misbehaved descendant
/// cannot push the total runtime far beyond the requested timeout, but generous enough
/// to absorb normal flush latency on slow platforms.
fn drain_budget_for(timeout_ms: u64) -> Duration {
    const MIN_DRAIN_MS: u64 = 2_000;
    const MAX_DRAIN_MS: u64 = 10_000;
    let derived = timeout_ms / 4;
    Duration::from_millis(derived.clamp(MIN_DRAIN_MS, MAX_DRAIN_MS))
}

fn append_status_line(stderr: &str, status_line: &str) -> String {
    if stderr.trim().is_empty() {
        status_line.to_string()
    } else {
        format!("{}\n{status_line}", stderr.trim_end())
    }
}

fn resolve_cell_index(
    cells: &[serde_json::Value],
    cell_id: Option<&str>,
    edit_mode: NotebookEditMode,
) -> Result<usize, String> {
    if cells.is_empty()
        && matches!(
            edit_mode,
            NotebookEditMode::Replace | NotebookEditMode::Delete
        )
    {
        return Err(String::from("Notebook has no cells to edit"));
    }
    if let Some(cell_id) = cell_id {
        cells
            .iter()
            .position(|cell| cell.get("id").and_then(serde_json::Value::as_str) == Some(cell_id))
            .ok_or_else(|| format!("Cell id not found: {cell_id}"))
    } else {
        Ok(cells.len().saturating_sub(1))
    }
}

fn source_lines(source: &str) -> Vec<serde_json::Value> {
    if source.is_empty() {
        return vec![serde_json::Value::String(String::new())];
    }
    source
        .split_inclusive('\n')
        .map(|line| serde_json::Value::String(line.to_string()))
        .collect()
}

fn format_notebook_edit_mode(mode: NotebookEditMode) -> String {
    match mode {
        NotebookEditMode::Replace => String::from("replace"),
        NotebookEditMode::Insert => String::from("insert"),
        NotebookEditMode::Delete => String::from("delete"),
    }
}

fn make_cell_id(index: usize) -> String {
    format!("cell-{}", index + 1)
}

fn parse_skill_description(contents: &str) -> Option<String> {
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod shell_group_tests {
    use super::{drain_budget_for, run_in_process_group, ShellOutcome};
    use runtime::HookAbortSignal;
    use std::time::{Duration, Instant};

    #[test]
    fn drain_budget_clamps_to_floor_for_tiny_timeouts() {
        // A 100ms timeout still gets at least 2s drain budget so a flushing pipe
        // is not artificially truncated.
        assert_eq!(drain_budget_for(100), Duration::from_millis(2_000));
    }

    #[test]
    fn drain_budget_scales_with_timeout() {
        // 16s timeout -> 4s drain (= timeout / 4).
        assert_eq!(drain_budget_for(16_000), Duration::from_millis(4_000));
    }

    #[test]
    fn drain_budget_caps_at_ceiling() {
        // Very long timeouts cap the drain at 10s — the polling loop already
        // enforced the real deadline; this is just cleanup.
        assert_eq!(drain_budget_for(600_000), Duration::from_millis(10_000));
    }

    fn bash_available() -> bool {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("bash").is_file()))
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[test]
    fn captures_stdout_and_zero_exit() {
        if !bash_available() {
            return;
        }
        let mut process = std::process::Command::new("bash");
        process.arg("-c").arg("printf 'hello-shell-group'");
        let result =
            run_in_process_group(&mut process, 5_000, None).expect("group spawn should succeed");
        assert!(matches!(
            result.outcome,
            ShellOutcome::Completed(status) if status.success()
        ));
        assert_eq!(
            String::from_utf8_lossy(&result.stdout).trim(),
            "hello-shell-group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_command_within_budget() {
        if !bash_available() {
            return;
        }
        let mut process = std::process::Command::new("bash");
        process.arg("-c").arg("sleep 30");
        let started = Instant::now();
        let result =
            run_in_process_group(&mut process, 200, None).expect("group spawn should succeed");
        let elapsed = started.elapsed();
        assert!(matches!(result.outcome, ShellOutcome::TimedOut));
        // Generous ceiling: poll loop is 10ms granularity + drain budget floor (2s).
        assert!(
            elapsed < Duration::from_secs(5),
            "timeout took {elapsed:?}; must respect budget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn abort_signal_interrupts_in_flight_command() {
        if !bash_available() {
            return;
        }
        let signal = HookAbortSignal::new();
        let trigger = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            trigger.abort();
        });
        let mut process = std::process::Command::new("bash");
        process.arg("-c").arg("sleep 30");
        let started = Instant::now();
        let result = run_in_process_group(&mut process, 30_000, Some(&signal))
            .expect("group spawn should succeed");
        let elapsed = started.elapsed();
        assert!(matches!(result.outcome, ShellOutcome::Interrupted));
        assert!(
            elapsed < Duration::from_secs(5),
            "abort took {elapsed:?}; should fire quickly"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_grandchildren_holding_stdio() {
        // This is the regression the command-group adoption is meant to fix:
        // a grandchild (background sleep) inherits stdout. Under the old
        // single-process kill it would survive and keep the pipe open,
        // hanging the drain. With group_spawn the entire tree dies and the
        // drain completes.
        if !bash_available() {
            return;
        }
        let mut process = std::process::Command::new("bash");
        process.arg("-c").arg(
            // Start a backgrounded sleep that survives the shell, then block
            // ourselves so the timeout path is exercised.
            "sleep 30 & disown; sleep 30",
        );
        let started = Instant::now();
        let result =
            run_in_process_group(&mut process, 200, None).expect("group spawn should succeed");
        let elapsed = started.elapsed();
        assert!(matches!(result.outcome, ShellOutcome::TimedOut));
        assert!(
            elapsed < Duration::from_secs(5),
            "grandchild kept us alive for {elapsed:?}; group kill should have reaped it"
        );
    }
}

pub mod lane_completion;
pub mod pdf_extract;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;

    use super::{
        agent_permission_policy, allowed_tools_for_subagent, auto_background_threshold,
        await_agent_output, build_agent_system_prompt, classify_lane_failure, derive_agent_state,
        execute_agent_inline_with_work, execute_agent_with_spawn, execute_tool,
        extract_recovery_outcome, final_assistant_text, global_cron_registry, lookup_custom_agent,
        maybe_commit_provenance, mvp_tool_specs, normalize_subagent_type,
        permission_mode_from_plugin, persist_agent_terminal_state, push_output_block,
        run_ask_user_question_v2, sweep_orphaned_tmp_files, AgentInput, AgentJob,
        AskUserQuestionInput, AskUserQuestionItem, AskUserQuestionOption, GlobalToolRegistry,
        LaneEventName, LaneFailureClass, SubagentToolExecutor,
    };
    use api::OutputContentBlock;
    use runtime::{
        permission_enforcer::PermissionEnforcer, ApiRequest, AssistantEvent, ConversationRuntime,
        PermissionMode, PermissionPolicy, RuntimeError, Session, SystemPrompt, ToolExecutor,
    };
    use serde_json::json;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn env_guard_recovers_after_poisoning() {
        let poisoned = std::thread::spawn(|| {
            let _guard = env_guard();
            panic!("poison env lock");
        })
        .join();
        assert!(poisoned.is_err(), "poisoning thread should panic");

        let _guard = env_guard();
    }

    fn temp_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir().join(format!("sudocode-tools-{unique}-{name}"))
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap_or_else(|error| panic!("git {} failed: {error}", args.join(" ")));
        assert!(
            status.success(),
            "git {} exited with {status}",
            args.join(" ")
        );
    }

    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path).expect("create repo");
        run_git(path, &["init", "--quiet", "-b", "main"]);
        run_git(path, &["config", "user.email", "tests@example.com"]);
        run_git(path, &["config", "user.name", "Tools Tests"]);
        std::fs::write(path.join("README.md"), "initial\n").expect("write readme");
        run_git(path, &["add", "README.md"]);
        run_git(path, &["commit", "-m", "initial commit", "--quiet"]);
    }

    fn commit_file(path: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(path.join(file), contents).expect("write file");
        run_git(path, &["add", file]);
        run_git(path, &["commit", "-m", message, "--quiet"]);
    }

    fn permission_policy_for_mode(mode: PermissionMode) -> PermissionPolicy {
        mvp_tool_specs()
            .into_iter()
            .fold(PermissionPolicy::new(mode), |policy, spec| {
                policy.with_tool_requirement(spec.name, spec.required_permission)
            })
    }

    #[test]
    fn exposes_mvp_tools() {
        let names = mvp_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"WebFetch"));
        assert!(names.contains(&"WebSearch"));
        assert!(names.contains(&"TodoWrite"));
        assert!(names.contains(&"Skill"));
        assert!(names.contains(&"Agent"));
        assert!(names.contains(&"ToolSearch"));
        assert!(names.contains(&"NotebookEdit"));
        assert!(names.contains(&"Sleep"));
        assert!(names.contains(&"SendUserMessage"));
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"EnterPlanMode"));
        assert!(names.contains(&"ExitPlanMode"));
        assert!(names.contains(&"StructuredOutput"));
        assert!(names.contains(&"REPL"));
        assert!(names.contains(&"PowerShell"));
    }

    #[test]
    fn rejects_unknown_tool_names() {
        let error = execute_tool("nope", &json!({})).expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool"));
    }

    #[test]
    fn global_tool_registry_denies_blocked_tool_before_dispatch() {
        // given
        let policy = permission_policy_for_mode(PermissionMode::ReadOnly);
        let registry = GlobalToolRegistry::builtin().with_enforcer(PermissionEnforcer::new(policy));

        // when
        let error = registry
            .execute(
                "write_file",
                &json!({
                    "path": "blocked.txt",
                    "content": "blocked"
                }),
            )
            .expect_err("write tool should be denied before dispatch");

        // then
        assert!(error.contains("requires workspace-write permission"));
    }

    #[test]
    fn subagent_tool_executor_denies_blocked_tool_before_dispatch() {
        // given
        let policy = permission_policy_for_mode(PermissionMode::ReadOnly);
        let mut executor = SubagentToolExecutor::new(BTreeSet::from([String::from("write_file")]))
            .with_enforcer(PermissionEnforcer::new(policy));

        // when
        let error = executor
            .execute(
                "write_file",
                &json!({
                    "path": "blocked.txt",
                    "content": "blocked"
                })
                .to_string(),
            )
            .expect_err("subagent write tool should be denied before dispatch");

        // then
        assert!(error
            .to_string()
            .contains("requires workspace-write permission"));
    }

    #[test]
    fn permission_mode_from_plugin_rejects_invalid_inputs() {
        let unknown_permission = permission_mode_from_plugin("admin")
            .expect_err("unknown plugin permission should fail");
        assert!(unknown_permission.contains("unsupported plugin permission: admin"));

        let empty_permission =
            permission_mode_from_plugin("").expect_err("empty plugin permission should fail");
        assert!(empty_permission.contains("unsupported plugin permission: "));
    }

    #[test]
    fn runtime_tools_extend_registry_definitions_permissions_and_search() {
        let registry = GlobalToolRegistry::builtin()
            .with_runtime_tools(vec![super::RuntimeToolDefinition {
                name: "mcp__demo__echo".to_string(),
                description: Some("Echo text from the demo MCP server".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "additionalProperties": false
                }),
                required_permission: runtime::PermissionMode::ReadOnly,
            }])
            .expect("runtime tools should register");

        let allowed = registry
            .normalize_allowed_tools(&["mcp__demo__echo".to_string()])
            .expect("runtime tool should be allow-listable")
            .expect("allow-list should be populated");
        assert!(allowed.contains("mcp__demo__echo"));

        let definitions = registry.definitions(Some(&allowed));
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "mcp__demo__echo");

        let permissions = registry
            .permission_specs(Some(&allowed))
            .expect("runtime tool permissions should resolve");
        assert_eq!(
            permissions,
            vec![(
                "mcp__demo__echo".to_string(),
                runtime::PermissionMode::ReadOnly
            )]
        );

        let search = registry.search(
            "demo echo",
            5,
            Some(vec!["pending-server".to_string()]),
            Some(runtime::McpDegradedReport::new(
                vec!["demo".to_string()],
                vec![runtime::McpFailedServer {
                    server_name: "pending-server".to_string(),
                    phase: runtime::McpLifecyclePhase::ToolDiscovery,
                    error: runtime::McpErrorSurface::new(
                        runtime::McpLifecyclePhase::ToolDiscovery,
                        Some("pending-server".to_string()),
                        "tool discovery failed",
                        BTreeMap::new(),
                        true,
                    ),
                }],
                vec!["mcp__demo__echo".to_string()],
                vec!["mcp__demo__echo".to_string()],
            )),
        );
        let output = serde_json::to_value(search).expect("search output should serialize");
        assert_eq!(output["matches"][0], "mcp__demo__echo");
        assert_eq!(output["pending_mcp_servers"][0], "pending-server");
        assert_eq!(
            output["mcp_degraded"]["failed_servers"][0]["phase"],
            "tool_discovery"
        );
    }

    #[test]
    fn web_fetch_returns_prompt_aware_summary() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /page "));
            HttpResponse::html(
                200,
                "OK",
                "<html><head><title>Ignored</title></head><body><h1>Test Page</h1><p>Hello <b>world</b> from local server.</p></body></html>",
            )
        }));

        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/page", server.addr()),
                "prompt": "Summarize this page"
            }),
        )
        .expect("WebFetch should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["code"], 200);
        let summary = output["result"].as_str().expect("result string");
        assert!(summary.contains("Fetched"));
        assert!(summary.contains("Test Page"));
        assert!(summary.contains("Hello world from local server"));

        let titled = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/page", server.addr()),
                "prompt": "What is the page title?"
            }),
        )
        .expect("WebFetch title query should succeed");
        let titled_output: serde_json::Value = serde_json::from_str(&titled).expect("valid json");
        let titled_summary = titled_output["result"].as_str().expect("result string");
        assert!(titled_summary.contains("Title: Ignored"));
    }

    #[test]
    fn web_fetch_supports_plain_text_and_rejects_invalid_url() {
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.starts_with("GET /plain "));
            HttpResponse::text(200, "OK", "plain text response")
        }));

        let result = execute_tool(
            "WebFetch",
            &json!({
                "url": format!("http://{}/plain", server.addr()),
                "prompt": "Show me the content"
            }),
        )
        .expect("WebFetch should succeed for text content");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["url"], format!("http://{}/plain", server.addr()));
        assert!(output["result"]
            .as_str()
            .expect("result")
            .contains("plain text response"));

        let error = execute_tool(
            "WebFetch",
            &json!({
                "url": "not a url",
                "prompt": "Summarize"
            }),
        )
        .expect_err("invalid URL should fail");
        assert!(error.contains("relative URL without a base") || error.contains("invalid"));
    }

    #[test]
    fn web_search_extracts_and_filters_results() {
        // Serialize env-var mutation so this test cannot race with the sibling
        // web_search_handles_generic_links_and_invalid_base_url test that also
        // sets SUDOCODE_WEB_SEARCH_BASE_URL. Without the lock, parallel test
        // runners can interleave the set/remove calls and cause assertion
        // failures on the wrong port.
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.contains("GET /search?q=rust+web+search "));
            HttpResponse::html(
                200,
                "OK",
                r#"
                <html><body>
                  <a class="result__a" href="https://docs.rs/reqwest">Reqwest docs</a>
                  <a class="result__a" href="https://example.com/blocked">Blocked result</a>
                </body></html>
                "#,
            )
        }));

        std::env::set_var(
            "SUDOCODE_WEB_SEARCH_BASE_URL",
            format!("http://{}/search", server.addr()),
        );
        std::env::set_var("SUDOCODE_WEB_SEARCH_PROVIDER", "duckduckgo");
        let result = execute_tool(
            "WebSearch",
            &json!({
                "query": "rust web search",
                "allowed_domains": ["https://DOCS.rs/"],
                "blocked_domains": ["HTTPS://EXAMPLE.COM"]
            }),
        )
        .expect("WebSearch should succeed");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_BASE_URL");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_PROVIDER");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["query"], "rust web search");
        let results = output["results"].as_array().expect("results array");
        let search_result = results
            .iter()
            .find(|item| item.get("content").is_some())
            .expect("search result block present");
        let content = search_result["content"].as_array().expect("content array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["title"], "Reqwest docs");
        assert_eq!(content[0]["url"], "https://docs.rs/reqwest");
    }

    #[test]
    fn web_search_handles_generic_links_and_invalid_base_url() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = TestServer::spawn(Arc::new(|request_line: &str| {
            assert!(request_line.contains("GET /fallback?q=generic+links "));
            HttpResponse::html(
                200,
                "OK",
                r#"
                <html><body>
                  <a href="https://example.com/one">Example One</a>
                  <a href="https://example.com/one">Duplicate Example One</a>
                  <a href="https://docs.rs/tokio">Tokio Docs</a>
                </body></html>
                "#,
            )
        }));

        std::env::set_var(
            "SUDOCODE_WEB_SEARCH_BASE_URL",
            format!("http://{}/fallback", server.addr()),
        );
        std::env::set_var("SUDOCODE_WEB_SEARCH_PROVIDER", "duckduckgo");
        let result = execute_tool(
            "WebSearch",
            &json!({
                "query": "generic links"
            }),
        )
        .expect("WebSearch fallback parsing should succeed");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_BASE_URL");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_PROVIDER");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        let results = output["results"].as_array().expect("results array");
        let search_result = results
            .iter()
            .find(|item| item.get("content").is_some())
            .expect("search result block present");
        let content = search_result["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["url"], "https://example.com/one");
        assert_eq!(content[1]["url"], "https://docs.rs/tokio");

        std::env::set_var("SUDOCODE_WEB_SEARCH_BASE_URL", "://bad-base-url");
        std::env::set_var("SUDOCODE_WEB_SEARCH_PROVIDER", "duckduckgo");
        let error = execute_tool("WebSearch", &json!({ "query": "generic links" }))
            .expect_err("invalid base URL should fail");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_BASE_URL");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_PROVIDER");
        assert!(error.contains("relative URL without a base") || error.contains("empty host"));
    }

    #[test]
    fn web_search_tavily_returns_results_with_snippets() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let server = TestServer::spawn(Arc::new(|_request_line: &str| {
            HttpResponse::json(
                200,
                "OK",
                r#"{
                    "query": "rust async runtime",
                    "results": [
                        {
                            "title": "Tokio - An async runtime for Rust",
                            "url": "https://tokio.rs",
                            "content": "Tokio is an asynchronous runtime for Rust.",
                            "score": 0.95
                        },
                        {
                            "title": "async-std documentation",
                            "url": "https://async.rs",
                            "content": "async-std is an async version of the Rust standard library.",
                            "score": 0.87
                        }
                    ]
                }"#,
            )
        }));

        std::env::set_var("SUDOCODE_WEB_SEARCH_PROVIDER", "tavily");
        std::env::set_var("SUDOCODE_TAVILY_API_KEY", "test-key-123");
        std::env::set_var(
            "SUDOCODE_TAVILY_API_URL",
            format!("http://{}/search", server.addr()),
        );
        // Ensure DDG base URL is not set so it doesn't interfere
        std::env::remove_var("SUDOCODE_WEB_SEARCH_BASE_URL");

        let result = execute_tool("WebSearch", &json!({ "query": "rust async runtime" }))
            .expect("WebSearch with Tavily should succeed");

        std::env::remove_var("SUDOCODE_WEB_SEARCH_PROVIDER");
        std::env::remove_var("SUDOCODE_TAVILY_API_KEY");
        std::env::remove_var("SUDOCODE_TAVILY_API_URL");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["query"], "rust async runtime");
        let results = output["results"].as_array().expect("results array");
        let search_result = results
            .iter()
            .find(|item| item.get("content").is_some())
            .expect("search result block present");
        let content = search_result["content"].as_array().expect("content array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["title"], "Tokio - An async runtime for Rust");
        assert_eq!(content[0]["url"], "https://tokio.rs");
        assert_eq!(
            content[0]["snippet"],
            "Tokio is an asynchronous runtime for Rust."
        );
        assert_eq!(content[1]["title"], "async-std documentation");
        assert_eq!(
            content[1]["snippet"],
            "async-std is an async version of the Rust standard library."
        );
    }

    #[test]
    fn web_search_tavily_missing_api_key_returns_error() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        std::env::set_var("SUDOCODE_WEB_SEARCH_PROVIDER", "tavily");
        std::env::set_var("SUDOCODE_TAVILY_API_KEY", "<PLACEHOLDER>");
        std::env::remove_var("SUDOCODE_WEB_SEARCH_BASE_URL");

        let error = execute_tool("WebSearch", &json!({ "query": "test" }))
            .expect_err("should fail with placeholder API key");

        std::env::remove_var("SUDOCODE_WEB_SEARCH_PROVIDER");
        std::env::remove_var("SUDOCODE_TAVILY_API_KEY");

        assert!(
            error.contains("apiKey") || error.contains("API key"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn pending_tools_preserve_multiple_streaming_tool_calls_by_index() {
        let mut events = Vec::new();
        let mut pending_tools = BTreeMap::new();

        push_output_block(
            OutputContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: json!({}),
                thought_signature: None,
            },
            1,
            &mut events,
            &mut pending_tools,
            true,
        );
        push_output_block(
            OutputContentBlock::ToolUse {
                id: "tool-2".to_string(),
                name: "grep_search".to_string(),
                input: json!({}),
                thought_signature: None,
            },
            2,
            &mut events,
            &mut pending_tools,
            true,
        );

        pending_tools
            .get_mut(&1)
            .expect("first tool pending")
            .2
            .push_str("{\"path\":\"src/main.rs\"}");
        pending_tools
            .get_mut(&2)
            .expect("second tool pending")
            .2
            .push_str("{\"pattern\":\"TODO\"}");

        assert_eq!(
            pending_tools.remove(&1),
            Some((
                "tool-1".to_string(),
                "read_file".to_string(),
                "{\"path\":\"src/main.rs\"}".to_string(),
                None,
            ))
        );
        assert_eq!(
            pending_tools.remove(&2),
            Some((
                "tool-2".to_string(),
                "grep_search".to_string(),
                "{\"pattern\":\"TODO\"}".to_string(),
                None,
            ))
        );
    }

    #[test]
    fn todo_write_persists_and_returns_previous_state() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("todos.json");
        std::env::set_var("SUDOCODE_TODO_STORE", &path);

        let first = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Add tool", "activeForm": "Adding tool", "status": "in_progress"},
                    {"content": "Run tests", "activeForm": "Running tests", "status": "pending"}
                ]
            }),
        )
        .expect("TodoWrite should succeed");
        let first_output: serde_json::Value = serde_json::from_str(&first).expect("valid json");
        assert_eq!(first_output["oldTodos"].as_array().expect("array").len(), 0);

        let second = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Add tool", "activeForm": "Adding tool", "status": "completed"},
                    {"content": "Run tests", "activeForm": "Running tests", "status": "completed"},
                    {"content": "Verify", "activeForm": "Verifying", "status": "completed"}
                ]
            }),
        )
        .expect("TodoWrite should succeed");
        std::env::remove_var("SUDOCODE_TODO_STORE");
        let _ = std::fs::remove_file(path);

        let second_output: serde_json::Value = serde_json::from_str(&second).expect("valid json");
        assert_eq!(
            second_output["oldTodos"].as_array().expect("array").len(),
            2
        );
        assert_eq!(
            second_output["newTodos"].as_array().expect("array").len(),
            3
        );
        assert!(second_output["verificationNudgeNeeded"].is_null());
    }

    #[test]
    fn todo_write_rejects_invalid_payloads_and_sets_verification_nudge() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("todos-errors.json");
        std::env::set_var("SUDOCODE_TODO_STORE", &path);

        let empty = execute_tool("TodoWrite", &json!({ "todos": [] }))
            .expect_err("empty todos should fail");
        assert!(empty.contains("todos must not be empty"));

        // Multiple in_progress items are now allowed for parallel workflows
        let _multi_active = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "One", "activeForm": "Doing one", "status": "in_progress"},
                    {"content": "Two", "activeForm": "Doing two", "status": "in_progress"}
                ]
            }),
        )
        .expect("multiple in-progress todos should succeed");

        let blank_content = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "   ", "activeForm": "Doing it", "status": "pending"}
                ]
            }),
        )
        .expect_err("blank content should fail");
        assert!(blank_content.contains("todo content must not be empty"));

        let nudge = execute_tool(
            "TodoWrite",
            &json!({
                "todos": [
                    {"content": "Write tests", "activeForm": "Writing tests", "status": "completed"},
                    {"content": "Fix errors", "activeForm": "Fixing errors", "status": "completed"},
                    {"content": "Ship branch", "activeForm": "Shipping branch", "status": "completed"}
                ]
            }),
        )
        .expect("completed todos should succeed");
        std::env::remove_var("SUDOCODE_TODO_STORE");
        let _ = fs::remove_file(path);

        let output: serde_json::Value = serde_json::from_str(&nudge).expect("valid json");
        assert_eq!(output["verificationNudgeNeeded"], true);
    }

    #[test]
    fn skill_loads_local_skill_prompt() {
        let _guard = env_guard();
        let home = temp_path("skills-home");
        let skill_dir = home.join(".agents").join("skills").join("help");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "# help\n\nGuide on using SudoCode plugin\n",
        )
        .expect("skill file should exist");
        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);

        let result = execute_tool(
            "Skill",
            &json!({
                "skill": "help",
                "args": "overview"
            }),
        )
        .expect("Skill should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(output["skill"], "help");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("/help/SKILL.md"));
        assert!(output["prompt"]
            .as_str()
            .expect("prompt")
            .contains("Guide on using SudoCode plugin"));

        let dollar_result = execute_tool(
            "Skill",
            &json!({
                "skill": "$help"
            }),
        )
        .expect("Skill should accept $skill invocation form");
        let dollar_output: serde_json::Value =
            serde_json::from_str(&dollar_result).expect("valid json");
        assert_eq!(dollar_output["skill"], "$help");
        assert!(dollar_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("/help/SKILL.md"));

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
        fs::remove_dir_all(home).expect("temp home should clean up");
    }

    #[test]
    fn skill_resolves_project_local_skills_and_legacy_commands() {
        let _guard = env_guard();
        let root = temp_path("project-skills");
        let skill_dir = root
            .join(".nexus")
            .join("sudocode")
            .join("skills")
            .join("plan");
        let command_dir = root.join(".nexus").join("sudocode").join("commands");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::create_dir_all(&command_dir).expect("command dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: plan\ndescription: Project planning guidance\n---\n\n# plan\n",
        )
        .expect("skill file should exist");
        fs::write(
            command_dir.join("handoff.md"),
            "---\nname: handoff\ndescription: Legacy handoff guidance\n---\n\n# handoff\n",
        )
        .expect("command file should exist");

        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        let skill_result = execute_tool("Skill", &json!({ "skill": "$plan" }))
            .expect("project-local skill should resolve");
        let skill_output: serde_json::Value =
            serde_json::from_str(&skill_result).expect("valid json");
        assert!(skill_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with(".nexus/sudocode/skills/plan/SKILL.md"));

        let command_result = execute_tool("Skill", &json!({ "skill": "/handoff" }))
            .expect("legacy command should resolve");
        let command_output: serde_json::Value =
            serde_json::from_str(&command_result).expect("valid json");
        assert!(command_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with(".nexus/sudocode/commands/handoff.md"));

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        fs::remove_dir_all(root).expect("temp project should clean up");
    }

    #[test]
    fn skill_tool_resolves_enabled_plugin_skills() {
        let _guard = env_guard();
        let root = temp_path("plugin-skill-tool");
        let home = root.join("home");
        let config_home = root.join("config");
        let workspace = root.join("workspace");
        let plugin_root = config_home
            .join("plugins")
            .join("installed")
            .join("skill-plugin");
        let skill_dir = plugin_root.join("skills").join("plugin-plan");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::create_dir_all(&workspace).expect("workspace should exist");
        fs::create_dir_all(plugin_root.join(".sudocode-plugin"))
            .expect("manifest dir should exist");
        fs::write(
            plugin_root.join(".sudocode-plugin").join("plugin.json"),
            r#"{
  "name": "skill-plugin",
  "version": "1.0.0",
  "description": "Plugin skill fixture",
  "skills": "./skills"
}"#,
        )
        .expect("manifest should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: plugin-plan\ndescription: Plugin planning guidance\n---\n# plugin-plan\n",
        )
        .expect("skill file should exist");
        fs::create_dir_all(&config_home).expect("config home should exist");
        fs::write(
            config_home.join("settings.json"),
            r#"{"plugins":{"enabled":{"skill-plugin@external":true}}}"#,
        )
        .expect("settings should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &config_home);
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::set_current_dir(&workspace).expect("set cwd");

        let result = execute_tool("Skill", &json!({ "skill": "$plugin-plan" }))
            .expect("plugin skill should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        let path = output["path"].as_str().expect("path");
        assert_eq!(
            fs::canonicalize(path).expect("resolved skill path should exist"),
            fs::canonicalize(skill_dir.join("SKILL.md")).expect("expected skill path should exist")
        );
        assert_eq!(output["description"], "Plugin planning guidance");

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_claude_config_dir {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_project_local_claude_skill_prompt() {
        let _guard = env_guard();
        let root = temp_path("project-skills");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let skill_dir = workspace.join(".claude").join("skills").join("trace");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::create_dir_all(&nested).expect("nested cwd should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: trace\ndescription: Project-local trace helper\n---\n# trace\n",
        )
        .expect("skill file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_current_dir(&nested).expect("set cwd");

        let result = execute_tool("Skill", &json!({ "skill": "trace" }))
            .expect("project-local skill should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with(".claude/skills/trace/SKILL.md"));
        assert_eq!(output["description"], "Project-local trace helper");

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_project_local_omc_and_agents_skill_prompts() {
        let _guard = env_guard();
        let root = temp_path("project-omc-skills");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let omc_skill_dir = workspace.join(".omc").join("skills").join("hud");
        let agents_skill_dir = workspace.join(".agents").join("skills").join("trace");
        fs::create_dir_all(&omc_skill_dir).expect("omc skill dir should exist");
        fs::create_dir_all(&agents_skill_dir).expect("agents skill dir should exist");
        fs::create_dir_all(&nested).expect("nested cwd should exist");
        fs::write(
            omc_skill_dir.join("SKILL.md"),
            "---\nname: hud\ndescription: Project-local OMC HUD helper\n---\n# hud\n",
        )
        .expect("omc skill file should exist");
        fs::write(
            agents_skill_dir.join("SKILL.md"),
            "---\nname: trace\ndescription: Project-local agents compatibility helper\n---\n# trace\n",
        )
        .expect("agents skill file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_current_dir(&nested).expect("set cwd");

        let omc_result =
            execute_tool("Skill", &json!({ "skill": "hud" })).expect("omc skill should resolve");
        let agents_result = execute_tool("Skill", &json!({ "skill": "trace" }))
            .expect("agents skill should resolve");

        let omc_output: serde_json::Value = serde_json::from_str(&omc_result).expect("valid json");
        let agents_output: serde_json::Value =
            serde_json::from_str(&agents_result).expect("valid json");
        assert!(omc_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with(".omc/skills/hud/SKILL.md"));
        assert_eq!(omc_output["description"], "Project-local OMC HUD helper");
        assert!(agents_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with(".agents/skills/trace/SKILL.md"));
        assert_eq!(
            agents_output["description"],
            "Project-local agents compatibility helper"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_learned_skill_from_claude_config_dir() {
        let _guard = env_guard();
        let root = temp_path("claude-config-learned-skill");
        let home = root.join("home");
        let claude_config_dir = root.join("claude-config");
        let learned_skill_dir = claude_config_dir
            .join("skills")
            .join("omc-learned")
            .join("learned");
        fs::create_dir_all(&learned_skill_dir).expect("learned skill dir should exist");
        fs::write(
            learned_skill_dir.join("SKILL.md"),
            "---\nname: learned\ndescription: Learned OMC skill\n---\n# learned\n",
        )
        .expect("learned skill file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

        let result = execute_tool("Skill", &json!({ "skill": "learned" }))
            .expect("learned skill should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("skills/omc-learned/learned/SKILL.md"));
        assert_eq!(output["description"], "Learned OMC skill");

        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_claude_config_dir {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_direct_skill_and_legacy_command_from_claude_config_dir() {
        let _guard = env_guard();
        let root = temp_path("claude-config-direct-skill");
        let home = root.join("home");
        let claude_config_dir = root.join("claude-config");
        let skill_dir = claude_config_dir.join("skills").join("statusline");
        let command_dir = claude_config_dir.join("commands");
        fs::create_dir_all(&skill_dir).expect("direct skill dir should exist");
        fs::create_dir_all(&command_dir).expect("command dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: statusline\ndescription: Claude config skill\n---\n# statusline\n",
        )
        .expect("direct skill file should exist");
        fs::write(
            command_dir.join("doctor-check.md"),
            "---\nname: doctor-check\ndescription: Claude config command\n---\n# doctor-check\n",
        )
        .expect("direct command file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_claude_config_dir = std::env::var("CLAUDE_CONFIG_DIR").ok();
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);

        let direct_skill =
            execute_tool("Skill", &json!({ "skill": "statusline" })).expect("direct skill");
        let direct_skill_output: serde_json::Value =
            serde_json::from_str(&direct_skill).expect("valid skill json");
        assert!(direct_skill_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("skills/statusline/SKILL.md"));
        assert_eq!(direct_skill_output["description"], "Claude config skill");

        let legacy_command =
            execute_tool("Skill", &json!({ "skill": "doctor-check" })).expect("direct command");
        let legacy_command_output: serde_json::Value =
            serde_json::from_str(&legacy_command).expect("valid command json");
        assert!(legacy_command_output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with("commands/doctor-check.md"));
        assert_eq!(
            legacy_command_output["description"],
            "Claude config command"
        );

        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        match original_claude_config_dir {
            Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn skill_loads_project_local_legacy_command_markdown() {
        let _guard = env_guard();
        let root = temp_path("project-legacy-command");
        let home = root.join("home");
        let workspace = root.join("workspace");
        let nested = workspace.join("nested");
        let command_dir = workspace.join(".claude").join("commands");
        fs::create_dir_all(&command_dir).expect("legacy command dir should exist");
        fs::create_dir_all(&nested).expect("nested cwd should exist");
        fs::write(
            command_dir.join("team.md"),
            "---\nname: team\ndescription: Legacy team workflow\n---\n# team\n",
        )
        .expect("legacy command file should exist");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_codex_home = std::env::var("CODEX_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::set_current_dir(&nested).expect("set cwd");

        let result = execute_tool("Skill", &json!({ "skill": "team" }))
            .expect("legacy command markdown should resolve");

        let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert!(output["path"]
            .as_str()
            .expect("path")
            .replace('\\', "/")
            .ends_with(".claude/commands/team.md"));
        assert_eq!(output["description"], "Legacy team workflow");

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        match original_codex_home {
            Some(value) => std::env::set_var("CODEX_HOME", value),
            None => std::env::remove_var("CODEX_HOME"),
        }
        fs::remove_dir_all(root).expect("temp tree should clean up");
    }

    #[test]
    fn tool_search_supports_keyword_and_select_queries() {
        let keyword = execute_tool(
            "ToolSearch",
            &json!({"query": "web current", "max_results": 3}),
        )
        .expect("ToolSearch should succeed");
        let keyword_output: serde_json::Value = serde_json::from_str(&keyword).expect("valid json");
        let matches = keyword_output["matches"].as_array().expect("matches");
        assert!(matches.iter().any(|value| value == "WebSearch"));

        let selected = execute_tool("ToolSearch", &json!({"query": "select:Agent,Skill"}))
            .expect("ToolSearch should succeed");
        let selected_output: serde_json::Value =
            serde_json::from_str(&selected).expect("valid json");
        assert_eq!(selected_output["matches"][0], "Agent");
        assert_eq!(selected_output["matches"][1], "Skill");

        let aliased = execute_tool("ToolSearch", &json!({"query": "AgentTool"}))
            .expect("ToolSearch should support tool aliases");
        let aliased_output: serde_json::Value = serde_json::from_str(&aliased).expect("valid json");
        assert_eq!(aliased_output["matches"][0], "Agent");
        assert_eq!(aliased_output["normalized_query"], "agent");

        let selected_with_alias =
            execute_tool("ToolSearch", &json!({"query": "select:AgentTool,Skill"}))
                .expect("ToolSearch alias select should succeed");
        let selected_with_alias_output: serde_json::Value =
            serde_json::from_str(&selected_with_alias).expect("valid json");
        assert_eq!(selected_with_alias_output["matches"][0], "Agent");
        assert_eq!(selected_with_alias_output["matches"][1], "Skill");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn agent_persists_handoff_metadata() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_path("agent-store");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);
        let captured = Arc::new(Mutex::new(None::<AgentJob>));
        let captured_for_spawn = Arc::clone(&captured);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Audit the branch".to_string(),
                prompt: "Check tests and outstanding work.".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("ship-audit".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            move |job| {
                *captured_for_spawn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job);
                Ok(())
            },
        )
        .expect("Agent should succeed");
        std::env::remove_var("SUDOCODE_AGENT_STORE");

        assert_eq!(manifest.name, "ship-audit");
        assert_eq!(manifest.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(manifest.status, "running");
        assert!(!manifest.created_at.is_empty());
        assert!(manifest.started_at.is_some());
        assert!(manifest.completed_at.is_none());
        let contents = std::fs::read_to_string(&manifest.output_file).expect("agent file exists");
        let manifest_contents =
            std::fs::read_to_string(&manifest.manifest_file).expect("manifest file exists");
        let manifest_json: serde_json::Value =
            serde_json::from_str(&manifest_contents).expect("manifest should be valid json");
        assert!(contents.contains("Audit the branch"));
        assert!(contents.contains("Check tests and outstanding work."));
        assert!(manifest_contents.contains("\"subagentType\": \"Explore\""));
        assert!(manifest_contents.contains("\"status\": \"running\""));
        assert_eq!(manifest_json["laneEvents"][0]["event"], "lane.started");
        assert_eq!(manifest_json["laneEvents"][0]["status"], "running");
        assert!(manifest_json["currentBlocker"].is_null());
        let captured_job = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("spawn job should be captured");
        assert_eq!(captured_job.prompt, "Check tests and outstanding work.");
        assert!(captured_job.allowed_tools.contains("read_file"));
        assert!(!captured_job.allowed_tools.contains("Agent"));

        let normalized = execute_tool(
            "Agent",
            &json!({
                "description": "Verify the branch",
                "prompt": "Check tests.",
                "subagent_type": "explorer"
            }),
        )
        .expect("Agent should normalize built-in aliases");
        let normalized_output: serde_json::Value =
            serde_json::from_str(&normalized).expect("valid json");
        assert_eq!(normalized_output["subagentType"], "Explore");

        let named = execute_tool(
            "Agent",
            &json!({
                "description": "Review the branch",
                "prompt": "Inspect diff.",
                "name": "Ship Audit!!!"
            }),
        )
        .expect("Agent should normalize explicit names");
        let named_output: serde_json::Value = serde_json::from_str(&named).expect("valid json");
        assert_eq!(named_output["name"], "ship-audit");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn agent_fake_runner_can_persist_completion_and_failure() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_path("agent-runner");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);
        // Cron is now persistent; point its dedicated store env at this temp
        // dir so the auto-disable assertions stay hermetic and never touch the
        // developer's real ~/.nexus/sudocode/crons.json. Using the cron-only
        // SUDOCODE_CRON_STORE (not SUDO_CODE_CONFIG_HOME) keeps this isolation
        // from perturbing other tests that read the config home.
        std::env::set_var("SUDOCODE_CRON_STORE", dir.join("crons.json"));

        let completed = execute_agent_with_spawn(
            AgentInput {
                description: "Complete the task".to_string(),
                prompt: "Do the work".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("complete-task".to_string()),
                model: Some("claude-sonnet-4-6".to_string()),
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some("Finished successfully in commit abc1234"),
                    None,
                )
            },
        )
        .expect("completed agent should succeed");

        let completed_manifest = std::fs::read_to_string(&completed.manifest_file)
            .expect("completed manifest should exist");
        let completed_manifest_json: serde_json::Value =
            serde_json::from_str(&completed_manifest).expect("completed manifest json");
        let completed_output =
            std::fs::read_to_string(&completed.output_file).expect("completed output should exist");
        assert!(completed_manifest.contains("\"status\": \"completed\""));
        assert!(completed_output.contains("Finished successfully"));
        assert_eq!(
            completed_manifest_json["laneEvents"][0]["event"],
            "lane.started"
        );
        assert_eq!(
            completed_manifest_json["laneEvents"][1]["event"],
            "lane.finished"
        );
        assert_eq!(
            completed_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
            false
        );
        assert_eq!(
            completed_manifest_json["laneEvents"][1]["detail"],
            "Finished successfully in commit abc1234"
        );
        assert_eq!(
            completed_manifest_json["laneEvents"][2]["event"],
            "lane.commit.created"
        );
        assert_eq!(
            completed_manifest_json["laneEvents"][2]["data"]["commit"],
            "abc1234"
        );
        assert!(completed_manifest_json["currentBlocker"].is_null());
        assert_eq!(
            completed_manifest_json["derivedState"],
            "finished_cleanable"
        );

        let failed = execute_agent_with_spawn(
            AgentInput {
                description: "Fail the task".to_string(),
                prompt: "Do the failing work".to_string(),
                subagent_type: Some("Verification".to_string()),
                name: Some("fail-task".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "failed",
                    None,
                    Some(String::from("tool failed: simulated failure")),
                )
            },
        )
        .expect("failed agent should still spawn");

        let failed_manifest =
            std::fs::read_to_string(&failed.manifest_file).expect("failed manifest should exist");
        let failed_manifest_json: serde_json::Value =
            serde_json::from_str(&failed_manifest).expect("failed manifest json");
        let failed_output =
            std::fs::read_to_string(&failed.output_file).expect("failed output should exist");
        assert!(failed_manifest.contains("\"status\": \"failed\""));
        assert!(failed_manifest.contains("simulated failure"));
        assert!(failed_output.contains("simulated failure"));
        assert!(failed_output.contains("failure_class: tool_runtime"));
        assert_eq!(
            failed_manifest_json["currentBlocker"]["failureClass"],
            "tool_runtime"
        );
        assert_eq!(
            failed_manifest_json["laneEvents"][1]["event"],
            "lane.blocked"
        );
        assert_eq!(
            failed_manifest_json["laneEvents"][2]["event"],
            "lane.failed"
        );
        assert_eq!(
            failed_manifest_json["laneEvents"][2]["failureClass"],
            "tool_runtime"
        );
        assert_eq!(failed_manifest_json["derivedState"], "truly_idle");

        let normalized = execute_agent_with_spawn(
            AgentInput {
                description: "Sweep the next backlog item".to_string(),
                prompt: "Produce a low-signal stop summary".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("summary-floor".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some("commit push everyting, keep sweeping $ralph"),
                    None,
                )
            },
        )
        .expect("normalized agent should succeed");

        let normalized_manifest = std::fs::read_to_string(&normalized.manifest_file)
            .expect("normalized manifest should exist");
        let normalized_manifest_json: serde_json::Value =
            serde_json::from_str(&normalized_manifest).expect("normalized manifest json");
        assert_eq!(
            normalized_manifest_json["laneEvents"][1]["event"],
            "lane.finished"
        );
        let normalized_detail = normalized_manifest_json["laneEvents"][1]["detail"]
            .as_str()
            .expect("normalized detail");
        assert!(normalized_detail.contains("Completed lane `summary-floor`"));
        assert!(normalized_detail.contains("Sweep the next backlog item"));
        assert_eq!(
            normalized_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
            true
        );
        assert_eq!(
            normalized_manifest_json["laneEvents"][1]["data"]["rawSummary"],
            "commit push everyting, keep sweeping $ralph"
        );
        assert_eq!(
            normalized_manifest_json["laneEvents"][1]["data"]["reasons"][0],
            "control_only"
        );

        let recovery = execute_agent_with_spawn(
            AgentInput {
                description: "Recover the stalled audit lane".to_string(),
                prompt: "Normalize OMX reinjection control prose".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("recovery-lane".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some(
                        "Team read-only-audit-only-for-roadm: worker panes stalled, no progress 2m30s. Next: omx team status read-only-audit-only-for-roadm; read worker messages; unblock/reassign or shutdown. [OMX_TMUX_INJECT]",
                    ),
                    None,
                )
            },
        )
        .expect("recovery agent should succeed");

        let recovery_manifest = std::fs::read_to_string(&recovery.manifest_file)
            .expect("recovery manifest should exist");
        let recovery_manifest_json: serde_json::Value =
            serde_json::from_str(&recovery_manifest).expect("recovery manifest json");
        let recovery_detail = recovery_manifest_json["laneEvents"][1]["detail"]
            .as_str()
            .expect("recovery detail");
        assert!(recovery_detail.contains("Recovery handoff observed via tmux reinjection"));
        assert!(recovery_detail.contains("read-only-audit-only-for-roadm"));
        assert!(!recovery_detail.contains("OMX_TMUX_INJECT"));
        assert_eq!(
            recovery_manifest_json["laneEvents"][1]["data"]["recoveryOutcome"]["cause"],
            "tmux_reinject_after_idle"
        );
        assert_eq!(
            recovery_manifest_json["laneEvents"][1]["data"]["recoveryOutcome"]["targetLane"],
            "read-only-audit-only-for-roadm"
        );
        assert_eq!(
            recovery_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
            true
        );
        assert_eq!(
            recovery_manifest_json["laneEvents"][1]["data"]["reasons"][0],
            "recovery_control_prose"
        );

        let review = execute_agent_with_spawn(
            AgentInput {
                description: "Review commit 1234abcd for ROADMAP #67".to_string(),
                prompt: "Review the scoped diff".to_string(),
                subagent_type: Some("Verification".to_string()),
                name: Some("review-lane".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some("APPROVE\n\nTarget: commit 1234abcd\nRationale: scoped diff is safe."),
                    None,
                )
            },
        )
        .expect("review agent should succeed");

        let review_manifest =
            std::fs::read_to_string(&review.manifest_file).expect("review manifest should exist");
        let review_manifest_json: serde_json::Value =
            serde_json::from_str(&review_manifest).expect("review manifest json");
        assert_eq!(
            review_manifest_json["laneEvents"][1]["data"]["reviewVerdict"],
            "approve"
        );
        assert_eq!(
            review_manifest_json["laneEvents"][1]["data"]["reviewTarget"],
            "Review commit 1234abcd for ROADMAP #67"
        );
        assert_eq!(
            review_manifest_json["laneEvents"][1]["data"]["reviewRationale"],
            "Target: commit 1234abcd Rationale: scoped diff is safe."
        );
        assert_eq!(
            review_manifest_json["laneEvents"][1]["data"]["qualityFloorApplied"],
            false
        );

        let selection = execute_agent_with_spawn(
            AgentInput {
                description: "Scan ROADMAP Immediate Backlog for the next repo-local item".to_string(),
                prompt: "Choose the next backlog target".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("backlog-scan".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some(
                        "Selected next backlog target.\nChosen: ROADMAP #65\nSkipped: ROADMAP #63, ROADMAP #64\nAction: execute\nRationale: #65 is the next repo-local lane-finished metadata task.",
                    ),
                    None,
                )
            },
        )
        .expect("selection agent should succeed");

        let selection_manifest = std::fs::read_to_string(&selection.manifest_file)
            .expect("selection manifest should exist");
        let selection_manifest_json: serde_json::Value =
            serde_json::from_str(&selection_manifest).expect("selection manifest json");
        assert_eq!(
            selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["chosenItems"][0],
            "ROADMAP #65"
        );
        assert_eq!(
            selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["skippedItems"][0],
            "ROADMAP #63"
        );
        assert_eq!(
            selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["skippedItems"][1],
            "ROADMAP #64"
        );
        assert_eq!(
            selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["action"],
            "execute"
        );
        assert_eq!(
            selection_manifest_json["laneEvents"][1]["data"]["selectionOutcome"]["rationale"],
            "#65 is the next repo-local lane-finished metadata task."
        );

        let artifact = execute_agent_with_spawn(
            AgentInput {
                description: "Land ROADMAP #64 provenance hardening".to_string(),
                prompt: "Ship structured artifact provenance".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("artifact-lane".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some(
                        "Completed ROADMAP #64. Files: rust/crates/tools/src/lib.rs ROADMAP.md. Diff stat: 2 files, +12/-1. Tested, committed, pushed as commit deadbee.",
                    ),
                    None,
                )
            },
        )
        .expect("artifact agent should succeed");

        let artifact_manifest = std::fs::read_to_string(&artifact.manifest_file)
            .expect("artifact manifest should exist");
        let artifact_manifest_json: serde_json::Value =
            serde_json::from_str(&artifact_manifest).expect("artifact manifest json");
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["sourceLanes"][0],
            "artifact-lane"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["roadmapIds"][0],
            "ROADMAP #64"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["files"][0],
            "ROADMAP.md"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["files"][1],
            "rust/crates/tools/src/lib.rs"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["diffStat"],
            "2 files, +12/-1."
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["verification"]
                [0],
            "tested"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["verification"]
                [1],
            "committed"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["verification"]
                [2],
            "pushed"
        );
        assert_eq!(
            artifact_manifest_json["laneEvents"][1]["data"]["artifactProvenance"]["commitSha"],
            "deadbee"
        );

        let cron = global_cron_registry().create(
            "*/10 * * * *",
            "roadmap-nudge-10min for ROADMAP #66",
            Some("ROADMAP #66 reminder"),
        );
        let reminder = execute_agent_with_spawn(
            AgentInput {
                description: "Close ROADMAP #66 reminder shutdown".to_string(),
                prompt: "Finish the cron shutdown fix".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("cron-closeout".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some("Completed ROADMAP #66 after verification."),
                    None,
                )
            },
        )
        .expect("reminder agent should succeed");

        let reminder_manifest = std::fs::read_to_string(&reminder.manifest_file)
            .expect("reminder manifest should exist");
        let reminder_manifest_json: serde_json::Value =
            serde_json::from_str(&reminder_manifest).expect("reminder manifest json");
        assert_eq!(
            reminder_manifest_json["laneEvents"][1]["data"]["disabledCronIds"][0],
            cron.cron_id
        );
        let disabled_entry = global_cron_registry()
            .get(&cron.cron_id)
            .expect("cron should still exist");
        assert!(!disabled_entry.enabled);

        let resume_outcome =
            extract_recovery_outcome("Continue from current mode state. [OMX_TMUX_INJECT]")
                .expect("resume outcome should be detected");
        assert_eq!(resume_outcome.cause, "resume_after_stop");
        assert_eq!(
            resume_outcome.preserved_state.as_deref(),
            Some("current mode state")
        );

        let spawn_error = execute_agent_with_spawn(
            AgentInput {
                description: "Spawn error task".to_string(),
                prompt: "Never starts".to_string(),
                subagent_type: None,
                name: Some("spawn-error".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |_| Err(String::from("thread creation failed")),
        )
        .expect_err("spawn errors should surface");
        assert!(spawn_error.contains("failed to spawn sub-agent"));
        let spawn_error_manifest = std::fs::read_dir(&dir)
            .expect("agent dir should exist")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .find_map(|path| {
                let contents = std::fs::read_to_string(&path).ok()?;
                contents
                    .contains("\"name\": \"spawn-error\"")
                    .then_some(contents)
            })
            .expect("failed manifest should still be written");
        let spawn_error_manifest_json: serde_json::Value =
            serde_json::from_str(&spawn_error_manifest).expect("spawn error manifest json");
        assert!(spawn_error_manifest.contains("\"status\": \"failed\""));
        assert!(spawn_error_manifest.contains("thread creation failed"));
        assert_eq!(
            spawn_error_manifest_json["currentBlocker"]["failureClass"],
            "infra"
        );
        assert_eq!(spawn_error_manifest_json["derivedState"], "truly_idle");

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        std::env::remove_var("SUDOCODE_CRON_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn agent_state_classification_covers_finished_and_specific_blockers() {
        assert_eq!(derive_agent_state("running", None, None, None), "working");
        assert_eq!(
            derive_agent_state("completed", Some("done"), None, None),
            "finished_cleanable"
        );
        assert_eq!(
            derive_agent_state("completed", None, None, None),
            "finished_pending_report"
        );
        assert_eq!(
            derive_agent_state("failed", None, Some("mcp handshake timed out"), None),
            "degraded_mcp"
        );
        assert_eq!(
            derive_agent_state(
                "failed",
                None,
                Some("background terminal still running"),
                None
            ),
            "blocked_background_job"
        );
        assert_eq!(
            derive_agent_state("failed", None, Some("merge conflict while rebasing"), None),
            "blocked_merge_conflict"
        );
        assert_eq!(
            derive_agent_state(
                "failed",
                None,
                Some("transport interrupted after partial progress"),
                None
            ),
            "interrupted_transport"
        );
    }

    #[test]
    fn commit_provenance_is_extracted_from_agent_results() {
        let provenance = maybe_commit_provenance(Some("landed as commit deadbee with clean push"))
            .expect("commit provenance");
        assert_eq!(provenance.commit, "deadbee");
        assert_eq!(provenance.canonical_commit.as_deref(), Some("deadbee"));
        assert_eq!(provenance.lineage, vec!["deadbee".to_string()]);
    }
    #[test]
    fn lane_failure_taxonomy_normalizes_common_blockers() {
        let cases = [
            (
                "prompt delivery failed in tmux pane",
                LaneFailureClass::PromptDelivery,
            ),
            (
                "trust prompt is still blocking startup",
                LaneFailureClass::TrustGate,
            ),
            (
                "branch stale against main after divergence",
                LaneFailureClass::BranchDivergence,
            ),
            (
                "compile failed after cargo check",
                LaneFailureClass::Compile,
            ),
            ("targeted tests failed", LaneFailureClass::Test),
            ("plugin bootstrap failed", LaneFailureClass::PluginStartup),
            ("mcp handshake timed out", LaneFailureClass::McpHandshake),
            (
                "mcp startup failed before listing tools",
                LaneFailureClass::McpStartup,
            ),
            (
                "gateway routing rejected the request",
                LaneFailureClass::GatewayRouting,
            ),
            (
                "tool failed: denied tool execution from hook",
                LaneFailureClass::ToolRuntime,
            ),
            (
                "workspace mismatch while resuming the managed session",
                LaneFailureClass::WorkspaceMismatch,
            ),
            ("thread creation failed", LaneFailureClass::Infra),
        ];

        for (message, expected) in cases {
            assert_eq!(classify_lane_failure(message), expected, "{message}");
        }
    }

    #[test]
    fn lane_event_schema_serializes_to_canonical_names() {
        let cases = [
            (LaneEventName::Started, "lane.started"),
            (LaneEventName::Ready, "lane.ready"),
            (LaneEventName::PromptMisdelivery, "lane.prompt_misdelivery"),
            (LaneEventName::Blocked, "lane.blocked"),
            (LaneEventName::Red, "lane.red"),
            (LaneEventName::Green, "lane.green"),
            (LaneEventName::CommitCreated, "lane.commit.created"),
            (LaneEventName::PrOpened, "lane.pr.opened"),
            (LaneEventName::MergeReady, "lane.merge.ready"),
            (LaneEventName::Finished, "lane.finished"),
            (LaneEventName::Failed, "lane.failed"),
            (
                LaneEventName::BranchStaleAgainstMain,
                "branch.stale_against_main",
            ),
            (
                LaneEventName::BranchWorkspaceMismatch,
                "branch.workspace_mismatch",
            ),
        ];

        for (event, expected) in cases {
            assert_eq!(
                serde_json::to_value(event).expect("serialize lane event"),
                json!(expected)
            );
        }
    }

    #[test]
    fn agent_tool_subset_mapping_is_expected() {
        let general = allowed_tools_for_subagent("general-purpose");
        assert!(general.contains("bash"));
        assert!(general.contains("write_file"));
        assert!(!general.contains("Agent"));

        let explore = allowed_tools_for_subagent("Explore");
        assert!(explore.contains("read_file"));
        assert!(explore.contains("grep_search"));
        assert!(!explore.contains("bash"));

        let plan = allowed_tools_for_subagent("Plan");
        assert!(plan.contains("TodoWrite"));
        assert!(plan.contains("StructuredOutput"));
        assert!(!plan.contains("Agent"));

        let verification = allowed_tools_for_subagent("Verification");
        assert!(verification.contains("bash"));
        assert!(verification.contains("PowerShell"));
        assert!(!verification.contains("write_file"));
    }

    #[derive(Debug)]
    struct MockSubagentApiClient {
        calls: usize,
        input_path: String,
    }

    #[async_trait::async_trait]
    impl runtime::ApiClient for MockSubagentApiClient {
        async fn stream(
            &mut self,
            request: ApiRequest,
        ) -> Result<runtime::AssistantEventStream, RuntimeError> {
            self.calls += 1;
            let events = match self.calls {
                1 => {
                    assert_eq!(request.messages.len(), 1);
                    vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "read_file".to_string(),
                            input: json!({ "path": self.input_path }).to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ]
                }
                2 => {
                    assert!(request.messages.len() >= 3);
                    vec![
                        AssistantEvent::TextDelta("Scope: completed mock review".to_string()),
                        AssistantEvent::MessageStop,
                    ]
                }
                _ => unreachable!("extra mock stream call"),
            };
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn subagent_runtime_executes_tool_loop_with_isolated_session() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = temp_path("subagent-input.txt");
        std::fs::write(&path, "hello from child").expect("write input file");

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            MockSubagentApiClient {
                calls: 0,
                input_path: path.display().to_string(),
            },
            SubagentToolExecutor::new(BTreeSet::from([String::from("read_file")])),
            agent_permission_policy(),
            SystemPrompt::default(),
        );

        let summary = runtime
            .run_turn("Inspect the delegated file", None, None)
            .await
            .expect("subagent loop should succeed");

        assert_eq!(
            final_assistant_text(&summary),
            "Scope: completed mock review"
        );
        assert!(runtime
            .session()
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .any(|block| matches!(
                block,
                runtime::ContentBlock::ToolResult { output, .. }
                    if output.contains("hello from child")
            )));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn agent_rejects_blank_required_fields() {
        let missing_description = execute_tool(
            "Agent",
            &json!({
                "description": "  ",
                "prompt": "Inspect"
            }),
        )
        .expect_err("blank description should fail");
        assert!(missing_description.contains("description must not be empty"));

        let missing_prompt = execute_tool(
            "Agent",
            &json!({
                "description": "Inspect branch",
                "prompt": " "
            }),
        )
        .expect_err("blank prompt should fail");
        assert!(missing_prompt.contains("prompt must not be empty"));
    }

    #[test]
    fn task_output_returns_completed_agent_via_condvar() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-completed");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Calculate 2+2".to_string(),
                prompt: "What is 2+2?".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("calc-task".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "completed",
                    Some("The answer is 4"),
                    None,
                )?;
                super::notify_agent_completion(&job.manifest);
                Ok(())
            },
        )
        .expect("spawn should succeed");

        let result = await_agent_output(&manifest.agent_id, true, 5_000)
            .expect("blocking await should succeed");
        let value: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["retrieval_status"], "success");
        assert_eq!(value["result"], "The answer is 4");

        assert!(
            PathBuf::from(value["output_file"].as_str().unwrap()).is_absolute(),
            "output_file should be an absolute path"
        );

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_output_returns_failed_agent() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-failed");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Failing calc".to_string(),
                prompt: "Divide by zero".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("fail-calc".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                persist_agent_terminal_state(
                    &job.manifest,
                    "failed",
                    None,
                    Some(String::from("division by zero")),
                )?;
                super::notify_agent_completion(&job.manifest);
                Ok(())
            },
        )
        .expect("spawn should succeed");

        let result = await_agent_output(&manifest.agent_id, true, 5_000)
            .expect("blocking await of failed agent should succeed");
        let value: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["status"], "failed");
        assert!(value["error"]
            .as_str()
            .unwrap()
            .contains("division by zero"));

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_output_blocks_until_thread_completes() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-threaded");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Slow calculation".to_string(),
                prompt: "What is 6*7?".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: Some("slow-calc".to_string()),
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |job| {
                let thread_name = format!("sudocode-agent-{}", job.manifest.agent_id);
                std::thread::Builder::new()
                    .name(thread_name)
                    .spawn(move || {
                        std::thread::sleep(Duration::from_millis(100));
                        let _ = persist_agent_terminal_state(
                            &job.manifest,
                            "completed",
                            Some("The answer is 42"),
                            None,
                        );
                        super::notify_agent_completion(&job.manifest);
                    })
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            },
        )
        .expect("spawn should succeed");

        let result = await_agent_output(&manifest.agent_id, true, 10_000)
            .expect("blocking await should succeed after thread finishes");
        let value: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["result"], "The answer is 42");

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_output_non_blocking_returns_not_ready_for_running_agent() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-nonblocking");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Slow task".to_string(),
                prompt: "Run slowly".to_string(),
                subagent_type: None,
                name: None,
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |_job| Ok(()), // spawn but don't complete — manifest stays "running"
        )
        .expect("spawn should succeed");

        let result = await_agent_output(&manifest.agent_id, false, 5_000)
            .expect("non-blocking poll should not error");
        let value: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["retrieval_status"], "not_ready");
        assert_eq!(value["status"], "running");

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_output_returns_timeout_when_deadline_exceeded() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-timeout");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Never completes".to_string(),
                prompt: "Spin forever".to_string(),
                subagent_type: None,
                name: None,
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |_job| Ok(()),
        )
        .expect("spawn should succeed");

        let result =
            await_agent_output(&manifest.agent_id, true, 1).expect("timeout path should return Ok");
        let value: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["retrieval_status"], "timeout");
        assert_eq!(value["status"], "running");

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── auto-background threshold (Commit 6) ───────────────────────

    #[test]
    fn auto_bg_threshold_defaults_to_120s_when_env_unset() {
        let _guard = env_guard();
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
        let t = auto_background_threshold().expect("default should enable auto-bg");
        assert_eq!(t.as_secs(), 120);
    }

    #[test]
    fn auto_bg_threshold_zero_disables_feature() {
        let _guard = env_guard();
        std::env::set_var("SUDOCODE_AGENT_AUTO_BG_SECS", "0");
        assert!(
            auto_background_threshold().is_none(),
            "0 must disable auto-bg — caller falls back to fully-sync path"
        );
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
    }

    #[test]
    fn auto_bg_threshold_reads_env_override() {
        let _guard = env_guard();
        std::env::set_var("SUDOCODE_AGENT_AUTO_BG_SECS", "5");
        let t = auto_background_threshold().expect("override should enable auto-bg");
        assert_eq!(t.as_secs(), 5);
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
    }

    #[test]
    fn auto_bg_threshold_falls_back_to_default_on_garbage_env() {
        let _guard = env_guard();
        std::env::set_var("SUDOCODE_AGENT_AUTO_BG_SECS", "not-a-number");
        let t = auto_background_threshold()
            .expect("unparseable value should fall back to default, not disable");
        assert_eq!(t.as_secs(), 120);
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
    }

    fn auto_bg_input(label: &str) -> AgentInput {
        AgentInput {
            description: format!("auto-bg test: {label}"),
            prompt: format!("scenario={label}"),
            subagent_type: None,
            name: Some(format!("auto-bg-{label}")),
            model: None,
            run_in_background: Some(false),
            auth_mode: None,
            permission_mode: None,
        }
    }

    #[test]
    fn auto_bg_returns_completed_manifest_when_work_finishes_within_threshold() {
        let _guard = env_guard();
        let dir = temp_path("agent-autobg-fast");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);
        std::env::set_var("SUDOCODE_AGENT_AUTO_BG_SECS", "5");

        let manifest = execute_agent_inline_with_work(auto_bg_input("fast"), None, |_job| {
            // Finishes well before the 5-second threshold.
            std::thread::sleep(Duration::from_millis(50));
            Ok(String::from("done fast"))
        })
        .expect("fast work should complete via auto-bg await path");

        assert_eq!(manifest.status, "completed");
        assert_eq!(manifest.result.as_deref(), Some("done fast"));

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_bg_returns_backgrounded_manifest_when_work_exceeds_threshold() {
        let _guard = env_guard();
        let dir = temp_path("agent-autobg-slow");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);
        // Very short threshold so the test's sync-await window closes quickly.
        std::env::set_var("SUDOCODE_AGENT_AUTO_BG_SECS", "1");

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let manifest = execute_agent_inline_with_work(auto_bg_input("slow"), None, move |_job| {
            // Block until the test releases us — proves the manifest is
            // returned from the timeout branch while the worker is
            // still running.
            let _ = rx.recv();
            Ok(String::from("eventually done"))
        })
        .expect("timeout path should still return Ok(manifest)");

        assert_eq!(
            manifest.status, "backgrounded",
            "auto-bg timeout MUST mark the manifest as backgrounded"
        );

        // The on-disk manifest should also reflect the backgrounded status —
        // a subsequent TaskOutput poll must see the same state.
        let persisted =
            std::fs::read_to_string(&manifest.manifest_file).expect("manifest must be on disk");
        let value: serde_json::Value = serde_json::from_str(&persisted).expect("valid json");
        assert_eq!(value["status"], "backgrounded");

        // Now release the worker and prove TaskOutput(block=true) eventually
        // sees the "completed" transition.
        let _ = tx.send(());
        let out = await_agent_output(&manifest.agent_id, true, 5_000)
            .expect("await should succeed after worker finishes");
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["result"], "eventually done");

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn auto_bg_disabled_runs_fully_sync() {
        let _guard = env_guard();
        let dir = temp_path("agent-autobg-disabled");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);
        std::env::set_var("SUDOCODE_AGENT_AUTO_BG_SECS", "0");

        // With auto-bg disabled, the call must block for the full work
        // duration — no early "backgrounded" return.
        let start = std::time::Instant::now();
        let manifest = execute_agent_inline_with_work(auto_bg_input("disabled"), None, |_job| {
            std::thread::sleep(Duration::from_millis(200));
            Ok(String::from("sync done"))
        })
        .expect("disabled auto-bg must still complete");
        let elapsed = start.elapsed();

        assert_eq!(manifest.status, "completed");
        assert_eq!(manifest.result.as_deref(), Some("sync done"));
        assert!(
            elapsed >= Duration::from_millis(200),
            "disabled auto-bg must block for the full work duration (elapsed={elapsed:?})"
        );

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        std::env::remove_var("SUDOCODE_AGENT_AUTO_BG_SECS");
        let _ = std::fs::remove_dir_all(dir);
    }

    // ── custom `.md` agents (Commit 8) ────────────────────────────

    /// Set HOME (or USERPROFILE on Windows) to the given path for the
    /// duration of the returned guard, so the standard-search-path
    /// resolver in `runtime::custom_agents` looks under our fixture.
    struct HomeGuard {
        keys: Vec<(&'static str, Option<String>)>,
    }
    impl HomeGuard {
        fn override_home(new_home: &Path) -> Self {
            let mut keys = Vec::new();
            for key in ["HOME", "USERPROFILE"] {
                let prior = std::env::var(key).ok();
                std::env::set_var(key, new_home);
                keys.push((key, prior));
            }
            Self { keys }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            for (key, prior) in &self.keys {
                match prior {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn write_fixture_agent(home: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let dir = home.join(".claude").join("agents");
        fs::create_dir_all(&dir).expect("mkdir fixture agents dir");
        let path = dir.join(format!("{name}.md"));
        let contents = format!("---\n{frontmatter}---\n{body}");
        fs::write(&path, contents).expect("write fixture agent");
        path
    }

    #[test]
    fn normalize_preserves_unknown_names_for_custom_lookup() {
        // A custom `.md` agent's name (e.g. `my-researcher`) must pass
        // through normalize_subagent_type verbatim so the lookup on
        // the other side can find it by exact match.
        assert_eq!(
            normalize_subagent_type(Some("my-researcher")),
            "my-researcher"
        );
        assert_eq!(
            normalize_subagent_type(Some("Naming.Committee")),
            "Naming.Committee"
        );
    }

    #[test]
    fn lookup_custom_agent_finds_md_file_under_home() {
        let _guard = env_guard();
        let home = temp_path("custom-agent-home");
        write_fixture_agent(
            &home,
            "my-researcher",
            "name: my-researcher\ndescription: Names only.\ntools: [read_file, glob_search]\n",
            "You are a naming committee. Reply with names only.\n",
        );
        let _home = HomeGuard::override_home(&home);

        let def =
            lookup_custom_agent("my-researcher").expect("custom agent must resolve under HOME");
        assert_eq!(def.name, "my-researcher");
        assert_eq!(
            def.tools.as_deref(),
            Some(&["glob_search".to_string(), "read_file".to_string()][..])
        );
        assert!(def.system_prompt.contains("naming committee"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn lookup_custom_agent_returns_none_for_builtin_names() {
        // A user dropping a shadow `.md` for a built-in preset must
        // NOT hijack the preset — same precedence as CC-fork's
        // getBuiltInAgents winning over parseAgentFromMarkdown.
        let _guard = env_guard();
        let home = temp_path("custom-agent-shadow");
        write_fixture_agent(
            &home,
            "Explore",
            "name: Explore\ndescription: Shadow.\n",
            "Bogus body.\n",
        );
        let _home = HomeGuard::override_home(&home);

        for builtin in [
            "Explore",
            "Plan",
            "Verification",
            "scode-guide",
            "statusline-setup",
            "general-purpose",
            "fork",
        ] {
            assert!(
                lookup_custom_agent(builtin).is_none(),
                "custom lookup must NOT shadow built-in `{builtin}`"
            );
        }

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn allowed_tools_for_custom_agent_uses_frontmatter_list() {
        let _guard = env_guard();
        let home = temp_path("custom-agent-tools");
        write_fixture_agent(
            &home,
            "restricted",
            "name: restricted\ndescription: R.\ntools: [read_file, glob_search]\n",
            "body",
        );
        let _home = HomeGuard::override_home(&home);

        let tools = allowed_tools_for_subagent("restricted");
        assert!(tools.contains("read_file"));
        assert!(tools.contains("glob_search"));
        assert!(!tools.contains("bash"), "write-side tool must be excluded");
        assert!(
            !tools.contains("edit_file"),
            "write-side tool must be excluded"
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn allowed_tools_for_custom_agent_star_inherits_general_purpose() {
        let _guard = env_guard();
        let home = temp_path("custom-agent-star");
        write_fixture_agent(
            &home,
            "wide-open",
            "name: wide-open\ndescription: W.\ntools: '*'\n",
            "body",
        );
        let _home = HomeGuard::override_home(&home);

        let tools = allowed_tools_for_subagent("wide-open");
        // `*` → inherit maximal set, so bash + write_file must appear.
        assert!(tools.contains("bash"), "`*` must inherit bash");
        assert!(tools.contains("write_file"), "`*` must inherit write_file");
        assert!(tools.contains("read_file"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn allowed_tools_for_custom_agent_missing_field_inherits_default() {
        let _guard = env_guard();
        let home = temp_path("custom-agent-no-tools");
        write_fixture_agent(
            &home,
            "inheritor",
            "name: inheritor\ndescription: I.\n",
            "body",
        );
        let _home = HomeGuard::override_home(&home);

        let tools = allowed_tools_for_subagent("inheritor");
        assert!(tools.contains("bash"));
        assert!(tools.contains("write_file"));
        assert!(tools.contains("read_file"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn allowed_tools_for_unknown_non_custom_agent_uses_general_purpose_default() {
        // No custom .md exists for this name. Historically this fell
        // to the `_ =>` default arm returning the general-purpose
        // set — that must NOT regress.
        let _guard = env_guard();
        let home = temp_path("custom-agent-none");
        fs::create_dir_all(&home).expect("home");
        let _home = HomeGuard::override_home(&home);

        let tools = allowed_tools_for_subagent("some-unknown-name");
        assert!(tools.contains("bash"));
        assert!(tools.contains("write_file"));
        assert!(tools.contains("read_file"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn build_system_prompt_uses_agent_scoped_memory_dir() {
        // Two sub-agents of DIFFERENT types running under the same
        // workspace must see DIFFERENT memory dirs — that's the whole
        // point of per-agent-type scoping.
        let _guard = env_guard();
        let base = temp_path("agent-memory-scope");
        std::env::set_var("SUDOCODE_MEMORY_DIR", &base);

        // Seed one entry under Explore's dir and a different one under
        // Plan's dir. Each build_agent_system_prompt call should only
        // see its own.
        let explore_dir = base.join("agent-memory").join("Explore");
        let plan_dir = base.join("agent-memory").join("Plan");
        fs::create_dir_all(&explore_dir).expect("mkdir explore");
        fs::create_dir_all(&plan_dir).expect("mkdir plan");
        fs::write(
            explore_dir.join("secret.md"),
            "---\nname: secret\ndescription: EXPLORE_ONLY_MARKER\nmetadata:\n  type: user\n---\nExplore's secret.\n",
        )
        .expect("write explore entry");
        fs::write(
            plan_dir.join("secret.md"),
            "---\nname: secret\ndescription: PLAN_ONLY_MARKER\nmetadata:\n  type: user\n---\nPlan's secret.\n",
        )
        .expect("write plan entry");

        let explore_prompt =
            build_agent_system_prompt("Explore", "claude-opus-4-8").expect("Explore prompt built");
        let plan_prompt =
            build_agent_system_prompt("Plan", "claude-opus-4-8").expect("Plan prompt built");

        std::env::remove_var("SUDOCODE_MEMORY_DIR");

        let explore_joined = explore_prompt.dynamic_sections.join("\n||\n");
        let plan_joined = plan_prompt.dynamic_sections.join("\n||\n");

        assert!(
            explore_joined.contains("EXPLORE_ONLY_MARKER"),
            "Explore's system prompt MUST see its own memory entry"
        );
        assert!(
            !explore_joined.contains("PLAN_ONLY_MARKER"),
            "Explore MUST NOT see Plan's memory entry — scoping broken"
        );
        assert!(
            plan_joined.contains("PLAN_ONLY_MARKER"),
            "Plan's system prompt MUST see its own memory entry"
        );
        assert!(
            !plan_joined.contains("EXPLORE_ONLY_MARKER"),
            "Plan MUST NOT see Explore's memory entry — scoping broken"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn build_system_prompt_embeds_custom_agent_body() {
        let _guard = env_guard();
        let home = temp_path("custom-agent-prompt");
        write_fixture_agent(
            &home,
            "committee",
            "name: committee\ndescription: Just names.\n",
            "You are the NAMING_COMMITTEE_SENTINEL. Reply with names only.\n",
        );
        let _home = HomeGuard::override_home(&home);

        let prompt =
            build_agent_system_prompt("committee", "claude-opus-4-8").expect("system prompt build");
        let joined = prompt.dynamic_sections.join("\n---section---\n");
        assert!(
            joined.contains("NAMING_COMMITTEE_SENTINEL"),
            "custom-agent body must be embedded in system prompt; got: {joined}"
        );
        assert!(
            joined.contains("custom sub-agent `committee`"),
            "identity line must reference the agent name; got: {joined}"
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn backgrounded_status_is_not_a_terminal_state_for_notifications() {
        // Under coord mode, TaskOutput on a still-running (backgrounded)
        // agent must return JSON not <task-notification> XML — otherwise
        // the coordinator would think the worker completed prematurely.
        assert!(!super::is_terminal_agent_status("backgrounded"));
        assert!(!super::is_terminal_agent_status("BACKGROUNDED"));
        assert!(super::is_terminal_agent_status("completed"));
        assert!(super::is_terminal_agent_status("failed"));
    }

    // ── legacy sync tests continue below ───────────────────────────

    #[test]
    fn task_output_clamps_oversized_timeout_to_cap() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-clamp");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);
        // Shrink the cap so the test is fast; the production cap is 60_000 ms.
        std::env::set_var("SUDOCODE_TASKOUTPUT_MAX_TIMEOUT_MS", "200");

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Never completes".to_string(),
                prompt: "Spin forever".to_string(),
                subagent_type: None,
                name: None,
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |_job| Ok(()),
        )
        .expect("spawn should succeed");

        // Request an absurdly long timeout — the cap must clamp it so ACP-style
        // upper-layer callers don't get cut off waiting for taskoutput.
        let start = std::time::Instant::now();
        let result = await_agent_output(&manifest.agent_id, true, 10 * 60 * 1000)
            .expect("clamped timeout path should return Ok");
        let elapsed = start.elapsed();

        let value: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(value["retrieval_status"], "timeout");
        assert_eq!(value["status"], "running");
        assert!(
            elapsed < Duration::from_secs(5),
            "await should return within the cap, took {elapsed:?}"
        );

        std::env::remove_var("SUDOCODE_TASKOUTPUT_MAX_TIMEOUT_MS");
        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_output_returns_error_for_missing_agent() {
        let _guard = env_guard();
        let dir = temp_path("agent-taskoutput-missing");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let err = await_agent_output("agent-nonexistent", false, 5_000)
            .expect_err("missing agent should error");
        assert!(err.contains("agent not found"), "got: {err}");

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn task_output_rejects_blank_agent_id() {
        let err = await_agent_output("  ", true, 5_000).expect_err("blank agent_id should fail");
        assert!(err.contains("agent_id must not be empty"));
    }

    #[test]
    fn agent_output_file_path_is_absolute_even_with_relative_store_env() {
        let _guard = env_guard();
        let dir = temp_path("agent-abs-path");
        std::env::set_var("SUDOCODE_AGENT_STORE", &dir);

        let manifest = execute_agent_with_spawn(
            AgentInput {
                description: "Path test".to_string(),
                prompt: "Test path".to_string(),
                subagent_type: Some("Explore".to_string()),
                name: None,
                model: None,
                run_in_background: None,
                auth_mode: None,
                permission_mode: None,
            },
            |_job| Ok(()),
        )
        .expect("spawn should succeed");

        assert!(
            PathBuf::from(&manifest.output_file).is_absolute(),
            "output_file should always be absolute, got: {}",
            manifest.output_file
        );
        assert!(
            PathBuf::from(&manifest.manifest_file).is_absolute(),
            "manifest_file should always be absolute, got: {}",
            manifest.manifest_file
        );

        std::env::remove_var("SUDOCODE_AGENT_STORE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sweep_orphaned_tmp_files_removes_tmp_and_leaves_json() {
        let dir = temp_path("agent-sweep-tmp");
        std::fs::create_dir_all(&dir).expect("create dir");

        let tmp_path = dir.join("agent-123.json.tmp");
        let json_path = dir.join("agent-123.json");
        std::fs::write(&tmp_path, b"partial").expect("write tmp");
        std::fs::write(&json_path, b"{}").expect("write json");

        sweep_orphaned_tmp_files(&dir);

        assert!(!tmp_path.exists(), ".tmp file should be removed");
        assert!(json_path.exists(), ".json file should be kept");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notebook_edit_replaces_inserts_and_deletes_cells() {
        let path = temp_path("notebook.ipynb");
        std::fs::write(
            &path,
            r#"{
  "cells": [
    {"cell_type": "code", "id": "cell-a", "metadata": {}, "source": ["print(1)\n"], "outputs": [], "execution_count": null}
  ],
  "metadata": {"kernelspec": {"language": "python"}},
  "nbformat": 4,
  "nbformat_minor": 5
}"#,
        )
        .expect("write notebook");

        let replaced = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "new_source": "print(2)\n",
                "edit_mode": "replace"
            }),
        )
        .expect("NotebookEdit replace should succeed");
        let replaced_output: serde_json::Value = serde_json::from_str(&replaced).expect("json");
        assert_eq!(replaced_output["cell_id"], "cell-a");
        assert_eq!(replaced_output["cell_type"], "code");

        let inserted = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "new_source": "# heading\n",
                "cell_type": "markdown",
                "edit_mode": "insert"
            }),
        )
        .expect("NotebookEdit insert should succeed");
        let inserted_output: serde_json::Value = serde_json::from_str(&inserted).expect("json");
        assert_eq!(inserted_output["cell_type"], "markdown");
        let appended = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "new_source": "print(3)\n",
                "edit_mode": "insert"
            }),
        )
        .expect("NotebookEdit append should succeed");
        let appended_output: serde_json::Value = serde_json::from_str(&appended).expect("json");
        assert_eq!(appended_output["cell_type"], "code");

        let deleted = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": path.display().to_string(),
                "cell_id": "cell-a",
                "edit_mode": "delete"
            }),
        )
        .expect("NotebookEdit delete should succeed without new_source");
        let deleted_output: serde_json::Value = serde_json::from_str(&deleted).expect("json");
        assert!(deleted_output["cell_type"].is_null());
        assert_eq!(deleted_output["new_source"], "");

        let final_notebook: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read notebook"))
                .expect("valid notebook json");
        let cells = final_notebook["cells"].as_array().expect("cells array");
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0]["cell_type"], "markdown");
        assert!(cells[0].get("outputs").is_none());
        assert_eq!(cells[1]["cell_type"], "code");
        assert_eq!(cells[1]["source"][0], "print(3)\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn notebook_edit_rejects_invalid_inputs() {
        let text_path = temp_path("notebook.txt");
        fs::write(&text_path, "not a notebook").expect("write text file");
        let wrong_extension = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": text_path.display().to_string(),
                "new_source": "print(1)\n"
            }),
        )
        .expect_err("non-ipynb file should fail");
        assert!(wrong_extension.contains("Jupyter notebook"));
        let _ = fs::remove_file(&text_path);

        let empty_notebook = temp_path("empty.ipynb");
        fs::write(
            &empty_notebook,
            r#"{"cells":[],"metadata":{"kernelspec":{"language":"python"}},"nbformat":4,"nbformat_minor":5}"#,
        )
        .expect("write empty notebook");

        let missing_source = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": empty_notebook.display().to_string(),
                "edit_mode": "insert"
            }),
        )
        .expect_err("insert without source should fail");
        assert!(missing_source.contains("new_source is required"));

        let missing_cell = execute_tool(
            "NotebookEdit",
            &json!({
                "notebook_path": empty_notebook.display().to_string(),
                "edit_mode": "delete"
            }),
        )
        .expect_err("delete on empty notebook should fail");
        assert!(missing_cell.contains("Notebook has no cells to edit"));
        let _ = fs::remove_file(empty_notebook);
    }

    // `#[cfg(unix)]` because every command in this test (`printf 'hello'`,
    // `false`, `sleep`, etc.) is POSIX shell vocabulary; the bash tool
    // routes through `runtime::execute_bash` which calls `sh -c "..."`.
    // sh is not on Windows by default. Cross-platform coverage of the
    // same surface needs cmd-equivalent commands and a parallel runtime
    // bash path (see runtime::bash test mod gate for the same trade-off).
    #[cfg(unix)]
    #[test]
    fn bash_tool_reports_success_exit_failure_timeout_and_background() {
        let success = execute_tool("bash", &json!({ "command": "printf 'hello'" }))
            .expect("bash should succeed");
        let success_output: serde_json::Value = serde_json::from_str(&success).expect("json");
        assert_eq!(success_output["stdout"], "hello");
        assert_eq!(success_output["interrupted"], false);

        let failure = execute_tool("bash", &json!({ "command": "printf 'oops' >&2; exit 7" }))
            .expect("bash failure should still return structured output");
        let failure_output: serde_json::Value = serde_json::from_str(&failure).expect("json");
        assert_eq!(failure_output["returnCodeInterpretation"], "exit_code:7");
        assert!(failure_output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("oops"));

        let timeout = execute_tool("bash", &json!({ "command": "sleep 1", "timeout": 10 }))
            .expect("bash timeout should return output");
        let timeout_output: serde_json::Value = serde_json::from_str(&timeout).expect("json");
        assert_eq!(timeout_output["interrupted"], true);
        assert_eq!(timeout_output["returnCodeInterpretation"], "timeout");
        assert!(timeout_output["stderr"]
            .as_str()
            .expect("stderr")
            .contains("Command exceeded timeout"));

        let background = execute_tool(
            "bash",
            &json!({ "command": "sleep 1", "run_in_background": true }),
        )
        .expect("bash background should succeed");
        let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
        assert!(background_output["backgroundTaskId"].as_str().is_some());
        assert_eq!(background_output["noOutputExpected"], true);
    }

    #[test]
    fn bash_workspace_tests_are_blocked_when_branch_is_behind_main() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("workspace-test-preflight");
        let original_dir = std::env::current_dir().expect("cwd");
        init_git_repo(&root);
        run_git(&root, &["checkout", "-b", "feature/stale-tests"]);
        run_git(&root, &["checkout", "main"]);
        commit_file(
            &root,
            "hotfix.txt",
            "fix from main\n",
            "fix: unblock workspace tests",
        );
        run_git(&root, &["checkout", "feature/stale-tests"]);
        std::env::set_current_dir(&root).expect("set cwd");

        let output = execute_tool(
            "bash",
            &json!({ "command": "cargo test --workspace --all-targets" }),
        )
        .expect("preflight should return structured output");
        let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
        assert_eq!(
            output_json["returnCodeInterpretation"],
            "preflight_blocked:branch_divergence"
        );
        assert!(output_json["stderr"]
            .as_str()
            .expect("stderr")
            .contains("branch divergence detected before workspace tests"));
        assert_eq!(
            output_json["structuredContent"][0]["event"],
            "branch.stale_against_main"
        );
        assert_eq!(
            output_json["structuredContent"][0]["failureClass"],
            "branch_divergence"
        );
        assert_eq!(
            output_json["structuredContent"][0]["data"]["missingCommits"][0],
            "fix: unblock workspace tests"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = std::fs::remove_dir_all(root);
    }

    // `#[cfg(unix)]` — same rationale as bash_tool_reports_*.
    #[cfg(unix)]
    #[test]
    fn bash_targeted_tests_skip_branch_preflight() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("targeted-test-no-preflight");
        let original_dir = std::env::current_dir().expect("cwd");
        init_git_repo(&root);
        run_git(&root, &["checkout", "-b", "feature/targeted-tests"]);
        run_git(&root, &["checkout", "main"]);
        commit_file(
            &root,
            "hotfix.txt",
            "fix from main\n",
            "fix: only broad tests should block",
        );
        run_git(&root, &["checkout", "feature/targeted-tests"]);
        std::env::set_current_dir(&root).expect("set cwd");

        let output = execute_tool(
            "bash",
            &json!({ "command": "printf 'targeted ok'; cargo test -p runtime stale_branch" }),
        )
        .expect("targeted commands should still execute");
        let output_json: serde_json::Value = serde_json::from_str(&output).expect("json");
        assert_ne!(
            output_json["returnCodeInterpretation"],
            "preflight_blocked:branch_divergence"
        );

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn file_tools_cover_read_write_and_edit_behaviors() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("fs-suite");
        fs::create_dir_all(&root).expect("create root");
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        let write_create = execute_tool(
            "write_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
        )
        .expect("write create should succeed");
        let write_create_output: serde_json::Value =
            serde_json::from_str(&write_create).expect("json");
        assert_eq!(write_create_output["type"], "create");
        assert!(root.join("nested/demo.txt").exists());

        let write_update = execute_tool(
            "write_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\ngamma\n" }),
        )
        .expect("write update should succeed");
        let write_update_output: serde_json::Value =
            serde_json::from_str(&write_update).expect("json");
        assert_eq!(write_update_output["type"], "update");
        assert_eq!(write_update_output["originalFile"], "alpha\nbeta\nalpha\n");

        let read_full = execute_tool("read_file", &json!({ "path": "nested/demo.txt" }))
            .expect("read full should succeed");
        let read_full_output: serde_json::Value = serde_json::from_str(&read_full).expect("json");
        assert_eq!(read_full_output["file"]["content"], "alpha\nbeta\ngamma");
        assert_eq!(read_full_output["file"]["startLine"], 1);

        let read_slice = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "offset": 1, "limit": 1 }),
        )
        .expect("read slice should succeed");
        let read_slice_output: serde_json::Value = serde_json::from_str(&read_slice).expect("json");
        assert_eq!(read_slice_output["file"]["content"], "beta");
        assert_eq!(read_slice_output["file"]["startLine"], 2);

        let read_past_end = execute_tool(
            "read_file",
            &json!({ "path": "nested/demo.txt", "offset": 50 }),
        )
        .expect("read past EOF should succeed");
        let read_past_end_output: serde_json::Value =
            serde_json::from_str(&read_past_end).expect("json");
        assert_eq!(read_past_end_output["file"]["content"], "");
        assert_eq!(read_past_end_output["file"]["startLine"], 4);

        let read_error = execute_tool("read_file", &json!({ "path": "missing.txt" }))
            .expect_err("missing file should fail");
        assert!(!read_error.is_empty());

        let edit_once = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "old_string": "alpha", "new_string": "omega" }),
        )
        .expect("single edit should succeed");
        let edit_once_output: serde_json::Value = serde_json::from_str(&edit_once).expect("json");
        assert_eq!(edit_once_output["replaceAll"], false);
        assert_eq!(
            fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
            "omega\nbeta\ngamma\n"
        );

        execute_tool(
            "write_file",
            &json!({ "path": "nested/demo.txt", "content": "alpha\nbeta\nalpha\n" }),
        )
        .expect("reset file");
        let edit_all = execute_tool(
            "edit_file",
            &json!({
                "path": "nested/demo.txt",
                "old_string": "alpha",
                "new_string": "omega",
                "replace_all": true
            }),
        )
        .expect("replace all should succeed");
        let edit_all_output: serde_json::Value = serde_json::from_str(&edit_all).expect("json");
        assert_eq!(edit_all_output["replaceAll"], true);
        assert_eq!(
            fs::read_to_string(root.join("nested/demo.txt")).expect("read file"),
            "omega\nbeta\nomega\n"
        );

        let edit_same = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "old_string": "omega", "new_string": "omega" }),
        )
        .expect_err("identical old/new should fail");
        assert!(edit_same.contains("must differ"));

        let edit_missing = execute_tool(
            "edit_file",
            &json!({ "path": "nested/demo.txt", "old_string": "missing", "new_string": "omega" }),
        )
        .expect_err("missing substring should fail");
        assert!(edit_missing.contains("old_string not found"));

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn glob_and_grep_tools_cover_success_and_errors() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("search-suite");
        fs::create_dir_all(root.join("nested")).expect("create root");
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("set cwd");

        fs::write(
            root.join("nested/lib.rs"),
            "fn main() {}\nlet alpha = 1;\nlet alpha = 2;\n",
        )
        .expect("write rust file");
        fs::write(root.join("nested/notes.txt"), "alpha\nbeta\n").expect("write txt file");

        let globbed = execute_tool("glob_search", &json!({ "pattern": "nested/*.rs" }))
            .expect("glob should succeed");
        let globbed_output: serde_json::Value = serde_json::from_str(&globbed).expect("json");
        assert_eq!(globbed_output["numFiles"], 1);
        assert!(globbed_output["filenames"][0]
            .as_str()
            .expect("filename")
            .replace('\\', "/")
            .ends_with("nested/lib.rs"));

        let glob_error = execute_tool("glob_search", &json!({ "pattern": "[" }))
            .expect_err("invalid glob should fail");
        assert!(!glob_error.is_empty());

        let grep_content = execute_tool(
            "grep_search",
            &json!({
                "pattern": "alpha",
                "path": "nested",
                "glob": "*.rs",
                "output_mode": "content",
                "-n": true,
                "head_limit": 1,
                "offset": 1
            }),
        )
        .expect("grep content should succeed");
        let grep_content_output: serde_json::Value =
            serde_json::from_str(&grep_content).expect("json");
        assert_eq!(grep_content_output["numFiles"], 0);
        assert!(grep_content_output["appliedLimit"].is_null());
        assert_eq!(grep_content_output["appliedOffset"], 1);
        assert!(grep_content_output["content"]
            .as_str()
            .expect("content")
            .contains("let alpha = 2;"));

        let grep_count = execute_tool(
            "grep_search",
            &json!({ "pattern": "alpha", "path": "nested", "output_mode": "count" }),
        )
        .expect("grep count should succeed");
        let grep_count_output: serde_json::Value = serde_json::from_str(&grep_count).expect("json");
        assert_eq!(grep_count_output["numMatches"], 3);

        let grep_error = execute_tool(
            "grep_search",
            &json!({ "pattern": "(alpha", "path": "nested" }),
        )
        .expect_err("invalid regex should fail");
        assert!(!grep_error.is_empty());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sleep_waits_and_reports_duration() {
        let started = std::time::Instant::now();
        let result =
            execute_tool("Sleep", &json!({"duration_ms": 20})).expect("Sleep should succeed");
        let elapsed = started.elapsed();
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["duration_ms"], 20);
        assert!(output["message"]
            .as_str()
            .expect("message")
            .contains("Slept for 20ms"));
        assert!(elapsed >= Duration::from_millis(15));
    }

    #[test]
    fn given_excessive_duration_when_sleep_then_rejects_with_error() {
        let result = execute_tool("Sleep", &json!({"duration_ms": 999_999_999_u64}));
        let error = result.expect_err("excessive sleep should fail");
        assert!(error.contains("exceeds maximum allowed sleep"));
    }

    #[test]
    fn given_zero_duration_when_sleep_then_succeeds() {
        let result =
            execute_tool("Sleep", &json!({"duration_ms": 0})).expect("0ms sleep should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["duration_ms"], 0);
    }

    #[test]
    fn brief_returns_sent_message_and_attachment_metadata() {
        let attachment = std::env::temp_dir().join(format!(
            "sudocode-brief-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::write(&attachment, b"png-data").expect("write attachment");

        let result = execute_tool(
            "SendUserMessage",
            &json!({
                "message": "hello user",
                "attachments": [attachment.display().to_string()],
                "status": "normal"
            }),
        )
        .expect("SendUserMessage should succeed");

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["message"], "hello user");
        assert!(output["sentAt"].as_str().is_some());
        assert_eq!(output["attachments"][0]["isImage"], true);
        let _ = std::fs::remove_file(attachment);
    }

    #[test]
    fn ask_user_question_v2_returns_structured_answers() {
        use std::io::Cursor;

        let input = AskUserQuestionInput {
            question: None,
            options: None,
            title: Some("Initial setup".to_string()),
            description: Some("Please answer the following.".to_string()),
            questions: vec![
                AskUserQuestionItem {
                    id: "save_scope".to_string(),
                    prompt: "Where to save preferences?".to_string(),
                    kind: Some("single_select".to_string()),
                    required: Some(true),
                    allow_custom_input: Some(false),
                    custom_input_hint: None,
                    options: vec![
                        AskUserQuestionOption {
                            label: "Project".to_string(),
                            value: "project".to_string(),
                            description: None,
                            recommended: Some(true),
                        },
                        AskUserQuestionOption {
                            label: "User".to_string(),
                            value: "user".to_string(),
                            description: None,
                            recommended: None,
                        },
                    ],
                },
                AskUserQuestionItem {
                    id: "default_language".to_string(),
                    prompt: "Default language?".to_string(),
                    kind: Some("single_select".to_string()),
                    required: Some(true),
                    allow_custom_input: Some(false),
                    custom_input_hint: None,
                    options: vec![
                        AskUserQuestionOption {
                            label: "zh-CN".to_string(),
                            value: "zh-CN".to_string(),
                            description: None,
                            recommended: None,
                        },
                        AskUserQuestionOption {
                            label: "en-US".to_string(),
                            value: "en-US".to_string(),
                            description: None,
                            recommended: None,
                        },
                    ],
                },
            ],
        };

        let mut output = Vec::new();
        let mut input_reader = Cursor::new(b"1\n2\n".to_vec());
        let result = run_ask_user_question_v2(input, &mut output, &mut input_reader)
            .expect("AskUserQuestion v2 should succeed");
        let json: serde_json::Value = serde_json::from_str(&result).expect("json");

        assert_eq!(json["status"], "answered");
        assert_eq!(json["title"], "Initial setup");
        assert_eq!(json["questions"][0]["id"], "save_scope");
        assert_eq!(json["answers"][0]["id"], "save_scope");
        assert_eq!(json["answers"][0]["value"], "project");
        assert_eq!(json["answers"][0]["label"], "Project");
        assert_eq!(json["answers"][1]["value"], "en-US");
        assert!(String::from_utf8(output)
            .expect("utf8")
            .contains("[Question Set] Initial setup"));
    }

    #[test]
    fn ask_user_question_v2_requires_non_empty_questions() {
        use std::io::Cursor;

        let input = AskUserQuestionInput {
            question: None,
            options: None,
            title: None,
            description: None,
            questions: vec![],
        };

        let mut output = Vec::new();
        let mut input_reader = Cursor::new(Vec::<u8>::new());
        let error = run_ask_user_question_v2(input, &mut output, &mut input_reader)
            .expect_err("empty questions should fail");
        assert!(error.contains("question or questions is required"));
    }

    #[test]
    fn config_reads_and_writes_supported_values() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "sudocode-config-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".nexus").join("sudocode")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("cwd dir");
        std::fs::write(
            home.join(".nexus").join("sudocode").join("settings.json"),
            r#"{"verbose":false}"#,
        )
        .expect("write global settings");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::set_current_dir(&cwd).expect("set cwd");

        let get = execute_tool("Config", &json!({"setting": "verbose"})).expect("get config");
        let get_output: serde_json::Value = serde_json::from_str(&get).expect("json");
        assert_eq!(get_output["value"], false);

        let set = execute_tool(
            "Config",
            &json!({"setting": "permissions.defaultMode", "value": "plan"}),
        )
        .expect("set config");
        let set_output: serde_json::Value = serde_json::from_str(&set).expect("json");
        assert_eq!(set_output["operation"], "set");
        assert_eq!(set_output["newValue"], "plan");

        let invalid = execute_tool(
            "Config",
            &json!({"setting": "permissions.defaultMode", "value": "bogus"}),
        )
        .expect_err("invalid config value should error");
        assert!(invalid.contains("Invalid value"));

        let unknown =
            execute_tool("Config", &json!({"setting": "nope"})).expect("unknown setting result");
        let unknown_output: serde_json::Value = serde_json::from_str(&unknown).expect("json");
        assert_eq!(unknown_output["success"], false);

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enter_and_exit_plan_mode_round_trip_existing_local_override() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "sudocode-plan-mode-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".nexus").join("sudocode")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("cwd dir");
        std::fs::write(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
            r#"{"permissions":{"defaultMode":"acceptEdits"}}"#,
        )
        .expect("write local settings");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::set_current_dir(&cwd).expect("set cwd");

        let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
        let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
        assert_eq!(enter_output["changed"], true);
        assert_eq!(enter_output["managed"], true);
        assert_eq!(enter_output["previousLocalMode"], "acceptEdits");
        assert_eq!(enter_output["currentLocalMode"], "plan");

        let local_settings = std::fs::read_to_string(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
        )
        .expect("local settings after enter");
        assert!(local_settings.contains(r#""defaultMode": "plan""#));
        let state = std::fs::read_to_string(
            cwd.join(".nexus")
                .join("sudocode")
                .join("tool-state")
                .join("plan-mode.json"),
        )
        .expect("plan mode state");
        assert!(state.contains(r#""hadLocalOverride": true"#));
        assert!(state.contains(r#""previousLocalMode": "acceptEdits""#));

        let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
        let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
        assert_eq!(exit_output["changed"], true);
        assert_eq!(exit_output["managed"], false);
        assert_eq!(exit_output["previousLocalMode"], "acceptEdits");
        assert_eq!(exit_output["currentLocalMode"], "acceptEdits");

        let local_settings = std::fs::read_to_string(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
        )
        .expect("local settings after exit");
        assert!(local_settings.contains(r#""defaultMode": "acceptEdits""#));
        assert!(!cwd
            .join(".nexus")
            .join("sudocode")
            .join("tool-state")
            .join("plan-mode.json")
            .exists());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exit_plan_mode_clears_override_when_enter_created_it_from_empty_local_state() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = std::env::temp_dir().join(format!(
            "sudocode-plan-mode-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(home.join(".nexus").join("sudocode")).expect("home dir");
        std::fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("cwd dir");

        let original_home = std::env::var("HOME").ok();
        let original_config_home = std::env::var("SUDO_CODE_CONFIG_HOME").ok();
        let original_dir = std::env::current_dir().expect("cwd");
        std::env::set_var("HOME", &home);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        std::env::set_current_dir(&cwd).expect("set cwd");

        let enter = execute_tool("EnterPlanMode", &json!({})).expect("enter plan mode");
        let enter_output: serde_json::Value = serde_json::from_str(&enter).expect("json");
        assert_eq!(enter_output["previousLocalMode"], serde_json::Value::Null);
        assert_eq!(enter_output["currentLocalMode"], "plan");

        let exit = execute_tool("ExitPlanMode", &json!({})).expect("exit plan mode");
        let exit_output: serde_json::Value = serde_json::from_str(&exit).expect("json");
        assert_eq!(exit_output["changed"], true);
        assert_eq!(exit_output["currentLocalMode"], serde_json::Value::Null);

        let local_settings = std::fs::read_to_string(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
        )
        .expect("local settings after exit");
        let local_settings_json: serde_json::Value =
            serde_json::from_str(&local_settings).expect("valid settings json");
        assert_eq!(
            local_settings_json.get("permissions"),
            None,
            "permissions override should be removed on exit"
        );
        assert!(!cwd
            .join(".nexus")
            .join("sudocode")
            .join("tool-state")
            .join("plan-mode.json")
            .exists());

        std::env::set_current_dir(&original_dir).expect("restore cwd");
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_config_home {
            Some(value) => std::env::set_var("SUDO_CODE_CONFIG_HOME", value),
            None => std::env::remove_var("SUDO_CODE_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn structured_output_echoes_input_payload() {
        let result = execute_tool("StructuredOutput", &json!({"ok": true, "items": [1, 2, 3]}))
            .expect("StructuredOutput should succeed");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["data"], "Structured output provided successfully");
        assert_eq!(output["structured_output"]["ok"], true);
        assert_eq!(output["structured_output"]["items"][1], 2);
    }

    #[test]
    fn given_empty_payload_when_structured_output_then_rejects_with_error() {
        let result = execute_tool("StructuredOutput", &json!({}));
        let error = result.expect_err("empty payload should fail");
        assert!(error.contains("must not be empty"));
    }

    // `#[cfg(unix)]` because the REPL tool's Python invocation path
    // has cross-platform quirks (temp-file path handling, default
    // python launcher resolution, exit-code reporting on Windows)
    // that need separate investigation before this test is
    // representative on `windows-latest`. On Ethan's local Windows 11
    // + Python 3.14 the test runs but `exitCode` comes back non-zero
    // — distinct from the "runtime not found" sentinel the original
    // skip branch covers. Tracked alongside the bash tool's
    // cross-platform refactor.
    #[cfg(unix)]
    #[test]
    fn repl_executes_python_code() {
        let result = execute_tool(
            "REPL",
            &json!({"language": "python", "code": "print(1 + 1)", "timeout_ms": 500}),
        );
        // Skip if Python is not installed (e.g. bare CI runners) — the
        // error string varies by platform, so accept the documented
        // sentinel ("runtime not found") *and* any other spawn failure
        // ("program not found", "cannot find the path", etc.) that
        // surfaces when no `python` is on PATH.
        let output_str = match &result {
            Err(e)
                if e.contains("runtime not found")
                    || e.contains("not found")
                    || e.contains("cannot find the path") =>
            {
                eprintln!("SKIP: python not available on this machine");
                return;
            }
            other => other.as_deref().expect("REPL should succeed").to_string(),
        };
        let output: serde_json::Value = serde_json::from_str(&output_str).expect("json");
        assert_eq!(output["language"], "python");
        assert_eq!(output["exitCode"], 0);
        assert!(output["stdout"].as_str().expect("stdout").contains('2'));
    }

    #[test]
    fn given_empty_code_when_repl_then_rejects_with_error() {
        let result = execute_tool("REPL", &json!({"language": "python", "code": "   "}));

        let error = result.expect_err("empty REPL code should fail");
        assert!(error.contains("code must not be empty"));
    }

    #[test]
    fn given_unsupported_language_when_repl_then_rejects_with_error() {
        let result = execute_tool("REPL", &json!({"language": "ruby", "code": "puts 1"}));

        let error = result.expect_err("unsupported REPL language should fail");
        assert!(error.contains("unsupported REPL language: ruby"));
    }

    #[test]
    fn given_timeout_ms_when_repl_blocks_then_returns_timeout_error() {
        let result = execute_tool(
            "REPL",
            &json!({
                "language": "python",
                "code": "import time\ntime.sleep(1)",
                "timeout_ms": 10
            }),
        );

        let error = match &result {
            Err(e) if e.contains("runtime not found") => {
                eprintln!("SKIP: python not available on this machine");
                return;
            }
            other => other
                .as_ref()
                .expect_err("timed out REPL execution should fail")
                .clone(),
        };
        assert!(error.contains("REPL execution exceeded timeout of 10 ms"));
    }

    // `#[cfg(unix)]` because the test builds a `pwsh` stub by writing
    // a `#!/bin/sh` script, marking it executable via the hardcoded
    // `/bin/chmod` binary, and prepending its directory to PATH with
    // a Unix `:` separator. Each piece is Unix-only by construction —
    // the test exercises the PowerShell tool's PATH-resolution +
    // arg-passing surface using a shim that only sh can interpret.
    // Cross-platform coverage of the same surface would use a `.bat`
    // shim and the appropriate process-args asserts; that's a real
    // test rewrite, not a one-liner, and is queued as a follow-up
    // with the rest of the cross-platform tools surface.
    #[cfg(unix)]
    #[test]
    fn powershell_runs_via_stub_shell() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!(
            "sudocode-pwsh-bin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        let script = dir.join("pwsh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while [ "$1" != "-Command" ] && [ $# -gt 0 ]; do shift; done
shift
printf 'pwsh:%s' "$1"
"#,
        )
        .expect("write script");
        std::process::Command::new("/bin/chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .expect("chmod");
        let original_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{}", dir.display(), original_path));

        let result = execute_tool(
            "PowerShell",
            &json!({"command": "Write-Output hello", "timeout": 1000}),
        )
        .expect("PowerShell should succeed");

        let background = execute_tool(
            "PowerShell",
            &json!({"command": "Write-Output hello", "run_in_background": true}),
        )
        .expect("PowerShell background should succeed");

        std::env::set_var("PATH", original_path);
        let _ = std::fs::remove_dir_all(dir);

        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["stdout"], "pwsh:Write-Output hello");
        assert!(output["stderr"].as_str().expect("stderr").is_empty());

        let background_output: serde_json::Value = serde_json::from_str(&background).expect("json");
        assert!(background_output["backgroundTaskId"].as_str().is_some());
        assert_eq!(background_output["backgroundedByUser"], true);
        assert_eq!(background_output["assistantAutoBackgrounded"], false);
    }

    #[test]
    fn powershell_errors_when_shell_is_missing() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_path = std::env::var("PATH").unwrap_or_default();
        let empty_dir = std::env::temp_dir().join(format!(
            "sudocode-empty-bin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        std::env::set_var("PATH", empty_dir.display().to_string());

        let err = execute_tool("PowerShell", &json!({"command": "Write-Output hello"}))
            .expect_err("PowerShell should fail when shell is missing");

        std::env::set_var("PATH", original_path);
        let _ = std::fs::remove_dir_all(empty_dir);

        assert!(err.contains("PowerShell executable not found"));
    }

    fn read_only_registry() -> super::GlobalToolRegistry {
        use runtime::permission_enforcer::PermissionEnforcer;
        use runtime::PermissionPolicy;

        let policy = mvp_tool_specs().into_iter().fold(
            PermissionPolicy::new(runtime::PermissionMode::ReadOnly),
            |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
        );
        let mut registry = super::GlobalToolRegistry::builtin();
        registry.set_enforcer(PermissionEnforcer::new(policy));
        registry
    }

    #[test]
    fn given_read_only_enforcer_when_bash_then_denied() {
        let registry = read_only_registry();
        // Use a command that requires DangerFullAccess (rm) to ensure it's blocked in read-only mode
        let err = registry
            .execute("bash", &json!({ "command": "rm -rf /" }))
            .expect_err("bash should be denied in read-only mode");
        assert!(
            err.contains("current mode is 'read-only'"),
            "should cite active mode: {err}"
        );
    }

    #[test]
    fn given_read_only_enforcer_when_write_file_then_denied() {
        let registry = read_only_registry();
        let err = registry
            .execute(
                "write_file",
                &json!({ "path": "/tmp/x.txt", "content": "x" }),
            )
            .expect_err("write_file should be denied in read-only mode");
        assert!(
            err.contains("current mode is read-only"),
            "should cite active mode: {err}"
        );
    }

    #[test]
    fn given_read_only_enforcer_when_edit_file_then_denied() {
        let registry = read_only_registry();
        let err = registry
            .execute(
                "edit_file",
                &json!({ "path": "/tmp/x.txt", "old_string": "a", "new_string": "b" }),
            )
            .expect_err("edit_file should be denied in read-only mode");
        assert!(
            err.contains("current mode is read-only"),
            "should cite active mode: {err}"
        );
    }

    #[test]
    fn given_read_only_enforcer_when_read_file_then_not_permission_denied() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = temp_path("perm-read");
        fs::create_dir_all(&root).expect("create root");
        let file = root.join("readable.txt");
        fs::write(&file, "content\n").expect("write test file");

        let registry = read_only_registry();
        let result = registry.execute("read_file", &json!({ "path": file.display().to_string() }));
        assert!(result.is_ok(), "read_file should be allowed: {result:?}");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn given_read_only_enforcer_when_glob_search_then_not_permission_denied() {
        let registry = read_only_registry();
        let result = registry.execute("glob_search", &json!({ "pattern": "*.rs" }));
        assert!(
            result.is_ok(),
            "glob_search should be allowed in read-only mode: {result:?}"
        );
    }

    // `#[cfg(unix)]` — same rationale as bash_tool_reports_*.
    #[cfg(unix)]
    #[test]
    fn given_no_enforcer_when_bash_then_executes_normally() {
        let _guard = env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let registry = super::GlobalToolRegistry::builtin();
        let result = registry
            .execute("bash", &json!({ "command": "printf 'ok'" }))
            .expect("bash should succeed without enforcer");
        let output: serde_json::Value = serde_json::from_str(&result).expect("json");
        assert_eq!(output["stdout"], "ok");
    }

    struct TestServer {
        addr: SocketAddr,
        shutdown: Option<std::sync::mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(handler: Arc<dyn Fn(&str) -> HttpResponse + Send + Sync + 'static>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set nonblocking listener");
            let addr = listener.local_addr().expect("local addr");
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            let handle = thread::spawn(move || loop {
                if rx.try_recv().is_ok() {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0_u8; 4096];
                        let size = stream.read(&mut buffer).expect("read request");
                        let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                        let request_line = request.lines().next().unwrap_or_default().to_string();
                        let response = handler(&request_line);
                        stream
                            .write_all(response.to_bytes().as_slice())
                            .expect("write response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("server accept failed: {error}"),
                }
            });

            Self {
                addr,
                shutdown: Some(tx),
                handle: Some(handle),
            }
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().expect("join test server");
            }
        }
    }

    struct HttpResponse {
        status: u16,
        reason: &'static str,
        content_type: &'static str,
        body: String,
    }

    impl HttpResponse {
        fn html(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "text/html; charset=utf-8",
                body: body.to_string(),
            }
        }

        fn text(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "text/plain; charset=utf-8",
                body: body.to_string(),
            }
        }

        fn json(status: u16, reason: &'static str, body: &str) -> Self {
            Self {
                status,
                reason,
                content_type: "application/json; charset=utf-8",
                body: body.to_string(),
            }
        }

        fn to_bytes(&self) -> Vec<u8> {
            format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                self.status,
                self.reason,
                self.content_type,
                self.body.len(),
                self.body
            )
            .into_bytes()
        }
    }
}
