use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

use crate::json::JsonValue;
use crate::sandbox::{FilesystemIsolationMode, SandboxConfig};

/// Schema name advertised by generated settings files.
pub const SUDOCODE_SETTINGS_SCHEMA_NAME: &str = "SettingsSchema";

/// The sample `sudocode.json` shipped with the repo, embedded at compile time.
/// Tests can write this to a temp config home instead of hardcoding config JSON.
pub const SAMPLE_SUDOCODE_JSON: &str = include_str!("sudocode.sample.json");

/// Origin of a loaded settings file in the configuration precedence chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigSource {
    User,
    Project,
    Local,
}

/// Effective permission mode after decoding config values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedPermissionMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// A discovered config file and the scope it contributes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub source: ConfigSource,
    pub path: PathBuf,
}

/// Fully merged runtime configuration plus parsed feature-specific views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    merged: BTreeMap<String, JsonValue>,
    loaded_entries: Vec<ConfigEntry>,
    feature_config: RuntimeFeatureConfig,
}

/// Parsed plugin-related settings extracted from runtime config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimePluginConfig {
    enabled_plugins: BTreeMap<String, bool>,
    external_directories: Vec<String>,
    install_root: Option<String>,
    registry_path: Option<String>,
    bundled_root: Option<String>,
    max_output_tokens: Option<u32>,
}

/// Connection details for a provider under a specific auth mode.
///
/// Parsed from `auth_modes.<mode>.<provider>` in `sudocode.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConnectionConfig {
    /// Base URL for API requests.
    pub base_url: String,
    /// Inline API key (api-key / proxy mode).
    pub api_key: Option<String>,
    /// Env var name for API key.
    pub api_key_env: Option<String>,
    /// Inline token (subscription mode).
    pub token: Option<String>,
    /// Env var name for token.
    pub token_env: Option<String>,
    /// Path to auth/credentials file (subscription mode).
    pub auth_file: Option<String>,
}

/// Which provider + wire model ID to use for a model under a given auth mode.
///
/// Parsed from `models.<alias>.providers.<mode>` in `sudocode.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProviderMapping {
    /// Key into `auth_modes.<mode>`.
    pub provider: String,
    /// Provider-specific wire model ID.
    pub model: String,
    /// Wire format override (only needed for proxy providers).
    pub api: Option<String>,
}

/// Model entry in the config registry.
///
/// Parsed from `models.<alias>` in `sudocode.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfigEntry {
    /// Short alias (also the map key), e.g. `"opus"`.
    pub alias: String,
    /// Display name, e.g. `"Claude Opus 4.6"`.
    pub name: String,
    /// Input modalities (informational).
    pub input: Vec<String>,
    /// Auth mode → provider mapping.
    pub providers: BTreeMap<String, ModelProviderMapping>,
}

/// Web search configuration from `sudocode.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchConfig {
    /// Search provider: `"tavily"` or `"duckduckgo"`.
    pub provider: String,
    /// API endpoint URL.
    pub api_url: String,
    /// API key (empty string = fallback to `proxy.sudorouter.apiKey`).
    pub api_key: String,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: "tavily".to_string(),
            api_url: "https://hk.sudorouter.ai/search/tavily/search".to_string(),
            api_key: String::new(),
        }
    }
}

/// Top-level config from `sudocode.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SudoCodeConfig {
    /// `auth_modes.<mode>.<provider_name>` → connection details.
    pub auth_modes: BTreeMap<String, BTreeMap<String, ProviderConnectionConfig>>,
    /// `models.<alias>` → model config.
    pub models: BTreeMap<String, ModelConfigEntry>,
    /// `web_search` → search provider configuration.
    pub web_search: WebSearchConfig,
}

impl SudoCodeConfig {
    /// Return the built-in config parsed from the embedded `sudocode.sample.json`.
    ///
    /// This always succeeds because the sample JSON is compile-time validated.
    #[must_use]
    pub fn builtin() -> Self {
        parse_sudocode_json_str(SAMPLE_SUDOCODE_JSON)
            .expect("embedded sudocode.sample.json must be valid")
    }
}

/// Structured feature configuration consumed by runtime subsystems.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeFeatureConfig {
    hooks: RuntimeHookConfig,
    plugins: RuntimePluginConfig,
    mcp: McpConfigCollection,
    oauth: Option<OAuthConfig>,
    model: Option<String>,
    aliases: BTreeMap<String, String>,
    permission_mode: Option<ResolvedPermissionMode>,
    permission_rules: RuntimePermissionRuleConfig,
    sandbox: SandboxConfig,
    provider_fallbacks: ProviderFallbackConfig,
    trusted_roots: Vec<String>,
}

/// Ordered chain of fallback model identifiers used when the primary
/// provider returns a retryable failure (429/500/503/etc.). The chain is
/// strict: each entry is tried in order until one succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProviderFallbackConfig {
    primary: Option<String>,
    fallbacks: Vec<String>,
}

/// Hook command lists grouped by lifecycle stage.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeHookConfig {
    pre_tool_use: Vec<String>,
    post_tool_use: Vec<String>,
    post_tool_use_failure: Vec<String>,
    hook_sources: BTreeMap<String, String>,
}

/// Raw permission rule lists grouped by allow, deny, and ask behavior.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimePermissionRuleConfig {
    allow: Vec<String>,
    deny: Vec<String>,
    ask: Vec<String>,
}

/// Collection of configured MCP servers after scope-aware merging.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpConfigCollection {
    servers: BTreeMap<String, ScopedMcpServerConfig>,
}

/// MCP server config paired with the scope that defined it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedMcpServerConfig {
    pub scope: ConfigSource,
    pub config: McpServerConfig,
}

/// Transport families supported by configured MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
    Ws,
    Sdk,
    ManagedProxy,
}

/// Scope-normalized MCP server configuration variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerConfig {
    Stdio(McpStdioServerConfig),
    Sse(McpRemoteServerConfig),
    Http(McpRemoteServerConfig),
    Ws(McpWebSocketServerConfig),
    Sdk(McpSdkServerConfig),
    ManagedProxy(McpManagedProxyServerConfig),
}

/// Configuration for an MCP server launched as a local stdio process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStdioServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: Option<PathBuf>,
    pub tool_call_timeout_ms: Option<u64>,
}

/// Configuration for an MCP server reached over HTTP or SSE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRemoteServerConfig {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub headers_helper: Option<String>,
    pub oauth: Option<McpOAuthConfig>,
}

/// Configuration for an MCP server reached over WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpWebSocketServerConfig {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub headers_helper: Option<String>,
}

/// Configuration for an MCP server addressed through an SDK name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpSdkServerConfig {
    pub name: String,
}

/// Configuration for an MCP managed-proxy endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpManagedProxyServerConfig {
    pub url: String,
    pub id: String,
}

/// OAuth overrides associated with a remote MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub callback_port: Option<u16>,
    pub auth_server_metadata_url: Option<String>,
    pub xaa: Option<bool>,
}

/// OAuth client configuration used by the main Claw runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthConfig {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub callback_port: Option<u16>,
    pub manual_redirect_url: Option<String>,
    pub scopes: Vec<String>,
}

/// Errors raised while reading or parsing runtime configuration files.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
}

