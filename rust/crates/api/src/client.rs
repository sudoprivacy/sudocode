use telemetry::SessionTracer;

use crate::error::ApiError;
use crate::prompt_cache::{PromptCache, PromptCacheRecord, PromptCacheStats};
use crate::providers::anthropic::{self, AnthropicClient, AuthSource};
use crate::providers::codex::CodexClient;
use crate::providers::gemini::{self, GeminiClient};
use crate::providers::openai_compat::{self, OpenAiCompatClient, OpenAiCompatConfig};
use crate::providers::registry::{ApiFormat, Credential, ResolvedProvider};
use crate::providers::{AuthMode, ProviderKind};
use crate::request_scope::RequestScope;
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
                // mode picked. This is the starting value; the engine may switch
                // it later through the client's shared `CacheTtl`.
                if let Some(ttl_1h) = resolved.cache_ttl_1h {
                    client = client.with_cache_ttl_1h(ttl_1h);
                }
                // Per-model `extraBody` (sudocode.json) — same additive rule
                // as the OpenAI-compatible path: sudocode's own fields win.
                // `render_json_body` skips keys the serialized request already
                // carries; the reserved list covers the ones it omits when
                // unset (e.g. `stream` when false).
                for (key, value) in &resolved.extra_body {
                    if key == "betas" {
                        let mut betas: Vec<String> = serde_json::from_value(value.clone())
                            .map_err(|_| {
                                ApiError::Configuration(
                                    "Anthropic extraBody.betas must be an array of strings".into(),
                                )
                            })?;
                        if betas.iter().any(|beta| {
                            beta.is_empty()
                                || !beta
                                    .bytes()
                                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                        }) {
                            return Err(ApiError::Configuration(
                                "Anthropic extraBody.betas contains an invalid beta name".into(),
                            ));
                        }
                        let profile = client.request_profile().clone();
                        if profile.betas.iter().any(|beta| beta == "oauth-2025-04-20")
                            && !betas.iter().any(|beta| beta == "oauth-2025-04-20")
                        {
                            betas.push("oauth-2025-04-20".into());
                        }
                        client = client.with_request_profile(profile.with_betas(betas));
                        continue;
                    }
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

    /// Local-only advisor context; the provider remains the policy owner.
    #[must_use]
    pub fn cache_advisor_context(&self) -> Option<(PromptCache, String, u32)> {
        let Self::Anthropic(client) = self else {
            return None;
        };
        let cache = client.prompt_cache()?.clone();
        Some((
            cache,
            client.cache_advisor_route_key(),
            client.cache_ttl().seconds(),
        ))
    }

    /// The live prompt-cache TTL, for providers that send one.
    #[must_use]
    pub fn cache_ttl(&self) -> Option<&crate::CacheTtl> {
        match self {
            Self::Anthropic(client) => Some(client.cache_ttl()),
            _ => None,
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
    pub fn session_tracer(&self) -> Option<&SessionTracer> {
        match self {
            Self::Anthropic(client) => client.session_tracer(),
            Self::Xai(client) | Self::OpenAi(client) => client.session_tracer(),
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
        let scope = RequestScope::default();
        let inner = scope
            .scope(self.stream_message_raw(request, trace_id))
            .await?;
        let provider = inner.provider_label();
        Ok(MessageStream {
            inner: Some(inner),
            replay: Some(ReplayRequest {
                client: self.clone(),
                request: request.clone(),
                trace_id: trace_id.map(ToOwned::to_owned),
            }),
            model: request.model.clone(),
            scope,
            provider,
            events_seen: 0,
            replayed: false,
            done: false,
            last_request_id: None,
        })
    }

    async fn stream_message_raw(
        &self,
        request: &MessageRequest,
        trace_id: Option<&str>,
    ) -> Result<RawMessageStream, ApiError> {
        match self {
            Self::Anthropic(client) => client
                .stream_message(request, trace_id)
                .await
                .map(RawMessageStream::Anthropic),
            Self::Xai(client) | Self::OpenAi(client) => client
                .stream_message(request, trace_id)
                .await
                .map(RawMessageStream::OpenAiCompat),
            Self::Codex(client) => client
                .stream_message(request, trace_id)
                .await
                .map(RawMessageStream::Codex),
            Self::Gemini(client) => client
                .stream_message(request, trace_id)
                .await
                .map(RawMessageStream::Gemini),
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
enum RawMessageStream {
    Anthropic(anthropic::MessageStream),
    OpenAiCompat(openai_compat::MessageStream),
    Codex(crate::providers::codex::MessageStream),
    Gemini(gemini::MessageStream),
}

impl RawMessageStream {
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
}

/// A logical provider stream. Socket bytes, SSE comments and provider pings do
/// not complete `next_event`, so liveness is measured at this shared boundary.
/// One safe replay is allowed before any event escapes, within the same HTTP
/// attempt budget. After events escape, failure is surfaced without replay.
pub struct MessageStream {
    inner: Option<RawMessageStream>,
    replay: Option<ReplayRequest>,
    model: String,
    scope: RequestScope,
    provider: &'static str,
    events_seen: u64,
    replayed: bool,
    done: bool,
    last_request_id: Option<String>,
}

struct ReplayRequest {
    client: ProviderClient,
    request: MessageRequest,
    trace_id: Option<String>,
}

impl std::fmt::Debug for MessageStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageStream")
            .field("provider", &self.provider)
            .field("events_seen", &self.events_seen)
            .field("replayed", &self.replayed)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.inner
            .as_ref()
            .and_then(RawMessageStream::request_id)
            .or(self.last_request_id.as_deref())
    }

    #[must_use]
    pub const fn provider_label(&self) -> &'static str {
        self.provider
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        loop {
            if self.done {
                return Ok(None);
            }
            // Start the timer when the caller asks for another event. Time spent
            // executing a tool or otherwise not polling is not upstream idleness.
            let timeout = self.scope.idle_timeout();
            let inner = self.inner.as_mut().expect("active provider stream");
            let result = if let Some(idle_timeout) = timeout {
                match tokio::time::timeout(idle_timeout, inner.next_event()).await {
                    Ok(result) => result,
                    Err(_) => Err(ApiError::StreamStalled {
                        provider: self.provider.into(),
                        model: self.model.clone(),
                        idle_timeout,
                        request_id: self.request_id().map(ToOwned::to_owned),
                    }),
                }
            } else {
                inner.next_event().await
            };
            match result {
                Ok(Some(event)) => {
                    self.events_seen = self.events_seen.saturating_add(1);
                    // Large conversations are needed for replay only until the
                    // first event. Do not retain another history for the turn.
                    self.replay = None;
                    return Ok(Some(event));
                }
                Ok(None) => {
                    self.replay = None;
                    self.done = true;
                    return Ok(None);
                }
                Err(error) => {
                    self.last_request_id = self.request_id().map(ToOwned::to_owned);
                    let error = if self.events_seen > 0
                        && error.is_retryable()
                        && !runtime::compaction_scope::is_active()
                    {
                        // Compaction owns its replay decision and already records
                        // partial usage in its parent budget before retrying.
                        ApiError::PartialStream {
                            source: Box::new(error),
                            request_id: self.last_request_id.clone(),
                        }
                    } else {
                        error
                    };
                    self.scope.record_stream_failure(&error);
                    // Drop the failed body before opening a replacement request.
                    self.inner = None;
                    if self.events_seen == 0
                        && !self.replayed
                        && error.is_retryable()
                        && self.scope.has_attempts_left()
                    {
                        self.replayed = true;
                        if let Err(error) = self.scope.wait_before_replay(&error).await {
                            self.replay = None;
                            self.done = true;
                            return Err(error);
                        }
                        let replay = self.replay.as_ref().expect("no event has escaped");
                        match self
                            .scope
                            .scope(
                                replay.client.stream_message_raw(
                                    &replay.request,
                                    replay.trace_id.as_deref(),
                                ),
                            )
                            .await
                        {
                            Ok(inner) => {
                                self.last_request_id = inner.request_id().map(ToOwned::to_owned);
                                self.inner = Some(inner);
                            }
                            Err(error) => {
                                self.replay = None;
                                self.done = true;
                                return Err(error);
                            }
                        }
                    } else {
                        self.replay = None;
                        self.done = true;
                        return Err(error);
                    }
                }
            }
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
        accumulator.finish(self.request_id().map(ToString::to_string))
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
            web_search: runtime::WebSearchConfig::default(),
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
