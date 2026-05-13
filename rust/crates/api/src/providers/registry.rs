//! Config-driven provider & model registry.
//!
//! Resolves model alias → auth mode → provider → connection details using the
//! `sudocode.json` config file.  The config is the single source of truth for
//! model metadata, token limits, and provider routing.

use std::path::PathBuf;

use serde::Serialize;

use super::{AuthMode, ProviderKind};
use crate::error::ApiError;
use crate::types::MessageRequest;

// Re-export the config types from the runtime crate so consumers can use them
// via `api::providers::registry::*` without depending on runtime directly.
pub use runtime::config::{
    ModelConfigEntry, ModelProviderMapping, ProviderConnectionConfig, SudoCodeConfig,
};

// ---------------------------------------------------------------------------
// Types (API-layer only)
// ---------------------------------------------------------------------------

/// Wire protocol / API format for talking to a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFormat {
    /// Native Anthropic Messages API (`/v1/messages`).
    AnthropicMessages,
    /// OpenAI-compatible chat completions API (`/v1/chat/completions`).
    OpenAiCompletions,
    /// `OpenAI` Responses API (`/v1/responses`).
    OpenAiResponses,
    /// Google Gemini `GenerateContent` API.
    GeminiGenerateContent,
}

/// Resolved credential for authenticating with a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Direct API key string.
    ApiKey(String),
    /// Bearer / OAuth token string.
    Token(String),
    /// Path to a credentials file (e.g. `~/.claude/credentials.json`).
    AuthFile(PathBuf),
    /// No credential available — provider may not require one.
    None,
}

/// Fully resolved provider information — everything needed to build a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub kind: ProviderKind,
    pub api_format: ApiFormat,
    pub base_url: String,
    pub credential: Credential,
    /// The wire model ID to send to the provider.
    pub model_id: String,
}

/// Token-limit metadata for a wire model ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTokenLimit {
    pub max_output_tokens: u32,
    pub context_window_tokens: u32,
}

// ---------------------------------------------------------------------------
// Hardcoded model specs (keyed by wire model ID)
// ---------------------------------------------------------------------------

/// Canonical model capabilities table. Wire model IDs referenced in
/// `sudocode.json` provider mappings should appear here so that token-limit
/// preflight checks and max-output-token defaults work correctly.
const MODEL_SPECS: &[(&str, ModelTokenLimit)] = &[
    // Anthropic
    (
        "claude-opus-4-6",
        ModelTokenLimit {
            max_output_tokens: 32_000,
            context_window_tokens: 200_000,
        },
    ),
    (
        "claude-sonnet-4-6",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 200_000,
        },
    ),
    (
        "claude-haiku-4-5-20251213",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 200_000,
        },
    ),
    // xAI
    (
        "grok-3",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 131_072,
        },
    ),
    (
        "grok-3-mini",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 131_072,
        },
    ),
    (
        "grok-2",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 131_072,
        },
    ),
    // Moonshot / Kimi (via DashScope)
    (
        "kimi-k2.5",
        ModelTokenLimit {
            max_output_tokens: 16_384,
            context_window_tokens: 256_000,
        },
    ),
    (
        "kimi-k1.5",
        ModelTokenLimit {
            max_output_tokens: 16_384,
            context_window_tokens: 256_000,
        },
    ),
    // OpenAI / Codex
    (
        "gpt-4.1",
        ModelTokenLimit {
            max_output_tokens: 32_768,
            context_window_tokens: 1_047_576,
        },
    ),
    (
        "gpt-4.1-mini",
        ModelTokenLimit {
            max_output_tokens: 32_768,
            context_window_tokens: 1_047_576,
        },
    ),
    (
        "gpt-4.1-nano",
        ModelTokenLimit {
            max_output_tokens: 32_768,
            context_window_tokens: 1_047_576,
        },
    ),
    (
        "gpt-5.4",
        ModelTokenLimit {
            max_output_tokens: 128_000,
            context_window_tokens: 1_000_000,
        },
    ),
    (
        "gpt-5.4-mini",
        ModelTokenLimit {
            max_output_tokens: 128_000,
            context_window_tokens: 400_000,
        },
    ),
    (
        "gpt-5.4-nano",
        ModelTokenLimit {
            max_output_tokens: 128_000,
            context_window_tokens: 400_000,
        },
    ),
    // Qwen (via DashScope)
    (
        "qwen-plus",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 131_072,
        },
    ),
    // Gemini (via proxy)
    (
        "gemini-3.1-pro-preview",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 200_000,
        },
    ),
    (
        "gemini-3-flash-preview",
        ModelTokenLimit {
            max_output_tokens: 64_000,
            context_window_tokens: 200_000,
        },
    ),
    // Gemini
    (
        "gemini-3.1-pro-preview",
        ModelTokenLimit {
            max_output_tokens: 65_536,
            context_window_tokens: 1_048_576,
        },
    ),
    (
        "gemini-3-flash-preview",
        ModelTokenLimit {
            max_output_tokens: 65_536,
            context_window_tokens: 1_048_576,
        },
    ),
];

