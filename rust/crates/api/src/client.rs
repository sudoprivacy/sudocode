use telemetry::SessionTracer;

use crate::error::ApiError;
use crate::prompt_cache::{PromptCache, PromptCacheRecord, PromptCacheStats};
use crate::providers::anthropic::{self, AnthropicClient, AuthSource};
use crate::providers::codex::CodexClient;
use crate::providers::gemini::{self, GeminiClient};
use crate::providers::openai_compat::{self, OpenAiCompatClient, OpenAiCompatConfig};
use crate::providers::registry::{ApiFormat, Credential, ResolvedProvider};
use crate::providers::{AuthMode, ProviderKind};
use crate::types::{MessageRequest, MessageResponse, StreamEvent};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ProviderClient {
    Anthropic(AnthropicClient),
    Xai(OpenAiCompatClient),
    OpenAi(OpenAiCompatClient),
    Codex(CodexClient),
    Gemini(GeminiClient),
}

impl ProviderClient {
    /// Build a `ProviderClient` from a fully resolved provider config.
    ///
    /// This is the primary entry point for config-driven provider construction.
    /// The caller is responsible for calling `resolve_provider_from_config()`
    /// first to obtain the `ResolvedProvider`.
    #[allow(clippy::too_many_lines)]
    pub fn from_resolved(
        resolved: &ResolvedProvider,
        mode: Option<AuthMode>,
    ) -> Result<Self, ApiError> {
        match resolved.api_format {
            ApiFormat::AnthropicMessages => {
                let auth = match &resolved.credential {
                    Credential::ApiKey(key) => AuthSource::ApiKey(key.clone()),
                    Credential::Token(token) => AuthSource::BearerToken(token.clone()),
                    Credential::AuthFile(path) => {
                        let content = std::fs::read_to_string(path).map_err(|e| {
                            ApiError::Configuration(format!(
                                "failed to read auth file {}: {e}",
                                path.display()
                            ))
                        })?;
                        let token = serde_json::from_str::<serde_json::Value>(&content)
                            .ok()
                            .and_then(|v| {
                                v.get("accessToken")
                                    .or_else(|| v.get("token"))
                                    .and_then(|t| t.as_str().map(String::from))
                            })
                            .unwrap_or_else(|| content.trim().to_string());
                        AuthSource::BearerToken(token)
                    }
                    Credential::None => {
                        return Err(ApiError::Configuration(
                            "no credential available for Anthropic provider".to_string(),
                        ));
                    }
                };
                let client = AnthropicClient::from_auth_with_mode(auth, mode)
                    .with_base_url(resolved.base_url.clone());
                Ok(Self::Anthropic(client))
            }
            ApiFormat::OpenAiCompletions | ApiFormat::OpenAiResponses => {
                // Codex uses its own client (Responses API + special headers).
                if resolved.kind == ProviderKind::Codex {
                    return match &resolved.credential {
                        Credential::AuthFile(path) => {
                            let content = std::fs::read_to_string(path).map_err(|e| {
                                ApiError::Configuration(format!(
                                    "failed to read codex auth file {}: {e}",
                                    path.display()
                                ))
                            })?;
                            let parsed: serde_json::Value = serde_json::from_str(&content)
                                .map_err(|e| {
                                    ApiError::Configuration(format!(
                                        "failed to parse codex auth file {}: {e}",
                                        path.display()
                                    ))
                                })?;
                            // Support both nested (`tokens.access_token`) and flat
                            // (`access_token`) layouts.
                            let tokens = parsed.get("tokens").unwrap_or(&parsed);
                            let access_token = tokens
                                .get("access_token")
                                .and_then(|v| v.as_str())
                                .ok_or_else(|| {
                                    ApiError::Configuration(
                                        "codex auth file missing 'access_token' field".to_string(),
                                    )
                                })?
                                .to_string();
                            let account_id = tokens
                                .get("account_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            Ok(Self::Codex(CodexClient::new(
                                resolved.base_url.clone(),
                                access_token,
                                account_id,
                            )))
                        }
                        Credential::Token(token) => Ok(Self::Codex(CodexClient::new(
                            resolved.base_url.clone(),
                            token.clone(),
                            String::new(),
                        ))),
                        _ => Err(ApiError::Configuration(
                            "codex provider requires authFile or token credential".to_string(),
                        )),
                    };
                }

                // Build OpenAiCompatClient with the resolved credential + base URL.
                let api_key = match &resolved.credential {
                    Credential::ApiKey(key) => key.clone(),
                    Credential::Token(token) => token.clone(),
                    Credential::None => String::new(),
                    Credential::AuthFile(_) => {
                        return Err(ApiError::Configuration(
                            "auth file credential not supported for OpenAI-compat providers"
                                .to_string(),
                        ));
                    }
                };
                let config = OpenAiCompatConfig::openai();
                let client = OpenAiCompatClient::new(api_key, config)
                    .with_base_url(resolved.base_url.clone());
                match resolved.kind {
                    ProviderKind::Xai => Ok(Self::Xai(client)),
                    _ => Ok(Self::OpenAi(client)),
                }
            }
            ApiFormat::GeminiGenerateContent => {
                let client = GeminiClient::from_resolved(resolved)?;
                Ok(Self::Gemini(client))
            }
        }
    }

    #[must_use]
    pub const fn provider_kind(&self) -> ProviderKind {
        match self {
            Self::Anthropic(_) => ProviderKind::Anthropic,
            Self::Xai(_) => ProviderKind::Xai,
            Self::OpenAi(_) => ProviderKind::OpenAi,
            Self::Codex(_) => ProviderKind::Codex,
            Self::Gemini(_) => ProviderKind::Gemini,
        }
    }

    #[must_use]
    pub fn with_session_tracer(self, session_tracer: SessionTracer) -> Self {
        match self {
            Self::Anthropic(client) => Self::Anthropic(client.with_session_tracer(session_tracer)),
            Self::Xai(client) => Self::Xai(client.with_session_tracer(session_tracer)),
            Self::OpenAi(client) => Self::OpenAi(client.with_session_tracer(session_tracer)),
            Self::Codex(client) => Self::Codex(client.with_session_tracer(session_tracer)),
            Self::Gemini(client) => Self::Gemini(client.with_session_tracer(session_tracer)),
        }
    }

    #[must_use]
    pub fn with_prompt_cache(self, prompt_cache: PromptCache) -> Self {
        match self {
            Self::Anthropic(client) => Self::Anthropic(client.with_prompt_cache(prompt_cache)),
            Self::Gemini(_) => self,
            other => other,
        }
    }

    #[must_use]
    pub fn prompt_cache_stats(&self) -> Option<PromptCacheStats> {
        match self {
            Self::Anthropic(client) => client.prompt_cache_stats(),
            Self::Xai(_) | Self::OpenAi(_) | Self::Codex(_) | Self::Gemini(_) => None,
        }
    }

    #[must_use]
    pub fn take_last_prompt_cache_record(&self) -> Option<PromptCacheRecord> {
        match self {
            Self::Anthropic(client) => client.take_last_prompt_cache_record(),
            Self::Xai(_) | Self::OpenAi(_) | Self::Codex(_) | Self::Gemini(_) => None,
        }
    }

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        match self {
            Self::Anthropic(client) => client.send_message(request).await,
            Self::Xai(client) | Self::OpenAi(client) => client.send_message(request).await,
            Self::Codex(client) => client.send_message(request).await,
            Self::Gemini(client) => client.send_message(request).await,
        }
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        match self {
            Self::Anthropic(client) => client
                .stream_message(request)
                .await
                .map(MessageStream::Anthropic),
            Self::Xai(client) | Self::OpenAi(client) => client
                .stream_message(request)
                .await
                .map(MessageStream::OpenAiCompat),
            Self::Codex(client) => client
                .stream_message(request)
                .await
                .map(MessageStream::Codex),
            Self::Gemini(client) => client
                .stream_message(request)
                .await
                .map(MessageStream::Gemini),
        }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum MessageStream {
    Anthropic(anthropic::MessageStream),
    OpenAiCompat(openai_compat::MessageStream),
    Codex(crate::providers::codex::MessageStream),
    Gemini(gemini::MessageStream),
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Anthropic(stream) => stream.request_id(),
            Self::OpenAiCompat(stream) => stream.request_id(),
            Self::Codex(stream) => stream.request_id(),
            Self::Gemini(stream) => stream.request_id(),
        }
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        match self {
            Self::Anthropic(stream) => stream.next_event().await,
            Self::OpenAiCompat(stream) => stream.next_event().await,
            Self::Codex(stream) => stream.next_event().await,
            Self::Gemini(stream) => stream.next_event().await,
        }
    }
}