impl Display for ConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Parse(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Discovers config files and merges them into a [`RuntimeConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLoader {
    cwd: PathBuf,
    config_home: PathBuf,
}

impl ConfigLoader {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, config_home: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            config_home: config_home.into(),
        }
    }

    #[must_use]
    pub fn default_for(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let config_home = default_config_home();
        Self { cwd, config_home }
    }

    #[must_use]
    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    #[must_use]
    pub fn discover(&self) -> Vec<ConfigEntry> {
        vec![
            ConfigEntry {
                source: ConfigSource::User,
                path: self.config_home.join("scode.json"),
            },
            ConfigEntry {
                source: ConfigSource::User,
                path: self.config_home.join("settings.json"),
            },
            ConfigEntry {
                source: ConfigSource::Project,
                path: self.cwd.join(".scode.json"),
            },
            ConfigEntry {
                source: ConfigSource::Project,
                path: self
                    .cwd
                    .join(".nexus")
                    .join("sudocode")
                    .join("settings.json"),
            },
            ConfigEntry {
                source: ConfigSource::Local,
                path: self
                    .cwd
                    .join(".nexus")
                    .join("sudocode")
                    .join("settings.local.json"),
            },
        ]
    }

    pub fn load(&self) -> Result<RuntimeConfig, ConfigError> {
        let mut merged = BTreeMap::new();
        let mut loaded_entries = Vec::new();
        let mut mcp_servers = BTreeMap::new();
        let mut all_warnings = Vec::new();

        for entry in self.discover() {
            crate::config_validate::check_unsupported_format(&entry.path)?;
            let Some(parsed) = read_optional_json_object(&entry.path)? else {
                continue;
            };
            let validation = crate::config_validate::validate_config_file(
                &parsed.object,
                &parsed.source,
                &entry.path,
            );
            if !validation.is_ok() {
                let first_error = &validation.errors[0];
                return Err(ConfigError::Parse(first_error.to_string()));
            }
            all_warnings.extend(validation.warnings);
            validate_optional_hooks_config(&parsed.object, &entry.path)?;
            merge_mcp_servers(&mut mcp_servers, entry.source, &parsed.object, &entry.path)?;
            deep_merge_objects(&mut merged, &parsed.object);
            loaded_entries.push(entry);
        }

        for warning in &all_warnings {
            eprintln!("warning: {warning}");
        }

        let merged_value = JsonValue::Object(merged.clone());

        let feature_config = RuntimeFeatureConfig {
            hooks: parse_optional_hooks_config(&merged_value)?,
            plugins: parse_optional_plugin_config(&merged_value)?,
            mcp: McpConfigCollection {
                servers: mcp_servers,
            },
            oauth: parse_optional_oauth_config(&merged_value, "merged settings.oauth")?,
            model: parse_optional_model(&merged_value),
            aliases: parse_optional_aliases(&merged_value)?,
            permission_mode: parse_optional_permission_mode(&merged_value)?,
            permission_rules: parse_optional_permission_rules(&merged_value)?,
            sandbox: parse_optional_sandbox_config(&merged_value)?,
            provider_fallbacks: parse_optional_provider_fallbacks(&merged_value)?,
            trusted_roots: parse_optional_trusted_roots(&merged_value)?,
        };

        Ok(RuntimeConfig {
            merged,
            loaded_entries,
            feature_config,
        })
    }

    /// Load `sudocode.json` from the config home directory.
    ///
    /// Returns an error if the file does not exist. Built-in defaults from
    /// `SudoCodeConfig::builtin()` are intentionally **not** merged here so
    /// that the on-disk file is the single source of truth. Callers that
    /// can tolerate a missing file (display helpers, alias resolution) are
    /// responsible for their own fallback.
    pub fn load_sudocode_config(&self) -> Result<SudoCodeConfig, ConfigError> {
        self.load_sudocode_config_with(&crate::fs_backend::StdFsBackend)
    }

    /// Load `sudocode.json` using a custom filesystem backend.
    pub fn load_sudocode_config_with(
        &self,
        backend: &dyn crate::fs_backend::FsBackend,
    ) -> Result<SudoCodeConfig, ConfigError> {
        let path = self.config_home.join("sudocode.json");
        if !backend.exists(&path.to_string_lossy()).unwrap_or(false) {
            return Err(ConfigError::Parse(format!(
                "missing sudocode.json: expected at {path}\n\
                 Create this file to configure models and providers.\n\n\
                 To get started, copy the sample config:\n  \
                 cp crates/runtime/src/sudocode.sample.json {path}",
                path = path.display()
            )));
        }
        parse_sudocode_json_with(backend, &path)
    }
}

impl RuntimeConfig {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            merged: BTreeMap::new(),
            loaded_entries: Vec::new(),
            feature_config: RuntimeFeatureConfig::default(),
        }
    }

    #[must_use]
    pub fn merged(&self) -> &BTreeMap<String, JsonValue> {
        &self.merged
    }

    #[must_use]
    pub fn loaded_entries(&self) -> &[ConfigEntry] {
        &self.loaded_entries
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        self.merged.get(key)
    }

    #[must_use]
    pub fn as_json(&self) -> JsonValue {
        JsonValue::Object(self.merged.clone())
    }

    #[must_use]
    pub fn feature_config(&self) -> &RuntimeFeatureConfig {
        &self.feature_config
    }

    #[must_use]
    pub fn mcp(&self) -> &McpConfigCollection {
        &self.feature_config.mcp
    }

    #[must_use]
    pub fn hooks(&self) -> &RuntimeHookConfig {
        &self.feature_config.hooks
    }

    #[must_use]
    pub fn plugins(&self) -> &RuntimePluginConfig {
        &self.feature_config.plugins
    }

    #[must_use]
    pub fn oauth(&self) -> Option<&OAuthConfig> {
        self.feature_config.oauth.as_ref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.feature_config.model.as_deref()
    }

    #[must_use]
    pub fn aliases(&self) -> &BTreeMap<String, String> {
        &self.feature_config.aliases
    }

    #[must_use]
    pub fn permission_mode(&self) -> Option<ResolvedPermissionMode> {
        self.feature_config.permission_mode
    }

    #[must_use]
    pub fn permission_rules(&self) -> &RuntimePermissionRuleConfig {
        &self.feature_config.permission_rules
    }

    #[must_use]
    pub fn sandbox(&self) -> &SandboxConfig {
        &self.feature_config.sandbox
    }

    #[must_use]
    pub fn provider_fallbacks(&self) -> &ProviderFallbackConfig {
        &self.feature_config.provider_fallbacks
    }

    #[must_use]
    pub fn trusted_roots(&self) -> &[String] {
        &self.feature_config.trusted_roots
    }
}

impl RuntimeFeatureConfig {
    #[must_use]
    pub fn with_hooks(mut self, hooks: RuntimeHookConfig) -> Self {
        self.hooks = hooks;
        self
    }

    #[must_use]
    pub fn with_plugins(mut self, plugins: RuntimePluginConfig) -> Self {
        self.plugins = plugins;
        self
    }

    #[must_use]
    pub fn hooks(&self) -> &RuntimeHookConfig {
        &self.hooks
    }

    #[must_use]
    pub fn plugins(&self) -> &RuntimePluginConfig {
        &self.plugins
    }

    #[must_use]
    pub fn mcp(&self) -> &McpConfigCollection {
        &self.mcp
    }

    #[must_use]
    pub fn oauth(&self) -> Option<&OAuthConfig> {
        self.oauth.as_ref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn aliases(&self) -> &BTreeMap<String, String> {
        &self.aliases
    }

    #[must_use]
    pub fn permission_mode(&self) -> Option<ResolvedPermissionMode> {
        self.permission_mode
    }

    #[must_use]
    pub fn permission_rules(&self) -> &RuntimePermissionRuleConfig {
        &self.permission_rules
    }

    #[must_use]
    pub fn sandbox(&self) -> &SandboxConfig {
        &self.sandbox
    }

    #[must_use]
    pub fn provider_fallbacks(&self) -> &ProviderFallbackConfig {
        &self.provider_fallbacks
    }

    #[must_use]
    pub fn trusted_roots(&self) -> &[String] {
        &self.trusted_roots
    }
}

impl ProviderFallbackConfig {
    #[must_use]
    pub fn new(primary: Option<String>, fallbacks: Vec<String>) -> Self {
        Self { primary, fallbacks }
    }

    #[must_use]
    pub fn primary(&self) -> Option<&str> {
        self.primary.as_deref()
    }

    #[must_use]
    pub fn fallbacks(&self) -> &[String] {
        &self.fallbacks
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fallbacks.is_empty()
    }
}

impl RuntimePluginConfig {
    #[must_use]
    pub fn enabled_plugins(&self) -> &BTreeMap<String, bool> {
        &self.enabled_plugins
    }

    #[must_use]
    pub fn external_directories(&self) -> &[String] {
        &self.external_directories
    }

    #[must_use]
    pub fn install_root(&self) -> Option<&str> {
        self.install_root.as_deref()
    }

    #[must_use]
    pub fn registry_path(&self) -> Option<&str> {
        self.registry_path.as_deref()
    }

    #[must_use]
    pub fn bundled_root(&self) -> Option<&str> {
        self.bundled_root.as_deref()
    }

    #[must_use]
    pub fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    pub fn set_max_output_tokens(&mut self, max_output_tokens: Option<u32>) {
        self.max_output_tokens = max_output_tokens;
    }

    pub fn set_plugin_state(&mut self, plugin_id: String, enabled: bool) {
        self.enabled_plugins.insert(plugin_id, enabled);
    }

    #[must_use]
    pub fn state_for(&self, plugin_id: &str, default_enabled: bool) -> bool {
        self.enabled_plugins
            .get(plugin_id)
            .copied()
            .unwrap_or(default_enabled)
    }

    /// Builds a [`plugins::PluginManagerConfig`] from these settings, resolving
    /// any relative directory/path overrides against `cwd` (for `.`-prefixed
    /// values) or `config_home` (for bare names).
    #[must_use]
    pub fn to_plugin_manager_config(
        &self,
        cwd: &Path,
        config_home: &Path,
    ) -> plugins::PluginManagerConfig {
        let mut config = plugins::PluginManagerConfig::new(config_home.to_path_buf());
        config.enabled_plugins = self.enabled_plugins.clone();
        config.external_dirs = self
            .external_directories
            .iter()
            .map(|path| resolve_plugin_path(cwd, config_home, path))
            .collect();
        config.install_root = self
            .install_root
            .as_deref()
            .map(|path| resolve_plugin_path(cwd, config_home, path));
        config.registry_path = self
            .registry_path
            .as_deref()
            .map(|path| resolve_plugin_path(cwd, config_home, path));
        config.bundled_root = self
            .bundled_root
            .as_deref()
            .map(|path| resolve_plugin_path(cwd, config_home, path));
        config
    }
}

/// Resolves a plugin directory/path override: absolute paths pass through,
/// `.`-prefixed values resolve against `cwd`, and bare names against `config_home`.
fn resolve_plugin_path(cwd: &Path, config_home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if value.starts_with('.') {
        cwd.join(path)
    } else {
        config_home.join(path)
    }
}

#[must_use]
/// Returns the default per-user config directory used by the runtime.
pub fn default_config_home() -> PathBuf {
    std::env::var_os("SUDO_CODE_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            // Windows sets USERPROFILE rather than HOME.
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".nexus").join("sudocode"))
        })
        .unwrap_or_else(|| PathBuf::from(".nexus/sudocode"))
}

impl RuntimeHookConfig {
    #[must_use]
    pub fn new(
        pre_tool_use: Vec<String>,
        post_tool_use: Vec<String>,
        post_tool_use_failure: Vec<String>,
    ) -> Self {
        Self {
            pre_tool_use,
            post_tool_use,
            post_tool_use_failure,
            hook_sources: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn new_with_sources(
        pre_tool_use: Vec<(String, String)>,
        post_tool_use: Vec<(String, String)>,
        post_tool_use_failure: Vec<(String, String)>,
    ) -> Self {
        let mut config = Self::default();
        config.extend_with_sources("PreToolUse", pre_tool_use);
        config.extend_with_sources("PostToolUse", post_tool_use);
        config.extend_with_sources("PostToolUseFailure", post_tool_use_failure);
        config
    }

    #[must_use]
    pub fn pre_tool_use(&self) -> &[String] {
        &self.pre_tool_use
    }

    #[must_use]
    pub fn post_tool_use(&self) -> &[String] {
        &self.post_tool_use
    }

    #[must_use]
    pub fn hook_source(&self, event: &str, command: &str) -> Option<&str> {
        self.hook_sources
            .get(&hook_source_key(event, command))
            .map(String::as_str)
    }

    #[must_use]
    pub fn merged(&self, other: &Self) -> Self {
        let mut merged = self.clone();
        merged.extend(other);
        merged
    }

    pub fn extend(&mut self, other: &Self) {
        extend_hook_commands(
            &mut self.pre_tool_use,
            &mut self.hook_sources,
            "PreToolUse",
            other.pre_tool_use(),
            &other.hook_sources,
        );
        extend_hook_commands(
            &mut self.post_tool_use,
            &mut self.hook_sources,
            "PostToolUse",
            other.post_tool_use(),
            &other.hook_sources,
        );
        extend_hook_commands(
            &mut self.post_tool_use_failure,
            &mut self.hook_sources,
            "PostToolUseFailure",
            other.post_tool_use_failure(),
            &other.hook_sources,
        );
    }

    #[must_use]
    pub fn post_tool_use_failure(&self) -> &[String] {
        &self.post_tool_use_failure
    }

    fn extend_with_sources(&mut self, event: &'static str, entries: Vec<(String, String)>) {
        let commands = match event {
            "PreToolUse" => &mut self.pre_tool_use,
            "PostToolUse" => &mut self.post_tool_use,
            "PostToolUseFailure" => &mut self.post_tool_use_failure,
            _ => return,
        };
        for (command, source) in entries {
            if !commands.contains(&command) {
                self.hook_sources
                    .insert(hook_source_key(event, &command), source);
                commands.push(command);
            }
        }
    }
}

impl RuntimePermissionRuleConfig {
    #[must_use]
    pub fn new(allow: Vec<String>, deny: Vec<String>, ask: Vec<String>) -> Self {
        Self { allow, deny, ask }
    }

    #[must_use]
    pub fn allow(&self) -> &[String] {
        &self.allow
    }

    #[must_use]
    pub fn deny(&self) -> &[String] {
        &self.deny
    }

    #[must_use]
    pub fn ask(&self) -> &[String] {
        &self.ask
    }
}

impl McpConfigCollection {
    #[must_use]
    pub fn servers(&self) -> &BTreeMap<String, ScopedMcpServerConfig> {
        &self.servers
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ScopedMcpServerConfig> {
        self.servers.get(name)
    }
}

impl ScopedMcpServerConfig {
    #[must_use]
    pub fn transport(&self) -> McpTransport {
        self.config.transport()
    }
}

impl McpServerConfig {
    #[must_use]
    pub fn transport(&self) -> McpTransport {
        match self {
            Self::Stdio(_) => McpTransport::Stdio,
            Self::Sse(_) => McpTransport::Sse,
            Self::Http(_) => McpTransport::Http,
            Self::Ws(_) => McpTransport::Ws,
            Self::Sdk(_) => McpTransport::Sdk,
            Self::ManagedProxy(_) => McpTransport::ManagedProxy,
        }
    }
}

/// Parsed JSON object paired with its raw source text for validation.
struct ParsedConfigFile {
    object: BTreeMap<String, JsonValue>,
    source: String,
}

fn read_optional_json_object(path: &Path) -> Result<Option<ParsedConfigFile>, ConfigError> {
    read_optional_json_object_with(&crate::fs_backend::StdFsBackend, path)
}

fn read_optional_json_object_with(
    backend: &dyn crate::fs_backend::FsBackend,
    path: &Path,
) -> Result<Option<ParsedConfigFile>, ConfigError> {
    let contents = match backend.read_to_string(&path.to_string_lossy()) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ConfigError::Io(error)),
    };

    if contents.trim().is_empty() {
        return Ok(Some(ParsedConfigFile {
            object: BTreeMap::new(),
            source: contents,
        }));
    }

    let parsed = match JsonValue::parse(&contents) {
        Ok(parsed) => parsed,
        Err(error) => return Err(ConfigError::Parse(format!("{}: {error}", path.display()))),
    };
    let Some(object) = parsed.as_object() else {
        return Err(ConfigError::Parse(format!(
            "{}: top-level settings value must be a JSON object",
            path.display()
        )));
    };
    Ok(Some(ParsedConfigFile {
        object: object.clone(),
        source: contents,
    }))
}

fn merge_mcp_servers(
    target: &mut BTreeMap<String, ScopedMcpServerConfig>,
    source: ConfigSource,
    root: &BTreeMap<String, JsonValue>,
    path: &Path,
) -> Result<(), ConfigError> {
    let Some(mcp_servers) = root.get("mcpServers") else {
        return Ok(());
    };
    let servers = expect_object(mcp_servers, &format!("{}: mcpServers", path.display()))?;
    for (name, value) in servers {
        let parsed = parse_mcp_server_config(
            name,
            value,
            &format!("{}: mcpServers.{name}", path.display()),
        )?;
        target.insert(
            name.clone(),
            ScopedMcpServerConfig {
                scope: source,
                config: parsed,
            },
        );
    }
    Ok(())
}

pub fn load_plugin_mcp_servers(
    path: &Path,
) -> Result<BTreeMap<String, ScopedMcpServerConfig>, ConfigError> {
    let Some(parsed) = read_optional_json_object(path)? else {
        return Ok(BTreeMap::new());
    };

    let mut servers = BTreeMap::new();
    if parsed.object.contains_key("mcpServers") {
        merge_mcp_servers(&mut servers, ConfigSource::Local, &parsed.object, path)?;
    } else {
        let mut wrapped = BTreeMap::new();
        wrapped.insert(
            "mcpServers".to_string(),
            JsonValue::Object(parsed.object.clone()),
        );
        merge_mcp_servers(&mut servers, ConfigSource::Local, &wrapped, path)?;
    }

    let root = path.parent().unwrap_or_else(|| Path::new("."));
    for server in servers.values_mut() {
        absolutize_plugin_mcp_server(root, &mut server.config);
    }
    Ok(servers)
}

fn absolutize_plugin_mcp_server(root: &Path, config: &mut McpServerConfig) {
    let McpServerConfig::Stdio(stdio) = config else {
        return;
    };
    stdio.current_dir = Some(root.to_path_buf());
    let command_path = Path::new(&stdio.command);
    if command_path.is_absolute() {
        return;
    }
    if stdio.command.starts_with("./") || stdio.command.starts_with("../") {
        let command_path = command_path.strip_prefix(".").unwrap_or(command_path);
        stdio.command = root.join(command_path).display().to_string();
    }
}

fn parse_optional_model(root: &JsonValue) -> Option<String> {
    root.as_object()
        .and_then(|object| object.get("model"))
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned)
}

fn parse_optional_aliases(root: &JsonValue) -> Result<BTreeMap<String, String>, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(BTreeMap::new());
    };
    Ok(optional_string_map(object, "aliases", "merged settings")?.unwrap_or_default())
}

fn parse_optional_hooks_config(root: &JsonValue) -> Result<RuntimeHookConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(RuntimeHookConfig::default());
    };
    parse_optional_hooks_config_object(object, "merged settings.hooks")
}

fn parse_optional_hooks_config_object(
    object: &BTreeMap<String, JsonValue>,
    context: &str,
) -> Result<RuntimeHookConfig, ConfigError> {
    let Some(hooks_value) = object.get("hooks") else {
        return Ok(RuntimeHookConfig::default());
    };
    let hooks = expect_object(hooks_value, context)?;
    Ok(RuntimeHookConfig {
        pre_tool_use: optional_string_array(hooks, "PreToolUse", context)?.unwrap_or_default(),
        post_tool_use: optional_string_array(hooks, "PostToolUse", context)?.unwrap_or_default(),
        post_tool_use_failure: optional_string_array(hooks, "PostToolUseFailure", context)?
            .unwrap_or_default(),
        hook_sources: BTreeMap::new(),
    })
}

fn validate_optional_hooks_config(
    root: &BTreeMap<String, JsonValue>,
    path: &Path,
) -> Result<(), ConfigError> {
    parse_optional_hooks_config_object(root, &format!("{}: hooks", path.display())).map(|_| ())
}

fn parse_optional_permission_rules(
    root: &JsonValue,
) -> Result<RuntimePermissionRuleConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(RuntimePermissionRuleConfig::default());
    };
    let Some(permissions) = object.get("permissions").and_then(JsonValue::as_object) else {
        return Ok(RuntimePermissionRuleConfig::default());
    };

    Ok(RuntimePermissionRuleConfig {
        allow: optional_string_array(permissions, "allow", "merged settings.permissions")?
            .unwrap_or_default(),
        deny: optional_string_array(permissions, "deny", "merged settings.permissions")?
            .unwrap_or_default(),
        ask: optional_string_array(permissions, "ask", "merged settings.permissions")?
            .unwrap_or_default(),
    })
}

fn parse_optional_plugin_config(root: &JsonValue) -> Result<RuntimePluginConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(RuntimePluginConfig::default());
    };

    let mut config = RuntimePluginConfig::default();
    if let Some(enabled_plugins) = object.get("enabledPlugins") {
        config.enabled_plugins =
            parse_plugin_enabled_map(enabled_plugins, "merged settings.enabledPlugins")?;
    }

    let Some(plugins_value) = object.get("plugins") else {
        return Ok(config);
    };
    let plugins = expect_object(plugins_value, "merged settings.plugins")?;

    if let Some(enabled_value) = plugins.get("enabled") {
        config.enabled_plugins.extend(parse_plugin_enabled_map(
            enabled_value,
            "merged settings.plugins.enabled",
        )?);
    }
    config.external_directories =
        optional_string_array(plugins, "externalDirectories", "merged settings.plugins")?
            .unwrap_or_default();
    config.install_root =
        optional_string(plugins, "installRoot", "merged settings.plugins")?.map(str::to_string);
    config.registry_path =
        optional_string(plugins, "registryPath", "merged settings.plugins")?.map(str::to_string);
    config.bundled_root =
        optional_string(plugins, "bundledRoot", "merged settings.plugins")?.map(str::to_string);
    config.max_output_tokens = optional_u32(plugins, "maxOutputTokens", "merged settings.plugins")?;
    Ok(config)
}

fn parse_optional_permission_mode(
    root: &JsonValue,
) -> Result<Option<ResolvedPermissionMode>, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(None);
    };
    if let Some(mode) = object.get("permissionMode").and_then(JsonValue::as_str) {
        return parse_permission_mode_label(mode, "merged settings.permissionMode").map(Some);
    }
    let Some(mode) = object
        .get("permissions")
        .and_then(JsonValue::as_object)
        .and_then(|permissions| permissions.get("defaultMode"))
        .and_then(JsonValue::as_str)
    else {
        return Ok(None);
    };
    parse_permission_mode_label(mode, "merged settings.permissions.defaultMode").map(Some)
}

fn parse_permission_mode_label(
    mode: &str,
    context: &str,
) -> Result<ResolvedPermissionMode, ConfigError> {
    match mode {
        "default" | "plan" | "read-only" => Ok(ResolvedPermissionMode::ReadOnly),
        "acceptEdits" | "auto" | "workspace-write" => Ok(ResolvedPermissionMode::WorkspaceWrite),
        "dontAsk" | "danger-full-access" => Ok(ResolvedPermissionMode::DangerFullAccess),
        other => Err(ConfigError::Parse(format!(
            "{context}: unsupported permission mode {other}"
        ))),
    }
}

fn parse_optional_sandbox_config(root: &JsonValue) -> Result<SandboxConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(SandboxConfig::default());
    };
    let Some(sandbox_value) = object.get("sandbox") else {
        return Ok(SandboxConfig::default());
    };
    let sandbox = expect_object(sandbox_value, "merged settings.sandbox")?;
    let filesystem_mode = optional_string(sandbox, "filesystemMode", "merged settings.sandbox")?
        .map(parse_filesystem_mode_label)
        .transpose()?;
    Ok(SandboxConfig {
        enabled: optional_bool(sandbox, "enabled", "merged settings.sandbox")?,
        namespace_restrictions: optional_bool(
            sandbox,
            "namespaceRestrictions",
            "merged settings.sandbox",
        )?,
        network_isolation: optional_bool(sandbox, "networkIsolation", "merged settings.sandbox")?,
        filesystem_mode,
        allowed_mounts: optional_string_array(sandbox, "allowedMounts", "merged settings.sandbox")?
            .unwrap_or_default(),
    })
}

fn parse_optional_provider_fallbacks(
    root: &JsonValue,
) -> Result<ProviderFallbackConfig, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(ProviderFallbackConfig::default());
    };
    let Some(value) = object.get("providerFallbacks") else {
        return Ok(ProviderFallbackConfig::default());
    };
    let entry = expect_object(value, "merged settings.providerFallbacks")?;
    let primary =
        optional_string(entry, "primary", "merged settings.providerFallbacks")?.map(str::to_string);
    let fallbacks = optional_string_array(entry, "fallbacks", "merged settings.providerFallbacks")?
        .unwrap_or_default();
    Ok(ProviderFallbackConfig { primary, fallbacks })
}

fn parse_optional_trusted_roots(root: &JsonValue) -> Result<Vec<String>, ConfigError> {
    let Some(object) = root.as_object() else {
        return Ok(Vec::new());
    };
    Ok(
        optional_string_array(object, "trustedRoots", "merged settings.trustedRoots")?
            .unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// sudocode.json parsing
// ---------------------------------------------------------------------------

/// Parse `sudocode.json` into `SudoCodeConfig`.
#[allow(dead_code)]
fn parse_sudocode_json(path: &Path) -> Result<SudoCodeConfig, ConfigError> {
    parse_sudocode_json_with(&crate::fs_backend::StdFsBackend, path)
}

fn parse_sudocode_json_with(
    backend: &dyn crate::fs_backend::FsBackend,
    path: &Path,
) -> Result<SudoCodeConfig, ConfigError> {
    let content = backend
        .read_to_string(&path.to_string_lossy())
        .map_err(ConfigError::Io)?;
    parse_sudocode_json_str_with_label(&content, &path.display().to_string())
}

fn parse_sudocode_json_str(content: &str) -> Result<SudoCodeConfig, ConfigError> {
    parse_sudocode_json_str_with_label(content, "<builtin>")
}

fn parse_sudocode_json_str_with_label(
    content: &str,
    label: &str,
) -> Result<SudoCodeConfig, ConfigError> {
    let root: JsonValue =
        JsonValue::parse(content).map_err(|e| ConfigError::Parse(format!("{label}: {e}")))?;
    let Some(root_obj) = root.as_object() else {
        return Err(ConfigError::Parse(format!(
            "{label}: expected JSON object at top level",
        )));
    };
    let sentinel = Path::new(label);

    let auth_modes = parse_auth_modes_section(root_obj, sentinel)?;
    let models = parse_sudocode_models_section(root_obj, sentinel)?;
    let web_search = parse_web_search_section(root_obj);

    Ok(SudoCodeConfig {
        auth_modes,
        models,
        web_search,
    })
}

fn parse_auth_modes_section(
    root: &BTreeMap<String, JsonValue>,
    path: &Path,
) -> Result<BTreeMap<String, BTreeMap<String, ProviderConnectionConfig>>, ConfigError> {
    let Some(value) = root.get("auth_modes") else {
        return Ok(BTreeMap::new());
    };
    let modes_obj = expect_object(value, &format!("{}.auth_modes", path.display()))?;
    let mut result = BTreeMap::new();
    for (mode_name, mode_value) in modes_obj {
        let providers_obj = expect_object(
            mode_value,
            &format!("{}.auth_modes.{mode_name}", path.display()),
        )?;
        let mut providers = BTreeMap::new();
        for (provider_name, provider_value) in providers_obj {
            let ctx = format!("{}.auth_modes.{mode_name}.{provider_name}", path.display());
            let entry = expect_object(provider_value, &ctx)?;
            let base_url = expect_string(entry, "baseUrl", &ctx)?.to_string();
            let api_key = optional_string(entry, "apiKey", &ctx)?.map(str::to_string);
            let api_key_env = optional_string(entry, "apiKeyEnv", &ctx)?.map(str::to_string);
            let token = optional_json_string_or_null(entry, "token");
            let token_env = optional_string(entry, "tokenEnv", &ctx)?.map(str::to_string);
            let auth_file = optional_string(entry, "authFile", &ctx)?.map(str::to_string);
            providers.insert(
                provider_name.clone(),
                ProviderConnectionConfig {
                    base_url,
                    api_key,
                    api_key_env,
                    token,
                    token_env,
                    auth_file,
                },
            );
        }
        result.insert(mode_name.clone(), providers);
    }
    Ok(result)
}

fn parse_web_search_section(root: &BTreeMap<String, JsonValue>) -> WebSearchConfig {
    let defaults = WebSearchConfig::default();
    let Some(value) = root.get("web_search") else {
        return defaults;
    };
    let Some(obj) = value.as_object() else {
        return defaults;
    };

    let provider = obj
        .get("provider")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .map_or(defaults.provider, str::to_string);
    let api_url = obj
        .get("apiUrl")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .map_or(defaults.api_url, str::to_string);
    let api_key = obj
        .get("apiKey")
        .and_then(JsonValue::as_str)
        .map_or(defaults.api_key, str::to_string);

    WebSearchConfig {
        provider,
        api_url,
        api_key,
    }
}

fn parse_sudocode_models_section(
    root: &BTreeMap<String, JsonValue>,
    path: &Path,
) -> Result<BTreeMap<String, ModelConfigEntry>, ConfigError> {
    let Some(value) = root.get("models") else {
        return Ok(BTreeMap::new());
    };
    let models_obj = expect_object(value, &format!("{}.models", path.display()))?;
    let mut result = BTreeMap::new();
    for (alias, model_value) in models_obj {
        let ctx = format!("{}.models.{alias}", path.display());
        let entry = expect_object(model_value, &ctx)?;
        let name =
            optional_string(entry, "name", &ctx)?.map_or_else(|| alias.clone(), str::to_string);
        let input = optional_string_array(entry, "input", &ctx)?.unwrap_or_default();
        let alias_field =
            optional_string(entry, "alias", &ctx)?.map_or_else(|| alias.clone(), str::to_string);

        // Parse the providers sub-object.
        let providers = if let Some(providers_value) = entry.get("providers") {
            let providers_obj = expect_object(providers_value, &format!("{ctx}.providers"))?;
            let mut mappings = BTreeMap::new();
            for (mode_name, mapping_value) in providers_obj {
                let m_ctx = format!("{ctx}.providers.{mode_name}");
                let m_entry = expect_object(mapping_value, &m_ctx)?;
                let provider = expect_string(m_entry, "provider", &m_ctx)?.to_string();
                let model = expect_string(m_entry, "model", &m_ctx)?.to_string();
                let api = optional_string(m_entry, "api", &m_ctx)?.map(str::to_string);
                mappings.insert(
                    mode_name.clone(),
                    ModelProviderMapping {
                        provider,
                        model,
                        api,
                    },
                );
            }
            mappings
        } else {
            BTreeMap::new()
        };

        result.insert(
            alias.to_ascii_lowercase(),
            ModelConfigEntry {
                alias: alias_field,
                name,
                input,
                providers,
            },
        );
    }
    Ok(result)
}

/// Read a JSON string field that might be `null` (treat as `None`).
fn optional_json_string_or_null(object: &BTreeMap<String, JsonValue>, key: &str) -> Option<String> {
    match object.get(key) {
        Some(JsonValue::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn parse_filesystem_mode_label(value: &str) -> Result<FilesystemIsolationMode, ConfigError> {
    match value {
        "off" => Ok(FilesystemIsolationMode::Off),
        "workspace-only" => Ok(FilesystemIsolationMode::WorkspaceOnly),
        "allow-list" => Ok(FilesystemIsolationMode::AllowList),
        other => Err(ConfigError::Parse(format!(
            "merged settings.sandbox.filesystemMode: unsupported filesystem mode {other}"
        ))),
    }
}

fn parse_optional_oauth_config(
    root: &JsonValue,
    context: &str,
) -> Result<Option<OAuthConfig>, ConfigError> {
    let Some(oauth_value) = root.as_object().and_then(|object| object.get("oauth")) else {
        return Ok(None);
    };
    let object = expect_object(oauth_value, context)?;
    let client_id = expect_string(object, "clientId", context)?.to_string();
    let authorize_url = expect_string(object, "authorizeUrl", context)?.to_string();
    let token_url = expect_string(object, "tokenUrl", context)?.to_string();
    let callback_port = optional_u16(object, "callbackPort", context)?;
    let manual_redirect_url =
        optional_string(object, "manualRedirectUrl", context)?.map(str::to_string);
    let scopes = optional_string_array(object, "scopes", context)?.unwrap_or_default();
    Ok(Some(OAuthConfig {
        client_id,
        authorize_url,
        token_url,
        callback_port,
        manual_redirect_url,
        scopes,
    }))
}

fn parse_mcp_server_config(
    server_name: &str,
    value: &JsonValue,
    context: &str,
) -> Result<McpServerConfig, ConfigError> {
    let object = expect_object(value, context)?;
    let server_type =
        optional_string(object, "type", context)?.unwrap_or_else(|| infer_mcp_server_type(object));
    match server_type {
        "stdio" => Ok(McpServerConfig::Stdio(McpStdioServerConfig {
            command: expect_string(object, "command", context)?.to_string(),
            args: optional_string_array(object, "args", context)?.unwrap_or_default(),
            env: optional_string_map(object, "env", context)?.unwrap_or_default(),
            current_dir: optional_string(object, "currentDir", context)?.map(PathBuf::from),
            tool_call_timeout_ms: optional_u64(object, "toolCallTimeoutMs", context)?,
        })),
        "sse" => Ok(McpServerConfig::Sse(parse_mcp_remote_server_config(
            object, context,
        )?)),
        "http" => Ok(McpServerConfig::Http(parse_mcp_remote_server_config(
            object, context,
        )?)),
        "ws" => Ok(McpServerConfig::Ws(McpWebSocketServerConfig {
            url: expect_string(object, "url", context)?.to_string(),
            headers: optional_string_map(object, "headers", context)?.unwrap_or_default(),
            headers_helper: optional_string(object, "headersHelper", context)?.map(str::to_string),
        })),
        "sdk" => Ok(McpServerConfig::Sdk(McpSdkServerConfig {
            name: expect_string(object, "name", context)?.to_string(),
        })),
        "claudeai-proxy" => Ok(McpServerConfig::ManagedProxy(McpManagedProxyServerConfig {
            url: expect_string(object, "url", context)?.to_string(),
            id: expect_string(object, "id", context)?.to_string(),
        })),
        other => Err(ConfigError::Parse(format!(
            "{context}: unsupported MCP server type for {server_name}: {other}"
        ))),
    }
}

fn infer_mcp_server_type(object: &BTreeMap<String, JsonValue>) -> &'static str {
    if object.contains_key("url") {
        "http"
    } else {
        "stdio"
    }
}

fn parse_mcp_remote_server_config(
    object: &BTreeMap<String, JsonValue>,
    context: &str,
) -> Result<McpRemoteServerConfig, ConfigError> {
    Ok(McpRemoteServerConfig {
        url: expect_string(object, "url", context)?.to_string(),
        headers: optional_string_map(object, "headers", context)?.unwrap_or_default(),
        headers_helper: optional_string(object, "headersHelper", context)?.map(str::to_string),
        oauth: parse_optional_mcp_oauth_config(object, context)?,
    })
}

fn parse_optional_mcp_oauth_config(
    object: &BTreeMap<String, JsonValue>,
    context: &str,
) -> Result<Option<McpOAuthConfig>, ConfigError> {
    let Some(value) = object.get("oauth") else {
        return Ok(None);
    };
    let oauth = expect_object(value, &format!("{context}.oauth"))?;
    Ok(Some(McpOAuthConfig {
        client_id: optional_string(oauth, "clientId", context)?.map(str::to_string),
        callback_port: optional_u16(oauth, "callbackPort", context)?,
        auth_server_metadata_url: optional_string(oauth, "authServerMetadataUrl", context)?
            .map(str::to_string),
        xaa: optional_bool(oauth, "xaa", context)?,
    }))
}

fn expect_object<'a>(
    value: &'a JsonValue,
    context: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, ConfigError> {
    value
        .as_object()
        .ok_or_else(|| ConfigError::Parse(format!("{context}: expected JSON object")))
}

fn expect_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<&'a str, ConfigError> {
    object
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| ConfigError::Parse(format!("{context}: missing string field {key}")))
}

fn optional_string<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<&'a str>, ConfigError> {
    match object.get(key) {
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| ConfigError::Parse(format!("{context}: field {key} must be a string"))),
        None => Ok(None),
    }
}

fn optional_bool(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<bool>, ConfigError> {
    match object.get(key) {
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| ConfigError::Parse(format!("{context}: field {key} must be a boolean"))),
        None => Ok(None),
    }
}

fn optional_u16(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<u16>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(number) = value.as_i64() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be an integer"
                )));
            };
            let number = u16::try_from(number).map_err(|_| {
                ConfigError::Parse(format!("{context}: field {key} is out of range"))
            })?;
            Ok(Some(number))
        }
        None => Ok(None),
    }
}

fn optional_u32(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<u32>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(number) = value.as_i64() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be a non-negative integer"
                )));
            };
            let number = u32::try_from(number).map_err(|_| {
                ConfigError::Parse(format!("{context}: field {key} is out of range"))
            })?;
            Ok(Some(number))
        }
        None => Ok(None),
    }
}

fn optional_u64(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<u64>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(number) = value.as_i64() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be a non-negative integer"
                )));
            };
            let number = u64::try_from(number).map_err(|_| {
                ConfigError::Parse(format!("{context}: field {key} is out of range"))
            })?;
            Ok(Some(number))
        }
        None => Ok(None),
    }
}

/// Parses a `SudoCode` plugin enabled-state map accepting both legacy bool values and
/// structured object entries.
///
/// Accepted forms per entry:
///   - `"plugin-id@source": true`                  (legacy boolean)
///   - `"plugin-id@source": { "enabled": true }`   (structured object)
fn parse_plugin_enabled_map(
    value: &JsonValue,
    context: &str,
) -> Result<BTreeMap<String, bool>, ConfigError> {
    let Some(map) = value.as_object() else {
        return Err(ConfigError::Parse(format!(
            "{context}: expected JSON object"
        )));
    };
    let mut result = BTreeMap::new();
    for (key, val) in map {
        if key.is_empty() {
            return Err(ConfigError::Parse(format!(
                "{context}: SudoCode plugin id must not be empty"
            )));
        }
        let enabled = match val {
            JsonValue::Bool(b) => *b,
            JsonValue::Object(obj) => match obj.get("enabled") {
                Some(JsonValue::Bool(b)) => *b,
                Some(other) => {
                    let rendered = other.render();
                    return Err(ConfigError::Parse(format!(
                        "{context}.{key}: SudoCode plugin entry `enabled` must be a boolean, got {rendered}"
                    )));
                }
                None => {
                    return Err(ConfigError::Parse(format!(
                        "{context}.{key}: SudoCode plugin object entry must include `enabled`"
                    )))
                }
            },
            other => {
                let rendered = other.render();
                return Err(ConfigError::Parse(format!(
                    "{context}.{key}: SudoCode plugin entry must be a boolean or object, got {rendered}"
                )));
            }
        };
        result.insert(key.clone(), enabled);
    }
    Ok(result)
}

fn optional_string_array(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<Vec<String>>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(array) = value.as_array() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be an array"
                )));
            };
            array
                .iter()
                .map(|item| {
                    item.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                        ConfigError::Parse(format!(
                            "{context}: field {key} must contain only strings"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some)
        }
        None => Ok(None),
    }
}

fn optional_string_map(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
    context: &str,
) -> Result<Option<BTreeMap<String, String>>, ConfigError> {
    match object.get(key) {
        Some(value) => {
            let Some(map) = value.as_object() else {
                return Err(ConfigError::Parse(format!(
                    "{context}: field {key} must be an object"
                )));
            };
            map.iter()
                .map(|(entry_key, entry_value)| {
                    entry_value
                        .as_str()
                        .map(|text| (entry_key.clone(), text.to_string()))
                        .ok_or_else(|| {
                            ConfigError::Parse(format!(
                                "{context}: field {key} must contain only string values"
                            ))
                        })
                })
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Some)
        }
        None => Ok(None),
    }
}

fn deep_merge_objects(
    target: &mut BTreeMap<String, JsonValue>,
    source: &BTreeMap<String, JsonValue>,
) {
    for (key, value) in source {
        match (target.get_mut(key), value) {
            (Some(JsonValue::Object(existing)), JsonValue::Object(incoming)) => {
                deep_merge_objects(existing, incoming);
            }
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn extend_hook_commands(
    target: &mut Vec<String>,
    target_sources: &mut BTreeMap<String, String>,
    event: &'static str,
    values: &[String],
    value_sources: &BTreeMap<String, String>,
) {
    for value in values {
        if !target.contains(value) {
            if let Some(source) = value_sources.get(&hook_source_key(event, value)) {
                target_sources.insert(hook_source_key(event, value), source.clone());
            }
            target.push(value.clone());
        }
    }
}

fn hook_source_key(event: &str, command: &str) -> String {
    format!("{event}\u{0}{command}")
}

#[cfg(test)]
mod tests {
    use super::{
        deep_merge_objects, load_plugin_mcp_servers, parse_permission_mode_label, ConfigLoader,
        ConfigSource, McpServerConfig, McpTransport, ResolvedPermissionMode, RuntimeHookConfig,
        RuntimePluginConfig, SUDOCODE_SETTINGS_SCHEMA_NAME,
    };
    use crate::json::JsonValue;
    use crate::sandbox::FilesystemIsolationMode;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> std::path::PathBuf {
        // #149: previously used `runtime-config-{nanos}` which collided
        // under parallel `cargo test --workspace` when multiple tests
        // started within the same nanosecond bucket on fast machines.
        // Add process id + a monotonically-incrementing atomic counter
        // so every callsite gets a provably-unique directory regardless
        // of clock resolution or scheduling.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let pid = std::process::id();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("runtime-config-{pid}-{nanos}-{seq}"))
    }

    #[test]
    fn rejects_non_object_settings_files() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(home.join("settings.json"), "[]").expect("write bad settings");

        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");
        assert!(error
            .to_string()
            .contains("top-level settings value must be a JSON object"));

        if root.exists() {
            fs::remove_dir_all(root).expect("cleanup temp dir");
        }
    }

    #[test]
    fn loads_and_merges_claude_code_config_files_by_precedence() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("scode.json"),
            r#"{"model":"haiku","env":{"A":"1"},"mcpServers":{"home":{"command":"uvx","args":["home"]}}}"#,
        )
        .expect("write user scode config");
        fs::write(
            home.join("settings.json"),
            r#"{"model":"sonnet","env":{"A2":"1"},"hooks":{"PreToolUse":["base"]},"permissions":{"defaultMode":"plan","allow":["Read"],"deny":["Bash(rm -rf)"]}}"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".scode.json"),
            r#"{"model":"project-compat","env":{"B":"2"}}"#,
        )
        .expect("write project compat config");
        fs::write(
            cwd.join(".nexus").join("sudocode").join("settings.json"),
            r#"{"env":{"C":"3"},"hooks":{"PostToolUse":["project"],"PostToolUseFailure":["project-failure"]},"permissions":{"ask":["Edit"]},"mcpServers":{"project":{"command":"uvx","args":["project"]}}}"#,
        )
        .expect("write project settings");
        fs::write(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
            r#"{"model":"opus","permissionMode":"acceptEdits"}"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(SUDOCODE_SETTINGS_SCHEMA_NAME, "SettingsSchema");
        assert_eq!(loaded.loaded_entries().len(), 5);
        assert_eq!(loaded.loaded_entries()[0].source, ConfigSource::User);
        assert_eq!(
            loaded.get("model"),
            Some(&JsonValue::String("opus".to_string()))
        );
        assert_eq!(loaded.model(), Some("opus"));
        assert_eq!(
            loaded.permission_mode(),
            Some(ResolvedPermissionMode::WorkspaceWrite)
        );
        assert_eq!(
            loaded
                .get("env")
                .and_then(JsonValue::as_object)
                .expect("env object")
                .len(),
            4
        );
        assert!(loaded
            .get("hooks")
            .and_then(JsonValue::as_object)
            .expect("hooks object")
            .contains_key("PreToolUse"));
        assert!(loaded
            .get("hooks")
            .and_then(JsonValue::as_object)
            .expect("hooks object")
            .contains_key("PostToolUse"));
        assert_eq!(loaded.hooks().pre_tool_use(), &["base".to_string()]);
        assert_eq!(loaded.hooks().post_tool_use(), &["project".to_string()]);
        assert_eq!(
            loaded.hooks().post_tool_use_failure(),
            &["project-failure".to_string()]
        );
        assert_eq!(loaded.permission_rules().allow(), &["Read".to_string()]);
        assert_eq!(
            loaded.permission_rules().deny(),
            &["Bash(rm -rf)".to_string()]
        );
        assert_eq!(loaded.permission_rules().ask(), &["Edit".to_string()]);
        assert!(loaded.mcp().get("home").is_some());
        assert!(loaded.mcp().get("project").is_some());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_sandbox_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
            r#"{
              "sandbox": {
                "enabled": true,
                "namespaceRestrictions": false,
                "networkIsolation": true,
                "filesystemMode": "allow-list",
                "allowedMounts": ["logs", "tmp/cache"]
              }
            }"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(loaded.sandbox().enabled, Some(true));
        assert_eq!(loaded.sandbox().namespace_restrictions, Some(false));
        assert_eq!(loaded.sandbox().network_isolation, Some(true));
        assert_eq!(
            loaded.sandbox().filesystem_mode,
            Some(FilesystemIsolationMode::AllowList)
        );
        assert_eq!(loaded.sandbox().allowed_mounts, vec!["logs", "tmp/cache"]);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_provider_fallbacks_chain_with_primary_and_ordered_fallbacks() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");
        fs::write(
            home.join("settings.json"),
            r#"{
              "providerFallbacks": {
                "primary": "claude-opus-4-6",
                "fallbacks": ["grok-3", "grok-3-mini"]
              }
            }"#,
        )
        .expect("write provider fallback settings");

        // when
        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        // then
        let chain = loaded.provider_fallbacks();
        assert_eq!(chain.primary(), Some("claude-opus-4-6"));
        assert_eq!(
            chain.fallbacks(),
            &["grok-3".to_string(), "grok-3-mini".to_string()]
        );
        assert!(!chain.is_empty());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn provider_fallbacks_default_is_empty_when_unset() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(home.join("settings.json"), "{}").expect("write empty settings");

        // when
        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        // then
        let chain = loaded.provider_fallbacks();
        assert_eq!(chain.primary(), None);
        assert!(chain.fallbacks().is_empty());
        assert!(chain.is_empty());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_trusted_roots_from_settings() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            home.join("settings.json"),
            r#"{"trustedRoots": ["/tmp/worktrees", "/home/user/projects"]}"#,
        )
        .expect("write settings");

        // when
        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        // then
        let roots = loaded.trusted_roots();
        assert_eq!(roots, ["/tmp/worktrees", "/home/user/projects"]);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn trusted_roots_default_is_empty_when_unset() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(home.join("settings.json"), "{}").expect("write empty settings");

        // when
        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        // then
        assert!(loaded.trusted_roots().is_empty());

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_typed_mcp_and_oauth_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "mcpServers": {
                "stdio-server": {
                  "command": "uvx",
                  "args": ["mcp-server"],
                  "env": {"TOKEN": "secret"}
                },
                "remote-server": {
                  "type": "http",
                  "url": "https://example.test/mcp",
                  "headers": {"Authorization": "Bearer token"},
                  "headersHelper": "helper.sh",
                  "oauth": {
                    "clientId": "mcp-client",
                    "callbackPort": 7777,
                    "authServerMetadataUrl": "https://issuer.test/.well-known/oauth-authorization-server",
                    "xaa": true
                  }
                }
              },
              "oauth": {
                "clientId": "runtime-client",
                "authorizeUrl": "https://console.test/oauth/authorize",
                "tokenUrl": "https://console.test/oauth/token",
                "callbackPort": 54545,
                "manualRedirectUrl": "https://console.test/oauth/callback",
                "scopes": ["org:read", "user:write"]
              }
            }"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
            r#"{
              "mcpServers": {
                "remote-server": {
                  "type": "ws",
                  "url": "wss://override.test/mcp",
                  "headers": {"X-Env": "local"}
                }
              }
            }"#,
        )
        .expect("write local settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        let stdio_server = loaded
            .mcp()
            .get("stdio-server")
            .expect("stdio server should exist");
        assert_eq!(stdio_server.scope, ConfigSource::User);
        assert_eq!(stdio_server.transport(), McpTransport::Stdio);

        let remote_server = loaded
            .mcp()
            .get("remote-server")
            .expect("remote server should exist");
        assert_eq!(remote_server.scope, ConfigSource::Local);
        assert_eq!(remote_server.transport(), McpTransport::Ws);
        match &remote_server.config {
            McpServerConfig::Ws(config) => {
                assert_eq!(config.url, "wss://override.test/mcp");
                assert_eq!(
                    config.headers.get("X-Env").map(String::as_str),
                    Some("local")
                );
            }
            other => panic!("expected ws config, got {other:?}"),
        }

        let oauth = loaded.oauth().expect("oauth config should exist");
        assert_eq!(oauth.client_id, "runtime-client");
        assert_eq!(oauth.callback_port, Some(54_545));
        assert_eq!(oauth.scopes, vec!["org:read", "user:write"]);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn loads_plugin_mcp_servers_from_wrapped_file_and_resolves_stdio_commands() {
        let root = temp_dir();
        let plugin_root = root.join("plugin");
        fs::create_dir_all(&plugin_root).expect("plugin dir");
        let mcp_path = plugin_root.join(".mcp.json");
        fs::write(
            &mcp_path,
            r#"{
              "mcpServers": {
                "plugin-tools": {
                  "command": "./bin/server",
                  "args": ["--stdio"],
                  "env": {"TOKEN": "x"}
                }
              }
            }"#,
        )
        .expect("write plugin mcp config");

        let servers = load_plugin_mcp_servers(&mcp_path).expect("plugin mcp should parse");
        let server = servers.get("plugin-tools").expect("server exists");
        assert_eq!(server.scope, ConfigSource::Local);
        match &server.config {
            McpServerConfig::Stdio(stdio) => {
                assert_eq!(
                    stdio.command,
                    plugin_root.join("bin/server").display().to_string()
                );
                assert_eq!(stdio.args, vec!["--stdio"]);
                assert_eq!(stdio.env.get("TOKEN").map(String::as_str), Some("x"));
                assert_eq!(stdio.current_dir.as_deref(), Some(plugin_root.as_path()));
            }
            other => panic!("expected stdio server, got {other:?}"),
        }

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn loads_plugin_mcp_servers_from_raw_server_map() {
        let root = temp_dir();
        let plugin_root = root.join("plugin");
        fs::create_dir_all(&plugin_root).expect("plugin dir");
        let mcp_path = plugin_root.join(".mcp.json");
        fs::write(
            &mcp_path,
            r#"{
              "remote": {
                "type": "http",
                "url": "https://example.test/mcp"
              }
            }"#,
        )
        .expect("write plugin mcp config");

        let servers = load_plugin_mcp_servers(&mcp_path).expect("plugin mcp should parse");
        let server = servers.get("remote").expect("server exists");
        match &server.config {
            McpServerConfig::Http(remote) => {
                assert_eq!(remote.url, "https://example.test/mcp");
            }
            other => panic!("expected http server, got {other:?}"),
        }

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn infers_http_mcp_servers_from_url_only_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            home.join("settings.json"),
            r#"{
              "mcpServers": {
                "remote": {
                  "url": "https://example.test/mcp"
                }
              }
            }"#,
        )
        .expect("write mcp settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        let remote_server = loaded
            .mcp()
            .get("remote")
            .expect("remote server should exist");
        assert_eq!(remote_server.transport(), McpTransport::Http);
        match &remote_server.config {
            McpServerConfig::Http(config) => {
                assert_eq!(config.url, "https://example.test/mcp");
            }
            other => panic!("expected http config, got {other:?}"),
        }

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_plugin_config_from_enabled_plugins() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "enabledPlugins": {
                "tool-guard@builtin": true,
                "sample-plugin@external": false
              }
            }"#,
        )
        .expect("write user settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(
            loaded.plugins().enabled_plugins().get("tool-guard@builtin"),
            Some(&true)
        );
        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("sample-plugin@external"),
            Some(&false)
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_plugin_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "enabledPlugins": {
                "core-helpers@builtin": true
              },
              "plugins": {
                "externalDirectories": ["./external-plugins"],
                "installRoot": "plugin-cache/installed",
                "registryPath": "plugin-cache/installed.json",
                "bundledRoot": "./bundled-plugins"
              }
            }"#,
        )
        .expect("write plugin settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("core-helpers@builtin"),
            Some(&true)
        );
        assert_eq!(
            loaded.plugins().external_directories(),
            &["./external-plugins".to_string()]
        );
        assert_eq!(
            loaded.plugins().install_root(),
            Some("plugin-cache/installed")
        );
        assert_eq!(
            loaded.plugins().registry_path(),
            Some("plugin-cache/installed.json")
        );
        assert_eq!(loaded.plugins().bundled_root(), Some("./bundled-plugins"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_structured_plugin_config_object_entries() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "plugins": {
                "enabled": {
                  "enabled-tool@builtin": { "enabled": true },
                  "disabled-tool@external": { "enabled": false },
                  "bool-style@bundled": true
                }
              }
            }"#,
        )
        .expect("write settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("enabled-tool@builtin"),
            Some(&true)
        );
        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("disabled-tool@external"),
            Some(&false)
        );
        assert_eq!(
            loaded.plugins().enabled_plugins().get("bool-style@bundled"),
            Some(&true)
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn merges_legacy_and_structured_plugin_config_entries() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{
              "enabledPlugins": {
                "legacy-only@builtin": true,
                "overridden@external": true
              },
              "plugins": {
                "enabled": {
                  "structured-only@bundled": { "enabled": true },
                  "overridden@external": { "enabled": false }
                }
              }
            }"#,
        )
        .expect("write settings");

        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("legacy-only@builtin"),
            Some(&true)
        );
        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("structured-only@bundled"),
            Some(&true)
        );
        assert_eq!(
            loaded
                .plugins()
                .enabled_plugins()
                .get("overridden@external"),
            Some(&false)
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_empty_plugin_id_in_plugin_enabled_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");

        fs::write(
            home.join("settings.json"),
            r#"{ "plugins": { "enabled": { "": true } } }"#,
        )
        .expect("write settings");

        let err = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        assert!(
            err.to_string()
                .contains("SudoCode plugin id must not be empty"),
            "error was: {err}"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_invalid_type_in_plugin_enabled_config() {
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");

        fs::write(
            home.join("settings.json"),
            r#"{ "plugins": { "enabled": { "my-plugin@external": 42 } } }"#,
        )
        .expect("write settings");

        let err = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        assert!(
            err.to_string()
                .contains("SudoCode plugin entry must be a boolean or object"),
            "error was: {err}"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn rejects_invalid_mcp_server_shapes() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            home.join("settings.json"),
            r#"{"mcpServers":{"broken":{"type":"http","url":123}}}"#,
        )
        .expect("write broken settings");

        // when
        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        // then
        assert!(error
            .to_string()
            .contains("mcpServers.broken: missing string field url"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_user_defined_model_aliases_from_settings() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{"aliases":{"fast":"claude-haiku-4-5-20251213","smart":"claude-opus-4-6"}}"#,
        )
        .expect("write user settings");
        fs::write(
            cwd.join(".nexus")
                .join("sudocode")
                .join("settings.local.json"),
            r#"{"aliases":{"smart":"claude-sonnet-4-6","cheap":"grok-3-mini"}}"#,
        )
        .expect("write local settings");

        // when
        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("config should load");

        // then
        let aliases = loaded.aliases();
        assert_eq!(
            aliases.get("fast").map(String::as_str),
            Some("claude-haiku-4-5-20251213")
        );
        assert_eq!(
            aliases.get("smart").map(String::as_str),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            aliases.get("cheap").map(String::as_str),
            Some("grok-3-mini")
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn empty_settings_file_loads_defaults() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(home.join("settings.json"), "").expect("write empty settings");

        // when
        let loaded = ConfigLoader::new(&cwd, &home)
            .load()
            .expect("empty settings should still load");

        // then
        assert_eq!(loaded.loaded_entries().len(), 1);
        assert_eq!(loaded.permission_mode(), None);
        assert_eq!(loaded.plugins().enabled_plugins().len(), 0);

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn deep_merge_objects_merges_nested_maps() {
        // given
        let mut target = JsonValue::parse(r#"{"env":{"A":"1","B":"2"},"model":"haiku"}"#)
            .expect("target JSON should parse")
            .as_object()
            .expect("target should be an object")
            .clone();
        let source =
            JsonValue::parse(r#"{"env":{"B":"override","C":"3"},"sandbox":{"enabled":true}}"#)
                .expect("source JSON should parse")
                .as_object()
                .expect("source should be an object")
                .clone();

        // when
        deep_merge_objects(&mut target, &source);

        // then
        let env = target
            .get("env")
            .and_then(JsonValue::as_object)
            .expect("env should remain an object");
        assert_eq!(env.get("A"), Some(&JsonValue::String("1".to_string())));
        assert_eq!(
            env.get("B"),
            Some(&JsonValue::String("override".to_string()))
        );
        assert_eq!(env.get("C"), Some(&JsonValue::String("3".to_string())));
        assert!(target.contains_key("sandbox"));
    }

    #[test]
    fn rejects_invalid_hook_entries_before_merge() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        let project_settings = cwd.join(".nexus").join("sudocode").join("settings.json");
        fs::create_dir_all(cwd.join(".nexus").join("sudocode")).expect("project config dir");
        fs::create_dir_all(&home).expect("home config dir");

        fs::write(
            home.join("settings.json"),
            r#"{"hooks":{"PreToolUse":["base"]}}"#,
        )
        .expect("write user settings");
        fs::write(
            &project_settings,
            r#"{"hooks":{"PreToolUse":["project",42]}}"#,
        )
        .expect("write invalid project settings");

        // when
        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        // then — config validation now catches the mixed array before the hooks parser
        let rendered = error.to_string();
        assert!(
            rendered.contains("hooks.PreToolUse")
                && rendered.contains("must be an array of strings"),
            "expected validation error for hooks.PreToolUse, got: {rendered}"
        );
        assert!(!rendered.contains("merged settings.hooks"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn permission_mode_aliases_resolve_to_expected_modes() {
        // given / when / then
        assert_eq!(
            parse_permission_mode_label("plan", "test").expect("plan should resolve"),
            ResolvedPermissionMode::ReadOnly
        );
        assert_eq!(
            parse_permission_mode_label("acceptEdits", "test").expect("acceptEdits should resolve"),
            ResolvedPermissionMode::WorkspaceWrite
        );
        assert_eq!(
            parse_permission_mode_label("dontAsk", "test").expect("dontAsk should resolve"),
            ResolvedPermissionMode::DangerFullAccess
        );
    }

    #[test]
    fn hook_config_merge_preserves_uniques() {
        // given
        let base = RuntimeHookConfig::new(
            vec!["pre-a".to_string()],
            vec!["post-a".to_string()],
            vec!["failure-a".to_string()],
        );
        let overlay = RuntimeHookConfig::new(
            vec!["pre-a".to_string(), "pre-b".to_string()],
            vec!["post-a".to_string(), "post-b".to_string()],
            vec!["failure-b".to_string()],
        );

        // when
        let merged = base.merged(&overlay);

        // then
        assert_eq!(
            merged.pre_tool_use(),
            &["pre-a".to_string(), "pre-b".to_string()]
        );
        assert_eq!(
            merged.post_tool_use(),
            &["post-a".to_string(), "post-b".to_string()]
        );
        assert_eq!(
            merged.post_tool_use_failure(),
            &["failure-a".to_string(), "failure-b".to_string()]
        );
    }

    #[test]
    fn plugin_state_falls_back_to_default_for_unknown_plugin() {
        // given
        let mut config = RuntimePluginConfig::default();
        config.set_plugin_state("known".to_string(), true);

        // when / then
        assert!(config.state_for("known", false));
        assert!(config.state_for("missing", true));
        assert!(!config.state_for("missing", false));
    }

    #[test]
    fn validates_unknown_top_level_keys_with_line_and_field_name() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        let user_settings = home.join("settings.json");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            &user_settings,
            "{\n  \"model\": \"opus\",\n  \"telemetry\": true\n}\n",
        )
        .expect("write user settings");

        // when
        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        // then
        let rendered = error.to_string();
        assert!(
            rendered.contains(&user_settings.display().to_string()),
            "error should include file path, got: {rendered}"
        );
        assert!(
            rendered.contains("line 3"),
            "error should include line number, got: {rendered}"
        );
        assert!(
            rendered.contains("telemetry"),
            "error should name the offending field, got: {rendered}"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn validates_deprecated_top_level_keys_with_replacement_guidance() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        let user_settings = home.join("settings.json");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            &user_settings,
            "{\n  \"model\": \"opus\",\n  \"allowedTools\": [\"Read\"]\n}\n",
        )
        .expect("write user settings");

        // when
        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        // then
        let rendered = error.to_string();
        assert!(
            rendered.contains(&user_settings.display().to_string()),
            "error should include file path, got: {rendered}"
        );
        assert!(
            rendered.contains("line 3"),
            "error should include line number, got: {rendered}"
        );
        assert!(
            rendered.contains("allowedTools"),
            "error should call out the unknown field, got: {rendered}"
        );
        // allowedTools is an unknown key; validator should name it in the error
        assert!(
            rendered.contains("allowedTools"),
            "error should name the offending field, got: {rendered}"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn validates_wrong_type_for_known_field_with_field_path() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        let user_settings = home.join("settings.json");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(
            &user_settings,
            "{\n  \"hooks\": {\n    \"PreToolUse\": \"not-an-array\"\n  }\n}\n",
        )
        .expect("write user settings");

        // when
        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        // then
        let rendered = error.to_string();
        assert!(
            rendered.contains(&user_settings.display().to_string()),
            "error should include file path, got: {rendered}"
        );
        assert!(
            rendered.contains("hooks"),
            "error should include field path component 'hooks', got: {rendered}"
        );
        assert!(
            rendered.contains("PreToolUse"),
            "error should describe the type mismatch, got: {rendered}"
        );
        assert!(
            rendered.contains("array"),
            "error should describe the expected type, got: {rendered}"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn unknown_top_level_key_suggests_closest_match() {
        // given
        let root = temp_dir();
        let cwd = root.join("project");
        let home = root.join("home").join(".nexus").join("sudocode");
        let user_settings = home.join("settings.json");
        fs::create_dir_all(&home).expect("home config dir");
        fs::create_dir_all(&cwd).expect("project dir");
        fs::write(&user_settings, "{\n  \"modle\": \"opus\"\n}\n").expect("write user settings");

        // when
        let error = ConfigLoader::new(&cwd, &home)
            .load()
            .expect_err("config should fail");

        // then
        let rendered = error.to_string();
        assert!(
            rendered.contains("modle"),
            "error should name the offending field, got: {rendered}"
        );
        assert!(
            rendered.contains("model"),
            "error should suggest the closest known key, got: {rendered}"
        );

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_sudocode_json_with_inline_credentials() {
        let root = temp_dir();
        let home = root.join("home").join(".nexus").join("sudocode");
        fs::create_dir_all(&home).expect("config dir");

        let json = r#"{
          "auth_modes": {
            "subscription": {
              "claude": {
                "baseUrl": "https://api.anthropic.com",
                "token": "sk-test-oauth-token"
              }
            },
            "proxy": {
              "sudorouter": {
                "baseUrl": "https://hk.sudorouter.ai/v1",
                "apiKey": "sk-test-proxy-key"
              }
            },
            "api-key": {
              "anthropic": {
                "baseUrl": "https://api.anthropic.com",
                "apiKey": "sk-test-anthropic-key"
              }
            }
          },
          "models": {
            "opus": {
              "alias": "opus",
              "name": "Claude Opus 4.6",
              "input": ["text"],
              "providers": {
                "subscription": { "provider": "claude", "model": "claude-opus-4-6" },
                "proxy":        { "provider": "sudorouter", "model": "claude-opus-4-6", "api": "openai-completions" },
                "api-key":      { "provider": "anthropic", "model": "claude-opus-4-6" }
              }
            },
            "deepseek": {
              "alias": "deepseek",
              "name": "DeepSeek V3",
              "input": ["text"],
              "providers": {
                "proxy": { "provider": "sudorouter", "model": "deepseek-chat", "api": "openai-completions" }
              }
            }
          }
        }"#;

        fs::write(home.join("sudocode.json"), json).expect("write sudocode.json");

        let cwd = root.join("project");
        fs::create_dir_all(&cwd).expect("project dir");
        let loader = ConfigLoader::new(&cwd, &home);
        let config = loader.load_sudocode_config().expect("should parse");

        // auth_modes
        assert_eq!(config.auth_modes.len(), 3);
        let sub_claude = &config.auth_modes["subscription"]["claude"];
        assert_eq!(sub_claude.base_url, "https://api.anthropic.com");
        assert_eq!(sub_claude.token.as_deref(), Some("sk-test-oauth-token"));
        assert!(sub_claude.token_env.is_none());

        let proxy = &config.auth_modes["proxy"]["sudorouter"];
        assert_eq!(proxy.base_url, "https://hk.sudorouter.ai/v1");
        assert_eq!(proxy.api_key.as_deref(), Some("sk-test-proxy-key"));

        let apikey = &config.auth_modes["api-key"]["anthropic"];
        assert_eq!(apikey.base_url, "https://api.anthropic.com");
        assert_eq!(apikey.api_key.as_deref(), Some("sk-test-anthropic-key"));

        // models — only entries from the file, no built-in merging.
        assert_eq!(config.models.len(), 2);
        let opus = &config.models["opus"];
        assert_eq!(opus.name, "Claude Opus 4.6");
        assert_eq!(opus.providers.len(), 3);
        assert_eq!(opus.providers["subscription"].provider, "claude");
        assert_eq!(opus.providers["subscription"].model, "claude-opus-4-6");
        assert!(opus.providers["subscription"].api.is_none());
        assert_eq!(opus.providers["proxy"].provider, "sudorouter");
        assert_eq!(
            opus.providers["proxy"].api.as_deref(),
            Some("openai-completions")
        );
        assert_eq!(opus.providers["api-key"].provider, "anthropic");

        let deepseek = &config.models["deepseek"];
        assert_eq!(deepseek.providers.len(), 1);
        assert_eq!(deepseek.providers["proxy"].model, "deepseek-chat");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn load_sudocode_config_errors_when_file_is_missing() {
        let root = temp_dir();
        let home = root.join("empty-home");
        fs::create_dir_all(&home).expect("config dir");
        let cwd = root.join("project");
        fs::create_dir_all(&cwd).expect("project dir");

        let loader = ConfigLoader::new(&cwd, &home);
        let result = loader.load_sudocode_config();
        assert!(result.is_err(), "should fail when sudocode.json is missing");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("missing sudocode.json"),
            "error should mention missing file: {err_msg}"
        );

        fs::remove_dir_all(root).expect("cleanup");
    }
}