// ---------------------------------------------------------------------------
// Model spec lookups
// ---------------------------------------------------------------------------

/// Look up token limits for a wire model ID.
///
/// Handles provider-prefixed model IDs (e.g. `openai/gpt-4.1-mini`) by
/// stripping the prefix before lookup.
#[must_use]
pub fn model_token_limit(model_id: &str) -> Option<ModelTokenLimit> {
    let base_model = model_id.rsplit('/').next().unwrap_or(model_id);
    MODEL_SPECS
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(base_model))
        .map(|(_, limit)| *limit)
}

/// Look up token limits by resolving an alias through config first, then
/// looking up the wire model ID in the hardcoded specs.
#[must_use]
pub fn model_token_limit_from_config(
    config: &SudoCodeConfig,
    alias: &str,
) -> Option<ModelTokenLimit> {
    let wire_id = resolve_model_alias_from_config(config, alias);
    model_token_limit(&wire_id)
}

/// Return the effective max output tokens for a wire model ID.
///
/// Uses a heuristic default (32k for opus, 64k otherwise) and caps it
/// against the model's registered `max_output_tokens` when available.
/// This prevents requesting more output tokens than the model supports
/// while keeping sensible defaults for unknown models.
#[must_use]
pub fn max_tokens_for_model(model_id: &str) -> u32 {
    let heuristic = if model_id.contains("opus") {
        32_000
    } else {
        64_000
    };

    model_token_limit(model_id)
        .map(|limit| heuristic.min(limit.max_output_tokens))
        .unwrap_or(heuristic)
}

/// Return the effective max output tokens by resolving an alias through
/// config first.  Applies the same heuristic cap as [`max_tokens_for_model`].
#[must_use]
pub fn max_tokens_for_model_from_config(config: &SudoCodeConfig, alias: &str) -> u32 {
    let wire_id = resolve_model_alias_from_config(config, alias);
    max_tokens_for_model(&wire_id)
}

/// Returns the effective max output tokens for a model, preferring a plugin
/// override when present. Falls back to spec defaults.
#[must_use]
pub fn max_tokens_for_model_with_override(model_id: &str, plugin_override: Option<u32>) -> u32 {
    plugin_override.unwrap_or_else(|| max_tokens_for_model(model_id))
}

/// Resolve a model alias through config to the wire model ID.
///
/// Looks up the alias in `config.models`, returning the wire model ID from
/// the first available provider mapping. If not found, returns the input
/// unchanged.
#[must_use]
pub fn resolve_model_alias_from_config(config: &SudoCodeConfig, alias: &str) -> String {
    let trimmed = alias.trim();
    if let Some(entry) = resolve_model(config, trimmed) {
        if let Some(mapping) = entry.providers.values().next() {
            return mapping.model.clone();
        }
    }
    trimmed.to_string()
}

/// Local preflight check: reject requests whose estimated token count
/// exceeds the model's context window (looked up from hardcoded specs).
pub fn preflight_message_request(request: &MessageRequest) -> Result<(), ApiError> {
    let Some(limit) = model_token_limit(&request.model) else {
        return Ok(());
    };

    let estimated_input_tokens = estimate_message_request_input_tokens(request);
    let estimated_total_tokens = estimated_input_tokens.saturating_add(request.max_tokens);
    if estimated_total_tokens > limit.context_window_tokens {
        return Err(ApiError::ContextWindowExceeded {
            model: request.model.clone(),
            estimated_input_tokens,
            requested_output_tokens: request.max_tokens,
            estimated_total_tokens,
            context_window_tokens: limit.context_window_tokens,
        });
    }

    Ok(())
}

