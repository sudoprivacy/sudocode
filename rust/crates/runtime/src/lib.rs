//! Core runtime primitives for the `scode` CLI and supporting crates.
//!
//! This crate owns session persistence, permission evaluation, prompt assembly,
//! MCP plumbing, tool-facing file operations, and the core conversation loop
//! that drives interactive and one-shot turns.

pub mod acp_sdk_server;
pub mod acp_stdio_server;
pub mod acp_ws_server;
pub mod agent_color;
pub mod agent_mailbox;
mod bash;
pub mod bash_validation;
mod bootstrap;
pub mod branch_lock;
mod compact;
pub mod config;
pub mod config_validate;
mod conversation;
pub mod coordinator_mode;
pub mod custom_agents;
mod file_intent;
mod file_ops;
mod file_redirect;
mod file_snapshot;
mod file_tracker;
pub mod fs_backend;
mod git_context;
pub mod green_contract;
mod hooks;
pub mod image_registry;
mod json;
pub mod jsonrpc_transport;
mod lane_events;
pub mod lsp_client;
mod mcp;
mod mcp_client;
pub mod mcp_lifecycle_hardened;
pub mod mcp_server;
mod mcp_stdio;
pub mod mcp_tool_bridge;
pub mod memory;
pub mod model_capabilities;
mod oauth;
pub mod permission_enforcer;
mod permissions;
pub mod plugin_lifecycle;
mod policy_engine;
mod prompt;
pub mod recovery_recipes;
mod remote;
pub mod sandbox;
mod session;
pub mod session_control;
pub mod verification_watcher;
pub use session_control::SessionStore;
pub mod cron_registry;
pub mod spawn_task;
mod sse;
pub mod stale_base;
pub mod stale_branch;
pub mod summary_compression;
pub mod task_packet;
pub mod task_registry;
mod time;
#[cfg(test)]
mod trust_resolver;
mod usage;
pub mod worker_boot;

