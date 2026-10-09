use telemetry::SessionTracer;

use crate::error::ApiError;
use crate::prompt_cache::{PromptCache, PromptCacheRecord, PromptCacheStats};
use crate::providers::anthropic::{self, AnthropicClient, AuthSource};
use crate::providers::codex::CodexClient;
use crate::providers::gemini::{self, GeminiClient};
use crate::providers::openai_compat::{self, OpenAiCompatClient, OpenAiCompatConfig};
use crate::providers::registry::{ApiFormat, Credential, ResolvedProvider};
use crate::providers::{AuthMode, ProviderKind};
use crate::stream_collect::ResponseAccumulator;
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
    /// Resolve a provider under this session's model egress policy.
    pub fn from_resolved_with_access(
        resolved: &ResolvedProvider,
        mode: Option<AuthMode>,
        access: &crate::ModelAccess,
    ) -> Result<Self, ApiError> {
        if !resolved.base_url.starts_with("nexus://") {
            if access.require_mount {
                return Err(ApiError::Configuration(
                    "co-hosted model requests require a nexus:///mount baseUrl".into(),
                ));
            }
            return Self::from_resolved(resolved, mode);
        }
        if resolved.kind == ProviderKind::Codex
            || resolved.api_format == ApiFormat::GeminiGenerateContent
        {
            return Err(ApiError::Configuration(
                "this provider has no Nexus model mount transport".into(),
            ));
        }
        let transport = crate::nexus_transport::NexusTransport::new(
            &resolved.base_url,
            std::sync::Arc::clone(&access.fs),
        )?;
        let mut local = resolved.clone();
        local.base_url = "http://nexus.invalid".into();
        // The mount authenticates upstream; this client never loads credentials.
        local.credential = Credential::ApiKey(String::new());
        let mut client = Self::from_resolved(&local, mode)?;
        match &mut client {
            Self::Anthropic(client) => client.set_nexus_transport(transport),
            Self::OpenAi(client) | Self::Xai(client) => client.set_nexus_transport(transport),
            _ => {
                return Err(ApiError::Configuration(
                    "unsupported Nexus provider transport".into(),
                ))
            }
        }
        Ok(client)
    }

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
                let mut client = AnthropicClient::from_auth_with_mode(auth, mode)
                    .with_base_url(resolved.base_url.clone());
                // `sudocode.json: cache_ttl_1h` overrides the default the auth
                // mode picked. Applied here, once, before any request: the TTL
                // is part of every `cache_control` block, so a value that moved
                // between turns would rewrite the prefix it was holding.
                if let Some(ttl_1h) = resolved.cache_ttl_1h {
                    client = client.with_cache_ttl_1h(ttl_1h);
                }
                // Per-model `extraBody` (sudocode.json) — same additive rule
                // as the OpenAI-compatible path: sudocode's own fields win.
                // `render_json_body` skips keys the serialized request already
                // carries; the reserved list covers the ones it omits when
                // unset (e.g. `stream` when false).
                for (key, value) in &resolved.extra_body {
                    if crate::types::is_reserved_request_body_key(key) {
                        continue;
                    }
                    client = client.with_extra_body_param(key.clone(), value.clone());
                }
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
                    .with_api_format(resolved.api_format)
                    .with_base_url(resolved.base_url.clone())
                    .with_extra_body(resolved.extra_body.clone());
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

    /// Stable request route identity for context-maintenance skip validation.
    #[must_use]
    pub fn route_identity(&self) -> String {
        let base_url = match self {
            Self::Anthropic(client) => client.base_url(),
            Self::Xai(client) | Self::OpenAi(client) => client.base_url(),
            Self::Codex(client) => client.base_url(),
            Self::Gemini(client) => client.base_url(),
        };
        format!("{:?}|{base_url}", self.provider_kind())
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
    pub fn session_tracer(&self) -> Option<&SessionTracer> {
        match self {
            Self::Anthropic(client) => client.session_tracer(),
            Self::Xai(client) => client.session_tracer(),
            Self::OpenAi(client) => client.session_tracer(),
            Self::Codex(client) => client.session_tracer(),
            Self::Gemini(client) => client.session_tracer(),
        }
    }

    pub fn set_retry_notifier(
        &mut self,
        notifier: Option<std::sync::Arc<dyn crate::http_transport::RetryNotifier>>,
    ) {
        match self {
            Self::Anthropic(client) => client.set_retry_notifier(notifier),
            Self::Xai(client) | Self::OpenAi(client) => client.set_retry_notifier(notifier),
            Self::Codex(client) => client.set_retry_notifier(notifier),
            Self::Gemini(client) => client.set_retry_notifier(notifier),
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
        trace_id: Option<&str>,
    ) -> Result<MessageResponse, ApiError> {
        match self {
            Self::Anthropic(client) => client.send_message(request, trace_id).await,
            Self::Xai(client) | Self::OpenAi(client) => {
                client.send_message(request, trace_id).await
            }
            Self::Codex(client) => client.send_message(request, trace_id).await,
            Self::Gemini(client) => client.send_message(request, trace_id).await,
        }
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
        trace_id: Option<&str>,
    ) -> Result<MessageStream, ApiError> {
        match self {
            Self::Anthropic(client) => client
                .stream_message(request, trace_id)
                .await
                .map(MessageStream::Anthropic),
            Self::Xai(client) | Self::OpenAi(client) => client
                .stream_message(request, trace_id)
                .await
                .map(MessageStream::OpenAiCompat),
            Self::Codex(client) => client
                .stream_message(request, trace_id)
                .await
                .map(MessageStream::Codex),
            Self::Gemini(client) => client
                .stream_message(request, trace_id)
                .await
                .map(MessageStream::Gemini),
        }
    }

    /// Get one whole response, transported as a stream.
    ///
    /// Prefer this over [`ProviderClient::send_message`] for any request whose
    /// duration is not bounded — which in practice means any request carrying a
    /// whole conversation. A non-streaming request puts no bytes on the socket
    /// until generation has finished, and on our path a connection that stays
    /// byte-quiet for ~50s is closed with no HTTP response at all: measured with
    /// the same prompt, model and route and only `stream` changed, `stream:
    /// false` died at 50.3s while `stream: true` had its first byte at 1.7s and
    /// ran to completion in 201.8s. The failure therefore scales with how slow
    /// the answer is, and the requests most likely to be slow are the big ones
    /// we least want to lose.
    ///
    /// Callers keep their shape: this returns the same `MessageResponse` the
    /// non-streaming call did, and goes through the same retrying transport.
    pub async fn send_message_streamed(
        &self,
        request: &MessageRequest,
        trace_id: Option<&str>,
    ) -> Result<MessageResponse, ApiError> {
        let streaming = MessageRequest {
            stream: true,
            ..request.clone()
        };
        let mut stream = self.stream_message(&streaming, trace_id).await?;
        stream.collect_response(&request.model).await
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

    /// Which provider this stream came from, for error messages only.
    #[must_use]
    pub const fn provider_label(&self) -> &'static str {
        match self {
            Self::Anthropic(_) => "anthropic",
            Self::OpenAiCompat(_) => "openai-compatible",
            Self::Codex(_) => "codex",
            Self::Gemini(_) => "gemini",
        }
    }

    /// Drain this stream and assemble the single `MessageResponse` an
    /// equivalent non-streaming request would have returned.
    ///
    /// `request_model` is only a fallback for providers whose `message_start`
    /// does not name the model; the stream's own answer wins when it has one.
    pub async fn collect_response(
        &mut self,
        request_model: &str,
    ) -> Result<MessageResponse, ApiError> {
        let request_id = self.request_id().map(ToString::to_string);
        let mut accumulator = ResponseAccumulator::new(self.provider_label(), request_model);
        let mut saw_terminal = false;
        while let Some(event) = self.next_event().await? {
            saw_terminal |= matches!(&event, StreamEvent::MessageStop(_))
                || matches!(&event, StreamEvent::MessageDelta(delta) if delta.delta.stop_reason.is_some())
                || matches!(&event, StreamEvent::MessageStart(start) if start.message.stop_reason.is_some());
            record_compaction_stream_event(&event)?;
            accumulator.push(event);
        }
        if runtime::compaction_scope::is_active() && !saw_terminal {
            return Err(ApiError::incomplete_stream(
                self.provider_label(),
                request_model,
                "summary stream ended without a provider terminal event",
            ));
        }
        accumulator.finish(request_id)
    }
}

/// Capture received summary usage before a queued event can be cancelled.
pub(crate) fn record_compaction_stream_event(event: &StreamEvent) -> Result<(), ApiError> {
    let usage = match event {
        StreamEvent::MessageStart(start) => Some(&start.message.usage),
        StreamEvent::MessageDelta(delta) => Some(&delta.usage),
        _ => None,
    };
    // Synthetic initial events in Codex/Gemini carry default usage.
    // Only a received bill changes an unknown receipt into known usage.
    if let Some(usage) = usage.filter(|usage| **usage != crate::types::Usage::default()) {
        runtime::compaction_scope::record_usage(usage.token_usage());
    }
    if matches!(event, StreamEvent::MessageStop(_))
        || matches!(event, StreamEvent::MessageDelta(delta) if delta.delta.stop_reason.is_some())
        || matches!(event, StreamEvent::MessageStart(start) if start.message.stop_reason.is_some())
    {
        runtime::compaction_scope::record_completed_response()
            .map_err(|error| ApiError::Configuration(error.to_string()))?;
    }
    Ok(())
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
                ..Default::default()
            },
        );

        SudoCodeConfig {
            cache_ttl_1h: None,
            auth_modes,
            models,
            web_search: Default::default(),
            selected_account: None,
            auth_profile_conflicts: Vec::new(),
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