fn estimate_message_request_input_tokens(request: &MessageRequest) -> u32 {
    let mut estimate = estimate_serialized_tokens(&request.messages);
    estimate = estimate.saturating_add(estimate_serialized_tokens(&request.system));
    estimate = estimate.saturating_add(estimate_serialized_tokens(&request.tools));
    estimate = estimate.saturating_add(estimate_serialized_tokens(&request.tool_choice));
    estimate
}

fn estimate_serialized_tokens<T: Serialize>(value: &T) -> u32 {
    serde_json::to_vec(value)
        .ok()
        .map_or(0, |bytes| (bytes.len() / 4 + 1) as u32)
}

// ---------------------------------------------------------------------------
// SudoCodeConfig helpers
// ---------------------------------------------------------------------------

/// Look up a model by alias (case-insensitive).
#[must_use]
pub fn resolve_model<'a>(config: &'a SudoCodeConfig, alias: &str) -> Option<&'a ModelConfigEntry> {
    resolve_model_for_mode(config, alias, None)
}

/// Look up a model by alias, preferring entries that support the given auth mode
/// when multiple entries share the same wire model ID.
fn resolve_model_for_mode<'a>(
    config: &'a SudoCodeConfig,
    alias: &str,
    auth_mode: Option<&str>,
) -> Option<&'a ModelConfigEntry> {
    let key = alias.trim().to_ascii_lowercase();
    // Direct alias lookup first.
    if let Some(entry) = config.models.get(&key) {
        return Some(entry);
    }
    // Fall back: match by wire model ID in any provider mapping.
    // When an auth mode is specified, prefer entries that support it.
    let mut fallback: Option<&'a ModelConfigEntry> = None;
    for entry in config.models.values() {
        let has_wire_match = entry
            .providers
            .values()
            .any(|m| m.model.eq_ignore_ascii_case(&key));
        if !has_wire_match {
            continue;
        }
        if let Some(mode) = auth_mode {
            if entry.providers.contains_key(mode) {
                return Some(entry);
            }
        }
        if fallback.is_none() {
            fallback = Some(entry);
        }
    }
    fallback
}

/// Look up connection config for a provider under a given auth mode.
#[must_use]
fn connection_for<'a>(
    config: &'a SudoCodeConfig,
    auth_mode: &str,
    provider_name: &str,
) -> Option<&'a ProviderConnectionConfig> {
    config.auth_modes.get(auth_mode)?.get(provider_name)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolve a model alias + optional auth mode through the config into a fully