pub use acp_sdk_server::AcpError;
pub use bash::{
    execute_bash, execute_bash_with_abort, execute_bash_with_tracking, BashCommandInput,
    BashCommandOutput, BashWithTrackingResult, DEFAULT_TOOL_SUBPROCESS_TIMEOUT_MS,
};
pub use bootstrap::{BootstrapPhase, BootstrapPlan};
pub use branch_lock::{detect_branch_lock_collisions, BranchLockCollision, BranchLockIntent};
pub use compact::{
    compact_session, estimate_block_tokens, estimate_session_tokens, format_compact_summary,
    get_compact_continuation_message, should_compact, CompactionConfig, CompactionResult,
};
pub use config::{
    default_config_home, load_plugin_mcp_servers, ConfigEntry, ConfigError, ConfigLoader,
    ConfigSource, McpConfigCollection, McpManagedProxyServerConfig, McpOAuthConfig,
    McpRemoteServerConfig, McpSdkServerConfig, McpServerConfig, McpStdioServerConfig, McpTransport,
    McpWebSocketServerConfig, ModelConfigEntry, ModelProviderMapping, OAuthConfig,
    ProviderConnectionConfig, ProviderFallbackConfig, ResolvedPermissionMode, RuntimeConfig,
    RuntimeFeatureConfig, RuntimeHookConfig, RuntimePermissionRuleConfig, RuntimePluginConfig,
    ScopedMcpServerConfig, SudoCodeConfig, WebSearchConfig, SAMPLE_SUDOCODE_JSON,
    SUDOCODE_SETTINGS_SCHEMA_NAME,
};
pub use config_validate::{
    check_unsupported_format, format_diagnostics, validate_config_file, ConfigDiagnostic,
    DiagnosticKind, ValidationResult,
};
pub use conversation::{
    auto_compaction_threshold_from_env, ApiClient, ApiRequest, AssistantEvent,
    AssistantEventStream, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
    RuntimeObserver, StaticToolExecutor, ToolDispatchContext, ToolError, ToolExecutor, TurnSummary,
    FORK_BOILERPLATE_TAG,
};
pub use file_intent::{detect_file_intent, FileIntent, FileOpKind, UserRequestIntent};
pub use file_ops::{
    edit_file, edit_file_with_intent, glob_search, grep_search, read_file, write_file,
    write_file_with_intent, EditFileOutput, FileOpResult, GlobSearchOutput, GrepSearchInput,
    GrepSearchOutput, ReadFileOutput, StructuredPatchHunk, TextFilePayload, WriteFileOutput,
};
pub use file_redirect::{get_drafts_dir, is_in_drafts, redirect_to_drafts, DRAFTS_DIR_NAME};
pub use file_snapshot::{FileChangeSnapshot, FileChangeSnapshotWithMtime};
pub use file_tracker::{CleanupResult, CleanupStrategy, FileOp, TurnFileTracker};
pub use fs_backend::{
    FsBackend, FsDirEntry, FsMetadata, KernelFsBackend, NexusVfsFsBackend, StdFsBackend,
};
pub use git_context::{GitCommitEntry, GitContext};
pub use hooks::{
    HookAbortSignal, HookEvent, HookProgressEvent, HookProgressReporter, HookRunResult, HookRunner,
};
pub use image_registry::{ImageRegistry, RegisteredImage};
pub use lane_events::{
    compute_event_fingerprint, dedupe_superseded_commit_events, dedupe_terminal_events,
    is_terminal_event, BlockedSubphase, EventProvenance, LaneCommitProvenance, LaneEvent,
    LaneEventBlocker, LaneEventBuilder, LaneEventMetadata, LaneEventName, LaneEventStatus,
    LaneFailureClass, LaneOwnership, SessionIdentity, ShipMergeMethod, ShipProvenance,
    WatcherAction,
};
pub use mcp::{
    mcp_server_signature, mcp_tool_name, mcp_tool_prefix, normalize_name_for_mcp,
    scoped_mcp_config_hash, unwrap_ccr_proxy_url,
};
pub use mcp_client::{
    McpClientAuth, McpClientBootstrap, McpClientTransport, McpManagedProxyTransport,
    McpRemoteTransport, McpSdkTransport, McpStdioTransport,
};
pub use mcp_lifecycle_hardened::{
    McpDegradedReport, McpErrorSurface, McpFailedServer, McpLifecyclePhase, McpLifecycleState,
    McpLifecycleValidator, McpPhaseResult,
};
pub use mcp_server::{McpServer, McpServerSpec, ToolCallHandler, MCP_SERVER_PROTOCOL_VERSION};
pub use mcp_stdio::{
    spawn_mcp_stdio_process, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    ManagedMcpTool, McpDiscoveryFailure, McpInitializeClientInfo, McpInitializeParams,
    McpInitializeResult, McpInitializeServerInfo, McpListResourcesParams, McpListResourcesResult,
    McpListToolsParams, McpListToolsResult, McpReadResourceParams, McpReadResourceResult,
    McpResource, McpResourceContents, McpServerManager, McpServerManagerError, McpStdioProcess,
    McpTool, McpToolCallContent, McpToolCallParams, McpToolCallResult, McpToolDiscoveryReport,
    UnsupportedMcpServer,
};
pub use oauth::{
    clear_oauth_credentials, clear_oauth_credentials_from_keyring, clear_oauth_credentials_with,
    code_challenge_s256, credentials_path, generate_pkce_pair, generate_state,
    import_claude_code_credentials, import_claude_code_credentials_with, load_oauth_credentials,
    load_oauth_credentials_with, loopback_redirect_uri, parse_oauth_callback_query,
    parse_oauth_callback_request_target, save_oauth_credentials, save_oauth_credentials_with,
    OAuthAuthorizationRequest, OAuthCallbackParams, OAuthRefreshRequest, OAuthTokenExchangeRequest,
    OAuthTokenSet, PkceChallengeMethod, PkceCodePair,
};
pub use permissions::{
    PermissionContext, PermissionMode, PermissionOutcome, PermissionOverride, PermissionPolicy,
    PermissionPromptDecision, PermissionPrompter, PermissionRequest, QuestionField, QuestionKind,
    QuestionOption, QuestionPromptAnswer, QuestionPromptRequest, QuestionPrompter,
};
pub use plugin_lifecycle::{
    DegradedMode, DiscoveryResult, PluginHealthcheck, PluginLifecycle, PluginLifecycleEvent,
    PluginState, ResourceInfo, ServerHealth, ServerStatus, ToolInfo,
};
pub use policy_engine::{
    evaluate, DiffScope, GreenLevel, LaneBlocker, LaneContext, PolicyAction, PolicyCondition,
    PolicyEngine, PolicyRule, ReconcileReason, ReviewStatus,
};
pub use prompt::{
    load_system_prompt, load_system_prompt_for_agent, load_system_prompt_with, prepend_bullets,
    ContextFile, ModelFamilyIdentity, ProjectContext, PromptBuildError, SystemPrompt,
    SystemPromptBuilder, FRONTIER_MODEL_NAME, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
pub use recovery_recipes::{
    attempt_recovery, recipe_for, EscalationPolicy, FailureScenario, RecoveryContext,
    RecoveryEvent, RecoveryRecipe, RecoveryResult, RecoveryStep,
};
pub use remote::{
    inherited_upstream_proxy_env, no_proxy_list, read_token, read_token_with,
    upstream_proxy_ws_url, RemoteSessionContext, UpstreamProxyBootstrap, UpstreamProxyState,
    DEFAULT_REMOTE_BASE_URL, DEFAULT_SESSION_TOKEN_PATH, DEFAULT_SYSTEM_CA_BUNDLE, NO_PROXY_HOSTS,
    UPSTREAM_PROXY_ENV_KEYS,
};
pub use sandbox::{
    build_linux_sandbox_command, detect_container_environment, detect_container_environment_from,
    resolve_sandbox_status, resolve_sandbox_status_for_request, ContainerEnvironment,
    FilesystemIsolationMode, LinuxSandboxCommand, SandboxConfig, SandboxDetectionInputs,
    SandboxRequest, SandboxStatus,
};
pub use session::{
    ContentBlock, ConversationMessage, MessageRole, Session, SessionCompaction, SessionError,
    SessionFork, SessionPromptEntry,
};
pub use sse::{IncrementalSseParser, SseEvent};
pub use stale_base::{
    check_base_commit, format_stale_base_warning, read_sudocode_base_file,
    read_sudocode_base_file_with, resolve_expected_base, BaseCommitSource, BaseCommitState,
};
pub use stale_branch::{
    apply_policy, check_freshness, BranchFreshness, StaleBranchAction, StaleBranchEvent,
    StaleBranchPolicy,
};
pub use task_packet::{validate_packet, TaskPacket, TaskPacketValidationError, ValidatedPacket};
pub use time::today_local;
#[cfg(test)]
pub use trust_resolver::{TrustConfig, TrustDecision, TrustEvent, TrustPolicy, TrustResolver};
pub use usage::{
    format_usd, parse_usage_cost_currency, pricing_for_model, ModelPricing, TokenUsage,
    UsageCostCurrency, UsageCostEstimate, UsageTracker,
};
pub use worker_boot::{
    Worker, WorkerEvent, WorkerEventKind, WorkerEventPayload, WorkerFailure, WorkerFailureKind,
    WorkerPromptTarget, WorkerReadySnapshot, WorkerRegistry, WorkerStatus, WorkerTrustResolution,
};

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