pub use anthropic::{
    base_url_for_mode, oauth_token_is_expired, resolve_saved_oauth_token,
    resolve_startup_auth_source, OAuthTokenSet,
};
#[must_use]
pub fn read_base_url() -> String {
    anthropic::read_base_url()
}

#[must_use]
pub fn read_xai_base_url() -> String {
    openai_compat::read_base_url(OpenAiCompatConfig::xai())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::ProviderClient;
    use crate::providers::registry::{
        resolve_model_alias_from_config, resolve_provider_from_config, ModelConfigEntry,
        ModelProviderMapping, ProviderConnectionConfig, SudoCodeConfig,
    };
    use crate::providers::ProviderKind;

    fn sample_config() -> SudoCodeConfig {
        let mut auth_modes = BTreeMap::new();
        let mut api_key = BTreeMap::new();
        api_key.insert(
            "dashscope".to_string(),
            ProviderConnectionConfig {
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
                api_key: Some("test-dashscope-key".to_string()),
                api_key_env: None,
                token: None,
                token_env: None,
                auth_file: None,
            },
        );
        auth_modes.insert("api-key".to_string(), api_key);

        let mut models = BTreeMap::new();
        let mut qwen_providers = BTreeMap::new();
        qwen_providers.insert(
            "api-key".to_string(),
            ModelProviderMapping {
                provider: "dashscope".to_string(),
                model: "qwen-plus".to_string(),
                api: None,
            },
        );
        models.insert(
            "qwen-plus".to_string(),
            ModelConfigEntry {
                alias: "qwen-plus".to_string(),
                name: "Qwen Plus".to_string(),
                input: vec!["text".to_string()],
                providers: qwen_providers,
            },
        );

        SudoCodeConfig {
            auth_modes,
            models,
            web_search: Default::default(),
        }
    }

    #[test]
    fn resolves_alias_from_config() {
        let config = sample_config();
        assert_eq!(
            resolve_model_alias_from_config(&config, "qwen-plus"),
            "qwen-plus"
        );
    }

    #[test]
    fn dashscope_model_routes_via_config() {
        let config = sample_config();
        let resolved = resolve_provider_from_config("qwen-plus", None, &config)
            .expect("qwen-plus should resolve from config");

        assert_eq!(resolved.kind, ProviderKind::OpenAi);
        assert!(resolved.base_url.contains("dashscope.aliyuncs.com"));

        let client = ProviderClient::from_resolved(&resolved, None)
            .expect("should build client from resolved");
        assert_eq!(client.provider_kind(), ProviderKind::OpenAi);
    }
}