/// resolved provider specification.
///
/// Resolution flow:
/// 1. Look up `models.<alias>` → get available auth modes
/// 2. If `explicit_auth` specified, use it; otherwise pick first available
/// 3. From `models.<alias>.providers.<mode>` get `{ provider, model, api? }`
/// 4. From `auth_modes.<mode>.<provider>` get connection details
/// 5. Resolve wire format: non-proxy → infer from provider type; proxy → use `api` field
/// 6. Resolve credentials from connection config
pub fn resolve_provider_from_config(
    model_alias: &str,
    explicit_auth: Option<AuthMode>,
    config: &SudoCodeConfig,
) -> Result<ResolvedProvider, ApiError> {
    let alias_lower = model_alias.trim().to_ascii_lowercase();

    // 1. Look up the model in config.
    let model_config = resolve_model(config, &alias_lower).ok_or_else(|| {
        ApiError::Configuration(format!(
            "model alias '{model_alias}' not found in sudocode.json"
        ))
    })?;

    // 2. Determine the auth mode to use.
    let auth_mode_str = if let Some(mode) = explicit_auth {
        let s = mode.as_str();
        if !model_config.providers.contains_key(s) {
            return Err(ApiError::Configuration(format!(
                "auth mode '{}' is not available for model '{}'. Available: {}",
                s,
                model_alias,
                model_config
                    .providers
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        s.to_string()
    } else {
        // Auto-detect from model config: pick the first auth mode the
        // model supports in priority order subscription > proxy > api-key.
        const PRIORITY: &[&str] = &["subscription", "proxy", "api-key"];
        PRIORITY
            .iter()
            .find(|m| model_config.providers.contains_key(**m))
            .map(|m| (*m).to_string())
            .ok_or_else(|| {
                ApiError::Configuration(format!(
                    "model '{model_alias}' has no provider mappings in sudocode.json"
                ))
            })?
    };

    // 3. Get the provider mapping for this auth mode.
    let mapping = model_config.providers.get(&auth_mode_str).ok_or_else(|| {
        ApiError::Configuration(format!(
            "no provider mapping for auth mode '{auth_mode_str}' on model '{model_alias}'"
        ))
    })?;

    // 4. Get connection config.
    let connection =
        connection_for(config, &auth_mode_str, &mapping.provider).ok_or_else(|| {
            ApiError::Configuration(format!(
                "provider '{}' not found under auth_modes.{} in sudocode.json",
                mapping.provider, auth_mode_str
            ))
        })?;

    // 5. Determine API format.
    let api_format = resolve_api_format(&auth_mode_str, &mapping.provider, mapping.api.as_deref())?;

    // 6. Resolve credential.
    let credential = resolve_credential(&auth_mode_str, &mapping.provider, connection)?;

    // 7. Determine provider kind from the provider name / api format.
    let kind = infer_provider_kind(&mapping.provider, api_format);

    Ok(ResolvedProvider {
        kind,
        api_format,
        base_url: connection.base_url.clone(),
        credential,
        model_id: mapping.model.clone(),
    })
}

/// Resolve the wire API format.
///
/// - Proxy providers: must have an `api` field (`"openai-completions"` or `"openai-responses"`).
/// - Non-proxy providers: inferred from the provider name.
fn resolve_api_format(
    auth_mode: &str,
    provider_name: &str,
    api_override: Option<&str>,
) -> Result<ApiFormat, ApiError> {
    // If there's an explicit `api` field, use it.
    if let Some(api) = api_override {
        return match api {
            "openai-completions" => Ok(ApiFormat::OpenAiCompletions),
            "openai-responses" => Ok(ApiFormat::OpenAiResponses),
            other => Err(ApiError::Configuration(format!(
                "unknown api format '{other}' for provider '{provider_name}' under mode '{auth_mode}'"
            ))),
        };
    }

    // For proxy mode without an explicit `api`, default to OpenAI completions.
    if auth_mode == "proxy" {
        return Ok(ApiFormat::OpenAiCompletions);
    }

    // Infer from provider name.
    match provider_name {
        "anthropic" | "claude" => Ok(ApiFormat::AnthropicMessages),
        "codex" => Ok(ApiFormat::OpenAiResponses),
        "gemini" => Ok(ApiFormat::GeminiGenerateContent),
        // Known and unknown providers default to OpenAI-compatible.
        _ => Ok(ApiFormat::OpenAiCompletions),
    }
}

/// Resolve credentials from the connection config.
///
/// - `api-key` mode: inline `apiKey` → `apiKeyEnv` from env
/// - `subscription` mode: inline `token` → `tokenEnv` from env → `authFile`
/// - `proxy` mode: inline `apiKey` → `apiKeyEnv` from env
fn resolve_credential(
    auth_mode: &str,
    provider_name: &str,
    connection: &ProviderConnectionConfig,
) -> Result<Credential, ApiError> {
    match auth_mode {
        "api-key" | "proxy" => {
            // Inline API key takes priority.
            if let Some(key) = &connection.api_key {
                if !key.is_empty() {
                    return Ok(Credential::ApiKey(key.clone()));
                }
            }
            // Then env var.
            if let Some(env_name) = &connection.api_key_env {
                if let Ok(val) = std::env::var(env_name) {
                    if !val.trim().is_empty() {
                        return Ok(Credential::ApiKey(val));
                    }
                }
            }
            // For proxy mode, allow no credential (some proxies don't need auth).
            if auth_mode == "proxy" {
                return Ok(Credential::None);
            }
            Err(ApiError::Configuration(format!(
                "no API key available for provider under auth mode '{auth_mode}'. \
                 Set apiKey or apiKeyEnv in sudocode.json, or set the appropriate env var."
            )))
        }
        "subscription" => {
            // 1. For claude/anthropic providers, CLAUDE_CODE_OAUTH_TOKEN env
            //    var has highest priority.
            if matches!(provider_name, "claude" | "anthropic") {
                if let Ok(val) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
                    if !val.trim().is_empty() {
                        return Ok(Credential::Token(val));
                    }
                }
            }
            // 2. Inline token (skip obvious placeholders like `<YOUR_...>`).
            if let Some(token) = &connection.token {
                if !token.is_empty() && !token.starts_with('<') {
                    return Ok(Credential::Token(token.clone()));
                }
            }
            // 4. Auth file.
            if let Some(path) = &connection.auth_file {
                let expanded = expand_tilde(path);
                if expanded.exists() {
                    return Ok(Credential::AuthFile(expanded));
                }
            }
            Err(ApiError::Configuration(
                "no token available for subscription provider. \
                 Set token, tokenEnv, or authFile in sudocode.json."
                    .to_string(),
            ))
        }
        _ => {
            // Unknown auth mode — try apiKey then token.
            if let Some(key) = &connection.api_key {
                if !key.is_empty() {
                    return Ok(Credential::ApiKey(key.clone()));
                }
            }
            if let Some(token) = &connection.token {
                if !token.is_empty() {
                    return Ok(Credential::Token(token.clone()));
                }
            }
            Ok(Credential::None)
        }
    }
}

/// Infer `ProviderKind` from the provider name and API format.
fn infer_provider_kind(provider_name: &str, api_format: ApiFormat) -> ProviderKind {
    match api_format {
        ApiFormat::AnthropicMessages => ProviderKind::Anthropic,
        ApiFormat::GeminiGenerateContent => ProviderKind::Gemini,
        ApiFormat::OpenAiCompletions | ApiFormat::OpenAiResponses => match provider_name {
            "xai" => ProviderKind::Xai,
            "codex" => ProviderKind::Codex,
            _ => ProviderKind::OpenAi,
        },
    }
}

/// Expand `~` prefix to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// List available auth modes for a model alias, in lexicographic order.
    fn available_auth_modes<'a>(config: &'a SudoCodeConfig, alias: &str) -> Vec<&'a str> {
        resolve_model(config, alias)
            .map(|m| m.providers.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_lines)]
    fn sample_config() -> SudoCodeConfig {
        let mut auth_modes = BTreeMap::new();

        // subscription mode
        let mut subscription = BTreeMap::new();
        subscription.insert(
            "claude".to_string(),
            ProviderConnectionConfig {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: None,
                api_key_env: None,
                token: None,
                token_env: Some("CLAUDE_CODE_OAUTH_TOKEN".to_string()),
                auth_file: Some("~/.claude/credentials.json".to_string()),
            },
        );
        subscription.insert(
            "codex".to_string(),
            ProviderConnectionConfig {
                base_url: "https://chatgpt.com/backend-api/codex".to_string(),
                api_key: None,
                api_key_env: None,
                token: None,
                token_env: None,
                auth_file: Some("~/.codex/auth.json".to_string()),
            },
        );
        auth_modes.insert("subscription".to_string(), subscription);

        // proxy mode
        let mut proxy = BTreeMap::new();
        proxy.insert(
            "sudorouter".to_string(),
            ProviderConnectionConfig {
                base_url: "https://hk.sudorouter.ai/v1".to_string(),
                api_key: Some("sk-test-key".to_string()),
                api_key_env: None,
                token: None,
                token_env: None,
                auth_file: None,
            },
        );
        auth_modes.insert("proxy".to_string(), proxy);

        // api-key mode
        let mut api_key = BTreeMap::new();
        api_key.insert(
            "anthropic".to_string(),
            ProviderConnectionConfig {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: None,
                api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                token: None,
                token_env: None,
                auth_file: None,
            },
        );
        api_key.insert(
            "xai".to_string(),
            ProviderConnectionConfig {
                base_url: "https://api.x.ai/v1".to_string(),
                api_key: None,
                api_key_env: Some("XAI_API_KEY".to_string()),
                token: None,
                token_env: None,
                auth_file: None,
            },
        );
        api_key.insert(
            "openai".to_string(),
            ProviderConnectionConfig {
                base_url: "https://api.openai.com/v1".to_string(),
                api_key: None,
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                token: None,
                token_env: None,
                auth_file: None,
            },
        );
        auth_modes.insert("api-key".to_string(), api_key);

        // models
        let mut models = BTreeMap::new();
        let mut opus_providers = BTreeMap::new();
        opus_providers.insert(
            "subscription".to_string(),
            ModelProviderMapping {
                provider: "claude".to_string(),
                model: "claude-opus-4-6".to_string(),
                api: None,
            },
        );
        opus_providers.insert(
            "proxy".to_string(),
            ModelProviderMapping {
                provider: "sudorouter".to_string(),
                model: "claude-opus-4-6".to_string(),
                api: Some("openai-completions".to_string()),
            },
        );
        opus_providers.insert(
            "api-key".to_string(),
            ModelProviderMapping {
                provider: "anthropic".to_string(),
                model: "claude-opus-4-6".to_string(),
                api: None,
            },
        );
        models.insert(
            "opus".to_string(),
            ModelConfigEntry {
                alias: "opus".to_string(),
                name: "Claude Opus 4.6".to_string(),
                input: vec!["text".to_string()],
                providers: opus_providers,
            },
        );

        let mut grok_providers = BTreeMap::new();
        grok_providers.insert(
            "api-key".to_string(),
            ModelProviderMapping {
                provider: "xai".to_string(),
                model: "grok-3".to_string(),
                api: None,
            },
        );
        models.insert(
            "grok".to_string(),
            ModelConfigEntry {
                alias: "grok".to_string(),
                name: "Grok 3".to_string(),
                input: vec!["text".to_string()],
                providers: grok_providers,
            },
        );

        // codex model
        let mut codex_providers = BTreeMap::new();
        codex_providers.insert(
            "subscription".to_string(),
            ModelProviderMapping {
                provider: "codex".to_string(),
                model: "gpt-5.4-mini".to_string(),
                api: None,
            },
        );
        codex_providers.insert(
            "api-key".to_string(),
            ModelProviderMapping {
                provider: "openai".to_string(),
                model: "gpt-5.4-mini".to_string(),
                api: None,
            },
        );
        models.insert(
            "codex".to_string(),
            ModelConfigEntry {
                alias: "codex".to_string(),
                name: "GPT 5.4 Mini".to_string(),
                input: vec!["text".to_string()],
                providers: codex_providers,
            },
        );

        SudoCodeConfig {
            auth_modes,
            models,
            web_search: Default::default(),
        }
    }

    #[test]
    fn resolve_model_finds_alias() {
        let config = sample_config();
        assert!(resolve_model(&config, "opus").is_some());
        assert!(resolve_model(&config, "OPUS").is_some()); // case insensitive
        assert!(resolve_model(&config, "unknown").is_none());
    }

    #[test]
    fn available_auth_modes_lists_keys() {
        let config = sample_config();
        let modes = available_auth_modes(&config, "opus");
        assert!(modes.contains(&"subscription"));
        assert!(modes.contains(&"proxy"));
        assert!(modes.contains(&"api-key"));

        let grok_modes = available_auth_modes(&config, "grok");
        assert_eq!(grok_modes, vec!["api-key"]);
    }

    #[test]
    fn connection_for_resolves() {
        let config = sample_config();
        let conn =
            connection_for(&config, "proxy", "sudorouter").expect("should find proxy sudorouter");
        assert_eq!(conn.base_url, "https://hk.sudorouter.ai/v1");
        assert_eq!(conn.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn resolve_provider_proxy_with_inline_key() {
        let config = sample_config();
        let resolved = resolve_provider_from_config("opus", Some(AuthMode::Proxy), &config)
            .expect("should resolve");
        assert_eq!(resolved.kind, ProviderKind::OpenAi);
        assert_eq!(resolved.api_format, ApiFormat::OpenAiCompletions);
        assert_eq!(resolved.base_url, "https://hk.sudorouter.ai/v1");
        assert_eq!(
            resolved.credential,
            Credential::ApiKey("sk-test-key".to_string())
        );
        assert_eq!(resolved.model_id, "claude-opus-4-6");
    }

    #[test]
    fn resolve_provider_picks_first_mode_by_default() {
        let config = sample_config();
        // First mode in BTreeMap for "grok" is "api-key" (only one).
        let resolved = resolve_provider_from_config("grok", None, &config);
        // This will fail credential resolution since XAI_API_KEY is not set in
        // test env, but the error should mention the right auth mode.
        match resolved {
            Err(ApiError::Configuration(msg)) => {
                assert!(msg.contains("API key"), "unexpected error: {msg}");
            }
            Ok(r) => {
                // If XAI_API_KEY happens to be set in the env, that's fine too.
                assert_eq!(r.kind, ProviderKind::Xai);
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn resolve_provider_rejects_unavailable_mode() {
        let config = sample_config();
        let result = resolve_provider_from_config("grok", Some(AuthMode::Subscription), &config);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not available"), "unexpected error: {msg}");
    }

    #[test]
    fn resolve_api_format_infers_correctly() {
        assert_eq!(
            resolve_api_format("api-key", "anthropic", None).unwrap(),
            ApiFormat::AnthropicMessages
        );
        assert_eq!(
            resolve_api_format("api-key", "openai", None).unwrap(),
            ApiFormat::OpenAiCompletions
        );
        assert_eq!(
            resolve_api_format("proxy", "any", Some("openai-responses")).unwrap(),
            ApiFormat::OpenAiResponses
        );
        assert_eq!(
            resolve_api_format("proxy", "any", None).unwrap(),
            ApiFormat::OpenAiCompletions
        );
    }

    #[test]
    fn infer_provider_kind_maps_correctly() {
        assert_eq!(
            infer_provider_kind("anthropic", ApiFormat::AnthropicMessages),
            ProviderKind::Anthropic
        );
        assert_eq!(
            infer_provider_kind("xai", ApiFormat::OpenAiCompletions),
            ProviderKind::Xai
        );
        assert_eq!(
            infer_provider_kind("openai", ApiFormat::OpenAiCompletions),
            ProviderKind::OpenAi
        );
    }

    #[test]
    fn empty_config_is_empty() {
        let config = SudoCodeConfig::default();
        assert!(config.auth_modes.is_empty());
        assert!(config.models.is_empty());
    }

    #[test]
    fn resolve_codex_subscription_uses_codex_provider() {
        let mut config = sample_config();
        // Set an inline token so the test doesn't depend on ~/.codex/auth.json existing.
        config
            .auth_modes
            .get_mut("subscription")
            .unwrap()
            .get_mut("codex")
            .unwrap()
            .token = Some("codex-test-token".to_string());

        let resolved = resolve_provider_from_config("codex", Some(AuthMode::Subscription), &config)
            .expect("should resolve codex subscription");
        assert_eq!(resolved.kind, ProviderKind::Codex);
        assert_eq!(resolved.api_format, ApiFormat::OpenAiResponses);
        assert_eq!(resolved.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(resolved.model_id, "gpt-5.4-mini");
        assert_eq!(
            resolved.credential,
            Credential::Token("codex-test-token".to_string())
        );
    }

    #[test]
    fn resolve_codex_apikey_uses_openai_provider() {
        let config = sample_config();
        let resolved = resolve_provider_from_config("codex", Some(AuthMode::ApiKey), &config);
        // Will fail credential resolution since OPENAI_API_KEY is not set, but
        // if it happens to be set that's fine — check the provider routing.
        match resolved {
            Err(ApiError::Configuration(msg)) => {
                assert!(msg.contains("API key"), "unexpected error: {msg}");
            }
            Ok(r) => {
                assert_eq!(r.kind, ProviderKind::OpenAi);
                assert_eq!(r.api_format, ApiFormat::OpenAiCompletions);
                assert_eq!(r.base_url, "https://api.openai.com/v1");
                assert_eq!(r.model_id, "gpt-5.4-mini");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn resolve_api_format_codex_returns_responses() {
        assert_eq!(
            resolve_api_format("subscription", "codex", None).unwrap(),
            ApiFormat::OpenAiResponses
        );
    }

    #[test]
    fn infer_provider_kind_codex() {
        assert_eq!(
            infer_provider_kind("codex", ApiFormat::OpenAiResponses),
            ProviderKind::Codex
        );
    }

    #[test]
    fn expand_tilde_works() {
        let path = expand_tilde("~/.claude/credentials.json");
        if let Some(home) = std::env::var_os("HOME") {
            let expected = std::path::PathBuf::from(home).join(".claude/credentials.json");
            assert_eq!(path, expected);
        }
    }

    #[test]
    fn resolve_provider_with_inline_token_subscription() {
        let mut config = sample_config();
        // Set an inline token on the subscription claude provider.
        config
            .auth_modes
            .get_mut("subscription")
            .unwrap()
            .get_mut("claude")
            .unwrap()
            .token = Some("sk-inline-oauth-token".to_string());

        let resolved = resolve_provider_from_config("opus", Some(AuthMode::Subscription), &config)
            .expect("should resolve with inline token");
        assert_eq!(resolved.kind, ProviderKind::Anthropic);
        assert_eq!(resolved.api_format, ApiFormat::AnthropicMessages);
        assert_eq!(resolved.base_url, "https://api.anthropic.com");
        assert_eq!(
            resolved.credential,
            Credential::Token("sk-inline-oauth-token".to_string())
        );
        assert_eq!(resolved.model_id, "claude-opus-4-6");
    }

    #[test]
    fn caps_default_max_tokens_to_openai_model_limits() {
        assert_eq!(max_tokens_for_model("gpt-4.1-mini"), 32_768);
        assert_eq!(max_tokens_for_model("openai/gpt-4.1-mini"), 32_768);
        assert_eq!(max_tokens_for_model("gpt-5.4"), 64_000);
        assert_eq!(max_tokens_for_model("openai/gpt-5.4"), 64_000);
    }

    #[test]
    fn keeps_existing_max_token_heuristic() {
        assert_eq!(max_tokens_for_model("claude-opus-4-6"), 32_000);
        assert_eq!(max_tokens_for_model("grok-3"), 64_000);
        assert_eq!(max_tokens_for_model("gpt-5.4"), 64_000);
    }

    #[test]
    fn model_token_limit_resolves_prefixed_models() {
        assert_eq!(
            model_token_limit("openai/gpt-4.1-mini")
                .expect("openai/gpt-4.1-mini should be registered")
                .context_window_tokens,
            1_047_576
        );
        assert_eq!(
            model_token_limit("gpt-5.4")
                .expect("gpt-5.4 should be registered")
                .context_window_tokens,
            1_000_000
        );
    }

    #[test]
    fn preflight_blocks_oversized_requests_for_gpt_5_4() {
        use crate::types::{InputContentBlock, InputMessage};

        let request = MessageRequest {
            model: "gpt-5.4".to_string(),
            max_tokens: 64_000,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    text: "x".repeat(3_900_000),
                }],
            }],
            system: Some("Keep the answer short.".to_string()),
            tools: None,
            tool_choice: None,
            stream: true,
            ..Default::default()
        };

        let error = preflight_message_request(&request)
            .expect_err("oversized gpt-5.4 request should be rejected before the provider call");

        match error {
            ApiError::ContextWindowExceeded {
                model,
                requested_output_tokens,
                context_window_tokens,
                ..
            } => {
                assert_eq!(model, "gpt-5.4");
                assert_eq!(requested_output_tokens, 64_000);
                assert_eq!(context_window_tokens, 1_000_000);
            }
            other => panic!("expected context-window preflight failure, got {other:?}"),
        }
    }

    #[test]
    fn preflight_skips_unknown_models() {
        use crate::types::{InputContentBlock, InputMessage};

        let request = MessageRequest {
            model: "unknown-model".to_string(),
            max_tokens: 64_000,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    text: "hello".to_string(),
                }],
            }],
            system: None,
            tools: None,
            tool_choice: None,
            stream: true,
            ..Default::default()
        };

        assert!(preflight_message_request(&request).is_ok());
    }

    #[test]
    fn resolve_provider_with_inline_apikey() {
        let mut config = sample_config();
        // Set an inline API key on the api-key anthropic provider.
        config
            .auth_modes
            .get_mut("api-key")
            .unwrap()
            .get_mut("anthropic")
            .unwrap()
            .api_key = Some("sk-inline-api-key".to_string());

        let resolved = resolve_provider_from_config("opus", Some(AuthMode::ApiKey), &config)
            .expect("should resolve with inline api key");
        assert_eq!(resolved.kind, ProviderKind::Anthropic);
        assert_eq!(resolved.api_format, ApiFormat::AnthropicMessages);
        assert_eq!(resolved.base_url, "https://api.anthropic.com");
        assert_eq!(
            resolved.credential,
            Credential::ApiKey("sk-inline-api-key".to_string())
        );
        assert_eq!(resolved.model_id, "claude-opus-4-6");
    }
}
